//! Desktop integration helpers: dialogs (kdialog on KDE, zenity elsewhere)
//! and the XDG autostart entry for "Start at login".

use sard_core::media;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn have(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

fn kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("KDE"))
}

/// Pick the dialog tool: kdialog on Plasma (native look), else zenity.
fn dialog_tool() -> Option<&'static str> {
    let order: [&'static str; 2] = if kde() { ["kdialog", "zenity"] } else { ["zenity", "kdialog"] };
    order.into_iter().find(|t| have(t))
}

/// Ask for one line of text. Blocks until the dialog closes; None if cancelled
/// or no dialog tool is installed. Call from a background thread.
pub fn prompt_text(title: &str, message: &str) -> Option<String> {
    let out = match dialog_tool()? {
        "kdialog" => Command::new("kdialog").args(["--title", title, "--inputbox", message, ""]).output(),
        _ => Command::new("zenity").args(["--entry", "--title", title, "--text", message]).output(),
    }
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Ask before doing something; `true` if the user chose `action`. Blocks
/// until answered: call from a background thread.
pub fn confirm(title: &str, message: &str, action: &str) -> bool {
    let status = match dialog_tool() {
        Some("kdialog") => Command::new("kdialog")
            .args(["--title", title, "--warningcontinuecancel", message, "--continue-label", action])
            .status(),
        Some(_) => Command::new("zenity")
            .args(["--question", "--no-wrap", "--title", title, "--text", message, "--ok-label", action])
            .args(["--cancel-label", "Cancel"])
            .status(),
        None => {
            eprintln!("{title}: {message} (install kdialog or zenity to confirm)");
            return false;
        }
    };
    status.is_ok_and(|s| s.success())
}

/// Show an informational message without blocking the caller.
pub fn show_message(title: &str, message: &str) {
    let mut cmd = match dialog_tool() {
        Some("kdialog") => {
            let mut c = Command::new("kdialog");
            c.args(["--title", title, "--msgbox", message]);
            c
        }
        Some(_) => {
            let mut c = Command::new("zenity");
            c.args(["--info", "--no-wrap", "--title", title, "--text", message]);
            c
        }
        None if have("notify-send") => {
            let mut c = Command::new("notify-send");
            c.args(["--app-name=Sard", title, message]);
            c
        }
        None => {
            eprintln!("{title}: {message}");
            return;
        }
    };
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        // Reap it when it closes.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(_) => eprintln!("{title}: {message}"),
    }
}

fn autostart_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("sard.desktop"))
}

/// Quote a path for a .desktop Exec line.
fn exec_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn set_launch_at_login(enabled: bool) -> media::Result<()> {
    let path = autostart_file().ok_or_else(|| media::err("no HOME"))?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(media::err(e)),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().map_err(media::err)?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Sard\nComment=Remote desktop\nExec={} app\nIcon=sard\n\
         Terminal=false\nX-GNOME-Autostart-enabled=true\nX-KDE-autostart-after=panel\n",
        exec_quote(&exe.to_string_lossy())
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(media::err)?;
    }
    std::fs::write(&path, entry).map_err(media::err)
}

pub fn launch_at_login() -> bool {
    autostart_file().is_some_and(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(exec_quote("/home/a b/sard"), "\"/home/a b/sard\"");
        assert_eq!(exec_quote("/x/$y\"z"), "\"/x/\\$y\\\"z\"");
    }
}
