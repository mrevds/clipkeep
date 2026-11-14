pub mod setup;
mod sync_cmd;
pub use sync_cmd::disable as sync_disable;

use crate::daemon::ipc::{self, Command};
use crate::store::{Store, View};
use crate::{daemon, paths, ui};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const HELP: &str = "clipkeep — fast clipboard history

USAGE:
  clipkeep [show] [TEXT]  open the history window (TEXT = start with this search)
  clipkeep setup          autostart + hotkey (GNOME, Windows, macOS)
  clipkeep uninstall      undo `setup` and stop the daemon (history is kept)
  clipkeep status         number of clips, size on disk, hotkey
  clipkeep list [N]       print the last N clips (default 20)
  clipkeep search WORDS   print clips containing all WORDS
  clipkeep clear          delete all unpinned clips
  clipkeep toggle         ask the daemon to open/close the window
  clipkeep daemon         run the clipboard watcher in the foreground
  clipkeep stop           stop the daemon
  clipkeep bench [N]      measure the store with N synthetic clips (default 100000)

SYNC (end-to-end encrypted, between your devices):
  clipkeep sync setup google        sign in with Google, choose a password (first device or new one)
  clipkeep sync setup folder PATH   same, but through any synced folder (Syncthing, Dropbox, USB)
  clipkeep sync status | now | off
  clipkeep sync password            change the sync password

WINDOW KEYS:
  type to search · ↑/↓ select · Enter copy · Ctrl+1…9 quick copy
  Tab / Shift+Tab switch tab (All, Pinned, Links, your groups)
  Ctrl+P pin · Ctrl+G put into a group · Shift+Del delete · Esc close

FILES:
  history:  ~/.local/share/clipkeep  (Windows %APPDATA%\\clipkeep, macOS ~/Library/Application Support/clipkeep)
  config:   config.txt in the same folder, e.g.  hotkey = Ctrl+Alt+V  /  max_entries = 0";

/// The executable used for background work. On Windows that is `clipkeepw.exe`
/// (no console window); elsewhere the current executable.
pub fn gui_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("clipkeep"));
    if cfg!(windows) {
        let w = exe.with_file_name("clipkeepw.exe");
        if w.exists() {
            return w;
        }
    }
    exe
}

/// Starts the daemon in the background (stderr goes to `daemon.log`) and waits until it answers.
pub fn spawn_daemon() -> Result<(), String> {
    use std::process::{Command as Proc, Stdio};
    let dir = paths::data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .map_err(|e| e.to_string())?;
    Proc::new(gui_exe())
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| format!("cannot start daemon: {e}"))?;
    for _ in 0..150 {
        if ipc::daemon_running() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(format!(
        "daemon did not start, see {}",
        dir.join("daemon.log").display()
    ))
}

pub fn ensure_daemon() -> Result<(), String> {
    if ipc::daemon_running() {
        Ok(())
    } else {
        spawn_daemon()
    }
}

/// Stops the running daemon (if any) and starts a fresh one, e.g. after the sync settings changed.
pub fn restart_daemon() -> Result<(), String> {
    let _ = ipc::request(&Command::Quit);
    for _ in 0..100 {
        if !ipc::daemon_running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    spawn_daemon()
}

fn ask(cmd: &Command) -> Result<Vec<String>, String> {
    ensure_daemon()?;
    ipc::request(cmd).map_err(|e| e.to_string())
}

fn print_hits(lines: &[String]) {
    for (i, h) in lines.iter().filter_map(|l| ipc::parse_hit(l)).enumerate() {
        let pin = if h.pinned { "📌 " } else { "" };
        println!("{:>4}  {pin}{}", i + 1, h.preview);
    }
}

pub fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

fn status() -> Result<(), String> {
    let lines = ask(&Command::Stats)?;
    let get = |k: &str| {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(&format!("S\t{k}\t")).map(str::to_owned))
            .unwrap_or_default()
    };
    let text: u64 = get("text_bytes").parse().unwrap_or(0);
    let disk: u64 = get("disk_bytes").parse().unwrap_or(0);
    let ratio = if disk > 0 && text > disk {
        format!(" ({:.1}× smaller than the text)", text as f64 / disk as f64)
    } else {
        String::new()
    };
    println!(
        "clips:    {} ({} pinned, {} groups)",
        get("entries"),
        get("pinned"),
        get("groups")
    );
    println!("text:     {}", human(text));
    println!("on disk:  {}{ratio}", human(disk));
    println!(
        "images:   {}",
        human(get("images_bytes").parse().unwrap_or(0))
    );
    println!(
        "encrypted: {}",
        if get("encrypted") == "true" {
            "yes (ChaCha20-Poly1305, key outside the data dir)"
        } else {
            "no"
        }
    );
    println!("hotkey:   {}", get("hotkey"));
    println!("data dir: {}", get("data_dir"));
    Ok(())
}

/// Synthetic benchmark of the store (no daemon, temp dir).
fn bench(n: usize) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("clipkeep-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let e = |e: std::io::Error| e.to_string();

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn word(&mut self, cyr: bool) -> String {
            let syl = if cyr { &CYR } else { &LAT };
            let n = 1 + self.next() % 3;
            (0..n).map(|_| syl[(self.next() % 24) as usize]).collect()
        }
    }
    const LAT: [&str; 24] = [
        "ka", "ro", "mi", "te", "lan", "dor", "pe", "su", "vi", "no", "gra", "tis", "el", "ox",
        "ber", "qu", "fa", "zen", "lo", "ri", "mon", "chi", "pa", "ut",
    ];
    const CYR: [&str; 24] = [
        "ка", "ро", "ми", "те", "лан", "дор", "пе", "су", "ви", "но", "гра", "тис", "ель", "ох",
        "бер", "ку", "фа", "зен", "ло", "ри", "мон", "чи", "па", "ут",
    ];
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut clips: Vec<String> = Vec::with_capacity(n);
    let mut raw = 0usize;
    for i in 0..n {
        let r = rng.next() % 100;
        let clip = if r < 10 {
            format!(
                "https://{}.example.com/{}/{}",
                rng.word(false),
                rng.word(false),
                rng.next() % 100_000
            )
        } else if r < 15 && !clips.is_empty() {
            clips[(rng.next() as usize) % clips.len()].clone()
        } else {
            let cyr = r.is_multiple_of(2);
            let words = 3 + rng.next() % 40;
            (0..words)
                .map(|_| rng.word(cyr))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let clip = if i == n / 10 {
            format!("{clip} золотойключик")
        } else {
            clip
        };
        raw += clip.len();
        clips.push(clip);
    }

    let t = Instant::now();
    {
        let mut s = Store::open(&dir, 0).map_err(e)?;
        for (i, c) in clips.iter().enumerate() {
            s.push(c, i as u32).map_err(e)?;
        }
        s.save().map_err(e)?;
    }
    let insert = t.elapsed();
    drop(clips);

    let t = Instant::now();
    let mut s = Store::open(&dir, 0).map_err(e)?;
    let open = t.elapsed();
    let st = s.stats();
    println!(
        "clips:        {n} generated, {} stored after dedupe",
        st.entries
    );
    println!(
        "text:         {} (generated {})",
        human(st.text_bytes),
        human(raw as u64)
    );
    println!(
        "on disk:      {} ({:.1}× smaller, {} blocks)",
        human(st.disk_bytes),
        st.text_bytes as f64 / st.disk_bytes.max(1) as f64,
        st.blocks
    );
    println!(
        "insert+save:  {:.1} ms total, {:.1} µs per clip",
        insert.as_secs_f64() * 1e3,
        insert.as_secs_f64() * 1e6 / n as f64
    );
    println!("open:         {:.1} ms", open.as_secs_f64() * 1e3);
    let t = Instant::now();
    s.push("one more clip after reopen", n as u32).map_err(e)?;
    let add = t.elapsed();
    let t = Instant::now();
    s.save().map_err(e)?;
    println!(
        "one copy:     {:.3} ms to add, {:.1} ms to save the index",
        add.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3
    );
    println!("search (limit 200, first run after open / best of 5 warm):");
    for (label, q) in [
        ("empty query", ""),
        ("common word", "ка"),
        ("two words", "ро ми"),
        ("rare, old", "золотойключик"),
        ("no match", "zzqqxx"),
    ] {
        let t = Instant::now();
        let hits = s.search(q, View::All, 200, 120).len();
        let cold = t.elapsed();
        let warm = (0..5)
            .map(|_| {
                let t = Instant::now();
                s.search(q, View::All, 200, 120);
                t.elapsed()
            })
            .min()
            .unwrap_or_default();
        println!(
            "  {label:<12} {:>4} hits  {:>7.2} ms / {:>6.2} ms",
            hits,
            cold.as_secs_f64() * 1e3,
            warm.as_secs_f64() * 1e3
        );
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let rest = || args[1..].join(" ");
    match args.first().map(String::as_str) {
        None => {
            ensure_daemon()?;
            ui::run(String::new())
        }
        Some("show") => {
            ensure_daemon()?;
            ui::run(rest())
        }
        Some("toggle") => ask(&Command::Show).map(|_| ()),
        Some("daemon") => daemon::run(),
        Some("setup") => self::setup::run(),
        Some("uninstall") => self::setup::uninstall(),
        Some("status") => status(),
        Some("list") => {
            let n = match args.get(1) {
                Some(s) => s.parse().map_err(|_| format!("not a number: {s}"))?,
                None => 20,
            };
            print_hits(&ask(&Command::Search {
                view: View::All,
                limit: n,
                query: String::new(),
            })?);
            Ok(())
        }
        Some("search") => {
            print_hits(&ask(&Command::Search {
                view: View::All,
                limit: 50,
                query: rest(),
            })?);
            Ok(())
        }
        Some("clear") => ask(&Command::Clear).map(|_| ()),
        Some("stop") => ipc::request(&Command::Quit)
            .map(|_| ())
            .map_err(|e| format!("daemon not running ({e})")),
        Some("sync") => sync_cmd::run(&args[1..]),
        Some("bench") => bench(match args.get(1) {
            Some(s) => s.parse().map_err(|_| format!("not a number: {s}"))?,
            None => 100_000,
        }),
        Some("help" | "-h" | "--help") => {
            println!("{HELP}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("clipkeep {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{HELP}")),
    }
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clipkeep: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_sizes() {
        assert_eq!(super::human(512), "512 B");
        assert_eq!(super::human(2048), "2.0 KB");
        assert_eq!(super::human(5 << 20), "5.0 MB");
    }
}
