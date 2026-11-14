//! `clipkeep sync ...`: set up, inspect and stop sync.

use crate::crypto::vault::{self, Vault};
use crate::daemon::ipc::Command;
use crate::paths;
use crate::sync::onboard::{self, Pending};
use crate::sync::settings::{BackendCfg, Settings};
use crate::sync::VAULT_FILE;

const USAGE: &str = "usage:
  clipkeep sync setup google [--recovery]
  clipkeep sync setup folder PATH [--recovery]
  clipkeep sync status | now | off | password";

pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("setup") => setup(&args[1..]),
        Some("status") | None => status(),
        Some("now") => {
            super::ask(&Command::Sync)?;
            println!("Sync started. See: clipkeep sync status");
            Ok(())
        }
        Some("off") => off(),
        Some("password") => change_password(),
        Some(other) => Err(format!("unknown sync command `{other}`\n{USAGE}")),
    }
}

/// Hidden prompt on a terminal. When input is piped (scripts), reads one line from stdin.
fn prompt(text: &str) -> Result<String, String> {
    use std::io::{BufRead, IsTerminal, Write};
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(text)
            .map_err(|e| format!("cannot read from the terminal: {e}"));
    }
    eprint!("{text}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn new_password() -> Result<String, String> {
    loop {
        let a = prompt("New sync password (10+ characters, a phrase of 3-4 words is best): ")?;
        if let Err(e) = vault::check_password(&a) {
            println!("{e}");
            continue;
        }
        if a == prompt("Repeat it: ")? {
            return Ok(a);
        }
        println!("They do not match, try again.");
    }
}

fn setup(args: &[String]) -> Result<(), String> {
    if Settings::load().map_err(|e| e.to_string())?.is_some() {
        return Err(
            "sync is already set up on this device (clipkeep sync off to remove it)".into(),
        );
    }
    let use_recovery = args.iter().any(|a| a == "--recovery");
    let words: Vec<&String> = args.iter().filter(|a| *a != "--recovery").collect();
    let cfg = match words.first().map(|s| s.as_str()) {
        Some("google") => onboard::google_login()?,
        Some("folder") => {
            let path = words
                .get(1)
                .ok_or("give a folder: clipkeep sync setup folder PATH")?;
            std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
            let abs = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
            BackendCfg::Folder {
                path: abs.display().to_string(),
            }
        }
        _ => return Err(USAGE.into()),
    };
    let pending = Pending::connect(cfg)?;
    if pending.exists() {
        join(&pending, use_recovery)?;
    } else {
        create(&pending)?;
    }
    super::restart_daemon()?;
    println!("\nSync is on. Your history uploads in the background; check: clipkeep sync status");
    Ok(())
}

/// First device: make a master key and protect it with a password and a recovery code.
fn create(pending: &Pending) -> Result<(), String> {
    println!("No sync data here yet: creating it.\n");
    let password = new_password()?;
    println!("Deriving the key (takes a moment)...");
    let recovery = pending.create(&password)?;
    println!(
        "\nRECOVERY CODE. Write it down and keep it somewhere safe:\n\n    {recovery}\n\n\
         If you forget the password, this code is the ONLY way to open your synced history.\n\
         Nobody else (not Google, not us) can recover it. It is not shown again."
    );
    Ok(())
}

/// Another device: unlock the existing vault with the password (or the recovery code).
fn join(pending: &Pending, use_recovery: bool) -> Result<(), String> {
    println!("Found your sync data. Unlock it on this device.");
    for _ in 0..3 {
        let secret = prompt(if use_recovery {
            "Recovery code: "
        } else {
            "Sync password: "
        })?;
        match pending.join(&secret, use_recovery) {
            Ok(()) => return Ok(()),
            Err(e) => println!("{e}. Try again."),
        }
    }
    Err("giving up after 3 wrong attempts".into())
}

fn status() -> Result<(), String> {
    match Settings::load().map_err(|e| e.to_string())? {
        None => {
            println!("sync: off (clipkeep sync setup google)");
            Ok(())
        }
        Some(s) => {
            println!("device:  {}", s.device);
            let lines = super::ask(&Command::Stats)?;
            let line = lines
                .iter()
                .find_map(|l| l.strip_prefix("S\tsync\t"))
                .unwrap_or("daemon has not started sync");
            println!("sync:    {line}");
            Ok(())
        }
    }
}

/// Turns sync off on this device (used by `clipkeep sync off` and the popup).
pub fn disable() -> Result<(), String> {
    Settings::remove().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(paths::data_dir().join("sync-state.bin"));
    super::restart_daemon()
}

fn off() -> Result<(), String> {
    if Settings::load().map_err(|e| e.to_string())?.is_none() {
        println!("sync is already off");
        return Ok(());
    }
    disable()?;
    println!("Sync is off on this device. Local history is untouched; the encrypted copy in the cloud stays\nuntil you delete it yourself (Google Drive: Settings > Manage apps > clipkeep > Delete hidden app data).");
    Ok(())
}

fn change_password() -> Result<(), String> {
    let s = Settings::load()
        .map_err(|e| e.to_string())?
        .ok_or("sync is off")?;
    let mut backend = s.backend().map_err(|e| e.to_string())?;
    let master = s.master_key().map_err(|e| e.to_string())?;
    let bytes = backend
        .get(VAULT_FILE)
        .map_err(|e| format!("cannot download the vault: {e}"))?;
    let vault = Vault::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let password = new_password()?;
    let changed = vault.with_new_password(&master, &password)?;
    backend
        .put(VAULT_FILE, &changed.to_bytes())
        .map_err(|e| format!("cannot upload: {e}"))?;
    println!("Password changed. Devices that are already signed in keep working; new ones need the new password.");
    Ok(())
}
