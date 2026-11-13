//! `sard shell <host>`: a terminal on a paired computer, in this terminal.
//!
//! Opens a session in the terminal lane (so it runs next to a screen
//! session from this device instead of replacing it) and relays keystrokes
//! and output. The shell keeps running on the host when the connection
//! drops; `--attach <id>` picks it up again. `Enter ~ .` disconnects without
//! ending the shell, like ssh.

use crate::args::Args;
use crate::client::{dial_lane, resolve};
use crate::{Result, console};
use sard_core::net::Event;
use sard_core::proto::{ClientHello, Control, Platform, ShellInfo, client_flags, lane};
use std::io::Write;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

fn describe(list: &[ShellInfo]) -> String {
    list.iter()
        .map(|s| {
            let age = match s.age_s {
                a if a < 120 => format!("{a} s"),
                a if a < 7200 => format!("{} min", a / 60),
                a => format!("{} h", a / 3600),
            };
            let state = if s.attached { "open elsewhere" } else { "detached" };
            format!("  {:>3}  {:<12} {age:>8}  {state}", s.id, s.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn run(args: &Args) -> Result<()> {
    let Some(arg) = args.positional(0) else {
        return Err("usage: sard shell <host | alias> [--list | --attach <id>]".into());
    };
    if !console::is_terminal() && !args.flag("list") {
        return Err("sard shell needs a terminal (stdin and stdout)".into());
    }
    let (host, addr) = resolve(arg)?;
    let conn = dial_lane(&host, &addr, args, lane::TERMINAL)?;
    let events = conn.take_events().ok_or("events taken")?;
    conn.send_control(&Control::ClientHello(ClientHello {
        platform: Platform::current(),
        decoders: 0,
        codec_pref: 0,
        max_width: 0,
        max_height: 0,
        max_fps: 0,
        bitrate_kbps: 0,
        flags: client_flags::TERMINAL,
        display: 0,
        fit_width: 0,
        fit_height: 0,
    }));

    // A host that knows terminals answers with its list of our shells.
    let deadline = Instant::now() + Duration::from_secs(6);
    let list = loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Control(Control::Shells { list })) => break list,
            Ok(Event::Disconnected(r)) => return Err(format!("{} disconnected: {r}", host.name).into()),
            Ok(_) => {}
            Err(_) => {
                return Err(format!("{} doesn't support terminals yet: update Sard on it first", host.name).into());
            }
        }
    };
    if args.flag("list") {
        if list.is_empty() {
            println!("No terminals running on {}.", host.name);
        } else {
            println!("Terminals on {}:\n{}", host.name, describe(&list));
            println!("Reattach with: sard shell {arg} --attach <id>");
        }
        conn.close();
        return Ok(());
    }
    let attach: u32 = args.parse_value("attach")?.unwrap_or(0);
    let detached: Vec<&ShellInfo> = list.iter().filter(|s| !s.attached).collect();
    if attach == 0 && !detached.is_empty() {
        eprintln!(
            "({} detached terminal{} on {}: `sard shell {arg} --list`)",
            detached.len(),
            if detached.len() == 1 { "" } else { "s" },
            host.name
        );
    }

    let (cols, rows) = console::size().unwrap_or((80, 24));
    conn.send_control(&Control::ShellOpen { token: 1, id: attach, cols, rows });
    let id = loop {
        match events.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::Control(Control::ShellOpened { token: 1, id, .. })) => break id,
            Ok(Event::Control(Control::ShellRefused { token: 1, reason })) => return Err(reason.into()),
            Ok(Event::Disconnected(r)) => return Err(format!("{} disconnected: {r}", host.name).into()),
            Ok(_) => {}
            Err(_) => return Err(format!("{} didn't start a shell", host.name).into()),
        }
    };

    let raw = console::RawMode::enable()?;
    {
        let c = conn.clone();
        console::on_resize(move |cols, rows| c.send_control(&Control::ShellResize { id, cols, rows }));
    }
    // Keyboard → shell. `Enter ~ .` detaches (the shell keeps running).
    let (detach_tx, detach_rx) = std::sync::mpsc::channel::<()>();
    {
        let c = conn.clone();
        std::thread::Builder::new().name("sard-stdin".into()).spawn(move || {
            let mut buf = [0u8; 4096];
            // 0: just after Enter, 1: after Enter ~, 2: anywhere else
            let mut state = 0u8;
            loop {
                let n = match console::read_input(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut out = Vec::with_capacity(n);
                for &b in &buf[..n] {
                    match (state, b) {
                        (0, b'~') => state = 1, // held back until we know
                        (1, b'.') => {
                            let _ = detach_tx.send(());
                            return;
                        }
                        (1, b'~') => {
                            out.push(b'~'); // "~~" sends one ~
                            state = 2;
                        }
                        (1, b) => {
                            out.extend_from_slice(&[b'~', b]);
                            state = if b == b'\r' || b == b'\n' { 0 } else { 2 };
                        }
                        (_, b'\r' | b'\n') => {
                            out.push(b);
                            state = 0;
                        }
                        (_, b) => {
                            out.push(b);
                            state = 2;
                        }
                    }
                }
                if !out.is_empty() {
                    c.send_control(&Control::ShellData { id, data: out });
                }
            }
        })?;
    }

    let mut stdout = std::io::stdout().lock();
    let outcome = loop {
        if detach_rx.try_recv().is_ok() {
            break format!("Disconnected. The terminal keeps running on {}: sard shell {arg} --attach {id}", host.name);
        }
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Control(Control::ShellData { id: i, data })) if i == id => {
                let _ = stdout.write_all(&data);
                let _ = stdout.flush();
                conn.send_control(&Control::ShellAck { id, count: 1 });
            }
            Ok(Event::Control(Control::ShellExit { id: i, message })) if i == id => break format!("Shell {message}."),
            Ok(Event::Disconnected(r)) => {
                break format!(
                    "Connection lost ({r}). The terminal keeps running on {}: sard shell {arg} --attach {id}",
                    host.name
                );
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break "Connection closed.".into(),
        }
    };
    drop(raw);
    conn.close();
    // Leave the cursor on a fresh line, then say what happened.
    eprintln!("\r\n{outcome}");
    // The stdin thread may still be blocked reading the terminal.
    std::process::exit(0);
}
