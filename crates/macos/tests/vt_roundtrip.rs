//! Hardware round trips that need no macOS permissions:
//! synthetic NV12 → VideoToolbox encode (Annex-B) → VideoToolbox decode →
//! PSNR, and decode → CVMetalTextureCache → Metal YUV→RGB → readback.

#![cfg(target_os = "macos")]

use sard_core::proto::Codec;
use sard_macos::annexb::{self, NalCodec};
use sard_macos::metal::Renderer;
use sard_macos::pattern::{Pattern, psnr};
use sard_macos::vt::{self, EncoderConfig, RateControl, VtDecoder, VtEncoder};
use std::time::Instant;

fn pct(v: &mut [u128], p: usize) -> u128 {
    v.sort();
    v[(v.len() * p / 100).min(v.len() - 1)]
}

fn roundtrip(codec: Codec, w: usize, h: usize, rate_control: RateControl) {
    sard_core::log::init();
    let cfg = EncoderConfig { codec, width: w as u32, height: h as u32, fps: 60, bitrate_kbps: 40_000, rate_control };
    let mut enc = VtEncoder::new(cfg).expect("encoder");
    let mut dec = VtDecoder::new(codec).expect("decoder");
    let nal = if codec == Codec::H264 { NalCodec::H264 } else { NalCodec::Hevc };
    let mut pat = Pattern::new(w, h);
    let (mut enc_us, mut dec_us) = (Vec::new(), Vec::new());
    let (mut worst_y, mut worst_c, mut bytes) = (99f64, 99f64, 0usize);
    let (mut max_p, mut max_key) = (0usize, 0usize);
    let base = sard_core::clock::now_us();
    for t in 0..90u32 {
        pat.render(t, (0.3, 0.6));
        let pb = enc.pixel_buffer().expect("pixel buffer");
        vt::write_nv12(&pb, &pat.frame, w, h).unwrap();
        vt::tag_bt709(&pb);
        let force = t == 0 || t == 45;
        let t0 = Instant::now();
        let out = enc.encode(&pb, base + t as u64 * 16_667, force).expect("encode").expect("frame dropped");
        enc_us.push(t0.elapsed().as_micros());
        bytes += out.data.len();
        if force {
            max_key = max_key.max(out.data.len());
        } else if t > 2 {
            max_p = max_p.max(out.data.len());
        }
        assert_eq!(out.keyframe, force, "frame {t}: keyframe flag");
        assert!(out.data.starts_with(&[0, 0, 0, 1]), "Annex-B start code");
        let types: Vec<u8> = annexb::nal_units(&out.data).map(|n| nal.nal_type(n)).collect();
        let has_params = types.iter().any(|&t| nal.param_rank(t).is_some());
        assert_eq!(has_params, force, "frame {t}: parameter sets only on keyframes ({types:?})");
        if force {
            let ranks: Vec<u8> = types.iter().filter_map(|&t| nal.param_rank(t)).collect();
            let need: &[u8] = if codec == Codec::H264 { &[1, 2] } else { &[0, 1, 2] };
            assert!(need.iter().all(|r| ranks.contains(r)), "missing parameter sets: {types:?}");
            assert!(types.iter().any(|&t| nal.is_keyframe(t)), "IDR slice missing: {types:?}");
        }

        let t1 = Instant::now();
        let img = dec.decode(&out.data).expect("decode").expect("decoder must output every frame immediately");
        dec_us.push(t1.elapsed().as_micros());
        assert_eq!(vt::dimensions(&img), (w as u32, h as u32));
        let (y, yw, _) = vt::read_plane(&img, 0).unwrap();
        let (c, cw, ch) = vt::read_plane(&img, 1).unwrap();
        let py = psnr(&pat.frame, &y, w, h, w, yw);
        let pc = psnr(&pat.frame[w * h..], &c, w, ch, w, cw);
        if std::env::var("SARD_PSNR_TRACE").is_ok() {
            eprintln!(
                "  frame {t:2} {:7} bytes  Y {py:.1} dB  C {pc:.1} dB{}",
                out.data.len(),
                if force { " KEY" } else { "" }
            );
        }
        worst_y = worst_y.min(py);
        worst_c = worst_c.min(pc);
    }
    eprintln!(
        "{} {w}x{h} ({}{}): encode p50 {}us p95 {}us | decode p50 {}us p95 {}us | {:.1} KB/frame (max P {:.0} KB, key {:.0} KB) | worst PSNR Y {:.1} dB, CbCr {:.1} dB",
        codec.name(),
        if enc.hardware { "hw" } else { "sw" },
        if enc.low_latency { ", low-latency RC" } else { "" },
        pct(&mut enc_us, 50),
        pct(&mut enc_us, 95),
        pct(&mut dec_us, 50),
        pct(&mut dec_us, 95),
        bytes as f64 / 90.0 / 1024.0,
        max_p as f64 / 1024.0,
        max_key as f64 / 1024.0,
        worst_y,
        worst_c,
    );
    assert!(worst_y > 32.0, "luma PSNR too low: {worst_y:.1} dB");
    assert!(worst_c > 32.0, "chroma PSNR too low: {worst_c:.1} dB");
}

#[test]
fn hevc_1080p_speed() {
    roundtrip(Codec::Hevc, 1920, 1080, RateControl::Speed);
}

#[test]
fn hevc_1080p_low_latency() {
    roundtrip(Codec::Hevc, 1920, 1080, RateControl::LowLatency);
}

#[test]
fn h264_1080p() {
    roundtrip(Codec::H264, 1920, 1080, RateControl::LowLatency);
}

#[test]
fn hevc_1440p() {
    roundtrip(Codec::Hevc, 2560, 1440, RateControl::for_preset(Codec::Hevc, 1));
}

/// The decoder must rebuild itself when the stream's parameter sets change
/// (e.g. host resolution change) and keep working.
#[test]
fn decoder_follows_resolution_change() {
    let mut dec = VtDecoder::new(Codec::Hevc).unwrap();
    for (w, h) in [(640usize, 360usize), (1280, 720), (640, 360)] {
        let mut enc = VtEncoder::new(EncoderConfig {
            codec: Codec::Hevc,
            width: w as u32,
            height: h as u32,
            fps: 60,
            bitrate_kbps: 8000,
            rate_control: RateControl::Speed,
        })
        .unwrap();
        let mut pat = Pattern::new(w, h);
        for t in 0..5u32 {
            pat.render(t, (0.5, 0.5));
            let pb = enc.pixel_buffer().unwrap();
            vt::write_nv12(&pb, &pat.frame, w, h).unwrap();
            let out = enc.encode(&pb, sard_core::clock::now_us(), t == 0).unwrap().unwrap();
            let img = dec.decode(&out.data).unwrap().unwrap();
            assert_eq!(vt::dimensions(&img), (w as u32, h as u32));
        }
    }
    // A P-frame for a fresh decoder is an error (client then requests a keyframe).
    let mut fresh = VtDecoder::new(Codec::Hevc).unwrap();
    let mut p = Vec::new();
    annexb::push_annexb(&mut p, &[0x02, 0x01, 0xd0, 0x40]);
    assert!(fresh.decode(&p).is_err());
}

/// BT.709 limited-range reference conversion (CPU).
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [f32; 3] {
    let y = (y as f32 - 16.0) / 219.0;
    let (cb, cr) = ((u as f32 - 128.0) / 224.0, (v as f32 - 128.0) / 224.0);
    [y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb].map(|c| c.clamp(0.0, 1.0) * 255.0)
}

/// Decoded IOSurface → Metal textures (zero copy) → shader → readback, with
/// letterboxing into a square target.
#[test]
fn metal_yuv_to_rgb_and_letterbox() {
    let (w, h) = (1280usize, 720usize);
    let mut enc = VtEncoder::new(EncoderConfig {
        codec: Codec::Hevc,
        width: w as u32,
        height: h as u32,
        fps: 60,
        bitrate_kbps: 30_000,
        rate_control: RateControl::Speed,
    })
    .unwrap();
    let mut dec = VtDecoder::new(Codec::Hevc).unwrap();
    let mut pat = Pattern::new(w, h);
    pat.render(10, (0.25, 0.25));
    let pb = enc.pixel_buffer().unwrap();
    vt::write_nv12(&pb, &pat.frame, w, h).unwrap();
    let out = enc.encode(&pb, sard_core::clock::now_us(), true).unwrap().unwrap();
    let img = dec.decode(&out.data).unwrap().unwrap();

    objc2::rc::autoreleasepool(|_| {
        let r = Renderer::new().expect("metal");
        let tex = r.textures(&img).expect("CVMetalTexture");
        // 1:1 render: compare against the CPU conversion of the source.
        let rgba = r.render_offscreen(&tex, w as u32, h as u32).expect("render");
        let (mut err, mut n) = (0f64, 0usize);
        for yy in (8..h - 8).step_by(7) {
            for xx in (8..w - 8).step_by(11) {
                let yv = pat.frame[yy * w + xx];
                let ci = w * h + (yy / 2) * w + (xx / 2) * 2;
                let want = yuv_to_rgb(yv, pat.frame[ci], pat.frame[ci + 1]);
                let p = &rgba[(yy * w + xx) * 4..][..4]; // BGRA
                let got = [p[2] as f32, p[1] as f32, p[0] as f32];
                err += want.iter().zip(got).map(|(a, b)| (a - b).abs() as f64).sum::<f64>() / 3.0;
                n += 1;
                assert_eq!(p[3], 255);
            }
        }
        let mae = err / n as f64;
        eprintln!("metal: {} | mean abs RGB error vs CPU reference {mae:.2}", r.device_name);
        assert!(mae < 6.0, "colour conversion error too high: {mae:.2}");

        // 16:9 into a square: black bars top and bottom.
        let s = 512usize;
        let sq = r.render_offscreen(&tex, s as u32, s as u32).unwrap();
        let bar = (s - s * 9 / 16) / 2;
        for yy in [0, bar / 2, bar - 3, s - bar + 3, s - 1] {
            for xx in [0, s / 2, s - 1] {
                let p = &sq[(yy * s + xx) * 4..][..3];
                assert!(p.iter().all(|&c| c == 0), "expected black at ({xx},{yy}), got {p:?}");
            }
        }
        let mid = &sq[(s / 2 * s + s / 2) * 4..][..3];
        assert!(mid.iter().any(|&c| c > 20), "expected picture in the middle, got {mid:?}");
    });
}

/// `SARD_BENCH="hevc 3840x2160 [speed|lowlatency]" cargo test --test vt_roundtrip bench -- --ignored --nocapture`
#[test]
#[ignore]
fn bench() {
    let spec = std::env::var("SARD_BENCH").unwrap_or_else(|_| "hevc 1920x1080".into());
    let mut it = spec.split_whitespace();
    let codec = Codec::parse(it.next().unwrap()).unwrap();
    let (w, h) = it.next().unwrap().split_once('x').unwrap();
    let rc = match it.next() {
        Some("speed") => RateControl::Speed,
        Some(_) => RateControl::LowLatency,
        None => RateControl::for_preset(codec, 1),
    };
    roundtrip(codec, w.parse().unwrap(), h.parse().unwrap(), rc);
}
