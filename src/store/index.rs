//! `index.bin`: metadata of every clip, the block table, groups and the open block,
//! written atomically (temp file + rename), compressed and optionally encrypted.

use super::*;

impl Store {
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

    pub(super) fn encode(&self) -> Vec<u8> {
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

    pub(super) fn decode(&mut self, file: &[u8]) -> io::Result<Format> {
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
pub(super) enum Format {
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
