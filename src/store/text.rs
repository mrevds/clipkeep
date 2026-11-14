//! Small text helpers shared by the store, the CLI and the popup.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Text = 0,
    Link = 1,
    Image = 2,
}

impl Kind {
    pub fn from_u8(v: u8) -> Kind {
        match v {
            1 => Kind::Link,
            2 => Kind::Image,
            _ => Kind::Text,
        }
    }
}

/// A clip is a link when it is one token that starts like a URL.
pub fn detect_kind(text: &str) -> Kind {
    if parse_image_marker(text).is_some() {
        return Kind::Image;
    }
    let t = text.trim();
    let starts =
        |p: &str| t.len() >= p.len() && t.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes());
    let link = !t.contains(char::is_whitespace)
        && ["http://", "https://", "ftp://", "www.", "mailto:"]
            .iter()
            .any(|p| starts(p));
    if link {
        Kind::Link
    } else {
        Kind::Text
    }
}

/// Images live in `media.rs`; the text store only holds this one-line marker for them, so
/// search, groups, pins, dedupe and ordering work the same as for text. The trailing words
/// make "image", "screenshot" or "скриншот" find them.
const IMG_PREFIX: &str = "\u{1}img:";
const IMG_WORDS: &str = " image picture screenshot картинка скриншот";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub hash: u64,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

pub fn image_marker(r: &ImageRef) -> String {
    format!(
        "{IMG_PREFIX}{:016x}:{}x{}:{}{IMG_WORDS}",
        r.hash, r.width, r.height, r.bytes
    )
}

/// Strict on purpose: the hash becomes a file name.
pub fn parse_image_marker(text: &str) -> Option<ImageRef> {
    let rest = text.strip_prefix(IMG_PREFIX)?;
    let token = rest.split(' ').next()?;
    let mut f = token.split(':');
    let hex = f.next()?;
    if hex.len() != 16
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let (w, h) = f.next()?.split_once('x')?;
    let r = ImageRef {
        hash: u64::from_str_radix(hex, 16).ok()?,
        width: w.parse().ok()?,
        height: h.parse().ok()?,
        bytes: f.next()?.parse().ok()?,
    };
    (f.next().is_none() && rest.len() == token.len() + IMG_WORDS.len() && rest.ends_with(IMG_WORDS))
        .then_some(r)
}

pub fn human_size(b: u64) -> String {
    match b {
        0..=1023 => format!("{b} B"),
        1024..=1048575 => format!("{:.0} KB", b as f64 / 1024.0),
        _ => format!("{:.1} MB", b as f64 / 1048576.0),
    }
}

/// What the list shows for a clip (images get a description instead of the marker).
pub fn clip_preview(t: &str, max_chars: usize) -> String {
    match parse_image_marker(t) {
        Some(r) => format!("Image {}×{} · {}", r.width, r.height, human_size(r.bytes)),
        None => preview(t, max_chars),
    }
}

/// 64-bit FNV-1a: stable across Rust versions, so it can live in the index file.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Case-folds `s` into `out` char by char (Cyrillic included), with an ASCII fast path.
/// Search and the block filters both use this, so they always agree.
pub fn fold(s: &str, out: &mut String) {
    out.clear();
    if s.is_ascii() {
        out.push_str(s);
        out.make_ascii_lowercase();
        return;
    }
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
}

/// Bits in one block filter (1 KiB).
pub const FILTER_BITS: u32 = 8192;

/// Calls `f` with a filter bit for every char and every pair of neighbouring chars
/// inside a word. A word matches a text only if all its bits are set in the text's filter.
pub fn for_each_feature(folded: &str, mut f: impl FnMut(u32)) {
    let mut prev: Option<char> = None;
    for c in folded.chars() {
        if c.is_whitespace() {
            prev = None;
            continue;
        }
        f(mix(c as u32, 0x9E37_79B9));
        if let Some(p) = prev {
            f(mix(((p as u32) << 21) ^ c as u32, 0x85EB_CA6B));
        }
        prev = Some(c);
    }
}

fn mix(x: u32, k: u32) -> u32 {
    let h = x.wrapping_mul(k);
    (h ^ (h >> 15)).wrapping_mul(0x2C1B_3C6D) >> 19
}

/// One-line preview for lists: whitespace collapsed, cut at `max_chars` characters
/// (on char boundaries, so Cyrillic and emoji stay intact). Never contains tabs or newlines.
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count >= max_chars {
                out.push('…');
                return out;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(detect_kind("https://example.com/путь"), Kind::Link);
        assert_eq!(detect_kind("  WWW.site.uz \n"), Kind::Link);
        assert_eq!(detect_kind("see https://example.com"), Kind::Text);
        assert_eq!(detect_kind("htt"), Kind::Text);
        assert_eq!(detect_kind("привет"), Kind::Text);
    }

    #[test]
    fn fold_cyrillic_and_ascii() {
        let mut s = String::new();
        fold("Привет МИР Hello", &mut s);
        assert_eq!(s, "привет мир hello");
        fold("ABC", &mut s);
        assert_eq!(s, "abc");
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn features_fit_filter_and_skip_spaces() {
        let mut bits = Vec::new();
        for_each_feature("ab cd", |b| bits.push(b));
        assert_eq!(bits.len(), 6); // a, b, ab, c, d, cd — no "b c" pair
        assert!(bits.iter().all(|&b| b < FILTER_BITS));
    }

    #[test]
    fn preview_collapses_and_cuts_on_char_boundary() {
        assert_eq!(preview("  a\n\n b\tc ", 80), "a b c");
        assert_eq!(preview("Съешь же ещё", 7), "Съешь ж…");
        assert_eq!(preview("😀😀😀", 2), "😀😀…");
    }

    #[test]
    fn image_markers_are_strict() {
        let r = ImageRef {
            hash: 0xdead_beef_0123_4567,
            width: 1920,
            height: 1080,
            bytes: 345_678,
        };
        let m = image_marker(&r);
        assert_eq!(parse_image_marker(&m), Some(r));
        assert_eq!(detect_kind(&m), Kind::Image);
        assert_eq!(clip_preview(&m, 50), "Image 1920×1080 · 338 KB");
        for bad in [
            "\u{1}img:../../etc/passwd:1x1:1 image picture screenshot картинка скриншот",
            "\u{1}img:DEADBEEF01234567:1x1:1 image picture screenshot картинка скриншот",
            "\u{1}img:deadbeef01234567:1x1:1",
            "\u{1}img:deadbeef01234567:1x1:1 evil",
            "img:deadbeef01234567:1x1:1 image picture screenshot картинка скриншот",
        ] {
            assert_eq!(parse_image_marker(bad), None, "{bad}");
        }
    }
}
