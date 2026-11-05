//! Desktop integration helpers: dialogs (kdialog / zenity), autostart.
//! (Work in progress.)

use sard_core::media;

pub fn prompt_text(_title: &str, _message: &str) -> Option<String> {
    None
}

pub fn show_message(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}

pub fn set_launch_at_login(_enabled: bool) -> media::Result<()> {
    Err(media::err("not implemented yet"))
}

pub fn launch_at_login() -> bool {
    false
}
