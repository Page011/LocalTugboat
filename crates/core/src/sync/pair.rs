//! Pairing flows over tugboat-net's pairing primitives. Owner: sync builder.
//!
//! PC: `begin` opens the window (QR payload), `handle_conn` serves each incoming pairing
//! connection, `confirm` is the [Pair] / [Cancel] click, `tick` expires the window.
//! Phone: `pair_with` runs the whole exchange from a scanned payload.
//!
//! Rules (PLAN "How it works / Pairing", notes/core-design.md section 3):
//! - The PC completes a pairing only after the decision reached the phone (`pc_send_decision`
//!   returned Ok): a PC that is paired with a phone that never heard "accepted" would be a
//!   silent half-pairing.
//! - A second phone using the secret while a code is pending cancels the window for both:
//!   the first phone gets Refused(SecretReused) through its pending connection, the second
//!   gets it directly, the PC shows "PAIRING BLOCKED - someone else used this code".
//! - The PC clock alone decides expiry (the injected clock, ticked by the app).

use std::time::Duration;

use tugboat_net::{
    close,
    pairing::{
        format_code, pc_read_hello, pc_send_decision, phone_start, HelloOutcome, PairDecision, PairError, PairPayload, PairingWindow,
        CONFIRM_TIMEOUT_MS,
    },
    NetError,
};
use tokio::sync::oneshot;

use crate::{
    events::{Event, PairingEvent},
    facade::{lock, Core},
    model::*,
    ops,
    sync::net::{net_err, Active},
    CoreError, Result,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Dialing the PC from the QR addresses.
const DIAL_TIMEOUT: Duration = Duration::from_secs(8);

type Decision = (PairDecision, oneshot::Sender<std::result::Result<(), NetError>>);

pub(crate) struct PendingConn {
    phone_key: [u8; 32],
    phone_name: String,
    decide: oneshot::Sender<Decision>,
}

pub(crate) struct PairState {
    window: PairingWindow,
    pending: Option<PendingConn>,
}

/// Stable snake_case code of a pairing error (the PairError serde tag).
pub fn pair_code(e: &PairError) -> String {
    serde_json::to_value(e).ok().and_then(|v| v.get("code").and_then(|c| c.as_str().map(String::from))).unwrap_or_else(|| "pairing".into())
}

/// One-line text, the same on both devices (notes/core-design.md section 3).
pub fn pair_text(e: &PairError) -> String {
    match e {
        PairError::SecretReused => "PAIRING BLOCKED - someone else used this code".into(),
        PairError::Expired => "PAIRING CODE EXPIRED - show a new QR".into(),
        PairError::Cancelled => "Pairing cancelled".into(),
        PairError::ConfirmTimeout => "Pairing timed out".into(),
        PairError::NotOpen => "Pairing is not open on the PC".into(),
        PairError::BadSecret => "WRONG PAIRING CODE - scan the QR again".into(),
        // `ours` is the PC's number: the side with the lower one is the side to update.
        PairError::VersionMismatch { ours, theirs } => format!("Update Tugboat on the {}", if ours < theirs { "PC" } else { "phone" }),
        PairError::BadPayload { reason } => format!("BAD PAIRING DATA - {reason}"),
    }
}

fn pair_err(e: &PairError) -> CoreError {
    CoreError::Pairing { code: pair_code(e), msg: pair_text(e) }
}

fn node_of(core: &Core) -> Option<tugboat_net::node::Node> {
    lock(&core.inner.net).as_ref().map(|n| n.node.clone())
}

// ---------- PC ----------

pub(crate) fn begin(core: &Core, replace_existing: bool) -> Result<String> {
    if core.inner.cfg.role != Role::Pc {
        return Err(CoreError::Invalid("only the PC opens a pairing window".into()));
    }
    let node = node_of(core).ok_or_else(|| CoreError::Invalid("the network is not started".into()))?;
    {
        let db = lock(&core.inner.db);
        if !replace_existing && crate::store::paired_peer(&db.conn)?.is_some() {
            return Err(CoreError::AlreadyPaired);
        }
    }
    let addrs = node.qr_addrs();
    if addrs.is_empty() {
        return Err(CoreError::Invalid("NO NETWORK - this PC has no local network address".into()));
    }
    let now = core.now();
    let window = PairingWindow::open(core.inner.cfg.self_id.0, now).map_err(net_err)?;
    let payload = window.payload(addrs, &core.inner.cfg.device_name).encode();
    let expires_at = window.expires_at_ms;
    let old = lock(&core.inner.pairing).replace(PairState { window, pending: None });
    if let Some(PairState { pending: Some(p), .. }) = old {
        // A window that was still waiting for a click is replaced: tell that phone.
        let (rtx, _) = oneshot::channel();
        let _ = p.decide.send((PairDecision::Refused { reason: PairError::Cancelled }, rtx));
    }
    node.set_pairing_open(true);
    core.emit(Event::Pairing(PairingEvent::WindowOpen { payload: payload.clone(), expires_at }));
    Ok(payload)
}

/// Close the window and tell everyone it ended for `reason` (PC UI, a pending phone).
fn finish_window(core: &Core, reason: PairError) {
    let st = lock(&core.inner.pairing).take();
    let Some(st) = st else { return };
    if let Some(node) = node_of(core) {
        node.set_pairing_open(false);
    }
    if let Some(p) = st.pending {
        let (rtx, _) = oneshot::channel();
        let _ = p.decide.send((PairDecision::Refused { reason: reason.clone() }, rtx));
    }
    core.emit(Event::Pairing(PairingEvent::Cancelled { code: pair_code(&reason), message: pair_text(&reason) }));
}

pub(crate) fn cancel(core: &Core) {
    finish_window(core, PairError::Cancelled);
}

/// Expire a window that ran out (Open past its expiry, Pending past the confirm timeout).
pub(crate) fn tick(core: &Core) {
    let now = core.now();
    let reason = lock(&core.inner.pairing).as_mut().and_then(|st| st.window.tick(now));
    if let Some(r) = reason {
        finish_window(core, r);
    }
}

enum HelloResult {
    Show { pending: tugboat_net::pairing::PendingPair, decision: oneshot::Receiver<Decision>, first: bool },
    Refuse { error: PairError, window_cancelled: bool },
}

/// Serve one incoming ALPN_PAIR connection.
pub(crate) async fn handle_conn(core: Core, peer: tugboat_net::node::PeerConn) {
    let phone_key = peer.peer();
    let (mut tx, hello) = match pc_read_hello(&peer, HELLO_TIMEOUT).await {
        Ok(x) => x,
        Err(e) => {
            tracing::debug!("pairing connection dropped before a hello: {e}");
            peer.close(close::PROTOCOL, "no pairing hello");
            return;
        }
    };
    let now = core.now();
    let result = {
        let mut g = lock(&core.inner.pairing);
        match g.as_mut() {
            None => HelloResult::Refuse { error: PairError::NotOpen, window_cancelled: false },
            Some(st) => match st.window.on_hello(phone_key, &hello, now) {
                Ok(outcome) => {
                    let (first, pp) = match outcome {
                        HelloOutcome::ShowCode(p) => (true, p),
                        HelloOutcome::Replaced(p) => (false, p),
                    };
                    let (dtx, drx) = oneshot::channel();
                    // A replaced sender is dropped here: the older task for the same phone sees
                    // its receiver close and ends quietly.
                    st.pending = Some(PendingConn { phone_key, phone_name: pp.phone_name.clone(), decide: dtx });
                    HelloResult::Show { pending: pp, decision: drx, first }
                }
                Err(error) => {
                    let window_cancelled = st.window.cancelled().is_some();
                    HelloResult::Refuse { error, window_cancelled }
                }
            },
        }
    };
    match result {
        HelloResult::Refuse { error, window_cancelled } => {
            // The phone hears the real reason, then the window (if the error killed it) closes.
            let _ = pc_send_decision(&mut tx, &PairDecision::Refused { reason: error.clone() }).await;
            peer.close(close::PAIRING_CLOSED, "pairing refused");
            if window_cancelled {
                finish_window(&core, error);
            }
        }
        HelloResult::Show { pending, decision, first } => {
            if first {
                core.emit(Event::Pairing(PairingEvent::Code { code: format_code(&pending.code), peer_name: pending.phone_name.clone() }));
            }
            tokio::select! {
                d = decision => match d {
                    Ok((decision, reply)) => {
                        let accepted = matches!(decision, PairDecision::Accepted { .. });
                        let r = pc_send_decision(&mut tx, &decision).await;
                        let _ = reply.send(r);
                        peer.close(if accepted { close::NORMAL } else { close::PAIRING_CLOSED }, "pairing finished");
                    }
                    Err(_) => peer.close(close::PAIRING_CLOSED, "pairing superseded"),
                },
                // The phone left. The code stays pending; a retry from the same phone replaces
                // it, otherwise the confirm timeout cancels the window.
                _ = peer.closed() => {}
            }
        }
    }
}

pub(crate) async fn confirm(core: &Core, accept: bool) -> Result<()> {
    if !accept {
        finish_window(core, PairError::Cancelled);
        return Ok(());
    }
    let now = core.now();
    let taken = {
        let mut g = lock(&core.inner.pairing);
        let Some(st) = g.as_mut() else { return Err(pair_err(&PairError::NotOpen)) };
        match st.window.confirm(now) {
            Ok(_) => {
                // The window is single use and now spent: take it out and close the pairing
                // port while still holding the lock. The delivery wait below can last 15 s; a
                // window opened meanwhile (begin_pairing is not serialized with this) is a new
                // one and this call must never touch it again.
                let pending = st.pending.take();
                g.take();
                if let Some(node) = node_of(core) {
                    node.set_pairing_open(false);
                }
                Ok(pending)
            }
            Err(e) => Err(e),
        }
    };
    let pending = match taken {
        Err(e) => {
            finish_window(core, e.clone());
            return Err(pair_err(&e));
        }
        Ok(None) => {
            // The window is already gone (taken above): only the screen needs telling.
            let e = PairError::NotOpen;
            core.emit(Event::Pairing(PairingEvent::Cancelled { code: pair_code(&e), message: pair_text(&e) }));
            return Err(pair_err(&e));
        }
        Ok(Some(p)) => p,
    };
    let (rtx, rrx) = oneshot::channel();
    let accepted = PairDecision::Accepted { pc_name: core.inner.cfg.device_name.clone() };
    let delivered = match pending.decide.send((accepted, rtx)) {
        Ok(()) => tokio::time::timeout(Duration::from_secs(15), rrx).await.ok().and_then(|r| r.ok()),
        Err(_) => None,
    };
    match delivered {
        Some(Ok(())) => {}
        _ => {
            let e = PairError::Cancelled;
            let message = "PAIRING FAILED - the phone did not confirm. Try again".to_string();
            core.emit(Event::Pairing(PairingEvent::Cancelled { code: "delivery_failed".into(), message: message.clone() }));
            return Err(CoreError::Pairing { code: pair_code(&e), msg: message });
        }
    }
    let row = DeviceRow {
        id: DeviceId(pending.phone_key),
        name: pending.phone_name.clone(),
        is_self: false,
        role: Role::Phone,
        paired_at: None,
        unpaired_at: None,
        last_seen_at: None,
        addresses: Vec::new(),
    };
    core.with_ctx(|c, ctx| ops::pairing_completed(c, ctx, &row))?;
    drop_other_sessions(core, row.id);
    core.emit(Event::Pairing(PairingEvent::Completed { peer: row.id, peer_name: row.name.clone() }));
    core.emit(Event::ActivityAdded);
    core.emit_status();
    core.inner.wake.notify_one();
    Ok(())
}

/// A session with a phone that is no longer the paired one must end (one phone per PC).
fn drop_other_sessions(core: &Core, keep: DeviceId) {
    let guard = lock(&core.inner.active);
    if let Some(Active { conn, .. }) = guard.as_ref() {
        if conn.peer() != keep.0 {
            conn.close(close::UNPAIRED, "replaced by another phone");
        }
    }
}

// ---------- phone ----------

/// Phone: the whole exchange. `cancel` abandons it (the user tapped Cancel) at any point up to the
/// PC's decision being read; after that the PC may already be paired with us, so the rest (drain,
/// close, store the pairing) always runs to the end.
pub(crate) async fn pair_with(core: &Core, payload: &str, mut cancel: impl std::future::Future<Output = ()> + Unpin) -> Result<DeviceRow> {
    if core.inner.cfg.role != Role::Phone {
        return Err(CoreError::Invalid("only the phone scans a pairing code".into()));
    }
    let parsed = PairPayload::parse(payload).map_err(|e| {
        let err = pair_err(&e);
        core.emit(Event::Pairing(PairingEvent::Cancelled { code: pair_code(&e), message: pair_text(&e) }));
        err
    })?;
    let node = node_of(core).ok_or_else(|| CoreError::Invalid("the network is not started".into()))?;
    let cancelled = |e: &NetError| {
        let (code, message) = match e {
            NetError::Pairing(pe) => (pair_code(pe), pair_text(pe)),
            other => (other.kind().to_string(), format!("PAIRING FAILED - {other}")),
        };
        core.emit(Event::Pairing(PairingEvent::Cancelled { code, message }));
    };
    let user_cancelled = || {
        let e = PairError::Cancelled;
        core.emit(Event::Pairing(PairingEvent::Cancelled { code: pair_code(&e), message: pair_text(&e) }));
        pair_err(&e)
    };
    let started = tokio::select! {
        biased;
        r = phone_start(&node, &parsed, &core.inner.cfg.device_name, &core.inner.cfg.app_version, DIAL_TIMEOUT) => r,
        _ = &mut cancel => return Err(user_cancelled()),
    };
    let mut phone = match started {
        Ok(p) => p,
        Err(e) => {
            cancelled(&e);
            return Err(net_err(e));
        }
    };
    core.emit(Event::Pairing(PairingEvent::Code { code: format_code(&phone.code), peer_name: parsed.pc_name.clone() }));
    let wait = Duration::from_millis(CONFIRM_TIMEOUT_MS as u64 + 10_000);
    let read = tokio::select! {
        // Biased: a decision that has already arrived always wins over a cancel.
        biased;
        r = phone.read_decision(wait) => r,
        _ = &mut cancel => return Err(user_cancelled()),
    };
    let decision = match read {
        Ok(d) => d,
        Err(e) => {
            cancelled(&e);
            return Err(net_err(e));
        }
    };
    // From here no cancel: the PC has decided and may already be paired with this phone.
    let pc_name = match phone.finish(decision).await {
        Ok(n) => n,
        Err(e) => {
            cancelled(&e);
            return Err(net_err(e));
        }
    };
    let row = DeviceRow {
        id: DeviceId(parsed.pc_key),
        name: pc_name.clone(),
        is_self: false,
        role: Role::Pc,
        paired_at: None,
        unpaired_at: None,
        last_seen_at: None,
        addresses: parsed.addrs.iter().map(|a| a.to_string()).collect(),
    };
    core.with_ctx(|c, ctx| ops::pairing_completed_phone(c, ctx, &row))?;
    drop_other_sessions(core, row.id);
    core.emit(Event::Pairing(PairingEvent::Completed { peer: row.id, peer_name: pc_name }));
    core.emit(Event::ActivityAdded);
    core.emit_status();
    core.inner.wake.notify_one();
    Ok(row)
}
