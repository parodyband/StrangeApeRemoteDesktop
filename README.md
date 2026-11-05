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

## macOS

Apple Silicon, macOS 13 or newer. Everything runs on the media engine and GPU:
ScreenCaptureKit delivers IOSurface NV12 buffers straight into VideoToolbox (no
copy); on the client, VideoToolbox decodes into IOSurfaces that Metal samples
through `CVMetalTextureCache` (no copy), converting BT.709 YUV→RGB in a shader and
presenting on a `CAMetalLayer` with display sync off.

Measured on an M5 Pro (macOS 26.5), Mac on Wi-Fi, Linux box wired:

| Path | Result |
|---|---|
| VideoToolbox HEVC encode, 1080p / 1440p / 4K (`--preset 1`, default) | 2.6 / 4.1 / 8.3 ms |
| same with low-latency rate control (`--preset 2`+) | 4.4 / 7.1 / 14.7 ms |
| VideoToolbox HEVC decode, 1080p / 1440p / 4K | 1.0 / 1.2 / 1.7 ms |
| Mac test pattern → decoded on Linux (2560×1440 HEVC @ 60) | **10.7–11.7 ms** p50 (low-latency RC: 13.4 ms) |
| Linux test pattern → decoded on Mac (1920×1080 HEVC @ 30) | 15.5–18.9 ms p50 (Wi-Fi RTT ≈ 10 ms) |

`--preset 1` (default) uses VideoToolbox's normal rate control with
*PrioritizeEncodingSpeedOverQuality* for HEVC, which encodes ~40 % faster than
the low-latency rate control but spends more bits for the same quality;
`--preset 2` and above (and H.264 always) use the low-latency rate control
(steadier frame sizes, better quality per bit).

### Permissions

macOS ties privacy permissions to the signed app, so build an app bundle:

```sh
scripts/macos-app.sh            # → dist/Sard.app (release build, signed)
```

It signs with the first code-signing identity in your keychain (e.g. a free
"Apple Development" certificate from Xcode). **With an ad-hoc signature (no
identity, or a locked keychain over SSH) every rebuild looks like a new app and
macOS forgets the grants below**, so use a stable identity for day-to-day use.

| Permission | Needed for | Where |
|---|---|---|
| Screen & System Audio Recording | `sard host` (screen capture) | System Settings → Privacy & Security → Screen & System Audio Recording → enable **Sard** |
| Accessibility | `sard host` (keyboard/mouse injection) | System Settings → Privacy & Security → Accessibility → enable **Sard** |
| Local Network | host and client (LAN traffic) | allow the "find devices on local networks" prompt; if missed: Privacy & Security → Local Network → **Sard** |

The first `sard host` run shows the system prompts (or adds Sard to the lists);
enable the switches, then restart the host. `sard info` prints whether Screen
Recording and Accessibility are granted. When sard is run from a terminal instead
of through the app, macOS attributes the permissions to the terminal app.
`sard host --test-pattern` and the client need no permissions besides Local Network
(without it, connections fail with "No route to host").

### Running

```sh
open -n dist/Sard.app --stdout /tmp/sard-host.log --stderr /tmp/sard-host.log --args host --pair
open -n dist/Sard.app --stdout /tmp/sard.log --stderr /tmp/sard.log --args connect 192.168.1.77
# environment variables: open --env SARD_LOG=debug --env SARD_CONFIG_DIR=... -n dist/Sard.app --args ...
```

`--display <n>` picks the display to share (0 = main display, then the others).
The cursor is drawn into the video by ScreenCaptureKit. Audio and clipboard are
not implemented on macOS yet.

## Roadmap

- [x] Core transport: crypto, FEC, reliable channel, video reassembly, adaptive FEC/bitrate
- [x] Linux host: portal + PipeWire capture (DMA-BUF zero-copy), NVENC, portal input injection
- [x] Linux client: NVDEC + GL presentation, input capture, local cursor, relative mouse
- [x] Audio (Linux), clipboard sync (Linux)
- [x] macOS client: VideoToolbox + Metal (zero-copy), headless mode
- [x] macOS host: ScreenCaptureKit + VideoToolbox + CGEvent
- [ ] macOS AV1 decode (hardware is there on M3+), client-side cursor on macOS hosts
- [ ] macOS audio and clipboard
- [ ] NAT traversal / relay (protocol already routes by session index)
