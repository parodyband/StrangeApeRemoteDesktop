//! App-shell helpers: modal dialogs (via `osascript`, so they work from any
//! thread without touching the winit run loop), launch at login
//! (`SMAppService`, macOS 13+) and the Dock icon (activation policy).

use crate::objc::{error_string, run_on_main};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{class, msg_send};
use sard_core::media;
use std::process::{Command, Output};

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

/// Text entry dialog; prints the entered text. Title and message come in as
/// script arguments (no quoting issues). Exits non-zero on Cancel.
const PROMPT_SCRIPT: &[&str] = &[
    "on run argv",
    "activate",
    "set r to display dialog (item 2 of argv) with title (item 1 of argv) default answer \"\" \
     buttons {\"Cancel\", \"OK\"} default button \"OK\" cancel button \"Cancel\"",
    "return text returned of r",
    "end run",
];

const MESSAGE_SCRIPT: &[&str] = &[
    "on run argv",
    "activate",
    "display alert (item 1 of argv) message (item 2 of argv) buttons {\"OK\"} default button \"OK\"",
    "end run",
];

/// Yes/no alert; exits non-zero on Cancel.
const CONFIRM_SCRIPT: &[&str] = &[
    "on run argv",
    "activate",
    "display alert (item 1 of argv) message (item 2 of argv) as warning \
     buttons {\"Cancel\", (item 3 of argv)} default button (item 3 of argv) cancel button \"Cancel\"",
    "end run",
];

fn osascript(script: &[&str], args: &[&str]) -> std::io::Result<Output> {
    let mut cmd = Command::new("/usr/bin/osascript");
    for line in script {
        cmd.arg("-e").arg(line);
    }
    // `--` so arguments starting with '-' are not taken as options.
    cmd.arg("--").args(args).output()
}

/// Modal text entry. Blocks until dismissed (any thread); `None` if cancelled
/// or no dialog could be shown.
pub fn prompt_text(title: &str, message: &str) -> Option<String> {
    match osascript(PROMPT_SCRIPT, &[title, message]) {
        Ok(out) if out.status.success() => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            if text.ends_with('\n') {
                text.pop();
            }
            Some(text)
        }
        Ok(out) => {
            sard_core::debug!("prompt dialog: {}", String::from_utf8_lossy(&out.stderr).trim());
            None
        }
        Err(e) => {
            sard_core::warn!("cannot run osascript: {e}");
            None
        }
    }
}

/// Ask before doing something; `true` if the user chose `action`. Blocks
/// until answered (any thread).
pub fn confirm(title: &str, message: &str, action: &str) -> bool {
    match osascript(CONFIRM_SCRIPT, &[title, message, action]) {
        Ok(out) => out.status.success(),
        Err(e) => {
            sard_core::warn!("cannot run osascript: {e}");
            false
        }
    }
}

/// Modal notice. Blocks until dismissed (any thread); logged as well.
pub fn show_message(title: &str, message: &str) {
    sard_core::info!("{title}: {message}");
    match osascript(MESSAGE_SCRIPT, &[title, message]) {
        Ok(out) if !out.status.success() => {
            sard_core::debug!("message dialog: {}", String::from_utf8_lossy(&out.stderr).trim())
        }
        Ok(_) => {}
        Err(e) => sard_core::warn!("cannot run osascript: {e}"),
    }
}

/// `SMAppServiceStatus`.
const SM_NOT_REGISTERED: isize = 0;
const SM_ENABLED: isize = 1;
const SM_REQUIRES_APPROVAL: isize = 2;

fn main_app_service() -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"SMAppService")?;
    // SAFETY: class method (macOS 13+) returning a +0 object.
    unsafe { msg_send![cls, mainAppService] }
}

fn service_status(s: &AnyObject) -> isize {
    // SAFETY: SMAppService property.
    unsafe { msg_send![s, status] }
}

/// Whether this app bundle is registered (and allowed) to open at login.
pub fn launch_at_login() -> bool {
    autoreleasepool(|_| main_app_service().is_some_and(|s| service_status(&s) == SM_ENABLED))
}

/// Register / unregister the app bundle as a login item.
pub fn set_launch_at_login(enabled: bool) -> media::Result<()> {
    autoreleasepool(|_| {
        let s = main_app_service().ok_or_else(|| media::err("launch at login needs macOS 13 or newer"))?;
        let status = service_status(&s);
        if (enabled && status == SM_ENABLED) || (!enabled && status == SM_NOT_REGISTERED) {
            return Ok(());
        }
        let mut err: *mut AnyObject = std::ptr::null_mut();
        // SAFETY: SMAppService methods with an NSError out-parameter.
        let ok: bool = unsafe {
            if enabled {
                msg_send![&*s, registerAndReturnError: &mut err]
            } else {
                msg_send![&*s, unregisterAndReturnError: &mut err]
            }
        };
        if !ok {
            // SAFETY: nil or NSError.
            let e = unsafe { error_string(err) };
            return Err(media::err(format!(
                "cannot {} the login item: {e}",
                if enabled { "register" } else { "remove" }
            )));
        }
        if enabled && service_status(&s) == SM_REQUIRES_APPROVAL {
            // SAFETY: class method (macOS 13+).
            unsafe {
                let _: () = msg_send![class!(SMAppService), openSystemSettingsLoginItems];
            }
            return Err(media::err("allow Sard in System Settings → General → Login Items to open it at login"));
        }
        Ok(())
    })
}

/// `NSApplicationActivationPolicy`.
const POLICY_REGULAR: isize = 0;
const POLICY_ACCESSORY: isize = 1;

/// Show (regular app: Dock icon, app menu) or hide (menu bar accessory) the
/// Dock icon. Applied on the main thread (queued if called elsewhere).
pub fn set_dock_visible(visible: bool) {
    run_on_main(move || {
        autoreleasepool(|_| {
            // SAFETY: main thread; NSApplication methods.
            unsafe {
                let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
                let policy = if visible { POLICY_REGULAR } else { POLICY_ACCESSORY };
                let current: isize = msg_send![app, activationPolicy];
                if current == policy {
                    return;
                }
                let ok: bool = msg_send![app, setActivationPolicy: policy];
                if !ok {
                    sard_core::warn!("cannot change the activation policy to {policy}");
                }
                if visible {
                    // Bring the newly regular app (and its window) to the front.
                    let _: () = msg_send![app, activateIgnoringOtherApps: true];
                }
            }
        })
    });
}

/// Current `NSApplicationActivationPolicy` (main thread only; diagnostics).
#[doc(hidden)]
pub fn activation_policy() -> Option<isize> {
    objc2::MainThreadMarker::new()?;
    // SAFETY: main thread; NSApplication getter.
    Some(unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        msg_send![app, activationPolicy]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AppleScript sources compile (no dialog is shown).
    #[test]
    fn dialog_scripts_compile() {
        let dir = std::env::temp_dir().join(format!("sard-osacompile-{}", std::process::id()));
        for (i, script) in [PROMPT_SCRIPT, MESSAGE_SCRIPT].iter().enumerate() {
            let out = dir.with_extension(format!("{i}.scpt"));
            let mut cmd = Command::new("/usr/bin/osacompile");
            for line in *script {
                cmd.arg("-e").arg(line);
            }
            let r = cmd.arg("-o").arg(&out).output().expect("osacompile");
            let _ = std::fs::remove_file(&out);
            assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        }
    }

    /// Script arguments arrive verbatim (quotes, unicode, leading dashes).
    #[test]
    fn osascript_passes_arguments_verbatim() {
        let script = ["on run argv", "return (item 1 of argv) & \"|\" & (item 2 of argv)", "end run"];
        let out = osascript(&script, &["-n \"quoted\" ✓", "two\nlines"]).expect("osascript");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "-n \"quoted\" ✓|two\nlines\n");
    }

    #[test]
    fn launch_at_login_status_query() {
        // A test binary is not an app bundle: must report "off", not fail.
        assert!(!launch_at_login());
    }
}
