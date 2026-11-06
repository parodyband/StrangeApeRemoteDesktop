//! End-to-end transport test over localhost UDP: pair, connect, exchange
//! control messages, stream video frames and check integrity/order.

use sard_core::crypto::{Keypair, generate_pairing_code, normalize_pairing_code};
use sard_core::net::{Client, Event, Host, HostOptions, NetConfig};
use sard_core::proto::{Codec, Control};
use sard_core::video::{FrameMeta, frame_flags};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn pair_connect_stream() {
    sard_core::log::init();
    let host_id = Keypair::generate();
    let client_id = Keypair::generate();
    let code = generate_pairing_code();
    let paired: Arc<Mutex<Vec<String>>> = Arc::default();
    let paired2 = Arc::clone(&paired);
    let host = Host::bind(
        "127.0.0.1:0".parse().unwrap(),
        host_id.clone(),
        NetConfig { name: "test-host".into(), ..Default::default() },
        HostOptions {
            trusted: HashMap::new(),
            pairing_code: normalize_pairing_code(&code),
            on_paired: Some(Box::new(move |_k, name| paired2.lock().unwrap().push(name.to_string()))),
        },
    )
    .unwrap();
    let addr = host.local_addr().unwrap().to_string();
    let ccfg = NetConfig { name: "test-client".into(), ..Default::default() };

    // Wrong code must fail (and count as a guess, but a few are tolerated).
    let wrong = if code.starts_with('0') { "111111" } else { "000000" };
    let bad = Client::pair(&addr, wrong, &client_id, &ccfg, Duration::from_millis(800));
    assert!(bad.is_err());

    let (hk, hname, _) = Client::pair(&addr, &code, &client_id, &ccfg, Duration::from_secs(3)).unwrap();
    assert_eq!(hk, host_id.public());
    assert_eq!(hname, "test-host");
    assert_eq!(paired.lock().unwrap().as_slice(), ["test-client"]);

    // The code is single-use: another device can't pair with it afterwards.
    let other = Keypair::generate();
    assert!(Client::pair(&addr, &code, &other, &ccfg, Duration::from_millis(800)).is_err());
    // ...but a retransmit of the original pairing request still gets its answer
    // (exercised implicitly by Client::pair's resend loop above).

    // Guessing is capped: after MAX_PAIRING_FAILURES wrong codes the window closes
    // even for the right code.
    let code2 = generate_pairing_code();
    host.set_pairing_code(normalize_pairing_code(&code2).as_deref());
    let wrong2 = if code2.starts_with('0') { "111111" } else { "000000" };
    for _ in 0..sard_core::net::MAX_PAIRING_FAILURES {
        assert!(Client::pair(&addr, wrong2, &other, &ccfg, Duration::from_millis(800)).is_err());
    }
    assert!(Client::pair(&addr, &code2, &other, &ccfg, Duration::from_millis(800)).is_err());

    // Unpaired client must not get in.
    let stranger = Keypair::generate();
    assert!(Client::connect(&addr, hk, stranger, ccfg.clone(), Duration::from_millis(700)).is_err());

    let client = Client::connect(&addr, hk, client_id.clone(), ccfg.clone(), Duration::from_secs(3)).unwrap();
    let server = host.accept(Duration::from_secs(3)).expect("host accept");
    assert_eq!(server.peer_name(), "test-client");
    assert_eq!(client.peer_name(), "test-host");

    let sev = server.take_events().unwrap();
    let cvid = client.take_video().unwrap();

    // Control: client -> host, in order.
    for code in 1..=50u16 {
        client.send_control(&Control::Key { code, down: true });
    }
    let mut got = Vec::new();
    while got.len() < 50 {
        if let Event::Control(Control::Key { code, .. }) =
            sev.recv_timeout(Duration::from_secs(2)).expect("control event")
        {
            got.push(code);
        }
    }
    assert_eq!(got, (1..=50).collect::<Vec<_>>());

    // Video: host -> client, consumed concurrently like a real decoder.
    let n = 120u32;
    let frames: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let key = i % 60 == 0;
            let mut f = Vec::new();
            FrameMeta {
                codec: Codec::Hevc,
                flags: if key { frame_flags::KEYFRAME } else { 0 },
                width: 2560,
                height: 1440,
                capture_us: i as u64,
                send_us: 0,
            }
            .write(&mut f);
            let len = if key { 250_000 } else { 40_000 + (i as usize * 131) % 20_000 };
            f.extend((0..len).map(|j| (j as u32).wrapping_mul(i + 7) as u8));
            f
        })
        .collect();
    let expected = frames.clone();
    let reader = std::thread::spawn(move || {
        for i in 0..n {
            let cf = cvid.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| panic!("frame {i} not received"));
            assert_eq!(cf.frame_id, i);
            assert_eq!(cf.data, expected[i as usize], "frame {i} corrupt");
        }
    });
    for (i, f) in frames.iter().enumerate() {
        let key = i % 60 == 0;
        server.send_video_frame(f, if key { frame_flags::KEYFRAME } else { 0 });
        std::thread::sleep(Duration::from_millis(2));
    }
    reader.join().expect("all frames received intact and in order");
    std::thread::sleep(Duration::from_millis(300));
    let st = client.stats();
    assert!(st.rtt_us < 50_000, "rtt {}", st.rtt_us);
    assert!(st.clock_offset_us.unwrap().abs() < 5_000, "same machine offset {:?}", st.clock_offset_us);

    client.close();
    match sev.recv_timeout(Duration::from_secs(2)) {
        Ok(Event::Disconnected(_)) => {}
        other => {
            // Drain until disconnect.
            let mut ok = matches!(other, Ok(Event::Disconnected(_)));
            while !ok {
                ok = matches!(sev.recv_timeout(Duration::from_secs(2)).expect("disconnect"), Event::Disconnected(_));
            }
        }
    }
}
