//! Turning sync on, without any terminal: shared by the popup's Sync screen and `clipkeep sync setup`.
//!
//! Flow: sign in (backend config) -> `Pending::connect` looks at the storage -> either `create`
//! (first device: new master key, password, recovery code) or `join` (another device: unlock the
//! existing vault with the password or the recovery code). Both save `sync.json` on success; the
//! caller restarts the daemon so it picks the new settings up.

use super::settings::{self, BackendCfg, Settings};
use super::VAULT_FILE;
use crate::crypto::vault::{self, Vault};

pub struct Pending {
    cfg: BackendCfg,
    vault: Option<Vault>,
}

impl Pending {
    /// Looks into the storage: is there already a vault (another device set sync up)?
    pub fn connect(cfg: BackendCfg) -> Result<Pending, String> {
        let mut backend = cfg.open().map_err(|e| e.to_string())?;
        let names = backend
            .list()
            .map_err(|e| format!("cannot reach the storage: {e}"))?;
        let vault = if names.iter().any(|n| n == VAULT_FILE) {
            let bytes = backend
                .get(VAULT_FILE)
                .map_err(|e| format!("cannot download the vault: {e}"))?;
            Some(Vault::from_bytes(&bytes).map_err(|e| e.to_string())?)
        } else {
            None
        };
        Ok(Pending { cfg, vault })
    }

    /// True: another device already set sync up, this one has to unlock it.
    pub fn exists(&self) -> bool {
        self.vault.is_some()
    }

    fn save(&self, master: &[u8; 32]) -> Result<(), String> {
        Settings {
            device: settings::new_device_id(),
            master: settings::hex(master),
            backend: self.cfg.clone(),
        }
        .save()
        .map_err(|e| e.to_string())
    }

    /// First device. Returns the recovery code: show it once, it cannot be shown again.
    pub fn create(&self, password: &str) -> Result<String, String> {
        vault::check_password(password)?;
        let master = crate::crypto::random_key();
        let (vault, recovery) = Vault::create(&master, password, vault::COST)?;
        let mut backend = self.cfg.open().map_err(|e| e.to_string())?;
        backend
            .put(VAULT_FILE, &vault.to_bytes())
            .map_err(|e| format!("cannot upload: {e}"))?;
        self.save(&master)?;
        Ok(recovery)
    }

    /// Another device: `secret` is the password, or the recovery code when `recovery` is set.
    pub fn join(&self, secret: &str, recovery: bool) -> Result<(), String> {
        let vault = self.vault.as_ref().ok_or("no sync data to join")?;
        let master = if recovery {
            vault.unlock_with_recovery(secret)
        } else {
            vault.unlock_with_password(secret)
        };
        match master {
            Some(m) => self.save(&m),
            None => Err(format!(
                "Wrong {}",
                if recovery {
                    "recovery code"
                } else {
                    "password"
                }
            )),
        }
    }
}

/// Browser sign-in with the OAuth client built into the program (or the one in `config.txt`).
#[cfg(feature = "google")]
pub fn google_login() -> Result<BackendCfg, String> {
    let c = crate::config::Config::load();
    let id = c
        .google_client_id
        .unwrap_or_else(|| super::google::DEFAULT_CLIENT_ID.to_owned());
    let secret = c
        .google_client_secret
        .unwrap_or_else(|| super::google::DEFAULT_CLIENT_SECRET.to_owned());
    if id.is_empty() || secret.is_empty() {
        return Err(format!(
            "This build has no Google sign-in built in (it comes with the ready-made releases).\n\
             Install a release, or put google_client_id and google_client_secret into {}",
            crate::paths::data_dir().join("config.txt").display()
        ));
    }
    let refresh = super::google::sign_in(&id, &secret).map_err(|e| e.to_string())?;
    Ok(BackendCfg::Google {
        client_id: id,
        client_secret: secret,
        refresh_token: refresh,
    })
}

#[cfg(not(feature = "google"))]
pub fn google_login() -> Result<BackendCfg, String> {
    Err("this build has no Google support (feature `google`)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_join_with_password_and_recovery() {
        let dir = std::env::temp_dir().join(format!("ck-onboard-{}", std::process::id()));
        let data = dir.join("data");
        let shared = dir.join("shared");
        std::env::set_var("CLIPKEEP_DIR", &data);
        let cfg = BackendCfg::Folder {
            path: shared.display().to_string(),
        };

        let first = Pending::connect(cfg.clone()).unwrap();
        assert!(!first.exists());
        assert!(first.create("short").is_err());
        let recovery = first.create("correct horse battery").unwrap();
        let master = Settings::load().unwrap().unwrap().master_key().unwrap();

        // Second device: sees the vault, rejects a wrong password, accepts the right one.
        Settings::remove().unwrap();
        let second = Pending::connect(cfg).unwrap();
        assert!(second.exists());
        assert!(second.join("wrong password!!", false).is_err());
        assert!(Settings::load().unwrap().is_none());
        second.join("correct horse battery", false).unwrap();
        assert_eq!(
            Settings::load().unwrap().unwrap().master_key().unwrap(),
            master
        );
        Settings::remove().unwrap();
        second.join(&recovery, true).unwrap();
        assert_eq!(
            Settings::load().unwrap().unwrap().master_key().unwrap(),
            master
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
