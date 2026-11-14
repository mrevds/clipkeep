//! Image clips. The text store holds a short marker per image (`text::image_marker`); the pixels
//! live here, one file per image in `<data dir>/images/`: a small PNG thumbnail for the list
//! plus the full PNG, encrypted with the user's key when there is one.

use crate::crypto::Cipher;
use std::collections::HashSet;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// Larger images (raw RGBA) are ignored, to keep memory and encode time bounded.
pub const MAX_RGBA_BYTES: usize = 64 << 20;
const THUMB_W: usize = 160;
const THUMB_H: usize = 48;

#[derive(Clone)]
pub struct Media {
    dir: PathBuf,
    cipher: Option<Cipher>,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg.into())
}

impl Media {
    pub fn new(data_dir: &Path, cipher: Option<Cipher>) -> Media {
        Media {
            dir: data_dir.join("images"),
            cipher,
        }
    }

    fn path(&self, hash: u64) -> PathBuf {
        self.dir.join(format!("{hash:016x}.img"))
    }

    pub fn size(&self, hash: u64) -> Option<u64> {
        std::fs::metadata(self.path(hash)).ok().map(|m| m.len())
    }

    /// Total bytes of all stored images.
    pub fn total_size(&self) -> u64 {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        dir.flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// Stores an RGBA image and returns the file size.
    pub fn put(&self, hash: u64, width: u32, height: u32, rgba: &[u8]) -> io::Result<u64> {
        let full = encode_png(
            rgba,
            width as usize,
            height as usize,
            png::Compression::Fast,
        )?;
        let (tw, th, thumb) = thumbnail(rgba, width as usize, height as usize);
        let thumb = encode_png(&thumb, tw, th, png::Compression::Default)?;
        let mut plain = Vec::with_capacity(4 + thumb.len() + full.len());
        plain.extend_from_slice(&(thumb.len() as u32).to_le_bytes());
        plain.extend_from_slice(&thumb);
        plain.extend_from_slice(&full);
        let data = match &self.cipher {
            Some(c) => c.seal(&plain, &hash.to_le_bytes()),
            None => plain,
        };
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join("tmp.part");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, self.path(hash))?;
        Ok(data.len() as u64)
    }

    fn load(&self, hash: u64) -> io::Result<Vec<u8>> {
        let data = std::fs::read(self.path(hash))?;
        match &self.cipher {
            Some(c) => c
                .open(&data, &hash.to_le_bytes())
                .ok_or_else(|| bad("image does not decrypt (wrong key?)")),
            None => Ok(data),
        }
    }

    fn split(plain: &[u8]) -> io::Result<(&[u8], &[u8])> {
        let n = u32::from_le_bytes(
            plain
                .get(..4)
                .ok_or_else(|| bad("short image file"))?
                .try_into()
                .expect("4 bytes"),
        );
        let rest = &plain[4..];
        if n as usize > rest.len() {
            return Err(bad("damaged image file"));
        }
        Ok(rest.split_at(n as usize))
    }

    /// PNG bytes of the thumbnail.
    pub fn thumb_png(&self, hash: u64) -> io::Result<Vec<u8>> {
        let plain = self.load(hash)?;
        Ok(Self::split(&plain)?.0.to_vec())
    }

    /// Full image as (width, height, RGBA).
    pub fn full(&self, hash: u64) -> io::Result<(u32, u32, Vec<u8>)> {
        let plain = self.load(hash)?;
        decode_png(Self::split(&plain)?.1)
    }

    /// The file's content without local encryption, for sync (which encrypts it with its own key).
    pub fn export(&self, hash: u64) -> io::Result<Vec<u8>> {
        self.load(hash)
    }

    /// Stores content received from another device. Checks it is a well-formed image file.
    pub fn import(&self, hash: u64, plain: &[u8]) -> io::Result<()> {
        let (thumb, full) = Self::split(plain)?;
        decode_png(thumb)?;
        decode_png(full)?;
        let data = match &self.cipher {
            Some(c) => c.seal(plain, &hash.to_le_bytes()),
            None => plain.to_vec(),
        };
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join("tmp.part");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, self.path(hash))
    }

    pub fn remove(&self, hash: u64) {
        let _ = std::fs::remove_file(self.path(hash));
    }

    /// Deletes every image that is not in `keep` (and leftovers of an interrupted write).
    pub fn sweep(&self, keep: &HashSet<u64>) {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in dir.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let hash = name
                .strip_suffix(".img")
                .and_then(|h| u64::from_str_radix(h, 16).ok());
            if hash.is_none_or(|h| !keep.contains(&h)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub fn encode_png(rgba: &[u8], w: usize, h: usize, level: png::Compression) -> io::Result<Vec<u8>> {
    if w == 0 || h == 0 || rgba.len() != w * h * 4 {
        return Err(bad("bad image size"));
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(level);
    let mut writer = enc.write_header().map_err(io::Error::other)?;
    writer.write_image_data(rgba).map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)?;
    Ok(out)
}

pub fn decode_png(bytes: &[u8]) -> io::Result<(u32, u32, Vec<u8>)> {
    let mut reader = png::Decoder::new(io::Cursor::new(bytes))
        .read_info()
        .map_err(io::Error::other)?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(io::Error::other)?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(bad("unexpected PNG format"));
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

/// Box-filter downscale to fit THUMB_W × THUMB_H (never upscales).
pub fn thumbnail(rgba: &[u8], w: usize, h: usize) -> (usize, usize, Vec<u8>) {
    let scale = (w as f64 / THUMB_W as f64)
        .max(h as f64 / THUMB_H as f64)
        .max(1.0);
    let tw = ((w as f64 / scale).round() as usize).max(1);
    let th = ((h as f64 / scale).round() as usize).max(1);
    let mut out = Vec::with_capacity(tw * th * 4);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1).min(w));
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &rgba[(y * w + x) * 4..][..4];
                    for c in 0..4 {
                        sum[c] += p[c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            out.extend(sum.iter().map(|s| (s / n) as u8));
        }
    }
    (tw, th, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| [(i % w) as u8, (i / w) as u8, 77, 255])
            .collect()
    }

    #[test]
    fn put_get_roundtrip_encrypted_and_sweep() {
        let dir = std::env::temp_dir().join(format!("clipkeep-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = Media::new(&dir, Some(Cipher::new(&[9; 32])));
        let img = gradient(300, 200);
        let size = m.put(0xabc, 300, 200, &img).unwrap();
        assert_eq!(m.size(0xabc), Some(size));
        let (w, h, back) = m.full(0xabc).unwrap();
        assert_eq!((w, h), (300, 200));
        assert_eq!(back, img);
        let (tw, th, _) = decode_png(&m.thumb_png(0xabc).unwrap()).unwrap();
        assert!(tw as usize <= THUMB_W && th as usize <= THUMB_H && tw > 0 && th > 0);
        // no PNG signature in clear text on disk
        let raw = std::fs::read(m.path(0xabc)).unwrap();
        assert!(!raw.windows(4).any(|w| w == b"IHDR"));
        assert!(Media::new(&dir, Some(Cipher::new(&[1; 32])))
            .full(0xabc)
            .is_err());
        m.put(0xdef, 4, 4, &gradient(4, 4)).unwrap();
        assert_eq!(
            m.total_size(),
            m.size(0xabc).unwrap() + m.size(0xdef).unwrap()
        );
        m.sweep(&HashSet::from([0xdef]));
        assert!(m.size(0xabc).is_none() && m.size(0xdef).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn thumbnail_keeps_aspect_and_never_upscales() {
        assert_eq!(thumbnail(&gradient(1600, 900), 1600, 900).0, 85);
        let (tw, th, px) = thumbnail(&gradient(1600, 900), 1600, 900);
        assert_eq!((tw, th, px.len()), (85, 48, 85 * 48 * 4));
        let (tw, th, _) = thumbnail(&gradient(10, 6), 10, 6);
        assert_eq!((tw, th), (10, 6));
        let (tw, th, _) = thumbnail(&gradient(5000, 3), 5000, 3);
        assert_eq!((tw, th), (160, 1));
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(encode_png(&[0; 8], 3, 1, png::Compression::Fast).is_err());
        assert!(decode_png(b"not a png").is_err());
    }
}
