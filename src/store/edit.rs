//! Write side: adding clips, pins, groups, deletes and the entry limit.

use super::*;

impl Store {
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

    pub(super) fn remove_at(&mut self, i: usize) -> Meta {
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

    pub(super) fn enforce_limit(&mut self) {
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
}
