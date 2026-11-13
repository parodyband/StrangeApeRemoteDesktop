# Sard: technical notes

How Sard works, how fast it is, and how to build, test and debug it. For using
Sard, see the [README](../README.md); for cutting releases, see
[RELEASING.md](RELEASING.md).

## Performance

Linux host: RTX 4070 Ti SUPER, KDE Plasma (Wayland). Mac: M5 Pro on Wi-Fi.

| Path | Result |
|---|---|
| Linux screen capture → encoded (DMA-BUF zero-copy, 5120×1440 HEVC) | **3.5 ms** p50 |
| Linux desktop → shown in a Linux viewer window (3840×1080 @ 60) | **3.5 ms** p50 |
| Linux → Mac, 2560×1440 HEVC @ 120 fps over Wi-Fi | **7.5 ms** p50, p95 8.3 ms |
| Linux → Mac, 2560×1440 AV1 @ 60 fps over Wi-Fi (hardware decode, M5) | **5.3–7.8 ms** p50 |
| Mac → Linux, 2560×1440 HEVC @ 60 fps over Wi-Fi | **9–11 ms** p50 |
| Switching remote screens | ~50 ms |
| 10 % / 25 % random packet loss (simulated, loopback) | 120 fps, 0 frames lost |
| Through the Connect Anywhere relay (both ends on one LAN, via Cloudflare) | 59 fps, 0 frames lost, 21 ms |

Latency is measured from capture to decoded frame, using synchronized clocks. It
does not include the display's scan-out.

## How it works

```
 HOST                                                        VIEWER
 ┌────────────┐   ┌──────────┐   ┌────────────┐  UDP   ┌────────────┐   ┌──────────┐   ┌─────────┐
 │  capture   │──▶│  encode  │──▶│ packetize  │══════▶│ reassemble │──▶│  decode  │──▶│ present │
 │ PipeWire / │   │ NVENC /  │   │ FEC + AEAD │◀══════│ NACK / KF  │   │ NVDEC /  │   │ GL /    │
 │ SCKit /    │   │ VideoTbx │   └────────────┘ input │  requests  │   │ VideoTbx │   │ Metal / │
 │ DXGI       │   │ / MF     │         ▲              └────────────┘   │ / MF     │   │ D3D11   │
 └────────────┘   └──────────┘         │                               └──────────┘   └─────────┘
 ┌────────────┐                        │ reliable: keys, buttons, scroll, cursor, clipboard
 │  inject    │◀───────────────────────┘ unreliable: pointer position, audio (5 ms PCM)
 └────────────┘
```

| | Linux (NVIDIA) | macOS (Apple Silicon) | Windows (new, untested) |
|---|---|---|---|
| Capture | xdg-desktop-portal + PipeWire, DMA-BUF | ScreenCaptureKit (IOSurface) | DXGI Desktop Duplication |
| Encode / decode | NVENC / NVDEC: AV1, HEVC, H.264 | VideoToolbox: HEVC, H.264 encode; AV1 (M3+), HEVC, H.264 decode | NVENC / NVDEC; AMD, Intel: Media Foundation hardware codecs |
| Present | EGL + OpenGL via CUDA/GL interop | Metal (`CAMetalLayer`) | Direct3D 11 flip model |
| Input | RemoteDesktop portal | CGEvent | SendInput |
| Audio | PipeWire | ScreenCaptureKit / CoreAudio | WASAPI (loopback) |
| Clipboard | Klipper (D-Bus), wl-clipboard | NSPasteboard | Win32 clipboard |
| Cursor | PipeWire cursor metadata | NSCursor + CGEvent location | DXGI pointer shape |
| Tray | StatusNotifierItem + dbusmenu | NSStatusItem | Notification area icon |

Codec choice: the host picks AV1, then HEVC, then H.264, among codecs both ends
handle in hardware (Mac viewers advertise AV1 only on M3 and later).

### Transport (`crates/core`)

One UDP socket and one network thread; there is no async runtime.

- **Handshake:** Noise IK (X25519, ChaCha20-Poly1305, SHA-256). Handshakes carry a
  WireGuard-style *mac1* and are rate-limited.
- **Pairing:** a 6-digit, single-use, 10-minute code. It goes through CPace (a PAKE
  on ristretto255), so the code can't be brute-forced offline, even by an attacker
  in the middle of the exchange. The host withdraws a code after 5 wrong tries. The
  PAKE key then seeds a Noise NNpsk0 exchange of static keys.
- **Packets:** per-packet AEAD with explicit counters and a replay window. A session
  index routes every packet, so peers can roam. Outside the LAN, datagrams are capped
  at 1200 bytes.
- **Video:** frames are split into shards protected by Reed-Solomon FEC (GF(2⁸),
  Cauchy) and interleaved. The viewer NACKs what FEC can't repair and resynchronizes
  on keyframes.
- **Adaptation:** FEC follows measured loss, while bitrate follows *congestion*:
  unrecoverable frames, or a standing queue (even the best round trip of the last
  0.6 s is above the 30 s baseline). Random Wi-Fi loss and brief radio stalls (a
  Mac's AWDL pauses Wi-Fi ~70 ms every half second while AirDrop/Handoff are
  active) don't lower quality.
- **Control:** a reliable ordered channel carries input and control messages. Clock
  sync gives true end-to-end latency figures.

### Terminals

A client that wants shells opens a session in the *terminal lane* (a byte in the
handshake hello): the host replaces an older session from the same device only
in the same lane, so a phone can show a screen and a terminal at once. Its
`ClientHello` carries the `TERMINAL` flag, so the host captures nothing and
injects no input; the reliable channel then carries `ShellOpen` / `ShellData` /
`ShellResize` / `ShellClose` (`crates/sard/src/terminal.rs`).

- The shell is the user's login shell on a pseudo-terminal (`openpty`,
  `crates/core/src/pty.rs`; ConPTY with PowerShell on Windows), started in the
  home directory with `TERM=xterm-256color`.
- Shells belong to the device that opened them and outlive the session: a phone
  drops its connection whenever the app goes to the background. Reattaching
  replays the last 256 KB of output and nudges the window size so full-screen
  programs repaint. A shell nobody reattaches to ends after 12 hours; at most 12
  per device.
- Flow control: the client acknowledges each output message (`ShellAck`) once it
  has shown it, and the host keeps at most 128 unacknowledged (and holds back
  while the reliable channel has a backlog), so `cat` of a huge file runs at the
  client's pace instead of overflowing it.
- Off until the host's owner turns on **Allow Terminal Access** (`terminal=on` in
  `settings.txt`, or `sard host --terminal`): pairing for screen sharing shouldn't
  silently grant a shell. Turning it off ends running shells.

### Fit to the viewer's screen

A viewer can ask the host to switch the streamed display to the mode closest to a
desktop size (`ClientHello.fit_*`, `Control::FitDisplay`). `media::pick_mode`
chooses: closest shape, then closest size, keeping HiDPI and refresh rate, never
larger than the current desktop. The original mode comes back when the viewer
asks, switches displays or leaves.

| | How | Restored if Sard crashes |
|---|---|---|
| macOS | `CGConfigureDisplayWithDisplayMode`, `kCGConfigureForAppOnly` | yes (by macOS) |
| Windows | `ChangeDisplaySettingsExW(CDS_FULLSCREEN)` (never saved) | not guaranteed |
| Linux | KDE: `kscreen-doctor`; GNOME: Mutter `ApplyMonitorsConfig` (temporary); X11: `xrandr` | no: the log has the command to restore it |

### Connect Anywhere

A Cloudflare Worker with one Durable Object per host (private repo
`sard-anywhere`) introduces a viewer to a host; video never passes through it.

1. The host keeps a WebSocket to its Durable Object (hibernating while idle) and
   publishes its LAN addresses plus its public address, learned from Cloudflare's
   STUN server on the socket it listens on.
2. A viewer learns its own public address the same way and asks the service for an
   introduction; the host punches UDP toward the viewer while the viewer handshakes
   with every host address. The first that answers wins.
3. If no direct path answers within 3 s, the viewer allocates a TURN relay
   (Cloudflare Realtime TURN, short-lived credentials minted by the Worker) and
   talks through it; the relay only ever sees Sard's encrypted packets.

Access is per account token (invite code); each code is its own namespace.

### Dependencies

GPU and OS libraries are loaded at runtime (`dlopen` / `LoadLibrary`) and bound by
hand: no bindgen, no `-sys` crates, no `windows` crate, no FFmpeg, no TLS crates
(HTTPS uses the system libcurl, or WinHTTP on Windows). Exact C struct layouts come
from small generators in `tools/`; `tools/winabi/check.py` checks the hand-written
Win32/COM declarations against the SDK headers. Crates: `libc`, `chacha20poly1305`,
`x25519-dalek`, `curve25519-dalek`, `sha2`, `sha1`, `md-5`, `ed25519-dalek`
(updates), `winit`, `raw-window-handle`, plus `objc2`/`block2` on macOS. A clean
build takes about 11 s.

## Building from source

```sh
cargo build --release                 # target/release/sard
scripts/macos-app.sh                  # macOS: dist/Sard.app (needed for permissions)
packaging/linux/install.sh            # Linux: install binary, icon and launcher entry
scripts/package-windows.sh            # Windows: release zip
```

Linux needs the NVIDIA driver (NVENC/NVDEC/CUDA), PipeWire and xdg-desktop-portal.
macOS needs the Xcode command line tools; a locally built Sard.app must be signed
with a stable identity (`scripts/macos-app.sh` uses the first one in your keychain),
or macOS forgets its permissions on every rebuild. Windows needs the MSVC build
tools ("Desktop development with C++"). Releases are built by GitHub Actions; see
[RELEASING.md](RELEASING.md).

## iPhone app

`ios/Sard.xcodeproj` (SwiftUI + UIKit, iOS 17+) on top of `crates/mobile`, a
static library with a C API (`crates/mobile/include/sard.h`). The Rust side does
everything protocol-shaped: pairing, Connect Anywhere, the transport,
VideoToolbox decode and Metal drawing (the macOS viewer's code, shared), audio
jitter buffering and key mapping. Swift does the UI, gestures, audio output, the
terminal emulator ([SwiftTerm](https://github.com/migueldeicaza/SwiftTerm)) and,
for Connect Anywhere, TCP + TLS on Network.framework (iOS has no public libcurl).

Building needs Xcode 16+ and Rust (`rustup target add aarch64-apple-ios
aarch64-apple-ios-sim`; the build phase adds them if missing). Open
`ios/Sard.xcodeproj`, pick your team under Signing & Capabilities, choose your
iPhone and Run: a build phase (`ios/build-rust.sh`) compiles the Rust core for the
selected device or simulator. From the command line:

```sh
xcodebuild -project ios/Sard.xcodeproj -scheme Sard \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build
```

Debug builds accept `-SardAutoOpen screen|terminal` to open the first paired
computer at launch. To pair a simulator without typing, run `sard pair` with
`SARD_CONFIG_DIR` set to the app's `Library/Application Support/sard` (from
`xcrun simctl get_app_container booted com.strangeape.sard.ios data`).
`cargo run -p sard-mobile --example smoke -- <dir> <addr> <code>` drives the C
API from the Mac against a host started with `sard host --pair --terminal
--test-pattern`.

## Command line

Everything in the menu also works from a terminal:

```sh
sard app                                 # the menu bar / tray app
sard host --pair                         # share this computer; prints a pairing code
sard pair 192.168.1.77 123456 --alias linux
sard connect linux                       # open a viewer window
sard shell linux                         # a terminal there (--list, --attach <id>)
sard alias 192.168.1.131 mac             # name a paired computer; `sard alias` lists them
sard info                                # identity, paired devices, encoders/decoders
sard update [--check]                    # install the latest release
```

Useful options: `--fps`, `--bitrate <Mbps>`, `--codec hevc|h264|av1`, `--yuv444`,
`--display <n>`, `--fullscreen`, `--view-only`, `--no-audio`, `--max-datagram 1200`
(VPNs). `sard help` lists them all.

## Diagnostics

- `sard host --test-pattern 2560x1440` streams a synthetic pattern with two fake
  screens and needs no capture permission.
- `sard connect <host> --headless --duration 10 --snapshot frame.png` runs without a
  window: it logs fps, latency, loss and cursor updates and saves a decoded frame.
  `--cycle-displays 2` switches screens every 2 s.
- `SARD_SIM_LOSS=0.05` drops 5 % of incoming packets; `SARD_LOG=debug` logs
  per-stage timings; `SARD_FORCE_RELAY=1` connects through the Connect Anywhere relay.
- `scripts/nat-check.py` tells whether a network allows direct connections.
- `cargo run -p sard-linux --example sni_panel` is a stand-in tray panel for testing
  the Linux tray on a private D-Bus.
- Logs of the tray app: `~/.local/state/sard/app.log` (Linux),
  `~/Library/Logs/Sard/app.log` (macOS), `%LOCALAPPDATA%\Sard\app.log` (Windows).

## Windows: first tests on real hardware

Windows support compiles and passes CI but hasn't run on a real PC yet. In order:

1. `sard info`: lists the GPUs, which encoder and decoder each codec would use and
   why (NVENC/NVDEC on NVIDIA, the GPU's Media Foundation codecs otherwise), and the
   screens Desktop Duplication sees.
2. `cargo test -p sard-windows --test gpu_roundtrip -- --nocapture`: GPU encode →
   decode of a test pattern for each codec.
3. `sard selftest-capture 10 --decode --snapshot shot.png` (add `--codec h264|av1`,
   `--display 2`, `--input-test`): the real screen through encode and decode.
4. `sard host --test-pattern 2560x1440` on the PC and `sard connect` from another
   machine, then the real screen; then the other direction.

`SARD_ENCODER=mf` / `SARD_DECODER=mf` force Media Foundation on NVIDIA GPUs (to test
the AMD/Intel path).

Known limits: the secure desktop (UAC prompts, lock screen, Ctrl+Alt+Del) and
windows of elevated apps can't be captured or controlled unless Sard runs as
administrator; Desktop Duplication doesn't work over Remote Desktop or as a
service; rotated displays are streamed unrotated; HDR desktops are tone-mapped to
SDR. HEVC/AV1 decoding with Media Foundation needs Microsoft's free *HEVC Video
Extensions* / *AV1 Video Extension* from the Store.

## Roadmap

- [x] Linux and macOS hosts and viewers, audio, clipboard, multi-monitor switching
- [ ] Windows host and viewer (written, builds and passes CI; not yet tested on hardware)
- [x] Menu bar / tray app, pairing from the UI, start at login, self-update
- [x] AV1 by default when both ends have hardware for it, else HEVC
- [x] Connect Anywhere: hole punching, TURN relay fallback, invite-only accounts
- [x] Viewer-drawn cursor for macOS hosts
- [x] Bitrate control that tells Wi-Fi stalls from real congestion
- [x] iPhone app: screen control, fit-to-phone resolution, terminals
- [x] Remote terminals that survive reconnects (`sard shell`, the iPhone app)
- [ ] Fit-to-phone and terminals on Windows and Linux hosts (written, builds; not yet tested on hardware)
- [ ] Connect Anywhere paid plans
