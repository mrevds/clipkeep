//! End-to-end encrypted sync between devices.
//!
//! Every device appends immutable, encrypted batches of changes ("ops") to a shared place (a
//! `Backend`: a folder or Google Drive) and applies the batches written by the others. There is
//! no server logic: the backend only stores blobs it cannot read.
//!
//! ```text
//! vault.ckv                 the password/recovery vault (public by design, see crypto::vault)
//! dev-<device>-<n>.ops      one batch of ops from one device, sealed with the master key
//! img-<hash>.blob           one image (thumbnail + PNG), sealed with the master key
//! ```
//!
//! Merging is last-writer-wins per clip: a clip is identified by the hash of its text; deletes
//! leave a timestamp so an older "add" arriving later cannot bring the clip back.
//! The work is split in three so the network never runs while the history is locked:
//! `push` and `fetch` talk to the backend, `apply` only touches the local store.

pub mod backend;
#[cfg(feature = "google")]
pub mod google;
pub mod onboard;
pub mod settings;

use crate::crypto::Cipher;
use crate::store::media::Media;
use crate::store::text;
use crate::store::Store;
use backend::Backend;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, ErrorKind};
use std::path::PathBuf;

pub const VAULT_FILE: &str = "vault.ckv";
const SNAPSHOT_CHUNK: usize = 1000;
/// Images bigger than this stay on the device they came from.
const MAX_SYNC_IMAGE: u64 = 30 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Op {
    Add { h: u64, text: String, time: u32 },
    Remove { h: u64, time: u32 },
    Pin { h: u64, on: bool, time: u32 },
    Group { h: u64, name: String, time: u32 },
}

impl Op {
    fn time(&self) -> u32 {
        match self {
            Op::Add { time, .. }
            | Op::Remove { time, .. }
            | Op::Pin { time, .. }
            | Op::Group { time, .. } => *time,
        }
    }

    pub fn add(text: &str, time: u32) -> Op {
        Op::Add {
            h: text::fnv1a(text.as_bytes()),
            text: text.to_owned(),
            time,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    device: String,
    next_seq: u32,
    initial_done: bool,
    applied: HashSet<String>,
    uploaded_images: HashSet<u64>,
    tombstones: HashMap<u64, u32>,
    pin_stamps: HashMap<u64, u32>,
    group_stamps: HashMap<u64, u32>,
}

/// Batches downloaded but not applied yet.
pub struct Fetched {
    files: Vec<(String, Vec<Op>)>,
}

impl Fetched {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// One batch per downloaded file, so the history lock is taken for short moments.
    pub fn batches(self) -> Vec<Fetched> {
        self.files
            .into_iter()
            .map(|f| Fetched { files: vec![f] })
            .collect()
    }
}

pub struct Syncer {
    backend: Box<dyn Backend>,
    master: Cipher,
    local: Option<Cipher>,
    state: State,
    state_path: PathBuf,
    media: Media,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg.into())
}

fn pack_ops(master: &Cipher, name: &str, ops: &[Op]) -> io::Result<Vec<u8>> {
    let json = serde_json::to_vec(ops).map_err(io::Error::other)?;
    Ok(master.seal(
        &miniz_oxide::deflate::compress_to_vec(&json, 6),
        name.as_bytes(),
    ))
}

fn unpack_ops(master: &Cipher, name: &str, data: &[u8]) -> io::Result<Vec<Op>> {
    let comp = master
        .open(data, name.as_bytes())
        .ok_or_else(|| bad("does not decrypt (wrong key or tampered)"))?;
    let json = miniz_oxide::inflate::decompress_to_vec_with_limit(&comp, 256 << 20)
        .map_err(|_| bad("corrupt batch"))?;
    serde_json::from_slice(&json).map_err(|e| bad(format!("bad batch: {e}")))
}

fn blob_name(hash: u64) -> String {
    format!("img-{hash:016x}.blob")
}

impl Syncer {
    pub fn new(
        backend: Box<dyn Backend>,
        master: [u8; 32],
        local: Option<Cipher>,
        media: Media,
        state_path: PathBuf,
        device: &str,
    ) -> io::Result<Syncer> {
        let mut s = Syncer {
            backend,
            master: Cipher::new(&master),
            local,
            state: State::default(),
            state_path,
            media,
        };
        match std::fs::read(&s.state_path) {
            Ok(bytes) => {
                let plain = match &s.local {
                    Some(c) => c
                        .open(&bytes, b"sync state")
                        .ok_or_else(|| bad("sync state does not decrypt"))?,
                    None => bytes,
                };
                s.state =
                    serde_json::from_slice(&plain).map_err(|e| bad(format!("sync state: {e}")))?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if s.state.device.is_empty() {
            s.state.device = device.to_owned();
        }
        Ok(s)
    }

    pub fn backend_name(&self) -> String {
        self.backend.name()
    }

    fn save_state(&self) -> io::Result<()> {
        let json = serde_json::to_vec(&self.state).map_err(io::Error::other)?;
        let data = match &self.local {
            Some(c) => c.seal(&json, b"sync state"),
            None => json,
        };
        let tmp = self.state_path.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, &self.state_path)
    }

    /// Records a local change so older remote ops cannot undo it.
    pub fn note(&mut self, op: &Op) {
        match op {
            Op::Remove { h, time } => {
                let t = self.state.tombstones.entry(*h).or_default();
                *t = (*t).max(*time);
            }
            Op::Pin { h, time, .. } => {
                let t = self.state.pin_stamps.entry(*h).or_default();
                *t = (*t).max(*time);
            }
            Op::Group { h, time, .. } => {
                let t = self.state.group_stamps.entry(*h).or_default();
                *t = (*t).max(*time);
            }
            Op::Add { .. } => {}
        }
    }

    pub fn needs_snapshot(&self) -> bool {
        !self.state.initial_done
    }

    /// The whole local history as ops, for the first upload from this device.
    pub fn snapshot(store: &mut Store) -> Vec<Op> {
        let mut ops = Vec::new();
        for c in store.export() {
            let h = text::fnv1a(c.text.as_bytes());
            ops.push(Op::Add {
                h,
                text: c.text,
                time: c.used,
            });
            if c.pinned {
                ops.push(Op::Pin {
                    h,
                    on: true,
                    time: c.used,
                });
            }
            if !c.group.is_empty() {
                ops.push(Op::Group {
                    h,
                    name: c.group,
                    time: c.used,
                });
            }
        }
        ops
    }

    pub fn mark_snapshot_done(&mut self) -> io::Result<()> {
        self.state.initial_done = true;
        self.save_state()
    }

    /// Uploads `ops` (and the images they mention) as one or more batches.
    pub fn push(&mut self, ops: &[Op]) -> io::Result<()> {
        for chunk in ops.chunks(SNAPSHOT_CHUNK) {
            for op in chunk {
                let Op::Add { text, .. } = op else { continue };
                let Some(img) = text::parse_image_marker(text) else {
                    continue;
                };
                if self.state.uploaded_images.contains(&img.hash) {
                    continue;
                }
                let Ok(plain) = self.media.export(img.hash) else {
                    continue;
                };
                if plain.len() as u64 > MAX_SYNC_IMAGE {
                    continue;
                }
                let name = blob_name(img.hash);
                self.backend
                    .put(&name, &self.master.seal(&plain, name.as_bytes()))?;
                self.state.uploaded_images.insert(img.hash);
            }
            let name = format!("dev-{}-{:08}.ops", self.state.device, self.state.next_seq);
            self.backend
                .put(&name, &pack_ops(&self.master, &name, chunk)?)?;
            self.state.applied.insert(name);
            self.state.next_seq += 1;
            self.save_state()?;
        }
        Ok(())
    }

    /// Downloads batches from other devices that have not been applied yet.
    pub fn fetch(&mut self) -> io::Result<Fetched> {
        let mut names: Vec<String> = self
            .backend
            .list()?
            .into_iter()
            .filter(|n| {
                n.starts_with("dev-") && n.ends_with(".ops") && !self.state.applied.contains(n)
            })
            .collect();
        names.sort();
        let mut files = Vec::new();
        for name in names {
            let data = self.backend.get(&name)?;
            let ops = match unpack_ops(&self.master, &name, &data) {
                Ok(ops) => ops,
                Err(_) => {
                    // Never going to decrypt: skip it for good instead of retrying every cycle.
                    self.state.applied.insert(name);
                    continue;
                }
            };
            let mut complete = true;
            for op in &ops {
                let Op::Add { text, .. } = op else { continue };
                let Some(img) = text::parse_image_marker(text) else {
                    continue;
                };
                if self.media.size(img.hash).is_some() {
                    continue;
                }
                let blob = blob_name(img.hash);
                match self.backend.get(&blob) {
                    Ok(data) => {
                        let plain = self
                            .master
                            .open(&data, blob.as_bytes())
                            .ok_or_else(|| bad("image does not decrypt"))?;
                        self.media.import(img.hash, &plain)?;
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => complete = false, // not uploaded yet
                    Err(e) => return Err(e),
                }
            }
            if complete {
                files.push((name, ops));
            }
        }
        self.save_state()?;
        Ok(Fetched { files })
    }

    /// Merges downloaded batches into the local history. Returns how many ops changed something.
    pub fn apply(&mut self, fetched: Fetched, store: &mut Store) -> io::Result<usize> {
        let mut all: Vec<&Op> = fetched.files.iter().flat_map(|(_, ops)| ops).collect();
        all.sort_by_key(|op| op.time()); // stable: ops of one batch keep their order
        let mut changed = 0;
        for op in all {
            match op {
                Op::Add { text, time, .. } => {
                    let h = text::fnv1a(text.as_bytes());
                    if self.state.tombstones.get(&h).is_some_and(|t| *t >= *time) {
                        continue;
                    }
                    if let Some(img) = text::parse_image_marker(text) {
                        if self.media.size(img.hash).is_none() {
                            continue;
                        }
                    }
                    if store.push(text, *time)? {
                        changed += 1;
                    }
                }
                Op::Remove { h, time } => {
                    let t = self.state.tombstones.entry(*h).or_default();
                    *t = (*t).max(*time);
                    if let Some(id) = store.id_for_hash(*h) {
                        if store.get(id).is_some_and(|m| m.used <= *time) && store.remove(id)? {
                            changed += 1;
                        }
                    }
                }
                Op::Pin { h, on, time } => {
                    let stamp = self.state.pin_stamps.entry(*h).or_default();
                    if *stamp < *time {
                        *stamp = *time;
                        if let Some(id) = store.id_for_hash(*h) {
                            store.set_pinned(id, *on);
                            changed += 1;
                        }
                    }
                }
                Op::Group { h, name, time } => {
                    let stamp = self.state.group_stamps.entry(*h).or_default();
                    if *stamp < *time {
                        *stamp = *time;
                        if let Some(id) = store.id_for_hash(*h) {
                            if store.set_group(id, name).is_ok() {
                                changed += 1;
                            }
                        }
                    }
                }
            }
        }
        for (name, _) in fetched.files {
            self.state.applied.insert(name);
        }
        self.save_state()?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::backend::memory::Memory;
    use super::*;

    struct Device {
        dir: PathBuf,
        store: Store,
        syncer: Syncer,
    }

    fn device(name: &str, remote: &Memory, master: [u8; 32]) -> Device {
        let dir = std::env::temp_dir().join(format!("clipkeep-sync-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let local = Cipher::new(&crate::crypto::random_key()); // each device has its own local key
        let store = Store::open_with(&dir, 0, Some(local.clone())).unwrap();
        let media = Media::new(&dir, Some(local.clone()));
        let syncer = Syncer::new(
            Box::new(remote.clone()),
            master,
            Some(local),
            media,
            dir.join("sync-state.bin"),
            name,
        )
        .unwrap();
        Device { dir, store, syncer }
    }

    impl Device {
        fn sync(&mut self, ops: &[Op]) -> usize {
            self.syncer.push(ops).unwrap();
            let f = self.syncer.fetch().unwrap();
            self.syncer.apply(f, &mut self.store).unwrap()
        }

        fn clips(&mut self) -> Vec<String> {
            self.store
                .search("", crate::store::View::All, 100, 100)
                .into_iter()
                .map(|h| h.preview)
                .collect()
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn two_devices_converge() {
        let remote = Memory::default();
        let master = crate::crypto::random_key();
        let (mut a, mut b) = (device("a", &remote, master), device("b", &remote, master));

        a.store.push("привет с ноутбука", 10).unwrap();
        a.store.push("https://example.com/a", 11).unwrap();
        let snap = Syncer::snapshot(&mut a.store);
        assert_eq!(a.sync(&snap), 0);
        assert!(b.sync(&[]) >= 2);
        assert_eq!(
            b.clips(),
            vec!["https://example.com/a", "привет с ноутбука"]
        );

        // b copies something and pins an old clip; a deletes one.
        b.store.push("с телефона", 20).unwrap();
        let h = text::fnv1a("привет с ноутбука".as_bytes());
        let id = b.store.id_for_hash(h).unwrap();
        b.store.set_pinned(id, true);
        b.sync(&[
            Op::add("с телефона", 20),
            Op::Pin {
                h,
                on: true,
                time: 21,
            },
        ]);
        a.sync(&[]);
        assert!(a.clips().contains(&"с телефона".to_owned()));
        assert_eq!(
            a.store.search("", crate::store::View::Pinned, 10, 50).len(),
            1
        );

        let hl = text::fnv1a("https://example.com/a".as_bytes());
        let id = a.store.id_for_hash(hl).unwrap();
        a.store.remove(id).unwrap();
        let del = Op::Remove { h: hl, time: 30 };
        a.syncer.note(&del);
        a.sync(&[del]);
        b.sync(&[]);
        assert!(!b.clips().contains(&"https://example.com/a".to_owned()));

        // An old "add" that shows up late must not resurrect the deleted clip.
        let late = Op::add("https://example.com/a", 12);
        b.sync(std::slice::from_ref(&late));
        a.sync(&[]);
        assert!(!a.clips().contains(&"https://example.com/a".to_owned()));
        let _ = late;
    }

    #[test]
    fn remote_holds_only_ciphertext_and_wrong_key_reads_nothing() {
        let remote = Memory::default();
        let mut a = device("c", &remote, crate::crypto::random_key());
        a.store
            .push("совершенно секретный пароль hunter2", 1)
            .unwrap();
        let snap = Syncer::snapshot(&mut a.store);
        a.sync(&snap);
        for (name, data) in remote.0.lock().unwrap().iter() {
            let hay = String::from_utf8_lossy(data);
            assert!(
                !hay.contains("hunter2") && !hay.contains("секретный"),
                "{name} leaks"
            );
        }
        let mut stranger = device("d", &remote, crate::crypto::random_key());
        assert_eq!(stranger.sync(&[]), 0);
        assert!(stranger.clips().is_empty());
    }

    #[test]
    fn images_travel_and_state_survives_restart() {
        let remote = Memory::default();
        let master = crate::crypto::random_key();
        let (mut a, mut b) = (device("e", &remote, master), device("f", &remote, master));
        let rgba: Vec<u8> = (0..64 * 48)
            .flat_map(|i| [(i % 64) as u8, 9, 9, 255])
            .collect();
        let hash = 0x1122_3344_5566_7788;
        let bytes = a.syncer.media.put(hash, 64, 48, &rgba).unwrap();
        let marker = text::image_marker(&text::ImageRef {
            hash,
            width: 64,
            height: 48,
            bytes,
        });
        a.store.push(&marker, 5).unwrap();
        a.sync(&[Op::add(&marker, 5)]);
        b.sync(&[]);
        assert_eq!(
            b.clips(),
            vec!["Image 64×48 · 1 KB".replace("1 KB", &text::human_size(bytes))]
        );
        assert_eq!(b.syncer.media.full(hash).unwrap().2, rgba);

        // Restart b: nothing is applied twice and the device keeps its sequence numbers.
        let b_dir = b.dir.clone();
        let seq = b.syncer.state.next_seq;
        b.syncer.save_state().unwrap();
        let local = b.syncer.local.clone();
        let again = Syncer::new(
            Box::new(remote.clone()),
            master,
            local,
            Media::new(&b_dir, b.syncer.local.clone()),
            b_dir.join("sync-state.bin"),
            "ignored",
        )
        .unwrap();
        assert_eq!(again.state.device, "f");
        assert_eq!(again.state.next_seq, seq);
        assert!(!again.state.applied.is_empty());
    }

    #[test]
    fn folder_backend_rejects_path_tricks() {
        let dir = std::env::temp_dir().join(format!("clipkeep-folder-{}", std::process::id()));
        let mut f = backend::FolderBackend::new(&dir).unwrap();
        assert!(f.put("../evil", b"x").is_err());
        assert!(f.get("a/b").is_err());
        f.put("dev-x-00000001.ops", b"ok").unwrap();
        assert_eq!(f.list().unwrap(), vec!["dev-x-00000001.ops"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
