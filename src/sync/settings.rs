//! Sync settings of this device: which backend, the device id, the master key and the Google
//! refresh token. One file next to `key.bin`, readable by the owner only. Absent file = sync off.

use super::backend::{Backend, FolderBackend};
use serde::{Deserialize, Serialize};
use std::io::{self, ErrorKind};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BackendCfg {
    Folder {
        path: String,
    },
    Google {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub device: String,
    /// The sync master key, hex.
    pub master: String,
    pub backend: BackendCfg,
}

pub fn path() -> PathBuf {
    crate::paths::key_file().with_file_name("sync.json")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A short random id that tells this device's files from the others'.
pub fn new_device_id() -> String {
    hex(&crate::crypto::random_key()[..4])
}

impl BackendCfg {
    pub fn open(&self) -> io::Result<Box<dyn Backend>> {
        Ok(match self {
            BackendCfg::Folder { path } => Box::new(FolderBackend::new(path)?),
            #[cfg(feature = "google")]
            BackendCfg::Google {
                client_id,
                client_secret,
                refresh_token,
            } => Box::new(super::google::GoogleDrive::new(
                client_id,
                client_secret,
                refresh_token,
            )),
            #[cfg(not(feature = "google"))]
            BackendCfg::Google { .. } => {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    "this build has no Google support (feature `google`)",
                ))
            }
        })
    }
}

impl Settings {
    pub fn load() -> io::Result<Option<Settings>> {
        match std::fs::read(path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                io::Error::new(ErrorKind::InvalidData, format!("{}: {e}", path().display()))
            }),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        use std::io::Write;
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&tmp)?
            .write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn remove() -> io::Result<()> {
        match std::fs::remove_file(path()) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn master_key(&self) -> io::Result<[u8; 32]> {
        unhex(&self.master)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "sync.json: bad master key"))
    }

    pub fn backend(&self) -> io::Result<Box<dyn Backend>> {
        self.backend.open()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let k = crate::crypto::random_key();
        assert_eq!(unhex::<32>(&hex(&k)), Some(k));
        assert_eq!(unhex::<4>("zzzzzzzz"), None);
        assert_eq!(unhex::<4>("abcd"), None);
        assert_eq!(new_device_id().len(), 8);
    }
}
