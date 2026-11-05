//! Minimal PNG writer (RGB8, uncompressed deflate) for diagnostics: frame
//! snapshots from headless clients. No dependencies.

use std::io::{self, Write};
use std::path::Path;

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

/// Encode RGB8 pixels (`w * h * 3` bytes, rows top to bottom) as PNG.
pub fn encode_rgb(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(rgb.len(), (w * h * 3) as usize);
    let mut raw = Vec::with_capacity(rgb.len() + h as usize);
    for row in rgb.chunks_exact(w as usize * 3) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    // zlib stream with stored (uncompressed) deflate blocks.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn write_rgb(path: &Path, w: u32, h: u32, rgb: &[u8]) -> io::Result<()> {
    std::fs::File::create(path)?.write_all(&encode_rgb(w, h, rgb))
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc_known_value() {
        assert_eq!(super::crc32(&[b"IEND"]), 0xAE42_6082);
        let png = super::encode_rgb(2, 1, &[255, 0, 0, 0, 0, 255]);
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }
}
