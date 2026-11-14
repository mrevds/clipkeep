use std::env;
use std::path::PathBuf;

fn home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

/// Linux: $XDG_DATA_HOME/clipkeep, Windows: %APPDATA%\clipkeep,
/// macOS: ~/Library/Application Support/clipkeep.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CLIPKEEP_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "windows")]
    let base = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(home);
    #[cfg(target_os = "macos")]
    let base = home().join("Library/Application Support");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    base.join("clipkeep")
}

#[cfg(unix)]
pub fn socket_file() -> PathBuf {
    if let Some(dir) = env::var_os("CLIPKEEP_DIR") {
        return PathBuf::from(dir).join("clipkeep.sock");
    }
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("clipkeep.sock"),
        None => data_dir().join("clipkeep.sock"),
    }
}

/// Linux: lets a second `clipkeep show` close the open window (toggle).
#[cfg(target_os = "linux")]
pub fn popup_socket_file() -> PathBuf {
    socket_file().with_file_name("clipkeep-ui.sock")
}

/// Encryption key. Kept away from the data dir, so copying or syncing the history alone
/// never exposes it. Linux: ~/.config/clipkeep, Windows: %LOCALAPPDATA%\\clipkeep (not roamed),
/// macOS: ~/Library/Application Support/clipkeep-key.
pub fn key_file() -> PathBuf {
    if let Some(dir) = env::var_os("CLIPKEEP_DIR") {
        return PathBuf::from(dir).join("key.bin");
    }
    #[cfg(target_os = "windows")]
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(home)
        .join("clipkeep");
    #[cfg(target_os = "macos")]
    let base = home().join("Library/Application Support/clipkeep-key");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("clipkeep");
    base.join("key.bin")
}
