//! Two-way text clipboard sync over the reliable channel. Polls the local
//! clipboard (cheap: one D-Bus call / pasteboard read) and suppresses echoes
//! of text that just arrived from the peer.

use crate::platform;
use sard_core::media::Clipboard;
use sard_core::net::Connection;
use sard_core::proto::Control;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Larger texts are not synced (the reliable channel is for small messages).
const MAX_TEXT: usize = 1 << 20;

pub struct ClipSync {
    cb: Arc<Mutex<Box<dyn Clipboard>>>,
    last: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
}

impl ClipSync {
    pub fn start(conn: Connection) -> Option<Self> {
        let mut cb = platform::open_clipboard()?;
        let initial = cb.get();
        let cb = Arc::new(Mutex::new(cb));
        // Don't push whatever is already on the clipboard at connect time.
        let last = Arc::new(Mutex::new(initial));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (cb, last, stop) = (Arc::clone(&cb), Arc::clone(&last), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("sard-clipboard".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) && !conn.is_closed() {
                        std::thread::sleep(Duration::from_millis(300));
                        let Some(text) = cb.lock().unwrap().get() else { continue };
                        let mut l = last.lock().unwrap();
                        if l.as_deref() != Some(text.as_str()) && !text.is_empty() && text.len() <= MAX_TEXT {
                            *l = Some(text.clone());
                            drop(l);
                            sard_core::debug!("clipboard → peer ({} bytes)", text.len());
                            conn.send_control(&Control::Clipboard { text });
                        }
                    }
                })
                .ok()?;
        }
        Some(Self { cb, last, stop })
    }

    /// Text arrived from the peer: put it on the local clipboard.
    pub fn on_remote(&self, text: String) {
        if text.len() > MAX_TEXT {
            return;
        }
        sard_core::debug!("clipboard ← peer ({} bytes)", text.len());
        *self.last.lock().unwrap() = Some(text.clone());
        self.cb.lock().unwrap().set(&text);
    }
}

impl Drop for ClipSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
