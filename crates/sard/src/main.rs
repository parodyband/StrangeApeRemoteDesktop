//! `sard` — StrangeApeRemoteDesktop command line.

mod args;
mod client;
mod clipsync;
mod host;
mod keymap;
mod platform;
mod update;

use args::Args;
use sard_core::config::{self, KnownHosts, TrustStore};
use sard_core::crypto::fingerprint;
use sard_core::net::{Client, NetConfig};
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
sard — low-latency GPU remote desktop (Linux ⇄ macOS)

USAGE:
  sard host [options]            Share this machine's screen
  sard pair <host> <code>        Pair with a host (one time; code shown by `sard host --pair`)
                                 add `--alias <name>` to name it at the same time
  sard connect <host> [options]  View and control a paired host
  sard alias <address> <name>    Name a host: then `sard connect <name>` (`sard alias` lists)
  sard unalias <name>            Remove an alias
  sard info                      Show identity, paired devices and GPU capabilities
  sard unpair <name>             Remove a trusted client (host side)
  sard update [--check]          Install the latest release (--check: only report it)

HOST OPTIONS:
  --pair                 Accept new pairings (prints a one-time code)
  --reselect             Ask again which screen to share (Linux: forget saved approval)
  --port <n>             UDP port (default 47800)
  --bind <addr>          Listen address (default 0.0.0.0)
  --codec <c>            Force hevc | h264 | av1
  --bitrate <mbps>       Cap bitrate (default: automatic from resolution and fps)
  --fps <n>              Cap frame rate (default: client's display refresh, max 240)
  --display <n>          Display to share (0 = ask / primary)
  --preset <1-7>         Encoder preset (1 = fastest, default)
  --yuv444               Full chroma 4:4:4 (sharper text, needs client support)
  --cursor <mode>        metadata (client draws cursor, default) | embedded
  --test-pattern [WxH]   Stream a synthetic pattern instead of the screen
  --no-audio             Don't stream audio

CLIENT OPTIONS:
  --fps <n>              Request a frame rate
  --bitrate <mbps>       Request a bitrate
  --codec <c>            Prefer hevc | h264 | av1
  --display <n>          Ask for a specific host display
  --fullscreen           Start fullscreen
  --view-only            Watch without sending keyboard/mouse input
  --no-audio             Don't play the host's audio
  --headless             No window: decode only and log stats (testing)
  --duration <secs>      Exit after this long (with --headless)
  --snapshot <file.png>  Save a decoded frame (with --headless)
  --audio                Also play audio (with --headless)
  --no-swap-modifiers    Don't map Cmd⇄Ctrl between macOS and Linux
  --no-auto-relative     Don't switch to relative mouse when the host hides its cursor
  --max-datagram <n>     UDP payload size (default 1400; use ~1200 over VPNs)

IN-SESSION HOTKEYS (Ctrl+Alt+Shift + key):
  F  toggle fullscreen     M  toggle relative mouse (games / 3D viewports)
  K  request keyframe      Q  disconnect

ENVIRONMENT:
  SARD_LOG=debug         More logging
  SARD_CONFIG_DIR=<dir>  Override config directory
";

fn main() -> ExitCode {
    sard_core::log::init();
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // Launched as an app bundle with no arguments (double-click, or macOS's
    // "Quit & Reopen" after granting a permission): act as a host.
    if argv.is_empty() && std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/")) {
        argv.push("host".into());
    }
    let Some((cmd, rest)) = argv.split_first() else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    let args = match Args::parse(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match cmd.as_str() {
        "host" | "serve" => host::run(&args),
        "connect" | "c" => client::run(&args),
        "pair" => pair(&args),
        "info" => info(),
        "unpair" => unpair(&args),
        "alias" => alias(&args),
        "unalias" => unalias(&args),
        "selftest-capture" => selftest_capture(&args),
        "update" | "release-sign" => update::cli(cmd, &args),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("sard {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}").into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn net_config(args: &Args) -> Result<NetConfig> {
    let mut cfg = NetConfig::default();
    if let Some(n) = args.value("name") {
        cfg.name = n.to_string();
    }
    if let Some(m) = args.parse_value::<usize>("max-datagram")? {
        if !(576..=1472).contains(&m) {
            return Err("--max-datagram must be between 576 and 1472".into());
        }
        cfg.max_datagram = m;
    }
    if let Some(p) = args.parse_value::<u32>("pace")? {
        cfg.pace_mbps = p;
    }
    Ok(cfg)
}

fn pair(args: &Args) -> Result<()> {
    let (Some(arg), Some(code)) = (args.positional(0), args.positional(1)) else {
        return Err("usage: sard pair <host | alias> <code> [--alias <name>]".into());
    };
    let dir = config::config_dir();
    let mut aliases = config::Aliases::load(&dir)?;
    let new_alias = args.value("alias");
    if let Some(a) = new_alias
        && !config::Aliases::valid_name(a)
    {
        return Err(format!("invalid alias '{a}' (letters, digits, '.', '_', '-'; must start with a letter)").into());
    }
    let host = aliases.resolve(arg).to_string();
    let host = host.as_str();
    let identity = config::load_or_create_identity(&dir)?;
    let cfg = net_config(args)?;
    println!("Pairing with {host} ...");
    let (key, name, addr) = Client::pair(host, code, &identity, &cfg, Duration::from_secs(10))?;
    let mut known = KnownHosts::load(&dir)?;
    let stored = if addr.port() == sard_core::DEFAULT_PORT { addr.ip().to_string() } else { addr.to_string() };
    known.add(key, &stored, &name)?;
    println!("Paired with \"{name}\" ({}) at {stored}.", fingerprint(&key));
    if let Some(a) = new_alias {
        aliases.set(a, host)?;
        println!("Connect with:  sard connect {a}");
    } else {
        println!("Connect with:  sard connect {arg}");
    }
    Ok(())
}

fn info() -> Result<()> {
    let dir = config::config_dir();
    let identity = config::load_or_create_identity(&dir)?;
    println!("sard {}  ({})", env!("CARGO_PKG_VERSION"), platform::describe());
    println!("config dir:   {}", dir.display());
    println!("this device:  {} ({})", config::hostname(), fingerprint(&identity.public()));
    let trust = TrustStore::load(&dir)?;
    println!("\ntrusted clients (may connect to this host):");
    if trust.peers.is_empty() {
        println!("  (none — run `sard host --pair`)");
    }
    for p in &trust.peers {
        println!("  {:<24} {}", p.name, fingerprint(&p.key));
    }
    let known = KnownHosts::load(&dir)?;
    println!("\npaired hosts (this device can connect to):");
    if known.hosts.is_empty() {
        println!("  (none — run `sard pair <host> <code>`)");
    }
    for h in &known.hosts {
        println!("  {:<24} {:<22} {}", h.name, h.addr, fingerprint(&h.key));
    }
    let aliases = config::Aliases::load(&dir)?;
    if !aliases.entries.is_empty() {
        println!("\naliases:");
        for (a, addr) in &aliases.entries {
            println!("  {a:<24} {addr}");
        }
    }
    println!("\nvideo:");
    for line in platform::capabilities() {
        println!("  {line}");
    }
    Ok(())
}

/// Hidden diagnostic: open the real screen capture and encode for a while.
fn selftest_capture(args: &Args) -> Result<()> {
    use sard_core::media::{CursorMode, SourceOptions};
    use std::time::Instant;
    let secs: u64 = args.positional(0).and_then(|s| s.parse().ok()).unwrap_or(10);
    let opts = SourceOptions {
        fps: args.parse_value("fps")?.unwrap_or(60),
        bitrate_kbps: 30_000,
        cursor: CursorMode::Metadata,
        test_pattern: args.flag("test-pattern"),
        ..Default::default()
    };
    let t0 = Instant::now();
    let (mut src, mut input) = platform::open_host(&opts)?;
    println!("opened in {:.1}s: {}", t0.elapsed().as_secs_f64(), src.describe());
    if args.flag("input-test") {
        input_roundtrip(src.as_mut(), input.as_mut())?;
    }
    let start = Instant::now();
    let (mut frames, mut bytes, mut keys, mut cursors) = (0u64, 0usize, 0u32, 0u32);
    let mut lat = sard_core::stats::Samples::default();
    while start.elapsed().as_secs() < secs {
        if let Some(f) = src.next_frame(std::time::Duration::from_millis(100))? {
            frames += 1;
            bytes += f.data.len();
            keys += f.keyframe as u32;
            lat.add((sard_core::clock::now_us() - f.capture_us) as u32);
        }
        if src.take_cursor().is_some() {
            cursors += 1;
        }
    }
    let l = lat.take();
    let dt = start.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {dt:.1}s ({:.1} fps), {:.1} KB/frame, {keys} keyframes, {cursors} cursor updates, capture->encoded p50 {:.2} ms p95 {:.2} ms max {:.2} ms",
        frames as f64 / dt,
        bytes as f64 / frames.max(1) as f64 / 1024.0,
        l.p50 as f64 / 1000.0,
        l.p95 as f64 / 1000.0,
        l.max as f64 / 1000.0
    );
    Ok(())
}

/// Inject absolute pointer moves and observe them in the capture's cursor
/// metadata: verifies coordinate mapping and measures input→screen latency.
fn input_roundtrip(
    src: &mut dyn sard_core::media::VideoSource,
    input: &mut dyn sard_core::media::InputSink,
) -> Result<()> {
    use sard_core::media::{InputEvent, VideoSource};
    use std::time::{Duration, Instant};
    fn wait_cursor(
        src: &mut dyn VideoSource,
        pred: &dyn Fn(f32, f32) -> bool,
        limit: Duration,
    ) -> Result<Option<(f32, f32)>> {
        let t = Instant::now();
        while t.elapsed() < limit {
            let _ = src.next_frame(Duration::from_millis(5))?;
            if let Some(c) = src.take_cursor()
                && c.visible
                && pred(c.x, c.y)
            {
                return Ok(Some((c.x, c.y)));
            }
        }
        Ok(None)
    }
    // Nudge to learn the current position, then restore it at the end.
    input.inject(&InputEvent::MoveRel { dx: 1.0, dy: 0.0 })?;
    let orig = wait_cursor(src, &|_, _| true, Duration::from_secs(2))?;
    println!("cursor at start: {orig:?}");
    let mut lat = Vec::new();
    for &(x, y) in &[(0.25f32, 0.5f32), (0.75, 0.25), (0.5, 0.75), (0.1, 0.1)] {
        let t = Instant::now();
        input.inject(&InputEvent::MoveAbs { x, y })?;
        match wait_cursor(src, &|cx, cy| (cx - x).abs() < 0.01 && (cy - y).abs() < 0.01, Duration::from_secs(1))? {
            Some(p) => {
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
                println!(
                    "  move to ({x:.2},{y:.2}) -> cursor ({:.3},{:.3}) in {:.2} ms",
                    p.0,
                    p.1,
                    lat.last().unwrap()
                );
            }
            None => println!("  move to ({x:.2},{y:.2}) -> NOT observed within 1 s"),
        }
    }
    if let Some((x, y)) = orig {
        input.inject(&InputEvent::MoveAbs { x, y })?;
    }
    if lat.len() < 4 {
        return Err("input round trip failed".into());
    }
    lat.sort_by(f64::total_cmp);
    println!("input→cursor metadata latency: median {:.2} ms, max {:.2} ms", lat[lat.len() / 2], lat[lat.len() - 1]);
    Ok(())
}

fn looks_like_address(s: &str) -> bool {
    s.parse::<std::net::IpAddr>().is_ok()
        || s.parse::<std::net::SocketAddr>().is_ok()
        || s.contains('.')
        || s.contains(':')
}

/// `sard alias` lists; `sard alias <address> <name>` sets.
fn alias(args: &Args) -> Result<()> {
    let dir = config::config_dir();
    let mut aliases = config::Aliases::load(&dir)?;
    match (args.positional(0), args.positional(1)) {
        (None, _) => {
            if aliases.entries.is_empty() {
                println!("No aliases. Add one with:  sard alias <address> <name>");
            }
            let known = KnownHosts::load(&dir)?;
            for (a, addr) in &aliases.entries {
                let paired = known.find(addr).map_or("not paired".to_string(), |h| format!("paired with {}", h.name));
                println!("  {a:<16} {addr:<24} ({paired})");
            }
            Ok(())
        }
        (Some(_), None) => Err("usage: sard alias <address> <name>   (or `sard alias` to list)".into()),
        (Some(x), Some(y)) => {
            // Accept either order: the address is the one that looks like one.
            let (addr, name) = if looks_like_address(y) && !looks_like_address(x) { (y, x) } else { (x, y) };
            if !config::Aliases::valid_name(name) {
                return Err(format!(
                    "invalid alias '{name}': use letters, digits, '.', '_' or '-', starting with a letter"
                )
                .into());
            }
            aliases.set(name, addr)?;
            println!("{name} → {addr}");
            if KnownHosts::load(&dir)?.find(addr).is_none() {
                println!("Not paired yet: run `sard host --pair` on {addr}, then `sard pair {name} <code>`.");
            } else {
                println!("Connect with:  sard connect {name}");
            }
            Ok(())
        }
    }
}

fn unalias(args: &Args) -> Result<()> {
    let Some(name) = args.positional(0) else { return Err("usage: sard unalias <name>".into()) };
    if config::Aliases::load(&config::config_dir())?.remove(name)? {
        println!("Removed alias {name}.");
        Ok(())
    } else {
        Err(format!("no alias named {name}").into())
    }
}

fn unpair(args: &Args) -> Result<()> {
    let Some(name) = args.positional(0) else {
        return Err("usage: sard unpair <name>".into());
    };
    let mut trust = TrustStore::load(&config::config_dir())?;
    if trust.remove_by_name(name)? {
        println!("Removed {name}.");
        Ok(())
    } else {
        Err(format!("no trusted client named {name}").into())
    }
}
