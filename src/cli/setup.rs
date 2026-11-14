//! `clipkeep setup` / `clipkeep uninstall`: start the daemon at login and wire up the hotkey.
//!
//! - GNOME: autostart .desktop file + a custom GNOME shortcut Super+V → `clipkeep show`.
//! - Windows: `HKCU\…\Run` entry for `clipkeepw.exe daemon`; the daemon owns the hotkey.
//! - macOS: a LaunchAgent; the daemon owns the hotkey.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::daemon::ipc::{self, Command};

const KEY_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/clipkeep/";

/// Adds `item` to a gsettings string list like `@as []` or `['/a/', '/b/']`.
pub fn add_to_list(list: &str, item: &str) -> String {
    let mut items = parse_list(list);
    if !items.iter().any(|i| i == item) {
        items.push(item.to_owned());
    }
    format_list(&items)
}

/// Removes `item` from a gsettings string list.
pub fn remove_from_list(list: &str, item: &str) -> String {
    let items: Vec<String> = parse_list(list).into_iter().filter(|i| i != item).collect();
    format_list(&items)
}

fn parse_list(list: &str) -> Vec<String> {
    let inner = list.trim().trim_start_matches("@as").trim();
    let inner = inner.trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('\'').to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn format_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("'{i}'")).collect();
    format!("[{}]", quoted.join(", "))
}

fn exec(program: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn start_daemon_and_report() -> Result<(), String> {
    if !ipc::daemon_running() {
        crate::cli::spawn_daemon()?;
        println!("✓ daemon started");
    }
    let hotkey = ipc::request(&Command::Stats).ok().and_then(|lines| {
        lines
            .iter()
            .find_map(|l| l.strip_prefix("S\thotkey\t").map(str::to_owned))
    });
    if let Some(hk) = hotkey {
        println!("✓ hotkey: {hk}");
    }
    Ok(())
}

fn stop_daemon() {
    if ipc::request(&Command::Quit).is_ok() {
        println!("✓ daemon stopped");
    }
}

#[cfg(target_os = "linux")]
fn autostart_file() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    Ok(std::path::Path::new(&home).join(".config/autostart/clipkeep.desktop"))
}

/// App icon + launcher entry, so the window, the dock and the app grid show the clipkeep logo.
/// GNOME matches the window (app id `clipkeep`) to `clipkeep.desktop` and takes its icon from there.
#[cfg(target_os = "linux")]
fn install_icon(exe: &str) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    let share = std::path::Path::new(&home).join(".local/share");
    let icons = share.join("icons/hicolor/256x256/apps");
    let apps = share.join("applications");
    std::fs::create_dir_all(&icons).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
    std::fs::write(
        icons.join("clipkeep.png"),
        include_bytes!("../../assets/logo-256.png"),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        apps.join("clipkeep.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=clipkeep\nComment=Clipboard history\n\
             Exec={exe} show\nIcon=clipkeep\nTerminal=false\nCategories=Utility;\n\
             StartupWMClass=clipkeep\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    // Best effort: refresh the icon cache so the icon shows up without logging out.
    let _ = exec(
        "gtk-update-icon-cache",
        &["-f", "-t", &share.join("icons/hicolor").to_string_lossy()],
    );
    println!(
        "✓ icon + launcher: {}",
        apps.join("clipkeep.desktop").display()
    );
    Ok(())
}

/// Registers Super+V as a GNOME custom shortcut for `clipkeep show`.
#[cfg(target_os = "linux")]
fn gnome_shortcut(gs: &dyn Fn(&[&str]) -> Result<String, String>, exe: &str) -> Result<(), String> {
    let media = "org.gnome.settings-daemon.plugins.media-keys";
    let list = gs(&["get", media, "custom-keybindings"])?;
    gs(&[
        "set",
        media,
        "custom-keybindings",
        &add_to_list(&list, KEY_PATH),
    ])?;
    let schema = format!("{media}.custom-keybinding:{KEY_PATH}");
    gs(&["set", &schema, "name", "clipkeep"])?;
    gs(&["set", &schema, "command", &format!("{exe} show")])?;
    gs(&["set", &schema, "binding", "<Super>v"])?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn run() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy();
    let gs = |args: &[&str]| exec("gsettings", args);

    let desktop = autostart_file()?;
    if let Some(dir) = desktop.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        &desktop,
        format!(
            "[Desktop Entry]\nType=Application\nName=clipkeep\nComment=Clipboard history daemon\n\
             Exec={exe} daemon\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    println!("✓ autostart: {}", desktop.display());
    install_icon(&exe)?;

    // GNOME uses Super+V for the notification list; Super+M still opens it.
    let tray = "org.gnome.shell.keybindings";
    if let Ok(list) = gs(&["get", tray, "toggle-message-tray"]) {
        let new = remove_from_list(&list, "<Super>v");
        if new != format_list(&parse_list(&list)) {
            gs(&["set", tray, "toggle-message-tray", &new])?;
            println!("✓ Super+V removed from GNOME notification list (Super+M still works)");
        }
    }

    let has_shortcut = gnome_shortcut(&gs, &exe);
    match &has_shortcut {
        Ok(()) => println!("✓ shortcut: Super+V → {exe} show"),
        // Not GNOME (KDE, Xfce, a bare WM...): everything else still works.
        Err(e) => println!(
            "! no GNOME shortcut ({e}).\n  Bind a key in your desktop's settings to the command: {exe} show"
        ),
    }

    start_daemon_and_report()?;
    if has_shortcut.is_ok() {
        println!("\nDone. Copy some text, then press Super+V.");
    } else {
        println!("\nDone. Copy some text, then press the key you bound to `{exe} show`.");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<(), String> {
    let gs = |args: &[&str]| exec("gsettings", args);
    if let Ok(path) = autostart_file() {
        if std::fs::remove_file(&path).is_ok() {
            println!("✓ autostart removed");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let share = std::path::Path::new(&home).join(".local/share");
        let _ = std::fs::remove_file(share.join("applications/clipkeep.desktop"));
        let _ = std::fs::remove_file(share.join("icons/hicolor/256x256/apps/clipkeep.png"));
    }
    let media = "org.gnome.settings-daemon.plugins.media-keys";
    if let Ok(list) = gs(&["get", media, "custom-keybindings"]) {
        gs(&[
            "set",
            media,
            "custom-keybindings",
            &remove_from_list(&list, KEY_PATH),
        ])?;
        let schema = format!("{media}.custom-keybinding:{KEY_PATH}");
        for key in ["name", "command", "binding"] {
            let _ = gs(&["reset", &schema, key]);
        }
        println!("✓ Super+V shortcut removed");
    }
    let tray = "org.gnome.shell.keybindings";
    if let Ok(list) = gs(&["get", tray, "toggle-message-tray"]) {
        let mut items = parse_list(&list);
        if !items.iter().any(|i| i == "<Super>v") {
            items.insert(0, "<Super>v".into());
            gs(&["set", tray, "toggle-message-tray", &format_list(&items)])?;
            println!("✓ Super+V gives the GNOME notification list back");
        }
    }
    stop_daemon();
    println!("History is kept in {}", crate::paths::data_dir().display());
    Ok(())
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
pub fn run() -> Result<(), String> {
    let gui = crate::cli::gui_exe();
    let value = format!("\"{}\" daemon", gui.display());
    exec(
        "reg",
        &[
            "add", RUN_KEY, "/v", "clipkeep", "/t", "REG_SZ", "/d", &value, "/f",
        ],
    )?;
    println!("✓ autostart: {RUN_KEY}\\clipkeep");
    start_daemon_and_report()?;
    println!("\nDone. Copy some text, then press the hotkey above.");
    println!(
        "Change it with `hotkey = …` in {}",
        crate::paths::data_dir().join("config.txt").display()
    );
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> Result<(), String> {
    if exec("reg", &["delete", RUN_KEY, "/v", "clipkeep", "/f"]).is_ok() {
        println!("✓ autostart removed");
    }
    stop_daemon();
    println!("History is kept in {}", crate::paths::data_dir().display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    Ok(std::path::Path::new(&home).join("Library/LaunchAgents/com.clipkeep.daemon.plist"))
}

#[cfg(target_os = "macos")]
pub fn run() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let xml = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let plist = plist_path()?;
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        &plist,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>com.clipkeep.daemon</string>\n\
             <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>\n\
             <key>RunAtLoad</key><true/>\n\
             <key>ProcessType</key><string>Interactive</string>\n\
             </dict></plist>\n",
            xml(&exe.to_string_lossy())
        ),
    )
    .map_err(|e| e.to_string())?;
    let p = plist.to_string_lossy();
    stop_daemon();
    let _ = exec("launchctl", &["unload", &p]);
    exec("launchctl", &["load", "-w", &p])?;
    println!("✓ autostart: {p}");
    for _ in 0..100 {
        if ipc::daemon_running() {
            break; // launchd started it
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    start_daemon_and_report()?;
    println!("\nDone. Copy some text, then press the hotkey above.");
    println!(
        "Change it with `hotkey = …` in {}",
        crate::paths::data_dir().join("config.txt").display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<(), String> {
    let plist = plist_path()?;
    let p = plist.to_string_lossy();
    let _ = exec("launchctl", &["unload", &p]);
    if std::fs::remove_file(&plist).is_ok() {
        println!("✓ autostart removed");
    }
    stop_daemon();
    println!("History is kept in {}", crate::paths::data_dir().display());
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn run() -> Result<(), String> {
    Err("`clipkeep setup` is not supported on this OS; run `clipkeep daemon` at login".into())
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn uninstall() -> Result<(), String> {
    stop_daemon();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_to_empty_and_existing() {
        assert_eq!(add_to_list("@as []", "/x/"), "['/x/']");
        assert_eq!(add_to_list("['/a/']", "/x/"), "['/a/', '/x/']");
        assert_eq!(add_to_list("['/a/', '/x/']", "/x/"), "['/a/', '/x/']");
    }

    #[test]
    fn remove_binding() {
        assert_eq!(
            remove_from_list("['<Super>v', '<Super>m']", "<Super>v"),
            "['<Super>m']"
        );
        assert_eq!(remove_from_list("['<Super>m']", "<Super>v"), "['<Super>m']");
        assert_eq!(remove_from_list("@as []", "<Super>v"), "[]");
    }
}
