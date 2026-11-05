//! Text clipboard access on Linux desktops: KDE Klipper over D-Bus, falling
//! back to the `wl-clipboard` tools (wl-paste / wl-copy) on other Wayland
//! desktops.

use crate::dbus::{Connection, Value};
use sard_core::media::Clipboard;
use std::io::Write;
use std::process::{Command, Stdio};

enum Backend {
    Klipper(Connection),
    WlClipboard,
}

pub struct LinuxClipboard {
    backend: Backend,
}

impl LinuxClipboard {
    pub fn open() -> Option<Self> {
        if let Ok(c) = Connection::session()
            && c.call("org.kde.klipper", "/klipper", "org.kde.klipper.klipper", "getClipboardContents", &[]).is_ok()
        {
            return Some(Self { backend: Backend::Klipper(c) });
        }
        let ok = Command::new("wl-paste").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status();
        if ok.is_ok_and(|s| s.success()) && std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return Some(Self { backend: Backend::WlClipboard });
        }
        None
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Klipper(_) => "Klipper (D-Bus)",
            Backend::WlClipboard => "wl-clipboard",
        }
    }
}

impl Clipboard for LinuxClipboard {
    fn get(&mut self) -> Option<String> {
        match &self.backend {
            Backend::Klipper(c) => c
                .call("org.kde.klipper", "/klipper", "org.kde.klipper.klipper", "getClipboardContents", &[])
                .ok()
                .and_then(|m| m.body.into_iter().next())
                .and_then(|v| v.as_str().map(String::from)),
            Backend::WlClipboard => {
                let out = Command::new("wl-paste")
                    .args(["--no-newline", "--type", "text/plain"])
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
            }
        }
    }

    fn set(&mut self, text: &str) {
        match &self.backend {
            Backend::Klipper(c) => {
                let _ = c.call(
                    "org.kde.klipper",
                    "/klipper",
                    "org.kde.klipper.klipper",
                    "setClipboardContents",
                    &[Value::Str(text.to_string())],
                );
            }
            Backend::WlClipboard => {
                if let Ok(mut child) =
                    Command::new("wl-copy").args(["--type", "text/plain"]).stdin(Stdio::piped()).spawn()
                {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = child.wait();
                }
            }
        }
    }
}
