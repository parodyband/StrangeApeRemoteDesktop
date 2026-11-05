//! Micro-benchmark of the per-packet hot path (AEAD + reassembly), to compare
//! platforms: `cargo run --release -p sard-core --example hotpath`.
use sard_core::crypto::TransportCipher;
use sard_core::proto::Codec;
use sard_core::video::{FrameMeta, VideoReceiver, VideoSender, frame_flags};
use std::time::Instant;

fn main() {
    let c = TransportCipher::new([7; 32]);
    let mut buf = vec![0u8; 1371];
    let t = Instant::now();
    let n = 20_000;
    for i in 0..n {
        let tag = c.seal(i, b"hdr", &mut buf);
        c.open(i, b"hdr", &mut buf, &tag).unwrap();
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "AEAD seal+open 1371 B: {:.2} us/packet ({:.0} MB/s)",
        dt * 1e6 / n as f64,
        n as f64 * 1371.0 * 2.0 / dt / 1e6
    );

    let mut tx = VideoSender::new(1371);
    let mut rx = VideoReceiver::new();
    let mut pkts = Vec::new();
    for i in 0..120u32 {
        let mut f = Vec::new();
        let key = i % 60 == 0;
        let flags = if key { frame_flags::KEYFRAME } else { 0 };
        FrameMeta { codec: Codec::Hevc, flags, width: 2560, height: 1440, capture_us: 0, send_us: 0 }.write(&mut f);
        f.resize(if key { 250_000 } else { 50_000 }, (i % 251) as u8);
        tx.send_frame(&f, flags, |p| pkts.push(p.to_vec()));
    }
    let t = Instant::now();
    let mut now = 0;
    for p in &pkts {
        now += 5;
        rx.on_shard(&p[1..], now).unwrap();
    }
    let mut frames = 0;
    while rx.pop_ready().is_some() {
        frames += 1;
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "reassembly: {} packets, {frames} frames in {:.1} ms ({:.2} us/packet)",
        pkts.len(),
        dt * 1e3,
        dt * 1e6 / pkts.len() as f64
    );
    let t = Instant::now();
    let mut tx2 = VideoSender::new(1371);
    let f = vec![1u8; 50_000];
    for _ in 0..200 {
        tx2.send_frame(&f, 0, |_| {});
    }
    println!("packetize+FEC 50 KB frame: {:.0} us", t.elapsed().as_secs_f64() * 1e6 / 200.0);
}
