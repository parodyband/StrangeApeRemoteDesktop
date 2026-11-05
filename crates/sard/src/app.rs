//! `sard app` (also `sard` with no arguments on a desktop, and the macOS app
//! bundle): a menu-bar / system-tray app that shares this computer and opens
//! viewer windows to paired hosts, with quick switching between their screens.

use crate::args::Args;
use crate::client;
use crate::host::{HostService, HostStatus};
use crate::session::{AppEvent, Outcome, Session, SessionEvent, SessionOpts};
use crate::{Result, platform};
use sard_core::config::{self, Aliases, KnownHosts};
use sard_core::media::{MenuEntry, Rgba, Tray, TrayIcons};
use sard_core::net::Client;
use sard_core::proto::Control;
use sard_core::{error, info, warn};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::WindowId;

fn rgba(size: u32, bytes: &[u8]) -> Rgba {
    Rgba { width: size, height: size, pixels: bytes.to_vec() }
}

fn tray_icons() -> TrayIcons {
    TrayIcons {
        mono: rgba(44, include_bytes!("../assets/tray/mono.rgba")),
        mono_active: rgba(44, include_bytes!("../assets/tray/mono_active.rgba")),
        color: rgba(64, include_bytes!("../assets/tray/color.rgba")),
        color_active: rgba(64, include_bytes!("../assets/tray/color_active.rgba")),
    }
}

/// `settings.txt`: `key=value` lines.
struct Settings {
    share: bool,
}

impl Settings {
    fn path() -> std::path::PathBuf {
        config::config_dir().join("settings.txt")
    }

    fn load() -> Settings {
        let mut s = Settings { share: true };
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let on = matches!(v.trim(), "on" | "true" | "1" | "yes");
                    if k.trim() == "share" {
                        s.share = on;
                    }
                }
            }
        }
        s
    }

    fn save(&self) {
        let text = format!("# sard app settings\nshare={}\n", if self.share { "on" } else { "off" });
        if let Err(e) = config::write_private_file(&config::config_dir(), "settings.txt", &text) {
            warn!("cannot save settings: {e}");
        }
    }
}

/// Keep a second copy of the app from starting (it would fight over the port
/// and show two tray icons). The lock is released when the process exits.
fn single_instance() -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let dir = config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock"))?;
    // SAFETY: flock on a file descriptor we own.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok((r == 0).then_some(f))
}

pub fn run(args: &Args) -> Result<()> {
    let Some(_lock) = single_instance()? else {
        println!("Sard is already running (look for it in the menu bar / system tray).");
        return Ok(());
    };
    let event_loop = crate::event_loop()?;
    let mut app = App {
        args: args.clone(),
        proxy: event_loop.create_proxy(),
        tray: None,
        host: None,
        host_error: None,
        settings: Settings::load(),
        sessions: Vec::new(),
        connecting: Vec::new(),
        next_id: 1,
        actions: Vec::new(),
        action_base: 1,
        started: false,
        pairing_busy: false,
        next_refresh: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[derive(Clone, Debug)]
enum Action {
    Connect(String),
    Focus(u64),
    SelectDisplay(u64, u32),
    Fullscreen(u64),
    Disconnect(u64),
    PairHost,
    ToggleShare,
    AllowPairing,
    ShowPairing,
    KickViewer,
    ChooseScreens,
    ToggleLogin,
    Quit,
}

struct App {
    args: Args,
    proxy: EventLoopProxy<AppEvent>,
    tray: Option<Box<dyn Tray>>,
    host: Option<HostService>,
    host_error: Option<String>,
    settings: Settings,
    sessions: Vec<Session>,
    /// Connect attempts in flight: (session id, label, target).
    connecting: Vec<(u64, String, String)>,
    next_id: u64,
    /// Menu item id → action (ids are never reused, so stale clicks miss).
    actions: Vec<(u32, Action)>,
    action_base: u32,
    started: bool,
    pairing_busy: bool,
    /// Re-render the menu then (pairing-code countdown).
    next_refresh: Option<Instant>,
}

/// (session id, label, [(display id, name)], current display)
type SessionMenu = (u64, String, Vec<(u32, String)>, Option<u32>);

/// One paired host as shown in the menu.
struct HostEntry {
    label: String,
    target: String,
}

fn paired_hosts() -> Vec<HostEntry> {
    let dir = config::config_dir();
    let (Ok(known), Ok(aliases)) = (KnownHosts::load(&dir), Aliases::load(&dir)) else { return Vec::new() };
    known
        .hosts
        .iter()
        .map(|h| {
            let alias = aliases.entries.iter().find(|(_, addr)| {
                addr == &h.addr
                    || addr.eq_ignore_ascii_case(&h.name)
                    || known.find(addr).is_some_and(|k| k.key == h.key)
            });
            match alias {
                Some((a, _)) if !a.eq_ignore_ascii_case(&h.name) => {
                    HostEntry { label: format!("{a} ({})", h.name), target: a.clone() }
                }
                _ => HostEntry { label: h.name.clone(), target: h.addr.clone() },
            }
        })
        .collect()
}

impl App {
    fn start(&mut self, el: &ActiveEventLoop) {
        let proxy = self.proxy.clone();
        match platform::create_tray(
            tray_icons(),
            Box::new(move |id| {
                let _ = proxy.send_event(AppEvent::Menu(id));
            }),
        ) {
            Ok(t) => self.tray = Some(t),
            Err(e) => {
                error!("cannot create the tray icon: {e}");
                platform::show_message("Sard", &format!("Sard could not add its icon to the system tray:\n{e}"));
                el.exit();
                return;
            }
        }
        if self.settings.share {
            self.start_host();
        }
        platform::set_dock_visible(false);
        self.refresh();
    }

    fn start_host(&mut self) {
        let proxy = self.proxy.clone();
        let on_change = Box::new(move || {
            let _ = proxy.send_event(AppEvent::HostChanged);
        });
        match HostService::start(&self.args, false, on_change) {
            Ok(h) => {
                let st = h.status();
                info!("sharing as {} on port {}", st.name, st.port);
                for m in &st.missing_permissions {
                    warn!("permission needed — {m}");
                }
                self.host = Some(h);
                self.host_error = None;
            }
            Err(e) => {
                let msg = if e.to_string().contains("in use") {
                    format!("port in use — is `sard host` already running? ({e})")
                } else {
                    e.to_string()
                };
                error!("cannot share this computer: {msg}");
                self.host_error = Some(msg);
            }
        }
    }

    fn host_status(&self) -> Option<HostStatus> {
        self.host.as_ref().map(HostService::status)
    }

    fn action(&mut self, a: Action) -> u32 {
        let id = self.action_base;
        self.action_base = self.action_base.wrapping_add(1).max(1);
        self.actions.push((id, a));
        id
    }

    fn item(&mut self, label: impl Into<String>, a: Action) -> MenuEntry {
        MenuEntry::Item { id: self.action(a), label: label.into(), enabled: true, checked: None }
    }

    fn check(&mut self, label: impl Into<String>, on: bool, a: Action) -> MenuEntry {
        MenuEntry::Item { id: self.action(a), label: label.into(), enabled: true, checked: Some(on) }
    }

    fn text(label: impl Into<String>) -> MenuEntry {
        MenuEntry::Item { id: 0, label: label.into(), enabled: false, checked: None }
    }

    fn build_menu(&mut self) -> Vec<MenuEntry> {
        self.actions.clear();
        let mut m = Vec::new();
        let status = self.host_status();
        self.next_refresh = None;

        // ---- this computer ----
        match (&status, &self.host_error) {
            (Some(st), _) => {
                let ip = st.ips.first().map(|i| format!(" ({i})")).unwrap_or_default();
                m.push(Self::text(format!("Sharing as {}{ip}", st.name)));
                if let Some(v) = &st.viewer {
                    m.push(Self::text(format!("{v} is viewing this computer")));
                }
                for p in &st.missing_permissions {
                    m.push(Self::text(format!("⚠ {p}")));
                }
                if let Some((code, until)) = &st.pairing {
                    let mins = until.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
                    let label = format!("Pairing code: {code} ({mins} min left)");
                    m.push(self.item(label, Action::ShowPairing));
                    self.next_refresh = Some(Instant::now() + Duration::from_secs(30));
                }
            }
            (None, Some(e)) => m.push(Self::text(format!("Sharing unavailable: {e}"))),
            (None, None) => m.push(Self::text("Not sharing this computer")),
        }
        m.push(MenuEntry::Separator);

        // ---- open viewers ----
        for (_, label, _) in &self.connecting {
            m.push(Self::text(format!("Connecting to {label}…")));
        }
        let sessions: Vec<SessionMenu> = self
            .sessions
            .iter()
            .map(|s| {
                let (list, cur) = s.displays();
                let list = list
                    .iter()
                    .map(|d| {
                        let size = if d.width > 0 { format!(" — {}×{}", d.width, d.height) } else { String::new() };
                        (d.id, format!("{}{size}", d.name))
                    })
                    .collect();
                (s.id, s.label.clone(), list, cur)
            })
            .collect();
        for (sid, label, displays, current) in sessions {
            let header = self.item(format!("Viewing {label}"), Action::Focus(sid));
            m.push(header);
            if displays.len() > 1 {
                for (did, name) in displays {
                    let e =
                        self.check(format!("Screen: {name}"), current == Some(did), Action::SelectDisplay(sid, did));
                    m.push(e);
                }
            }
            let fs = self.item("Full Screen", Action::Fullscreen(sid));
            let dc = self.item(format!("Disconnect from {label}"), Action::Disconnect(sid));
            m.push(fs);
            m.push(dc);
            m.push(MenuEntry::Separator);
        }

        // ---- connect ----
        let hosts = paired_hosts();
        let mut sub = Vec::new();
        for h in hosts {
            let open = self.sessions.iter().find(|s| s.target == h.target).map(|s| s.id);
            let e = match open {
                Some(sid) => self.check(h.label, true, Action::Focus(sid)),
                None => self.item(h.label, Action::Connect(h.target)),
            };
            sub.push(e);
        }
        if !sub.is_empty() {
            sub.push(MenuEntry::Separator);
        }
        let pair_label = if self.pairing_busy { "Pairing…" } else { "Pair with a New Computer…" };
        let pair = self.item(pair_label, Action::PairHost);
        sub.push(pair);
        m.push(MenuEntry::Submenu { label: "Connect To".into(), enabled: true, items: sub });
        m.push(MenuEntry::Separator);

        // ---- sharing ----
        let share = self.check("Share This Computer", self.host.is_some(), Action::ToggleShare);
        m.push(share);
        if let Some(st) = &status {
            if let Some(v) = &st.viewer {
                let k = self.item(format!("Disconnect {v}"), Action::KickViewer);
                m.push(k);
            }
            let p = self.item("Allow a New Device to Pair…", Action::AllowPairing);
            m.push(p);
            if cfg!(target_os = "linux") {
                let c = self.item("Choose Screens to Share…", Action::ChooseScreens);
                m.push(c);
            }
        }
        m.push(MenuEntry::Separator);
        let login = self.check("Start at Login", platform::launch_at_login(), Action::ToggleLogin);
        m.push(login);
        m.push(Self::text(format!("Sard {}", env!("CARGO_PKG_VERSION"))));
        let quit = self.item("Quit Sard", Action::Quit);
        m.push(quit);
        m
    }

    fn refresh(&mut self) {
        let menu = self.build_menu();
        let status = self.host_status();
        let viewer = status.as_ref().and_then(|s| s.viewer.clone());
        let mut tip = Vec::new();
        if !self.sessions.is_empty() {
            let names: Vec<&str> = self.sessions.iter().map(|s| s.label.as_str()).collect();
            tip.push(format!("Viewing {}", names.join(", ")));
        }
        match (&status, viewer) {
            (Some(_), Some(v)) => tip.push(format!("{v} is viewing this computer")),
            (Some(st), None) => tip.push(format!("Sharing as {}", st.name)),
            (None, _) => tip.push("Not sharing".into()),
        }
        let active = !self.sessions.is_empty() || status.as_ref().is_some_and(|s| s.viewer.is_some());
        if let Some(t) = &self.tray {
            t.set_menu(menu);
            t.set_tooltip(&tip.join(" · "));
            t.set_active(active);
        }
        platform::set_dock_visible(!self.sessions.is_empty());
    }

    fn connect(&mut self, target: String) {
        if let Some(s) = self.sessions.iter().find(|s| s.target == target) {
            s.focus();
            return;
        }
        if self.connecting.iter().any(|(_, _, t)| *t == target) {
            return;
        }
        let (host, addr) = match client::resolve(&target) {
            Ok(x) => x,
            Err(e) => {
                platform::show_message("Sard", &e.to_string());
                return;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let label = host.name.clone();
        self.connecting.push((id, label.clone(), target));
        let (proxy, args) = (self.proxy.clone(), self.args.clone());
        std::thread::spawn(move || {
            let result = (|| -> Result<_> {
                let identity = config::load_or_create_identity(&config::config_dir())?;
                let cfg = crate::net_config(&args)?;
                Ok(Client::connect(&addr, host.key, identity, cfg, Duration::from_secs(8))?)
            })()
            .map_err(|e| e.to_string());
            let _ = proxy.send_event(AppEvent::Connected { id, label, result });
        });
        self.refresh();
    }

    fn pair_host(&mut self) {
        if self.pairing_busy {
            return;
        }
        self.pairing_busy = true;
        let (proxy, args) = (self.proxy.clone(), self.args.clone());
        std::thread::spawn(move || {
            let r = (|| -> Result<Option<String>> {
                let Some(addr) = platform::prompt_text(
                    "Pair with a computer",
                    "Address of the computer to connect to\n(shown in the Sard menu on that computer):",
                ) else {
                    return Ok(None);
                };
                let Some(code) = platform::prompt_text(
                    "Pair with a computer",
                    &format!(
                        "Pairing code for {addr}\n(on that computer choose “Allow a New Device to Pair…”, or run `sard host --pair`):"
                    ),
                ) else {
                    return Ok(None);
                };
                let cfg = crate::net_config(&args)?;
                let (name, _, stored) = crate::pair_and_save(addr.trim(), code.trim(), &cfg)?;
                info!("paired with {name} at {stored}");
                Ok(Some(stored))
            })();
            let _ = proxy.send_event(AppEvent::Paired(r.map_err(|e| e.to_string())));
        });
        self.refresh();
    }

    fn on_menu(&mut self, el: &ActiveEventLoop, id: u32) {
        let Some(a) = self.actions.iter().find(|(i, _)| *i == id).map(|(_, a)| a.clone()) else { return };
        info!("menu: {a:?}");
        match a {
            Action::Connect(t) => self.connect(t),
            Action::Focus(sid) => {
                if let Some(s) = self.sessions.iter().find(|s| s.id == sid) {
                    s.focus();
                }
            }
            Action::SelectDisplay(sid, did) => {
                if let Some(s) = self.sessions.iter().find(|s| s.id == sid) {
                    s.select_display(did);
                }
            }
            Action::Fullscreen(sid) => {
                if let Some(s) = self.sessions.iter().find(|s| s.id == sid) {
                    s.toggle_fullscreen();
                    s.focus();
                }
            }
            Action::Disconnect(sid) => {
                self.sessions.retain(|s| s.id != sid);
                self.refresh();
            }
            Action::PairHost => self.pair_host(),
            Action::ToggleShare => {
                if self.host.take().is_none() {
                    self.start_host();
                }
                self.settings.share = self.host.is_some();
                if self.host.is_none() {
                    self.host_error = None;
                }
                self.settings.save();
                self.refresh();
            }
            Action::AllowPairing => {
                if let Some(h) = &self.host {
                    h.new_pairing_code();
                    self.show_pairing();
                }
                self.refresh();
            }
            Action::ShowPairing => self.show_pairing(),
            Action::KickViewer => {
                if let Some(h) = &self.host {
                    h.disconnect_viewer();
                }
            }
            Action::ChooseScreens => {
                let _ = std::fs::remove_file(config::config_dir().join("portal_restore_token"));
                if let Some(h) = &self.host {
                    h.disconnect_viewer();
                }
                platform::show_message(
                    "Sard",
                    "The next time someone connects, this computer will ask which screens to share.",
                );
            }
            Action::ToggleLogin => {
                let on = !platform::launch_at_login();
                if let Err(e) = platform::set_launch_at_login(on) {
                    platform::show_message("Sard", &format!("Could not change Start at Login: {e}"));
                }
                self.refresh();
            }
            Action::Quit => {
                self.sessions.clear();
                self.host = None;
                self.tray = None;
                el.exit();
            }
        }
    }

    fn show_pairing(&self) {
        let Some(st) = self.host_status() else { return };
        let Some((code, _)) = st.pairing else { return };
        let addr = st.ips.first().cloned().unwrap_or_else(|| st.name.clone());
        platform::show_message(
            "Pair a new device",
            &format!(
                "On the other computer, open Sard → Connect To → Pair with a New Computer…\n\n\
                 Address:  {addr}\nCode:  {code}\n\n\
                 Or run:  sard pair {addr} {code}\n\nThe code works once, for 10 minutes."
            ),
        );
    }

    fn session_closed(&mut self, sid: u64, reason: &str) {
        if let Some(i) = self.sessions.iter().position(|s| s.id == sid) {
            let s = self.sessions.remove(i);
            info!("session with {} closed: {reason}", s.label);
            if reason.starts_with("Disconnected:") {
                platform::show_message("Sard", &format!("{}: {}", s.label, reason));
            }
        }
        self.refresh();
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if !self.started {
            self.started = true;
            self.start(el);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: AppEvent) {
        match ev {
            AppEvent::Menu(id) => self.on_menu(el, id),
            AppEvent::HostChanged | AppEvent::Refresh => self.refresh(),
            AppEvent::Paired(r) => {
                self.pairing_busy = false;
                match r {
                    Ok(Some(target)) => self.connect(target),
                    Ok(None) => self.refresh(),
                    Err(e) => {
                        platform::show_message("Pairing failed", &e);
                        self.refresh();
                    }
                }
            }
            AppEvent::Connected { id, label, result } => {
                let Some(i) = self.connecting.iter().position(|(cid, _, _)| *cid == id) else { return };
                let (_, _, target) = self.connecting.remove(i);
                match result {
                    Ok(conn) => {
                        match Session::open(el, conn, label.clone(), SessionOpts::defaults(), self.proxy.clone(), id) {
                            Ok(mut s) => {
                                s.target = target;
                                s.focus();
                                self.sessions.push(s);
                            }
                            Err(e) => platform::show_message("Sard", &format!("Cannot open a window for {label}: {e}")),
                        }
                    }
                    Err(e) => platform::show_message("Sard", &format!("Could not connect to {label}: {e}")),
                }
                self.refresh();
            }
            AppEvent::Session(sid, ev) => {
                let menu_change =
                    matches!(ev, SessionEvent::Control(Control::Displays { .. } | Control::StreamConfig(_)));
                let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) else { return };
                match s.on_event(el, ev) {
                    Outcome::Closed(r) => self.session_closed(sid, &r),
                    Outcome::Continue if menu_change => self.refresh(),
                    Outcome::Continue => {}
                }
            }
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, wid: WindowId, event: WindowEvent) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.window_id() == wid) else { return };
        if let Outcome::Closed(r) = s.window_event(event) {
            let sid = s.id;
            self.session_closed(sid, &r);
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        for s in &mut self.sessions {
            s.device_event(event.clone());
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        use winit::event_loop::ControlFlow;
        match self.next_refresh {
            Some(t) if Instant::now() >= t => self.refresh(),
            _ => {}
        }
        el.set_control_flow(match self.next_refresh {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}
