//! CPU reference colour conversion and quality metrics (tests, benchmarks,
//! synthetic sources). The real pipeline converts on the GPU.

/// BT.709 limited-range RGB -> Y'CbCr for one pixel.
#[inline]
pub fn rgb_to_yuv709(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    (q(16.0 + 219.0 * y), q(128.0 + 224.0 * cb), q(128.0 + 224.0 * cr))
}

/// Fill an NV12 frame (Y plane then interleaved UV) with an animated test
/// pattern: colour gradients, moving bars and fine high-contrast "text"
/// stripes, which stress the encoder like real desktop content.
pub fn test_pattern_nv12(w: usize, h: usize, pitch: usize, t: u32, out: &mut [u8]) {
    let (y_plane, uv_plane) = out.split_at_mut(pitch * h);
    let bar = (t as usize * 7) % w;
    for yy in 0..h {
        for xx in 0..w {
            let (r, g, b) = pattern_rgb(xx, yy, w, h, t, bar);
            y_plane[yy * pitch + xx] = rgb_to_yuv709(r, g, b).0;
        }
    }
    for yy in 0..h / 2 {
        for xx in 0..w / 2 {
            let (r, g, b) = pattern_rgb(xx * 2, yy * 2, w, h, t, bar);
            let (_, u, v) = rgb_to_yuv709(r, g, b);
            uv_plane[yy * pitch + xx * 2] = u;
            uv_plane[yy * pitch + xx * 2 + 1] = v;
        }
    }
}

#[inline]
fn pattern_rgb(x: usize, y: usize, w: usize, h: usize, t: u32, bar: usize) -> (u8, u8, u8) {
    if y > h * 3 / 4 {
        // "text" region: 1px stripes that scroll slowly
        let on = ((x + y / 3 + t as usize) / 2) % 3 == 0 && (y / 12) % 2 == 0;
        return if on { (20, 20, 20) } else { (235, 235, 235) };
    }
    if x.abs_diff(bar) < 24 {
        return (255, 64, 32);
    }
    let r = (x * 255 / w.max(1)) as u8;
    let g = (y * 255 / h.max(1)) as u8;
    let b = ((t * 3) % 256) as u8;
    (r, g, b)
}

/// PSNR (dB) between two planes.
pub fn psnr(a: &[u8], b: &[u8], w: usize, h: usize, pitch_a: usize, pitch_b: usize) -> f64 {
    let mut se = 0f64;
    for y in 0..h {
        for x in 0..w {
            let d = a[y * pitch_a + x] as f64 - b[y * pitch_b + x] as f64;
            se += d * d;
        }
    }
    let mse = se / (w * h) as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}
