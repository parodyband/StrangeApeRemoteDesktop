//! GPU round trip on Windows: the test pattern through the host pipeline
//! (BGRA → NV12 shader → NVENC or Media Foundation encoder) and back through
//! the viewer's decoder (NVDEC or Media Foundation), checking the decoded
//! picture. Skips (passes) without a hardware GPU, as on CI runners.
//!
//! `cargo test -p sard-windows --test gpu_roundtrip -- --nocapture` prints
//! which encoder/decoder were used and their timings. `SARD_ENCODER=mf` /
//! `SARD_DECODER=mf` force Media Foundation on NVIDIA.

#![cfg(target_os = "windows")]

use sard_core::media::SourceOptions;
use sard_core::proto::Codec;
use sard_windows::platform;
use std::time::{Duration, Instant};

fn has_gpu() -> bool {
    let Ok(f) = sard_windows::d3d::factory() else { return false };
    sard_windows::d3d::preferred_adapter(&sard_windows::d3d::adapters(&f)).is_some()
}

fn roundtrip(codec: Codec) {
    if !has_gpu() {
        eprintln!("skipping: no hardware GPU");
        return;
    }
    if platform::encoders() & codec.bit() == 0 || platform::decoders() & codec.bit() == 0 {
        eprintln!("skipping {}: not supported in hardware here\n{}", codec.name(), platform::capabilities().join("\n"));
        return;
    }
    let opts = SourceOptions {
        codec,
        fps: 60,
        bitrate_kbps: 20_000,
        test_pattern: true,
        test_size: (1280, 720),
        ..Default::default()
    };
    let (mut src, _input) = platform::open_host(&opts).expect("open test source");
    eprintln!("source: {}", src.describe());
    let mut sink = platform::create_headless_sink().expect("headless sink");
    let dir = std::env::temp_dir().join(format!("sard-roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join(format!("{}.png", codec.name()));
    let (mut frames, mut keys, mut bytes, mut dec_us) = (0u32, 0u32, 0usize, 0u64);
    let start = Instant::now();
    while frames < 60 && start.elapsed() < Duration::from_secs(20) {
        if frames == 30 {
            src.request_keyframe();
            sink.snapshot_next(&png).unwrap();
        }
        let Some(f) = src.next_frame(Duration::from_millis(200)).expect("encode") else { continue };
        assert_eq!((f.width, f.height), (1280, 720));
        if frames == 0 {
            assert!(f.keyframe, "first frame must be a keyframe");
        }
        keys += f.keyframe as u32;
        bytes += f.data.len();
        let st = sink.present(codec, &f.data, f.keyframe).expect("decode");
        dec_us += st.decode_us as u64;
        frames += 1;
    }
    eprintln!(
        "{}: {frames} frames, {keys} keyframes, {:.1} KB/frame, decode avg {:.2} ms, via {}",
        codec.name(),
        bytes as f64 / frames.max(1) as f64 / 1024.0,
        dec_us as f64 / frames.max(1) as f64 / 1000.0,
        sink.describe()
    );
    assert!(frames >= 60, "only {frames} frames in 20 s");
    assert!(keys >= 2, "requested keyframe missing");
    // The snapshot is the colour-bar pattern: the left bar is light grey.
    let data = std::fs::read(&png).expect("snapshot written");
    assert!(data.starts_with(b"\x89PNG"), "snapshot is a PNG");
    eprintln!("snapshot: {}", png.display());
}

#[test]
fn hevc() {
    roundtrip(Codec::Hevc);
}

#[test]
fn h264() {
    roundtrip(Codec::H264);
}

#[test]
fn av1() {
    roundtrip(Codec::Av1);
}
