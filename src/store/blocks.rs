//! The blocks file: reading, caching and sealing ~64 KiB compressed (and encrypted) blocks,
//! plus the per-block search filter.

use super::*;

pub(super) fn new_filter() -> Filter64 {
    Box::new([0; FILTER_WORDS])
}

pub(super) fn add_to_filter(filter: &mut [u64; FILTER_WORDS], folded: &str) {
    text::for_each_feature(folded, |b| filter[(b / 64) as usize] |= 1 << (b % 64));
}

pub(super) fn filter_has(filter: &[u64; FILTER_WORDS], bits: &[u32]) -> bool {
    bits.iter()
        .all(|&b| filter[(b / 64) as usize] & (1 << (b % 64)) != 0)
}

impl Store {
    pub(super) fn index_path(&self) -> PathBuf {
        self.dir.join("index.bin")
    }

    pub(super) fn blocks_path(&self, gen: u32) -> PathBuf {
        self.dir.join(format!("blocks-{gen}.dat"))
    }

    pub(super) fn open_file(&mut self) -> io::Result<()> {
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

    pub(super) fn remove_stale_files(&self) {
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

    pub(super) fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("store file is opened in Store::open")
    }

    pub(super) fn read_block(&mut self, b: u32) -> io::Result<String> {
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

    pub(super) fn block_text(&mut self, b: u32) -> io::Result<&str> {
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

    pub(super) fn entry_text(&mut self, m: Meta) -> io::Result<&str> {
        let (start, end) = (m.offset as usize, m.offset as usize + m.len as usize);
        self.block_text(m.block)?
            .get(start..end)
            .ok_or_else(|| bad("clip out of block bounds"))
    }

    /// Deflate, then encrypt (bound to the position in the file) if there is a key.
    pub(super) fn pack(&self, raw: &[u8], file_off: u64) -> Vec<u8> {
        let comp = miniz_oxide::deflate::compress_to_vec(raw, BLOCK_LEVEL);
        match &self.cipher {
            Some(c) => c.seal(&comp, &file_off.to_le_bytes()),
            None => comp,
        }
    }

    /// Compresses the open block and appends it to the blocks file.
    pub(super) fn seal(&mut self) -> io::Result<()> {
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
}
