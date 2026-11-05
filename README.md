# StrangeApeRemoteDesktop (`sard`)

Low-latency, GPU-accelerated remote desktop for **Linux ⇄ macOS**, written in Rust
with a deliberately tiny dependency tree so builds stay fast.

Built for streaming 3D/game-engine work (Unity, Unreal, Godot, Blender) over a LAN
at high frame rates, with hardware encode/decode on both ends and no extra copies
where avoidable.

> Status: under active development. See [Roadmap](#roadmap).

## Architecture

```
 HOST                                                        CLIENT
 ┌────────────┐   ┌──────────┐   ┌────────────┐  UDP   ┌────────────┐   ┌──────────┐   ┌─────────┐
 │  capture   │──▶│  encode  │──▶│ packetize  │══════▶│ reassemble │──▶│  decode  │──▶│ present │
 │ PipeWire / │   │ NVENC /  │   │ FEC + AEAD │◀══════│ NACK / KF  │   │ NVDEC /  │   │ GL /    │
 │ SCKit      │   │ VideoTbx │   └────────────┘ input │  requests  │   │ VideoTbx │   │ Metal   │
 └────────────┘   └──────────┘         ▲              └────────────┘   └──────────┘   └─────────┘
                                       │ reliable channel: keys, buttons, scroll, cursor, clipboard
 ┌────────────┐                        │
 │  inject    │◀───────────────────────┘
 │ portal /   │
 │ CGEvent    │
 └────────────┘
```

| Layer            | Linux (NVIDIA)                                         | macOS (Apple Silicon)                    |
|------------------|--------------------------------------------------------|------------------------------------------|
| Capture          | xdg-desktop-portal ScreenCast + PipeWire (DMA-BUF/SHM) | ScreenCaptureKit (IOSurface)             |
| Encode           | NVENC via `libnvidia-encode` (dlopen), HEVC/H.264/AV1  | VideoToolbox HEVC/H.264, low-latency RC  |
| Decode           | NVDEC via `libnvcuvid` (dlopen)                        | VideoToolbox                             |
| Present          | EGL + OpenGL                                           | Metal (CAMetalLayer)                     |
| Input injection  | RemoteDesktop portal (uinput fallback)                 | CGEvent                                  |

### Transport (`crates/core`)

* **UDP**, one socket, one network thread that also runs all timers. No async runtime.
* **Noise IK** handshake (X25519, ChaCha20-Poly1305, SHA-256) with mutual static-key
  authentication; one-time **pairing** with a 100-bit code via Noise NNpsk0.
* **Per-packet AEAD** with explicit counters + 4096-packet replay window (WireGuard-style),
  so loss and reordering are harmless.
* **Session indices** in every packet header: routing never depends on source address
  (peers can roam; a future relay can forward by index without decrypting).
* **Video**: frames split into equal shards, grouped into Reed-Solomon (Cauchy, GF(2⁸))
  FEC blocks, interleaved across blocks; receiver repairs with FEC, NACKs what FEC can't
  fix, delivers frames strictly in order, and on unrecoverable loss drops to the next
  keyframe/recovery frame and requests one. FEC overhead adapts to reported loss.
* **Reliable ordered channel** (selective acks, fragmentation) for input and control.
* Clock-offset estimation (min-RTT NTP style) for true glass-to-glass latency readouts.

### Dependencies

Runtime GPU/OS libraries are loaded with `dlopen` and bound by hand (no bindgen, no
FFmpeg). Crates used: `libc`, `chacha20poly1305`, `x25519-dalek`, `sha2`, `winit`,
and on macOS `objc2`/`block2`.

## Building

```sh
cargo build --release
```

Linux needs the NVIDIA driver (for NVENC/NVDEC) and PipeWire; macOS needs Xcode
command line tools.

## Usage

```sh
# On the machine to be controlled:
sard host --pair            # prints a one-time pairing code

# On the viewing machine (once):
sard pair 192.168.1.77 ABCDE-FGHJK-LMNPQ-RSTUV

# Then, any time:
sard connect 192.168.1.77
```

## Roadmap

- [x] Core transport: crypto, FEC, reliable channel, video reassembly
- [ ] Linux host: portal + PipeWire capture, NVENC, input injection
- [ ] Linux client: NVDEC + GL presentation, input capture
- [ ] macOS client: VideoToolbox + Metal
- [ ] macOS host: ScreenCaptureKit + VideoToolbox + CGEvent
- [ ] Audio, clipboard, client-side cursor, adaptive bitrate, stats overlay
