//! Minimal PNG codec: writes RGB8/RGBA8 (frame snapshots, QR codes,
//! clipboard images) and reads the common formats into RGBA8 (clipboard
//! images for platforms that want a bitmap). No dependencies.

use crate::deflate;
use std::io::{self, Write};
use std::path::Path;

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
/// Largest image decoded (pixels), so a crafted file can't exhaust memory.
pub const MAX_PIXELS: usize = 64 << 20;

fn crc32(data: &[&[u8]]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = !0u32;
    for d in data {
        for &b in *d {
            c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[ty, data]).to_be_bytes());
}

fn encode(w: u32, h: u32, channels: usize, px: &[u8]) -> Vec<u8> {
    assert_eq!(px.len(), w as usize * h as usize * channels);
    let stride = w as usize * channels;
    let mut raw = Vec::with_capacity(px.len() + h as usize);
    for row in px.chunks_exact(stride.max(1)).take(h as usize) {
        // Filter "Sub" (difference from the pixel to the left): flat areas and
        // gradients become runs that compress well.
        raw.push(1);
        raw.extend_from_slice(&row[..channels.min(row.len())]);
        raw.extend(row.iter().zip(&row[channels.min(row.len())..]).map(|(a, b)| b.wrapping_sub(*a)));
    }
    let mut out = SIGNATURE.to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, if channels == 4 { 6 } else { 2 }, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &deflate::zlib_compress(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Encode RGB8 pixels (`w * h * 3` bytes, rows top to bottom) as PNG.
pub fn encode_rgb(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    encode(w, h, 3, rgb)
}

/// Encode RGBA8 pixels (`w * h * 4` bytes, rows top to bottom) as PNG.
pub fn encode_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    encode(w, h, 4, rgba)
}

pub fn write_rgb(path: &Path, w: u32, h: u32, rgb: &[u8]) -> io::Result<()> {
    std::fs::File::create(path)?.write_all(&encode_rgb(w, h, rgb))
}

/// Length of the PNG at the start of `data` (through its IEND chunk):
/// clipboards may hand back a buffer with padding after it.
pub fn byte_len(data: &[u8]) -> Option<usize> {
    let mut i = SIGNATURE.len();
    if !data.starts_with(SIGNATURE) {
        return None;
    }
    loop {
        let len = u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?) as usize;
        let ty = data.get(i + 4..i + 8)?;
        i = i.checked_add(12 + len)?;
        if i > data.len() {
            return None;
        }
        if ty == b"IEND" {
            return Some(i);
        }
    }
}

/// A decoded image: RGBA8, rows top to bottom.
#[derive(Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Decode a PNG (any bit depth and color type, not interlaced) to RGBA8.
pub fn decode(png: &[u8]) -> Option<Image> {
    let mut data = png.strip_prefix(SIGNATURE)?;
    let (mut w, mut h, mut depth, mut color) = (0usize, 0usize, 0u8, 0u8);
    let (mut palette, mut trns, mut idat) = (Vec::new(), Vec::new(), Vec::new());
    while data.len() >= 12 {
        let len = u32::from_be_bytes(data[..4].try_into().ok()?) as usize;
        let ty = &data[4..8];
        let body = data.get(8..8 + len)?;
        data = data.get(12 + len..)?;
        match ty {
            b"IHDR" if body.len() >= 13 => {
                w = u32::from_be_bytes(body[..4].try_into().ok()?) as usize;
                h = u32::from_be_bytes(body[4..8].try_into().ok()?) as usize;
                (depth, color) = (body[8], body[9]);
                if body[12] != 0 {
                    return None; // interlaced
                }
            }
            b"PLTE" => palette = body.to_vec(),
            b"tRNS" => trns = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
    }
    let channels = match color {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    let ok_depth = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        _ => matches!(depth, 8 | 16),
    };
    if !ok_depth || w == 0 || h == 0 || w.checked_mul(h)? > MAX_PIXELS {
        return None;
    }
    let bits_pp = channels * depth as usize;
    let stride = (w * bits_pp).div_ceil(8);
    let bpp = bits_pp.div_ceil(8);
    let mut raw = deflate::zlib_decompress(&idat, h * (stride + 1))?;
    if raw.len() != h * (stride + 1) {
        return None;
    }
    // Undo the per-row filters in place.
    for y in 0..h {
        let (done, rest) = raw.split_at_mut(y * (stride + 1));
        let prev = (y > 0).then(|| &done[done.len() - stride..]);
        let (f, row) = rest[..stride + 1].split_first_mut()?;
        for x in 0..stride {
            let a = if x >= bpp { row[x - bpp] } else { 0 };
            let b = prev.map_or(0, |p| p[x]);
            let c = if x >= bpp { prev.map_or(0, |p| p[x - bpp]) } else { 0 };
            row[x] = row[x].wrapping_add(match *f {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((a as u16 + b as u16) / 2) as u8,
                4 => paeth(a, b, c),
                _ => return None,
            });
        }
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    let max = (1u32 << depth.min(8)) - 1;
    for y in 0..h {
        let row = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        // Sample n of this row, scaled to 8 bits (16-bit: the high byte).
        let sample = |n: usize| -> u8 {
            match depth {
                8 => row[n],
                16 => row[n * 2],
                d => {
                    let bit = n * d as usize;
                    let v = (row[bit / 8] >> (8 - d as usize - bit % 8)) as u32 & max;
                    if color == 3 { v as u8 } else { (v * 255 / max) as u8 }
                }
            }
        };
        let raw16 = |n: usize| -> u16 {
            if depth == 16 { u16::from_be_bytes([row[n * 2], row[n * 2 + 1]]) } else { sample(n) as u16 }
        };
        for x in 0..w {
            let px = match color {
                0 => {
                    let g = sample(x);
                    let key = trns.get(..2).map(|t| u16::from_be_bytes([t[0], t[1]]));
                    let a = if key.is_some_and(|k| k == raw16(x) || (depth < 8 && k as u32 * 255 / max == g as u32)) {
                        0
                    } else {
                        255
                    };
                    [g, g, g, a]
                }
                2 => {
                    let (r, g, b) = (sample(x * 3), sample(x * 3 + 1), sample(x * 3 + 2));
                    let key = trns.get(..6).map(|t| {
                        [
                            u16::from_be_bytes([t[0], t[1]]),
                            u16::from_be_bytes([t[2], t[3]]),
                            u16::from_be_bytes([t[4], t[5]]),
                        ]
                    });
                    let a = if key == Some([raw16(x * 3), raw16(x * 3 + 1), raw16(x * 3 + 2)]) { 0 } else { 255 };
                    [r, g, b, a]
                }
                3 => {
                    let i = sample(x) as usize;
                    let p = palette.get(i * 3..i * 3 + 3)?;
                    [p[0], p[1], p[2], trns.get(i).copied().unwrap_or(255)]
                }
                4 => {
                    let g = sample(x * 2);
                    [g, g, g, sample(x * 2 + 1)]
                }
                _ => [sample(x * 4), sample(x * 4 + 1), sample(x * 4 + 2), sample(x * 4 + 3)],
            };
            rgba.extend_from_slice(&px);
        }
    }
    Some(Image { width: w as u32, height: h as u32, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_value() {
        assert_eq!(crc32(&[b"IEND"]), 0xAE42_6082);
        let png = encode_rgb(2, 1, &[255, 0, 0, 0, 0, 255]);
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn round_trip() {
        let rgb = [255, 0, 0, 0, 0, 255, 10, 20, 30, 40, 50, 60];
        let img = decode(&encode_rgb(2, 2, &rgb)).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.rgba, [255, 0, 0, 255, 0, 0, 255, 255, 10, 20, 30, 255, 40, 50, 60, 255]);
        let mut rgba = Vec::new();
        for i in 0..(37 * 23) {
            rgba.extend_from_slice(&[(i * 7) as u8, (i / 37) as u8, 128, (i % 3 * 100) as u8]);
        }
        let img = decode(&encode_rgba(37, 23, &rgba)).unwrap();
        assert_eq!(img.rgba, rgba);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(decode(b"not a png"), None);
        let mut png = encode_rgb(2, 1, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(byte_len(&png), Some(png.len()));
        png.extend_from_slice(&[0; 7]);
        assert_eq!(byte_len(&png), Some(png.len() - 7));
        png.truncate(png.len() - 27);
        assert_eq!((decode(&png), byte_len(&png)), (None, None));
    }
}
