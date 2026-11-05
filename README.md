# StrangeApeRemoteDesktop (`sard`)

Low-latency, GPU-accelerated remote desktop for **Linux ⇄ macOS**, written in Rust
with a deliberately tiny dependency tree so builds stay fast (clean build ≈ 11 s,
incremental ≈ 1 s).

Built for streaming 3D / game-engine work (Unity, Unreal, Godot, Blender) over a
LAN at high frame rates, with hardware encode/decode on both ends and zero-copy
GPU paths.

## Measured (Linux host, RTX 4070 Ti SUPER, KDE Plasma Wayland)

| Path | Result |
|---|---|
| Screen capture → encoded (DMA-BUF zero-copy, 5120×1440 HEVC) | **3.5 ms** p50 |
| Capture → decoded on client (2560×1440 HEVC @ 120 fps, loopback) | **3.0–3.3 ms** p50 |
| Real desktop → displayed in client window (3840×1080 @ 60 fps) | **3.5 ms** p50 |
| NVENC 1440p HEVC encode (preset P1) / NVDEC decode | 1.6 ms / 1.1 ms |
| 10 % / 25 % random packet loss (simulated) | 120 fps, 0 frames lost, 3.2–3.4 ms |
| Input injection → visible in capture | 7–17 ms (bounded by compositor frame timing) |

Latency figures are capture → decode/present and exclude the display's scan-out.

## Architecture

```
 HOST                                                        CLIENT
 ┌────────────┐   ┌──────────┐   ┌────────────┐  UDP   ┌────────────┐   ┌──────────┐   ┌─────────┐
 │  capture   │──▶│  encode  │──▶│ packetize  │══════▶│ reassemble │──▶│  decode  │──▶│ present │
 │ PipeWire / │   │ NVENC /  │   │ FEC + AEAD │◀══════│ NACK / KF  │   │ NVDEC /  │   │ GL /    │
 │ SCKit      │   │ VideoTbx │   └────────────┘ input │  requests  │   │ VideoTbx │   │ Metal   │
 └────────────┘   └──────────┘         ▲              └────────────┘   └──────────┘   └─────────┘
                                       │ reliable channel: keys, buttons, scroll, cursor, clipboard
 ┌────────────┐                        │ unreliable: pointer position, audio (5 ms PCM)
 │  inject    │◀───────────────────────┘
 │ portal /   │
 │ CGEvent    │
 └────────────┘
```

| Layer            | Linux (NVIDIA)                                                    | macOS (Apple Silicon)                    |
|------------------|-------------------------------------------------------------------|------------------------------------------|
| Capture          | xdg-desktop-portal RemoteDesktop+ScreenCast → PipeWire, DMA-BUF   | ScreenCaptureKit (IOSurface)             |
| Colour convert   | OpenGL shader, BT.709 limited, GPU downscale to client size       | ScreenCaptureKit `420v`                  |
| Encode           | NVENC (dlopen), HEVC / H.264 / AV1, ultra-low-latency CBR         | VideoToolbox HEVC / H.264                |
| Decode           | NVDEC (dlopen), zero display delay                                | VideoToolbox                             |
| Present          | EGL + OpenGL via CUDA/GL interop (no CPU copies)                  | Metal (CAMetalLayer)                     |
| Input injection  | RemoteDesktop portal (evdev codes)                                | CGEvent                                  |
| Audio            | PipeWire monitor capture / playback                               | (in progress)                            |
| Clipboard        | Klipper over D-Bus, wl-clipboard fallback                         | (in progress)                            |

### Transport (`crates/core`)

* **UDP**, one socket, one network thread that also runs all timers. No async runtime.
* **Noise IK** handshake (X25519, ChaCha20-Poly1305, SHA-256) with mutual static-key
  authentication; one-time **pairing** with a 100-bit code via Noise NNpsk0.
* **Per-packet AEAD** with explicit counters + 4096-packet replay window
  (WireGuard-style), so loss and reordering are harmless.
* **Session indices** in every packet header: routing never depends on the source
  address (peers can roam; a future relay can forward by index without decrypting).
* **Video**: frames split into equal shards, grouped into Reed-Solomon (Cauchy,
  GF(2⁸)) FEC blocks, interleaved across blocks. The receiver repairs with FEC,
  NACKs what FEC can't fix, delivers frames strictly in order, and on unrecoverable
  loss skips to the next keyframe and requests one. FEC overhead adapts to the
  reported loss; bitrate adapts to *congestion* (unrecoverable frames, queueing
  delay), not to random Wi-Fi loss.
* **Reliable ordered channel** (selective acks, fragmentation) for input and control.
* Clock-offset estimation (min-RTT, NTP style) for real end-to-end latency readouts.

### Dependencies

Runtime GPU/OS libraries are loaded with `dlopen` and bound by hand — no bindgen,
no `-sys` crates, no FFmpeg. Exact C struct layouts come from small generators in
`tools/` (compiled once against NVIDIA's and PipeWire's headers, output committed),
so nothing is hand-transcribed. The D-Bus client for the desktop portal is ~600
lines of Rust instead of zbus. Crates: `libc`, `chacha20poly1305`, `x25519-dalek`,
`sha2`, `winit`, `raw-window-handle`; on macOS `objc2`/`block2`.

## Building

```sh
cargo build --release          # binary: target/release/sard
```

Linux needs the NVIDIA driver (NVENC/NVDEC/CUDA), PipeWire and xdg-desktop-portal
(KDE Plasma / GNOME). macOS needs the Xcode command line tools.

## Usage

```sh
# On the machine to be controlled:
sard host --pair            # prints a one-time pairing code

# On the viewing machine (once):
sard pair 192.168.1.77 ABCDE-FGHJK-LMNPQ-RSTUV

# Then, any time:
sard connect 192.168.1.77
```

The first connection to a Linux host shows the desktop's screen-share dialog (pick a
monitor and allow remote control). The approval is remembered; `sard host
--reselect` asks again. To run the host at login see `scripts/sard-host.service`.

In-session hotkeys (Ctrl+Alt+Shift + key): **F** fullscreen, **M** relative mouse
(for 3D viewports / games), **K** request keyframe, **Q** disconnect. Between macOS
and Linux, Cmd and Ctrl are swapped so shortcuts land naturally
(`--no-swap-modifiers` to disable).

Useful options: `--fps`, `--bitrate <Mbps>`, `--codec hevc|h264|av1`, `--yuv444`,
`--view-only`, `--no-audio`, `--max-datagram 1200` (VPNs). Run `sard help` for all.

### Diagnostics

* `sard info` — identity, paired devices, encoder/decoder support.
* `sard host --test-pattern 2560x1440` — stream a synthetic pattern (no capture permission needed).
* `sard connect <host> --headless --duration 10 --snapshot frame.png` — windowless
  client that logs fps / latency / loss and saves a decoded frame.
* `SARD_SIM_LOSS=0.05` — drop 5 % of incoming packets (test FEC / recovery).
* `SARD_LOG=debug` — verbose logs, including per-stage capture timings.

## Roadmap

- [x] Core transport: crypto, FEC, reliable channel, video reassembly, adaptive FEC/bitrate
- [x] Linux host: portal + PipeWire capture (DMA-BUF zero-copy), NVENC, portal input injection
- [x] Linux client: NVDEC + GL presentation, input capture, local cursor, relative mouse
- [x] Audio (Linux), clipboard sync (Linux)
- [ ] macOS client: VideoToolbox + Metal (in progress)
- [ ] macOS host: ScreenCaptureKit + VideoToolbox + CGEvent (in progress)
- [ ] macOS audio and clipboard
- [ ] NAT traversal / relay (protocol already routes by session index)
