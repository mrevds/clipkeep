//! Store tests: dedupe, search, groups, persistence, crash recovery, compaction, encryption.

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
