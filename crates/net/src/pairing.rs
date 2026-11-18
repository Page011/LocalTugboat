//! Pairing: QR payload, 6-digit code, the PC's pairing window, and the exchange.
//!
//! PLAN Security: "the QR holds the PC's public key, LAN addresses, port, a one-time 128-bit
//! secret and a 2-minute expiry, and is shown only on request. Both screens then show a
//! 6-digit code derived from both public keys and the secret, and the PC owner confirms it
//! matches. A second use of the secret cancels pairing with a warning."
//!
//! Exchange (ALPN_PAIR connection dialed by the phone to the QR addresses and key; iroh's
//! TLS proves the PC holds the QR key, so the secret travels only to the real PC):
//!   phone -> PC  frame PairHello { proto, secret, phone_name, app_version }
//!   (both sides compute pairing_code(pc_key, phone_key, secret) and show it)
//!   PC user clicks [Pair] or [Cancel]
//!   PC -> phone  frame PairDecision, then FIN; the PC closes the window either way.
//! The phone shows the code and waits; it has its own [Cancel] (closes the connection).
//!
//! Clock: the PC's clock alone decides expiry (the phone's clock may be skewed); the phone
//! shows the expiry only as a hint. All times are unix milliseconds passed in by the caller
//! (core's injected clock), so the window state machine is pure and tested without sleeping.

use std::{collections::HashSet, net::SocketAddr, time::Duration};

use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

use crate::{
    close,
    error::{map_conn_err, stream_err},
    frame::{read_frame_limited, write_frame},
    local::is_local_addr,
    node::{Node, PeerConn, StreamKind},
    NetError, ALPN_PAIR,
};

/// QR window length (PLAN: "shows a QR code for 2 minutes").
pub const WINDOW_MS: i64 = 120_000;
/// How long a shown code waits for the PC user's click before the window cancels itself.
pub const CONFIRM_TIMEOUT_MS: i64 = 120_000;
/// Most addresses carried in a QR code (keeps the code scannable).
pub const MAX_QR_ADDRS: usize = 4;
/// Longest device name accepted in a payload or hello, in chars, after cleaning.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest accepted QR payload string, in bytes.
pub const MAX_PAYLOAD_LEN: usize = 512;
/// Pairing exchange protocol version (the `proto` field of PairHello).
pub const PAIR_PROTO: u32 = 1;
/// BLAKE3 derive_key context for the code. Never change it: both sides must agree.
pub const CODE_CONTEXT: &str = "localtugboat 2026-11-17 pairing code v1";

/// A hello or decision is a few hundred bytes. The pairing stream is read before the peer is
/// trusted, so it gets a far smaller cap than a sync frame.
const MAX_PAIR_FRAME: u32 = 4096;
/// Waiting for the decision to reach the phone before the PC closes the connection.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

const PAYLOAD_PREFIX: &str = "localtugboat://pair?";

/// Why pairing did not complete. Serialized into PairDecision::Refused so the phone shows
/// the same reason the PC shows. Each has its own user text (notes/core-design.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum PairError {
    /// QR older than WINDOW_MS by the PC clock.
    #[error("pairing code expired")]
    Expired,
    /// Hello carried a different secret (not this QR). Window stays open.
    #[error("wrong pairing secret")]
    BadSecret,
    /// A second device used the secret while a pairing was pending or done. Cancels the
    /// window for both: "PAIRING BLOCKED - someone else used this code".
    #[error("someone else tried to use this pairing code")]
    SecretReused,
    /// PC user clicked Cancel (or the phone user cancelled).
    #[error("pairing cancelled")]
    Cancelled,
    /// The PC user did not click within CONFIRM_TIMEOUT_MS.
    #[error("pairing not confirmed in time")]
    ConfirmTimeout,
    /// No window open (closed, finished, or never opened).
    #[error("pairing is not open on the PC")]
    NotOpen,
    /// Hello `proto` differs from PAIR_PROTO.
    #[error("pairing protocol {theirs} not supported (this device speaks {ours})")]
    VersionMismatch { ours: u32, theirs: u32 },
    /// QR text or hello field failed validation; the text says which field.
    #[error("bad pairing data: {reason}")]
    BadPayload { reason: String },
}

fn bad(reason: impl Into<String>) -> PairError {
    PairError::BadPayload { reason: reason.into() }
}

/// One-time 128-bit secret.
#[derive(Clone, PartialEq, Eq)]
pub struct PairSecret(pub [u8; 16]);

impl std::fmt::Debug for PairSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret into logs.
        f.write_str("PairSecret(..)")
    }
}

impl PairSecret {
    /// 16 bytes from the OS RNG (`getrandom::fill`). RNG failure is an error, never a
    /// weak fallback.
    pub fn generate() -> Result<PairSecret, NetError> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| NetError::Endpoint(format!("OS random number generator failed: {e}")))?;
        Ok(PairSecret(b))
    }

    /// Constant-time comparison (XOR-fold of all 16 bytes), so timing does not leak a prefix.
    pub fn ct_eq(&self, other: &[u8; 16]) -> bool {
        self.0.iter().zip(other.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }

    fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// What the QR code carries.
///
/// Text format (one line, ASCII):
/// `localtugboat://pair?v=1&k=<64 hex pc key>&a=<addr>[,<addr>...]&s=<32 hex secret>&e=<expiry unix ms>&n=<name>`
/// - `addr` is `ip:port`, IPv6 as `[ip]:port`; 1..=MAX_QR_ADDRS of them.
/// - `n` is percent-encoded UTF-8 (unreserved ASCII `A-Za-z0-9-._~` literal, every other byte %XX).
/// - Parameter order is fixed as above when encoding; parsing accepts any order, ignores
///   unknown parameters (forward compatible), and rejects a repeated parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairPayload {
    pub pc_key: [u8; 32],
    pub addrs: Vec<SocketAddr>,
    pub secret: PairSecret,
    pub expires_at_ms: i64,
    pub pc_name: String,
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> Result<String, PairError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1..i + 3).ok_or_else(|| bad("n has a truncated %XX escape"))?;
            let v = std::str::from_utf8(h).ok().and_then(|t| u8::from_str_radix(t, 16).ok()).ok_or_else(|| bad("n has a bad %XX escape"))?;
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad("n is not valid UTF-8"))
}

impl PairPayload {
    /// Encode to the text format above.
    pub fn encode(&self) -> String {
        let addrs: Vec<String> = self.addrs.iter().map(|a| a.to_string()).collect();
        format!(
            "{PAYLOAD_PREFIX}v=1&k={}&a={}&s={}&e={}&n={}",
            hex::encode(self.pc_key),
            addrs.join(","),
            self.secret.to_hex(),
            self.expires_at_ms,
            pct_encode(&self.pc_name)
        )
    }

    /// Parse and validate. Every failure is `PairError::BadPayload{reason}` naming the field:
    /// - longer than MAX_PAYLOAD_LEN, wrong prefix, `v` != 1;
    /// - `k` not 64 hex chars, `s` not 32 hex chars, `e` not a decimal i64;
    /// - `a` empty, more than MAX_QR_ADDRS, unparsable, port 0, or any address that
    ///   `local::is_local_addr` rejects (the phone never dials a public address from a QR);
    /// - `n` not valid percent-encoding / UTF-8. The name is cleaned with `clean_name`.
    pub fn parse(text: &str) -> Result<PairPayload, PairError> {
        let text = text.trim();
        if text.len() > MAX_PAYLOAD_LEN {
            return Err(bad(format!("payload is {} bytes, the limit is {MAX_PAYLOAD_LEN}", text.len())));
        }
        if !text.is_ascii() {
            return Err(bad("payload is not ASCII"));
        }
        let query = text.strip_prefix(PAYLOAD_PREFIX).ok_or_else(|| bad("not a tugboat pairing code"))?;

        let mut seen: HashSet<&str> = HashSet::new();
        let (mut v, mut k, mut a, mut s, mut e, mut n) = (None, None, None, None, None, None);
        for part in query.split('&').filter(|p| !p.is_empty()) {
            let (key, val) = part.split_once('=').ok_or_else(|| bad(format!("parameter {part:?} has no value")))?;
            if !seen.insert(key) {
                return Err(bad(format!("parameter {key} is repeated")));
            }
            match key {
                "v" => v = Some(val),
                "k" => k = Some(val),
                "a" => a = Some(val),
                "s" => s = Some(val),
                "e" => e = Some(val),
                "n" => n = Some(val),
                _ => {} // forward compatible
            }
        }
        if v.ok_or_else(|| bad("missing v"))? != "1" {
            return Err(bad(format!("unsupported version v={}", v.unwrap_or(""))));
        }
        let pc_key: [u8; 32] = hex::decode(k.ok_or_else(|| bad("missing k"))?)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| bad("k is not 64 hex characters"))?;
        let secret: [u8; 16] = hex::decode(s.ok_or_else(|| bad("missing s"))?)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| bad("s is not 32 hex characters"))?;
        let expires_at_ms: i64 = e.ok_or_else(|| bad("missing e"))?.parse().map_err(|_| bad("e is not a decimal number"))?;

        let a = a.ok_or_else(|| bad("missing a"))?;
        let mut addrs = Vec::new();
        for item in a.split(',') {
            let sa: SocketAddr = item.parse().map_err(|_| bad(format!("a has an unparsable address {item:?}")))?;
            if sa.port() == 0 {
                return Err(bad(format!("a has port 0 in {item}")));
            }
            if !is_local_addr(sa) {
                return Err(bad(format!("a has an address that is not on the local network: {sa}")));
            }
            addrs.push(sa);
        }
        if addrs.is_empty() || addrs.len() > MAX_QR_ADDRS {
            return Err(bad(format!("a must hold 1 to {MAX_QR_ADDRS} addresses, found {}", addrs.len())));
        }
        let pc_name = clean_name(&pct_decode(n.unwrap_or(""))?, "PC");
        Ok(PairPayload { pc_key, addrs, secret: PairSecret(secret), expires_at_ms, pc_name })
    }
}

/// Unicode bidi control characters. Stripped from every name that came from a peer.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Device-name cleaning for names received in a payload or hello: strip Unicode control
/// (Cc) and bidi control chars (U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069), trim,
/// cap at MAX_NAME_CHARS chars; empty result -> `fallback`.
pub fn clean_name(raw: &str, fallback: &str) -> String {
    let stripped: String = raw.chars().filter(|c| !c.is_control() && !is_bidi_control(*c)).collect();
    let capped: String = stripped.trim().chars().take(MAX_NAME_CHARS).collect();
    let capped = capped.trim();
    if capped.is_empty() {
        fallback.to_string()
    } else {
        capped.to_string()
    }
}

/// The 6-digit code. `blake3::derive_key(CODE_CONTEXT, pc_key || phone_key || secret)`
/// (32 + 32 + 16 = 80 bytes), first 8 bytes as little-endian u64, modulo 1_000_000,
/// zero-padded to 6 digits ("004213"). Both sides compute it; it never travels on the wire.
pub fn pairing_code(pc_key: &[u8; 32], phone_key: &[u8; 32], secret: &PairSecret) -> String {
    let mut material = [0u8; 80];
    material[..32].copy_from_slice(pc_key);
    material[32..64].copy_from_slice(phone_key);
    material[64..].copy_from_slice(&secret.0);
    let out = blake3::derive_key(CODE_CONTEXT, &material);
    let n = u64::from_le_bytes(out[..8].try_into().expect("8 bytes"));
    format!("{:06}", n % 1_000_000)
}

/// "482913" -> "482 913" for display (both apps show it this way).
pub fn format_code(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairHello {
    pub proto: u32,
    /// 32 hex chars.
    pub secret: String,
    pub phone_name: String,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PairDecision {
    Accepted { pc_name: String },
    Refused { reason: PairError },
}

/// A code waiting for the PC user's click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPair {
    pub phone_key: [u8; 32],
    /// Cleaned with `clean_name(.., "Phone")`.
    pub phone_name: String,
    pub app_version: String,
    pub code: String,
    pub since_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowState {
    Open,
    Pending(PendingPair),
    Done,
    Cancelled(PairError),
}

/// What `on_hello` tells the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloOutcome {
    /// Show "Phone shows <code>? [Pair] [Cancel]".
    ShowCode(PendingPair),
    /// Same phone key sent a second hello while pending (network retry): replace the
    /// pending stream, keep the code, no cancel.
    Replaced(PendingPair),
}

/// The PC's pairing window. Pure state machine; time passed in.
///
/// Transitions (`now` = PC clock):
/// | state   | event                                   | result / new state                                   |
/// |---------|-----------------------------------------|------------------------------------------------------|
/// | Open    | hello, now >= expires_at                | Err(Expired); Cancelled(Expired)                     |
/// | Open    | hello, proto != PAIR_PROTO              | Err(VersionMismatch); stays Open                     |
/// | Open    | hello, wrong secret                     | Err(BadSecret); stays Open                           |
/// | Open    | hello, right secret                     | ShowCode; Pending                                    |
/// | Pending | hello, right secret, same phone key     | Replaced; Pending (same code)                        |
/// | Pending | hello, right secret, other phone key    | Err(SecretReused); Cancelled(SecretReused)           |
/// | Pending | hello, wrong secret                     | Err(BadSecret); stays Pending                        |
/// | Pending | confirm, now - since > CONFIRM_TIMEOUT  | Err(ConfirmTimeout); Cancelled(ConfirmTimeout)       |
/// | Pending | confirm                                 | Ok(PendingPair); Done                                |
/// | any     | cancel                                  | Cancelled(Cancelled) (Done stays Done)               |
/// | Done    | hello (any)                             | Err(SecretReused) (single use)                       |
/// | Cancelled(r) | hello / confirm                    | Err(NotOpen)                                         |
/// | Open/Pending | expire check, now >= expires_at and no Pending | Cancelled(Expired)                     |
/// A Pending window does not expire at expires_at (the user is looking at the code);
/// CONFIRM_TIMEOUT_MS bounds it instead.
#[derive(Debug)]
pub struct PairingWindow {
    pub pc_key: [u8; 32],
    pub secret: PairSecret,
    pub expires_at_ms: i64,
    state: WindowState,
}

impl PairingWindow {
    /// New window with a fresh secret, expiring at now + WINDOW_MS.
    pub fn open(pc_key: [u8; 32], now_ms: i64) -> Result<PairingWindow, NetError> {
        Ok(PairingWindow { pc_key, secret: PairSecret::generate()?, expires_at_ms: now_ms + WINDOW_MS, state: WindowState::Open })
    }

    /// The payload to render as the QR code.
    pub fn payload(&self, addrs: Vec<SocketAddr>, pc_name: &str) -> PairPayload {
        PairPayload {
            pc_key: self.pc_key,
            addrs,
            secret: self.secret.clone(),
            expires_at_ms: self.expires_at_ms,
            pc_name: clean_name(pc_name, "PC"),
        }
    }

    /// Apply a hello (table above). `phone_key` is the TLS-authenticated remote key.
    pub fn on_hello(&mut self, phone_key: [u8; 32], hello: &PairHello, now_ms: i64) -> Result<HelloOutcome, PairError> {
        match &self.state {
            WindowState::Cancelled(_) => return Err(PairError::NotOpen),
            WindowState::Done => return Err(PairError::SecretReused),
            WindowState::Open if now_ms >= self.expires_at_ms => {
                self.state = WindowState::Cancelled(PairError::Expired);
                return Err(PairError::Expired);
            }
            _ => {}
        }
        if hello.proto != PAIR_PROTO {
            return Err(PairError::VersionMismatch { ours: PAIR_PROTO, theirs: hello.proto });
        }
        // A malformed secret is a wrong secret: the caller learns nothing more than "no".
        let right = hex::decode(&hello.secret)
            .ok()
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .map(|b| self.secret.ct_eq(&b))
            .unwrap_or(false);
        if !right {
            return Err(PairError::BadSecret);
        }
        match &self.state {
            WindowState::Open => {
                let pending = PendingPair {
                    phone_key,
                    phone_name: clean_name(&hello.phone_name, "Phone"),
                    app_version: clean_name(&hello.app_version, ""),
                    code: pairing_code(&self.pc_key, &phone_key, &self.secret),
                    since_ms: now_ms,
                };
                self.state = WindowState::Pending(pending.clone());
                Ok(HelloOutcome::ShowCode(pending))
            }
            WindowState::Pending(p) if p.phone_key == phone_key => Ok(HelloOutcome::Replaced(p.clone())),
            WindowState::Pending(_) => {
                self.state = WindowState::Cancelled(PairError::SecretReused);
                Err(PairError::SecretReused)
            }
            // Handled above.
            WindowState::Done | WindowState::Cancelled(_) => unreachable!("closed states return before the secret check"),
        }
    }

    /// PC user clicked [Pair].
    pub fn confirm(&mut self, now_ms: i64) -> Result<PendingPair, PairError> {
        match &self.state {
            WindowState::Pending(p) => {
                if now_ms - p.since_ms > CONFIRM_TIMEOUT_MS {
                    self.state = WindowState::Cancelled(PairError::ConfirmTimeout);
                    return Err(PairError::ConfirmTimeout);
                }
                let p = p.clone();
                self.state = WindowState::Done;
                Ok(p)
            }
            // Nothing is waiting for a click: never complete a pairing from here.
            WindowState::Open | WindowState::Done | WindowState::Cancelled(_) => Err(PairError::NotOpen),
        }
    }

    /// PC user clicked [Cancel], or the window was closed by the app.
    pub fn cancel(&mut self) {
        if matches!(self.state, WindowState::Open | WindowState::Pending(_)) {
            self.state = WindowState::Cancelled(PairError::Cancelled);
        }
    }

    /// Expiry tick: Open past expires_at -> Cancelled(Expired); Pending past
    /// since + CONFIRM_TIMEOUT_MS -> Cancelled(ConfirmTimeout). Returns the new
    /// cancellation, if this call caused one, so the caller emits exactly one event.
    pub fn tick(&mut self, now_ms: i64) -> Option<PairError> {
        let reason = match &self.state {
            WindowState::Open if now_ms >= self.expires_at_ms => PairError::Expired,
            WindowState::Pending(p) if now_ms - p.since_ms > CONFIRM_TIMEOUT_MS => PairError::ConfirmTimeout,
            _ => return None,
        };
        self.state = WindowState::Cancelled(reason.clone());
        Some(reason)
    }

    /// True while hellos are accepted (Open or Pending). The caller keeps ALPN_PAIR in the
    /// endpoint's list exactly while this is true.
    pub fn accepting(&self) -> bool {
        matches!(self.state, WindowState::Open | WindowState::Pending(_))
    }

    /// The cancellation reason, if cancelled.
    pub fn cancelled(&self) -> Option<&PairError> {
        match &self.state {
            WindowState::Cancelled(r) => Some(r),
            _ => None,
        }
    }
}

/// PC side: accept the `StreamKind::Pair` stream on an ALPN_PAIR connection and read the
/// hello, bounded by `timeout` (10 s in core). Returns the send half for the decision.
pub async fn pc_read_hello(conn: &PeerConn, timeout: Duration) -> Result<(SendStream, PairHello), NetError> {
    let work = async {
        let (kind, tx, mut rx) = conn.accept_stream().await?;
        if kind != StreamKind::Pair {
            conn.close(close::PROTOCOL, "expected the pairing stream");
            return Err(NetError::Protocol(format!("expected a pairing stream, got {kind:?}")));
        }
        let hello: PairHello = read_frame_limited(&mut rx, MAX_PAIR_FRAME)
            .await?
            .ok_or_else(|| NetError::Protocol("pairing stream ended before the hello".into()))?;
        Ok((tx, hello))
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| NetError::Timeout(format!("no pairing hello within {}s", timeout.as_secs())))?
}

/// PC side: write the decision frame and finish the stream, then wait until the phone has
/// the bytes (closing the connection earlier can drop them). The caller closes the connection
/// afterwards (`close::NORMAL` on accept, `close::PAIRING_CLOSED` on refusal).
///
/// `Ok` means the phone received the whole decision. Core completes the pairing on the PC only
/// after `Ok` for an `Accepted` decision: on `Err` the phone may never have heard it, and a
/// PC that is paired with a phone that is not would be a silent half-pairing.
pub async fn pc_send_decision(tx: &mut SendStream, decision: &PairDecision) -> Result<(), NetError> {
    write_frame(tx, decision).await?;
    tx.finish().map_err(|e| stream_err("finish decision", &e))?;
    match tokio::time::timeout(DELIVERY_TIMEOUT, tx.stopped()).await {
        Err(_) => Err(NetError::Timeout("the phone did not confirm it received the decision".into())),
        // Ok(None): all data acknowledged. Ok(Some(_)): the phone stopped reading after it had
        // the whole frame. Both mean delivered.
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(stream_err("deliver decision", &e)),
    }
}

/// Phone side, step 1: dial the payload addresses on ALPN_PAIR with the payload key,
/// open the Pair stream, send the hello, compute the code.
pub async fn phone_start(
    node: &Node,
    payload: &PairPayload,
    phone_name: &str,
    app_version: &str,
    timeout: Duration,
) -> Result<PhonePairing, NetError> {
    let conn = node.dial(payload.pc_key, &payload.addrs, ALPN_PAIR, timeout).await.map_err(|e| match e {
        // A PC that is not in a pairing window does not offer the pairing protocol at all, and
        // QUIC reports that as a failed handshake. Say what it means.
        NetError::Connect(ref m) if m.contains("doesn't support any known protocol") => NetError::Pairing(PairError::NotOpen),
        other => other,
    })?;
    let (mut tx, rx) = conn.open_stream(StreamKind::Pair).await?;
    let hello = PairHello {
        proto: PAIR_PROTO,
        secret: payload.secret.to_hex(),
        phone_name: clean_name(phone_name, "Phone"),
        app_version: clean_name(app_version, ""),
    };
    write_frame(&mut tx, &hello).await?;
    tx.finish().map_err(|e| stream_err("finish hello", &e))?;
    let code = pairing_code(&payload.pc_key, &node.id(), &payload.secret);
    Ok(PhonePairing { code, conn, rx })
}

/// Phone side state after the hello: show `code`, then `wait_decision`.
#[derive(Debug)]
pub struct PhonePairing {
    pub code: String,
    pub conn: PeerConn,
    pub rx: RecvStream,
}

impl PhonePairing {
    /// Waits for the PC's decision, bounded by `timeout` (CONFIRM_TIMEOUT_MS + 10 s in core).
    /// Accepted -> Ok(pc_name, cleaned). Refused -> Err(NetError::Pairing(reason)).
    /// Connection closed without a decision -> Err(NetError::ClosedByPeer / Stream).
    pub async fn wait_decision(mut self, timeout: Duration) -> Result<String, NetError> {
        let got = tokio::time::timeout(timeout, read_frame_limited::<PairDecision>(&mut self.rx, MAX_PAIR_FRAME))
            .await
            .map_err(|_| NetError::Timeout(format!("no decision from the PC within {}s", timeout.as_secs())))??;
        let decision = match got {
            Some(d) => d,
            None => {
                return Err(match self.conn.conn.close_reason() {
                    Some(e) => map_conn_err(&e),
                    None => NetError::Stream("the PC ended the pairing stream without a decision".into()),
                })
            }
        };
        // Read the stream to its end before leaving: that is what tells the PC the decision
        // arrived. Closing first would make a delivered decision look undelivered on the PC.
        let mut one = [0u8; 1];
        let _ = tokio::time::timeout(DELIVERY_TIMEOUT, self.rx.read(&mut one)).await;
        // The PC closes after it sees the delivery; give it a moment, then leave ourselves.
        let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.conn.closed()).await;
        self.conn.close(close::NORMAL, "pairing finished");
        match decision {
            PairDecision::Accepted { pc_name } => Ok(clean_name(&pc_name, "PC")),
            PairDecision::Refused { reason } => Err(NetError::Pairing(reason)),
        }
    }
}
