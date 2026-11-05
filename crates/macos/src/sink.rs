//! Client-side video output on macOS: VideoToolbox decode → IOSurface
//! `CVPixelBuffer` → Metal textures (zero copy) → `CAMetalLayer`.
//! Also a windowless variant (decode only) for tests.

use crate::cf::CfRef;
use crate::metal::Presenter;
use crate::vt::{self, VtDecoder};
use objc2::runtime::AnyObject;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, PresentStats, VideoSink};
use sard_core::proto::Codec;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Decode with a (re)created decoder for `codec`; returns the picture, if any.
fn decode(decoder: &mut Option<VtDecoder>, codec: Codec, data: &[u8]) -> media::Result<Option<CfRef>> {
    if decoder.as_ref().is_none_or(|d| d.codec() != codec) {
        *decoder = Some(VtDecoder::new(codec).map_err(media::err)?);
    }
    decoder.as_mut().unwrap().decode(data).map_err(media::err)
}

/// Save a decoded NV12 buffer as PNG (CPU conversion; diagnostics only).
fn save_snapshot(image: &CfRef, path: &Path) -> media::Result<()> {
    let (w, h) = vt::dimensions(image);
    let (y, y_pitch, _) = vt::read_plane(image, 0).map_err(media::err)?;
    let (uv, uv_pitch, _) = vt::read_plane(image, 1).map_err(media::err)?;
    let mut rgb = vec![0u8; w as usize * h as usize * 3];
    crate::pattern::nv12_to_rgb(&y, &uv, w as usize, h as usize, y_pitch, uv_pitch, &mut rgb);
    sard_core::png::write_rgb(path, w, h, &rgb).map_err(|e| media::err(format!("snapshot {}: {e}", path.display())))?;
    sard_core::info!("saved snapshot {}", path.display());
    Ok(())
}

fn hw_note(decoder: &Option<VtDecoder>) -> &'static str {
    match decoder {
        Some(d) if d.hardware => " (hardware)",
        _ => "",
    }
}

pub struct MetalVideoSink {
    presenter: Presenter,
    decoder: Option<VtDecoder>,
    snapshot: Option<PathBuf>,
}

// SAFETY: created on the main thread, then moved to and used only by the
// render thread. Metal devices/queues/layers' drawable APIs are thread-safe.
unsafe impl Send for MetalVideoSink {}

impl MetalVideoSink {
    /// Must be called on the main thread (attaches a CAMetalLayer to the view).
    pub fn new(_display: RawDisplayHandle, window: RawWindowHandle, width: u32, height: u32) -> media::Result<Self> {
        let RawWindowHandle::AppKit(h) = window else {
            return Err(media::err(format!("unsupported window handle: {window:?}")));
        };
        let view = h.ns_view.as_ptr() as *mut AnyObject;
        // SAFETY: called on the main thread by the client (see platform docs);
        // `view` is the live NSView of the winit window.
        let presenter = unsafe { Presenter::new(view, width, height) }.map_err(media::err)?;
        Ok(MetalVideoSink { presenter, decoder: None, snapshot: None })
    }
}

impl VideoSink for MetalVideoSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        let t0 = Instant::now();
        let image = decode(&mut self.decoder, codec, data)?;
        let decode_us = t0.elapsed().as_micros() as u32;
        let Some(image) = image else {
            return Ok(PresentStats { decode_us, present_us: 0 });
        };
        if let Some(p) = self.snapshot.take() {
            save_snapshot(&image, &p)?;
        }
        let t1 = Instant::now();
        self.presenter.set_frame(&image).map_err(media::err)?;
        self.presenter.draw().map_err(media::err)?;
        Ok(PresentStats { decode_us, present_us: t1.elapsed().as_micros() as u32 })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.presenter.resize(width, height);
    }

    fn redraw(&mut self) -> media::Result<()> {
        if self.presenter.has_frame() {
            self.presenter.draw().map_err(media::err)?;
        }
        Ok(())
    }

    fn snapshot_next(&mut self, path: &Path) -> media::Result<()> {
        self.snapshot = Some(path.to_path_buf());
        Ok(())
    }

    fn describe(&self) -> String {
        format!("VideoToolbox{} → Metal on {}", hw_note(&self.decoder), self.presenter.renderer.device_name)
    }
}

/// Windowless client sink: VideoToolbox decode only (tests, benchmarks).
#[derive(Default)]
pub struct HeadlessSink {
    decoder: Option<VtDecoder>,
    snapshot: Option<PathBuf>,
}

impl VideoSink for HeadlessSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        let t0 = Instant::now();
        let image = decode(&mut self.decoder, codec, data)?;
        let decode_us = t0.elapsed().as_micros() as u32;
        if let (Some(image), Some(p)) = (&image, &self.snapshot) {
            let p = p.clone();
            self.snapshot = None;
            save_snapshot(image, &p)?;
        }
        Ok(PresentStats { decode_us, present_us: 0 })
    }

    fn resize(&mut self, _width: u32, _height: u32) {}

    fn snapshot_next(&mut self, path: &Path) -> media::Result<()> {
        self.snapshot = Some(path.to_path_buf());
        Ok(())
    }

    fn describe(&self) -> String {
        format!("VideoToolbox{} (headless)", hw_note(&self.decoder))
    }
}
