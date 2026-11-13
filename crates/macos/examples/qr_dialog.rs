//! Show the pairing dialog with a QR code, as the menu bar app does:
//! `cargo run -p sard-macos --example qr_dialog`.

#[cfg(target_os = "macos")]
fn main() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let link = "sard://pair?a=192.168.1.20&c=123456&k=abababababababababababababababababababababababababababababababab&n=Example";
    let qr = sard_core::qr::QrCode::encode(link.as_bytes()).unwrap();
    std::thread::spawn(move || {
        let r = sard_macos::platform::choose_with_qr(
            "Pair a New Computer",
            "From an iPhone: scan this code with the Sard app or the Camera.\n\nAddress:  192.168.1.20\nCode:  123 456",
            &qr,
            &["Enter Code…", "Done"],
        );
        println!("chose {r:?}");
        std::process::exit(0);
    });
    // SAFETY: main thread; run the app so the main queue is serviced.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: 1isize]; // accessory, like the menu bar app
        let _: () = msg_send![app, run];
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
