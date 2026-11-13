//! Write QR PNGs for decoder checks: `cargo run -p sard-core --example qrgen -- <dir>`.
fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("dir"));
    for n in [1usize, 10, 14, 20, 40, 80, 120, 160, 200, 300, 600] {
        let text: String = (0..n).map(|i| (b'a' + (i * 7 % 26) as u8) as char).collect();
        let q = sard_core::qr::QrCode::encode(text.as_bytes()).unwrap();
        std::fs::write(dir.join(format!("{n}.png")), q.to_png(6)).unwrap();
        std::fs::write(dir.join(format!("{n}.txt")), &text).unwrap();
    }
}
