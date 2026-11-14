//! Unlimited, compressed clip history.
//!
//! Layout on disk (in the data dir):
//! - `blocks-<gen>.dat`: sealed blocks, each ~64 KiB of clip text compressed with deflate.
//! - `index.bin`: `CKS1` + deflate(metadata of every clip, block table with a 1 KiB search
//!   filter per block, group names, and the open block that is not sealed yet).
//!
//! With a key (see `crypt.rs`) the index becomes `CKS2` + encrypted(...) and every block is
//! encrypted after compression; a plain history from an older version is converted on open.
//!
//! In RAM the daemon keeps only metadata (32 bytes per clip), the filters, the open block and a
//! small cache of decompressed blocks. Search walks clips newest first, skips whole blocks whose
//! filter cannot contain the query, and stops once it has `limit` hits — so it stays fast no
//! matter how long the history is.
pub mod media;
pub mod text;

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

fn new_filter() -> Filter64 {
    Box::new([0; FILTER_WORDS])
}

fn add_to_filter(filter: &mut [u64; FILTER_WORDS], folded: &str) {
    text::for_each_feature(folded, |b| filter[(b / 64) as usize] |= 1 << (b % 64));
}

fn filter_has(filter: &[u64; FILTER_WORDS], bits: &[u32]) -> bool {
    bits.iter()
        .all(|&b| filter[(b / 64) as usize] & (1 << (b % 64)) != 0)
}

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

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.bin")
    }

    fn blocks_path(&self, gen: u32) -> PathBuf {
        self.dir.join(format!("blocks-{gen}.dat"))
    }

    fn open_file(&mut self) -> io::Result<()> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.blocks_path(self.gen))?;
        let len = f.metadata()?.len();
        if len < self.file_len {
            return Err(bad(
                "blocks file is shorter than index.bin says (damaged data dir)",
            ));
        }
        if len > self.file_len {
            f.set_len(self.file_len)?; // a block written right before a crash, never indexed
        }
        self.file = Some(f);
        Ok(())
    }

    fn remove_stale_files(&self) {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let keep = format!("blocks-{}.dat", self.gen);
        for e in dir.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let stale_blocks =
                name.starts_with("blocks-") && name.ends_with(".dat") && name != keep;
            if stale_blocks || name == "index.tmp" {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("store file is opened in Store::open")
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

    // ---------- reading ----------

    fn read_block(&mut self, b: u32) -> io::Result<String> {
        let (off, comp_len, raw_len) = {
            let blk = self
                .blocks
                .get(b as usize)
                .ok_or_else(|| bad("no such block"))?;
            (blk.file_off, blk.comp_len as usize, blk.raw_len as usize)
        };
        let mut comp = vec![0; comp_len];
        let f = self.file();
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(&mut comp)?;
        if self.blocks_encrypted {
            let cipher = self
                .cipher
                .as_ref()
                .ok_or_else(|| bad("history is encrypted: no key"))?;
            comp = cipher
                .open(&comp, &off.to_le_bytes())
                .ok_or_else(|| bad("block does not decrypt (wrong key?)"))?;
        }
        let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(&comp, raw_len)
            .map_err(|_| bad("corrupt block"))?;
        String::from_utf8(raw).map_err(|_| bad("block is not UTF-8"))
    }

    fn block_text(&mut self, b: u32) -> io::Result<&str> {
        if b == OPEN {
            return Ok(&self.open);
        }
        match self.cache.iter().position(|(id, _)| *id == b) {
            Some(pos) if pos + 1 != self.cache.len() => {
                let item = self.cache.remove(pos);
                self.cache.push(item);
            }
            Some(_) => {}
            None => {
                let text = self.read_block(b)?;
                if self.cache.len() >= CACHE_BLOCKS {
                    self.cache.remove(0);
                }
                self.cache.push((b, text));
            }
        }
        Ok(&self.cache.last().expect("just pushed").1)
    }

    fn entry_text(&mut self, m: Meta) -> io::Result<&str> {
        let (start, end) = (m.offset as usize, m.offset as usize + m.len as usize);
        self.block_text(m.block)?
            .get(start..end)
            .ok_or_else(|| bad("clip out of block bounds"))
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

    /// Pinned clips first, then the rest; both most recently used first.
    /// Every word of `query` must appear in the clip (case-insensitive).
    pub fn search(
        &mut self,
        query: &str,
        view: View,
        limit: usize,
        preview_chars: usize,
    ) -> Vec<Hit> {
        let mut words = Vec::new();
        let mut bits = Vec::new();
        for w in query.split_whitespace() {
            let mut f = String::new();
            text::fold(w, &mut f);
            text::for_each_feature(&f, |b| bits.push(b));
            words.push(f);
        }
        let mut hits = Vec::new();
        let mut buf = std::mem::take(&mut self.fold_buf);
        'passes: for pinned_pass in [true, false] {
            for i in (0..self.entries.len()).rev() {
                let m = self.entries[i];
                if m.pinned != pinned_pass {
                    continue;
                }
                let in_view = match view {
                    View::All => true,
                    View::Pinned => m.pinned,
                    View::Links => m.kind == Kind::Link as u8,
                    View::Images => m.kind == Kind::Image as u8,
                    View::Group(g) => m.group == g,
                };
                if !in_view {
                    continue;
                }
                if !words.is_empty() {
                    let filter = match m.block {
                        OPEN => &self.open_filter,
                        b => &self.blocks[b as usize].filter,
                    };
                    if !filter_has(filter, &bits) {
                        continue;
                    }
                }
                let Ok(t) = self.entry_text(m) else { continue };
                if !words.is_empty() {
                    text::fold(t, &mut buf);
                    if !words.iter().all(|w| buf.contains(w.as_str())) {
                        continue;
                    }
                }
                hits.push(Hit {
                    id: m.id,
                    pinned: m.pinned,
                    group: m.group,
                    kind: m.kind,
                    preview: text::clip_preview(t, preview_chars),
                });
                if hits.len() >= limit {
                    break 'passes;
                }
            }
        }
        self.fold_buf = buf;
        hits
    }

    /// Id of the clip whose text hashes to `hash` (`text::fnv1a`). Used to match clips across devices.
    pub fn id_for_hash(&self, hash: u64) -> Option<u32> {
        let id = *self.by_hash.get(&hash)?;
        self.entries.iter().any(|m| m.id == id).then_some(id)
    }

    pub fn hash_of(&self, id: u32) -> Option<u64> {
        self.entries.iter().find(|m| m.id == id).map(|m| m.hash)
    }

    pub fn set_pinned(&mut self, id: u32, pinned: bool) -> bool {
        match self.entries.iter_mut().find(|m| m.id == id) {
            Some(m) => {
                if m.pinned != pinned {
                    m.pinned = pinned;
                    self.dirty = true;
                }
                true
            }
            None => false,
        }
    }

    /// Every clip, oldest first, with what sync needs to recreate it elsewhere.
    pub fn export(&mut self) -> Vec<Export> {
        let mut out = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            let m = self.entries[i];
            let group = self
                .groups
                .iter()
                .find(|g| g.id == m.group)
                .map(|g| g.name.clone())
                .unwrap_or_default();
            if let Ok(t) = self.entry_text(m) {
                out.push(Export {
                    text: t.to_owned(),
                    used: m.used,
                    pinned: m.pinned,
                    group,
                });
            }
        }
        out
    }

    /// Image clips, oldest first: (clip id, image hash, pinned).
    pub fn image_refs(&mut self) -> Vec<(u32, u64, bool)> {
        let mut out = Vec::new();
        for i in 0..self.entries.len() {
            let m = self.entries[i];
            if m.kind == Kind::Image as u8 {
                if let Ok(t) = self.entry_text(m) {
                    if let Some(r) = text::parse_image_marker(t) {
                        out.push((m.id, r.hash, m.pinned));
                    }
                }
            }
        }
        out
    }

    pub fn stats(&self) -> Stats {
        let index = std::fs::metadata(self.index_path())
            .map(|m| m.len())
            .unwrap_or(0);
        Stats {
            entries: self.entries.len(),
            pinned: self.entries.iter().filter(|m| m.pinned).count(),
            groups: self.groups.len(),
            blocks: self.blocks.len(),
            text_bytes: self.entries.iter().map(|m| m.len as u64).sum(),
            disk_bytes: self.file_len + index,
        }
    }

    // ---------- writing ----------

    /// Adds a clip on top. A clip that already exists moves to the top (keeping pin and group).
    /// Returns false when nothing changed.
    pub fn push(&mut self, clip: &str, now: u32) -> io::Result<bool> {
        if clip.trim().is_empty() || clip.len() > MAX_ENTRY_BYTES {
            return Ok(false);
        }
        let hash = text::fnv1a(clip.as_bytes());
        if let Some(&id) = self.by_hash.get(&hash) {
            // Usually a recent clip, so search from the newest end.
            if let Some(i) = self.entries.iter().rposition(|m| m.id == id) {
                let m = self.entries[i];
                if matches!(self.entry_text(m), Ok(t) if t == clip) {
                    if i + 1 == self.entries.len() {
                        return Ok(false);
                    }
                    let mut m = self.entries.remove(i);
                    m.used = now;
                    self.entries.push(m);
                    self.dirty = true;
                    return Ok(true);
                }
            }
        }

        let offset = self.open.len() as u32;
        self.open.push_str(clip);
        self.open_live += clip.len() as u64;
        let mut buf = std::mem::take(&mut self.fold_buf);
        text::fold(clip, &mut buf);
        add_to_filter(&mut self.open_filter, &buf);
        self.fold_buf = buf;
        self.entries.push(Meta {
            id: self.next_id,
            block: OPEN,
            offset,
            len: clip.len() as u32,
            used: now,
            hash,
            group: 0,
            kind: text::detect_kind(clip) as u8,
            pinned: false,
        });
        self.by_hash.entry(hash).or_insert(self.next_id);
        self.open_count += 1;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.dirty = true;
        if self.open.len() >= BLOCK_RAW {
            self.seal()?;
        }
        self.enforce_limit();
        self.maybe_compact()?;
        Ok(true)
    }

    /// Deflate, then encrypt (bound to the position in the file) if there is a key.
    fn pack(&self, raw: &[u8], file_off: u64) -> Vec<u8> {
        let comp = miniz_oxide::deflate::compress_to_vec(raw, BLOCK_LEVEL);
        match &self.cipher {
            Some(c) => c.seal(&comp, &file_off.to_le_bytes()),
            None => comp,
        }
    }

    /// Compresses the open block and appends it to the blocks file.
    fn seal(&mut self) -> io::Result<()> {
        if self.open.is_empty() {
            return Ok(());
        }
        let off = self.file_len;
        let comp = self.pack(self.open.as_bytes(), off);
        let f = self.file();
        f.seek(SeekFrom::Start(off))?;
        f.write_all(&comp)?;
        self.file_len += comp.len() as u64;
        self.unsynced = true;
        let idx = self.blocks.len() as u32;
        let filter = std::mem::replace(&mut self.open_filter, new_filter());
        self.blocks.push(Block {
            file_off: off,
            comp_len: comp.len() as u32,
            raw_len: self.open.len() as u32,
            live: self.open_live as u32,
            filter,
        });
        let mut left = self.open_count;
        for m in self.entries.iter_mut().rev() {
            if left == 0 {
                break;
            }
            if m.block == OPEN {
                m.block = idx;
                left -= 1;
            }
        }
        self.open_count = 0;
        let text = std::mem::take(&mut self.open);
        self.open_live = 0;
        if self.cache.len() >= CACHE_BLOCKS {
            self.cache.remove(0);
        }
        self.cache.push((idx, text));
        self.dirty = true;
        Ok(())
    }

    fn remove_at(&mut self, i: usize) -> Meta {
        let m = self.entries.remove(i);
        if self.by_hash.get(&m.hash) == Some(&m.id) {
            self.by_hash.remove(&m.hash);
        }
        if m.block == OPEN {
            self.open_live -= m.len as u64;
            self.open_count -= 1;
        } else {
            self.blocks[m.block as usize].live -= m.len;
        }
        self.dirty = true;
        m
    }

    fn enforce_limit(&mut self) {
        if self.max_entries == 0 {
            return;
        }
        let mut unpinned = self.entries.iter().filter(|m| !m.pinned).count();
        let mut i = 0;
        while unpinned > self.max_entries && i < self.entries.len() {
            if self.entries[i].pinned {
                i += 1;
            } else {
                self.remove_at(i); // oldest first
                unpinned -= 1;
            }
        }
    }

    pub fn toggle_pin(&mut self, id: u32) -> bool {
        match self.entries.iter_mut().find(|m| m.id == id) {
            Some(m) => {
                m.pinned = !m.pinned;
                self.dirty = true;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, id: u32) -> io::Result<bool> {
        let Some(i) = self.entries.iter().position(|m| m.id == id) else {
            return Ok(false);
        };
        self.remove_at(i);
        self.maybe_compact()?;
        Ok(true)
    }

    /// Removes every unpinned clip.
    pub fn clear(&mut self) -> io::Result<()> {
        let mut i = self.entries.len();
        while i > 0 {
            i -= 1;
            if !self.entries[i].pinned {
                self.remove_at(i);
            }
        }
        self.maybe_compact()
    }

    /// Puts a clip into the group called `name` (created on demand, matched case-insensitively).
    /// An empty name takes the clip out of its group. Empty groups disappear on save.
    pub fn set_group(&mut self, id: u32, name: &str) -> Result<u16, &'static str> {
        let name: String = name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let name: String = name.trim().chars().take(40).collect();
        let pos = self
            .entries
            .iter()
            .position(|m| m.id == id)
            .ok_or("no such clip")?;
        let gid = if name.is_empty() {
            0
        } else {
            let (mut a, mut b) = (String::new(), String::new());
            text::fold(&name, &mut a);
            let found = self.groups.iter().find(|g| {
                text::fold(&g.name, &mut b);
                a == b
            });
            match found {
                Some(g) => g.id,
                None => {
                    let id = (1..=u16::MAX)
                        .find(|i| !self.groups.iter().any(|g| g.id == *i))
                        .ok_or("too many groups")?;
                    self.groups.push(Group { id, name });
                    id
                }
            }
        };
        self.entries[pos].group = gid;
        self.dirty = true;
        Ok(gid)
    }

    fn garbage(&self) -> (u64, u64) {
        let total: u64 =
            self.blocks.iter().map(|b| b.raw_len as u64).sum::<u64>() + self.open.len() as u64;
        let live: u64 = self.blocks.iter().map(|b| b.live as u64).sum::<u64>() + self.open_live;
        (total - live, total)
    }

    fn maybe_compact(&mut self) -> io::Result<()> {
        let (garbage, total) = self.garbage();
        if garbage > (1 << 20) && garbage * 2 > total {
            self.compact()?;
        }
        Ok(())
    }

    /// Rewrites all live clips into a fresh blocks file, dropping deleted text.
    pub fn compact(&mut self) -> io::Result<()> {
        let new_gen = self.gen.wrapping_add(1);
        let path = self.blocks_path(new_gen);
        let mut out = File::create(&path)?;
        let mut blocks = Vec::new();
        let mut file_len = 0u64;
        let mut open = String::new();
        let mut filter = new_filter();
        let mut chunk: Vec<usize> = Vec::new();
        let mut kept: Vec<Meta> = Vec::with_capacity(self.entries.len());
        let mut buf = String::new();

        // Oldest first, so recent clips end up together in the last blocks and the open block.
        for i in 0..self.entries.len() {
            let mut m = self.entries[i];
            let Ok(t) = self.entry_text(m) else { continue };
            let t = t.to_owned();
            m.block = OPEN;
            m.offset = open.len() as u32;
            open.push_str(&t);
            text::fold(&t, &mut buf);
            add_to_filter(&mut filter, &buf);
            chunk.push(kept.len());
            kept.push(m);
            if open.len() >= BLOCK_RAW {
                let comp = self.pack(open.as_bytes(), file_len);
                out.write_all(&comp)?;
                let idx = blocks.len() as u32;
                for &k in &chunk {
                    kept[k].block = idx;
                }
                blocks.push(Block {
                    file_off: file_len,
                    comp_len: comp.len() as u32,
                    raw_len: open.len() as u32,
                    live: open.len() as u32,
                    filter: std::mem::replace(&mut filter, new_filter()),
                });
                file_len += comp.len() as u64;
                chunk.clear();
                open.clear();
            }
        }
        out.sync_all()?;
        drop(out);

        let old = self.blocks_path(self.gen);
        self.entries = kept;
        self.blocks = blocks;
        self.open_live = open.len() as u64;
        self.open_count = chunk.len();
        self.open = open;
        self.open_filter = filter;
        self.gen = new_gen;
        self.file_len = file_len;
        self.cache.clear();
        self.blocks_encrypted = self.cipher.is_some();
        self.file = Some(OpenOptions::new().read(true).write(true).open(&path)?);
        self.unsynced = false;
        self.dirty = true;
        self.save()?; // the index now points at the new file; only then drop the old one
        let _ = std::fs::remove_file(old);
        Ok(())
    }

    /// Writes `index.bin` atomically (temp file + rename) if anything changed.
    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if self.unsynced {
            self.file().sync_data()?;
            self.unsynced = false;
        }
        let used: Vec<u16> = self.entries.iter().map(|m| m.group).collect();
        self.groups.retain(|g| used.contains(&g.id));
        let comp = miniz_oxide::deflate::compress_to_vec(&self.encode(), INDEX_LEVEL);
        let tmp = self.dir.join("index.tmp");
        let mut f = File::create(&tmp)?;
        match &self.cipher {
            Some(c) => {
                f.write_all(MAGIC_ENC)?;
                f.write_all(&c.seal(&comp, MAGIC_ENC))?;
            }
            None => {
                f.write_all(MAGIC)?;
                f.write_all(&comp)?;
            }
        }
        f.sync_data()?;
        drop(f);
        std::fs::rename(&tmp, self.index_path())?;
        self.dirty = false;
        Ok(())
    }

    // ---------- index encoding ----------

    fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(64 + self.entries.len() * 24));
        w.u(FORMAT);
        w.u(self.gen as u64);
        w.u(self.next_id as u64);
        w.u(self.file_len);
        w.u(self.blocks.len() as u64);
        for b in &self.blocks {
            w.u(b.file_off);
            w.u(b.comp_len as u64);
            w.u(b.raw_len as u64);
            w.u(b.live as u64);
            for word in b.filter.iter() {
                w.0.extend_from_slice(&word.to_le_bytes());
            }
        }
        w.u(self.groups.len() as u64);
        for g in &self.groups {
            w.u(g.id as u64);
            w.bytes(g.name.as_bytes());
        }
        w.bytes(self.open.as_bytes());
        w.u(self.open_live);
        w.u(self.entries.len() as u64);
        for m in &self.entries {
            w.u(m.id as u64);
            w.u(if m.block == OPEN {
                0
            } else {
                m.block as u64 + 1
            });
            w.u(m.offset as u64);
            w.u(m.len as u64);
            w.u(m.used as u64);
            w.0.extend_from_slice(&m.hash.to_le_bytes());
            w.u(m.group as u64);
            w.u(m.kind as u64 | if m.pinned { 0x80 } else { 0 });
        }
        w.0
    }

    fn decode(&mut self, file: &[u8]) -> io::Result<Format> {
        let (format, body) = if let Some(body) = file.strip_prefix(MAGIC_ENC) {
            let cipher = self
                .cipher
                .as_ref()
                .ok_or_else(|| bad("the history is encrypted, but there is no key"))?;
            let plain = cipher
                .open(body, MAGIC_ENC)
                .ok_or_else(|| bad("index.bin does not decrypt (wrong key?)"))?;
            self.blocks_encrypted = true;
            (Format::Encrypted, std::borrow::Cow::Owned(plain))
        } else if let Some(body) = file.strip_prefix(MAGIC) {
            self.blocks_encrypted = false;
            (Format::Plain, std::borrow::Cow::Borrowed(body))
        } else {
            return Err(bad("index.bin: wrong magic"));
        };
        let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(&body, MAX_INDEX_RAW)
            .map_err(|_| bad("index.bin: corrupt"))?;
        let mut r = Reader { b: &raw, p: 0 };
        if r.u()? != FORMAT {
            return Err(bad("index.bin: unknown format version"));
        }
        self.gen = r.u32()?;
        self.next_id = r.u32()?.max(1);
        self.file_len = r.u()?;
        let n = r.len()?;
        for _ in 0..n {
            let file_off = r.u()?;
            let comp_len = r.u32()?;
            let raw_len = r.u32()?;
            let live = r.u32()?;
            let mut filter = new_filter();
            for word in filter.iter_mut() {
                *word = u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"));
            }
            if file_off + comp_len as u64 > self.file_len || live > raw_len {
                return Err(bad("index.bin: block out of range"));
            }
            self.blocks.push(Block {
                file_off,
                comp_len,
                raw_len,
                live,
                filter,
            });
        }
        let n = r.len()?;
        for _ in 0..n {
            let id = u16::try_from(r.u()?).map_err(|_| bad("index.bin: group id"))?;
            let name =
                String::from_utf8(r.bytes()?.to_vec()).map_err(|_| bad("index.bin: group name"))?;
            self.groups.push(Group { id, name });
        }
        self.open =
            String::from_utf8(r.bytes()?.to_vec()).map_err(|_| bad("index.bin: open block"))?;
        self.open_live = r.u()?;
        let mut buf = String::new();
        text::fold(&self.open, &mut buf);
        add_to_filter(&mut self.open_filter, &buf);
        let n = r.len()?;
        self.entries.reserve(n);
        for _ in 0..n {
            let id = r.u32()?;
            let block = match r.u()? {
                0 => OPEN,
                b => u32::try_from(b - 1).map_err(|_| bad("index.bin: block ref"))?,
            };
            let offset = r.u32()?;
            let len = r.u32()?;
            let used = r.u32()?;
            let hash = u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"));
            let group = u16::try_from(r.u()?).map_err(|_| bad("index.bin: group"))?;
            let flags = r.u()?;
            let limit = match block {
                OPEN => self.open.len() as u64,
                b => {
                    self.blocks
                        .get(b as usize)
                        .ok_or_else(|| bad("index.bin: block ref"))?
                        .raw_len as u64
                }
            };
            if offset as u64 + len as u64 > limit {
                return Err(bad("index.bin: clip out of range"));
            }
            if block == OPEN {
                self.open_count += 1;
            }
            self.by_hash.entry(hash).or_insert(id);
            self.entries.push(Meta {
                id,
                block,
                offset,
                len,
                used,
                hash,
                group,
                kind: (flags & 0x7f) as u8,
                pinned: flags & 0x80 != 0,
            });
        }
        Ok(format)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Plain,
    Encrypted,
}

struct Writer(Vec<u8>);

impl Writer {
    fn u(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.u(b.len() as u64);
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self
                .b
                .get(self.p)
                .ok_or_else(|| bad("index.bin: truncated"))?;
            self.p += 1;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad("index.bin: bad varint"))
    }

    fn u32(&mut self) -> io::Result<u32> {
        u32::try_from(self.u()?).map_err(|_| bad("index.bin: number too big"))
    }

    fn len(&mut self) -> io::Result<usize> {
        let n = self.u()? as usize;
        if n > self.b.len() - self.p {
            return Err(bad("index.bin: bad length"));
        }
        Ok(n)
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self
            .b
            .get(self.p..self.p + n)
            .ok_or_else(|| bad("index.bin: truncated"))?;
        self.p += n;
        Ok(s)
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("clipkeep-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn previews(s: &mut Store, q: &str, view: View) -> Vec<String> {
        s.search(q, view, 100, 200)
            .into_iter()
            .map(|h| h.preview)
            .collect()
    }

    #[test]
    fn push_dedupes_and_moves_to_top() {
        let d = TempDir::new("dedupe");
        let mut s = Store::open(&d.0, 0).unwrap();
        assert!(s.push("a", 1).unwrap());
        assert!(s.push("b", 2).unwrap());
        assert!(!s.push("b", 3).unwrap());
        assert!(s.push("a", 4).unwrap());
        assert!(!s.push("   ", 5).unwrap());
        assert!(!s.push(&"x".repeat(MAX_ENTRY_BYTES + 1), 5).unwrap());
        assert_eq!(previews(&mut s, "", View::All), ["a", "b"]);
    }

    #[test]
    fn search_cyrillic_pinned_first_and_views() {
        let d = TempDir::new("search");
        let mut s = Store::open(&d.0, 0).unwrap();
        s.push("Привет мир", 1).unwrap();
        s.push("https://example.com/Привет", 2).unwrap();
        s.push("ПРИВЕТ, Ташкент", 3).unwrap();
        s.push("hello world", 4).unwrap();
        let first = s.search("мир", View::All, 10, 80)[0].id;
        s.toggle_pin(first);
        assert_eq!(
            previews(&mut s, "привет", View::All),
            [
                "Привет мир",
                "ПРИВЕТ, Ташкент",
                "https://example.com/Привет"
            ]
        );
        assert_eq!(
            previews(&mut s, "ташкент привет", View::All),
            ["ПРИВЕТ, Ташкент"]
        );
        assert_eq!(
            previews(&mut s, "", View::Links),
            ["https://example.com/Привет"]
        );
        assert_eq!(previews(&mut s, "", View::Pinned), ["Привет мир"]);
        assert!(previews(&mut s, "нет такого", View::All).is_empty());
        assert_eq!(s.search("", View::All, 2, 80).len(), 2);
    }

    #[test]
    fn groups_create_match_and_vanish() {
        let d = TempDir::new("groups");
        let mut s = Store::open(&d.0, 0).unwrap();
        s.push("один", 1).unwrap();
        s.push("два", 2).unwrap();
        let ids: Vec<u32> = s
            .search("", View::All, 10, 80)
            .iter()
            .map(|h| h.id)
            .collect();
        let g = s.set_group(ids[0], "  Работа\n").unwrap();
        assert_eq!(s.set_group(ids[1], "РАБОТА").unwrap(), g);
        assert_eq!(
            s.groups(),
            [Group {
                id: g,
                name: "Работа".into()
            }]
        );
        assert_eq!(previews(&mut s, "", View::Group(g)), ["два", "один"]);
        s.set_group(ids[0], "").unwrap();
        s.set_group(ids[1], "").unwrap();
        s.save().unwrap();
        assert!(s.groups().is_empty());
        assert!(s.set_group(999, "x").is_err());
    }

    #[test]
    fn survives_reopen_with_sealed_blocks() {
        let d = TempDir::new("reopen");
        {
            let mut s = Store::open(&d.0, 0).unwrap();
            for i in 0..3000 {
                s.push(
                    &format!("clip-{i:05} строка с текстом, чтобы быстро заполнить блок"),
                    i,
                )
                .unwrap();
            }
            let id = s.search("clip-00042", View::All, 1, 80)[0].id;
            s.toggle_pin(id);
            s.set_group(id, "Важное").unwrap();
            assert!(s.blocks.len() > 2, "test must cover sealed blocks");
            s.save().unwrap();
        }
        let mut s = Store::open(&d.0, 0).unwrap();
        assert_eq!(s.len(), 3000);
        let hits = s.search("CLIP-00042", View::All, 10, 200);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].pinned);
        assert_eq!(s.groups()[0].name, "Важное");
        assert!(s
            .text(hits[0].id)
            .unwrap()
            .unwrap()
            .starts_with("clip-00042 строка"));
        let top = s.search("", View::All, 2, 200);
        assert!(top[0].preview.starts_with("clip-00042"));
        assert!(top[1].preview.starts_with("clip-02999"));
        // dedupe index is rebuilt on open: an old clip moves to the top instead of a copy
        assert!(s
            .push(
                "clip-00007 строка с текстом, чтобы быстро заполнить блок",
                9999
            )
            .unwrap());
        assert_eq!(s.len(), 3000);
        assert!(s.search("", View::All, 2, 80)[1]
            .preview
            .starts_with("clip-00007"));
        let st = s.stats();
        assert!(
            st.disk_bytes * 3 < st.text_bytes,
            "expected >3x compression, got {st:?}"
        );
    }

    #[test]
    fn crash_tail_is_truncated() {
        let d = TempDir::new("crash");
        {
            let mut s = Store::open(&d.0, 0).unwrap();
            s.push("сохранено", 1).unwrap();
            s.save().unwrap();
        }
        let blocks = d.0.join("blocks-0.dat");
        std::fs::write(&blocks, b"garbage from a crash").unwrap();
        let mut s = Store::open(&d.0, 0).unwrap();
        assert_eq!(std::fs::metadata(&blocks).unwrap().len(), 0);
        assert_eq!(previews(&mut s, "", View::All), ["сохранено"]);
    }

    #[test]
    fn delete_clear_and_compaction() {
        let d = TempDir::new("compact");
        let mut s = Store::open(&d.0, 0).unwrap();
        let big = "ж".repeat(40_000); // 80 KB each in UTF-8
        for i in 0..40 {
            s.push(&format!("{big}{i}"), i).unwrap();
        }
        s.push("keep me", 100).unwrap();
        let keep = s.search("keep", View::All, 1, 80)[0].id;
        s.toggle_pin(keep);
        let gen = s.gen;
        s.clear().unwrap();
        assert!(s.gen != gen, "clear of ~3 MB must compact");
        assert_eq!(previews(&mut s, "", View::All), ["keep me"]);
        assert!(!d.0.join(format!("blocks-{gen}.dat")).exists());
        drop(s);
        let mut s = Store::open(&d.0, 0).unwrap();
        assert_eq!(previews(&mut s, "", View::All), ["keep me"]);
        assert!(s.remove(keep).unwrap());
        assert!(!s.remove(keep).unwrap());
        assert!(s.is_empty());
    }

    #[test]
    fn max_entries_keeps_pinned() {
        let d = TempDir::new("limit");
        let mut s = Store::open(&d.0, 2).unwrap();
        s.push("old", 1).unwrap();
        let old = s.search("", View::All, 1, 80)[0].id;
        s.toggle_pin(old);
        for t in ["1", "2", "3"] {
            s.push(t, 2).unwrap();
        }
        assert_eq!(previews(&mut s, "", View::All), ["old", "3", "2"]);
    }

    #[test]
    fn filter_never_hides_a_real_match() {
        let d = TempDir::new("filter");
        let mut s = Store::open(&d.0, 0).unwrap();
        for i in 0..2000 {
            s.push(&format!("запись {i} Ёлка-{} ÆØÅ", i * 7), i)
                .unwrap();
        }
        for q in ["ёлка-700", "æøå", "запись 1999", "ЗАПИСЬ 5 "] {
            let hits = s.search(q, View::All, 5000, 80);
            let mut f = String::new();
            text::fold(q, &mut f);
            let words: Vec<&str> = f.split_whitespace().collect();
            let expected = s
                .entries
                .clone()
                .into_iter()
                .filter(|m| {
                    let mut t = String::new();
                    text::fold(s.entry_text(*m).unwrap(), &mut t);
                    words.iter().all(|w| t.contains(w))
                })
                .count();
            assert_eq!(hits.len(), expected, "query {q}");
            assert!(expected > 0, "query {q}");
        }
    }

    fn key(n: u8) -> Cipher {
        Cipher::new(&[n; 32])
    }

    fn all_files_text(dir: &Path) -> Vec<u8> {
        let mut all = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            all.extend(std::fs::read(e.path()).unwrap());
        }
        all
    }

    #[test]
    fn encrypted_store_roundtrip_and_no_plaintext_on_disk() {
        let t = TempDir::new("enc");
        {
            let mut s = Store::open_with(&t.0, 0, Some(key(1))).unwrap();
            for i in 0..4000 {
                s.push(
                    &format!("секретный-пароль-{i:05} lorem ipsum dolor sit amet"),
                    i,
                )
                .unwrap();
            }
            s.toggle_pin(5);
            s.save().unwrap();
            assert!(s.is_encrypted());
        }
        let disk = all_files_text(&t.0);
        for needle in ["секретный", "lorem ipsum", "пароль"] {
            assert!(
                !disk.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{needle} leaked"
            );
        }
        assert!(Store::dir_is_encrypted(&t.0));
        assert!(
            Store::open(&t.0, 0).is_err(),
            "no key must not open an encrypted history"
        );
        assert!(
            Store::open_with(&t.0, 0, Some(key(2))).is_err(),
            "wrong key must fail"
        );
        let mut s = Store::open_with(&t.0, 0, Some(key(1))).unwrap();
        assert_eq!(s.len(), 4000);
        assert_eq!(
            previews(&mut s, "пароль-03999", View::All),
            vec!["секретный-пароль-03999 lorem ipsum dolor sit amet"]
        );
        assert!(
            !s.push("секретный-пароль-03999 lorem ipsum dolor sit amet", 9)
                .unwrap()
                || s.len() == 4000
        );
    }

    #[test]
    fn plain_history_is_converted_to_encrypted() {
        let t = TempDir::new("migrate");
        {
            let mut s = Store::open(&t.0, 0).unwrap();
            for i in 0..3000 {
                s.push(&format!("старая запись {i:05} some text to fill blocks"), i)
                    .unwrap();
            }
            s.save().unwrap();
        }
        assert!(!Store::dir_is_encrypted(&t.0));
        let mut s = Store::open_with(&t.0, 0, Some(key(3))).unwrap();
        assert!(Store::dir_is_encrypted(&t.0));
        assert_eq!(s.len(), 3000);
        assert_eq!(previews(&mut s, "02999", View::All).len(), 1);
        s.push("новая после миграции", 1).unwrap();
        s.save().unwrap();
        drop(s);
        let disk = all_files_text(&t.0);
        assert!(!disk
            .windows(8)
            .any(|w| w == "старая".as_bytes() || w == b"some tex"));
        let mut s = Store::open_with(&t.0, 0, Some(key(3))).unwrap();
        assert_eq!(s.len(), 3001);
        assert_eq!(previews(&mut s, "после миграции", View::All).len(), 1);
    }

    #[test]
    fn images_have_their_own_view() {
        let t = TempDir::new("img");
        let mut s = Store::open_with(&t.0, 0, Some(key(4))).unwrap();
        let r = text::ImageRef {
            hash: 0x1234_5678_9abc_def0,
            width: 800,
            height: 600,
            bytes: 12_345,
        };
        s.push("обычный текст", 1).unwrap();
        s.push(&text::image_marker(&r), 2).unwrap();
        assert_eq!(
            previews(&mut s, "", View::Images),
            vec!["Image 800×600 · 12 KB"]
        );
        assert_eq!(
            previews(&mut s, "скриншот", View::All),
            vec!["Image 800×600 · 12 KB"]
        );
        assert_eq!(s.image_refs(), vec![(2, r.hash, false)]);
    }
}
