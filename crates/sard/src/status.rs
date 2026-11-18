//! The viewer window's own screen before video arrives: "Connecting to …",
//! "Starting video…" and connection errors. Drawn on the CPU (Inter glyphs
//! pre-rendered into an atlas by tools/fontatlas) and shown through the video
//! output's still-image path, so it looks the same on every platform.

use crate::font_atlas;
use sard_core::media::Nv12Image;
use std::sync::OnceLock;

/// One pre-rendered glyph: its box in the atlas and how to place it, all in
/// atlas pixels (`font_atlas::SIZE` px per em).
#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    pub ch: char,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// From the pen position to the bitmap's left edge.
    pub left: i16,
    /// From the baseline up to the bitmap's top edge.
    pub top: i16,
    pub advance: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular = 0,
    SemiBold = 1,
}

/// What the screen says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Screen {
    pub title: String,
    /// Under the title; wrapped to a few lines (error messages).
    pub detail: String,
    /// Small print at the bottom ("Close this window to cancel").
    pub hint: String,
    /// Still working: show the animated bar and the seconds elapsed.
    pub busy: bool,
}

type Rgb = [u8; 3];
const BG: Rgb = [17, 19, 23];
const TITLE: Rgb = [238, 240, 243];
const DETAIL: Rgb = [154, 160, 168];
const HINT: Rgb = [110, 115, 124];
const TRACK: Rgb = [40, 43, 50];
const ACCENT: Rgb = [92, 104, 245];

struct Bitmap {
    w: usize,
    h: usize,
    /// One byte (font atlas: coverage) or four (RGBA, straight alpha) per pixel.
    px: Vec<u8>,
}

fn atlas() -> &'static Bitmap {
    static A: OnceLock<Bitmap> = OnceLock::new();
    A.get_or_init(|| {
        let img = sard_core::png::decode(include_bytes!("../assets/font/inter-atlas.png")).expect("font atlas");
        let px = img.rgba.chunks(4).map(|p| p[0]).collect();
        Bitmap { w: img.width as usize, h: img.height as usize, px }
    })
}

fn logo() -> &'static Bitmap {
    static L: OnceLock<Bitmap> = OnceLock::new();
    L.get_or_init(|| {
        let img = sard_core::png::decode(include_bytes!("../../../assets/icons/app/sard-128.png")).expect("logo");
        Bitmap { w: img.width as usize, h: img.height as usize, px: img.rgba }
    })
}

fn glyph(weight: Weight, ch: char) -> &'static Glyph {
    let table = font_atlas::GLYPHS[weight as usize];
    let find = |c: char| table.binary_search_by_key(&c, |g| g.ch).ok().map(|i| &table[i]);
    find(ch).or_else(|| find('?')).expect("'?' glyph")
}

/// Width of `text` set at `px` pixels per em.
pub fn text_width(text: &str, weight: Weight, px: f32) -> f32 {
    text.chars().map(|c| glyph(weight, c).advance).sum::<f32>() * px / font_atlas::SIZE
}

/// Break `text` into lines no wider than `max` (words longer than a line
/// are left to overflow), at most `max_lines` (the last one gets "…").
fn wrap(text: &str, weight: Weight, px: f32, max: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && text_width(&candidate, weight, px) > max {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.last_mut().unwrap();
        while !last.is_empty() && text_width(&format!("{last}…"), weight, px) > max {
            last.pop();
        }
        last.push('…');
    }
    lines
}

/// An RGB canvas (a whole screen, or the animated strip of one).
struct Canvas {
    w: usize,
    h: usize,
    /// Where (0, 0) of this canvas sits on the screen.
    ox: f32,
    oy: f32,
    rgb: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize, ox: f32, oy: f32) -> Self {
        let mut rgb = vec![0u8; w * h * 3];
        for p in rgb.chunks_mut(3) {
            p.copy_from_slice(&BG);
        }
        Canvas { w, h, ox, oy, rgb }
    }

    /// Blend `color` over screen pixel (x, y) with coverage `a` (0..1).
    fn blend(&mut self, x: i64, y: i64, color: Rgb, a: f32) {
        let (x, y) = (x - self.ox as i64, y - self.oy as i64);
        if a <= 0.0 || x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 3;
        let a = a.min(1.0);
        for (d, c) in self.rgb[i..i + 3].iter_mut().zip(color) {
            *d = (*d as f32 + (c as f32 - *d as f32) * a + 0.5) as u8;
        }
    }

    /// Draw `src`'s box (`sx`, `sy`, `sw`, `sh`) scaled into the screen
    /// rectangle at (`x`, `y`) of size `dw`×`dh`, averaging several samples
    /// per pixel when shrinking. `color`: tint a coverage bitmap (None: RGBA).
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        src: &Bitmap,
        (sx, sy, sw, sh): (f32, f32, f32, f32),
        x: f32,
        y: f32,
        dw: f32,
        dh: f32,
        color: Option<Rgb>,
    ) {
        let (kx, ky) = (sw / dw, sh / dh); // source pixels per screen pixel
        let taps = (kx.max(ky).ceil() as usize).clamp(1, 4);
        let (x0, y0) = ((x.floor() as i64).max(self.ox as i64), (y.floor() as i64).max(self.oy as i64));
        let x1 = ((x + dw).ceil() as i64).min(self.ox as i64 + self.w as i64);
        let y1 = ((y + dh).ceil() as i64).min(self.oy as i64 + self.h as i64);
        let channels = if color.is_some() { 1 } else { 4 };
        // Bilinear sample inside the box (zero outside it).
        let sample = |u: f32, v: f32, out: &mut [f32; 4]| {
            let (u, v) = (u - 0.5, v - 0.5);
            let (fu, fv) = (u.floor(), v.floor());
            let (tu, tv) = (u - fu, v - fv);
            for (dy, wy) in [(0, 1.0 - tv), (1, tv)] {
                for (dx, wx) in [(0, 1.0 - tu), (1, tu)] {
                    let (px, py) = (fu as i64 + dx, fv as i64 + dy);
                    if px < 0 || py < 0 || px >= sw as i64 || py >= sh as i64 {
                        continue;
                    }
                    let i = ((sy as usize + py as usize) * src.w + sx as usize + px as usize) * channels;
                    let wgt = wx * wy;
                    for (o, &b) in out.iter_mut().zip(&src.px[i..i + channels]) {
                        *o += b as f32 * wgt;
                    }
                }
            }
        };
        for py in y0..y1 {
            for px in x0..x1 {
                let mut acc = [0f32; 4];
                for ty in 0..taps {
                    for tx in 0..taps {
                        let u = (px as f32 + (tx as f32 + 0.5) / taps as f32 - x) * kx;
                        let v = (py as f32 + (ty as f32 + 0.5) / taps as f32 - y) * ky;
                        sample(u, v, &mut acc);
                    }
                }
                let n = (taps * taps) as f32 * 255.0;
                match color {
                    Some(c) => self.blend(px, py, c, acc[0] / n),
                    None => {
                        let a = acc[3] / n;
                        if a > 0.0 {
                            // Straight alpha: average color weighted by alpha.
                            let c = [0, 1, 2].map(|k| (acc[k] / n / a * 255.0).clamp(0.0, 255.0) as u8);
                            self.blend(px, py, c, a);
                        }
                    }
                }
            }
        }
    }

    /// Text with its baseline at `y`, starting at `x`.
    fn text(&mut self, text: &str, weight: Weight, px: f32, x: f32, y: f32, color: Rgb) {
        let k = px / font_atlas::SIZE;
        let atlas = atlas();
        let mut pen = x;
        for ch in text.chars() {
            let g = glyph(weight, ch);
            if g.w > 0 {
                let gx = pen + g.left as f32 * k;
                let gy = y - g.top as f32 * k;
                let b = (g.x as f32, g.y as f32, g.w as f32, g.h as f32);
                self.draw(atlas, b, gx, gy, g.w as f32 * k, g.h as f32 * k, Some(color));
            }
            pen += g.advance * k;
        }
    }

    fn text_centered(&mut self, text: &str, weight: Weight, px: f32, cx: f32, y: f32, color: Rgb) {
        let w = text_width(text, weight, px);
        self.text(text, weight, px, (cx - w / 2.0).round(), y, color);
    }

    /// A horizontal capsule from `x0` to `x1` (anti-aliased), clipped to
    /// [`clip0`, `clip1`].
    #[allow(clippy::too_many_arguments)]
    fn capsule(&mut self, x0: f32, x1: f32, y: f32, h: f32, clip0: f32, clip1: f32, color: Rgb) {
        let r = h / 2.0;
        let cy = y + r;
        for py in (y.floor() as i64)..((y + h).ceil() as i64) {
            for px in ((x0.max(clip0)).floor() as i64)..((x1.min(clip1)).ceil() as i64) {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                // Distance to the capsule's core segment, minus the radius.
                let nx = fx.clamp(x0 + r, (x1 - r).max(x0 + r));
                let d = ((fx - nx).powi(2) + (fy - cy).powi(2)).sqrt() - r;
                // Also inside the clip capsule (the track).
                let tx = fx.clamp(clip0 + r, (clip1 - r).max(clip0 + r));
                let dt = ((fx - tx).powi(2) + (fy - cy).powi(2)).sqrt() - r;
                let a = (0.5 - d.max(dt)).clamp(0.0, 1.0);
                self.blend(px, py, color, a);
            }
        }
    }
}

/// Layout of one screen at one size (screen pixels).
struct Layout {
    px: f32,
    cx: f32,
    logo: (f32, f32, f32),
    title_y: f32,
    detail: Vec<(String, f32)>,
    /// The animated strip: bar track and hint line.
    bar: (f32, f32, f32, f32),
    hint_y: f32,
    strip: (u32, u32, u32, u32),
}

fn layout(s: &Screen, w: u32, h: u32, scale: f32) -> Layout {
    let u = |v: f32| v * scale;
    let (wf, hf) = (w as f32, h as f32);
    // Shrink everything a little in small windows.
    let fit = (wf / u(560.0)).min(hf / u(380.0)).clamp(0.6, 1.0);
    let u = |v: f32| v * scale * fit;
    let cx = wf / 2.0;
    let detail_px = u(15.0);
    let max_w = (wf * 0.85).min(u(520.0));
    let detail = wrap(&s.detail, Weight::Regular, detail_px, max_w, 4);
    let logo_size = u(64.0);
    let title_px = u(23.0);
    let line = u(22.0);
    let total =
        logo_size + u(28.0) + title_px + u(12.0) + line * detail.len() as f32 + u(30.0) + u(4.0) + u(24.0) + u(13.0);
    let mut y = ((hf - total) / 2.0 - hf * 0.03).max(0.0);
    let logo = ((cx - logo_size / 2.0).round(), y.round(), logo_size.round());
    y += logo_size + u(28.0) + title_px * 0.78;
    let title_y = y.round();
    y += u(12.0);
    let detail: Vec<(String, f32)> = detail
        .into_iter()
        .map(|l| {
            y += line;
            (l, (y - line * 0.25).round())
        })
        .collect();
    y += u(30.0);
    let bar_w = u(240.0);
    let bar = ((cx - bar_w / 2.0).round(), y.round(), bar_w.round(), u(4.0).max(2.0).round());
    y += bar.3 + u(24.0) + u(13.0) * 0.78;
    let hint_y = y.round();
    // Strip: full width (the hint is centered and changes length), from just
    // above the bar to below the hint's descenders. Even-aligned for NV12.
    let top = (bar.1 - u(4.0)).max(0.0) as u32 & !1;
    let bottom = (((hint_y + u(8.0)).ceil() as u32).min(h) + 1) & !1;
    Layout {
        px: u(1.0),
        cx,
        logo,
        title_y,
        detail,
        bar,
        hint_y,
        strip: (0, top, w & !1, bottom.saturating_sub(top).max(2)),
    }
}

/// Renders [`Screen`]s, redrawing only the animated strip between frames.
#[derive(Default)]
pub struct Painter {
    /// Everything but the strip, for the screen it was drawn for.
    base: Option<(Screen, u32, u32, u32, Nv12Image)>,
    frame: Nv12Image,
}

impl Painter {
    /// The screen at `w`×`h` pixels (`scale`: pixels per point), `t`
    /// seconds after it first appeared (animation, elapsed time).
    pub fn render(&mut self, s: &Screen, w: u32, h: u32, scale: f32, t: f32) -> &Nv12Image {
        let (w, h) = ((w & !1).max(2), (h & !1).max(2));
        let l = layout(s, w, h, scale);
        let key = (s.title.as_str(), s.detail.as_str(), w, h, scale.to_bits());
        let fresh = !matches!(&self.base, Some((b, bw, bh, bs, _)) if (b.title.as_str(), b.detail.as_str(), *bw, *bh, *bs) == key);
        if fresh {
            let mut c = Canvas::new(w as usize, h as usize, 0.0, 0.0);
            let (lx, ly, ls) = l.logo;
            let lg = logo();
            c.draw(lg, (0.0, 0.0, lg.w as f32, lg.h as f32), lx, ly, ls, ls, None);
            c.text_centered(&s.title, Weight::SemiBold, l.px * 23.0, l.cx, l.title_y, TITLE);
            for (line, y) in &l.detail {
                c.text_centered(line, Weight::Regular, l.px * 15.0, l.cx, *y, DETAIL);
            }
            let mut img = Nv12Image::new(w, h);
            img.put_rgb(0, 0, w, h, &c.rgb);
            self.base = Some((s.clone(), w, h, scale.to_bits(), img));
        }
        let base = &self.base.as_ref().unwrap().4;
        if self.frame.width != w || self.frame.height != h {
            self.frame = base.clone();
        } else {
            self.frame.data.copy_from_slice(&base.data);
        }
        // The strip: bar and hint, redrawn every frame.
        let (sx, sy, sw, sh) = l.strip;
        let mut c = Canvas::new(sw as usize, sh as usize, sx as f32, sy as f32);
        let (bx, by, bw, bh) = l.bar;
        if s.busy {
            c.capsule(bx, bx + bw, by, bh, bx, bx + bw, TRACK);
            // An eased segment sweeping across, every 1.6 s.
            let p = (t / 1.6).fract();
            let e = if p < 0.5 { 2.0 * p * p } else { 1.0 - (-2.0 * p + 2.0).powi(2) / 2.0 };
            let seg = bw * 0.32;
            let x0 = bx - seg / 2.0 + e * bw; // always at least half on the track
            c.capsule(x0, x0 + seg, by, bh, bx, bx + bw, ACCENT);
        }
        let hint = match (s.busy, t as u32) {
            (true, secs) if secs >= 3 && !s.hint.is_empty() => format!("{} · {secs} s", s.hint),
            (true, secs) if secs >= 3 => format!("{secs} s"),
            _ => s.hint.clone(),
        };
        c.text_centered(&hint, Weight::Regular, l.px * 13.0, l.cx, l.hint_y, HINT);
        self.frame.put_rgb(sx, sy, sw, sh, &c.rgb);
        &self.frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_and_wrapping() {
        assert!(text_width("Connecting", Weight::SemiBold, 20.0) > text_width("Connecting", Weight::Regular, 20.0));
        assert_eq!(glyph(Weight::Regular, '☃').ch, '?');
        let lines = wrap(
            "one two three four five six",
            Weight::Regular,
            20.0,
            text_width("one two three", Weight::Regular, 20.0),
            9,
        );
        assert_eq!(lines, ["one two three", "four five six"]);
        let cut = wrap(
            "one two three four five six",
            Weight::Regular,
            20.0,
            text_width("one two", Weight::Regular, 20.0) + 1.0,
            2,
        );
        assert_eq!(cut.len(), 2);
        assert!(cut[1].ends_with('…'));
    }

    /// Renders the screens to PNGs for a look: `SARD_STATUS_PNG=dir cargo test -p sard status`.
    #[test]
    fn renders() {
        let mut p = Painter::default();
        let screens = [
            (
                "connecting",
                Screen {
                    title: "Connecting to austin-linux".into(),
                    detail: "Looking on this network…".into(),
                    hint: "Close this window to cancel".into(),
                    busy: true,
                },
            ),
            (
                "failed",
                Screen {
                    title: "Couldn't connect to austin-linux".into(),
                    detail: "austin-linux didn't answer on this network or through Connect Anywhere. Is Sard running there, and is the computer awake?".into(),
                    hint: "Press Return to try again · Esc to close".into(),
                    busy: false,
                },
            ),
        ];
        for (name, s) in &screens {
            for (w, h, scale) in [(2560, 1440, 2.0), (1280, 720, 1.0), (640, 360, 1.0)] {
                let img = p.render(s, w, h, scale, 4.3).clone();
                assert_eq!((img.width, img.height), (w, h));
                // Something was drawn, and the background is the background.
                assert_ne!(img.luma().iter().max(), img.luma().iter().min());
                if let Some(dir) = std::env::var_os("SARD_STATUS_PNG") {
                    let rgb = nv12_to_rgb(&img);
                    let path = std::path::Path::new(&dir).join(format!("{name}-{w}x{h}.png"));
                    sard_core::png::write_rgb(&path, w, h, &rgb).unwrap();
                }
            }
        }
    }

    fn nv12_to_rgb(img: &Nv12Image) -> Vec<u8> {
        let (w, h) = (img.width as usize, img.height as usize);
        let mut out = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let l = (img.luma()[y * w + x] as f32 - 16.0) * 255.0 / 219.0;
                let c = &img.chroma()[y / 2 * w + x / 2 * 2..];
                let (cb, cr) = ((c[0] as f32 - 128.0) * 255.0 / 224.0, (c[1] as f32 - 128.0) * 255.0 / 224.0);
                let rgb = [l + 1.5748 * cr, l - 0.1873 * cb - 0.4681 * cr, l + 1.8556 * cb];
                for (o, v) in out[(y * w + x) * 3..][..3].iter_mut().zip(rgb) {
                    *o = v.clamp(0.0, 255.0) as u8;
                }
            }
        }
        out
    }
}
