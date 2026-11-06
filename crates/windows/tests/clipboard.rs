//! Clipboard round trip through two independent `WinClipboard`s (so the second
//! really reads what the first wrote). Restores the previous text afterwards.

#![cfg(target_os = "windows")]

use sard_core::media::Clipboard;
use sard_windows::clipboard::WinClipboard;

#[test]
fn round_trip() {
    let (Some(mut a), Some(mut b)) = (WinClipboard::open(), WinClipboard::open()) else {
        eprintln!("skipping: no clipboard window");
        return;
    };
    let before = a.get();
    let text = format!("sard clipboard test {}\nsecond line ✓", std::process::id());
    a.set(&text);
    match b.get() {
        Some(t) => assert_eq!(t, text),
        None => eprintln!("skipping: clipboard not readable in this session"),
    }
    if let Some(t) = before {
        a.set(&t);
    }
}
