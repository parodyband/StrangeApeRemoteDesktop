//! Menu bar item: an `NSStatusItem` with a template image and an `NSMenu`
//! built from [`MenuEntry`]s. Item clicks reach the app's callback through a
//! small target object (`-itemClicked:`), keyed by the `NSMenuItem` tag.
//!
//! AppKit objects live on the main thread only: the tray is created there,
//! and every later update is queued onto the main dispatch queue (also when
//! called on the main thread, so replacing the menu from inside a menu
//! callback never frees the menu that is still dispatching the click).

use crate::ffi::CGSize;
use crate::objc::{main_async, ns_string, to_string};
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, class, define_class, msg_send, sel};
use sard_core::media::{self, MenuEntry, Rgba, Tray, TrayCallback, TrayIcons};
use std::fmt::Write;
use std::sync::Arc;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSDeviceRGBColorSpace: *const AnyObject;
}

/// `NSVariableStatusItemLength`.
const VARIABLE_LENGTH: f64 = -1.0;
/// `NSBitmapFormatAlphaNonpremultiplied`.
const ALPHA_NONPREMULTIPLIED: usize = 1 << 1;
/// Menu bar icons taller than this (points) are scaled down.
const MAX_ICON_POINTS: f64 = 22.0;

struct TargetIvars {
    on_click: TrayCallback,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(itemClicked:))]
        fn item_clicked(&self, sender: *mut AnyObject) {
            if sender.is_null() {
                return;
            }
            // SAFETY: the sender of our action is an NSMenuItem.
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Ok(id) = u32::try_from(tag) else { return };
            let cb = &self.ivars().on_click;
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(id))).is_err() {
                sard_core::error!("tray: panic in menu callback");
            }
        }
    }
);

impl MenuTarget {
    fn new(on_click: TrayCallback) -> Retained<Self> {
        let this = Self::alloc().set_ivars(TargetIvars { on_click });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Main-thread-only AppKit state. Only ever touched (and dropped) on the
/// main thread: see [`MacTray::on_main`] and `Drop`.
struct Ui {
    item: Retained<AnyObject>,
    target: Retained<MenuTarget>,
    icon: Retained<AnyObject>,
    icon_active: Retained<AnyObject>,
}

struct MainOnly(Ui);
// SAFETY: the wrapped objects are only accessed on the main thread (all access
// goes through closures queued on the main dispatch queue, and the last
// reference is released there too).
unsafe impl Send for MainOnly {}
unsafe impl Sync for MainOnly {}

/// A live menu bar item; see the module docs.
pub struct MacTray {
    ui: Option<Arc<MainOnly>>,
}

/// Point size for a menu bar image: art taller than 32 px is taken as @2x
/// (e.g. 44 px → 22 pt), then capped at [`MAX_ICON_POINTS`] high.
pub fn icon_points(width: u32, height: u32) -> (f64, f64) {
    let scale = if height > 32 { 2.0 } else { 1.0 };
    let (w, h) = (width as f64 / scale, height as f64 / scale);
    let fit = (MAX_ICON_POINTS / h).min(1.0);
    (w * fit, h * fit)
}

/// `NSImage` from straight-alpha RGBA (`template`: only alpha matters and
/// AppKit tints it for light/dark menu bars).
pub fn ns_image(img: &Rgba, template: bool) -> Option<Retained<AnyObject>> {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 || img.pixels.len() < w * h * 4 {
        return None;
    }
    // SAFETY: NSBitmapImageRep allocates its own buffer of `bytesPerRow * h`
    // bytes, which we fill row by row; NSImage retains the rep.
    unsafe {
        let rep: Allocated<AnyObject> = msg_send![class!(NSBitmapImageRep), alloc];
        let planes: *mut *mut u8 = std::ptr::null_mut();
        let rep: Option<Retained<AnyObject>> = msg_send![
            rep,
            initWithBitmapDataPlanes: planes,
            pixelsWide: w as isize,
            pixelsHigh: h as isize,
            bitsPerSample: 8isize,
            samplesPerPixel: 4isize,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: NSDeviceRGBColorSpace,
            bitmapFormat: ALPHA_NONPREMULTIPLIED,
            bytesPerRow: (w * 4) as isize,
            bitsPerPixel: 32isize
        ];
        let rep = rep?;
        let data: *mut u8 = msg_send![&*rep, bitmapData];
        let stride: isize = msg_send![&*rep, bytesPerRow];
        if data.is_null() || (stride as usize) < w * 4 {
            return None;
        }
        for (y, row) in img.pixels.chunks_exact(w * 4).take(h).enumerate() {
            std::ptr::copy_nonoverlapping(row.as_ptr(), data.add(y * stride as usize), w * 4);
        }
        let (pw, ph) = icon_points(img.width, img.height);
        let image: Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
        let image: Retained<AnyObject> = msg_send![image, initWithSize: CGSize { width: pw, height: ph }];
        let _: () = msg_send![&*image, addRepresentation: &*rep];
        let _: () = msg_send![&*image, setTemplate: template];
        Some(image)
    }
}

fn menu_item(label: &str, action: Option<Sel>) -> Retained<AnyObject> {
    let (title, key) = (ns_string(label), ns_string(""));
    // SAFETY: NSMenuItem's designated initializer.
    unsafe {
        let a: Allocated<AnyObject> = msg_send![class!(NSMenuItem), alloc];
        msg_send![a, initWithTitle: &*title, action: action, keyEquivalent: &*key]
    }
}

/// Build an `NSMenu` (and submenus) whose items send `-itemClicked:` to `target`.
fn build_menu(title: &str, entries: &[MenuEntry], target: &MenuTarget) -> Retained<AnyObject> {
    let title = ns_string(title);
    // SAFETY: standard NSMenu / NSMenuItem setters with valid objects.
    unsafe {
        let menu: Allocated<AnyObject> = msg_send![class!(NSMenu), alloc];
        let menu: Retained<AnyObject> = msg_send![menu, initWithTitle: &*title];
        // Our `enabled` flags are authoritative (no validateMenuItem: pass).
        let _: () = msg_send![&*menu, setAutoenablesItems: false];
        for e in entries {
            let item: Retained<AnyObject> = match e {
                MenuEntry::Separator => msg_send![class!(NSMenuItem), separatorItem],
                MenuEntry::Item { id, label, enabled, checked } => {
                    let item = menu_item(label, Some(sel!(itemClicked:)));
                    let _: () = msg_send![&*item, setTarget: target];
                    let _: () = msg_send![&*item, setTag: *id as isize];
                    let _: () = msg_send![&*item, setEnabled: *enabled];
                    // NSControlStateValueOn = 1, Off = 0.
                    let _: () = msg_send![&*item, setState: isize::from(*checked == Some(true))];
                    item
                }
                MenuEntry::Submenu { label, enabled, items } => {
                    let item = menu_item(label, None);
                    let sub = build_menu(label, items, target);
                    let _: () = msg_send![&*item, setSubmenu: &*sub];
                    let _: () = msg_send![&*item, setEnabled: *enabled];
                    item
                }
            };
            let _: () = msg_send![&*menu, addItem: &*item];
        }
        menu
    }
}

impl Ui {
    fn button(&self) -> *mut AnyObject {
        // SAFETY: NSStatusItem accessor (+0).
        unsafe { msg_send![&*self.item, button] }
    }

    fn set_menu(&self, entries: &[MenuEntry]) {
        let menu = build_menu("Sard", entries, &self.target);
        // SAFETY: valid status item and menu.
        unsafe {
            let _: () = msg_send![&*self.item, setMenu: &*menu];
        }
    }

    fn set_tooltip(&self, text: &str) {
        let button = self.button();
        if !button.is_null() {
            let s = ns_string(text);
            // SAFETY: NSView setter.
            unsafe {
                let _: () = msg_send![button, setToolTip: &*s];
            }
        }
    }

    fn set_active(&self, active: bool) {
        let button = self.button();
        if !button.is_null() {
            let img = if active { &self.icon_active } else { &self.icon };
            // SAFETY: NSButton setter.
            unsafe {
                let _: () = msg_send![button, setImage: &**img];
            }
        }
    }

    /// The menu (or submenu) containing the item with `tag`, and its index.
    fn find_item(menu: *mut AnyObject, tag: isize) -> Option<(*mut AnyObject, isize)> {
        if menu.is_null() {
            return None;
        }
        // SAFETY: NSMenu / NSMenuItem accessors (+0).
        unsafe {
            let n: isize = msg_send![menu, numberOfItems];
            for i in 0..n {
                let item: *mut AnyObject = msg_send![menu, itemAtIndex: i];
                let sub: *mut AnyObject = msg_send![item, submenu];
                if !sub.is_null() {
                    if let Some(found) = Self::find_item(sub, tag) {
                        return Some(found);
                    }
                } else {
                    let (t, action): (isize, Option<Sel>) = (msg_send![item, tag], msg_send![item, action]);
                    if t == tag && action == Some(sel!(itemClicked:)) {
                        return Some((menu, i));
                    }
                }
            }
        }
        None
    }

    fn dump_menu(out: &mut String, menu: *mut AnyObject, depth: usize) {
        if menu.is_null() {
            return;
        }
        // SAFETY: NSMenu / NSMenuItem accessors (+0).
        unsafe {
            let n: isize = msg_send![menu, numberOfItems];
            for i in 0..n {
                let item: *mut AnyObject = msg_send![menu, itemAtIndex: i];
                let indent = "  ".repeat(depth);
                if msg_send![item, isSeparatorItem] {
                    let _ = writeln!(out, "{indent}---");
                    continue;
                }
                let title: *mut AnyObject = msg_send![item, title];
                let enabled: bool = msg_send![item, isEnabled];
                let state: isize = msg_send![item, state];
                let tag: isize = msg_send![item, tag];
                let sub: *mut AnyObject = msg_send![item, submenu];
                let _ = writeln!(
                    out,
                    "{indent}[{}] {:?} tag={tag}{}{}",
                    if state == 1 { "x" } else { " " },
                    to_string(title),
                    if enabled { "" } else { " (disabled)" },
                    if sub.is_null() { "" } else { " >" }
                );
                Self::dump_menu(out, sub, depth + 1);
            }
        }
    }
}

impl MacTray {
    /// Must be called on the main thread with NSApplication running.
    pub fn new(icons: TrayIcons, on_click: TrayCallback) -> media::Result<Self> {
        if MainThreadMarker::new().is_none() {
            return Err(media::err("the menu bar item must be created on the main thread"));
        }
        autoreleasepool(|_| {
            let icon = ns_image(&icons.mono, true).ok_or_else(|| media::err("invalid tray icon"))?;
            let icon_active = ns_image(&icons.mono_active, true).unwrap_or_else(|| icon.clone());
            // SAFETY: main thread; NSStatusBar returns a +0 status item that we retain.
            let item: Option<Retained<AnyObject>> = unsafe {
                let bar: *mut AnyObject = msg_send![class!(NSStatusBar), systemStatusBar];
                msg_send![bar, statusItemWithLength: VARIABLE_LENGTH]
            };
            let item = item.ok_or_else(|| media::err("cannot create the menu bar item"))?;
            let ui = Ui { item, target: MenuTarget::new(on_click), icon, icon_active };
            ui.set_active(false);
            ui.set_menu(&[]);
            Ok(MacTray { ui: Some(Arc::new(MainOnly(ui))) })
        })
    }

    /// Queue `f` on the main thread with the AppKit state.
    fn on_main(&self, f: impl FnOnce(&Ui) + Send + 'static) {
        let Some(ui) = self.ui.clone() else { return };
        main_async(move || autoreleasepool(|_| f(&ui.0)));
    }

    /// Diagnostics: click the item with `id` the way AppKit does
    /// (`-performActionForItemAtIndex:`), on the main thread.
    #[doc(hidden)]
    pub fn debug_click(&self, id: u32) {
        self.on_main(move |ui| {
            // SAFETY: status item accessor; found menu/index are valid.
            unsafe {
                let menu: *mut AnyObject = msg_send![&*ui.item, menu];
                match Ui::find_item(menu, id as isize) {
                    Some((m, i)) => {
                        let _: () = msg_send![m, performActionForItemAtIndex: i];
                    }
                    None => sard_core::warn!("tray: no menu item {id}"),
                }
            }
        });
    }

    /// Diagnostics: describe the status item and its menu (on the main thread).
    #[doc(hidden)]
    pub fn debug_dump(&self, done: impl FnOnce(String) + Send + 'static) {
        self.on_main(move |ui| {
            let mut out = String::new();
            let button = ui.button();
            // SAFETY: AppKit accessors on valid objects (+0).
            unsafe {
                if !button.is_null() {
                    let image: *mut AnyObject = msg_send![button, image];
                    let tip: *mut AnyObject = msg_send![button, toolTip];
                    let size: CGSize = if image.is_null() { CGSize::default() } else { msg_send![image, size] };
                    let template: bool = !image.is_null() && msg_send![image, isTemplate];
                    let active = std::ptr::eq(image, Retained::as_ptr(&ui.icon_active));
                    let _ = writeln!(
                        out,
                        "button: image {}x{} pt template={template} active={active} tooltip={:?}",
                        size.width,
                        size.height,
                        to_string(tip)
                    );
                }
                let visible: bool = msg_send![&*ui.item, isVisible];
                let _ = writeln!(out, "visible={visible}");
                let menu: *mut AnyObject = msg_send![&*ui.item, menu];
                Ui::dump_menu(&mut out, menu, 0);
            }
            done(out);
        });
    }
}

impl Tray for MacTray {
    fn set_menu(&self, menu: Vec<MenuEntry>) {
        self.on_main(move |ui| ui.set_menu(&menu));
    }

    fn set_tooltip(&self, text: &str) {
        let text = text.to_string();
        self.on_main(move |ui| ui.set_tooltip(&text));
    }

    fn set_active(&self, active: bool) {
        self.on_main(move |ui| ui.set_active(active));
    }
}

impl Drop for MacTray {
    fn drop(&mut self) {
        let Some(ui) = self.ui.take() else { return };
        // Remove the item and release the AppKit objects on the main thread.
        main_async(move || {
            autoreleasepool(|_| {
                // SAFETY: main thread; valid status item.
                unsafe {
                    let bar: *mut AnyObject = msg_send![class!(NSStatusBar), systemStatusBar];
                    let _: () = msg_send![bar, removeStatusItem: &*ui.0.item];
                }
                drop(ui);
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_point_sizes() {
        assert_eq!(icon_points(44, 44), (22.0, 22.0)); // @2x art
        assert_eq!(icon_points(36, 36), (18.0, 18.0));
        assert_eq!(icon_points(18, 18), (18.0, 18.0)); // @1x art
        assert_eq!(icon_points(32, 16), (32.0, 16.0));
        assert_eq!(icon_points(128, 64), (44.0, 22.0)); // capped, aspect kept
    }

    #[test]
    fn image_from_rgba() {
        autoreleasepool(|_| {
            let img = Rgba { width: 44, height: 40, pixels: vec![0x80; 44 * 40 * 4] };
            let image = ns_image(&img, true).expect("NSImage");
            // SAFETY: NSImage accessors.
            unsafe {
                let size: CGSize = msg_send![&*image, size];
                let template: bool = msg_send![&*image, isTemplate];
                assert_eq!((size.width, size.height, template), (22.0, 20.0, true));
                let reps: *mut AnyObject = msg_send![&*image, representations];
                let rep: *mut AnyObject = msg_send![reps, firstObject];
                let (w, h): (isize, isize) = (msg_send![rep, pixelsWide], msg_send![rep, pixelsHigh]);
                assert_eq!((w, h), (44, 40));
                let data: *const u8 = msg_send![rep, bitmapData];
                assert_eq!(*data.add(4 * 44 * 39 + 3), 0x80);
            }
            assert!(ns_image(&Rgba { width: 4, height: 4, pixels: vec![0; 10] }, true).is_none());
        });
    }
}
