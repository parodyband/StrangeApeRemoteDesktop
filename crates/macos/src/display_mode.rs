//! Display modes ("fit to the viewer's screen"): list a display's modes and
//! switch to one for as long as a session wants it.
//!
//! Changes use `kCGConfigureForAppOnly`, so they never touch the user's
//! saved arrangement and macOS reverts them by itself when Sard exits, even
//! if it crashes. [`Fitted`] also puts the original mode back when dropped.

use crate::cf::{self, CfRef};
use crate::ffi::*;
use sard_core::media::DisplayMode;
use std::time::{Duration, Instant};

/// `DisplayMode` of a `CGDisplayModeRef`.
fn describe_mode(m: &CfRef) -> DisplayMode {
    let p = m.as_ptr();
    // SAFETY: `p` is a valid CGDisplayModeRef for these plain getters.
    unsafe {
        DisplayMode {
            width: CGDisplayModeGetWidth(p) as u32,
            height: CGDisplayModeGetHeight(p) as u32,
            pixel_width: CGDisplayModeGetPixelWidth(p) as u32,
            pixel_height: CGDisplayModeGetPixelHeight(p) as u32,
            refresh: CGDisplayModeGetRefreshRate(p),
        }
    }
}

/// The display's current mode.
pub fn current(display: CGDirectDisplayID) -> Option<(DisplayMode, CfRef)> {
    // SAFETY: Copy rule: we own the returned mode.
    let m = unsafe { CfRef::from_owned(CGDisplayCopyDisplayMode(display)) }?;
    Some((describe_mode(&m), m))
}

/// Modes the desktop can use on `display`, HiDPI variants included.
pub fn modes(display: CGDirectDisplayID) -> Vec<(DisplayMode, CfRef)> {
    // SAFETY: framework constant; a valid CFDictionary of options.
    let opts = unsafe { cf::Dict::new().set(kCGDisplayShowDuplicateLowResolutionModes, cf::boolean(true).cast()) };
    // SAFETY: Copy rule: we own the array; its elements are retained below.
    let Some(list) = (unsafe { CfRef::from_owned(CGDisplayCopyAllDisplayModes(display, opts.as_ptr())) }) else {
        return Vec::new();
    };
    // SAFETY: `list` is a CFArray of CGDisplayModeRefs.
    let n = unsafe { cf::CFArrayGetCount(list.as_ptr()) };
    (0..n)
        .filter_map(|i| {
            // SAFETY: index in range; Get rule, so retain to own it.
            let m = unsafe { CfRef::retain(cf::CFArrayGetValueAtIndex(list.as_ptr(), i)) }?;
            // SAFETY: valid mode.
            let usable = unsafe { CGDisplayModeIsUsableForDesktopGUI(m.as_ptr()) };
            usable.then(|| (describe_mode(&m), m))
        })
        .collect()
}

fn apply(display: CGDirectDisplayID, mode: &CfRef) -> Result<(), String> {
    let mut config = std::ptr::null_mut();
    // SAFETY: standard configuration transaction on a valid display and mode.
    unsafe {
        if CGBeginDisplayConfiguration(&mut config) != 0 {
            return Err("cannot change display settings".into());
        }
        let r = CGConfigureDisplayWithDisplayMode(config, display, mode.as_ptr(), std::ptr::null());
        if r != 0 {
            CGCancelDisplayConfiguration(config);
            return Err(format!("the display refused the mode (error {r})"));
        }
        let r = CGCompleteDisplayConfiguration(config, kCGConfigureForAppOnly);
        if r != 0 {
            return Err(format!("cannot change the display mode (error {r})"));
        }
    }
    // The switch is asynchronous: wait until the display reports it, so
    // capture restarts at the new size.
    let want = describe_mode(mode);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if current(display)
            .is_some_and(|(m, _)| (m.width, m.height, m.pixel_width) == (want.width, want.height, want.pixel_width))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // Let the window server settle before ScreenCaptureKit is asked again.
    std::thread::sleep(Duration::from_millis(150));
    Ok(())
}

/// A display switched to another mode; dropping it switches back.
pub struct Fitted {
    pub display: CGDirectDisplayID,
    original: CfRef,
    pub original_mode: DisplayMode,
}

impl Fitted {
    /// Switch `display` to the mode closest to a `target` desktop size (see
    /// `media::pick_mode`), relative to `previous` (the mode before any fit,
    /// when re-fitting). Returns None when the current mode already fits best.
    pub fn new(
        display: CGDirectDisplayID,
        target: (u32, u32),
        previous: Option<Fitted>,
    ) -> Result<(Option<Fitted>, DisplayMode), String> {
        let (original_mode, original) = match previous {
            Some(mut p) => {
                // Keep the very first mode as the one to restore.
                let o = (p.original_mode, p.original.clone());
                p.display = 0; // nothing to restore on drop: we re-fit
                o
            }
            None => current(display).ok_or("cannot read the display mode")?,
        };
        let list = modes(display);
        let candidates: Vec<DisplayMode> = list.iter().map(|(m, _)| *m).collect();
        let Some(i) = sard_core::media::pick_mode(&candidates, &original_mode, target) else {
            // Already the best fit: go back to (or stay at) the original.
            apply(display, &original)?;
            return Ok((None, original_mode));
        };
        let (mode, cg) = &list[i];
        sard_core::info!(
            "display {display}: {} → {} for the viewer (original mode is restored when the session ends)",
            original_mode.describe(),
            mode.describe()
        );
        apply(display, cg)?;
        Ok((Some(Fitted { display, original, original_mode }), *mode))
    }

    /// Put the original mode back now.
    pub fn restore(mut self) -> Result<DisplayMode, String> {
        let d = std::mem::replace(&mut self.display, 0);
        apply(d, &self.original)?;
        Ok(self.original_mode)
    }
}

impl Drop for Fitted {
    fn drop(&mut self) {
        if self.display != 0 {
            sard_core::info!("display {}: back to {}", self.display, self.original_mode.describe());
            if let Err(e) = apply(self.display, &self.original) {
                sard_core::warn!("cannot restore the display mode: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads modes and picks one for a phone, without switching (needs no
    /// permission, changes nothing).
    #[test]
    fn modes_and_phone_pick() {
        // SAFETY: plain query.
        let main = unsafe { CGMainDisplayID() };
        let (cur, _) = current(main).expect("current mode");
        let list = modes(main);
        assert!(!list.is_empty());
        assert!(list.iter().any(|(m, _)| (m.width, m.height) == (cur.width, cur.height)), "current mode listed");
        let candidates: Vec<DisplayMode> = list.iter().map(|(m, _)| *m).collect();
        let pick = sard_core::media::pick_mode(&candidates, &cur, (1280, 590));
        eprintln!(
            "current {}; {} modes; phone pick: {:?}",
            cur.describe(),
            list.len(),
            pick.map(|i| candidates[i].describe())
        );
        if let Some(i) = pick {
            assert!(candidates[i].width <= cur.width);
        }
    }

    /// Actually switches the main display for two seconds and back. Run by
    /// hand: `cargo test -p sard-macos fit_switch -- --ignored`.
    #[test]
    #[ignore]
    fn fit_switch_and_restore() {
        // SAFETY: plain query.
        let main = unsafe { CGMainDisplayID() };
        let (before, _) = current(main).unwrap();
        let (fitted, mode) = Fitted::new(main, (1280, 590), None).unwrap();
        eprintln!("switched to {}", mode.describe());
        std::thread::sleep(Duration::from_secs(2));
        if let Some(f) = fitted {
            f.restore().unwrap();
        }
        let (after, _) = current(main).unwrap();
        assert_eq!((after.width, after.height, after.pixel_width), (before.width, before.height, before.pixel_width));
    }
}
