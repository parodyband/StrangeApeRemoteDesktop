<p align="center"><img src="assets/icons/app/sard-256.png" width="128" alt="Sard icon"></p>

<h1 align="center">Sard</h1>

<p align="center"><b>Low-latency, GPU-accelerated remote desktop for Linux ⇄ macOS.</b><br>
Built for driving Unity, Unreal, Godot and Blender on another machine across the room.</p>

---

- **Fast.** Hardware encode and decode on both ends with zero-copy GPU paths:
  3–4 ms from capture to decode on a wired LAN, about 8 ms Linux → Mac over Wi-Fi,
  up to 240 fps.
- **Made for 3D work.** It switches to relative mouse automatically when a viewport
  hides the cursor, and maps Cmd ⇄ Ctrl between macOS and Linux. Clipboard and
  audio are shared.
- **Any screen.** Pick which remote monitor you're looking at from the menu, or with
  <kbd>Ctrl</kbd><kbd>Alt</kbd><kbd>Shift</kbd>+<kbd>1</kbd>…<kbd>9</kbd>.
- **Private.** Devices pair once with a one-time code. After that every connection
  is mutually authenticated and end-to-end encrypted (Noise IK). There are no
  accounts and no cloud.
- **Small.** One Rust binary with six crates of dependencies. A clean build takes
  about 11 s.

## Install

Download the latest release from
[Releases](https://github.com/parodyband/StrangeApeRemoteDesktop/releases/latest).
Sard checks for updates itself (menu → *Check for Updates…*).

**macOS** (Apple Silicon, macOS 13+): unzip `Sard-macos.zip`, move **Sard.app** to
Applications and open it. Sard lives in the menu bar.

**Linux** (NVIDIA GPU, PipeWire, KDE Plasma or GNOME on Wayland or X11):

```sh
tar xf sard-linux-x86_64.tar.gz && cd sard-* && ./install.sh   # installs to ~/.local
```

Then start **Sard** from your app launcher. It lives in the system tray.

## Getting started

1. Open Sard on both computers.
2. On the computer you want to **control**, choose **Allow a New Device to Pair…**
   from its menu. It shows the address and a one-time code.
3. On the computer you're **sitting at**, choose **Connect To → Pair with a New
   Computer…** and enter that address and code.

That's it. From now on the computer is listed under **Connect To**. While you're
connected, the menu lists the remote computer's screens: click one to switch.

The first time a Linux computer is viewed, the desktop asks which screens to share
and whether to allow remote control. Select **all** monitors there to be able to
switch between them. The answer is remembered; use **Choose Screens to Share…** to
be asked again. On macOS, grant the permissions described [below](#macos-permissions).

### Shortcuts in a viewer window

| <kbd>Ctrl</kbd><kbd>Alt</kbd><kbd>Shift</kbd> + | |
|---|---|
| <kbd>1</kbd> … <kbd>9</kbd> | show remote screen *n* |
| <kbd>D</kbd> | next remote screen |
| <kbd>F</kbd> | full screen |
| <kbd>M</kbd> | relative mouse on/off (games, 3D viewports) |
| <kbd>K</kbd> | request a fresh keyframe |
| <kbd>Q</kbd> | disconnect |

## Command line

Everything in the menu also works from a terminal:

```sh
sard host --pair                         # share this computer; prints a pairing code
sard pair 192.168.1.77 <code> --alias linux
sard connect linux                       # open a viewer window
sard alias 192.168.1.131 mac             # name a paired computer; `sard alias` lists them
sard info                                # identity, paired devices, encoders/decoders
sard update                              # install the latest release
```

Useful options: `--fps`, `--bitrate <Mbps>`, `--codec hevc|h264|av1`, `--yuv444`,
`--display <n>`, `--fullscreen`, `--view-only`, `--no-audio`, `--max-datagram 1200`
(VPNs). `sard help` lists them all.

## Performance

Linux host: RTX 4070 Ti SUPER, KDE Plasma (Wayland). Mac: M5 Pro on Wi-Fi.

| Path | Result |
|---|---|
| Linux screen capture → encoded (DMA-BUF zero-copy, 5120×1440 HEVC) | **3.5 ms** p50 |
| Linux desktop → shown in a Linux viewer window (3840×1080 @ 60) | **3.5 ms** p50 |
| Linux → Mac, 2560×1440 HEVC @ 120 fps over Wi-Fi | **7.5 ms** p50, p95 8.3 ms |
| Mac → Linux, 2560×1440 HEVC @ 60 fps over Wi-Fi | **9–11 ms** p50 |
| Switching remote screens | ~50 ms |
| 10 % / 25 % random packet loss (simulated, loopback) | 120 fps, 0 frames lost |

Latency is measured from capture to decoded frame, using synchronized clocks. It
does not include the display's scan-out.

## How it works

```
 HOST                                                        VIEWER
 ┌────────────┐   ┌──────────┐   ┌────────────┐  UDP   ┌────────────┐   ┌──────────┐   ┌─────────┐
 │  capture   │──▶│  encode  │──▶│ packetize  │══════▶│ reassemble │──▶│  decode  │──▶│ present │
 │ PipeWire / │   │ NVENC /  │   │ FEC + AEAD │◀══════│ NACK / KF  │   │ NVDEC /  │   │ GL /    │
 │ SCKit      │   │ VideoTbx │   └────────────┘ input │  requests  │   │ VideoTbx │   │ Metal   │
 └────────────┘   └──────────┘         ▲              └────────────┘   └──────────┘   └─────────┘
 ┌────────────┐                        │ reliable: keys, buttons, scroll, cursor, clipboard
 │  inject    │◀───────────────────────┘ unreliable: pointer position, audio (5 ms PCM)
 │ portal /   │
 │ CGEvent    │
 └────────────┘
```

| | Linux (NVIDIA) | macOS (Apple Silicon) |
|---|---|---|
| Capture | xdg-desktop-portal + PipeWire, DMA-BUF | ScreenCaptureKit (IOSurface) |
| Encode / decode | NVENC / NVDEC: HEVC, H.264, AV1 | VideoToolbox: HEVC, H.264 |
| Present | EGL + OpenGL via CUDA/GL interop | Metal (`CAMetalLayer`) |
| Input | RemoteDesktop portal | CGEvent |
| Audio | PipeWire | ScreenCaptureKit / CoreAudio |
| Clipboard | Klipper (D-Bus), wl-clipboard | NSPasteboard |
| Tray | StatusNotifierItem + dbusmenu | NSStatusItem |

**Transport** (`crates/core`): one UDP socket and one network thread; there is no
async runtime.
- **Handshake:** Noise IK (X25519, ChaCha20-Poly1305, SHA-256). Pairing uses Noise
  NNpsk0 with a single-use 10-minute code. Handshakes carry a WireGuard-style *mac1*
  and are rate-limited.
- **Packets:** per-packet AEAD with explicit counters and a replay window. A
  session index routes every packet, so peers can roam and a future relay can
  forward without decrypting.
- **Video:** frames are split into shards protected by Reed-Solomon FEC
  (GF(2⁸), Cauchy) and interleaved. The viewer NACKs what FEC can't repair and
  resynchronizes on keyframes.
- **Adaptation:** FEC follows measured loss, while bitrate follows *congestion*
  (queueing delay, unrecoverable frames). Random Wi-Fi loss doesn't lower quality.
- **Control:** a reliable ordered channel carries input and control messages.
  Clock sync gives true end-to-end latency figures.

**Dependencies:** GPU and OS libraries are loaded with `dlopen` and bound by hand,
with no bindgen, no `-sys` crates and no FFmpeg. Exact C struct layouts come from
small generators in `tools/`, and the D-Bus client is about 800 lines of Rust. The
crates used are `libc`, `chacha20poly1305`, `x25519-dalek`, `sha2`, `winit` and
`raw-window-handle`, plus `objc2`/`block2` on macOS.

## Building from source

```sh
cargo build --release                 # target/release/sard
scripts/macos-app.sh                  # macOS: dist/Sard.app (needed for permissions)
packaging/linux/install.sh            # Linux: install binary, icon and launcher entry
```

Linux needs the NVIDIA driver (NVENC/NVDEC/CUDA), PipeWire and xdg-desktop-portal.
macOS needs the Xcode command line tools. Releases are built by GitHub Actions; see
[docs/RELEASING.md](docs/RELEASING.md).

### macOS permissions

macOS ties privacy permissions to the signed app, so run Sard as **Sard.app** (not a
bare binary from a terminal) when sharing the Mac.

| Permission | Needed for | System Settings → Privacy & Security → |
|---|---|---|
| Screen & System Audio Recording | sharing the screen and audio | Screen & System Audio Recording → **Sard** |
| Accessibility | remote keyboard and mouse | Accessibility → **Sard** |
| Local Network | all connections | Local Network → **Sard** (or accept the prompt) |

The menu shows which permissions are still missing. A locally built app has to be
signed with a stable identity (`scripts/macos-app.sh` uses the first one in your
keychain); with an ad-hoc signature, macOS forgets the grants on every rebuild.

### Diagnostics

- `sard host --test-pattern 2560x1440` streams a synthetic pattern with two fake
  screens and needs no capture permission.
- `sard connect <host> --headless --duration 10 --snapshot frame.png` runs without a
  window: it logs fps, latency and loss and saves a decoded frame.
  `--cycle-displays 2` switches screens every 2 s.
- `SARD_SIM_LOSS=0.05` drops 5 % of incoming packets; `SARD_LOG=debug` logs per-stage
  timings.
- `cargo run -p sard-linux --example sni_panel` is a stand-in tray panel for testing
  the Linux tray on a private D-Bus.

## Roadmap

- [x] Linux and macOS hosts and viewers, audio, clipboard, multi-monitor switching
- [x] Menu bar / tray app, pairing from the UI, start at login, self-update
- [ ] macOS AV1 decode (M3+ hardware), viewer-drawn cursor for macOS hosts
- [ ] Connections beyond the LAN (NAT traversal / relay; the protocol already
      routes by session index)

## License

MIT — see [LICENSE](LICENSE).
