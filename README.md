<p align="center"><img src="assets/icons/app/sard-256.png" width="128" alt="Sard icon"></p>

<h1 align="center">Sard</h1>

<p align="center"><b>Low-latency, GPU-accelerated remote desktop for Linux, macOS and Windows.</b><br>
Built for driving Unity, Unreal, Godot and Blender on another machine across the room.</p>

---

- **Fast.** Hardware encode and decode on both ends with zero-copy GPU paths:
  3–4 ms from capture to decode on a wired LAN, about 8 ms Linux → Mac over Wi-Fi,
  up to 240 fps.
- **Made for 3D work.** It switches to relative mouse automatically when a viewport
  hides the cursor, and maps Cmd ⇄ Ctrl between macOS and Linux / Windows. Clipboard
  and audio are shared.
- **Any screen.** Pick which remote monitor you're looking at from the menu, or with
  <kbd>Ctrl</kbd><kbd>Alt</kbd><kbd>Shift</kbd>+<kbd>1</kbd>…<kbd>9</kbd>.
- **Private.** Devices pair once with a 6-digit one-time code. After that every connection
  is mutually authenticated and end-to-end encrypted (Noise IK). There are no
  accounts and no cloud.
- **Small.** One Rust binary (plus a tiny launcher on Windows) with six crates of dependencies. A clean build takes
  about 11 s.

## Install

Paste this into a terminal, on Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/install.sh | sh
```

On Windows 10 or 11, paste this into PowerShell:

```powershell
irm https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/install.ps1 | iex
```

It downloads the latest release, checks it against the SHA-256 in the release
manifest, installs it and starts Sard. On Linux it goes into `~/.local` (no root),
with a launcher entry and a system tray icon. On macOS it installs **Sard.app**
into Applications, with an icon in the menu bar. On Windows it goes into
`%LOCALAPPDATA%\Programs\Sard` (no administrator rights), with a Start menu entry,
`sard` on your PATH and an icon in the notification area. Run the same command again
to update, or use **Check for Updates…** in the menu.

> **Windows support is new and has not been tested on real hardware yet.** See
> [Windows](#windows) for what to try first.

<details>
<summary>Manual install, uninstall, requirements</summary>

Download from [Releases](https://github.com/parodyband/StrangeApeRemoteDesktop/releases/latest):

- **macOS** (Apple Silicon, macOS 13+): unzip `sard-<version>-macos-arm64.zip`, move
  **Sard.app** to Applications and open it.
- **Linux** (x86-64, NVIDIA GPU, PipeWire, KDE Plasma or GNOME on Wayland or X11):
  `tar xf sard-*-linux-x86_64.tar.gz && cd sard-*-linux-x86_64 && ./install.sh`
- **Windows** (10/11 x64; NVIDIA, AMD or Intel GPU): unzip `sard-<version>-windows-x86_64.zip`
  anywhere and run `sardw.exe` (the tray app) or `sard.exe` in a terminal. The files
  aren't code-signed, so a browser download triggers SmartScreen (*More info → Run
  anyway*); the PowerShell installer doesn't.

To uninstall on Linux, run
`curl -fsSL https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/install.sh | sh -s -- --uninstall`.
On macOS, quit Sard and move it to the Trash. On Windows, run
`$env:SARD_UNINSTALL=1; irm https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/install.ps1 | iex`.
Pairings stay in `~/.config/sard` (Windows: `%APPDATA%\sard`).
</details>

## Getting started

1. Open Sard on both computers.
2. On the computer you want to **control**, choose **Allow a New Device to Pair…**
   from its menu. It shows the address and a 6-digit code.
3. On the computer you're **sitting at**, choose **Connect To → Pair with a New
   Computer…** and enter that address and code.

That's it. From now on the computer is listed under **Connect To**. While you're
connected, the menu lists the remote computer's screens: click one to switch.

The first time a Linux computer is viewed, the desktop asks which screens to share
and whether to allow remote control. Select **all** monitors there to be able to
switch between them. The answer is remembered; use **Choose Screens to Share…** to
be asked again. On macOS, grant the permissions described [below](#macos-permissions).
On Windows, allow Sard through Windows Firewall when it asks (the first time the PC is
shared); all screens are always available.

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
sard pair 192.168.1.77 123456 --alias linux
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

| | Linux (NVIDIA) | macOS (Apple Silicon) | Windows (new, untested) |
|---|---|---|---|
| Capture | xdg-desktop-portal + PipeWire, DMA-BUF | ScreenCaptureKit (IOSurface) | DXGI Desktop Duplication |
| Encode / decode | NVENC / NVDEC: AV1, HEVC, H.264 | VideoToolbox: HEVC, H.264 encode; AV1 (M3+), HEVC, H.264 decode | NVENC / NVDEC; AMD, Intel: Media Foundation hardware codecs |
| Present | EGL + OpenGL via CUDA/GL interop | Metal (`CAMetalLayer`) | Direct3D 11 flip model (CUDA/D3D11 interop) |
| Input | RemoteDesktop portal | CGEvent | SendInput |
| Audio | PipeWire | ScreenCaptureKit / CoreAudio | WASAPI (loopback) |
| Clipboard | Klipper (D-Bus), wl-clipboard | NSPasteboard | Win32 clipboard |
| Tray | StatusNotifierItem + dbusmenu | NSStatusItem | Notification area icon |

**Transport** (`crates/core`): one UDP socket and one network thread; there is no
async runtime.
- **Handshake:** Noise IK (X25519, ChaCha20-Poly1305, SHA-256). Handshakes carry a
  WireGuard-style *mac1* and are rate-limited.
- **Pairing:** a 6-digit, single-use, 10-minute code. It goes through CPace (a PAKE
  on ristretto255), so the code can't be brute-forced offline, even by an attacker
  in the middle of the exchange. The host withdraws a code after 5 wrong tries. The
  PAKE key then seeds a Noise NNpsk0 exchange of static keys.
- **Packets:** per-packet AEAD with explicit counters and a replay window. A
  session index routes every packet, so peers can roam and a future relay can
  forward without decrypting.
- **Video:** frames are split into shards protected by Reed-Solomon FEC
  (GF(2⁸), Cauchy) and interleaved. The viewer NACKs what FEC can't repair and
  resynchronizes on keyframes.
- **Adaptation:** FEC follows measured loss, while bitrate follows *congestion*:
  unrecoverable frames, or a standing queue (even the best round trip of the
  last 0.6 s is late). Random Wi-Fi loss and brief radio stalls (a Mac's AWDL
  for AirDrop pauses Wi-Fi ~70 ms every half second while active) don't lower
  quality.
- **Control:** a reliable ordered channel carries input and control messages.
  Clock sync gives true end-to-end latency figures.

**Dependencies:** GPU and OS libraries are loaded with `dlopen` (`LoadLibrary` on
Windows) and bound by hand, with no bindgen, no `-sys` crates, no `windows` crate and
no FFmpeg. Exact C struct layouts come from small generators in `tools/`
(`tools/winabi/check.py` checks the hand-written Win32/COM declarations against the
SDK headers), and the D-Bus client is about 800 lines of Rust. The crates used are
`libc`, `chacha20poly1305`, `x25519-dalek`, `sha2`, `winit` and `raw-window-handle`,
plus `objc2`/`block2` on macOS.

## Building from source

```sh
cargo build --release                 # target/release/sard
scripts/macos-app.sh                  # macOS: dist/Sard.app (needed for permissions)
packaging/linux/install.sh            # Linux: install binary, icon and launcher entry
```

Linux needs the NVIDIA driver (NVENC/NVDEC/CUDA), PipeWire and xdg-desktop-portal.
macOS needs the Xcode command line tools. Windows needs the MSVC build tools (Visual
Studio Build Tools, "Desktop development with C++"); `scripts/package-windows.sh`
makes the release zip. Releases are built by GitHub Actions; see
[docs/RELEASING.md](docs/RELEASING.md).

### Windows

Windows support (host and viewer) is a first version that compiles and passes CI but
**has not run on real hardware yet**. What to try first on a PC:

1. `sard info`: lists the GPUs, which encoder and decoder each codec would use and
   why (NVENC/NVDEC on NVIDIA, the GPU's Media Foundation codecs otherwise), the
   shader/Media Foundation checks and the screens Desktop Duplication sees.
2. `cargo test -p sard-windows --test gpu_roundtrip -- --nocapture`: encodes a test
   pattern on the GPU and decodes it again, for each codec.
3. `sard selftest-capture 10 --decode --snapshot shot.png` (add `--codec h264|av1`,
   `--display 2`, `--input-test`): captures the real screen, encodes, decodes every
   frame and saves one as a PNG; prints timings.
4. `sard host --test-pattern 2560x1440` on the PC and `sard connect` from another
   machine, then the real screen; then the other direction.

`SARD_ENCODER=mf` / `SARD_DECODER=mf` force Media Foundation on NVIDIA GPUs (to test
the AMD/Intel path); `SARD_LOG=debug` adds per-stage timings. The tray app logs to
`%LOCALAPPDATA%\Sard\app.log`.

Known limits: the secure desktop (UAC prompts, the lock screen, Ctrl+Alt+Del) can't be
captured or controlled by a normal process, and nor can windows of elevated apps unless
Sard runs as administrator; Desktop Duplication doesn't work over Remote Desktop or as
a service; rotated displays are streamed unrotated; HDR desktops are tone-mapped to
SDR by Windows. HEVC and AV1 decoding with Media Foundation need Microsoft's (free)
*HEVC Video Extensions* / *AV1 Video Extension* from the Store.

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
- [ ] Windows host and viewer (written, builds and passes CI; not yet tested on hardware)
- [x] Menu bar / tray app, pairing from the UI, start at login, self-update
- [x] AV1 by default when both ends have hardware for it (Mac viewers: M3 and later), else HEVC
- [x] Connect Anywhere: reach paired computers across the internet (hole punching, TURN relay fallback)
- [x] Viewer-drawn cursor for macOS hosts (instant pointer; the video no longer carries the cursor)
- [x] Bitrate control that tells Wi-Fi stalls (AWDL / power save) from real congestion
- [ ] Optional: pause AWDL (AirDrop/Handoff radio) while viewing from a Mac on Wi-Fi

## License

MIT — see [LICENSE](LICENSE).
