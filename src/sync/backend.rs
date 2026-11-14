//! Where the encrypted sync files live. A backend is a flat bag of named blobs; it never sees
//! anything but ciphertext (and the public vault).

use std::io::{self, ErrorKind};
use std::path::PathBuf;

pub trait Backend: Send {
    fn name(&self) -> String;
    fn list(&mut self) -> io::Result<Vec<String>>;
    fn get(&mut self, name: &str) -> io::Result<Vec<u8>>;
    /// Creates or replaces.
    fn put(&mut self, name: &str, data: &[u8]) -> io::Result<()>;
}

/// A plain folder: Syncthing, Dropbox, a USB stick, a network share — anything that copies files.
pub struct FolderBackend {
    dir: PathBuf,
}

impl FolderBackend {
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<FolderBackend> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(FolderBackend { dir })
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        let ok = !name.is_empty()
            && name.len() < 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            && !name.starts_with('.');
        if ok {
            Ok(self.dir.join(name))
        } else {
            Err(io::Error::new(ErrorKind::InvalidInput, "bad object name"))
        }
    }
}

impl Backend for FolderBackend {
    fn name(&self) -> String {
        format!("folder {}", self.dir.display())
    }

    fn list(&mut self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            if let Some(n) = e.file_name().to_str() {
                if !n.starts_with('.') && !n.ends_with(".part") {
                    out.push(n.to_owned());
                }
            }
        }
        Ok(out)
    }

    fn get(&mut self, name: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.path(name)?)
    }

    fn put(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        let path = self.path(name)?;
        let tmp = self.dir.join(format!("{name}.part"));
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
pub mod memory {
    //! An in-memory backend shared between "devices" in tests.
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct Memory(pub Arc<Mutex<BTreeMap<String, Vec<u8>>>>);

    impl Backend for Memory {
        fn name(&self) -> String {
            "memory".into()
        }
        fn list(&mut self) -> io::Result<Vec<String>> {
            Ok(self.0.lock().unwrap().keys().cloned().collect())
        }
        fn get(&mut self, name: &str) -> io::Result<Vec<u8>> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or_else(|| io::Error::new(ErrorKind::NotFound, name.to_owned()))
        }
        fn put(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(name.to_owned(), data.to_vec());
            Ok(())
        }
    }
}
