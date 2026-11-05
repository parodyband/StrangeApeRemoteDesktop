//! GPU round trip: synthetic NV12 -> NVENC -> NVDEC -> compare. Skips (passes)
//! when no NVIDIA GPU/driver is available.

use sard_core::proto::Codec;
use sard_linux::nv::cuda::{Cuda, Mem};
use sard_linux::nv::nvdec::NvDecoder;
use sard_linux::nv::nvenc::{EncoderParams, NvEncoder};
use sard_linux::yuv::{psnr, test_pattern_nv12};
use std::time::Instant;

fn roundtrip(codec: Codec, w: u32, h: u32) {
    let cuda = match Cuda::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let preset = std::env::var("SARD_PRESET").ok().and_then(|p| p.parse().ok()).unwrap_or(1);
    let params = EncoderParams { codec, width: w, height: h, fps: 60, bitrate_kbps: 40_000, preset, ..Default::default() };
    let mut enc = NvEncoder::new(cuda.clone(), params).expect("encoder");
    let mut dec = NvDecoder::new(cuda.clone(), codec).expect("decoder");
    let (wu, hu) = (w as usize, h as usize);
    let mut src = vec![0u8; wu * hu * 3 / 2];
    let mut got = vec![0u8; wu * hu];
    let mut enc_us = Vec::new();
    let mut dec_us = Vec::new();
    let mut total_bytes = 0usize;
    let mut worst = 99.0f64;
    let mut psnrs = Vec::new();
    for t in 0..90u32 {
        test_pattern_nv12(wu, hu, wu, t, &mut src);
        let (idx, dptr, pitch) = enc.next_input();
        {
            let _g = cuda.push();
            cuda.copy_2d(Mem::Host(src.as_ptr()), wu, Mem::Device(dptr), pitch, wu, hu * 3 / 2, std::ptr::null_mut()).unwrap();
        }
        let mut bits = Vec::new();
        let t0 = Instant::now();
        let key = enc.encode(idx, t as u64, t == 45, &mut bits).expect("encode");
        enc_us.push(t0.elapsed().as_micros());
        assert_eq!(key, t == 0 || t == 45, "frame {t} keyframe flag");
        total_bytes += bits.len();
        let t1 = Instant::now();
        let mut frames = 0;
        dec.decode(&bits, t as i64, |f| {
            frames += 1;
            assert_eq!((f.width, f.height), (w, h));
            assert_eq!(f.timestamp, t as i64);
            let _g = cuda.push();
            cuda.copy_2d(Mem::Device(f.dptr), f.pitch, Mem::HostMut(got.as_mut_ptr()), wu, wu, hu, std::ptr::null_mut()).unwrap();
        })
        .expect("decode");
        dec_us.push(t1.elapsed().as_micros());
        assert_eq!(frames, 1, "frame {t}: decoder must output immediately (zero delay)");
        let p = psnr(&src, &got, wu, hu, wu, wu);
        psnrs.push(p);
        if std::env::var("SARD_PSNR_TRACE").is_ok() {
            eprintln!("  frame {t:2} {:6} bytes psnr {p:.1}{}", bits.len(), if key { " KEY" } else { "" });
        }
        worst = worst.min(p);
    }
    enc_us.sort();
    dec_us.sort();
    eprintln!(
        "{} {w}x{h}: encode p50 {}us p95 {}us | decode p50 {}us p95 {}us | {:.1} KB/frame | worst Y PSNR {:.1} dB",
        codec.name(),
        enc_us[enc_us.len() / 2],
        enc_us[enc_us.len() * 95 / 100],
        dec_us[dec_us.len() / 2],
        dec_us[dec_us.len() * 95 / 100],
        total_bytes as f64 / 90.0 / 1024.0,
        worst
    );
    let mean = psnrs.iter().sum::<f64>() / psnrs.len() as f64;
    eprintln!("    mean Y PSNR {mean:.1} dB");
    assert!(mean > 33.0, "mean quality too low: {mean} dB");
    assert!(worst > 24.0, "worst frame quality too low: {worst} dB");
}

#[test]
fn hevc_1440p() {
    roundtrip(Codec::Hevc, 2560, 1440);
}

#[test]
fn h264_1080p() {
    roundtrip(Codec::H264, 1920, 1080);
}

#[test]
fn av1_1440p() {
    roundtrip(Codec::Av1, 2560, 1440);
}
