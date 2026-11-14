//! Passwords and keys the daemon holds in memory only: never written to disk, never synced,
//! shown masked in the window and forgotten after a while (see `crate::secret` for detection).

use crate::store::text::Kind;
use crate::store::{Hit, View};
use std::time::{Duration, Instant};

/// Ids of in-memory secrets: far above anything the store hands out.
const ID_BASE: u32 = 0xF000_0000;

struct Secret {
    id: u32,
    text: String,
    until: Instant,
}

pub(super) struct Secrets {
    items: Vec<Secret>,
    next: u32,
    /// Zero: detection is off and passwords are stored like any other clip.
    ttl: Duration,
}

impl Secrets {
    pub(super) fn new(ttl: Duration) -> Secrets {
        Secrets {
            items: Vec::new(),
            next: 0,
            ttl,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// True for ids that belong to secrets rather than to the store.
    pub(super) fn owns(id: u32) -> bool {
        id >= ID_BASE
    }

    /// Remembers a secret until `now + ttl`; copying the same one again restarts its timer.
    pub(super) fn add(&mut self, text: String, now: Instant) {
        let until = now + self.ttl;
        if let Some(i) = self.items.iter().position(|s| s.text == text) {
            let mut s = self.items.remove(i);
            s.until = until;
            self.items.push(s);
            return;
        }
        self.items.push(Secret {
            id: ID_BASE + self.next,
            text,
            until,
        });
        self.next = (self.next + 1) % 0x0FFF_FFFF;
    }

    /// Forgets the secrets whose time is up and returns their text (to wipe the clipboard).
    pub(super) fn expire(&mut self, now: Instant) -> Vec<String> {
        if self.items.iter().all(|s| s.until > now) {
            return Vec::new();
        }
        let (expired, kept): (Vec<Secret>, Vec<Secret>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|s| s.until <= now);
        self.items = kept;
        expired.into_iter().map(|s| s.text).collect()
    }

    pub(super) fn get(&self, id: u32) -> Option<&str> {
        self.items
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.text.as_str())
    }

    pub(super) fn remove(&mut self, id: u32) {
        self.items.retain(|s| s.id != id);
    }

    /// Search hits: newest first, masked, only in the All view without a query
    /// (the text of a secret is never searched).
    pub(super) fn hits(&self, view: View, query: &str, now: Instant) -> Vec<Hit> {
        if view != View::All || !query.trim().is_empty() {
            return Vec::new();
        }
        self.items
            .iter()
            .rev()
            .map(|s| Hit {
                id: s.id,
                pinned: false,
                group: 0,
                kind: Kind::Secret as u8,
                preview: format!(
                    "Password hidden · forgotten in {} s",
                    s.until.saturating_duration_since(now).as_secs() + 1
                ),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_expire_and_never_leak_into_previews() {
        let t0 = Instant::now();
        let mut s = Secrets::new(Duration::from_secs(60));
        assert!(s.enabled() && !Secrets::new(Duration::ZERO).enabled());
        s.add("Hunter2023!".into(), t0);
        s.add("Пароль#2024x".into(), t0 + Duration::from_secs(10));

        let hits = s.hits(View::All, "", t0 + Duration::from_secs(30));
        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .all(|h| Secrets::owns(h.id) && h.kind == Kind::Secret as u8));
        assert!(hits
            .iter()
            .all(|h| !h.preview.contains("Hunter") && !h.preview.contains("Пароль")));
        assert_eq!(hits[0].preview, "Password hidden · forgotten in 41 s"); // newest first
        assert!(s.hits(View::All, "Hunter", t0).is_empty()); // never searched
        assert!(s.hits(View::Pinned, "", t0).is_empty());
        assert_eq!(s.get(hits[1].id), Some("Hunter2023!"));

        assert!(s.expire(t0 + Duration::from_secs(59)).is_empty());
        assert_eq!(
            s.expire(t0 + Duration::from_secs(60)),
            vec!["Hunter2023!".to_owned()]
        );
        assert_eq!(s.hits(View::All, "", t0).len(), 1);
    }

    #[test]
    fn copying_again_restarts_the_timer() {
        let t0 = Instant::now();
        let mut s = Secrets::new(Duration::from_secs(60));
        s.add("k9$Lm#2qWz".into(), t0);
        s.add("k9$Lm#2qWz".into(), t0 + Duration::from_secs(50));
        assert_eq!(s.hits(View::All, "", t0).len(), 1);
        assert!(s.expire(t0 + Duration::from_secs(70)).is_empty());
        let id = s.hits(View::All, "", t0)[0].id;
        s.remove(id);
        assert!(s.get(id).is_none());
    }
}
