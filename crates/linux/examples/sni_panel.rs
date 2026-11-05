//! A stand-in system tray "panel" for testing the tray headlessly: owns
//! org.kde.StatusNotifierWatcher on the current session bus (use a private
//! `dbus-daemon`!), waits for an item to register, then runs commands from
//! stdin, one per line:
//!
//!   menu              print the menu tree
//!   tooltip           print the tooltip text
//!   click <text>      click the first enabled item whose label contains <text>
//!   sleep <secs>      wait
//!
//! Example: printf 'menu\nclick Share\nsleep 1\nmenu\n' | sni_panel

use sard_linux::dbus::{Connection, METHOD_CALL, Value};
use std::io::BufRead;
use std::time::{Duration, Instant};

fn label(props: &Value) -> String {
    props.get("label").and_then(Value::as_str).unwrap_or("").replace("__", "_")
}

fn walk(node: &Value, depth: usize, out: &mut Vec<(i32, String, bool)>, print: bool) {
    let f = node.items();
    let id = f[0].as_i32().unwrap_or(-1);
    let props = &f[1];
    if id != 0 {
        let sep = props.get("type").and_then(Value::as_str) == Some("separator");
        let enabled = !matches!(props.get("enabled").map(Value::unvariant), Some(Value::Bool(false)));
        let check = match props.get("toggle-state").and_then(Value::as_i32) {
            Some(1) => "[x] ",
            Some(_) => "[ ] ",
            None => "",
        };
        let text = if sep { "────────".to_string() } else { format!("{check}{}", label(props)) };
        if print {
            let dim = if enabled || sep { "" } else { "  (disabled)" };
            println!("{:indent$}{text}{dim}", "", indent = depth * 2);
        }
        if !sep {
            out.push((id, label(props), enabled));
        }
    }
    for c in f[2].items() {
        walk(c.unvariant(), depth + (id != 0) as usize, out, print);
    }
}

fn main() {
    let conn = Connection::session().expect("session bus");
    conn.request_name("org.kde.StatusNotifierWatcher").expect("own watcher name");
    eprintln!("panel: waiting for a tray item...");
    let m = conn
        .wait(
            |m| m.msg_type == METHOD_CALL && m.member.as_deref() == Some("RegisterStatusNotifierItem"),
            Instant::now() + Duration::from_secs(60),
        )
        .expect("no tray item registered");
    let item = m.body[0].as_str().unwrap().to_string();
    conn.reply(&m, &[]).unwrap();
    eprintln!("panel: item {item} registered");
    let props = |name: &str| {
        conn.call(
            &item,
            "/StatusNotifierItem",
            "org.freedesktop.DBus.Properties",
            "Get",
            &[Value::Str("org.kde.StatusNotifierItem".into()), Value::Str(name.into())],
        )
        .map(|r| r.body[0].unvariant().clone())
    };
    let menu_path = match props("Menu") {
        Ok(Value::Path(p)) => p,
        other => panic!("no Menu property: {other:?}"),
    };
    let layout = || {
        let r = conn
            .call(
                &item,
                &menu_path,
                "com.canonical.dbusmenu",
                "GetLayout",
                &[Value::I32(0), Value::I32(-1), Value::Array("s".into(), Vec::new())],
            )
            .expect("GetLayout");
        r.body[1].clone()
    };
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        match cmd {
            "menu" => {
                println!("--- menu ---");
                walk(&layout(), 0, &mut Vec::new(), true);
            }
            "tooltip" => {
                let t = props("ToolTip").unwrap();
                println!("tooltip: {}", t.items()[3].as_str().unwrap_or(""));
            }
            "click" => {
                let mut items = Vec::new();
                walk(&layout(), 0, &mut items, false);
                match items.iter().find(|(_, l, en)| *en && l.contains(arg)) {
                    Some((id, l, _)) => {
                        println!("click: {l}");
                        conn.call(
                            &item,
                            &menu_path,
                            "com.canonical.dbusmenu",
                            "Event",
                            &[
                                Value::I32(*id),
                                Value::Str("clicked".into()),
                                Value::Variant(Box::new(Value::I32(0))),
                                Value::U32(0),
                            ],
                        )
                        .expect("Event");
                    }
                    None => println!("click: no enabled item matching {arg:?}"),
                }
            }
            "sleep" => std::thread::sleep(Duration::from_secs_f32(arg.parse().unwrap_or(1.0))),
            "" => {}
            other => println!("unknown command {other}"),
        }
    }
}
