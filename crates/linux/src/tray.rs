//! System tray icon via the StatusNotifierItem D-Bus protocol (KDE Plasma,
//! and other desktops with an SNI host) with a com.canonical.dbusmenu menu.
//! (Work in progress.)

use sard_core::media::{self, Tray, TrayCallback, TrayIcons};

pub fn create(_icons: TrayIcons, _on_click: TrayCallback) -> media::Result<Box<dyn Tray>> {
    Err(media::err("system tray not implemented yet"))
}
