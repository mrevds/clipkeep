//! Compaction: rewrites the blocks file once more than half of it is deleted clips.

use super::*;

impl Store {
    pub(super) fn garbage(&self) -> (u64, u64) {
        let total: u64 =
            self.blocks.iter().map(|b| b.raw_len as u64).sum::<u64>() + self.open.len() as u64;
        let live: u64 = self.blocks.iter().map(|b| b.live as u64).sum::<u64>() + self.open_live;
        (total - live, total)
    }

    pub(super) fn maybe_compact(&mut self) -> io::Result<()> {
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
}
