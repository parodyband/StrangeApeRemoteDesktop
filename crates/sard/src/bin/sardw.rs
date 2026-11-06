//! `sardw.exe`: the Windows launcher for the tray app. `sard.exe` is a console
//! program (so `sard info` etc. work in a terminal); started from Explorer,
//! the Start menu or the Run key it would flash a console window. This
//! GUI-subsystem stub starts the `sard.exe` next to it without a window
//! (default command: `app`) and exits.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("sard.exe"))) else {
            return;
        };
        let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if args.is_empty() {
            args.push("app".into());
        }
        let _ = std::process::Command::new(exe).args(args).creation_flags(CREATE_NO_WINDOW).spawn();
    }
    #[cfg(not(windows))]
    eprintln!("sardw is the Windows launcher; run sard instead");
}
