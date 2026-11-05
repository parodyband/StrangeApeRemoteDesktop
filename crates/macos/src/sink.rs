//! Client-side video output on macOS: VideoToolbox decode → IOSurface
//! `CVPixelBuffer` → Metal textures (zero copy) → `CAMetalLayer`.

use crate::metal::Presenter;
use crate::vt::VtDecoder;
use objc2::runtime::AnyObject;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, PresentStats, VideoSink};
use sard_core::proto::Codec;
use std::time::Instant;

pub struct MetalVideoSink {
    presenter: Presenter,
    decoder: Option<VtDecoder>,
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
        Ok(MetalVideoSink { presenter, decoder: None })
    }
}

impl VideoSink for MetalVideoSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        if self.decoder.as_ref().is_none_or(|d| d.codec() != codec) {
            self.decoder = Some(VtDecoder::new(codec).map_err(media::err)?);
        }
        let t0 = Instant::now();
        let image = self.decoder.as_mut().unwrap().decode(data).map_err(media::err)?;
        let decode_us = t0.elapsed().as_micros() as u32;
        let Some(image) = image else {
            return Ok(PresentStats { decode_us, present_us: 0 });
        };
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

    fn describe(&self) -> String {
        let hw = match &self.decoder {
            Some(d) if d.hardware => " (hardware)",
            _ => "",
        };
        format!("VideoToolbox{hw} → Metal on {}", self.presenter.renderer.device_name)
    }
}
