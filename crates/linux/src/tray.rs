//! System tray icon via the StatusNotifierItem D-Bus protocol (KDE Plasma,
//! and other desktops with an SNI host) with a com.canonical.dbusmenu menu.
//!
//! Served from our own session-bus connection on a background thread; the
//! menu model lives behind a mutex so `set_menu` etc. can be called from the
//! app's main thread and just emit change signals.

use crate::dbus::{self, METHOD_CALL, Message, SIGNAL, Value, dict};
use sard_core::media::{self, MenuEntry, Rgba, Tray, TrayCallback, TrayIcons};
use sard_core::warn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

/// One dbusmenu node (index = dbusmenu id; 0 is the root).
#[derive(Clone, Debug)]
struct Node {
    props: Vec<(&'static str, Value)>,
    children: Vec<i32>,
    /// The app's id for clickable items.
    user_id: Option<u32>,
}

struct State {
    nodes: Vec<Node>,
    revision: u32,
    tooltip: String,
    active: bool,
}

fn flatten(entries: &[MenuEntry], nodes: &mut Vec<Node>) -> Vec<i32> {
    let mut ids = Vec::new();
    for e in entries {
        let id = nodes.len() as i32;
        nodes.push(Node { props: Vec::new(), children: Vec::new(), user_id: None });
        let node = match e {
            MenuEntry::Separator => {
                Node { props: vec![("type", Value::Str("separator".into()))], children: Vec::new(), user_id: None }
            }
            MenuEntry::Item { id: uid, label, enabled, checked } => {
                let mut props = vec![("label", Value::Str(label.replace('_', "__")))];
                if !enabled {
                    props.push(("enabled", Value::Bool(false)));
                }
                if let Some(c) = checked {
                    props.push(("toggle-type", Value::Str("checkmark".into())));
                    props.push(("toggle-state", Value::I32(*c as i32)));
                }
                Node { props, children: Vec::new(), user_id: Some(*uid) }
            }
            MenuEntry::Submenu { label, enabled, items } => {
                let children = flatten(items, nodes);
                let mut props = vec![
                    ("label", Value::Str(label.replace('_', "__"))),
                    ("children-display", Value::Str("submenu".into())),
                ];
                if !enabled {
                    props.push(("enabled", Value::Bool(false)));
                }
                Node { props, children, user_id: None }
            }
        };
        nodes[id as usize] = node;
        ids.push(id);
    }
    ids
}

fn build_nodes(menu: &[MenuEntry]) -> Vec<Node> {
    let mut nodes = vec![Node {
        props: vec![("children-display", Value::Str("submenu".into()))],
        children: Vec::new(),
        user_id: None,
    }];
    let top = flatten(menu, &mut nodes);
    nodes[0].children = top;
    nodes
}

fn props_value(node: &Node, filter: &[String]) -> Value {
    dict(
        node.props
            .iter()
            .filter(|(k, _)| filter.is_empty() || filter.iter().any(|f| f == k))
            .map(|(k, v)| (*k, v.clone()))
            .collect(),
    )
}

/// `(ia{sv}av)` for `id`, recursing `depth` levels (-1 = all).
fn layout(nodes: &[Node], id: i32, depth: i32, filter: &[String]) -> Value {
    let node = &nodes[id as usize];
    let children = if depth == 0 {
        Vec::new()
    } else {
        node.children.iter().map(|&c| Value::Variant(Box::new(layout(nodes, c, depth - 1, filter)))).collect()
    };
    Value::Struct(vec![Value::I32(id), props_value(node, filter), Value::Array("v".into(), children)])
}

/// SNI pixmaps are ARGB32 in network byte order.
fn pixmap(icon: &Rgba) -> Value {
    let mut argb = Vec::with_capacity(icon.pixels.len());
    for p in icon.pixels.as_chunks::<4>().0 {
        argb.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
    }
    Value::Array(
        "(iiay)".into(),
        vec![Value::Struct(vec![
            Value::I32(icon.width as i32),
            Value::I32(icon.height as i32),
            Value::Array("y".into(), argb.into_iter().map(Value::Byte).collect()),
        ])],
    )
}

struct Shared {
    conn: dbus::Connection,
    state: Mutex<State>,
    icon: Value,
    icon_active: Value,
    on_click: TrayCallback,
    stop: AtomicBool,
    bus_name: String,
}

impl Shared {
    fn item_prop(&self, name: &str) -> Option<Value> {
        let st = self.state.lock().unwrap();
        Some(match name {
            "Category" => Value::Str("ApplicationStatus".into()),
            "Id" => Value::Str("sard".into()),
            "Title" => Value::Str("Sard".into()),
            "Status" => Value::Str("Active".into()),
            "WindowId" => Value::I32(0),
            "IconName" | "OverlayIconName" | "AttentionIconName" | "AttentionMovieName" | "IconThemePath" => {
                Value::Str(String::new())
            }
            "IconPixmap" => {
                if st.active {
                    self.icon_active.clone()
                } else {
                    self.icon.clone()
                }
            }
            "OverlayIconPixmap" | "AttentionIconPixmap" => Value::Array("(iiay)".into(), Vec::new()),
            "ToolTip" => Value::Struct(vec![
                Value::Str(String::new()),
                Value::Array("(iiay)".into(), Vec::new()),
                Value::Str("Sard".into()),
                Value::Str(st.tooltip.clone()),
            ]),
            "ItemIsMenu" => Value::Bool(true),
            "Menu" => Value::Path(MENU_PATH.into()),
            _ => return None,
        })
    }

    fn menu_prop(&self, name: &str) -> Option<Value> {
        Some(match name {
            "Version" => Value::U32(3),
            "TextDirection" => Value::Str("ltr".into()),
            "Status" => Value::Str("normal".into()),
            "IconThemePath" => Value::Array("s".into(), Vec::new()),
            _ => return None,
        })
    }

    const ITEM_PROPS: [&str; 16] = [
        "Category",
        "Id",
        "Title",
        "Status",
        "WindowId",
        "IconName",
        "IconPixmap",
        "OverlayIconName",
        "OverlayIconPixmap",
        "AttentionIconName",
        "AttentionIconPixmap",
        "AttentionMovieName",
        "ToolTip",
        "ItemIsMenu",
        "Menu",
        "IconThemePath",
    ];
    const MENU_PROPS: [&str; 4] = ["Version", "TextDirection", "Status", "IconThemePath"];

    fn handle(&self, m: &Message) -> dbus::Result<()> {
        let c = &self.conn;
        let path = m.path.as_deref().unwrap_or("");
        let iface = m.interface.as_deref().unwrap_or("");
        let member = m.member.as_deref().unwrap_or("");
        let arg_str = |i: usize| m.body.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        let arg_i32 = |i: usize| m.body.get(i).and_then(Value::as_i32).unwrap_or(0);
        match (path, iface, member) {
            (_, "org.freedesktop.DBus.Introspectable", "Introspect") => c.reply(m, &[Value::Str(introspect(path))]),
            (_, "org.freedesktop.DBus.Peer", "Ping") => c.reply(m, &[]),
            (ITEM_PATH | MENU_PATH, PROPS_IFACE, "Get") => {
                let (want_iface, name) = (arg_str(0), arg_str(1));
                let v = if path == ITEM_PATH { self.item_prop(&name) } else { self.menu_prop(&name) };
                match v {
                    Some(v) if want_iface.is_empty() || want_iface == ITEM_IFACE || want_iface == MENU_IFACE => {
                        c.reply(m, &[Value::Variant(Box::new(v))])
                    }
                    _ => c.reply_error(m, "org.freedesktop.DBus.Error.UnknownProperty", &name),
                }
            }
            (ITEM_PATH | MENU_PATH, PROPS_IFACE, "GetAll") => {
                let all: Vec<(&str, Value)> = if path == ITEM_PATH {
                    Self::ITEM_PROPS.iter().filter_map(|n| Some((*n, self.item_prop(n)?))).collect()
                } else {
                    Self::MENU_PROPS.iter().filter_map(|n| Some((*n, self.menu_prop(n)?))).collect()
                };
                c.reply(m, &[dict(all)])
            }
            (ITEM_PATH | MENU_PATH, PROPS_IFACE, "Set") => {
                c.reply_error(m, "org.freedesktop.DBus.Error.PropertyReadOnly", "read-only")
            }
            // Clicking the icon opens the menu (ItemIsMenu); nothing else to do.
            (ITEM_PATH, ITEM_IFACE, "Activate" | "SecondaryActivate" | "ContextMenu" | "Scroll") => c.reply(m, &[]),
            (MENU_PATH, MENU_IFACE, "GetLayout") => {
                let (parent, depth) = (arg_i32(0), arg_i32(1));
                let filter: Vec<String> = m
                    .body
                    .get(2)
                    .map(|v| v.items().iter().filter_map(|s| s.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let st = self.state.lock().unwrap();
                if parent < 0 || parent as usize >= st.nodes.len() {
                    return c.reply_error(m, "org.freedesktop.DBus.Error.InvalidArgs", "no such menu item");
                }
                let l = layout(&st.nodes, parent, depth, &filter);
                c.reply(m, &[Value::U32(st.revision), l])
            }
            (MENU_PATH, MENU_IFACE, "GetGroupProperties") => {
                let ids: Vec<i32> =
                    m.body.first().map(|v| v.items().iter().filter_map(Value::as_i32).collect()).unwrap_or_default();
                let filter: Vec<String> = m
                    .body
                    .get(1)
                    .map(|v| v.items().iter().filter_map(|s| s.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let st = self.state.lock().unwrap();
                let ids: Vec<i32> = if ids.is_empty() { (0..st.nodes.len() as i32).collect() } else { ids };
                let out = ids
                    .into_iter()
                    .filter(|&i| i >= 0 && (i as usize) < st.nodes.len())
                    .map(|i| Value::Struct(vec![Value::I32(i), props_value(&st.nodes[i as usize], &filter)]))
                    .collect();
                c.reply(m, &[Value::Array("(ia{sv})".into(), out)])
            }
            (MENU_PATH, MENU_IFACE, "GetProperty") => {
                let (id, name) = (arg_i32(0), arg_str(1));
                let st = self.state.lock().unwrap();
                let v = st
                    .nodes
                    .get(id.max(0) as usize)
                    .and_then(|n| n.props.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()));
                match v {
                    Some(v) => c.reply(m, &[Value::Variant(Box::new(v))]),
                    None => c.reply_error(m, "org.freedesktop.DBus.Error.InvalidArgs", "no such property"),
                }
            }
            (MENU_PATH, MENU_IFACE, "Event") => {
                c.reply(m, &[])?;
                self.event(arg_i32(0), &arg_str(1));
                Ok(())
            }
            (MENU_PATH, MENU_IFACE, "EventGroup") => {
                c.reply(m, &[Value::Array("i".into(), Vec::new())])?;
                let events: Vec<(i32, String)> = m
                    .body
                    .first()
                    .map(|v| {
                        v.items()
                            .iter()
                            .map(|e| {
                                let f = e.items();
                                (
                                    f.first().and_then(Value::as_i32).unwrap_or(-1),
                                    f.get(1).and_then(Value::as_str).unwrap_or("").to_string(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (id, ev) in events {
                    self.event(id, &ev);
                }
                Ok(())
            }
            (MENU_PATH, MENU_IFACE, "AboutToShow") => c.reply(m, &[Value::Bool(false)]),
            (MENU_PATH, MENU_IFACE, "AboutToShowGroup") => {
                c.reply(m, &[Value::Array("i".into(), Vec::new()), Value::Array("i".into(), Vec::new())])
            }
            _ => c.reply_error(m, "org.freedesktop.DBus.Error.UnknownMethod", &format!("{iface}.{member}")),
        }
    }

    fn event(&self, id: i32, event: &str) {
        if event != "clicked" {
            return;
        }
        let user = self.state.lock().unwrap().nodes.get(id.max(0) as usize).and_then(|n| n.user_id);
        if let Some(u) = user {
            (self.on_click)(u);
        }
    }

    fn register(&self) -> dbus::Result<()> {
        self.conn
            .call(
                WATCHER,
                "/StatusNotifierWatcher",
                WATCHER,
                "RegisterStatusNotifierItem",
                &[Value::Str(self.bus_name.clone())],
            )
            .map(|_| ())
    }

    fn serve(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            let pred = |m: &Message| {
                m.msg_type == METHOD_CALL || (m.msg_type == SIGNAL && m.member.as_deref() == Some("NameOwnerChanged"))
            };
            match self.conn.wait(pred, Instant::now() + Duration::from_millis(250)) {
                Ok(m) if m.msg_type == METHOD_CALL => {
                    if let Err(e) = self.handle(&m) {
                        warn!("tray: {e}");
                    }
                }
                Ok(m) => {
                    // The panel restarted: register with the new watcher.
                    let new_owner = m.body.get(2).and_then(Value::as_str).unwrap_or("");
                    if m.body.first().and_then(Value::as_str) == Some(WATCHER) && !new_owner.is_empty() {
                        std::thread::sleep(Duration::from_millis(200));
                        if let Err(e) = self.register() {
                            warn!("tray: re-register failed: {e}");
                        }
                    }
                }
                Err(e) if e.starts_with("timed out") => {}
                Err(e) => {
                    warn!("tray: D-Bus connection lost: {e}");
                    return;
                }
            }
        }
    }
}

fn introspect(path: &str) -> String {
    let body = match path {
        ITEM_PATH => {
            r#"<interface name="org.kde.StatusNotifierItem">
  <property name="Category" type="s" access="read"/><property name="Id" type="s" access="read"/>
  <property name="Title" type="s" access="read"/><property name="Status" type="s" access="read"/>
  <property name="WindowId" type="i" access="read"/><property name="IconName" type="s" access="read"/>
  <property name="IconPixmap" type="a(iiay)" access="read"/><property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
  <property name="ItemIsMenu" type="b" access="read"/><property name="Menu" type="o" access="read"/>
  <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <signal name="NewIcon"/><signal name="NewToolTip"/><signal name="NewStatus"><arg type="s"/></signal>
</interface>"#
        }
        MENU_PATH => {
            r#"<interface name="com.canonical.dbusmenu">
  <property name="Version" type="u" access="read"/><property name="Status" type="s" access="read"/>
  <method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
  <method name="GetGroupProperties"><arg type="ai" direction="in"/><arg type="as" direction="in"/><arg type="a(ia{sv})" direction="out"/></method>
  <method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/></method>
  <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
  <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
</interface>"#
        }
        _ => "",
    };
    format!(
        "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
         \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n<node>{body}</node>"
    )
}

pub struct SniTray {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Tray for SniTray {
    fn set_menu(&self, menu: Vec<MenuEntry>) {
        let rev = {
            let mut st = self.shared.state.lock().unwrap();
            st.nodes = build_nodes(&menu);
            st.revision = st.revision.wrapping_add(1);
            st.revision
        };
        let _ = self.shared.conn.emit(MENU_PATH, MENU_IFACE, "LayoutUpdated", &[Value::U32(rev), Value::I32(0)]);
    }

    fn set_tooltip(&self, text: &str) {
        {
            let mut st = self.shared.state.lock().unwrap();
            if st.tooltip == text {
                return;
            }
            st.tooltip = text.to_string();
        }
        let _ = self.shared.conn.emit(ITEM_PATH, ITEM_IFACE, "NewToolTip", &[]);
    }

    fn set_active(&self, active: bool) {
        {
            let mut st = self.shared.state.lock().unwrap();
            if st.active == active {
                return;
            }
            st.active = active;
        }
        let _ = self.shared.conn.emit(ITEM_PATH, ITEM_IFACE, "NewIcon", &[]);
    }
}

impl Drop for SniTray {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// Create the tray icon on the session bus.
pub fn create(icons: TrayIcons, on_click: TrayCallback) -> media::Result<Box<dyn Tray>> {
    let conn = dbus::Connection::session().map_err(media::err)?;
    create_on(conn, icons, on_click).map(|t| Box::new(t) as Box<dyn Tray>)
}

pub(crate) fn create_on(conn: dbus::Connection, icons: TrayIcons, on_click: TrayCallback) -> media::Result<SniTray> {
    let bus_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    conn.request_name(&bus_name).map_err(media::err)?;
    conn.add_match(&format!(
        "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{WATCHER}'"
    ))
    .map_err(media::err)?;
    let shared = Arc::new(Shared {
        conn,
        state: Mutex::new(State { nodes: build_nodes(&[]), revision: 1, tooltip: String::new(), active: false }),
        // Colour icons: the monochrome ones are black templates (macOS tints them).
        icon: pixmap(&icons.color),
        icon_active: pixmap(&icons.color_active),
        on_click,
        stop: AtomicBool::new(false),
        bus_name,
    });
    let s = Arc::clone(&shared);
    let thread = std::thread::Builder::new().name("sard-tray".into()).spawn(move || s.serve()).map_err(media::err)?;
    let tray = SniTray { shared, thread: Mutex::new(Some(thread)) };
    tray.shared
        .register()
        .map_err(|e| media::err(format!("no system tray on this desktop (StatusNotifierWatcher unavailable): {e}")))?;
    Ok(tray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn icon() -> Rgba {
        Rgba { width: 2, height: 1, pixels: vec![10, 20, 30, 255, 0, 0, 0, 0] }
    }

    fn icons() -> TrayIcons {
        TrayIcons { mono: icon(), mono_active: icon(), color: icon(), color_active: icon() }
    }

    #[test]
    fn layout_flattening() {
        let menu = vec![
            MenuEntry::Item { id: 7, label: "Status".into(), enabled: false, checked: None },
            MenuEntry::Separator,
            MenuEntry::Submenu {
                label: "Connect".into(),
                enabled: true,
                items: vec![MenuEntry::Item { id: 1000, label: "my_mac".into(), enabled: true, checked: Some(true) }],
            },
        ];
        let nodes = build_nodes(&menu);
        assert_eq!(nodes.len(), 5);
        assert_eq!(nodes[0].children, vec![1, 2, 3]);
        assert_eq!(nodes[3].children, vec![4]);
        assert_eq!(nodes[4].user_id, Some(1000));
        let label = nodes[4].props.iter().find(|(k, _)| *k == "label").unwrap();
        assert_eq!(label.1, Value::Str("my__mac".into()));
        // Depth 1: root plus its children, but not grandchildren.
        let l = layout(&nodes, 0, 1, &[]);
        let kids = l.items()[2].items();
        assert_eq!(kids.len(), 3);
        assert!(kids[2].unvariant().items()[2].items().is_empty());
        assert_eq!(pixmap(&icon()).items()[0].items()[2].items()[..4], [255, 10, 20, 30].map(Value::Byte));
    }

    /// Full round trip on a private bus: a fake StatusNotifierWatcher, the
    /// tray, and a "panel" that reads the menu and clicks an item.
    #[test]
    fn serves_sni_and_dbusmenu_on_private_bus() {
        if std::process::Command::new("dbus-daemon").arg("--version").output().is_err() {
            return; // no dbus-daemon: skip
        }
        let mut child = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut addr = String::new();
        {
            use std::io::BufRead;
            let mut r = std::io::BufReader::new(child.stdout.take().unwrap());
            r.read_line(&mut addr).unwrap();
        }
        let addr = addr.trim().to_string();
        let result = std::panic::catch_unwind(|| {
            // Fake watcher.
            let watcher = dbus::Connection::open(&addr).unwrap();
            watcher.request_name(WATCHER).unwrap();
            let registered = Arc::new(Mutex::new(String::new()));
            let w = {
                let registered = Arc::clone(&registered);
                std::thread::spawn(move || {
                    let m =
                        watcher.wait(|m| m.msg_type == METHOD_CALL, Instant::now() + Duration::from_secs(5)).unwrap();
                    assert_eq!(m.member.as_deref(), Some("RegisterStatusNotifierItem"));
                    *registered.lock().unwrap() = m.body[0].as_str().unwrap().to_string();
                    watcher.reply(&m, &[]).unwrap();
                })
            };
            let clicked = Arc::new(AtomicU32::new(0));
            let c2 = Arc::clone(&clicked);
            let tray = create_on(
                dbus::Connection::open(&addr).unwrap(),
                icons(),
                Box::new(move |id| c2.store(id, Ordering::SeqCst)),
            )
            .unwrap();
            w.join().unwrap();
            let name = registered.lock().unwrap().clone();
            assert!(name.starts_with("org.kde.StatusNotifierItem-"));
            tray.set_menu(vec![
                MenuEntry::Item { id: 1, label: "Sharing".into(), enabled: false, checked: None },
                MenuEntry::Submenu {
                    label: "Displays".into(),
                    enabled: true,
                    items: vec![
                        MenuEntry::Item { id: 2000, label: "DP-1".into(), enabled: true, checked: Some(true) },
                        MenuEntry::Item { id: 2001, label: "HDMI-A-1".into(), enabled: true, checked: Some(false) },
                    ],
                },
            ]);
            tray.set_tooltip("Viewed by mac");
            tray.set_active(true);

            let panel = dbus::Connection::open(&addr).unwrap();
            let r = panel
                .call(&name, ITEM_PATH, PROPS_IFACE, "Get", &[Value::Str(ITEM_IFACE.into()), Value::Str("Menu".into())])
                .unwrap();
            assert_eq!(r.body[0].unvariant(), &Value::Path(MENU_PATH.into()));
            let r = panel.call(&name, ITEM_PATH, PROPS_IFACE, "GetAll", &[Value::Str(ITEM_IFACE.into())]).unwrap();
            let tip = r.body[0].get("ToolTip").unwrap();
            assert_eq!(tip.items()[3].as_str(), Some("Viewed by mac"));
            let r = panel
                .call(
                    &name,
                    MENU_PATH,
                    MENU_IFACE,
                    "GetLayout",
                    &[Value::I32(0), Value::I32(-1), Value::Array("s".into(), Vec::new())],
                )
                .unwrap();
            let root = &r.body[1];
            let displays = root.items()[2].items()[1].unvariant();
            assert_eq!(displays.items()[1].get("label").and_then(Value::as_str), Some("Displays"));
            let hdmi = displays.items()[2].items()[1].unvariant();
            let hdmi_id = hdmi.items()[0].as_i32().unwrap();
            panel
                .call(
                    &name,
                    MENU_PATH,
                    MENU_IFACE,
                    "Event",
                    &[
                        Value::I32(hdmi_id),
                        Value::Str("clicked".into()),
                        Value::Variant(Box::new(Value::I32(0))),
                        Value::U32(0),
                    ],
                )
                .unwrap();
            assert_eq!(clicked.load(Ordering::SeqCst), 2001);
            let r = panel.call(&name, MENU_PATH, "org.freedesktop.DBus.Introspectable", "Introspect", &[]).unwrap();
            assert!(r.body[0].as_str().unwrap().contains("com.canonical.dbusmenu"));
            // An independent implementation (GLib) must parse our replies too.
            let gdbus = |obj: &str, method: &str, args: &[&str]| {
                std::process::Command::new("gdbus")
                    .args(["call", "--address", &addr, "--dest", &name, "--object-path", obj, "--method", method, "--"])
                    .args(args)
                    .output()
                    .ok()
                    .map(|o| {
                        (
                            o.status.success(),
                            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
                        )
                    })
            };
            if let Some((ok, out)) = gdbus(MENU_PATH, "com.canonical.dbusmenu.GetLayout", &["0", "-1", "[]"]) {
                assert!(ok && out.contains("HDMI-A-1") && out.contains("'toggle-state': <1>"), "{out}");
                let (ok, out) =
                    gdbus(ITEM_PATH, "org.freedesktop.DBus.Properties.GetAll", &["org.kde.StatusNotifierItem"])
                        .unwrap();
                assert!(ok && out.contains("'ItemIsMenu': <true>") && out.contains("[(2, 1, [byte 0xff"), "{out}");
                let (ok, out) =
                    gdbus(MENU_PATH, "com.canonical.dbusmenu.GetGroupProperties", &["[]", "['label']"]).unwrap();
                assert!(ok && out.contains("'Displays'"), "{out}");
            }
            drop(tray);
        });
        let _ = child.kill();
        let _ = child.wait();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
