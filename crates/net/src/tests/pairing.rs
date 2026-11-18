//! N17 to N21.

use std::time::Duration;

use iroh::SecretKey;

use super::{bounded, start, TestNode};
use crate::{
    close,
    node::Role,
    pairing::*,
    NetError,
};

fn key() -> [u8; 32] {
    *SecretKey::generate().public().as_bytes()
}

fn payload() -> PairPayload {
    PairPayload {
        pc_key: key(),
        addrs: vec!["192.168.1.20:47300".parse().unwrap(), "[fd12:3456::1]:47300".parse().unwrap()],
        secret: PairSecret([7u8; 16]),
        expires_at_ms: 1_800_000_000_000,
        pc_name: "Office PC".into(),
    }
}

fn bad_reason(r: Result<PairPayload, PairError>) -> String {
    match r {
        Err(PairError::BadPayload { reason }) => reason,
        other => panic!("expected BadPayload, got {other:?}"),
    }
}

/// N17: roundtrip, each bad field rejected, unknown parameter ignored, name decoding.
#[test]
fn n17_payload_roundtrip_and_rejections() {
    let p = payload();
    let text = p.encode();
    assert!(text.len() <= MAX_PAYLOAD_LEN && text.is_ascii());
    assert!(text.starts_with("localtugboat://pair?v=1&k="));
    assert_eq!(PairPayload::parse(&text).unwrap(), p);

    let k = hex::encode(p.pc_key);
    let s = hex::encode([7u8; 16]);
    let good = |a: &str| format!("localtugboat://pair?v=1&k={k}&a={a}&s={s}&e=5&n=PC");
    assert!(PairPayload::parse(&good("192.168.1.2:1")).is_ok());

    // addresses
    assert!(bad_reason(PairPayload::parse(&good("8.8.8.8:47300"))).contains("not on the local network"));
    assert!(bad_reason(PairPayload::parse(&good("192.168.1.2:1,8.8.8.8:1"))).contains("not on the local network"));
    assert!(bad_reason(PairPayload::parse(&good("[2001:4860:4860::8888]:1"))).contains("not on the local network"));
    assert!(bad_reason(PairPayload::parse(&good("192.168.1.1:1,192.168.1.2:1,192.168.1.3:1,192.168.1.4:1,192.168.1.5:1"))).contains("1 to 4"));
    assert!(bad_reason(PairPayload::parse(&good(""))).contains("unparsable"));
    assert!(bad_reason(PairPayload::parse(&good("192.168.1.2:0"))).contains("port 0"));
    assert!(bad_reason(PairPayload::parse(&good("not-an-address"))).contains("unparsable"));
    // hex fields
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k=zz{}&a=10.0.0.1:1&s={s}&e=5", &k[2..]))).contains("k is not 64 hex"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={}&a=10.0.0.1:1&s={s}&e=5", &k[2..]))).contains("k is not 64 hex"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={}&e=5", &s[2..]))).contains("s is not 32 hex"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s=gg{}&e=5", &s[2..]))).contains("s is not 32 hex"));
    // version, expiry, prefix, size
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=2&k={k}&a=10.0.0.1:1&s={s}&e=5"))).contains("version"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=soon"))).contains("e is not"));
    assert!(bad_reason(PairPayload::parse(&format!("https://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=5"))).contains("not a tugboat"));
    let long = format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=5&n={}", "a".repeat(MAX_PAYLOAD_LEN));
    assert!(bad_reason(PairPayload::parse(&long)).contains("limit"));
    // missing fields
    for missing in ["v", "k", "a", "s", "e"] {
        let all = [("v", "1".to_string()), ("k", k.clone()), ("a", "10.0.0.1:1".to_string()), ("s", s.clone()), ("e", "5".to_string())];
        let q: Vec<String> = all.iter().filter(|(n, _)| *n != missing).map(|(n, v)| format!("{n}={v}")).collect();
        let r = bad_reason(PairPayload::parse(&format!("localtugboat://pair?{}", q.join("&"))));
        assert!(r.contains(&format!("missing {missing}")), "{missing}: {r}");
    }
    // repeated parameter, even with the same value
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=5&e=5"))).contains("repeated"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&v=1&k={k}&a=10.0.0.1:1&s={s}&e=5"))).contains("repeated"));
    // non-ASCII
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=5&n=\u{00e9}"))).contains("ASCII"));

    // order is free, unknown parameters are ignored, surrounding whitespace is trimmed
    let shuffled = format!("  localtugboat://pair?e=5&zz=1&s={s}&n=Desk&a=10.0.0.1:47300&k={k}&v=1\n");
    let q = PairPayload::parse(&shuffled).unwrap();
    assert_eq!((q.pc_name.as_str(), q.expires_at_ms, q.addrs.len()), ("Desk", 5, 1));
}

#[test]
fn n17_name_decoding_and_cleaning() {
    let mut p = payload();
    p.pc_name = "Caf\u{00e9} & \u{4e2d}\u{6587}/PC".into();
    let round = PairPayload::parse(&p.encode()).unwrap();
    assert_eq!(round.pc_name, p.pc_name);

    // escaped controls and bidi overrides in `n` are stripped after decoding
    let k = hex::encode(key());
    let s = hex::encode([1u8; 16]);
    let q = PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=1&n=%E2%80%AEevil%00%0A%20PC%20")).unwrap();
    assert_eq!(q.pc_name, "evil PC");
    // empty after cleaning falls back; a bad escape or invalid UTF-8 is rejected
    let q = PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=1&n=%E2%80%AE")).unwrap();
    assert_eq!(q.pc_name, "PC");
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=1&n=%G1"))).contains("escape"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=1&n=%"))).contains("escape"));
    assert!(bad_reason(PairPayload::parse(&format!("localtugboat://pair?v=1&k={k}&a=10.0.0.1:1&s={s}&e=1&n=%FF%FE"))).contains("UTF-8"));

    assert_eq!(clean_name("  a\u{202E}b\u{200F}c\u{0007}  ", "x"), "abc");
    assert_eq!(clean_name("\u{061C}\u{2066}\u{2069}", "fallback"), "fallback");
    assert_eq!(clean_name(&"x".repeat(200), "f").chars().count(), MAX_NAME_CHARS);
    assert_eq!(clean_name(&"\u{4e2d}".repeat(200), "f").chars().count(), MAX_NAME_CHARS);
}

/// A long non-ASCII PC name must not make the producer emit a code its own parser refuses.
#[test]
fn n17_long_non_ascii_names_still_encode_within_the_limit() {
    for name in ["\u{754c}".repeat(64), "\u{1F600}".repeat(64), "Office PC".to_string()] {
        let mut p = payload();
        p.addrs = vec![
            "192.168.1.20:47300".parse().unwrap(),
            "192.168.1.21:47300".parse().unwrap(),
            "192.168.1.22:47300".parse().unwrap(),
            "192.168.1.23:47300".parse().unwrap(),
        ];
        p.pc_name = name.clone();
        let text = p.encode();
        assert!(text.len() <= MAX_PAYLOAD_LEN, "{} bytes for {name:?}", text.len());
        let back = PairPayload::parse(&text).expect("the parser accepts what the encoder made");
        assert!(name.starts_with(&back.pc_name), "the name is only ever shortened");
        assert_eq!((back.pc_key, back.addrs, back.secret, back.expires_at_ms), (p.pc_key, p.addrs, p.secret, p.expires_at_ms));
        if name == "Office PC" {
            assert_eq!(back.pc_name, name, "a short name is unchanged");
        } else {
            assert!(!back.pc_name.is_empty());
        }
    }
}

/// A phone that stopped reading did not necessarily get the whole decision: not delivered.
#[test]
fn n20_a_stop_from_the_phone_is_not_delivery() {
    assert!(decision_delivery(None).is_ok());
    assert!(matches!(decision_delivery(Some(0)), Err(NetError::Stream(_))));
}

/// N18: known-answer vector, sensitivity to every input byte, zero padding.
#[test]
fn n18_pairing_code() {
    let (pc, phone, secret) = ([1u8; 32], [2u8; 32], PairSecret([3u8; 16]));
    let code = pairing_code(&pc, &phone, &secret);
    // Pinned: if this changes, a PC and a phone on different versions would show different codes.
    assert_eq!(code, KNOWN_CODE, "pairing code derivation changed");
    assert_eq!(code.len(), 6);
    assert_eq!(format_code(&code), format!("{} {}", &code[..3], &code[3..]));

    // flipping any single input byte changes the code
    for i in 0..80 {
        let (mut p, mut f, mut s) = (pc, phone, secret.0);
        match i {
            0..=31 => p[i] ^= 1,
            32..=63 => f[i - 32] ^= 1,
            _ => s[i - 64] ^= 1,
        }
        assert_ne!(pairing_code(&p, &f, &PairSecret(s)), code, "byte {i} does not affect the code");
    }
    // The same number by another route: streaming the three parts into a derive-key hasher
    // and reading the first 8 bytes by hand. Pins the concatenation order and the modulus.
    let mut h = blake3::Hasher::new_derive_key(CODE_CONTEXT);
    h.update(&pc).update(&phone).update(&secret.0);
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&h.finalize().as_bytes()[..8]);
    assert_eq!(format!("{:06}", u64::from_le_bytes(first8) % 1_000_000), code);
    // key order matters: swapping PC and phone key gives another code
    assert_ne!(pairing_code(&phone, &pc, &secret), code);

    // always six digits, even when the number is small
    let mut seen_leading_zero = false;
    for n in 0u16..2000 {
        let c = pairing_code(&[n as u8; 32], &[(n >> 8) as u8; 32], &PairSecret([n as u8; 16]));
        assert!(c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()), "{c}");
        seen_leading_zero |= c.starts_with('0');
    }
    assert!(seen_leading_zero, "no leading zero seen in 2000 codes: padding untested");
    assert_eq!(format_code("004213"), "004 213");
    assert_eq!(format_code("12"), "12");
}

const KNOWN_CODE: &str = "509395";

fn hello(secret: &PairSecret) -> PairHello {
    PairHello { proto: PAIR_PROTO, secret: hex::encode(secret.0), phone_name: "Pixel".into(), app_version: "1.0".into() }
}

const T0: i64 = 1_000_000;

fn win() -> (PairingWindow, [u8; 32]) {
    (PairingWindow::open(key(), T0).unwrap(), key())
}

/// N19: one test per transition row of the table in `pairing.rs`.
#[test]
fn n19_open_hello_after_expiry() {
    let (mut w, phone) = win();
    let h = hello(&w.secret);
    assert_eq!(w.on_hello(phone, &h, T0 + WINDOW_MS - 1).map(|_| ()), Ok(()));
    let (mut w, phone) = win();
    let h = hello(&w.secret);
    assert_eq!(w.on_hello(phone, &h, T0 + WINDOW_MS), Err(PairError::Expired));
    assert_eq!(w.cancelled(), Some(&PairError::Expired));
    assert!(!w.accepting());
    // after that the window is closed
    assert_eq!(w.on_hello(phone, &h, T0 + 1), Err(PairError::NotOpen));
}

#[test]
fn n19_open_hello_wrong_proto_stays_open() {
    let (mut w, phone) = win();
    let mut h = hello(&w.secret);
    // A 0.1.x phone (proto 1) and a phone from the future both get the mismatch, the window stays open.
    h.proto = 1;
    assert_eq!(w.on_hello(phone, &h, T0 + 1), Err(PairError::VersionMismatch { ours: PAIR_PROTO, theirs: 1 }));
    h.proto = PAIR_PROTO + 1;
    assert_eq!(w.on_hello(phone, &h, T0 + 1), Err(PairError::VersionMismatch { ours: PAIR_PROTO, theirs: PAIR_PROTO + 1 }));
    assert!(w.accepting() && w.cancelled().is_none());
    h.proto = PAIR_PROTO;
    assert!(w.on_hello(phone, &h, T0 + 2).is_ok());
}

#[test]
fn n19_open_hello_wrong_or_malformed_secret_stays_open() {
    let (mut w, phone) = win();
    for bad in [hex::encode([9u8; 16]), "zz".repeat(16), String::new(), "ab".repeat(17), hex::encode(w.secret.0).to_uppercase() + "00"] {
        let mut h = hello(&w.secret);
        h.secret = bad.clone();
        assert_eq!(w.on_hello(phone, &h, T0 + 1), Err(PairError::BadSecret), "{bad}");
        assert!(w.accepting());
    }
    // the correct secret still works afterwards, in upper case too
    let mut h = hello(&w.secret);
    h.secret = h.secret.to_uppercase();
    assert!(w.on_hello(phone, &h, T0 + 2).is_ok());
}

#[test]
fn n19_open_hello_right_secret_shows_code() {
    let (mut w, phone) = win();
    let mut h = hello(&w.secret);
    h.phone_name = "  Pi\u{202E}xel\u{0007}  ".into();
    match w.on_hello(phone, &h, T0 + 5).unwrap() {
        HelloOutcome::ShowCode(p) => {
            assert_eq!(p.phone_key, phone);
            assert_eq!(p.phone_name, "Pixel");
            assert_eq!(p.since_ms, T0 + 5);
            assert_eq!(p.code, pairing_code(&w.pc_key, &phone, &w.secret));
        }
        other => panic!("{other:?}"),
    }
    assert!(w.accepting());
}

#[test]
fn n19_pending_same_phone_replaces_other_phone_cancels() {
    let (mut w, phone) = win();
    let h = hello(&w.secret);
    let HelloOutcome::ShowCode(first) = w.on_hello(phone, &h, T0 + 1).unwrap() else { panic!() };
    // network retry from the same phone: same code, still pending
    match w.on_hello(phone, &h, T0 + 2).unwrap() {
        HelloOutcome::Replaced(p) => assert_eq!(p, first),
        other => panic!("{other:?}"),
    }
    assert!(w.accepting());
    // a wrong secret while pending changes nothing
    let mut wrong = hello(&w.secret);
    wrong.secret = hex::encode([5u8; 16]);
    assert_eq!(w.on_hello(key(), &wrong, T0 + 3), Err(PairError::BadSecret));
    assert!(w.accepting());
    // a second phone with the right secret: the window dies for everyone
    assert_eq!(w.on_hello(key(), &h, T0 + 4), Err(PairError::SecretReused));
    assert_eq!(w.cancelled(), Some(&PairError::SecretReused));
    assert!(!w.accepting());
    // ... including the first phone
    assert_eq!(w.confirm(T0 + 5), Err(PairError::NotOpen));
    assert_eq!(w.on_hello(phone, &h, T0 + 6), Err(PairError::NotOpen));
}

#[test]
fn n19_pending_does_not_expire_at_expires_at() {
    let (mut w, phone) = win();
    let h = hello(&w.secret);
    w.on_hello(phone, &h, T0 + WINDOW_MS - 10).unwrap();
    // the user is looking at the code: the QR expiry no longer applies
    assert_eq!(w.tick(T0 + WINDOW_MS + 5), None);
    assert!(w.confirm(T0 + WINDOW_MS + 5).is_ok());
}

#[test]
fn n19_pending_confirm_timeout_boundary() {
    let (mut w, phone) = win();
    let h = hello(&w.secret);
    w.on_hello(phone, &h, T0 + 1).unwrap();
    // exactly CONFIRM_TIMEOUT_MS after the code appeared is still in time
    let mut w2 = PairingWindow::open(w.pc_key, T0).unwrap();
    w2.secret = w.secret.clone();
    w2.on_hello(phone, &h, T0 + 1).unwrap();
    assert!(w2.confirm(T0 + 1 + CONFIRM_TIMEOUT_MS).is_ok());
    // one ms later it is not
    assert_eq!(w.confirm(T0 + 1 + CONFIRM_TIMEOUT_MS + 1), Err(PairError::ConfirmTimeout));
    assert_eq!(w.cancelled(), Some(&PairError::ConfirmTimeout));
    assert_eq!(w.confirm(T0 + 2), Err(PairError::NotOpen));
}

#[test]
fn n19_pending_confirm_completes_once() {
    let (mut w, phone) = win();
    let h = hello(&w.secret);
    w.on_hello(phone, &h, T0 + 1).unwrap();
    let p = w.confirm(T0 + 2).unwrap();
    assert_eq!(p.phone_key, phone);
    assert!(!w.accepting());
    // Done: the secret is single use, any hello is SecretReused, a second confirm is refused
    assert_eq!(w.on_hello(phone, &h, T0 + 3), Err(PairError::SecretReused));
    assert_eq!(w.on_hello(key(), &h, T0 + 3), Err(PairError::SecretReused));
    assert_eq!(w.confirm(T0 + 4), Err(PairError::NotOpen));
    // cancel after Done does not undo it
    w.cancel();
    assert_eq!(w.cancelled(), None);
    assert_eq!(w.on_hello(phone, &h, T0 + 5), Err(PairError::SecretReused));
}

#[test]
fn n19_cancel_from_every_live_state_and_confirm_without_hello() {
    let (mut w, phone) = win();
    assert_eq!(w.confirm(T0 + 1), Err(PairError::NotOpen), "no code shown yet, nothing to confirm");
    w.cancel();
    assert_eq!(w.cancelled(), Some(&PairError::Cancelled));
    assert!(!w.accepting());
    assert_eq!(w.on_hello(phone, &hello(&w.secret), T0 + 1), Err(PairError::NotOpen));

    let (mut w, phone) = win();
    w.on_hello(phone, &hello(&w.secret), T0 + 1).unwrap();
    w.cancel();
    assert_eq!(w.cancelled(), Some(&PairError::Cancelled));
    assert_eq!(w.confirm(T0 + 2), Err(PairError::NotOpen));
    // a second cancel keeps the first reason
    w.cancel();
    assert_eq!(w.cancelled(), Some(&PairError::Cancelled));
}

#[test]
fn n19_tick_reports_each_cancellation_exactly_once() {
    let (mut w, _) = win();
    assert_eq!(w.tick(T0 + WINDOW_MS - 1), None);
    assert_eq!(w.tick(T0 + WINDOW_MS), Some(PairError::Expired));
    assert_eq!(w.tick(T0 + WINDOW_MS + 1), None);

    let (mut w, phone) = win();
    w.on_hello(phone, &hello(&w.secret), T0 + 1).unwrap();
    assert_eq!(w.tick(T0 + 1 + CONFIRM_TIMEOUT_MS), None);
    assert_eq!(w.tick(T0 + 2 + CONFIRM_TIMEOUT_MS), Some(PairError::ConfirmTimeout));
    assert_eq!(w.tick(T0 + 3 + CONFIRM_TIMEOUT_MS), None);

    // a finished or cancelled window never reports again
    let (mut w, phone) = win();
    w.on_hello(phone, &hello(&w.secret), T0 + 1).unwrap();
    w.confirm(T0 + 2).unwrap();
    assert_eq!(w.tick(T0 + 10 * WINDOW_MS), None);
}

#[test]
fn n19_payload_carries_window_state() {
    let (w, _) = win();
    let p = w.payload(vec!["192.168.1.2:1".parse().unwrap()], "  My\u{202E} PC ");
    assert_eq!((p.pc_key, p.expires_at_ms, p.pc_name.as_str()), (w.pc_key, T0 + WINDOW_MS, "My PC"));
    assert!(p.secret.ct_eq(&w.secret.0));
    assert_eq!(PairPayload::parse(&p.encode()).unwrap(), p);
}

/// N21.
#[test]
fn n21_secret_is_random_and_hidden() {
    let a = PairSecret::generate().unwrap();
    let b = PairSecret::generate().unwrap();
    assert_ne!(a, b);
    assert_ne!(a.0, [0u8; 16]);
    assert_eq!(format!("{a:?}"), "PairSecret(..)");
    assert!(!format!("{:?}", payload()).contains("0707070707"));
    assert!(a.ct_eq(&a.0));
    for i in 0..16 {
        let mut o = a.0;
        o[i] ^= 0x80;
        assert!(!a.ct_eq(&o), "byte {i}");
    }
}

/// What the app-side glue does with the library: the helper below drives both ends.
struct Pc {
    n: TestNode,
    w: PairingWindow,
    payload: PairPayload,
}

async fn pc(name: &str) -> Pc {
    let n = start(Role::Pc).await;
    n.node.set_pairing_open(true);
    let w = PairingWindow::open(n.node.id(), T0).unwrap();
    let payload = w.payload(n.node.qr_addrs(), name);
    // through the text form, like a real scan
    let payload = PairPayload::parse(&payload.encode()).unwrap();
    Pc { n, w, payload }
}

/// N20: both sides show the same code; accept, then refuse, both reach the phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n20_loopback_pairing_accept_and_refuse() {
    for accept in [true, false] {
        let mut pc = pc("Office PC").await;
        assert_eq!(pc.payload.addrs, pc.n.node.bound_addrs());
        let phone = start(Role::Phone).await;

        let started = bounded(20, phone_start(&phone.node, &pc.payload, "Pixel 9", "1.2.3", Duration::from_secs(10))).await.unwrap();
        let acc = bounded(20, pc.n.node.accept()).await.unwrap().unwrap();
        assert_eq!(acc.alpn, crate::ALPN_PAIR);
        assert_eq!(acc.peer.peer(), phone.node.id());
        let (mut tx, h) = pc_read_hello(&acc.peer, Duration::from_secs(10)).await.unwrap();
        assert_eq!((h.phone_name.as_str(), h.app_version.as_str(), h.proto), ("Pixel 9", "1.2.3", PAIR_PROTO));
        let HelloOutcome::ShowCode(p) = pc.w.on_hello(acc.peer.peer(), &h, T0 + 10).unwrap() else { panic!() };
        assert_eq!(p.code, started.code, "PC and phone must show the same code");
        assert_eq!(p.phone_name, "Pixel 9");

        let decision = if accept {
            let done = pc.w.confirm(T0 + 20).unwrap();
            assert_eq!(done.phone_key, phone.node.id());
            PairDecision::Accepted { pc_name: "Office PC".into() }
        } else {
            pc.w.cancel();
            PairDecision::Refused { reason: PairError::Cancelled }
        };
        let phone_side = tokio::spawn(started.wait_decision(Duration::from_secs(20)));
        pc_send_decision(&mut tx, &decision).await.unwrap();
        acc.peer.close(close::NORMAL, "done");
        let got = bounded(20, phone_side).await.unwrap();
        if accept {
            assert_eq!(got.unwrap(), "Office PC");
        } else {
            assert!(matches!(got, Err(NetError::Pairing(PairError::Cancelled))), "{got:?}");
        }
    }
}

/// N20: a second phone while a code is pending. The PC window dies and both phones are told.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n20_second_phone_while_pending_cancels_both() {
    let mut pc = pc("Office PC").await;
    let (p1, p2) = (start(Role::Phone).await, start(Role::Phone).await);

    let s1 = bounded(20, phone_start(&p1.node, &pc.payload, "Phone one", "1", Duration::from_secs(10))).await.unwrap();
    let acc1 = bounded(20, pc.n.node.accept()).await.unwrap().unwrap();
    let (mut tx1, h1) = pc_read_hello(&acc1.peer, Duration::from_secs(10)).await.unwrap();
    pc.w.on_hello(acc1.peer.peer(), &h1, T0 + 1).unwrap();

    let s2 = bounded(20, phone_start(&p2.node, &pc.payload, "Phone two", "1", Duration::from_secs(10))).await.unwrap();
    let acc2 = bounded(20, pc.n.node.accept()).await.unwrap().unwrap();
    let (mut tx2, h2) = pc_read_hello(&acc2.peer, Duration::from_secs(10)).await.unwrap();
    let err = pc.w.on_hello(acc2.peer.peer(), &h2, T0 + 2).unwrap_err();
    assert_eq!(err, PairError::SecretReused);
    assert!(!pc.w.accepting());

    let d = PairDecision::Refused { reason: PairError::SecretReused };
    let (w1, w2) = (tokio::spawn(s1.wait_decision(Duration::from_secs(20))), tokio::spawn(s2.wait_decision(Duration::from_secs(20))));
    pc_send_decision(&mut tx1, &d).await.unwrap();
    acc1.peer.close(close::PAIRING_CLOSED, "cancelled");
    pc_send_decision(&mut tx2, &d).await.unwrap();
    acc2.peer.close(close::PAIRING_CLOSED, "cancelled");
    for w in [w1, w2] {
        let r = bounded(20, w).await.unwrap();
        assert!(matches!(r, Err(NetError::Pairing(PairError::SecretReused))), "{r:?}");
    }
    assert_eq!(pc.w.confirm(T0 + 3), Err(PairError::NotOpen));
}

/// The phone is told plainly when the PC has no pairing window open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_start_says_when_pairing_is_not_open() {
    let pc = pc("PC").await;
    pc.n.node.set_pairing_open(false);
    let phone = start(Role::Phone).await;
    let err = bounded(20, phone_start(&phone.node, &pc.payload, "P", "1", Duration::from_secs(5))).await.unwrap_err();
    assert!(matches!(err, NetError::Pairing(PairError::NotOpen)), "{err}");
}

/// A phone that never gets a decision times out with a clear error, and a PC that closes
/// the connection reports the close code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_wait_decision_times_out_or_sees_the_close() {
    let pc = pc("PC").await;
    let phone = start(Role::Phone).await;
    let started = bounded(20, phone_start(&phone.node, &pc.payload, "P", "1", Duration::from_secs(10))).await.unwrap();
    let _acc = bounded(20, pc.n.node.accept()).await.unwrap().unwrap();
    let err = started.wait_decision(Duration::from_millis(300)).await.unwrap_err();
    assert!(matches!(err, NetError::Timeout(_)), "{err}");

    let phone = start(Role::Phone).await;
    let started = bounded(20, phone_start(&phone.node, &pc.payload, "P", "1", Duration::from_secs(10))).await.unwrap();
    let acc = bounded(20, pc.n.node.accept()).await.unwrap().unwrap();
    acc.peer.close(close::PAIRING_CLOSED, "window closed");
    let err = bounded(20, started.wait_decision(Duration::from_secs(10))).await.unwrap_err();
    assert!(matches!(err, NetError::ClosedByPeer { code, .. } if code == close::PAIRING_CLOSED), "{err}");
}

/// The hello is read before the phone is trusted: a stream of another kind or no hello is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pc_read_hello_refuses_wrong_stream_and_times_out() {
    let pc = pc("PC").await;
    let phone = start(Role::Phone).await;
    let conn = bounded(20, phone.node.dial(pc.n.node.id(), &pc.n.node.bound_addrs(), crate::ALPN_PAIR, Duration::from_secs(10))).await.unwrap();
    let acc = bounded(20, pc.n.node.accept()).await.unwrap().unwrap();

    // nothing sent: bounded wait, clear error
    let err = pc_read_hello(&acc.peer, Duration::from_millis(300)).await.unwrap_err();
    assert!(matches!(err, NetError::Timeout(_)), "{err}");

    // a Control stream on the pairing connection: refused, connection closed with PROTOCOL
    let _s = conn.open_stream(crate::node::StreamKind::Control).await.unwrap();
    let err = pc_read_hello(&acc.peer, Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(_)), "{err}");
    let seen = bounded(10, conn.closed()).await;
    assert!(matches!(seen, NetError::ClosedByPeer { code, .. } if code == close::PROTOCOL), "{seen}");
}
