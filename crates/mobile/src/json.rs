//! Just enough JSON writing for the events the app receives (no serde in
//! this project).

use sard_core::config::clean_display;
use sard_core::proto::{Control, Platform};

pub struct Json(Vec<String>);

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

impl Json {
    pub fn object() -> Json {
        Json(Vec::new())
    }
    pub fn str(mut self, k: &str, v: &str) -> Json {
        self.0.push(format!("{}:{}", escape(k), escape(v)));
        self
    }
    pub fn num(mut self, k: &str, v: f64) -> Json {
        let v = if v.is_finite() { v } else { 0.0 };
        self.0.push(format!("{}:{v}", escape(k)));
        self
    }
    pub fn bool(mut self, k: &str, v: bool) -> Json {
        self.0.push(format!("{}:{v}", escape(k)));
        self
    }
    pub fn raw(mut self, k: &str, json: &str) -> Json {
        self.0.push(format!("{}:{json}", escape(k)));
        self
    }
    pub fn build(self) -> String {
        format!("{{{}}}", self.0.join(","))
    }
    pub fn error(e: &str) -> String {
        Json::object().bool("ok", false).str("error", e).build()
    }
}

fn platform(p: Platform) -> &'static str {
    match p {
        Platform::Linux => "linux",
        Platform::MacOs => "macos",
        Platform::Windows => "windows",
        Platform::Ios => "ios",
        Platform::Unknown => "unknown",
    }
}

/// The event for a control message the app cares about.
pub fn control(c: &Control) -> Option<String> {
    Some(match c {
        Control::StreamConfig(s) => Json::object()
            .str("type", "stream")
            .num("width", s.width as f64)
            .num("height", s.height as f64)
            .num("fps", s.fps as f64)
            .str("codec", s.codec.name())
            .str("host_platform", platform(s.host_platform))
            .str("host_name", &clean_display(&s.host_name))
            // Cursor sent separately (the app draws it), else it's in the video.
            .bool("cursor_metadata", s.flags & 1 != 0)
            .build(),
        Control::Status { text } => Json::object().str("type", "status").str("text", &clean_display(text)).build(),
        Control::Displays { current, list } => {
            let items: Vec<String> = list
                .iter()
                .map(|d| {
                    Json::object()
                        .num("id", d.id as f64)
                        .str("name", &clean_display(&d.name))
                        .num("width", d.width as f64)
                        .num("height", d.height as f64)
                        .build()
                })
                .collect();
            Json::object()
                .str("type", "displays")
                .num("current", *current as f64)
                .raw("list", &format!("[{}]", items.join(",")))
                .build()
        }
        Control::Clipboard { text } => Json::object().str("type", "clipboard").str("text", text).build(),
        Control::Shells { list } => {
            let items: Vec<String> = list
                .iter()
                .map(|s| {
                    Json::object()
                        .num("id", s.id as f64)
                        .str("name", &s.name)
                        .bool("attached", s.attached)
                        .num("age_s", s.age_s as f64)
                        .build()
                })
                .collect();
            Json::object().str("type", "shells").raw("list", &format!("[{}]", items.join(","))).build()
        }
        Control::ShellOpened { token, id, name, resumed } => Json::object()
            .str("type", "shell_opened")
            .num("token", *token as f64)
            .num("id", *id as f64)
            .str("name", name)
            .bool("resumed", *resumed)
            .build(),
        Control::ShellRefused { token, reason } => {
            Json::object().str("type", "shell_refused").num("token", *token as f64).str("reason", reason).build()
        }
        Control::ShellExit { id, message } => {
            Json::object().str("type", "shell_exit").num("id", *id as f64).str("message", message).build()
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(escape("a\"b\\c\n\u{1}é"), "\"a\\\"b\\\\c\\n\\u0001é\"");
        let j = Json::object().str("type", "x").num("n", 1.5).bool("b", true).raw("l", "[]").build();
        assert_eq!(j, r#"{"type":"x","n":1.5,"b":true,"l":[]}"#);
        assert_eq!(Json::object().num("n", f64::NAN).build(), r#"{"n":0}"#);
    }
}
