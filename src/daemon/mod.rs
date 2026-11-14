//! Background process: watches the clipboard, owns the store, serves the popup and CLI,
//! and on Windows/macOS also owns the global hotkey.
pub mod ipc;
pub mod paste;
pub mod platform;
mod secrets;
mod sync_loop;

use self::ipc::Command;
use self::platform::Event;
use self::secrets::Secrets;
use crate::config::Config;
use crate::paths;
use crate::store::media::{self, Media};
use crate::store::text::{self, ImageRef};
use crate::store::Store;
use std::process::Child;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Changes are written to disk at most this long after they happen.
const SAVE_DELAY: Duration = Duration::from_millis(500);
const PREVIEW_CHARS: usize = 120;

struct State {
    store: Store,
    clipboard: arboard::Clipboard,
    /// Last clipboard text we saw, so a deleted entry is not re-added by the next event.
    last_seen: Option<String>,
    changed_at: Option<Instant>,
    hotkey: String,
    popup: Option<Child>,
    paster: Result<crate::daemon::paste::Paster, String>,
    media: Media,
    /// Pixel hash of the last image we saw (same purpose as `last_seen`).
    last_image: Option<u64>,
    image_limit: u64,
    sync: Option<sync_loop::Handle>,
    secrets: Secrets,
}

fn now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

impl State {
    fn touched(&mut self) {
        if self.store.is_dirty() && self.changed_at.is_none() {
            self.changed_at = Some(Instant::now());
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.store.save() {
            eprintln!("clipkeep: cannot save history: {e}");
        }
        self.changed_at = None;
    }

    /// `concealed`: the app that copied marked it as a password (asked before taking the lock).
    fn capture(&mut self, concealed: bool) {
        match self.clipboard.get_text() {
            Ok(text) if !text.trim().is_empty() => {
                if self.secrets.enabled() && (concealed || crate::secret::looks_secret(&text)) {
                    self.capture_secret(text);
                } else {
                    self.capture_text(text);
                }
            }
            _ => self.capture_image(),
        }
    }

    fn capture_secret(&mut self, text: String) {
        if self.last_seen.as_deref() == Some(text.as_str()) {
            return;
        }
        self.last_image = None;
        self.secrets.add(text.clone(), Instant::now());
        self.last_seen = Some(text);
    }

    /// Forgets secrets whose time is up, and wipes the clipboard if it still holds one.
    fn expire_secrets(&mut self) {
        for text in self.secrets.expire(Instant::now()) {
            if self.clipboard.get_text().ok().as_deref() == Some(text.as_str()) {
                let _ = self.clipboard.clear();
                self.last_seen = None;
            }
        }
    }

    /// Tells the sync thread about a local change (no-op when sync is off).
    fn emit(&self, op: crate::sync::Op) {
        if let Some(s) = &self.sync {
            let _ = s.tx.send(sync_loop::Msg::Op(op));
        }
    }

    fn capture_image(&mut self) {
        let Ok(img) = self.clipboard.get_image() else {
            return;
        };
        let (w, h) = (img.width as u32, img.height as u32);
        if img.bytes.is_empty() || img.bytes.len() > media::MAX_RGBA_BYTES {
            return;
        }
        let hash = text::fnv1a(&img.bytes) ^ ((w as u64) << 40 | (h as u64) << 20);
        if self.last_image == Some(hash) {
            return;
        }
        self.last_image = Some(hash);
        self.last_seen = None;
        let bytes = match self.media.size(hash) {
            Some(b) => b,
            None => match self.media.put(hash, w, h, &img.bytes) {
                Ok(b) => b,
                Err(e) => return eprintln!("clipkeep: cannot store image: {e}"),
            },
        };
        let marker = text::image_marker(&ImageRef {
            hash,
            width: w,
            height: h,
            bytes,
        });
        match self.store.push(&marker, now()) {
            Ok(true) => self.emit(crate::sync::Op::add(&marker, now())),
            Ok(false) => {}
            Err(e) => eprintln!("clipkeep: cannot store clip: {e}"),
        }
        self.trim_images();
        self.touched();
    }

    /// Deletes image files nobody refers to, and the oldest unpinned images above the size limit.
    fn trim_images(&mut self) {
        let mut refs = self.store.image_refs();
        if self.image_limit > 0 {
            let mut total = self.media.total_size();
            for &(id, hash, pinned) in &refs {
                if total <= self.image_limit {
                    break;
                }
                if !pinned && self.store.remove(id).unwrap_or(false) {
                    total = total.saturating_sub(self.media.size(hash).unwrap_or(0));
                    self.media.remove(hash);
                }
            }
            refs = self.store.image_refs();
        }
        self.media.sweep(&refs.iter().map(|r| r.1).collect());
    }

    fn capture_text(&mut self, text: String) {
        if self.last_seen.as_deref() == Some(text.as_str()) {
            return;
        }
        self.last_image = None;
        match self.store.push(&text, now()) {
            Ok(true) => self.emit(crate::sync::Op::add(&text, now())),
            Ok(false) => {}
            Err(e) => eprintln!("clipkeep: cannot store clip: {e}"),
        }
        self.last_seen = Some(text);
        self.touched();
    }

    /// Opens the popup, or closes it if it is already open (Windows/macOS hotkey).
    fn toggle_popup(&mut self) {
        if let Some(child) = &mut self.popup {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
                self.popup = None;
                return;
            }
        }
        #[cfg(windows)]
        // SAFETY: plain Win32 call. Lets the popup we start take the foreground.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
            );
        }
        match std::process::Command::new(crate::cli::gui_exe())
            .arg("show")
            .spawn()
        {
            Ok(child) => self.popup = Some(child),
            Err(e) => eprintln!("clipkeep: cannot open popup: {e}"),
        }
    }

    fn handle(&mut self, cmd: &Command) -> Result<Vec<String>, String> {
        let err = |e: std::io::Error| e.to_string();
        let mut out = Vec::new();
        match cmd {
            Command::Ping | Command::Quit => {}
            Command::Show => self.toggle_popup(),
            Command::Copy(id) if Secrets::owns(*id) => {
                let text = self
                    .secrets
                    .get(*id)
                    .ok_or("this secret was already forgotten")?
                    .to_owned();
                self.clipboard
                    .set_text(text.clone())
                    .map_err(|e| e.to_string())?;
                self.last_seen = Some(text);
                self.last_image = None;
            }
            Command::Pin(id) | Command::SetGroup(id, _) if Secrets::owns(*id) => {
                return Err("passwords are not kept; save it in a password manager".into());
            }
            Command::Delete(id) if Secrets::owns(*id) => {
                self.secrets.remove(*id);
            }
            Command::Thumb(id) if Secrets::owns(*id) => return Err("not an image".into()),
            Command::Copy(id) => {
                let text = self.store.text(*id).map_err(err)?.ok_or("no such clip")?;
                if let Some(r) = text::parse_image_marker(&text) {
                    let (w, h, rgba) = self.media.full(r.hash).map_err(err)?;
                    self.clipboard
                        .set_image(arboard::ImageData {
                            width: w as usize,
                            height: h as usize,
                            bytes: rgba.into(),
                        })
                        .map_err(|e| e.to_string())?;
                    self.last_image = Some(r.hash);
                    self.last_seen = None;
                } else {
                    self.clipboard
                        .set_text(text.clone())
                        .map_err(|e| e.to_string())?;
                    self.last_seen = Some(text.clone());
                    self.last_image = None;
                }
                self.store.push(&text, now()).map_err(err)?;
                self.emit(crate::sync::Op::add(&text, now()));
            }
            Command::Thumb(id) => {
                let text = self.store.text(*id).map_err(err)?.ok_or("no such clip")?;
                let r = text::parse_image_marker(&text).ok_or("not an image")?;
                out.push(format!(
                    "T\t{}",
                    ipc::base64_encode(&self.media.thumb_png(r.hash).map_err(err)?)
                ));
            }
            Command::Paste => {
                let paster = self
                    .paster
                    .as_mut()
                    .map_err(|e| format!("auto-paste unavailable: {e}"))?;
                // Give the desktop a moment to return focus to the previous window.
                thread::sleep(Duration::from_millis(120));
                paster
                    .ctrl_v()
                    .map_err(|e| format!("auto-paste failed: {e}"))?;
            }
            Command::Pin(id) => {
                self.store
                    .toggle_pin(*id)
                    .then_some(())
                    .ok_or("no such clip")?;
                if let (Some(h), Some(m)) = (self.store.hash_of(*id), self.store.get(*id)) {
                    self.emit(crate::sync::Op::Pin {
                        h,
                        on: m.pinned,
                        time: now(),
                    });
                }
            }
            Command::Delete(id) => {
                let hash = self.store.hash_of(*id);
                if let Some(h) = hash {
                    self.emit(crate::sync::Op::Remove { h, time: now() });
                }
                self.store
                    .remove(*id)
                    .map_err(err)?
                    .then_some(())
                    .ok_or("no such clip")?;
                self.trim_images();
            }
            Command::Clear => {
                self.store.clear().map_err(err)?;
                self.trim_images();
            }
            Command::SetGroup(id, name) => {
                self.store.set_group(*id, name)?;
                if let Some(h) = self.store.hash_of(*id) {
                    self.emit(crate::sync::Op::Group {
                        h,
                        name: name.trim().to_owned(),
                        time: now(),
                    });
                }
            }
            Command::Sync => match &self.sync {
                Some(s) => {
                    let _ = s.tx.send(sync_loop::Msg::Now);
                }
                None => return Err("sync is not set up (run: clipkeep sync setup)".into()),
            },
            Command::Groups => {
                for g in self.store.groups() {
                    let count = self.store.group_size(g.id);
                    if count > 0 {
                        out.push(ipc::encode_group(g.id, count, &g.name));
                    }
                }
            }
            Command::Stats => {
                let s = self.store.stats();
                for (k, v) in [
                    ("entries", s.entries.to_string()),
                    ("pinned", s.pinned.to_string()),
                    ("groups", s.groups.to_string()),
                    ("blocks", s.blocks.to_string()),
                    ("text_bytes", s.text_bytes.to_string()),
                    ("disk_bytes", s.disk_bytes.to_string()),
                    (
                        "sync",
                        match &self.sync {
                            Some(s) => s.status.lock().unwrap().describe(),
                            None => "off".into(),
                        },
                    ),
                    ("images_bytes", self.media.total_size().to_string()),
                    ("encrypted", self.store.is_encrypted().to_string()),
                    ("hotkey", self.hotkey.clone()),
                    ("data_dir", paths::data_dir().display().to_string()),
                ] {
                    out.push(format!("S\t{k}\t{v}"));
                }
            }
            Command::Search { view, limit, query } => {
                let limit = (*limit).min(10_000);
                let secrets = self.secrets.hits(*view, query, Instant::now());
                for h in secrets.iter() {
                    out.push(ipc::encode_hit(h));
                }
                let limit = limit.saturating_sub(secrets.len());
                for h in self.store.search(query, *view, limit, PREVIEW_CHARS) {
                    out.push(ipc::encode_hit(&h));
                }
            }
        }
        self.touched();
        Ok(out)
    }
}

/// Imports the v0.1 `history.json` (if any) into an empty store, oldest first.
fn import_legacy(store: &mut Store) {
    #[derive(serde::Deserialize)]
    struct Old {
        entries: Vec<OldEntry>,
    }
    #[derive(serde::Deserialize)]
    struct OldEntry {
        text: String,
        #[serde(default)]
        pinned: bool,
    }
    let path = paths::data_dir().join("history.json");
    if !store.is_empty() {
        return;
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let Ok(old) = serde_json::from_slice::<Old>(&bytes) else {
        return;
    };
    let t = now();
    for e in old.entries.iter().rev() {
        if store.push(&e.text, t).unwrap_or(false) && e.pinned {
            if let Some(id) = store.newest_id() {
                store.toggle_pin(id);
            }
        }
    }
    if store.save().is_ok() {
        let _ = std::fs::rename(&path, path.with_extension("json.imported"));
    }
}

pub fn run() -> Result<(), String> {
    hide_console();
    let listener = ipc::listen().map_err(|e| e.to_string())?;
    let config = Config::load();
    let dir = paths::data_dir();
    let cipher = crate::crypto::load_or_create(&paths::key_file(), Store::dir_is_encrypted(&dir))
        .map_err(|e| format!("encryption key: {e}"))?;
    let mut store = Store::open_with(&dir, config.max_entries, Some(cipher.clone()))
        .map_err(|e| format!("cannot open history in {}: {e}", dir.display()))?;
    import_legacy(&mut store);
    let clipboard = arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?;
    let state = Arc::new(Mutex::new(State {
        store,
        clipboard,
        last_seen: None,
        changed_at: None,
        hotkey: if cfg!(target_os = "linux") {
            "desktop shortcut (clipkeep setup)".into()
        } else {
            "—".into()
        },
        popup: None,
        paster: crate::daemon::paste::Paster::new().map_err(|e| e.to_string()),
        media: Media::new(&dir, Some(cipher.clone())),
        last_image: None,
        image_limit: config.image_limit_mb << 20,
        sync: None,
        secrets: Secrets::new(Duration::from_secs(config.secret_seconds)),
    }));
    state.lock().unwrap().trim_images();
    match crate::sync::settings::Settings::load() {
        Ok(Some(settings)) => {
            let media = Media::new(&dir, Some(cipher.clone()));
            let handle = sync_loop::start(
                Arc::clone(&state),
                &settings,
                cipher.clone(),
                media,
                dir.join("sync-state.bin"),
            );
            state.lock().unwrap().sync = handle;
        }
        Ok(None) => {}
        Err(e) => eprintln!("clipkeep: sync is off: {e}"),
    }

    let (tx, rx) = mpsc::channel::<Event>();

    let server_state = Arc::clone(&state);
    thread::spawn(move || {
        ipc::serve(listener, |cmd| server_state.lock().unwrap().handle(cmd));
        let mut s = server_state.lock().unwrap();
        s.save();
        #[cfg(unix)]
        let _ = std::fs::remove_file(paths::socket_file());
        std::process::exit(0);
    });

    let core_state = Arc::clone(&state);
    thread::spawn(move || {
        let concealed = platform::concealed();
        core_state.lock().unwrap().capture(concealed);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Event::Clipboard) => {
                    // Let the new owner finish setting up its data before we ask for it.
                    thread::sleep(Duration::from_millis(20));
                    while let Ok(Event::Clipboard) = rx.try_recv() {}
                    // Ask the owner for its data types before taking the lock: this is a round
                    // trip to another app and must not block the popup.
                    let concealed = platform::concealed();
                    core_state.lock().unwrap().capture(concealed);
                }
                Ok(Event::Hotkey) => core_state.lock().unwrap().toggle_popup(),
                Ok(Event::HotkeyReady(name)) => core_state.lock().unwrap().hotkey = name,
                Ok(Event::HotkeyFailed(why)) => {
                    eprintln!("clipkeep: no global hotkey: {why}. Set another one in config.txt.");
                    core_state.lock().unwrap().hotkey = format!("none ({why})");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            let mut s = core_state.lock().unwrap();
            s.expire_secrets();
            if s.changed_at.is_some_and(|t| t.elapsed() >= SAVE_DELAY) {
                s.save();
            }
        }
    });

    let result = platform::run(tx, &config.hotkeys());
    state.lock().unwrap().save();
    result
}

/// Windows: if `clipkeep.exe daemon` was started with a console of its own, drop it.
fn hide_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 calls with a valid buffer.
    unsafe {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
        let mut pids = [0u32; 2];
        if GetConsoleProcessList(pids.as_mut_ptr(), 2) == 1 {
            FreeConsole();
        }
    }
}
