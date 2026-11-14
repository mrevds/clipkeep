//! The sync key vault. One random master key encrypts everything that leaves the device; the
//! vault stores that key wrapped twice:
//!
//! - by a key derived from the user's password (Argon2id, deliberately slow and memory-hard);
//! - by a random recovery code the user writes down once.
//!
//! Either unlocks the master key. Changing the password re-wraps only the 32-byte master key,
//! never the data. The vault is safe to keep in the cloud: it contains no secret in clear text.

use super::{random_key, Cipher};
use argon2::{Algorithm, Argon2, Params, Version};
use std::io::{self, ErrorKind};

const MAGIC: &[u8; 4] = b"CKV1";
const AAD_PW: &[u8] = b"clipkeep vault: password";
const AAD_RC: &[u8] = b"clipkeep vault: recovery";
pub const MIN_PASSWORD_CHARS: usize = 10;

/// Argon2id cost. 64 MiB and 3 passes take roughly 0.2-0.5 s per guess on a laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cost {
    pub memory_kib: u32,
    pub passes: u32,
}

pub const COST: Cost = Cost {
    memory_kib: 64 * 1024,
    passes: 3,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vault {
    cost: Cost,
    salt: [u8; 16],
    by_password: Vec<u8>,
    by_recovery: Vec<u8>,
}

fn derive(password: &str, salt: &[u8; 16], cost: Cost) -> Option<[u8; 32]> {
    let params = Params::new(cost.memory_kib, cost.passes, 1, Some(32)).ok()?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .ok()?;
    Some(out)
}

pub fn check_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters (a phrase of 3-4 words is best)"
        ));
    }
    Ok(())
}

impl Vault {
    /// New vault around `master`. Returns it with the recovery code to show to the user once.
    pub fn create(
        master: &[u8; 32],
        password: &str,
        cost: Cost,
    ) -> Result<(Vault, String), String> {
        check_password(password)?;
        let recovery = random_key();
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&random_key()[..16]);
        let pw_key = derive(password, &salt, cost).ok_or("bad Argon2 parameters")?;
        let vault = Vault {
            cost,
            salt,
            by_password: Cipher::new(&pw_key).seal(master, AAD_PW),
            by_recovery: Cipher::new(&recovery).seal(master, AAD_RC),
        };
        Ok((vault, format_recovery(&recovery)))
    }

    /// `None`: wrong password.
    pub fn unlock_with_password(&self, password: &str) -> Option<[u8; 32]> {
        let key = derive(password, &self.salt, self.cost)?;
        Cipher::new(&key)
            .open(&self.by_password, AAD_PW)?
            .try_into()
            .ok()
    }

    /// `None`: wrong or mistyped recovery code.
    pub fn unlock_with_recovery(&self, code: &str) -> Option<[u8; 32]> {
        let key = parse_recovery(code)?;
        Cipher::new(&key)
            .open(&self.by_recovery, AAD_RC)?
            .try_into()
            .ok()
    }

    /// Same master key and recovery code, new password (new salt).
    pub fn with_new_password(&self, master: &[u8; 32], password: &str) -> Result<Vault, String> {
        check_password(password)?;
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&random_key()[..16]);
        let key = derive(password, &salt, self.cost).ok_or("bad Argon2 parameters")?;
        Ok(Vault {
            cost: self.cost,
            salt,
            by_password: Cipher::new(&key).seal(master, AAD_PW),
            by_recovery: self.by_recovery.clone(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&self.cost.memory_kib.to_le_bytes());
        out.extend_from_slice(&self.cost.passes.to_le_bytes());
        out.extend_from_slice(&self.salt);
        for part in [&self.by_password, &self.by_recovery] {
            out.extend_from_slice(&(part.len() as u16).to_le_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> io::Result<Vault> {
        let bad = |m: &str| io::Error::new(ErrorKind::InvalidData, format!("vault: {m}"));
        let b = b
            .strip_prefix(MAGIC.as_slice())
            .ok_or_else(|| bad("wrong magic"))?;
        if b.len() < 24 {
            return Err(bad("too short"));
        }
        let cost = Cost {
            memory_kib: u32::from_le_bytes(b[0..4].try_into().expect("4")),
            passes: u32::from_le_bytes(b[4..8].try_into().expect("4")),
        };
        // A hostile vault must not make us allocate gigabytes.
        if cost.memory_kib > 1 << 20 || cost.passes > 64 || cost.memory_kib < 8 || cost.passes == 0
        {
            return Err(bad("unreasonable Argon2 cost"));
        }
        let salt: [u8; 16] = b[8..24].try_into().expect("16");
        let mut rest = &b[24..];
        let mut parts = Vec::new();
        for _ in 0..2 {
            let n = u16::from_le_bytes(
                rest.get(..2)
                    .ok_or_else(|| bad("truncated"))?
                    .try_into()
                    .expect("2"),
            ) as usize;
            parts.push(rest.get(2..2 + n).ok_or_else(|| bad("truncated"))?.to_vec());
            rest = &rest[2 + n..];
        }
        let by_recovery = parts.pop().expect("two parts");
        let by_password = parts.pop().expect("two parts");
        Ok(Vault {
            cost,
            salt,
            by_password,
            by_recovery,
        })
    }
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// 32 bytes as 52 base32 letters in groups of 4: `ABCD-EFGH-...`.
fn format_recovery(key: &[u8; 32]) -> String {
    let (mut acc, mut bits) = (0u32, 0);
    let mut letters = Vec::new();
    for &b in key {
        acc = acc << 8 | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            letters.push(ALPHABET[(acc >> bits & 31) as usize]);
        }
    }
    if bits > 0 {
        letters.push(ALPHABET[(acc << (5 - bits) & 31) as usize]);
    }
    letters
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect::<Vec<_>>()
        .join("-")
}

fn parse_recovery(code: &str) -> Option<[u8; 32]> {
    let (mut acc, mut bits) = (0u32, 0);
    let mut out = Vec::new();
    for c in code.chars().filter(|c| !c.is_whitespace() && *c != '-') {
        let v = ALPHABET
            .iter()
            .position(|&a| a as char == c.to_ascii_uppercase())? as u32;
        acc = acc << 5 | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Cost = Cost {
        memory_kib: 64,
        passes: 1,
    };

    #[test]
    fn password_and_recovery_both_unlock() {
        let master = random_key();
        let (vault, code) = Vault::create(&master, "correct horse battery", FAST).unwrap();
        assert_eq!(
            vault.unlock_with_password("correct horse battery"),
            Some(master)
        );
        assert_eq!(vault.unlock_with_password("correct horse batterz"), None);
        assert_eq!(vault.unlock_with_recovery(&code), Some(master));
        assert_eq!(
            vault.unlock_with_recovery(&code.to_lowercase().replace('-', " ")),
            Some(master)
        );
        assert_eq!(vault.unlock_with_recovery("AAAA-AAAA"), None);
        assert_eq!(code.len(), 52 + 12);
    }

    #[test]
    fn serialization_and_hostile_input() {
        let master = random_key();
        let (vault, _) = Vault::create(&master, "пароль из нескольких слов", FAST).unwrap();
        let back = Vault::from_bytes(&vault.to_bytes()).unwrap();
        assert_eq!(back, vault);
        assert_eq!(
            back.unlock_with_password("пароль из нескольких слов"),
            Some(master)
        );
        assert!(Vault::from_bytes(b"nope").is_err());
        let mut b = vault.to_bytes();
        b[4..8].copy_from_slice(&u32::MAX.to_le_bytes()); // 4 TiB of Argon2 memory
        assert!(Vault::from_bytes(&b).is_err());
        assert!(Vault::from_bytes(&vault.to_bytes()[..30]).is_err());
    }

    #[test]
    fn password_change_keeps_master_and_recovery() {
        let master = random_key();
        let (vault, code) = Vault::create(&master, "first password 1", FAST).unwrap();
        let changed = vault
            .with_new_password(&master, "second password 2")
            .unwrap();
        assert_eq!(
            changed.unlock_with_password("second password 2"),
            Some(master)
        );
        assert_eq!(changed.unlock_with_password("first password 1"), None);
        assert_eq!(changed.unlock_with_recovery(&code), Some(master));
    }

    #[test]
    fn short_passwords_are_refused() {
        assert!(Vault::create(&random_key(), "short", FAST).is_err());
        assert!(check_password("ровно десять").is_ok());
    }
}
