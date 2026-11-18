//! A connect attempt in progress. Its viewer window opens right away and
//! shows what's happening (looking on this network, trying the internet,
//! the relay; then any error, with Return to retry) until the session takes
//! the window over.

use crate::Result;
use crate::session::ViewerWindow;
use crate::status::{Painter, Screen};
use sard_core::anywhere::{Progress, Stage};
use sard_core::info;
use sard_core::media::VideoSink;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// What the owner should do after a window event.
pub enum Reply {
    Nothing,
    /// Window closed (or Esc): forget the attempt.
    Cancel,
    /// Return after a failure: dial again.
    Retry,
}

/// Shared between the UI thread, the dialing thread and the painter.
struct State {
    screen: Screen,
    size: (u32, u32),
    scale: f32,
    /// When the current screen started (animation, seconds elapsed).
    since: Instant,
    /// Changed since last painted.
    dirty: bool,
}

pub struct Pending {
    pub id: u64,
    pub label: String,
    /// What the user picked (alias or address), for de-duplication.
    pub target: String,
    window: Arc<Window>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    painter: Option<JoinHandle<Box<dyn VideoSink>>>,
    failed: bool,
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn connecting(label: &str, stage: Stage) -> Screen {
    let detail = match stage {
        Stage::Local => "Looking for it on this network…",
        Stage::Internet => "Trying its home network and internet addresses…",
        Stage::Relay => "No direct route. Connecting through the relay…",
    };
    Screen {
        title: format!("Connecting to {label}"),
        detail: detail.into(),
        hint: "Close this window to cancel".into(),
        busy: true,
    }
}

impl Pending {
    /// Open the window and start showing "Connecting to `label`".
    pub fn open(el: &ActiveEventLoop, id: u64, label: String, target: String) -> Result<Pending> {
        let ViewerWindow { window, sink } = ViewerWindow::create(el, &label, false)?;
        window.set_title(&format!("Sard — Connecting to {label}…"));
        let size = window.inner_size();
        let state = Arc::new(Mutex::new(State {
            screen: connecting(&label, Stage::Local),
            size: (size.width, size.height),
            scale: window.scale_factor() as f32,
            since: Instant::now(),
            dirty: true,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let painter = {
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            std::thread::Builder::new().name("sard-connecting".into()).spawn(move || paint(sink, state, stop))?
        };
        Ok(Pending { id, label, target, window, state, stop, painter: Some(painter), failed: false })
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn focus(&self) {
        self.window.focus_window();
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// For the dialing thread: moves the screen through the [`Stage`]s.
    pub fn progress(&self) -> Progress {
        let (state, label) = (Arc::clone(&self.state), self.label.clone());
        Arc::new(move |stage| {
            let mut st = lock(&state);
            if st.screen.busy {
                info!("connecting to {label}: {stage:?}");
                // The bar keeps its rhythm; only the text changes.
                st.screen = connecting(&label, stage);
                st.dirty = true;
            }
        })
    }

    /// The attempt failed: say why, and offer to try again.
    pub fn fail(&mut self, error: &str) {
        self.failed = true;
        self.window.set_title(&format!("Sard — Couldn't connect to {}", self.label));
        let mut st = lock(&self.state);
        st.screen = Screen {
            title: format!("Couldn't connect to {}", self.label),
            detail: capitalized(error),
            hint: "Press Return to try again · Esc to close".into(),
            busy: false,
        };
        st.dirty = true;
        drop(st);
        self.focus();
    }

    /// Back to "Connecting…" for another attempt.
    pub fn retry(&mut self) {
        self.failed = false;
        self.window.set_title(&format!("Sard — Connecting to {}…", self.label));
        let mut st = lock(&self.state);
        st.screen = connecting(&self.label, Stage::Local);
        st.since = Instant::now();
        st.dirty = true;
    }

    pub fn window_event(&mut self, event: &WindowEvent) -> Reply {
        match event {
            WindowEvent::CloseRequested => return Reply::Cancel,
            WindowEvent::Resized(s) => {
                let mut st = lock(&self.state);
                st.size = (s.width, s.height);
                st.dirty = true;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let mut st = lock(&self.state);
                st.scale = *scale_factor as f32;
                st.dirty = true;
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => return Reply::Cancel,
                    Key::Named(NamedKey::Enter) if self.failed => return Reply::Retry,
                    _ => {}
                }
            }
            _ => {}
        }
        Reply::Nothing
    }

    /// Connected: hand the window and its video output to the session.
    pub fn into_viewer(mut self) -> Option<ViewerWindow> {
        let sink = self.stop_painter()?;
        Some(ViewerWindow { window: Arc::clone(&self.window), sink })
    }

    fn stop_painter(&mut self) -> Option<Box<dyn VideoSink>> {
        self.stop.store(true, Ordering::Relaxed);
        self.painter.take()?.join().ok()
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // The video output goes before the window it draws into.
        drop(self.stop_painter());
    }
}

/// "timed out (…)" → "Timed out (…)": error texts read as sentences here.
fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// The painter thread: owns the video output until the session takes it.
fn paint(mut sink: Box<dyn VideoSink>, state: Arc<Mutex<State>>, stop: Arc<AtomicBool>) -> Box<dyn VideoSink> {
    let mut painter = Painter::default();
    let mut size = (0, 0);
    let mut works = true;
    while !stop.load(Ordering::Relaxed) {
        let (screen, t, scale) = {
            let mut st = lock(&state);
            if st.size != size {
                size = st.size;
                sink.resize(size.0, size.1);
                st.dirty = true;
            }
            if !(st.dirty || st.screen.busy) || !works {
                (None, 0.0, 0.0)
            } else {
                st.dirty = false;
                (Some(st.screen.clone()), st.since.elapsed().as_secs_f32(), st.scale)
            }
        };
        if let Some(screen) = screen {
            let img = painter.render(&screen, size.0, size.1, scale, t);
            if let Err(e) = sink.show_image(img) {
                info!("no connecting screen: {e}");
                works = false;
            }
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    sink
}
