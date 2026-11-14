//! The Win+V-style window: search box, tabs (All / Pinned / Links / your groups), clip list.
//! It holds no history itself: every search goes to the daemon on a worker thread,
//! so the window opens instantly even with a huge history and typing never blocks.

use crate::daemon::ipc::{self, Command, GroupRow};
use crate::store::text::Kind;
use crate::store::{Hit, View};
use eframe::egui::{self, Key, Modifiers};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

const RESULTS: usize = 200;

type Results = Arc<Mutex<Option<(u64, Result<Vec<Hit>, String>)>>>;

pub fn run(query: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let Some(toggle) = toggle::claim()?
    else {
        return Ok(()); // a window was already open: we just closed it
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("clipkeep")
            .with_app_id("clipkeep")
            .with_inner_size([600.0, 500.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top()
            .with_icon(theme::window_icon().unwrap_or_default()),
        ..Default::default()
    };
    let paste = Arc::new(AtomicBool::new(false));
    let paste_flag = Arc::clone(&paste);
    let result = eframe::run_native(
        "clipkeep",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "linux")]
            toggle.listen(cc.egui_ctx.clone());
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Popup::new(cc.egui_ctx.clone(), query, paste_flag)))
        }),
    )
    .map_err(|e| e.to_string());
    #[cfg(target_os = "linux")]
    toggle::release();
    // The window is gone now, so Ctrl+V lands in the app the user came from.
    if paste.load(Ordering::Relaxed) && crate::config::Config::load().auto_paste {
        if let Err(e) = ipc::request(&Command::Paste) {
            eprintln!("clipkeep: {e}");
        }
    }
    result
}

mod debug;
mod input;
mod list;
mod sync_screen;
mod theme;

use self::input::Action;
#[cfg(target_os = "linux")]
mod toggle;
mod workers;

struct GroupEdit {
    id: u32,
    name: String,
}

struct Popup {
    query: String,
    tab: usize,
    groups: Vec<GroupRow>,
    hits: Vec<Hit>,
    loaded: bool,
    gen: u64,
    jobs: Sender<(u64, View, String)>,
    results: Results,
    selected: usize,
    scroll_to_selected: bool,
    was_focused: bool,
    frames: u32,
    error: Option<String>,
    sync: Option<sync_screen::SyncScreen>,
    edit: Option<GroupEdit>,
    paste_after_close: Arc<AtomicBool>,
    ctx: egui::Context,
    logo: Option<egui::TextureHandle>,
    secrets_refreshed: std::time::Instant,
    thumbs: HashMap<u32, Option<egui::TextureHandle>>,
    thumbs_asked: HashSet<u32>,
    thumb_jobs: Sender<u32>,
    thumb_results: Receiver<(u32, Option<egui::ColorImage>)>,
    /// Debug: `CLIPKEEP_SCREENSHOT=out.ppm clipkeep show` saves one frame and exits.
    screenshot_to: Option<std::path::PathBuf>,
}

impl Popup {
    pub(super) fn new(
        ctx: egui::Context,
        query: String,
        paste_after_close: Arc<AtomicBool>,
    ) -> Self {
        let (jobs, rx) = mpsc::channel();
        let results: Results = Arc::default();
        let out = Arc::clone(&results);
        let (thumb_jobs, thumb_rx) = mpsc::channel();
        let (thumb_tx, thumb_results) = mpsc::channel();
        let thumb_ctx = ctx.clone();
        let ctx_for_self = ctx.clone();
        std::thread::spawn(move || workers::thumb_worker(thumb_rx, thumb_tx, thumb_ctx));
        std::thread::spawn(move || workers::search_worker(rx, out, ctx));
        let mut p = Popup {
            query,
            tab: std::env::var("CLIPKEEP_TAB")
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
            groups: Vec::new(),
            hits: Vec::new(),
            loaded: false,
            gen: 0,
            jobs,
            results,
            selected: 0,
            scroll_to_selected: false,
            was_focused: false,
            frames: 0,
            error: None,
            sync: None,
            edit: None,
            paste_after_close,
            ctx: ctx_for_self.clone(),
            secrets_refreshed: std::time::Instant::now(),
            logo: theme::logo_image()
                .map(|img| ctx_for_self.load_texture("logo", img, egui::TextureOptions::LINEAR)),
            thumbs: HashMap::new(),
            thumbs_asked: HashSet::new(),
            thumb_jobs,
            thumb_results,
            screenshot_to: std::env::var_os("CLIPKEEP_SCREENSHOT").map(Into::into),
        };
        p.reload_groups();
        p.refresh();
        if std::env::var_os("CLIPKEEP_SYNC_STATE").is_some()
            || std::env::var_os("CLIPKEEP_SYNC").is_some()
        {
            p.open_sync(&p.ctx.clone());
        }
        p
    }

    pub(super) fn tabs(&self) -> Vec<(String, View)> {
        let mut t = vec![
            ("All".to_owned(), View::All),
            ("📌 Pinned".to_owned(), View::Pinned),
            ("🔗 Links".to_owned(), View::Links),
            ("🖼 Images".to_owned(), View::Images),
        ];
        t.extend(
            self.groups
                .iter()
                .map(|g| (format!("# {} {}", g.name, g.count), View::Group(g.id))),
        );
        t
    }

    pub(super) fn view(&self) -> View {
        self.tabs().get(self.tab).map(|t| t.1).unwrap_or(View::All)
    }

    pub(super) fn group_name(&self, id: u16) -> Option<&str> {
        self.groups
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.name.as_str())
    }

    pub(super) fn refresh(&mut self) {
        self.gen += 1;
        let _ = self.jobs.send((self.gen, self.view(), self.query.clone()));
    }

    pub(super) fn reload_groups(&mut self) {
        match ipc::request(&Command::Groups) {
            Ok(lines) => self.groups = lines.iter().filter_map(|l| ipc::parse_group(l)).collect(),
            Err(e) => self.error = Some(format!("daemon: {e}")),
        }
        if self.tab >= self.tabs().len() {
            self.tab = 0;
        }
    }

    pub(super) fn send(&mut self, cmd: Command) -> bool {
        match ipc::request(&cmd) {
            Ok(_) => true,
            Err(e) => {
                self.error = Some(format!("daemon: {e}"));
                false
            }
        }
    }

    pub(super) fn take_results(&mut self) {
        let Some((gen, result)) = self.results.lock().unwrap().take() else {
            return;
        };
        if gen != self.gen {
            return; // an older query; a newer answer is on its way
        }
        match result {
            Ok(hits) => {
                self.hits = hits;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.loaded = true;
        if self.selected >= self.hits.len() {
            self.selected = self.hits.len().saturating_sub(1);
        }
    }

    pub(super) fn take_thumbs(&mut self, ctx: &egui::Context) {
        while let Ok((id, image)) = self.thumb_results.try_recv() {
            let tex = image.map(|img| {
                ctx.load_texture(format!("thumb{id}"), img, egui::TextureOptions::LINEAR)
            });
            self.thumbs.insert(id, tex);
        }
    }

    pub(super) fn open_sync(&mut self, ctx: &egui::Context) {
        self.sync = Some(sync_screen::SyncScreen::open(ctx.clone()));
        // Browser and password prompts need a normal window: not on top, and it must survive losing focus.
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
    }

    pub(super) fn close_sync(&mut self, ctx: &egui::Context) {
        self.sync = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    pub(super) fn switch_tab(&mut self, delta: isize) {
        let n = self.tabs().len() as isize;
        self.tab = (self.tab as isize + delta).rem_euclid(n) as usize;
        self.selected = 0;
        self.refresh();
    }
}

impl eframe::App for Popup {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.debug_screenshot(&ctx);
        self.take_results();
        self.take_thumbs(&ctx);
        // Secrets show a countdown and disappear on their own: refresh while one is listed.
        if self.hits.iter().any(|h| h.kind == Kind::Secret as u8) {
            if self.secrets_refreshed.elapsed() >= std::time::Duration::from_secs(1) {
                self.secrets_refreshed = std::time::Instant::now();
                self.refresh();
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }

        // Ask for focus once (Windows/macOS start us from the daemon), then close
        // when the user clicks somewhere else, like Win+V does.
        match ui.input(|i| i.viewport().focused) {
            Some(true) => self.was_focused = true,
            Some(false)
                if self.was_focused && self.screenshot_to.is_none() && self.sync.is_none() =>
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            _ if !self.was_focused => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
            _ => {}
        }

        if let Some(screen) = &mut self.sync {
            let mut back = false;
            let logo = self.logo.clone();
            egui::CentralPanel::default()
                .frame(theme::window_frame())
                .show(ui, |ui| back = screen.show(ui, logo.as_ref()));
            if back {
                self.close_sync(&ctx);
            }
            return;
        }

        let mut action = if self.edit.is_none() {
            self.keyboard(ui)
        } else {
            None
        };

        egui::CentralPanel::default().frame(theme::window_frame()).show(ui, |ui| {
            let mut open_sync = false;
            let search = ui
                .horizontal(|ui| {
                    if let Some(logo) = &self.logo {
                        ui.add(egui::Image::new((logo.id(), egui::vec2(28.0, 28.0))));
                    }
                    let w = ui.available_width() - 84.0;
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Search…")
                            .desired_width(w),
                    );
                    open_sync = ui.button("Sync").clicked();
                    search
                })
                .inner;
            if open_sync {
                self.open_sync(&ctx);
            }
            if self.edit.is_none() {
                search.request_focus();
            }
            if search.changed() {
                self.selected = 0;
                self.refresh();
            }

            ui.horizontal_wrapped(|ui| {
                for (i, (label, _)) in self.tabs().into_iter().enumerate() {
                    if ui.selectable_label(i == self.tab, label).clicked() && i != self.tab {
                        self.tab = i;
                        self.selected = 0;
                        self.refresh();
                    }
                }
            });
            ui.add_space(2.0);

            if let Some(a) = self.clip_list(ui) {
                action = Some(a);
            }

            ui.separator();
            if self.edit.is_some() {
                self.group_editor(ui);
            } else {
                match &self.error {
                    Some(e) => ui.colored_label(theme::ERR, e),
                    None => ui.label(theme::muted("Enter copy · Ctrl+1…9 quick · Tab tabs · Ctrl+P pin · Ctrl+G group · Shift+Del delete").size(12.0)),
                };
            }
        });

        if let Some(action) = action {
            self.apply(&ctx, action);
        }
    }
}
