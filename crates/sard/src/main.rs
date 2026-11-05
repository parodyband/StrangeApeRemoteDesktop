//! `sard` — StrangeApeRemoteDesktop command line.

mod args;
mod client;
mod host;
mod keymap;
mod platform;

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
  sard connect <host> [options]  View and control a paired host
  sard info                      Show identity, paired devices and GPU capabilities
  sard unpair <name>             Remove a trusted client (host side)

HOST OPTIONS:
  --pair                 Accept new pairings (prints a one-time code)
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

CLIENT OPTIONS:
  --fps <n>              Request a frame rate
  --bitrate <mbps>       Request a bitrate
  --codec <c>            Prefer hevc | h264 | av1
  --display <n>          Ask for a specific host display
  --fullscreen           Start fullscreen
  --no-swap-modifiers    Don't map Cmd⇄Ctrl between macOS and Linux
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
    let argv: Vec<String> = std::env::args().skip(1).collect();
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
    let (Some(host), Some(code)) = (args.positional(0), args.positional(1)) else {
        return Err("usage: sard pair <host> <code>".into());
    };
    let dir = config::config_dir();
    let identity = config::load_or_create_identity(&dir)?;
    let cfg = net_config(args)?;
    println!("Pairing with {host} ...");
    let (key, name, addr) = Client::pair(host, code, &identity, &cfg, Duration::from_secs(10))?;
    let mut known = KnownHosts::load(&dir)?;
    let stored = if addr.port() == sard_core::DEFAULT_PORT { addr.ip().to_string() } else { addr.to_string() };
    known.add(key, &stored, &name)?;
    println!("Paired with \"{name}\" ({}) at {stored}.", fingerprint(&key));
    println!("Connect with:  sard connect {host}");
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
    println!("\nvideo:");
    for line in platform::capabilities() {
        println!("  {line}");
    }
    Ok(())
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
