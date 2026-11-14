//! Unlimited, compressed clip history.
//!
//! Layout on disk (in the data dir):
//! - `blocks-<gen>.dat`: sealed blocks, each ~64 KiB of clip text compressed with deflate.
//! - `index.bin`: `CKS1` + deflate(metadata of every clip, block table with a 1 KiB search
//!   filter per block, group names, and the open block that is not sealed yet).
//!
//! With a key (see `crate::crypto`) the index becomes `CKS2` + encrypted(...) and every block is
//! encrypted after compression; a plain history from an older version is converted on open.
//!
//! In RAM the daemon keeps only metadata (32 bytes per clip), the filters, the open block and a
//! small cache of decompressed blocks. Search walks clips newest first, skips whole blocks whose
//! filter cannot contain the query, and stops once it has `limit` hits — so it stays fast no
//! matter how long the history is.
//!
//! Files: `blocks.rs` (block file, cache, filters), `index.rs` (index.bin), `edit.rs` (writes),
//! `query.rs` (search and reads), `compact.rs`, `text.rs` (clip text helpers), `media.rs` (images).
pub mod media;
pub mod text;

mod blocks;
mod compact;
mod edit;
mod index;
mod query;
#[cfg(test)]
mod tests;

use self::blocks::{add_to_filter, filter_has, new_filter};
use self::index::Format;

use self::text::{Kind, FILTER_BITS};
use crate::crypto::Cipher;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Raw text per block before it gets sealed and compressed.
pub const BLOCK_RAW: usize = 64 * 1024;
/// Clips bigger than this are not stored.
pub const MAX_ENTRY_BYTES: usize = 1 << 20;
/// Decompressed blocks kept in RAM (64 × 64 KiB ≈ 4 MiB).
const CACHE_BLOCKS: usize = 64;
const FILTER_WORDS: usize = (FILTER_BITS / 64) as usize;
const OPEN: u32 = u32::MAX;
const MAGIC: &[u8; 4] = b"CKS1";
/// Same layout, but the whole index and every block are encrypted.
const MAGIC_ENC: &[u8; 4] = b"CKS2";
const FORMAT: u64 = 1;
const BLOCK_LEVEL: u8 = 9;
const INDEX_LEVEL: u8 = 1;
const MAX_INDEX_RAW: usize = 1 << 31;

type Filter64 = Box<[u64; FILTER_WORDS]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub id: u32,
    block: u32,
    offset: u32,
    pub len: u32,
    /// Unix seconds of the last copy.
    pub used: u32,
    hash: u64,
    pub group: u16,
    pub kind: u8,
    pub pinned: bool,
}

struct Block {
    file_off: u64,
    comp_len: u32,
    raw_len: u32,
    /// Bytes of clips in this block that are not deleted.
    live: u32,
    filter: Filter64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: u16,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    All,
    Pinned,
    Links,
    Images,
    Group(u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub id: u32,
    pub pinned: bool,
    pub group: u16,
    pub kind: u8,
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub text: String,
    pub used: u32,
    pub pinned: bool,
    /// Empty when the clip has no group.
    pub group: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub entries: usize,
    pub pinned: usize,
    pub groups: usize,
    pub blocks: usize,
    /// Text size of all stored clips, uncompressed.
    pub text_bytes: u64,
    /// Files on disk (blocks + index).
    pub disk_bytes: u64,
}

pub struct Store {
    dir: PathBuf,
    /// Least recently used first, so adding a clip is a cheap append.
    entries: Vec<Meta>,
    /// Text hash → clip id, to find duplicates without scanning the history.
    by_hash: HashMap<u64, u32>,
    /// Clips that live in the open block (they need a block number when it is sealed).
    open_count: usize,
    blocks: Vec<Block>,
    groups: Vec<Group>,
    open: String,
    open_live: u64,
    open_filter: Filter64,
    next_id: u32,
    gen: u32,
    file: Option<File>,
    file_len: u64,
    /// Decompressed blocks, least recently used first.
    cache: Vec<(u32, String)>,
    max_entries: usize,
    dirty: bool,
    unsynced: bool,
    fold_buf: String,
    cipher: Option<Cipher>,
    /// Whether the blocks file on disk is encrypted (differs from `cipher` only while migrating).
    blocks_encrypted: bool,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg.to_owned())
}

impl Store {
    /// Opens (or creates) the store in `dir`. `max_entries == 0` means unlimited.
    pub fn open(dir: &Path, max_entries: usize) -> io::Result<Self> {
        Self::open_with(dir, max_entries, None)
    }

    /// With a cipher, everything on disk is encrypted. A plain history from an older version is
    /// converted on the first open. Without one, an encrypted history is refused.
    pub fn open_with(dir: &Path, max_entries: usize, cipher: Option<Cipher>) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut s = Store {
            dir: dir.to_owned(),
            entries: Vec::new(),
            by_hash: HashMap::new(),
            open_count: 0,
            blocks: Vec::new(),
            groups: Vec::new(),
            open: String::new(),
            open_live: 0,
            open_filter: new_filter(),
            next_id: 1,
            gen: 0,
            file: None,
            file_len: 0,
            cache: Vec::new(),
            max_entries,
            dirty: false,
            unsynced: false,
            fold_buf: String::new(),
            blocks_encrypted: cipher.is_some(),
            cipher,
        };
        let mut migrate = false;
        match std::fs::read(s.index_path()) {
            Ok(bytes) => {
                migrate = s.decode(&bytes)? == Format::Plain && s.cipher.is_some();
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        s.open_file()?;
        if migrate {
            s.compact()?; // rewrites every block (and the index) encrypted
        }
        s.remove_stale_files();
        Ok(s)
    }

    /// True when the history on disk is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Does `dir` hold an encrypted history? (Tells the caller whether a missing key is fatal.)
    pub fn dir_is_encrypted(dir: &Path) -> bool {
        let mut magic = [0u8; 4];
        File::open(dir.join("index.bin"))
            .and_then(|mut f| f.read_exact(&mut magic))
            .is_ok()
            && &magic == MAGIC_ENC
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Id of the most recently used clip.
    pub fn newest_id(&self) -> Option<u32> {
        self.entries.last().map(|m| m.id)
    }

    pub fn get(&self, id: u32) -> Option<Meta> {
        self.entries.iter().copied().find(|m| m.id == id)
    }

    pub fn text(&mut self, id: u32) -> io::Result<Option<String>> {
        match self.get(id) {
            Some(m) => Ok(Some(self.entry_text(m)?.to_owned())),
            None => Ok(None),
        }
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn group_size(&self, id: u16) -> usize {
        self.entries.iter().filter(|m| m.group == id).count()
    }
}
