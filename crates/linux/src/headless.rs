//! Windowless client sink: NVDEC decode only (for tests and benchmarks that
//! must not pop up windows). Can save decoded frames as PNG.

use crate::nv::cuda::{Cuda, Mem};
use crate::nv::nvdec::NvDecoder;
use sard_core::media::{self, PresentStats, VideoSink};
use sard_core::proto::Codec;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct HeadlessSink {
    cuda: Arc<Cuda>,
    decoder: Option<(Codec, NvDecoder)>,
    snapshot: Option<PathBuf>,
}

impl HeadlessSink {
    pub fn new() -> media::Result<Self> {
        Ok(Self { cuda: Cuda::new()?, decoder: None, snapshot: None })
    }
}

impl VideoSink for HeadlessSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        if self.decoder.as_ref().is_none_or(|(c, _)| *c != codec) {
            self.decoder = Some((codec, NvDecoder::new(Arc::clone(&self.cuda), codec)?));
        }
        let t0 = Instant::now();
        let cuda = Arc::clone(&self.cuda);
        let snap = &mut self.snapshot;
        let mut err = None;
        let (_, dec) = self.decoder.as_mut().unwrap();
        dec.decode(data, 0, |f| {
            let Some(path) = snap.take() else { return };
            let (w, h) = (f.width as usize, f.height as usize);
            let mut y = vec![0u8; w * h];
            let mut uv = vec![0u8; w * h / 2];
            let plane = f.pitch as u64 * ((f.surface_height as u64 + 1) & !1);
            let r = cuda
                .copy_2d(Mem::Device(f.dptr), f.pitch, Mem::HostMut(y.as_mut_ptr()), w, w, h, std::ptr::null_mut())
                .and_then(|_| {
                    cuda.copy_2d(
                        Mem::Device(f.dptr + plane),
                        f.pitch,
                        Mem::HostMut(uv.as_mut_ptr()),
                        w,
                        w,
                        h / 2,
                        std::ptr::null_mut(),
                    )
                });
            if let Err(e) = r {
                err = Some(e);
                return;
            }
            let mut rgb = vec![0u8; w * h * 3];
            crate::yuv::nv12_to_rgb(&y, &uv, w, h, w, &mut rgb);
            match sard_core::png::write_rgb(&path, w as u32, h as u32, &rgb) {
                Ok(()) => sard_core::info!("saved snapshot {}", path.display()),
                Err(e) => err = Some(format!("snapshot {}: {e}", path.display())),
            }
        })?;
        if let Some(e) = err {
            return Err(media::err(e));
        }
        Ok(PresentStats { decode_us: t0.elapsed().as_micros() as u32, present_us: 0 })
    }

    fn resize(&mut self, _width: u32, _height: u32) {}

    fn snapshot_next(&mut self, path: &std::path::Path) -> media::Result<()> {
        self.snapshot = Some(path.to_path_buf());
        Ok(())
    }

    fn describe(&self) -> String {
        format!("NVDEC on {} (headless)", self.cuda.device_name)
    }
}
