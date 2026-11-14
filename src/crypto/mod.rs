//! Encryption at rest: ChaCha20-Poly1305 with a random 256-bit key that belongs to this user.
//! Each blob is `nonce (12) || ciphertext || tag (16)`. The key is generated on first run and
//! lives in `paths::key_file()`. There is no key in the source and none is shared between users.

pub mod vault;

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::io::{self, ErrorKind, Write};
use std::path::Path;

/// A fresh random 256-bit key.
pub fn random_key() -> [u8; 32] {
    ChaCha20Poly1305::generate_key(&mut OsRng).into()
}

pub const OVERHEAD: usize = 12 + 16;

#[derive(Clone)]
pub struct Cipher(ChaCha20Poly1305);

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Cipher {
        Cipher(ChaCha20Poly1305::new(Key::from_slice(key)))
    }

    /// `aad` is authenticated but not stored: a blob cannot be moved to another place unnoticed.
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Vec<u8> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = self
            .0
            .encrypt(&nonce, Payload { msg: plain, aad })
            .expect("encryption cannot fail");
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// `None` means a wrong key or damaged data.
    pub fn open(&self, data: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        if data.len() < OVERHEAD {
            return None;
        }
        let (nonce, ct) = data.split_at(12);
        self.0
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
            .ok()
    }
}

/// Loads the key file. If there is none, creates one, unless `must_exist` is set: then the
/// history on disk is already encrypted and a fresh key would silently lock it forever.
pub fn load_or_create(path: &Path, must_exist: bool) -> io::Result<Cipher> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let key: [u8; 32] = bytes.try_into().map_err(|_| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("{}: key file must be exactly 32 bytes", path.display()),
                )
            })?;
            return Ok(Cipher::new(&key));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if must_exist {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!("the history is encrypted but the key file {} is missing (restore it from a backup)", path.display()),
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let key = ChaCha20Poly1305::generate_key(&mut OsRng);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(&key)?;
    Ok(Cipher::new(&key.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_tamper() {
        let c = Cipher::new(&[7; 32]);
        let sealed = c.seal("секрет".as_bytes(), b"aad");
        assert_eq!(sealed.len(), "секрет".len() + OVERHEAD);
        assert!(!sealed.windows(4).any(|w| w == "секр".as_bytes()));
        assert_eq!(c.open(&sealed, b"aad").unwrap(), "секрет".as_bytes());
        assert!(c.open(&sealed, b"other").is_none());
        assert!(Cipher::new(&[8; 32]).open(&sealed, b"aad").is_none());
        let mut bad = sealed.clone();
        bad[20] ^= 1;
        assert!(c.open(&bad, b"aad").is_none());
        assert!(c.open(&[0; 5], b"").is_none());
        assert_ne!(c.seal(b"x", b""), c.seal(b"x", b""), "nonce must be fresh");
    }

    #[test]
    fn key_file_is_created_once_and_private() {
        let dir = std::env::temp_dir().join(format!("clipkeep-key-{}", std::process::id()));
        let path = dir.join("sub/key.bin");
        assert!(load_or_create(&path, true).is_err());
        let a = load_or_create(&path, false).unwrap();
        let b = load_or_create(&path, true).unwrap();
        assert_eq!(b.open(&a.seal(b"hi", b""), b"").unwrap(), b"hi");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&path, b"short").unwrap();
        assert!(load_or_create(&path, false).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
