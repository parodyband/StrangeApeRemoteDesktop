//! Manual test for the macOS menu bar item (`platform::create_tray`): a winit
//! event loop with a placeholder icon, a few items, a submenu; clicks are
//! logged and update the menu.
//!
//! ```sh
//! cargo run -p sard-macos --example tray_demo [-- --self-test] [--quit-after <secs>]
//! ```
//!
//! Run it from an app bundle with `LSUIElement` (as Sard.app is) so it has no
//! Dock icon. `--self-test` clicks a few items programmatically, prints the
//! menu before and after, checks the result and quits.

#[cfg(target_os = "macos")]
mod demo {
    use sard_core::media::{MenuEntry, Rgba, Tray, TrayIcons};
    use sard_macos::platform;
    use sard_macos::tray::MacTray;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
    use winit::platform::macos::EventLoopBuilderExtMacOS;
    use winit::window::WindowId;

    const SHARE: u32 = 1;
    const ACTIVE: u32 = 2;
    const DOCK: u32 = 3;
    const PROMPT: u32 = 4;
    const DISPLAY0: u32 = 10;
    const QUIT: u32 = 99;

    enum Event {
        Click(u32),
        Dumped { label: &'static str, text: String },
    }

    type Proxy = Arc<Mutex<EventLoopProxy<Event>>>;

    #[derive(Default)]
    struct State {
        sharing: bool,
        active: bool,
        dock: bool,
        display: u32,
    }

    fn menu(s: &State) -> Vec<MenuEntry> {
        let item = |id, label: &str, checked| MenuEntry::Item { id, label: label.into(), enabled: true, checked };
        vec![
            MenuEntry::Item { id: 0, label: "Sard tray demo".into(), enabled: false, checked: None },
            MenuEntry::Separator,
            item(SHARE, "Share this screen", Some(s.sharing)),
            MenuEntry::Submenu {
                label: "Display".into(),
                enabled: true,
                items: (0..3)
                    .map(|i| item(DISPLAY0 + i, &format!("Display {}", i + 1), Some(s.display == i)))
                    .collect(),
            },
            MenuEntry::Submenu {
                label: "Disabled submenu".into(),
                enabled: false,
                items: vec![item(50, "unreachable", None)],
            },
            item(ACTIVE, "Active icon", Some(s.active)),
            item(DOCK, "Show Dock icon", Some(s.dock)),
            item(PROMPT, "Prompt…", None),
            MenuEntry::Separator,
            item(QUIT, "Quit", None),
        ]
    }

    /// 44x44 (22 pt @2x) template placeholder: a ring with a dot, or a disc.
    fn icon(filled: bool) -> Rgba {
        let n = 44usize;
        let c = (n as f32 - 1.0) / 2.0;
        let mut pixels = vec![0u8; n * n * 4];
        for y in 0..n {
            for x in 0..n {
                let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                if if filled { d <= 16.0 } else { (12.0..=16.0).contains(&d) || d <= 4.0 } {
                    pixels[(y * n + x) * 4 + 3] = 255;
                }
            }
        }
        Rgba { width: n as u32, height: n as u32, pixels }
    }

    struct App {
        proxy: Proxy,
        tray: Option<MacTray>,
        state: State,
        self_test: bool,
        clicks: usize,
        deadline: Option<Instant>,
    }

    impl App {
        fn dump(&self, label: &'static str) {
            let proxy = Arc::clone(&self.proxy);
            if let Some(t) = &self.tray {
                t.debug_dump(move |text| {
                    let _ = proxy.lock().unwrap().send_event(Event::Dumped { label, text });
                });
            }
        }
    }

    impl ApplicationHandler<Event> for App {
        fn resumed(&mut self, _el: &ActiveEventLoop) {
            if self.tray.is_some() {
                return;
            }
            let icons =
                TrayIcons { mono: icon(false), mono_active: icon(true), color: icon(false), color_active: icon(true) };
            let proxy = Arc::clone(&self.proxy);
            let on_click = Box::new(move |id: u32| {
                eprintln!("tray_demo: callback id={id} on main thread: {}", objc2::MainThreadMarker::new().is_some());
                let _ = proxy.lock().unwrap().send_event(Event::Click(id));
            });
            let tray = match MacTray::new(icons, on_click) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("tray_demo: create_tray failed: {e}");
                    std::process::exit(1);
                }
            };
            tray.set_menu(menu(&self.state));
            tray.set_tooltip("Sard tray demo");
            eprintln!(
                "tray_demo: tray created; activation policy {:?}, launch at login {}",
                sard_macos::shell::activation_policy(),
                platform::launch_at_login()
            );
            self.tray = Some(tray);
            if self.self_test {
                self.dump("initial");
                let t = self.tray.as_ref().unwrap();
                t.debug_click(SHARE);
                t.debug_click(DISPLAY0 + 2); // in the submenu
                t.debug_click(ACTIVE);
            }
        }

        fn user_event(&mut self, el: &ActiveEventLoop, ev: Event) {
            match ev {
                Event::Click(id) => {
                    eprintln!("tray_demo: clicked {id}");
                    let s = &mut self.state;
                    match id {
                        SHARE => s.sharing = !s.sharing,
                        ACTIVE => s.active = !s.active,
                        DOCK => {
                            s.dock = !s.dock;
                            platform::set_dock_visible(s.dock);
                        }
                        PROMPT => {
                            std::thread::spawn(|| {
                                let r = platform::prompt_text("Sard tray demo", "Type something:");
                                eprintln!("tray_demo: prompt returned {r:?}");
                                platform::show_message("Sard tray demo", &format!("You typed: {r:?}"));
                            });
                        }
                        QUIT => el.exit(),
                        d if (DISPLAY0..DISPLAY0 + 3).contains(&d) => s.display = d - DISPLAY0,
                        _ => {}
                    }
                    if let Some(t) = &self.tray {
                        t.set_menu(menu(&self.state));
                        t.set_active(self.state.active);
                    }
                    self.clicks += 1;
                    if self.self_test && self.clicks == 3 {
                        self.dump("after clicks");
                    }
                }
                Event::Dumped { label, text } => {
                    eprintln!("tray_demo: menu ({label}):\n{text}");
                    if self.self_test && label == "after clicks" {
                        let ok = text.contains("active=true")
                            && text.contains("tooltip=\"Sard tray demo\"")
                            && text.contains("[x] \"Share this screen\" tag=1")
                            && text.contains("[x] \"Display 3\" tag=12")
                            && text.contains("[ ] \"Display 1\" tag=10")
                            && text.contains("\"Disabled submenu\" tag=0 (disabled) >")
                            && text.contains("\"Sard tray demo\" tag=0 (disabled)");
                        eprintln!("tray_demo: SELF-TEST {}", if ok { "OK" } else { "FAILED" });
                        el.exit();
                    }
                }
            }
        }

        fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, _ev: WindowEvent) {}

        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            if let Some(d) = self.deadline {
                if Instant::now() >= d {
                    eprintln!("tray_demo: time is up");
                    el.exit();
                } else {
                    el.set_control_flow(ControlFlow::WaitUntil(d));
                }
            }
        }
    }

    pub fn main() {
        sard_core::log::init();
        let args: Vec<String> = std::env::args().collect();
        let self_test = args.iter().any(|a| a == "--self-test");
        let quit_after = args
            .iter()
            .position(|a| a == "--quit-after")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f64>().ok())
            .or(self_test.then_some(10.0));
        let event_loop = EventLoop::<Event>::with_user_event()
            .with_activate_ignoring_other_apps(false) // don't steal focus at launch
            .with_default_menu(false)
            .build()
            .expect("event loop");
        let mut app = App {
            proxy: Arc::new(Mutex::new(event_loop.create_proxy())),
            tray: None,
            state: State::default(),
            self_test,
            clicks: 0,
            deadline: quit_after.map(|s| Instant::now() + Duration::from_secs_f64(s)),
        };
        event_loop.run_app(&mut app).expect("event loop");
        drop(app.tray.take());
        eprintln!("tray_demo: exit");
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    demo::main();
    #[cfg(not(target_os = "macos"))]
    eprintln!("tray_demo is macOS only");
}
