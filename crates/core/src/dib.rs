//! Windows device-independent bitmaps (the clipboard's `CF_DIB` format):
//! a BITMAPINFOHEADER followed by the pixels. Converted to and from RGBA8
//! here so it can be tested anywhere.

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// CF_DIB / CF_DIBV5 contents (24 or 32 bits per pixel, uncompressed) →
/// (width, height, RGBA8 rows top to bottom).
pub fn to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let header = u32_at(dib, 0)? as usize;
    let w = u32_at(dib, 4)? as i32;
    let h = u32_at(dib, 8)? as i32;
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?) as usize;
    let compression = u32_at(dib, 16)?;
    let colors_used = u32_at(dib, 32)? as usize;
    if header < 40 || w <= 0 || h == 0 || !matches!(bits, 24 | 32) {
        return None;
    }
    let (w, top_down, h) = (w as usize, h < 0, h.unsigned_abs() as usize);
    if w.checked_mul(h)? > crate::png::MAX_PIXELS {
        return None;
    }
    // Channel masks: BI_BITFIELDS stores them after a 40-byte header (inside
    // a V4/V5 header); BI_RGB 32-bit is BGRX.
    let (masks, mut offset) = match compression {
        BI_RGB => ([0xff_0000, 0xff00, 0xff, 0], header + colors_used * 4),
        BI_BITFIELDS if bits == 32 => {
            let m =
                [u32_at(dib, 40)?, u32_at(dib, 44)?, u32_at(dib, 48)?, if header >= 56 { u32_at(dib, 52)? } else { 0 }];
            (m, if header == 40 { 52 } else { header } + colors_used * 4)
        }
        _ => return None,
    };
    // Some apps leave the 12 mask bytes after a plain header even for BI_RGB.
    let stride = (w * bits).div_ceil(32) * 4;
    if compression == BI_RGB && dib.len() == offset + 12 + stride * h {
        offset += 12;
    }
    let pixels = dib.get(offset..offset + stride * h)?;
    let channel = |v: u32, m: u32| if m == 0 { 255 } else { ((v & m) >> m.trailing_zeros()) as u8 };
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &pixels[(if top_down { y } else { h - 1 - y }) * stride..][..stride];
        for x in 0..w {
            if bits == 24 {
                let p = &row[x * 3..x * 3 + 3];
                rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
            } else {
                let v = u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into().unwrap());
                let a = if masks[3] == 0 && compression == BI_RGB { (v >> 24) as u8 } else { channel(v, masks[3]) };
                rgba.extend_from_slice(&[channel(v, masks[0]), channel(v, masks[1]), channel(v, masks[2]), a]);
            }
        }
    }
    // 32-bit BI_RGB's fourth byte is usually unused (zero): then it's opaque.
    if bits == 32 && compression == BI_RGB && rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        rgba.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
    }
    Some((w as u32, h as u32, rgba))
}

/// RGBA8 (rows top to bottom) → CF_DIB contents: 32-bit BGRA, bottom-up.
pub fn from_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    assert_eq!(rgba.len(), wu * hu * 4);
    let mut out = Vec::with_capacity(40 + rgba.len());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&((wu * hu * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]); // resolution, palette
    for y in (0..hu).rev() {
        for p in rgba[y * wu * 4..(y + 1) * wu * 4].as_chunks::<4>().0 {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let rgba = [1, 2, 3, 255, 4, 5, 6, 128, 7, 8, 9, 0, 10, 11, 12, 255, 13, 14, 15, 255, 16, 17, 18, 255];
        let dib = from_rgba(3, 2, &rgba);
        assert_eq!(to_rgba(&dib), Some((3, 2, rgba.to_vec())));
    }

    #[test]
    fn bottom_up_24_bit_with_padding() {
        // 1x2, 24-bit: each row padded to 4 bytes; bottom row first.
        let mut dib = vec![0u8; 40];
        dib[0] = 40;
        dib[4] = 1;
        dib[8] = 2;
        dib[12] = 1;
        dib[14] = 24;
        dib.extend_from_slice(&[30, 20, 10, 0, 60, 50, 40, 0]);
        assert_eq!(to_rgba(&dib), Some((1, 2, vec![40, 50, 60, 255, 10, 20, 30, 255])));
    }

    #[test]
    fn zero_alpha_means_opaque() {
        let mut dib = from_rgba(1, 1, &[9, 8, 7, 0]);
        assert_eq!(to_rgba(&dib).unwrap().2, [9, 8, 7, 255]);
        dib.truncate(30);
        assert_eq!(to_rgba(&dib), None);
    }
}
