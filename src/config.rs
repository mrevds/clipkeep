//! `config.txt` in the data dir, `key = value` lines, `#` comments:
//!
//! ```text
//! hotkey = Win+Alt+V     # Windows / macOS only; on GNOME the shortcut lives in GNOME settings
//! max_entries = 0        # 0 = unlimited
//! auto_paste = true      # press Ctrl+V after a clip is chosen
//! google_client_id = ...  # for `clipkeep sync setup google` (your own Google Cloud OAuth client)
//! google_client_secret = ...
//! image_limit_mb = 1024  # images take at most this much disk (oldest unpinned go first); 0 = no limit
//! secret_seconds = 60    # passwords and keys: memory only, forgotten after this; 0 = treat as normal clips
//! ```

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Win on Windows, Cmd on macOS, Super on Linux.
    pub meta: bool,
    /// `A`–`Z` or `0`–`9`.
    pub key: char,
}

impl Hotkey {
    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut hk = Hotkey {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key: '\0',
        };
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        let (last, mods) = parts.split_last()?;
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => hk.ctrl = true,
                "alt" | "option" | "opt" => hk.alt = true,
                "shift" => hk.shift = true,
                "win" | "super" | "cmd" | "command" | "meta" => hk.meta = true,
                _ => return None,
            }
        }
        let mut chars = last.chars();
        let key = chars.next()?.to_ascii_uppercase();
        if chars.next().is_some() || !key.is_ascii_alphanumeric() || mods.is_empty() {
            return None;
        }
        hk.key = key;
        Some(hk)
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let meta = if cfg!(target_os = "macos") {
            "Cmd"
        } else if cfg!(windows) {
            "Win"
        } else {
            "Super"
        };
        let alt = if cfg!(target_os = "macos") {
            "Option"
        } else {
            "Alt"
        };
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.meta, meta),
            (self.alt, alt),
            (self.shift, "Shift"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        write!(f, "{}", self.key)
    }
}

/// Hotkeys tried in order when `config.txt` has none.
/// Windows: Win+V belongs to the built-in clipboard, so Win+Alt+V, then Ctrl+Alt+V.
/// macOS: Cmd+Shift+V is "paste and match style" in many apps, so Ctrl+Cmd+V.
pub fn default_hotkeys() -> Vec<Hotkey> {
    let names: &[&str] = if cfg!(windows) {
        &["Win+Alt+V", "Ctrl+Alt+V"]
    } else if cfg!(target_os = "macos") {
        &["Ctrl+Cmd+V", "Ctrl+Option+V"]
    } else {
        &[]
    };
    names.iter().filter_map(|n| Hotkey::parse(n)).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub hotkey: Option<Hotkey>,
    pub max_entries: usize,
    pub auto_paste: bool,
    pub image_limit_mb: u64,
    pub secret_seconds: u64,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: None,
            max_entries: 0,
            auto_paste: true,
            image_limit_mb: 1024,
            secret_seconds: 60,
            google_client_id: None,
            google_client_secret: None,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let mut errors = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                errors.push(format!("config.txt:{}: expected `key = value`", n + 1));
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "hotkey" => match Hotkey::parse(value) {
                    Some(hk) => cfg.hotkey = Some(hk),
                    None => errors.push(format!(
                        "config.txt:{}: bad hotkey `{value}` (example: Win+Alt+V)",
                        n + 1
                    )),
                },
                "auto_paste" => match value {
                    "true" | "yes" | "on" | "1" => cfg.auto_paste = true,
                    "false" | "no" | "off" | "0" => cfg.auto_paste = false,
                    _ => errors.push(format!(
                        "config.txt:{}: auto_paste must be true or false",
                        n + 1
                    )),
                },
                "google_client_id" => cfg.google_client_id = Some(value.to_owned()),
                "google_client_secret" => cfg.google_client_secret = Some(value.to_owned()),
                "image_limit_mb" => match value.parse() {
                    Ok(v) => cfg.image_limit_mb = v,
                    Err(_) => errors.push(format!(
                        "config.txt:{}: image_limit_mb must be a number",
                        n + 1
                    )),
                },
                "secret_seconds" => match value.parse() {
                    Ok(v) => cfg.secret_seconds = v,
                    Err(_) => errors.push(format!(
                        "config.txt:{}: secret_seconds must be a number",
                        n + 1
                    )),
                },
                "max_entries" => match value.parse() {
                    Ok(v) => cfg.max_entries = v,
                    Err(_) => errors.push(format!(
                        "config.txt:{}: max_entries must be a number",
                        n + 1
                    )),
                },
                other => errors.push(format!("config.txt:{}: unknown key `{other}`", n + 1)),
            }
        }
        (cfg, errors)
    }

    pub fn load() -> Config {
        let path = crate::paths::data_dir().join("config.txt");
        let Ok(text) = std::fs::read_to_string(path) else {
            return Config::default();
        };
        let (cfg, errors) = Config::parse(&text);
        for e in errors {
            eprintln!("clipkeep: {e}");
        }
        cfg
    }

    pub fn hotkeys(&self) -> Vec<Hotkey> {
        match self.hotkey {
            Some(hk) => vec![hk],
            None => default_hotkeys(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hotkeys() {
        let hk = Hotkey::parse("win + alt + v").unwrap();
        assert!(hk.meta && hk.alt && !hk.ctrl && !hk.shift);
        assert_eq!(hk.key, 'V');
        assert_eq!(Hotkey::parse("Ctrl+Shift+1").unwrap().key, '1');
        assert_eq!(Hotkey::parse("Cmd+Option+K"), Hotkey::parse("meta+alt+k"));
        assert!(Hotkey::parse("V").is_none());
        assert!(Hotkey::parse("Ctrl+F1").is_none());
        assert!(Hotkey::parse("Hyper+V").is_none());
        assert!(Hotkey::parse("Ctrl+Ж").is_none());
        assert!(Hotkey::parse("").is_none());
    }

    #[test]
    fn display_roundtrips() {
        for s in ["Ctrl+Alt+V", "Ctrl+Shift+C"] {
            let hk = Hotkey::parse(s).unwrap();
            assert_eq!(Hotkey::parse(&hk.to_string()), Some(hk));
        }
    }

    #[test]
    fn parse_config() {
        let (cfg, errors) = Config::parse("# мой конфиг\nhotkey = Ctrl+Alt+B\nmax_entries = 5000 # limit\n\nfoo = 1\nmax_entries=x\n");
        assert_eq!(cfg.hotkey, Hotkey::parse("Ctrl+Alt+B"));
        assert_eq!(cfg.max_entries, 5000);
        assert_eq!(errors.len(), 2);
        assert_eq!(cfg.hotkeys(), vec![Hotkey::parse("Ctrl+Alt+B").unwrap()]);
        assert_eq!(Config::default().max_entries, 0);
        assert!(Config::default().auto_paste);
        assert_eq!(Config::default().image_limit_mb, 1024);
        assert_eq!(Config::parse("image_limit_mb = 50").0.image_limit_mb, 50);
        assert!(!Config::parse("auto_paste = off").0.auto_paste);
        assert_eq!(Config::default().secret_seconds, 60);
        assert_eq!(Config::parse("secret_seconds = 0").0.secret_seconds, 0);
        assert_eq!(Config::parse("auto_paste = maybe").1.len(), 1);
    }
}
