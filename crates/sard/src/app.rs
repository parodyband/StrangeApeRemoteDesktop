//! `sard app` (also `sard` with no arguments on a desktop, and the macOS app
//! bundle): a menu-bar / system-tray app that shares this computer and opens
//! viewer windows to paired hosts, with quick switching between their screens.

use crate::args::Args;
use crate::client;
use crate::host::{HostService, HostStatus};
use crate::session::{AppEvent, Outcome, Session, SessionEvent, SessionOpts};
use crate::{Result, platform};
use sard_core::config::{self, Aliases, KnownHosts, TrustStore};
use sard_core::media::{MenuEntry, Rgba, Tray, TrayIcons};
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

/// Show a message without blocking the event loop (macOS dialogs are modal
/// `osascript` runs that only return when dismissed).
fn notify(title: &str, message: &str) {
    let (t, m) = (title.to_string(), message.to_string());
    std::thread::spawn(move || platform::show_message(&t, &m));
}

/// `settings.txt`: `key=value` lines.
struct Settings {
    share: bool,
    /// An update the user chose "Skip This Version" for.
    skip_version: Option<String>,
}

impl Settings {
    fn path() -> std::path::PathBuf {
        config::config_dir().join("settings.txt")
    }

    fn load() -> Settings {
        let mut s = Settings { share: true, skip_version: None };
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let on = matches!(v.trim(), "on" | "true" | "1" | "yes");
                    match k.trim() {
                        "share" => s.share = on,
                        "skip_version" if !v.trim().is_empty() => s.skip_version = Some(v.trim().to_string()),
                        _ => {}
                    }
                }
            }
        }
        s
    }

    fn save(&self) {
        let mut text = format!("# sard app settings\nshare={}\n", if self.share { "on" } else { "off" });
        if let Some(v) = &self.skip_version {
            text += &format!("skip_version={v}\n");
        }
        if let Err(e) = config::write_private_file(&config::config_dir(), "settings.txt", &text) {
            warn!("cannot save settings: {e}");
        }
    }
}

/// Without a terminal (launcher, login item, app bundle), log to a file
/// instead of nowhere: Linux `~/.local/state/sard/app.log`, macOS
/// `~/Library/Logs/Sard/app.log`, Windows `%LOCALAPPDATA%\Sard\app.log`.
#[cfg(windows)]
fn log_to_file() {
    // Started from the desktop: close the console window Windows opened for us.
    if sard_windows::os::console_is_ours() {
        sard_windows::os::detach_console();
    }
    if sard_windows::os::stderr_is_console() {
        return;
    }
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from) else { return };
    let dir = base.join("Sard");
    let path = dir.join("app.log");
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 << 20) {
        let _ = std::fs::rename(&path, dir.join("app.log.1"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        sard_windows::os::redirect_stderr(f);
    }
}

#[cfg(unix)]
fn log_to_file() {
    // SAFETY: isatty on a standard descriptor.
    if unsafe { libc::isatty(2) } == 1 {
        return;
    }
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Logs/Sard")
    } else {
        std::env::var_os("XDG_STATE_HOME").map_or_else(|| home.join(".local/state"), Into::into).join("sard")
    };
    let path = dir.join("app.log");
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 << 20) {
        let _ = std::fs::rename(&path, dir.join("app.log.1"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::os::fd::AsRawFd;
        // SAFETY: duplicate our log file onto stderr; both descriptors are valid.
        unsafe {
            libc::dup2(f.as_raw_fd(), 2);
        }
    }
}

pub fn run(args: &Args) -> Result<()> {
    log_to_file();
    info!("Sard {} starting", env!("CARGO_PKG_VERSION"));
    crate::update::remove_replaced();
    // After an update, the new process starts while the old one is still exiting.
    let mut lock = crate::lock_file("app.lock")?;
    if lock.is_none() && std::env::var_os(crate::update::RESTARTED_ENV).is_some() {
        let deadline = Instant::now() + Duration::from_secs(15);
        while lock.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            lock = crate::lock_file("app.lock")?;
        }
    }
    let Some(_lock) = lock else {
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
        update: None,
        update_busy: false,
        update_installing: false,
        offered: Vec::new(),
        // First check shortly after start, then twice a day.
        next_update_check: Instant::now() + Duration::from_secs(15),
        missing_permissions: Vec::new(),
        next_permission_check: None,
        next_share_retry: None,
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
    CheckUpdates,
    InstallUpdate,
    OpenPermission(&'static str),
    SetUpAnywhere,
    AnywhereOff,
    AirDropSettings,
    ForgetHost([u8; 32], String),
    RemoveDevice([u8; 32], String),
    Quit,
}

const UPDATE_INTERVAL: Duration = Duration::from_secs(12 * 3600);

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
    /// A newer release, once a check found one.
    update: Option<crate::update::UpdateInfo>,
    update_busy: bool,
    update_installing: bool,
    /// Versions already offered in a dialog this run (background checks ask once).
    offered: Vec<String>,
    next_update_check: Instant,
    /// Host permissions not granted yet (macOS), re-checked while any are missing.
    missing_permissions: Vec<&'static str>,
    next_permission_check: Option<Instant>,
    /// Sharing is wanted but couldn't start (port busy...): try again then.
    next_share_retry: Option<Instant>,
}

/// (session id, label, [(display id, name)], current display)
/// (session id, label, [(display id, name)], current display, Wi-Fi stalls seen)
type SessionMenu = (u64, String, Vec<(u32, String)>, Option<u32>, bool);

/// Remove a paired host and any aliases pointing at it.
fn forget_host(key: &[u8; 32]) -> Result<()> {
    let dir = config::config_dir();
    if let Some(h) = KnownHosts::load(&dir)?.remove(key)? {
        Aliases::load(&dir)?.remove_for(&h)?;
        info!("forgot {} ({})", h.name, h.addr);
    }
    Ok(())
}

/// One paired host as shown in the menu.
struct HostEntry {
    key: [u8; 32],
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
                    HostEntry { key: h.key, label: format!("{a} ({})", h.name), target: a.clone() }
                }
                _ => HostEntry { key: h.key, label: h.name.clone(), target: h.addr.clone() },
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
                self.next_share_retry = None;
                self.check_permissions();
            }
            Err(e) => {
                let msg = if e.to_string().contains("in use") {
                    "another program is using Sard's port".to_string()
                } else if e.to_string().contains("already sharing") {
                    "Sard is already sharing from a terminal (`sard host`)".to_string()
                } else {
                    e.to_string()
                };
                if self.host_error.as_deref() != Some(msg.as_str()) {
                    error!("cannot share this computer: {e}");
                }
                self.host_error = Some(msg);
                // Keep trying: the other program may go away.
                self.next_share_retry = Some(Instant::now() + Duration::from_secs(5));
            }
        }
    }

    /// Re-check host permissions (granted in System Settings meanwhile?).
    fn check_permissions(&mut self) {
        let now = if self.host.is_some() { platform::missing_host_permissions() } else { Vec::new() };
        self.next_permission_check = (!now.is_empty()).then(|| Instant::now() + Duration::from_secs(2));
        if now != self.missing_permissions {
            self.missing_permissions = now;
            self.refresh();
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
                match &st.anywhere {
                    Some(a) if a.online => m.push(Self::text("Reachable from anywhere")),
                    Some(a) => {
                        let why = a.error.as_deref().unwrap_or("connecting…");
                        let why: String = why.chars().take(60).collect();
                        m.push(Self::text(format!("⚠ Connect Anywhere: {why}")));
                    }
                    None => {}
                }
                for p in self.missing_permissions.clone() {
                    let e = self.item(format!("⚠ Allow {p}…"), Action::OpenPermission(p));
                    m.push(e);
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
                (s.id, s.label.clone(), list, cur, s.wifi_stalls)
            })
            .collect();
        for (sid, label, displays, current, stalls) in sessions {
            let header = self.item(format!("Viewing {label}"), Action::Focus(sid));
            m.push(header);
            if stalls {
                let e = self.item("⚠ Wi-Fi Stalls: Turn Off AirDrop & Handoff…", Action::AirDropSettings);
                m.push(e);
            }
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
        let mut forget = Vec::new();
        for h in &hosts {
            let e = self.item(format!("{}…", h.label), Action::ForgetHost(h.key, h.label.clone()));
            forget.push(e);
        }
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
        if !forget.is_empty() {
            sub.push(MenuEntry::Submenu { label: "Forget".into(), enabled: true, items: forget });
        }
        m.push(MenuEntry::Submenu { label: "Connect To".into(), enabled: true, items: sub });
        m.push(MenuEntry::Separator);

        // ---- sharing ----
        let share = self.check("Share This Computer", self.settings.share, Action::ToggleShare);
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
        let devices = TrustStore::load(&config::config_dir()).map(|t| t.peers).unwrap_or_default();
        let mut allowed = Vec::new();
        for d in devices {
            let name = if d.name.is_empty() { sard_core::crypto::fingerprint(&d.key) } else { d.name.clone() };
            let e = self.item(format!("Remove {name}…"), Action::RemoveDevice(d.key, name));
            allowed.push(e);
        }
        if allowed.is_empty() {
            allowed.push(Self::text("No devices yet"));
        }
        m.push(MenuEntry::Submenu { label: "Allowed Devices".into(), enabled: true, items: allowed });
        m.push(MenuEntry::Separator);
        if sard_core::anywhere::Service::load(&config::config_dir()).is_some() {
            let change = self.item("Change Invite Code…", Action::SetUpAnywhere);
            let off = self.item("Turn Off on This Computer…", Action::AnywhereOff);
            m.push(MenuEntry::Submenu { label: "Connect Anywhere".into(), enabled: true, items: vec![change, off] });
        } else {
            let e = self.item("Connect Anywhere (Invite Only)…", Action::SetUpAnywhere);
            m.push(e);
        }
        let login = self.check("Start at Login", platform::launch_at_login(), Action::ToggleLogin);
        m.push(login);
        let upd = match (&self.update, self.update_busy) {
            (Some(u), true) if self.update_installing => MenuEntry::Item {
                id: 0,
                label: format!("Installing Sard {}…", u.version),
                enabled: false,
                checked: None,
            },
            (_, true) => {
                MenuEntry::Item { id: 0, label: "Checking for Updates…".into(), enabled: false, checked: None }
            }
            (Some(u), false) => {
                let label = format!("Install Sard {} and Restart", u.version);
                self.item(label, Action::InstallUpdate)
            }
            (None, false) => self.item("Check for Updates…", Action::CheckUpdates),
        };
        m.push(upd);
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
                notify("Sard", &e.to_string());
                return;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let label = host.name.clone();
        self.connecting.push((id, label.clone(), target));
        let (proxy, args) = (self.proxy.clone(), self.args.clone());
        std::thread::spawn(move || {
            let result = client::dial(&host, &addr, &args).map_err(|e| e.to_string());
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
                        "6-digit pairing code for {addr}\n(on that computer choose “Allow a New Device to Pair…”):"
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
                self.settings.share = !self.settings.share;
                self.settings.save();
                if self.settings.share {
                    self.start_host();
                } else {
                    self.host = None;
                    self.host_error = None;
                    self.next_share_retry = None;
                }
                self.check_permissions();
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
                notify("Sard", "The next time someone connects, this computer will ask which screens to share.");
            }
            Action::AirDropSettings => {
                // macOS 13+: System Settings → General → AirDrop & Handoff.
                std::thread::spawn(|| {
                    std::process::Command::new("/usr/bin/open")
                        .arg("x-apple.systempreferences:com.apple.AirDrop-Handoff-Settings.extension")
                        .status()
                });
            }
            Action::SetUpAnywhere => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let Some(token) = platform::prompt_text(
                        "Connect Anywhere",
                        "Reach your computers from any network, not just this one.\n\n\
                         Connect Anywhere is invite-only for now; paid plans are coming soon.\n\
                         Paste your invite code (the same one on each of your computers):",
                    ) else {
                        return;
                    };
                    if token.trim().is_empty() {
                        return;
                    }
                    match sard_core::anywhere::Service::save_token(&config::config_dir(), &token) {
                        Ok(()) => {
                            let _ = proxy.send_event(AppEvent::AnywhereChanged);
                        }
                        Err(e) => platform::show_message("Connect Anywhere", &e.to_string()),
                    }
                });
            }
            Action::AnywhereOff => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    if platform::confirm(
                        "Turn off Connect Anywhere?",
                        "This computer will only be reachable on its local network. \
                         You'll need your invite code to turn it back on.",
                        "Turn Off",
                    ) && sard_core::anywhere::Service::save_token(&config::config_dir(), "").is_ok()
                    {
                        let _ = proxy.send_event(AppEvent::AnywhereChanged);
                    }
                });
            }
            Action::ToggleLogin => {
                let on = !platform::launch_at_login();
                if let Err(e) = platform::set_launch_at_login(on) {
                    notify("Sard", &format!("Could not change Start at Login: {e}"));
                }
                self.refresh();
            }
            Action::OpenPermission(p) => platform::open_permission_settings(p),
            Action::ForgetHost(key, label) => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let msg = format!(
                        "This computer won't be able to connect to {label} any more until you pair them again."
                    );
                    if platform::confirm(&format!("Forget {label}?"), &msg, "Forget") {
                        let _ = proxy.send_event(AppEvent::ForgetHost(key));
                    }
                });
            }
            Action::RemoveDevice(key, name) => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let msg = format!(
                        "{name} won't be able to view or control this computer any more until you pair them again."
                    );
                    if platform::confirm(&format!("Remove {name}?"), &msg, "Remove") {
                        let _ = proxy.send_event(AppEvent::RemoveDevice(key));
                    }
                });
            }
            Action::CheckUpdates => self.check_updates(true),
            Action::InstallUpdate => self.install_update(),
            Action::Quit => {
                self.sessions.clear();
                self.host = None;
                self.tray = None;
                el.exit();
            }
        }
    }

    fn check_updates(&mut self, manual: bool) {
        if self.update_busy {
            return;
        }
        self.update_busy = manual; // quiet background checks don't change the menu
        self.next_update_check = Instant::now() + UPDATE_INTERVAL;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = crate::update::check().map_err(|e| e.to_string());
            let _ = proxy.send_event(AppEvent::UpdateChecked { manual, result });
        });
        if manual {
            self.refresh();
        }
    }

    fn install_update(&mut self) {
        let Some(info) = self.update.clone() else { return };
        if let Err(e) = crate::update::install_target() {
            notify(
                "Sard update",
                &format!("Sard {} is available, but this copy can't update itself: {e}", info.version),
            );
            return;
        }
        self.update_busy = true;
        self.update_installing = true;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            info!("installing Sard {}", info.version);
            let r = crate::update::install(&info).map_err(|e| e.to_string());
            let _ = proxy.send_event(AppEvent::UpdateInstalled(r));
        });
        self.refresh();
    }

    /// One dialog: Install and Restart / Later / Skip This Version.
    fn offer_update(&mut self, u: crate::update::UpdateInfo) {
        self.offered.push(u.version.clone());
        let installable = crate::update::install_target().map_err(|e| e.to_string());
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let current = env!("CARGO_PKG_VERSION");
            if let Err(e) = installable {
                platform::show_message(
                    "Sard Update",
                    &format!(
                        "Sard {} is available (you have {current}), but this copy can't update itself: {e}",
                        u.version
                    ),
                );
                return;
            }
            let notes = u.notes.as_deref().map(|n| format!("\n\nWhat's new: {n}")).unwrap_or_default();
            let msg = format!("Sard {} is available — you have {current}.{notes}", u.version);
            let choice = platform::choose("Sard Update", &msg, &["Install and Restart", "Later", "Skip This Version"]);
            let _ = proxy.send_event(AppEvent::UpdateChoice { version: u.version, choice });
        });
    }

    fn show_pairing(&self) {
        let Some(st) = self.host_status() else { return };
        let Some((code, _)) = st.pairing else { return };
        let addr = st.ips.first().cloned().unwrap_or_else(|| st.name.clone());
        notify(
            "Pair a new device",
            &format!(
                "On the other computer: Sard menu → Connect To → Pair with a New Computer…\n\n\
                 Address:  {addr}\nCode:  {code}\n\n\
                 The code works once, for 10 minutes.\n(Terminal: sard pair {addr} {})",
                code.replace(' ', "")
            ),
        );
    }

    fn session_closed(&mut self, sid: u64, reason: &str) {
        if let Some(i) = self.sessions.iter().position(|s| s.id == sid) {
            let s = self.sessions.remove(i);
            info!("session with {} closed: {reason}", s.label);
            if reason.starts_with("Disconnected:") {
                notify("Sard", &format!("{}: {}", s.label, reason));
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
            AppEvent::AnywhereChanged => {
                // The host service reads the configuration when it starts.
                if self.host.take().is_some() {
                    self.start_host();
                }
                self.refresh();
            }
            AppEvent::ForgetHost(key) => {
                if let Err(e) = forget_host(&key) {
                    notify("Sard", &format!("Could not forget that computer: {e}"));
                }
                self.sessions.retain(|s| s.host_key() != key);
                self.refresh();
            }
            AppEvent::RemoveDevice(key) => {
                let r = match &self.host {
                    Some(h) => h.remove_device(&key),
                    None => TrustStore::load(&config::config_dir())
                        .and_then(|mut t| t.remove(&key))
                        .map(|_| ())
                        .map_err(Into::into),
                };
                if let Err(e) = r {
                    notify("Sard", &format!("Could not remove that device: {e}"));
                }
                self.refresh();
            }
            AppEvent::UpdateChecked { manual, result } => {
                self.update_busy = false;
                match result {
                    Ok(Some(u)) => {
                        info!("update available: Sard {}", u.version);
                        let skipped = self.settings.skip_version.as_deref() == Some(u.version.as_str());
                        // Asked for, or new and not skipped: offer it right away.
                        if manual || (!skipped && !self.offered.contains(&u.version)) {
                            self.offer_update(u.clone());
                        }
                        self.update = (manual || !skipped).then_some(u);
                    }
                    Ok(None) if manual => notify("Sard", &format!("Sard {} is up to date.", env!("CARGO_PKG_VERSION"))),
                    Ok(None) => {}
                    Err(e) if manual => {
                        warn!("update check failed: {e}");
                        notify("Sard update", &format!("Could not check for updates: {e}"))
                    }
                    Err(e) => warn!("update check failed: {e}"),
                }
                self.refresh();
            }
            AppEvent::UpdateChoice { version, choice } => {
                let current = self.update.as_ref().is_some_and(|u| u.version == version);
                match choice {
                    Some(0) if current => self.install_update(),
                    Some(2) => {
                        info!("skipping Sard {version}");
                        self.settings.skip_version = Some(version);
                        self.settings.save();
                        if current {
                            self.update = None;
                        }
                    }
                    _ => {}
                }
                self.refresh();
            }
            AppEvent::UpdateInstalled(r) => {
                self.update_busy = false;
                self.update_installing = false;
                match r {
                    Ok(()) => {
                        info!("update installed; restarting");
                        self.sessions.clear();
                        self.host = None;
                        self.tray = None;
                        crate::update::restart();
                    }
                    Err(e) => {
                        notify("Sard update", &format!("The update could not be installed: {e}"));
                        self.refresh();
                    }
                }
            }
            AppEvent::Paired(r) => {
                self.pairing_busy = false;
                match r {
                    Ok(Some(target)) => self.connect(target),
                    Ok(None) => self.refresh(),
                    Err(e) => {
                        notify("Pairing failed", &e);
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
                            Err(e) => notify("Sard", &format!("Cannot open a window for {label}: {e}")),
                        }
                    }
                    Err(e) => notify("Sard", &format!("Could not connect to {label}: {e}")),
                }
                self.refresh();
            }
            AppEvent::Session(sid, ev) => {
                let menu_change = matches!(
                    ev,
                    SessionEvent::Control(Control::Displays { .. } | Control::StreamConfig(_))
                        | SessionEvent::WifiStalls
                );
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
        if !self.started {
            return;
        }
        match self.next_refresh {
            Some(t) if Instant::now() >= t => self.refresh(),
            _ => {}
        }
        if Instant::now() >= self.next_update_check {
            self.check_updates(false);
        }
        if self.next_permission_check.is_some_and(|t| Instant::now() >= t) {
            self.check_permissions();
        }
        if self.next_share_retry.is_some_and(|t| Instant::now() >= t) {
            self.next_share_retry = None;
            if self.settings.share && self.host.is_none() {
                self.start_host();
                self.refresh();
            }
        }
        let mut wake = self.next_refresh.map_or(self.next_update_check, |t| t.min(self.next_update_check));
        for t in [self.next_permission_check, self.next_share_retry].into_iter().flatten() {
            wake = wake.min(t);
        }
        el.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}
