//! Two-way clipboard sync over the reliable channel: text, and images as PNG.
//! Polls the local clipboard (cheap: backends cache by the system's change
//! counter) and suppresses echoes of what just arrived from the peer.
//!
//! Images go in pieces, each sent only once the channel has drained, so a
//! big screenshot never queues up in front of keystrokes.

use crate::platform;
use sard_core::media::Clipboard;
use sard_core::net::Connection;
use sard_core::proto::Control;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Larger texts are not synced.
const MAX_TEXT: usize = 1 << 20;
/// Larger images are not synced (a 4K screenshot is 2–15 MB as PNG).
pub const MAX_IMAGE: usize = 48 << 20;
/// Image piece size.
const PIECE: usize = 32 << 10;
/// Reliable segments allowed in flight before the next piece is sent.
const MAX_BACKLOG: usize = 128;

#[derive(Clone, PartialEq)]
enum Content {
    Text(String),
    /// PNG.
    Image(Arc<[u8]>),
}

/// What's on the clipboard now: text wins over an image.
fn current(cb: &Mutex<Box<dyn Clipboard>>) -> Option<Content> {
    let mut cb = cb.lock().unwrap();
    match cb.get() {
        Some(t) if !t.is_empty() => Some(Content::Text(t)),
        _ => cb.get_image().filter(|p| !p.is_empty()).map(Content::Image),
    }
}

/// Equal, or the same picture in a different PNG encoding (a clipboard
/// manager taking over an image may re-encode it): not worth sending again.
fn same_content(a: &Content, b: &Content) -> bool {
    match (a, b) {
        (Content::Image(x), Content::Image(y)) if x != y => {
            let decode = |p: &[u8]| sard_core::png::decode(p).map(|i| (i.width, i.height, i.rgba));
            decode(x).is_some_and(|x| decode(y).as_ref() == Some(&x))
        }
        _ => a == b,
    }
}

struct Incoming {
    serial: u32,
    total: usize,
    buf: Vec<u8>,
}

pub struct ClipSync {
    cb: Arc<Mutex<Box<dyn Clipboard>>>,
    last: Arc<Mutex<Option<Content>>>,
    incoming: Mutex<Option<Incoming>>,
    stop: Arc<AtomicBool>,
}

impl ClipSync {
    pub fn start(conn: Connection) -> Option<Self> {
        Self::with(conn, platform::open_clipboard()?)
    }

    fn with(conn: Connection, cb: Box<dyn Clipboard>) -> Option<Self> {
        let cb: Arc<Mutex<Box<dyn Clipboard>>> = Arc::new(Mutex::new(cb));
        // Don't push whatever is already on the clipboard at connect time.
        let last = Arc::new(Mutex::new(current(&cb)));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (cb, last, stop) = (Arc::clone(&cb), Arc::clone(&last), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("sard-clipboard".into())
                .spawn(move || {
                    let mut serial = 0u32;
                    while !stop.load(Ordering::Relaxed) && !conn.is_closed() {
                        std::thread::sleep(Duration::from_millis(300));
                        let Some(now) = current(&cb) else { continue };
                        {
                            let mut l = last.lock().unwrap();
                            let same = l.as_ref().is_some_and(|l| same_content(l, &now));
                            *l = Some(now.clone());
                            if same {
                                continue;
                            }
                        }
                        match now {
                            Content::Text(text) if text.len() <= MAX_TEXT => {
                                sard_core::debug!("clipboard → peer ({} bytes of text)", text.len());
                                conn.send_control(&Control::Clipboard { text });
                            }
                            Content::Image(png) if png.len() <= MAX_IMAGE => {
                                serial = serial.wrapping_add(1);
                                send_image(&conn, &png, serial, &last, &stop);
                            }
                            _ => sard_core::debug!("clipboard too large to sync"),
                        }
                    }
                })
                .ok()?;
        }
        Some(Self { cb, last, incoming: Mutex::new(None), stop })
    }

    /// Text arrived from the peer: put it on the local clipboard.
    pub fn on_remote(&self, text: String) {
        *self.incoming.lock().unwrap() = None;
        if text.len() > MAX_TEXT {
            return;
        }
        sard_core::debug!("clipboard ← peer ({} bytes of text)", text.len());
        *self.last.lock().unwrap() = Some(Content::Text(text.clone()));
        self.cb.lock().unwrap().set(&text);
    }

    /// A piece of an image from the peer; the last piece puts it on the
    /// local clipboard.
    pub fn on_remote_image(&self, serial: u32, total: u32, offset: u32, data: Vec<u8>) {
        let png = {
            let mut inc = self.incoming.lock().unwrap();
            let total = total as usize;
            if offset == 0 {
                *inc = (total <= MAX_IMAGE).then(|| Incoming { serial, total, buf: Vec::with_capacity(total) });
            }
            let Some(i) = inc.as_mut() else { return };
            if i.serial != serial
                || i.total != total
                || i.buf.len() != offset as usize
                || i.buf.len() + data.len() > total
            {
                *inc = None;
                return;
            }
            i.buf.extend_from_slice(&data);
            if i.buf.len() < total {
                return;
            }
            let png: Arc<[u8]> = std::mem::take(&mut i.buf).into();
            *inc = None;
            png
        };
        sard_core::debug!("clipboard ← peer ({} bytes of PNG)", png.len());
        *self.last.lock().unwrap() = Some(Content::Image(Arc::clone(&png)));
        if !self.cb.lock().unwrap().set_image(&png) {
            sard_core::debug!("clipboard images aren't supported here");
        }
    }
}

/// Send `png` in pieces, pacing on the reliable channel. Gives up when the
/// connection closes or the clipboard moves on (a newer copy here or from
/// the peer): the peer drops an unfinished image when the next one starts.
fn send_image(conn: &Connection, png: &Arc<[u8]>, serial: u32, last: &Mutex<Option<Content>>, stop: &AtomicBool) {
    sard_core::debug!("clipboard → peer ({} bytes of PNG)", png.len());
    let current = Some(Content::Image(Arc::clone(png)));
    for (i, data) in png.chunks(PIECE).enumerate() {
        while conn.reliable_backlog() > MAX_BACKLOG {
            if stop.load(Ordering::Relaxed) || conn.is_closed() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if *last.lock().unwrap() != current {
            sard_core::debug!("clipboard image superseded; stopped sending");
            return;
        }
        conn.send_control(&Control::ClipboardImage {
            serial,
            total: png.len() as u32,
            offset: (i * PIECE) as u32,
            data: data.to_vec(),
        });
    }
}

impl Drop for ClipSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sard_core::crypto::Keypair;
    use sard_core::net::{Client, Event, Host, HostOptions, NetConfig};
    use std::collections::HashMap;
    use std::time::Instant;

    #[derive(Default)]
    struct State {
        text: Option<String>,
        image: Option<Arc<[u8]>>,
        sets: usize,
    }

    /// An in-memory clipboard the test can look at.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl Clipboard for Fake {
        fn get(&mut self) -> Option<String> {
            self.0.lock().unwrap().text.clone()
        }
        fn set(&mut self, text: &str) {
            let mut s = self.0.lock().unwrap();
            (s.text, s.image, s.sets) = (Some(text.into()), None, s.sets + 1);
        }
        fn get_image(&mut self) -> Option<Arc<[u8]>> {
            let s = self.0.lock().unwrap();
            if s.text.is_some() { None } else { s.image.clone() }
        }
        fn set_image(&mut self, png: &[u8]) -> bool {
            let mut s = self.0.lock().unwrap();
            (s.text, s.image, s.sets) = (None, Some(png.into()), s.sets + 1);
            true
        }
    }

    impl Fake {
        fn copy_image(&self, png: &[u8]) {
            let mut s = self.0.lock().unwrap();
            (s.text, s.image) = (None, Some(png.into()));
        }
        fn copy_text(&self, t: &str) {
            let mut s = self.0.lock().unwrap();
            (s.text, s.image) = (Some(t.into()), None);
        }
        fn wait(&self, what: &str, f: impl Fn(&State) -> bool) -> Duration {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(20) {
                if f(&self.0.lock().unwrap()) {
                    return start.elapsed();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for {what}");
        }
    }

    /// Feed clipboard messages from the peer into `clip` until the connection closes.
    fn pump(conn: Connection, cb: Fake) {
        std::thread::spawn(move || {
            let events = conn.take_events().unwrap();
            let clip = ClipSync::with(conn.clone(), Box::new(cb)).unwrap();
            while !conn.is_closed() {
                match events.recv_timeout(Duration::from_millis(100)) {
                    Ok(Event::Control(Control::Clipboard { text })) => clip.on_remote(text),
                    Ok(Event::Control(Control::ClipboardImage { serial, total, offset, data })) => {
                        clip.on_remote_image(serial, total, offset, data)
                    }
                    _ => {}
                }
            }
        });
    }

    /// Two ends over real UDP: images and text cross both ways, big images
    /// arrive whole, and nothing echoes back.
    #[test]
    fn images_and_text_sync_both_ways() {
        let (host_id, client_id) = (Keypair::generate(), Keypair::generate());
        let host_key = host_id.public();
        let trusted = HashMap::from([(client_id.public(), "laptop".to_string())]);
        let opts = HostOptions { trusted, pairing_code: None, on_paired: None };
        let host = Host::bind("127.0.0.1:0".parse().unwrap(), host_id, NetConfig::default(), opts).unwrap();
        let addr = host.local_addr().unwrap().to_string();
        let (a, b) = (Fake::default(), Fake::default());
        let a2 = a.clone();
        std::thread::spawn(move || {
            if let Some(conn) = host.accept(Duration::from_secs(10)) {
                pump(conn, a2);
            }
            std::thread::sleep(Duration::from_secs(60));
        });
        let client = Client::connect(&addr, host_key, client_id, NetConfig::default(), Duration::from_secs(5)).unwrap();
        pump(client.clone(), b.clone());
        std::thread::sleep(Duration::from_millis(400));

        // A big (incompressible) image: ~6 MB in 32 KB pieces.
        let mut r = sard_core::rand::XorShift(5);
        let big: Vec<u8> = (0..6_000_000).map(|_| r.next_u64() as u8).collect();
        a.copy_image(&big);
        let t = b.wait("big image", |s| s.image.as_deref() == Some(&big[..]));
        eprintln!("6 MB image crossed in {t:?}");

        b.copy_text("from b ✂");
        a.wait("text", |s| s.text.as_deref() == Some("from b ✂"));

        let small = b"\x89PNG small".to_vec();
        b.copy_image(&small);
        a.wait("small image", |s| s.image.as_deref() == Some(&small[..]) && s.text.is_none());

        // A re-encoded copy of the same picture isn't sent back.
        let px = [10u8, 20, 30, 255].repeat(64);
        let png = sard_core::png::encode_rgba(8, 8, &px);
        b.copy_image(&png);
        a.wait("png", |s| s.image.as_deref() == Some(&png[..]));
        let mut other = sard_core::png::encode_rgb(8, 8, &[10u8, 20, 30].repeat(64));
        assert_ne!(other, png);
        a.copy_image(&other);
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(b.0.lock().unwrap().image.as_deref(), Some(&png[..]), "not echoed");
        other.clear();

        // Each side was written only by the peer, once per copy: no echoes.
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(b.0.lock().unwrap().sets, 1, "b: the big image only");
        assert_eq!(a.0.lock().unwrap().sets, 3, "a: text, the small image, the png");
        drop(client);
    }
}
