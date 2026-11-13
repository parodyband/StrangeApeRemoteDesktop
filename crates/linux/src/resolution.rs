//! "Fit to phone": list a monitor's modes and switch between them, so the
//! capture source can lower the shared monitor's resolution while a phone
//! watches and put it back afterwards.
//!
//! - KDE Plasma: `kscreen-doctor -j` (JSON) and `kscreen-doctor output.<name>.mode.<id>`.
//! - GNOME: `org.gnome.Mutter.DisplayConfig` (`GetCurrentState`, then
//!   `ApplyMonitorsConfig` with method 1, temporary).
//! - Other X11 desktops: `xrandr`.
//!
//! The parsers are pure functions with unit tests; errors are short,
//! user-facing text (they are shown on the phone).

use crate::dbus::{Connection, Value, dict};
use sard_core::media::DisplayMode;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Kde,
    Gnome,
    X11,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    /// Backend's mode id (KScreen mode id, Mutter mode id, "<xrandr mode>@<rate>").
    pub id: String,
    pub mode: DisplayMode,
    /// Size in the desktop's layout coordinates (portal stream / input space).
    pub layout: (i32, i32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// Connector name ("DP-1").
    pub name: String,
    /// Top-left corner in layout coordinates (matches the portal stream's position).
    pub position: (i32, i32),
    pub modes: Vec<Mode>,
    /// Index of the current mode in `modes`.
    pub current: usize,
}

impl Monitor {
    pub fn current_mode(&self) -> &Mode {
        &self.modes[self.current]
    }
}

/// Which mechanism this desktop offers.
pub fn desktop() -> Result<Desktop> {
    let name = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let up = name.to_ascii_uppercase();
    if up.contains("KDE") || (up.is_empty() && have("kscreen-doctor")) {
        Ok(Desktop::Kde)
    } else if up.contains("GNOME") {
        Ok(Desktop::Gnome)
    } else if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_some() {
        Ok(Desktop::X11)
    } else {
        let what = if name.is_empty() { "this desktop".to_string() } else { name };
        Err(format!("changing the resolution is supported on KDE Plasma and GNOME, not on {what}"))
    }
}

/// The enabled monitors and their modes.
pub fn monitors(desk: Desktop) -> Result<Vec<Monitor>> {
    match desk {
        Desktop::Kde => parse_kscreen(&run("kscreen-doctor", &["-j"], KSCREEN_HINT)?),
        Desktop::Gnome => Ok(mutter_state()?.monitors()),
        Desktop::X11 => Ok(parse_xrandr(&run("xrandr", &["--query"], XRANDR_HINT)?)),
    }
}

/// The monitor at `position` (a portal stream's position); if nothing is
/// there but only one monitor is on, that one.
pub fn find(mons: &[Monitor], position: (i32, i32)) -> Option<&Monitor> {
    mons.iter().find(|m| m.position == position).or(if mons.len() == 1 { mons.first() } else { None })
}

/// Switch monitor `name` to `mode`. With `wait`, poll until the desktop
/// reports the new mode (up to a few seconds).
pub fn set_mode(desk: Desktop, name: &str, mode: &Mode, wait: bool) -> Result<()> {
    match desk {
        Desktop::Kde => {
            // Mode ids are plain numbers; "WxH@Hz" is the fallback kscreen-doctor also accepts.
            let id = if mode.id.contains('.') {
                format!("{}x{}@{}", mode.mode.pixel_width, mode.mode.pixel_height, mode.mode.refresh.round())
            } else {
                mode.id.clone()
            };
            // Its exit status is 0 even when the change fails: the poll below checks.
            run("kscreen-doctor", &[&format!("output.{name}.mode.{id}")], KSCREEN_HINT)?;
        }
        Desktop::Gnome => {
            let args = mutter_state()?.apply_args(name, &mode.id)?;
            let conn = Connection::session()?;
            conn.call(MUTTER, MUTTER_PATH, MUTTER, "ApplyMonitorsConfig", &args)
                .map_err(|e| format!("GNOME refused the new mode: {}", e.rsplit(": ").next().unwrap_or(&e)))?;
        }
        Desktop::X11 => {
            let (xmode, rate) = mode.id.rsplit_once('@').unwrap_or((&mode.id, ""));
            let mut args = vec!["--output", name, "--mode", xmode];
            if !rate.is_empty() {
                args.extend(["--rate", rate]);
            }
            run("xrandr", &args, XRANDR_HINT)?;
        }
    }
    if !wait {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let applied = monitors(desk)
            .ok()
            .and_then(|mons| mons.into_iter().find(|m| m.name == name))
            .is_some_and(|m| same_mode(m.current_mode(), mode));
        if applied {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("the desktop didn't switch to {}", mode.mode.describe()));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn same_mode(a: &Mode, b: &Mode) -> bool {
    a.id == b.id
        || ((a.mode.pixel_width, a.mode.pixel_height) == (b.mode.pixel_width, b.mode.pixel_height)
            && (a.mode.refresh - b.mode.refresh).abs() < 0.5)
}

/// How to put `mode` back by hand (logged in case Sard dies before restoring).
pub fn restore_hint(desk: Desktop, name: &str, mode: &Mode) -> String {
    let m = &mode.mode;
    match desk {
        Desktop::Kde => {
            format!("kscreen-doctor output.{name}.mode.{}x{}@{}", m.pixel_width, m.pixel_height, m.refresh.round())
        }
        Desktop::Gnome => {
            format!("Settings → Displays → {name}: {}×{} @ {:.2} Hz", m.pixel_width, m.pixel_height, m.refresh)
        }
        Desktop::X11 => {
            let (xmode, rate) = mode.id.rsplit_once('@').unwrap_or((&mode.id, ""));
            format!("xrandr --output {name} --mode {xmode} --rate {rate}")
        }
    }
}

/// Desktop size for a mode of `px` pixels at `scale`, rotated by 90° or not.
fn display_mode(px: (u32, u32), scale: f64, rotated: bool, refresh: f64) -> DisplayMode {
    let (pw, ph) = if rotated { (px.1, px.0) } else { px };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    DisplayMode {
        width: (pw as f64 / scale).round() as u32,
        height: (ph as f64 / scale).round() as u32,
        pixel_width: pw,
        pixel_height: ph,
        refresh,
    }
}

// ---------------------------------------------------------------------------
// Running tools
// ---------------------------------------------------------------------------

const KSCREEN_HINT: &str = "install the kscreen package";
const XRANDR_HINT: &str = "install xrandr (x11-xserver-utils)";

fn have(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

/// Run a tool and return its stdout; gives up after 10 s.
fn run(prog: &str, args: &[&str], install: &str) -> Result<String> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{prog} not found ({install})"),
            _ => format!("{prog}: {e}"),
        })?;
    // Read both pipes on threads so a large output can't block the tool.
    let drain = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut v);
            }
            String::from_utf8_lossy(&v).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    match status {
        None => Err(format!("{prog} did not answer")),
        Some(s) if !s.success() => {
            let why = err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("no details");
            Err(format!("{prog} failed: {why}"))
        }
        Some(_) => Ok(out),
    }
}

// ---------------------------------------------------------------------------
// KDE: kscreen-doctor -j
// ---------------------------------------------------------------------------

/// Parse `kscreen-doctor -j` (libkscreen's ConfigSerializer): enabled,
/// connected outputs with their modes. `pos` is in logical coordinates;
/// logical size = mode pixels / `scale` (rotation 2 = left, 8 = right swap
/// width and height).
pub fn parse_kscreen(text: &str) -> Result<Vec<Monitor>> {
    let bad = || "unexpected output from kscreen-doctor -j".to_string();
    // Qt may print warnings before the JSON.
    let start = text.find('{').ok_or_else(bad)?;
    let doc = Json::parse(&text[start..]).map_err(|_| bad())?;
    let outputs = doc.get("outputs").ok_or_else(bad)?;
    let mut mons = Vec::new();
    for o in outputs.items() {
        if o.get("connected").and_then(Json::as_bool) == Some(false)
            || o.get("enabled").and_then(Json::as_bool) == Some(false)
        {
            continue;
        }
        let Some(name) = o.get("name").and_then(Json::as_str) else { continue };
        let pos = o.get("pos");
        let coord = |k: &str| pos.and_then(|p| p.get(k)).and_then(Json::as_f64).unwrap_or(0.0).round() as i32;
        let scale = o.get("scale").and_then(Json::as_f64).unwrap_or(1.0);
        let rotation = o.get("rotation").and_then(Json::as_f64).unwrap_or(1.0) as u32;
        let rotated = rotation == 2 || rotation == 8;
        let current_id = o.get("currentModeId").and_then(Json::as_id);
        let mut modes = Vec::new();
        let mut current = None;
        for m in o.get("modes").map(Json::items).unwrap_or(&[]) {
            let (Some(id), Some(size)) = (m.get("id").and_then(Json::as_id), m.get("size")) else { continue };
            let dim = |k: &str| size.get(k).and_then(Json::as_f64).unwrap_or(0.0) as u32;
            let (w, h) = (dim("width"), dim("height"));
            if w == 0 || h == 0 {
                continue;
            }
            let refresh = m.get("refreshRate").and_then(Json::as_f64).unwrap_or(0.0);
            let mode = display_mode((w, h), scale, rotated, refresh);
            if current_id.as_deref() == Some(id.as_str()) {
                current = Some(modes.len());
            }
            modes.push(Mode { layout: (mode.width as i32, mode.height as i32), id, mode });
        }
        let Some(current) = current else { continue };
        mons.push(Monitor { name: name.to_string(), position: (coord("x"), coord("y")), modes, current });
    }
    Ok(mons)
}

/// Just enough JSON for kscreen-doctor.
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(s: &str) -> std::result::Result<Json, ()> {
        let mut p = JsonParser { s: s.as_bytes(), i: 0 };
        p.value(0)
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn items(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            _ => &[],
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// An id that may be serialized as a string or a number.
    fn as_id(&self) -> Option<String> {
        match self {
            Json::Str(s) if !s.is_empty() => Some(s.clone()),
            Json::Num(n) => Some(format!("{n}")),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> std::result::Result<(), ()> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(())
        }
    }

    fn value(&mut self, depth: u32) -> std::result::Result<Json, ()> {
        if depth > 64 {
            return Err(());
        }
        self.ws();
        match *self.s.get(self.i).ok_or(())? {
            b'{' => {
                self.i += 1;
                let mut kv = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(kv));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    self.eat(":")?;
                    kv.push((k, self.value(depth + 1)?));
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(kv));
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(v));
                }
                loop {
                    v.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(v));
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.eat("true").map(|_| Json::Bool(true)),
            b'f' => self.eat("false").map(|_| Json::Bool(false)),
            b'n' => self.eat("null").map(|_| Json::Null),
            _ => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                let n = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| ())?;
                n.parse().map(Json::Num).map_err(|_| ())
            }
        }
    }

    fn string(&mut self) -> std::result::Result<String, ()> {
        self.eat("\"")?;
        let mut out = Vec::new();
        loop {
            let c = *self.s.get(self.i).ok_or(())?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| ()),
                b'\\' => {
                    let e = *self.s.get(self.i).ok_or(())?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hex = self.s.get(self.i..self.i + 4).ok_or(())?;
                            self.i += 4;
                            let code =
                                u32::from_str_radix(std::str::from_utf8(hex).map_err(|_| ())?, 16).map_err(|_| ())?;
                            // Surrogate pairs don't occur in output names; replace them.
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        _ => return Err(()),
                    };
                    let mut b = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                }
                c => out.push(c),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// X11: xrandr
// ---------------------------------------------------------------------------

/// Parse `xrandr --query`: connected outputs that are on, with their modes
/// (`*` marks the current one). Mode ids are "<mode name>@<rate>".
pub fn parse_xrandr(text: &str) -> Vec<Monitor> {
    let mut mons: Vec<Monitor> = Vec::new();
    let mut cur: Option<(Monitor, bool, Option<usize>)> = None;
    let flush = |cur: Option<(Monitor, bool, Option<usize>)>, mons: &mut Vec<Monitor>| {
        if let Some((mut m, _, Some(c))) = cur {
            m.current = c;
            mons.push(m);
        }
    };
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            flush(cur.take(), &mut mons);
            let t: Vec<&str> = line.split_whitespace().collect();
            if t.get(1) != Some(&"connected") {
                continue;
            }
            // "DP-2 connected primary 2560x1440+1920+0 left (normal left ...) 597mm x 336mm"
            let Some(g) = t.iter().position(|s| parse_geometry(s).is_some()) else { continue };
            let (_, pos) = parse_geometry(t[g]).unwrap();
            let rotated = matches!(t.get(g + 1), Some(&"left") | Some(&"right"));
            let mon = Monitor { name: t[0].to_string(), position: pos, modes: Vec::new(), current: 0 };
            cur = Some((mon, rotated, None));
        } else if let Some((mon, rotated, current)) = cur.as_mut() {
            // "   2560x1440     59.95 +  143.97*"
            let mut t = line.split_whitespace();
            let Some(name) = t.next() else { continue };
            // Interlaced modes ("1920x1080i") would only make text worse.
            let Some(px) = parse_size(name).filter(|_| !name.ends_with('i')) else { continue };
            for tok in t {
                let rate = tok.trim_end_matches(['*', '+']);
                if rate.is_empty() {
                    // A flag separated from its rate ("59.95 +"); '*' never is, but be lenient.
                    if tok.contains('*') {
                        *current = mon.modes.len().checked_sub(1);
                    }
                    continue;
                }
                let Ok(hz) = rate.parse::<f64>() else { continue };
                if tok.contains('*') {
                    *current = Some(mon.modes.len());
                }
                let mode = display_mode(px, 1.0, *rotated, hz);
                let layout = (mode.width as i32, mode.height as i32);
                mon.modes.push(Mode { id: format!("{name}@{rate}"), mode, layout });
            }
        }
    }
    flush(cur, &mut mons);
    mons
}

/// "2560x1440" (or "1920x1080i") → pixels.
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let h = h.trim_end_matches(|c: char| !c.is_ascii_digit());
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// "2560x1440+1920+0" → (size, position).
fn parse_geometry(s: &str) -> Option<((u32, u32), (i32, i32))> {
    let (size, rest) = s.split_once('+')?;
    let (x, y) = rest.split_once('+')?;
    Some((parse_size(size)?, (x.parse().ok()?, y.parse().ok()?)))
}

// ---------------------------------------------------------------------------
// GNOME: org.gnome.Mutter.DisplayConfig
// ---------------------------------------------------------------------------

const MUTTER: &str = "org.gnome.Mutter.DisplayConfig";
const MUTTER_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

fn mutter_state() -> Result<MutterState> {
    let conn = Connection::session()?;
    let reply = conn
        .call(MUTTER, MUTTER_PATH, MUTTER, "GetCurrentState", &[])
        .map_err(|_| "GNOME's display settings aren't reachable (org.gnome.Mutter.DisplayConfig)".to_string())?;
    MutterState::parse(&reply.body)
}

#[derive(Clone, Debug, PartialEq)]
struct MutterMode {
    id: String,
    size: (u32, u32),
    refresh: f64,
    scales: Vec<f64>,
    current: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct MutterMonitor {
    connector: String,
    modes: Vec<MutterMode>,
}

#[derive(Clone, Debug, PartialEq)]
struct MutterLogical {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connectors: Vec<String>,
}

/// `GetCurrentState`: (u serial, a((ssss)a(siiddada{sv})a{sv}) monitors,
/// a(iiduba(ssss)a{sv}) logical_monitors, a{sv} properties).
#[derive(Clone, Debug, PartialEq)]
struct MutterState {
    serial: u32,
    monitors: Vec<MutterMonitor>,
    logical: Vec<MutterLogical>,
    /// 1 = logical (layout in logical pixels), 2 = physical (layout in pixels).
    layout_mode: u32,
    can_change_layout_mode: bool,
}

fn as_f64(v: &Value) -> Option<f64> {
    match v.unvariant() {
        Value::F64(x) => Some(*x),
        Value::I32(x) => Some(*x as f64),
        Value::U32(x) => Some(*x as f64),
        _ => None,
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v.unvariant() {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

impl MutterState {
    fn parse(body: &[Value]) -> Result<MutterState> {
        let bad = || "unexpected reply from GNOME's display settings".to_string();
        let [serial, monitors, logical, props] = body else { return Err(bad()) };
        let mut st = MutterState {
            serial: serial.as_u32().ok_or_else(bad)?,
            monitors: Vec::new(),
            logical: Vec::new(),
            layout_mode: props.get("layout-mode").and_then(Value::as_u32).unwrap_or(1),
            can_change_layout_mode: props.get("supports-changing-layout-mode").and_then(as_bool).unwrap_or(false),
        };
        for m in monitors.items() {
            let [spec, modes, ..] = m.items() else { return Err(bad()) };
            let connector = spec.items().first().and_then(Value::as_str).ok_or_else(bad)?.to_string();
            let mut list = Vec::new();
            for md in modes.items() {
                let f = md.items();
                let (Some(id), Some(w), Some(h)) = (
                    f.first().and_then(Value::as_str),
                    f.get(1).and_then(Value::as_i32),
                    f.get(2).and_then(Value::as_i32),
                ) else {
                    return Err(bad());
                };
                list.push(MutterMode {
                    id: id.to_string(),
                    size: (w.max(0) as u32, h.max(0) as u32),
                    refresh: f.get(3).and_then(as_f64).unwrap_or(0.0),
                    scales: f.get(5).map(|s| s.items().iter().filter_map(as_f64).collect()).unwrap_or_default(),
                    current: f.get(6).and_then(|p| p.get("is-current")).and_then(as_bool).unwrap_or(false),
                });
            }
            st.monitors.push(MutterMonitor { connector, modes: list });
        }
        for l in logical.items() {
            let f = l.items();
            let (Some(x), Some(y), Some(scale)) =
                (f.first().and_then(Value::as_i32), f.get(1).and_then(Value::as_i32), f.get(2).and_then(as_f64))
            else {
                return Err(bad());
            };
            st.logical.push(MutterLogical {
                x,
                y,
                scale,
                transform: f.get(3).and_then(Value::as_u32).unwrap_or(0),
                primary: f.get(4).and_then(as_bool).unwrap_or(false),
                connectors: f
                    .get(5)
                    .map(|m| m.items().iter().filter_map(|s| s.items().first()?.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            });
        }
        Ok(st)
    }

    fn monitor(&self, connector: &str) -> Option<&MutterMonitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }

    /// Layout size of `mode` in logical monitor `l`.
    fn layout_size(&self, l: &MutterLogical, mode: &MutterMode) -> (i32, i32) {
        let scale = if self.layout_mode == 1 { l.scale } else { 1.0 };
        let m = display_mode(mode.size, scale, l.transform % 2 == 1, mode.refresh);
        (m.width as i32, m.height as i32)
    }

    /// One `Monitor` per logical monitor (its first connector), listing the
    /// modes that support the logical monitor's current scale.
    fn monitors(&self) -> Vec<Monitor> {
        let mut out = Vec::new();
        for l in &self.logical {
            let Some(mon) = l.connectors.first().and_then(|c| self.monitor(c)) else { continue };
            let mut modes = Vec::new();
            let mut current = None;
            for md in &mon.modes {
                let scale_ok = md.scales.is_empty() || md.scales.iter().any(|s| (s - l.scale).abs() < 0.01);
                if !scale_ok && !md.current {
                    continue;
                }
                if md.current {
                    current = Some(modes.len());
                }
                let mode = display_mode(md.size, l.scale, l.transform % 2 == 1, md.refresh);
                modes.push(Mode { id: md.id.clone(), mode, layout: self.layout_size(l, md) });
            }
            let Some(current) = current else { continue };
            out.push(Monitor { name: mon.connector.clone(), position: (l.x, l.y), modes, current });
        }
        out
    }

    /// `ApplyMonitorsConfig` arguments that keep everything as it is except
    /// `connector`'s mode. Monitors right of / below the changed one move by
    /// its change in size, so the layout stays adjacent (Mutter rejects gaps).
    fn apply_args(&self, connector: &str, mode_id: &str) -> Result<Vec<Value>> {
        let gone = || format!("{connector} is no longer on");
        let target = self.logical.iter().find(|l| l.connectors.iter().any(|c| c == connector)).ok_or_else(gone)?;
        let mon = self.monitor(connector).ok_or_else(gone)?;
        let old = mon.modes.iter().find(|m| m.current).ok_or_else(gone)?;
        let new = mon.modes.iter().find(|m| m.id == mode_id).ok_or_else(|| format!("{connector} has no such mode"))?;
        let (ow, oh) = self.layout_size(target, old);
        let (nw, nh) = self.layout_size(target, new);
        let mut logical = Vec::new();
        for l in &self.logical {
            let (mut x, mut y) = (l.x, l.y);
            if l != target {
                if x >= target.x + ow {
                    x += nw - ow;
                }
                if y >= target.y + oh {
                    y += nh - oh;
                }
            }
            let mut mons = Vec::new();
            for c in &l.connectors {
                let id = if c == connector {
                    mode_id.to_string()
                } else {
                    let m = self.monitor(c).and_then(|m| m.modes.iter().find(|m| m.current));
                    m.ok_or_else(|| format!("{c} has no current mode"))?.id.clone()
                };
                mons.push(Value::Struct(vec![Value::Str(c.clone()), Value::Str(id), dict(vec![])]));
            }
            logical.push(Value::Struct(vec![
                Value::I32(x),
                Value::I32(y),
                Value::F64(l.scale),
                Value::U32(l.transform),
                Value::Bool(l.primary),
                Value::Array("(ssa{sv})".into(), mons),
            ]));
        }
        let props = if self.can_change_layout_mode {
            dict(vec![("layout-mode", Value::U32(self.layout_mode))])
        } else {
            dict(vec![])
        };
        // Method 1: temporary (not saved to monitors.xml, no confirmation dialog).
        Ok(vec![Value::U32(self.serial), Value::U32(1), Value::Array("(iiduba(ssa{sv}))".into(), logical), props])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sard_core::media::pick_mode;

    /// `kscreen-doctor -j` on Plasma 6 (QJsonDocument::Indented sorts keys):
    /// a 5120x1440 ultrawide at 1.25 and a 4K TV at 2, rotated, plus a
    /// disabled output.
    const KSCREEN: &str = r#"kscreen.doctor: some Qt warning
{
    "outputs": [
        {
            "clones": [
            ],
            "connected": true,
            "currentModeId": "1",
            "enabled": true,
            "followPreferredMode": false,
            "hdr": false,
            "icon": "",
            "id": 1,
            "maxBpc": 0,
            "modes": [
                {
                    "id": "0",
                    "name": "5120x1440@60",
                    "refreshRate": 59.977001190185547,
                    "size": {
                        "height": 1440,
                        "width": 5120
                    }
                },
                {
                    "id": "1",
                    "name": "5120x1440@240",
                    "refreshRate": 239.76100158691406,
                    "size": {
                        "height": 1440,
                        "width": 5120
                    }
                },
                {
                    "id": "2",
                    "name": "3840x1080@120",
                    "refreshRate": 119.97399902343750,
                    "size": {
                        "height": 1080,
                        "width": 3840
                    }
                },
                {
                    "id": "3",
                    "name": "2560x1440@240",
                    "refreshRate": 239.76100158691406,
                    "size": {
                        "height": 1440,
                        "width": 2560
                    }
                },
                {
                    "id": "4",
                    "name": "1920x1080@60",
                    "refreshRate": 60,
                    "size": {
                        "height": 1080,
                        "width": 1920
                    }
                }
            ],
            "name": "DP-1",
            "overscan": 0,
            "pos": {
                "x": 0,
                "y": 0
            },
            "preferredModes": [
                "1"
            ],
            "priority": 1,
            "replicationSource": 0,
            "rgbRange": 0,
            "rotation": 1,
            "scale": 1.25,
            "sdr-brightness": 200,
            "size": {
                "height": 1440,
                "width": 5120
            },
            "sizeMM": {
                "height": 337,
                "width": 1191
            },
            "type": 14,
            "vrrPolicy": 2,
            "wcg": false
        },
        {
            "clones": [
            ],
            "connected": true,
            "currentModeId": "7",
            "enabled": true,
            "followPreferredMode": false,
            "icon": "",
            "id": 2,
            "modes": [
                {
                    "id": "7",
                    "name": "3840x2160@60",
                    "refreshRate": 60,
                    "size": {
                        "height": 2160,
                        "width": 3840
                    }
                },
                {
                    "id": "8",
                    "name": "1920x1080@60",
                    "refreshRate": 60,
                    "size": {
                        "height": 1080,
                        "width": 1920
                    }
                }
            ],
            "name": "HDMI-A-1",
            "pos": {
                "x": 4096,
                "y": 0
            },
            "preferredModes": [
                "7"
            ],
            "priority": 2,
            "replicationSource": 0,
            "rotation": 8,
            "scale": 2,
            "size": {
                "height": 2160,
                "width": 3840
            },
            "sizeMM": {
                "height": 700,
                "width": 1210
            },
            "type": 11
        },
        {
            "clones": [
            ],
            "connected": true,
            "currentModeId": "",
            "enabled": false,
            "followPreferredMode": false,
            "icon": "",
            "id": 3,
            "modes": [
            ],
            "name": "DP-3",
            "pos": {
                "x": 0,
                "y": 0
            },
            "priority": 0,
            "rotation": 1,
            "scale": 1,
            "type": 14
        }
    ],
    "screen": {
        "currentSize": {
            "height": 1920,
            "width": 5176
        },
        "id": 0,
        "maxActiveOutputsCount": 4,
        "maxSize": {
            "height": 64000,
            "width": 64000
        },
        "minSize": {
            "height": 0,
            "width": 0
        }
    },
    "tabletModeAvailable": false,
    "tabletModeEngaged": false
}
"#;

    #[test]
    fn kscreen_outputs() {
        let mons = parse_kscreen(KSCREEN).unwrap();
        assert_eq!(mons.len(), 2, "disabled output skipped");
        let dp = &mons[0];
        assert_eq!((dp.name.as_str(), dp.position, dp.modes.len()), ("DP-1", (0, 0), 5));
        let cur = dp.current_mode();
        assert_eq!(cur.id, "1");
        assert_eq!((cur.mode.width, cur.mode.height, cur.mode.pixel_width), (4096, 1152, 5120));
        assert_eq!(cur.layout, (4096, 1152));
        assert!((cur.mode.refresh - 239.761).abs() < 0.01);
        let tv = &mons[1];
        assert_eq!((tv.name.as_str(), tv.position), ("HDMI-A-1", (4096, 0)));
        // Rotated portrait at scale 2.
        let m = &tv.current_mode().mode;
        assert_eq!((m.width, m.height, m.pixel_width, m.pixel_height), (1080, 1920, 2160, 3840));
        assert!(m.hidpi());
        assert_eq!(find(&mons, (4096, 0)).unwrap().name, "HDMI-A-1");
        assert!(find(&mons, (1, 1)).is_none());
    }

    #[test]
    fn kscreen_pick_for_phone() {
        let mons = parse_kscreen(KSCREEN).unwrap();
        let dp = &mons[0];
        let modes: Vec<DisplayMode> = dp.modes.iter().map(|m| m.mode).collect();
        // iPhone in landscape, ~932x430 points: no 21:9 mode is small enough,
        // so the smallest 16:9 one wins (1920x1080 at 1.25).
        let i = pick_mode(&modes, &dp.current_mode().mode, (932, 430)).unwrap();
        assert_eq!(dp.modes[i].id, "4");
        assert_eq!(dp.modes[i].mode.describe(), "1536×864");
    }

    #[test]
    fn kscreen_garbage() {
        assert!(parse_kscreen("").is_err());
        assert!(parse_kscreen("kscreen-doctor: could not connect").is_err());
        assert!(parse_kscreen("{\"outputs\": [").is_err());
        assert_eq!(parse_kscreen("{\"outputs\": []}").unwrap(), vec![]);
    }

    #[test]
    fn json_basics() {
        let j = Json::parse(r#" {"a": [1, -2.5e1, true, null], "s": "x\"é\n", "e": {}} "#).unwrap();
        assert_eq!(j.get("a").unwrap().items()[1], Json::Num(-25.0));
        assert_eq!(j.get("a").unwrap().items()[3], Json::Null);
        assert_eq!(j.get("s").and_then(Json::as_str), Some("x\"é\n"));
        assert_eq!(j.get("e"), Some(&Json::Obj(vec![])));
        assert_eq!(Json::Num(3.0).as_id().as_deref(), Some("3"));
        assert!(Json::parse("[1,]").is_err());
        assert!(Json::parse("\"open").is_err());
    }

    const XRANDR: &str = "\
Screen 0: minimum 8 x 8, current 4480 x 1440, maximum 32767 x 32767
DP-0 disconnected (normal left inverted right x axis y axis)
DP-2 connected primary 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440     59.95 +  143.97*
   1920x1080     60.00    59.94
   1280x720      60.00
HDMI-0 connected 1080x1920+0+0 left (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00
   1920x1080i    60.00
DP-4 connected (normal left inverted right x axis y axis)
   1920x1080     60.00 +
";

    #[test]
    fn xrandr_outputs() {
        let mons = parse_xrandr(XRANDR);
        assert_eq!(mons.len(), 2, "disconnected and off outputs skipped");
        let dp = &mons[0];
        assert_eq!((dp.name.as_str(), dp.position, dp.modes.len()), ("DP-2", (1920, 0), 5));
        assert_eq!(dp.current_mode().id, "2560x1440@143.97");
        assert_eq!(
            dp.modes[4].mode,
            DisplayMode { width: 1280, height: 720, pixel_width: 1280, pixel_height: 720, refresh: 60.0 }
        );
        let hdmi = &mons[1];
        assert_eq!(hdmi.current_mode().id, "1920x1080@60.00");
        assert_eq!((hdmi.current_mode().mode.width, hdmi.current_mode().mode.height), (1080, 1920));
        assert_eq!(hdmi.modes.len(), 2, "interlaced mode skipped");
        assert_eq!(
            restore_hint(Desktop::X11, "DP-2", dp.current_mode()),
            "xrandr --output DP-2 --mode 2560x1440 --rate 143.97"
        );
    }

    fn mutter_mode(id: &str, w: i32, h: i32, hz: f64, scales: &[f64], current: bool) -> Value {
        let mut props = vec![];
        if current {
            props.push(("is-current", Value::Bool(true)));
        }
        Value::Struct(vec![
            Value::Str(id.into()),
            Value::I32(w),
            Value::I32(h),
            Value::F64(hz),
            Value::F64(1.0),
            Value::Array("d".into(), scales.iter().map(|s| Value::F64(*s)).collect()),
            dict(props),
        ])
    }

    fn mutter_monitor(connector: &str, modes: Vec<Value>) -> Value {
        let spec = ["MetaProducts Inc.", "MetaMonitor", "0x123456"].map(|s| Value::Str(s.into()));
        let mut f = vec![Value::Str(connector.into())];
        f.extend(spec);
        Value::Struct(vec![
            Value::Struct(f),
            Value::Array("(siiddada{sv})".into(), modes),
            dict(vec![("display-name", Value::Str("Monitor".into()))]),
        ])
    }

    fn mutter_logical(x: i32, y: i32, scale: f64, primary: bool, connector: &str) -> Value {
        let spec = [connector, "MetaProducts Inc.", "MetaMonitor", "0x123456"].map(|s| Value::Str(s.into()));
        Value::Struct(vec![
            Value::I32(x),
            Value::I32(y),
            Value::F64(scale),
            Value::U32(0),
            Value::Bool(primary),
            Value::Array("(ssss)".into(), vec![Value::Struct(spec.to_vec())]),
            dict(vec![]),
        ])
    }

    /// Two monitors side by side, layout-mode 1 (logical), DP-1 at 1.5.
    fn mutter_body() -> Vec<Value> {
        let s = [1.0, 1.25, 1.5, 1.75, 2.0];
        vec![
            Value::U32(42),
            Value::Array(
                "((ssss)a(siiddada{sv})a{sv})".into(),
                vec![
                    mutter_monitor(
                        "DP-1",
                        vec![
                            mutter_mode("3840x2160@59.997", 3840, 2160, 59.997, &s, true),
                            mutter_mode("2560x1440@59.951", 2560, 1440, 59.951, &s, false),
                            mutter_mode("1920x1080@60.000", 1920, 1080, 60.0, &s, false),
                            mutter_mode("1366x768@59.790", 1366, 768, 59.79, &[1.0], false),
                        ],
                    ),
                    mutter_monitor(
                        "HDMI-1",
                        vec![mutter_mode("1920x1080@60.000", 1920, 1080, 60.0, &[1.0, 2.0], true)],
                    ),
                ],
            ),
            Value::Array(
                "(iiduba(ssss)a{sv})".into(),
                vec![mutter_logical(0, 0, 1.5, true, "DP-1"), mutter_logical(2560, 0, 1.0, false, "HDMI-1")],
            ),
            dict(vec![
                ("layout-mode", Value::U32(1)),
                ("supports-changing-layout-mode", Value::Bool(true)),
                ("global-scale-required", Value::Bool(false)),
            ]),
        ]
    }

    #[test]
    fn mutter_state_and_monitors() {
        let st = MutterState::parse(&mutter_body()).unwrap();
        assert_eq!((st.serial, st.layout_mode, st.can_change_layout_mode), (42, 1, true));
        let mons = st.monitors();
        assert_eq!(mons.len(), 2);
        let dp = &mons[0];
        assert_eq!((dp.name.as_str(), dp.position, dp.modes.len()), ("DP-1", (0, 0), 3), "1366x768 lacks 1.5");
        assert_eq!(dp.current_mode().layout, (2560, 1440));
        assert_eq!(dp.modes[1].layout, (1707, 960));
        assert_eq!(find(&mons, (2560, 0)).unwrap().name, "HDMI-1");
        assert!(MutterState::parse(&[Value::U32(1)]).is_err());
    }

    #[test]
    fn mutter_apply_shifts_neighbours() {
        let st = MutterState::parse(&mutter_body()).unwrap();
        let args = st.apply_args("DP-1", "1920x1080@60.000").unwrap();
        assert_eq!(args[0], Value::U32(42));
        assert_eq!(args[1], Value::U32(1));
        let lms = args[2].items();
        assert_eq!(args[2].signature(), "a(iiduba(ssa{sv}))");
        // DP-1 stays at 0,0 with the new mode; HDMI-1 moves left by 2560 - 1280.
        assert_eq!(&lms[0].items()[..3], &[Value::I32(0), Value::I32(0), Value::F64(1.5)]);
        assert_eq!(lms[0].items()[5].items()[0].items()[1], Value::Str("1920x1080@60.000".into()));
        assert_eq!(&lms[1].items()[..2], &[Value::I32(1280), Value::I32(0)]);
        assert_eq!(lms[1].items()[5].items()[0].items()[1], Value::Str("1920x1080@60.000".into()));
        assert_eq!(args[3].get("layout-mode"), Some(&Value::U32(1)));
        assert!(st.apply_args("DP-9", "x").is_err());
        assert!(st.apply_args("DP-1", "nope").is_err());
    }
}
