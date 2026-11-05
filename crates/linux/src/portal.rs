//! xdg-desktop-portal RemoteDesktop + ScreenCast session.
//!
//! One combined session gives both the PipeWire screen stream and input
//! injection (pointer/keyboard), with a single user approval. With
//! `persist_mode = 2` the portal returns a restore token which we store, so
//! later sessions start silently.

use crate::dbus::{Connection, Value, dict};
use sard_core::{debug, info, warn};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const RD: &str = "org.freedesktop.portal.RemoteDesktop";
const SC: &str = "org.freedesktop.portal.ScreenCast";

pub type Result<T> = std::result::Result<T, String>;

pub mod cursor_mode {
    pub const HIDDEN: u32 = 1;
    pub const EMBEDDED: u32 = 2;
    pub const METADATA: u32 = 4;
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub node_id: u32,
    pub position: (i32, i32),
    /// Logical size (input coordinates for NotifyPointerMotionAbsolute).
    pub size: (i32, i32),
}

pub struct Session {
    conn: Arc<Connection>,
    path: String,
    pub streams: Vec<StreamInfo>,
    pub remote_input: bool,
    pub restore_token: Option<String>,
    pub cursor_mode: u32,
}

fn token() -> String {
    format!("sard{}", sard_core::rand::u32())
}

fn get_property(conn: &Connection, iface: &str, prop: &str) -> Option<Value> {
    conn.call(
        DEST,
        PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        &[Value::Str(iface.into()), Value::Str(prop.into())],
    )
    .ok()
    .and_then(|m| m.body.into_iter().next())
    .map(|v| v.unvariant().clone())
}

/// Call a portal method that returns a Request handle and wait for its Response.
fn request(conn: &Connection, iface: &str, method: &str, mut args: Vec<Value>, timeout: Duration) -> Result<Value> {
    let tok = token();
    let sender = conn.unique_name.trim_start_matches(':').replace('.', "_");
    let expected = format!("{PATH}/request/{sender}/{tok}");
    let rule = format!("type='signal',interface='org.freedesktop.portal.Request',member='Response',path='{expected}'");
    conn.add_match(&rule)?;
    // The options dict is always the last argument; add our handle_token.
    if let Some(Value::Array(_, items)) = args.last_mut() {
        items.push(Value::DictEntry(
            Box::new(Value::Str("handle_token".into())),
            Box::new(Value::Variant(Box::new(Value::Str(tok)))),
        ));
    }
    let reply = conn.call(DEST, PATH, iface, method, &args);
    let reply = match reply {
        Ok(r) => r,
        Err(e) => {
            conn.remove_match(&rule);
            return Err(e);
        }
    };
    let handle = reply.body.first().and_then(Value::as_str).unwrap_or(&expected).to_string();
    let sig = conn.wait(
        |m| m.member.as_deref() == Some("Response") && m.path.as_deref() == Some(handle.as_str()),
        Instant::now() + timeout,
    );
    conn.remove_match(&rule);
    let sig = sig.map_err(|_| format!("{method}: no response from the portal (dialog not answered?)"))?;
    let code = sig.body.first().and_then(Value::as_u32).unwrap_or(2);
    match code {
        0 => Ok(sig.body.get(1).cloned().unwrap_or(Value::Array("{sv}".into(), vec![]))),
        1 => Err(format!("{method}: request was cancelled on the host")),
        _ => Err(format!("{method}: the portal refused the request")),
    }
}

impl Session {
    /// Start a session. Shows the portal dialog unless `restore_token` is accepted.
    pub fn start(cursor: u32, restore_token: Option<&str>, want_input: bool) -> Result<Session> {
        let conn = Arc::new(Connection::session()?);
        let sc_version = get_property(&conn, SC, "version").and_then(|v| v.as_u32()).unwrap_or(0);
        if sc_version == 0 {
            return Err("xdg-desktop-portal ScreenCast is not available on this desktop".into());
        }
        let modes =
            get_property(&conn, SC, "AvailableCursorModes").and_then(|v| v.as_u32()).unwrap_or(cursor_mode::EMBEDDED);
        let cursor = if modes & cursor != 0 {
            cursor
        } else if modes & cursor_mode::EMBEDDED != 0 {
            cursor_mode::EMBEDDED
        } else {
            cursor_mode::HIDDEN
        };
        let rd_version =
            if want_input { get_property(&conn, RD, "version").and_then(|v| v.as_u32()).unwrap_or(0) } else { 0 };
        let use_rd = rd_version > 0;
        debug!("portal: ScreenCast v{sc_version}, RemoteDesktop v{rd_version}, cursor modes {modes:#x}");

        let quick = Duration::from_secs(30);
        let iface = if use_rd { RD } else { SC };
        let created = request(
            &conn,
            iface,
            "CreateSession",
            vec![dict(vec![("session_handle_token", Value::Str(token()))])],
            quick,
        )?;
        let path = created
            .get("session_handle")
            .and_then(Value::as_str)
            .ok_or("CreateSession: no session handle")?
            .to_string();
        let mut session = Session {
            conn: Arc::clone(&conn),
            path: path.clone(),
            streams: Vec::new(),
            remote_input: false,
            restore_token: None,
            cursor_mode: cursor,
        };

        let persist = |v: &mut Vec<(&str, Value)>| {
            v.push(("persist_mode", Value::U32(2)));
            if let Some(t) = restore_token {
                v.push(("restore_token", Value::Str(t.into())));
            }
        };
        if use_rd {
            let mut o = vec![("types", Value::U32(1 | 2))]; // keyboard | pointer
            if rd_version >= 2 {
                persist(&mut o);
            }
            request(&conn, RD, "SelectDevices", vec![Value::Path(path.clone()), dict(o)], quick)?;
        }
        let mut o =
            vec![("types", Value::U32(1)), ("multiple", Value::Bool(false)), ("cursor_mode", Value::U32(cursor))];
        if !use_rd && sc_version >= 4 {
            persist(&mut o);
        }
        request(&conn, SC, "SelectSources", vec![Value::Path(path.clone()), dict(o)], quick)?;

        info!("portal: waiting for screen-share approval (a dialog may be shown on this machine)...");
        let started = request(
            &conn,
            iface,
            "Start",
            vec![Value::Path(path.clone()), Value::Str(String::new()), dict(vec![])],
            Duration::from_secs(600),
        )?;
        session.restore_token = started.get("restore_token").and_then(Value::as_str).map(String::from);
        session.remote_input = use_rd && started.get("devices").and_then(Value::as_u32).unwrap_or(0) & 3 != 0;
        if let Some(streams) = started.get("streams") {
            for s in streams.items() {
                let it = s.items();
                let Some(node_id) = it.first().and_then(Value::as_u32) else { continue };
                let props = it.get(1);
                let pair = |k: &str| {
                    props.and_then(|p| p.get(k)).map(|v| {
                        let i = v.items();
                        (i.first().and_then(Value::as_i32).unwrap_or(0), i.get(1).and_then(Value::as_i32).unwrap_or(0))
                    })
                };
                session.streams.push(StreamInfo {
                    node_id,
                    position: pair("position").unwrap_or((0, 0)),
                    size: pair("size").unwrap_or((0, 0)),
                });
            }
        }
        if session.streams.is_empty() {
            return Err("portal returned no screen streams".into());
        }
        if want_input && !session.remote_input {
            warn!("portal did not grant remote input; the client will be view-only");
        }
        info!(
            "portal: streaming node {} ({}x{} logical), input {}",
            session.streams[0].node_id,
            session.streams[0].size.0,
            session.streams[0].size.1,
            if session.remote_input { "granted" } else { "unavailable" }
        );
        Ok(session)
    }

    /// File descriptor of the PipeWire connection that can see our stream.
    pub fn open_pipewire_remote(&self) -> Result<OwnedFd> {
        let mut r =
            self.conn.call(DEST, PATH, SC, "OpenPipeWireRemote", &[Value::Path(self.path.clone()), dict(vec![])])?;
        let idx = match r.body.first() {
            Some(Value::Fd(i)) => *i as usize,
            _ => return Err("OpenPipeWireRemote: no fd in reply".into()),
        };
        if idx >= r.fds.len() {
            return Err("OpenPipeWireRemote: fd missing".into());
        }
        Ok(r.fds.swap_remove(idx))
    }

    pub fn input(&self) -> PortalInput {
        PortalInput {
            conn: Arc::clone(&self.conn),
            session: self.path.clone(),
            stream: self.streams[0].clone(),
            scroll_acc: (0.0, 0.0),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.conn.call_noreply(DEST, &self.path, "org.freedesktop.portal.Session", "Close", &[]);
    }
}

/// Input injection through the RemoteDesktop portal (evdev codes).
pub struct PortalInput {
    conn: Arc<Connection>,
    session: String,
    stream: StreamInfo,
    scroll_acc: (f32, f32),
}

impl PortalInput {
    fn notify(&self, method: &str, mut args: Vec<Value>) -> Result<()> {
        let mut full = vec![Value::Path(self.session.clone()), dict(vec![])];
        full.append(&mut args);
        self.conn.call_noreply(DEST, PATH, RD, method, &full)
    }

    pub fn move_abs(&self, x: f32, y: f32) -> Result<()> {
        let (w, h) = self.stream.size;
        let px = (x.clamp(0.0, 1.0) as f64 * w as f64).min(w as f64 - 0.5);
        let py = (y.clamp(0.0, 1.0) as f64 * h as f64).min(h as f64 - 0.5);
        self.notify(
            "NotifyPointerMotionAbsolute",
            vec![Value::U32(self.stream.node_id), Value::F64(px), Value::F64(py)],
        )
    }

    pub fn move_rel(&self, dx: f32, dy: f32) -> Result<()> {
        self.notify("NotifyPointerMotion", vec![Value::F64(dx as f64), Value::F64(dy as f64)])
    }

    pub fn button(&self, evdev_button: u16, down: bool) -> Result<()> {
        self.notify("NotifyPointerButton", vec![Value::I32(evdev_button as i32), Value::U32(down as u32)])
    }

    pub fn key(&self, evdev_code: u16, down: bool) -> Result<()> {
        self.notify("NotifyKeyboardKeycode", vec![Value::I32(evdev_code as i32), Value::U32(down as u32)])
    }

    /// `dy > 0` scrolls up (wire convention); the portal/Wayland use `> 0` = down.
    pub fn scroll(&mut self, dx: f32, dy: f32, discrete: bool) -> Result<()> {
        if discrete {
            self.scroll_acc.0 += dx;
            self.scroll_acc.1 += dy;
            let sy = self.scroll_acc.1.trunc();
            let sx = self.scroll_acc.0.trunc();
            if sy != 0.0 {
                self.scroll_acc.1 -= sy;
                self.notify("NotifyPointerAxisDiscrete", vec![Value::U32(0), Value::I32(-(sy as i32))])?;
            }
            if sx != 0.0 {
                self.scroll_acc.0 -= sx;
                self.notify("NotifyPointerAxisDiscrete", vec![Value::U32(1), Value::I32(-(sx as i32))])?;
            }
            Ok(())
        } else {
            let mut full = vec![
                Value::Path(self.session.clone()),
                dict(vec![("finish", Value::Bool(false))]),
                Value::F64(-dx as f64),
                Value::F64(-dy as f64),
            ];
            let _ = &mut full;
            self.conn.call_noreply(DEST, PATH, RD, "NotifyPointerAxis", &full)
        }
    }
}
