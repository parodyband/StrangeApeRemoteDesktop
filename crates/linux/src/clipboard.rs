//! Clipboard access on Linux desktops.
//!
//! * Text: KDE Klipper over D-Bus, falling back to the `wl-clipboard` tools
//!   (wl-paste / wl-copy) on other Wayland desktops.
//! * Images (PNG): `wl-clipboard` only (Klipper's D-Bus API is text-only).
//!   A long-running `wl-paste --watch` says when the clipboard changes, so
//!   an image is read once per copy rather than on every poll. Watching
//!   needs the compositor's data-control protocol (KDE, wlroots desktops;
//!   not GNOME), so images sync only there.

use crate::dbus::{Connection, Value};
use sard_core::media::Clipboard;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

enum Backend {
    Klipper(Connection),
    WlClipboard,
}

/// Change notifications from `wl-paste --watch`, and what the clipboard
/// offered at the last change.
struct Watch {
    child: Child,
    changes: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    seen: u64,
    has_text: bool,
    has_png: bool,
    /// None: not read since the last change.
    image: Option<Option<Arc<[u8]>>>,
}

impl Watch {
    fn start() -> Option<Watch> {
        // The command gets the contents on stdin at every change: drain them
        // (so the source app never sees a broken pipe) and print a line.
        let mut child = Command::new("wl-paste")
            .args(["--watch", "sh", "-c", "cat >/dev/null; echo"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let out = child.stdout.take()?;
        let changes = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let (c, a) = (Arc::clone(&changes), Arc::clone(&alive));
        std::thread::Builder::new()
            .name("sard-clipboard-watch".into())
            .spawn(move || {
                for line in BufReader::new(out).lines() {
                    if line.is_err() {
                        break;
                    }
                    c.fetch_add(1, Ordering::Relaxed);
                }
                // wl-paste exited: no data-control protocol, or we're shutting down.
                a.store(false, Ordering::Relaxed);
            })
            .ok()?;
        Some(Watch { child, changes, alive, seen: u64::MAX, has_text: false, has_png: false, image: None })
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Re-read the offered types if the clipboard changed.
    fn refresh(&mut self) {
        let n = self.changes.load(Ordering::Relaxed);
        if n == self.seen {
            return;
        }
        self.seen = n;
        self.image = None;
        let types = Command::new("wl-paste")
            .arg("--list-types")
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let types: Vec<&str> = types.lines().map(str::trim).collect();
        self.has_text = types.iter().any(|t| is_text_type(t));
        self.has_png = types.contains(&"image/png");
    }

    fn image(&mut self) -> Option<Arc<[u8]>> {
        self.refresh();
        if self.has_text || !self.has_png {
            return None;
        }
        self.image
            .get_or_insert_with(|| {
                let out = Command::new("wl-paste")
                    .args(["--no-newline", "--type", "image/png"])
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                (out.status.success() && out.stdout.starts_with(b"\x89PNG")).then(|| out.stdout.into())
            })
            .clone()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_text_type(t: &str) -> bool {
    t.starts_with("text/plain") || matches!(t, "UTF8_STRING" | "STRING" | "TEXT")
}

/// Klipper describes an image as text like "▨ 1920x1080 32bpp": that's not
/// something to paste.
fn is_klipper_image_label(t: &str) -> bool {
    t.starts_with('▨') && t.len() < 64 && !t.contains('\n')
}

pub struct LinuxClipboard {
    backend: Backend,
    watch: Option<Watch>,
}

fn has_wl_clipboard() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && Command::new("wl-paste")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

impl LinuxClipboard {
    pub fn open() -> Option<Self> {
        let wl = has_wl_clipboard();
        let watch = if wl { Watch::start() } else { None };
        if let Ok(c) = Connection::session()
            && c.call("org.kde.klipper", "/klipper", "org.kde.klipper.klipper", "getClipboardContents", &[]).is_ok()
        {
            return Some(Self { backend: Backend::Klipper(c), watch });
        }
        wl.then_some(Self { backend: Backend::WlClipboard, watch })
    }

    pub fn backend_name(&self) -> &'static str {
        match (&self.backend, &self.watch) {
            (Backend::Klipper(_), Some(_)) => "Klipper (D-Bus), images via wl-clipboard",
            (Backend::Klipper(_), None) => "Klipper (D-Bus); install wl-clipboard to sync images",
            (Backend::WlClipboard, _) => "wl-clipboard",
        }
    }

    /// The change watcher, while it's running.
    fn watch(&mut self) -> Option<&mut Watch> {
        self.watch.as_mut().filter(|w| w.alive())
    }
}

impl Clipboard for LinuxClipboard {
    fn get(&mut self) -> Option<String> {
        // An image with no text: nothing to read as text.
        if let Some(w) = self.watch() {
            w.refresh();
            if !w.has_text && w.has_png {
                return None;
            }
        }
        match &self.backend {
            Backend::Klipper(c) => c
                .call("org.kde.klipper", "/klipper", "org.kde.klipper.klipper", "getClipboardContents", &[])
                .ok()
                .and_then(|m| m.body.into_iter().next())
                .and_then(|v| v.as_str().map(String::from))
                .filter(|t| !is_klipper_image_label(t)),
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
            Backend::WlClipboard => wl_copy("text/plain", text.as_bytes()),
        }
    }

    fn get_image(&mut self) -> Option<Arc<[u8]>> {
        self.watch()?.image()
    }

    fn set_image(&mut self, png: &[u8]) -> bool {
        if self.watch.is_none() && !has_wl_clipboard() {
            return false;
        }
        wl_copy("image/png", png);
        true
    }
}

/// Offer `data` as the clipboard. wl-copy keeps serving it from a background
/// process until something else is copied.
fn wl_copy(mime: &str, data: &[u8]) {
    if let Ok(mut child) = Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data);
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_rules() {
        assert!(is_text_type("text/plain;charset=utf-8"));
        assert!(is_text_type("UTF8_STRING"));
        assert!(!is_text_type("text/html"));
        assert!(!is_text_type("image/png"));
        assert!(is_klipper_image_label("▨ 1920x1080 32bpp"));
        assert!(!is_klipper_image_label("hello"));
    }
}
