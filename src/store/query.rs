//! Read side: search, lookups by hash, export for sync, image references and stats.

use super::*;

impl Store {
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
}
