//! Synthetic video source: an animated pattern drawn on the CPU, uploaded as
//! BGRA and sent through the same GPU colour conversion and encoder as screen
//! capture. Tests the pipeline without a desktop (CI, remote sessions).

use crate::d3d::{self, Device};
use crate::encode::{Encoder, Settings};
use crate::gpu::{Nv12Target, Passes};
use sard_core::clock;
use sard_core::media::{self, DisplayInfo, EncodedFrame, SourceOptions, VideoSource};
use sard_core::proto::Codec;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct TestPatternSource {
    dev: Device,
    passes: Passes,
    vendor: u32,
    gpu_name: String,
    enc: Encoder,
    tex: d3d::ID3D11Texture2D,
    srv: d3d::ID3D11ShaderResourceView,
    targets: Vec<Nv12Target>,
    next: usize,
    pixels: Vec<u8>,
    w: usize,
    h: usize,
    interval: Duration,
    next_due: Instant,
    t: u32,
    force_key: bool,
    queue: VecDeque<EncodedFrame>,
    /// Last pointer position reported by the input sink (drawn as a marker).
    pub pointer: Arc<Mutex<(f32, f32)>>,
    displays: Vec<DisplayInfo>,
    current: u32,
    opts: SourceOptions,
}

// SAFETY: created on the host thread, then used only by the video thread.
unsafe impl Send for TestPatternSource {}

impl TestPatternSource {
    pub fn new(opts: &SourceOptions) -> media::Result<Self> {
        let f = d3d::factory()?;
        let list = d3d::adapters(&f);
        let a = d3d::preferred_adapter(&list).ok_or("no GPU (DXGI found no hardware adapter)")?;
        let dev = Device::new(Some(&a.adapter))?;
        let passes = Passes::new(&dev)?;
        let (w, h) = (opts.test_size.0.max(64) & !1, opts.test_size.1.max(64) & !1);
        let displays = vec![
            DisplayInfo { id: 1, name: "Test Screen A".into(), width: w, height: h, x: 0, y: 0 },
            DisplayInfo { id: 2, name: "Test Screen B".into(), width: (w * 3 / 4) & !1, height: h, x: w as i32, y: 0 },
        ];
        let current = if opts.display == 2 { 2 } else { 1 };
        let d = &displays[current as usize - 1];
        let (w, h) = (d.width, d.height);
        let (enc, tex, srv, targets) = Self::pipeline(&dev, a.vendor, opts, w, h)?;
        Ok(TestPatternSource {
            dev,
            passes,
            vendor: a.vendor,
            gpu_name: a.name.clone(),
            enc,
            tex,
            srv,
            targets,
            next: 0,
            pixels: vec![0; w as usize * h as usize * 4],
            w: w as usize,
            h: h as usize,
            interval: Duration::from_secs_f64(1.0 / opts.fps.max(1) as f64),
            next_due: Instant::now(),
            t: 0,
            force_key: true,
            queue: VecDeque::new(),
            pointer: Arc::new(Mutex::new((0.5, 0.5))),
            displays,
            current,
            opts: opts.clone(),
        })
    }

    #[allow(clippy::type_complexity)]
    fn pipeline(
        dev: &Device,
        vendor: u32,
        opts: &SourceOptions,
        w: u32,
        h: u32,
    ) -> media::Result<(Encoder, d3d::ID3D11Texture2D, d3d::ID3D11ShaderResourceView, Vec<Nv12Target>)> {
        let s = Settings {
            codec: opts.codec,
            width: w,
            height: h,
            fps: opts.fps.max(1),
            bitrate_kbps: opts.bitrate_kbps.max(1000),
            preset: opts.preset,
        };
        let enc = Encoder::new(dev, vendor, s)?;
        let tex = dev.texture(
            &d3d::D3D11_TEXTURE2D_DESC::new(
                w,
                h,
                d3d::DXGI_FORMAT_B8G8R8A8_UNORM,
                d3d::D3D11_USAGE_DEFAULT,
                d3d::D3D11_BIND_SHADER_RESOURCE,
                0,
            ),
            None,
        )?;
        let srv = dev.srv(&tex, d3d::DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let targets = (0..3).map(|_| Nv12Target::new(dev, w, h)).collect::<Result<Vec<_>, _>>()?;
        Ok((enc, tex, srv, targets))
    }

    fn fill(&mut self, x0: usize, y0: usize, rw: usize, rh: usize, rgb: (u8, u8, u8)) {
        let (x1, y1) = ((x0 + rw).min(self.w), (y0 + rh).min(self.h));
        for y in y0..y1 {
            for x in x0..x1 {
                self.pixels[(y * self.w + x) * 4..][..4].copy_from_slice(&[rgb.2, rgb.1, rgb.0, 255]);
            }
        }
    }

    fn render(&mut self) {
        let (w, h) = (self.w, self.h);
        // Background: colour bars over a grey ramp, with a scrolling "text" band.
        let bars =
            [(192, 192, 192), (192, 192, 0), (0, 192, 192), (0, 192, 0), (192, 0, 192), (192, 0, 0), (0, 0, 192)];
        for y in 0..h {
            for x in 0..w {
                let c = if y < h * 3 / 4 {
                    bars[x * bars.len() / w]
                } else {
                    let xs = (x + self.t as usize * 2) % w;
                    let on = (xs / 6 + y / 6).is_multiple_of(3);
                    if on { (230, 230, 230) } else { (20, 20, 20) }
                };
                self.pixels[(y * w + x) * 4..][..4].copy_from_slice(&[c.2, c.1, c.0, 255]);
            }
        }
        let bx = (self.t as usize * 8) % w.saturating_sub(32).max(1);
        self.fill(bx, 0, 32, h * 3 / 4, (255, 60, 30));
        for bit in 0..16 {
            let on = self.t >> (15 - bit) & 1 != 0;
            self.fill(16 + bit * 24, 16, 20, 20, if on { (255, 255, 255) } else { (0, 0, 0) });
        }
        let (px, py) = *self.pointer.lock().unwrap();
        let (cx, cy) = ((px * w as f32) as usize, (py * h as f32) as usize);
        self.fill(cx.saturating_sub(6), cy.saturating_sub(6), 12, 12, (0, 255, 120));
    }
}

impl VideoSource for TestPatternSource {
    fn next_frame(&mut self, timeout: Duration) -> media::Result<Option<EncodedFrame>> {
        if let Some(f) = self.queue.pop_front() {
            return Ok(Some(f));
        }
        let now = Instant::now();
        if self.next_due > now {
            let wait = self.next_due - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next_due = if Instant::now() > self.next_due + self.interval {
            Instant::now() + self.interval
        } else {
            self.next_due + self.interval
        };
        self.render();
        let capture_us = clock::now_us();
        // SAFETY: the buffer covers the whole texture at the given pitch.
        unsafe {
            self.dev.context.UpdateSubresource(
                crate::com::Interface::as_raw(&self.tex),
                0,
                std::ptr::null(),
                self.pixels.as_ptr().cast(),
                (self.w * 4) as u32,
                0,
            )
        };
        let i = self.next;
        self.next = (self.next + 1) % self.targets.len();
        self.passes.rgb_to_nv12(&self.dev, &self.srv, &self.targets[i]);
        let key = std::mem::take(&mut self.force_key);
        for p in self.enc.encode(&self.targets[i].texture, capture_us, key)? {
            self.queue.push_back(EncodedFrame {
                data: p.data,
                keyframe: p.keyframe,
                capture_us: if p.timestamp_us != 0 { p.timestamp_us } else { capture_us },
                width: self.w as u32,
                height: self.h as u32,
            });
        }
        self.t = self.t.wrapping_add(1);
        Ok(self.queue.pop_front())
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.enc.set_bitrate(kbps);
    }

    fn codec(&self) -> Codec {
        self.opts.codec
    }

    fn size(&self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }

    fn describe(&self) -> String {
        format!("test pattern {}x{} {} {} on {}", self.w, self.h, self.enc.name, self.opts.codec.name(), self.gpu_name)
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        self.displays.clone()
    }

    fn current_display(&self) -> Option<u32> {
        Some(self.current)
    }

    fn select_display(&mut self, id: u32) -> media::Result<()> {
        let d =
            self.displays.iter().find(|d| d.id == id).ok_or_else(|| media::err(format!("no display {id}")))?.clone();
        if id != self.current {
            let (enc, tex, srv, targets) = Self::pipeline(&self.dev, self.vendor, &self.opts, d.width, d.height)?;
            (self.enc, self.tex, self.srv, self.targets) = (enc, tex, srv, targets);
            self.next = 0;
            self.w = d.width as usize;
            self.h = d.height as usize;
            self.pixels = vec![0; self.w * self.h * 4];
            self.current = id;
            self.queue.clear();
        }
        self.force_key = true;
        Ok(())
    }
}
