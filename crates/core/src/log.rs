//! Minimal leveled logging to stderr. Level comes from `SARD_LOG`
//! (`error`, `warn`, `info`, `debug`, `trace`), default `info`.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = 4;
pub const TRACE: u8 = 5;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);
static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);
    if let Ok(v) = std::env::var("SARD_LOG") {
        set_level(match v.to_ascii_lowercase().as_str() {
            "error" => ERROR,
            "warn" => WARN,
            "debug" => DEBUG,
            "trace" => TRACE,
            _ => INFO,
        });
    }
}

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

#[inline]
pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

#[doc(hidden)]
pub fn write(level: u8, module: &str, args: Arguments) {
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let tag = match level {
        ERROR => "ERROR",
        WARN => "WARN ",
        INFO => "INFO ",
        DEBUG => "DEBUG",
        _ => "TRACE",
    };
    let module = module.strip_prefix("sard_").unwrap_or(module);
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{t:>9.3} {tag} {module}: {args}");
}

#[macro_export]
macro_rules! error { ($($t:tt)*) => { if $crate::log::enabled($crate::log::ERROR) { $crate::log::write($crate::log::ERROR, module_path!(), format_args!($($t)*)) } } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { if $crate::log::enabled($crate::log::WARN) { $crate::log::write($crate::log::WARN, module_path!(), format_args!($($t)*)) } } }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { if $crate::log::enabled($crate::log::INFO) { $crate::log::write($crate::log::INFO, module_path!(), format_args!($($t)*)) } } }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { if $crate::log::enabled($crate::log::DEBUG) { $crate::log::write($crate::log::DEBUG, module_path!(), format_args!($($t)*)) } } }
#[macro_export]
macro_rules! trace { ($($t:tt)*) => { if $crate::log::enabled($crate::log::TRACE) { $crate::log::write($crate::log::TRACE, module_path!(), format_args!($($t)*)) } } }
