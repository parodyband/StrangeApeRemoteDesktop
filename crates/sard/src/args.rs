//! Tiny argument parser: positionals, `--flag`, `--key value`, `--key=value`.

use std::collections::HashMap;

/// Flags that never take a value.
const BOOL_FLAGS: &[&str] = &[
    "pair",
    "yuv444",
    "fullscreen",
    "no-swap-modifiers",
    "no-audio",
    "verbose",
    "input-test",
    "view-only",
    "reselect",
    "headless",
    "audio",
    "no-auto-relative",
    "check",
    "decode",
    "terminal",
    "list",
];
/// Flags whose value is optional (`--test-pattern` or `--test-pattern 1920x1080`).
const OPTIONAL_VALUE: &[&str] = &["test-pattern"];

#[derive(Clone)]
pub struct Args {
    positional: Vec<String>,
    flags: HashMap<String, Option<String>>,
}

impl Args {
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        let mut positional = Vec::new();
        let mut flags = HashMap::new();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if let Some(name) = a.strip_prefix("--") {
                if let Some((k, v)) = name.split_once('=') {
                    flags.insert(k.to_string(), Some(v.to_string()));
                } else if BOOL_FLAGS.contains(&name) {
                    flags.insert(name.to_string(), None);
                } else if OPTIONAL_VALUE.contains(&name) {
                    let next = argv.get(i + 1).filter(|n| !n.starts_with("--") && n.contains('x'));
                    if let Some(v) = next {
                        flags.insert(name.to_string(), Some(v.clone()));
                        i += 1;
                    } else {
                        flags.insert(name.to_string(), None);
                    }
                } else {
                    let v = argv.get(i + 1).ok_or_else(|| format!("--{name} needs a value"))?;
                    flags.insert(name.to_string(), Some(v.clone()));
                    i += 1;
                }
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Ok(Self { positional, flags })
    }

    pub fn positional(&self, i: usize) -> Option<&str> {
        self.positional.get(i).map(String::as_str)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags.get(name).and_then(|v| v.as_deref())
    }

    pub fn parse_value<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.value(name) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| format!("invalid value for --{name}: {v}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Args {
        Args::parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn parses() {
        let a = p("1.2.3.4 --fps 120 --pair --codec=av1 --test-pattern --bitrate 50");
        assert_eq!(a.positional(0), Some("1.2.3.4"));
        assert_eq!(a.parse_value::<u32>("fps").unwrap(), Some(120));
        assert!(a.flag("pair"));
        assert_eq!(a.value("codec"), Some("av1"));
        assert!(a.flag("test-pattern") && a.value("test-pattern").is_none());
        assert_eq!(a.value("bitrate"), Some("50"));
        let b = p("--test-pattern 1280x720");
        assert_eq!(b.value("test-pattern"), Some("1280x720"));
    }
}
