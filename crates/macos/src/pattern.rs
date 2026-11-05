//! CPU colour helpers and the synthetic test pattern (same design as the
//! Linux one: gradient, moving bar, scrolling "text" band, binary frame
//! counter, remote-pointer marker).

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

#[inline]
pub fn pattern_rgb(x: usize, y: usize, w: usize, h: usize, t: u32) -> (u8, u8, u8) {
    let bar = (t as usize * 7) % w.max(1);
    if y > h * 3 / 4 {
        // "text" region: 1px stripes
        let on = ((x + y / 3 + t as usize) / 2).is_multiple_of(3) && (y / 12).is_multiple_of(2);
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

/// Fill a tightly packed NV12 frame (Y plane, then interleaved CbCr).
pub fn fill_nv12(w: usize, h: usize, t: u32, out: &mut [u8]) {
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for yy in 0..h {
        for xx in 0..w {
            let (r, g, b) = pattern_rgb(xx, yy, w, h, t);
            y_plane[yy * w + xx] = rgb_to_yuv709(r, g, b).0;
        }
    }
    for yy in 0..h / 2 {
        for xx in 0..w / 2 {
            let (r, g, b) = pattern_rgb(xx * 2, yy * 2, w, h, t);
            let (_, u, v) = rgb_to_yuv709(r, g, b);
            uv_plane[yy * w + xx * 2] = u;
            uv_plane[yy * w + xx * 2 + 1] = v;
        }
    }
}

/// Animated NV12 frames built from a precomputed background.
pub struct Pattern {
    pub w: usize,
    pub h: usize,
    background: Vec<u8>,
    pub frame: Vec<u8>,
}

impl Pattern {
    pub fn new(w: usize, h: usize) -> Self {
        let mut background = vec![0u8; w * h * 3 / 2];
        fill_nv12(w, h, 0, &mut background);
        Pattern { w, h, frame: background.clone(), background }
    }

    pub fn fill_rect(&mut self, x0: usize, y0: usize, rw: usize, rh: usize, rgb: (u8, u8, u8)) {
        let (w, h) = (self.w, self.h);
        let (yv, u, v) = rgb_to_yuv709(rgb.0, rgb.1, rgb.2);
        let (x1, y1) = ((x0 + rw).min(w), (y0 + rh).min(h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            self.frame[y * w + x0..y * w + x1].fill(yv);
        }
        let uv = &mut self.frame[w * h..];
        for y in y0 / 2..y1 / 2 {
            for x in x0 / 2..x1 / 2 {
                uv[y * w + x * 2] = u;
                uv[y * w + x * 2 + 1] = v;
            }
        }
    }

    /// Render frame `t` with the pointer marker at normalized (px, py).
    pub fn render(&mut self, t: u32, pointer: (f32, f32)) {
        let (w, h) = (self.w, self.h);
        self.frame.copy_from_slice(&self.background);
        // Scroll the "text" band horizontally.
        let band = h * 3 / 4 + 2;
        let off = (t as usize * 2) % w;
        for y in band..h {
            let row = &self.background[y * w..(y + 1) * w];
            let dst = &mut self.frame[y * w..(y + 1) * w];
            dst[..w - off].copy_from_slice(&row[off..]);
            dst[w - off..].copy_from_slice(&row[..off]);
        }
        // Moving bar.
        let bx = (t as usize * 8) % w.saturating_sub(32).max(1);
        self.fill_rect(bx, 0, 32, h * 3 / 4, (255, 60, 30));
        // Frame counter in binary (16 blocks) so freshness is visible.
        for bit in 0..16 {
            let on = t >> (15 - bit) & 1 != 0;
            self.fill_rect(16 + bit * 24, 16, 20, 20, if on { (255, 255, 255) } else { (0, 0, 0) });
        }
        // Remote pointer marker (shows the input round trip).
        let (cx, cy) = ((pointer.0 * w as f32) as usize, (pointer.1 * h as f32) as usize);
        self.fill_rect(cx.saturating_sub(6), cy.saturating_sub(6), 12, 12, (0, 255, 120));
    }
}

/// BT.709 limited-range NV12 → packed RGB8 (CPU; snapshots only).
pub fn nv12_to_rgb(y: &[u8], uv: &[u8], w: usize, h: usize, y_pitch: usize, uv_pitch: usize, out: &mut [u8]) {
    let c = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    for row in 0..h {
        for col in 0..w {
            let yy = (y[row * y_pitch + col] as f32 - 16.0) * (255.0 / 219.0);
            let i = (row / 2) * uv_pitch + (col / 2) * 2;
            let cb = (uv[i] as f32 - 128.0) * (255.0 / 224.0);
            let cr = (uv[i + 1] as f32 - 128.0) * (255.0 / 224.0);
            let o = (row * w + col) * 3;
            out[o] = c(yy + 1.5748 * cr);
            out[o + 1] = c(yy - 0.1873 * cb - 0.4681 * cr);
            out[o + 2] = c(yy + 1.8556 * cb);
        }
    }
}

/// PSNR (dB) between two 8-bit planes.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_reference_points() {
        assert_eq!(rgb_to_yuv709(0, 0, 0), (16, 128, 128));
        assert_eq!(rgb_to_yuv709(255, 255, 255), (235, 128, 128));
        let (y, _, v) = rgb_to_yuv709(255, 0, 0);
        assert_eq!((y, v), (63, 240));
    }

    #[test]
    fn pattern_changes_every_frame() {
        let mut p = Pattern::new(320, 180);
        p.render(1, (0.5, 0.5));
        let a = p.frame.clone();
        p.render(2, (0.5, 0.5));
        assert_ne!(a, p.frame);
        assert!(psnr(&a, &p.frame, 320, 180, 320, 320) < 40.0);
    }
}
