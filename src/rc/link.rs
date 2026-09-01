//! The WSS link to the hub: connect, register, heartbeat, reconnect, replay.
//!
//! # Outbound only
//!
//! `agitd` dials the hub; the hub never dials back. Users' machines are behind
//! NAT, inside corporate firewalls, on changing IPs, and are often laptops that
//! sleep. Requiring an inbound port means requiring router configuration, and
//! then nobody uses the feature. One outbound WSS connection on 443 makes "no
//! public IP" and "no firewall change" true by default.
//!
//! # Reconnect is not a special case
//!
//! Disconnection destroys no state anywhere: the harness keeps running, the
//! journal keeps numbering, the hub keeps the workspace rows. Reconnect is
//! therefore just: dial, `rc.register` (carrying our per-stream `last_seq`),
//! and take the hub's `persisted_seq` to lift our own counters past anything it
//! already stored. Backoff is exponential from `BACKOFF_MIN_MS` to
//! `BACKOFF_MAX_MS` with jitter — without jitter a hub restart brings every
//! daemon back in the same millisecond.
//!
//! **Reconnect itself replays nothing.** A gap is not resent from the ring
//! buffer: `persisted_seq` only lifts the counters (a collision makes viewers
//! swallow new frames as old ones). There is one way to fill a gap — the viewer
//! sends `session.subscribe` with `after_seq`, and only then is anything taken
//! out of the ring. This has to be spelled out: whoever assumes the frames from
//! the disconnected stretch come back on their own misses that subscribe.

use crate::protocol::{
    ConnectionFeature, DeliveryStatus, ErrorCode, Frame, RcRegister, RcRegisterResult, RpcError,
    VERSION, method,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const BACKOFF_MIN_MS: u64 = 1_000;
pub const BACKOFF_MAX_MS: u64 = 30_000;
/// The socket pump is the only task polling Close/Pong/liveness. It may wait a
/// little for the daemon to drain an instruction, but never long enough for a
/// full internal queue to pin the current connection feature lease forever.
pub const EVENT_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a connection may stay unregistered before the link is replaced.
///
/// **Shorter than the liveness deadline.** Registration is the first thing on a
/// new connection (on the hub side it is one workspace reconciliation), and a
/// link slow enough to take two heartbeat periods is cheaper to reconnect than
/// to wait out. Staying under `HEARTBEAT_SECS * 3` also keeps the two tests
/// from expiring together — "never registered" and "half-open link" are two
/// diagnoses, and the one reported has to be the right one.
const REGISTER_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(crate::protocol::HEARTBEAT_SECS * 2);

fn connection_allows(frame: &Frame, epoch: u64, agent_identity_v1: bool) -> bool {
    let Some(delivery) = frame.connection_delivery.as_ref() else {
        return true;
    };
    delivery.status() == DeliveryStatus::Pending
        && delivery.epoch() == epoch
        && match delivery.feature() {
            ConnectionFeature::AgentIdentityV1 => agent_identity_v1,
        }
}

/// Next backoff delay, doubling with ±25% jitter.
pub fn next_backoff(current_ms: u64, seed: u64) -> u64 {
    let doubled = (current_ms.max(BACKOFF_MIN_MS))
        .saturating_mul(2)
        .min(BACKOFF_MAX_MS);
    // Deterministic jitter from a caller-supplied seed — no rand dependency in
    // the hot path, and testable.
    let spread = doubled / 4;
    let offset = if spread == 0 { 0 } else { seed % (spread * 2) };
    (doubled - spread + offset).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS)
}

/// Turn an `https://hub` base URL into the RC WebSocket URL.
///
/// `http` → `ws`, `https` → `wss`. A bare host is assumed to be TLS: getting
/// this wrong silently downgrades the connection carrying a long-lived token.
pub fn ws_url(hub_base: &str) -> String {
    let b = hub_base.trim_end_matches('/');
    let ws = if let Some(rest) = b.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = b.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("wss://{b}")
    };
    format!("{ws}/rc/ws")
}

/// What the link hands back to the daemon.
pub enum LinkEvent {
    Connected {
        epoch: u64,
        result: Box<RcRegisterResult>,
    },
    /// Boxed for the same reason as above: `Frame` is the largest variant here,
    /// and without the box every `LinkEvent` (including a `Disconnected` that
    /// carries one string) is allocated at its size.
    Frame {
        epoch: u64,
        frame: Box<Frame>,
    },
    Disconnected(String),
}

pub(crate) async fn deliver_event(
    events: &mpsc::Sender<LinkEvent>,
    event: LinkEvent,
) -> Result<(), String> {
    deliver_event_within(events, event, EVENT_DELIVERY_TIMEOUT).await
}

async fn deliver_event_within(
    events: &mpsc::Sender<LinkEvent>,
    event: LinkEvent,
    within: std::time::Duration,
) -> Result<(), String> {
    match tokio::time::timeout(within, events.send(event)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("daemon event receiver closed".into()),
        Err(_) => Err(wedged_queue_reason(within)),
    }
}

/// The one sentence reported when the queue wedges. Both delivery paths must
/// say the same thing — the reconnect loop takes it as the test for "the far
/// side did not reject us; we wedged ourselves".
fn wedged_queue_reason(within: std::time::Duration) -> String {
    format!(
        "daemon event queue stayed full for {}ms; closing the hub link",
        within.as_millis()
    )
}

async fn deliver_registration_within(
    events: &mpsc::Sender<LinkEvent>,
    epoch: u64,
    result: RcRegisterResult,
    on_registered: impl FnOnce(&RcRegisterResult),
    within: std::time::Duration,
) -> Result<(), String> {
    // **Reserve the slot, then grant authority, then push the event.** The
    // order of the three steps is forced at both ends.
    //
    // Authority cannot come first: the callback leases the connection
    // features to this socket, and a wedged queue keeps this pump from
    // getting back to select — the lease is live while nobody polls Close or
    // the liveness deadline. So with no slot nothing is granted: error out
    // and take another link.
    //
    // `send` cannot come first either: `send` wakes the receiver **before it
    // returns**, the daemon runs on a multi-threaded runtime, and another
    // worker can take the event before the next statement runs. The epoch is
    // then still the previous socket's (revoked with `+1` at disconnect),
    // the daemon's `Connected` branch is guarded by
    // `connection_epoch_is_current`, and a mismatch falls into the trailing
    // `=> {}` — each socket sends `Connected` once, and a dropped one is
    // never retried. The callback then supplies the epoch, every later
    // `Frame` is accepted, and the link looks entirely healthy while this
    // connection never reconciles once in its whole lifetime:
    // `adopt_persisted` does not run — settlement epochs start at 0 and the
    // first socket is 1, so **the first connection after a restart** is on
    // this same track, which is exactly where sequence numbers collide and
    // viewers swallow new frames as old ones; `mirror.adopt` does not run —
    // a directory the owner unbound from the web interface while this
    // machine was offline stays in the allowlist for the whole connection
    // (`project.unbind` only covers the unbind that happens while online);
    // `refresh_confinement` does not run with it, and local `agit rc status`
    // keeps saying offline.
    //
    // `reserve` splits the two apart: the slot is claimed first (a wedged
    // queue cannot get one, which settles the first test), and there is
    // nothing in it to take yet, so the receiver sees no event (which
    // settles the second). Once the permit is held `send` cannot fail, and
    // no seam is left between granting authority and delivering.
    let permit = match tokio::time::timeout(within, events.reserve()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err("daemon event receiver closed".into()),
        Err(_) => return Err(wedged_queue_reason(within)),
    };
    on_registered(&result);
    permit.send(LinkEvent::Connected {
        epoch,
        result: Box::new(result),
    });
    Ok(())
}

pub struct Link {
    hub: String,
    token: String,
}

impl Link {
    pub fn new(hub: &str, token: &str) -> Link {
        Link {
            hub: hub.to_string(),
            token: token.to_string(),
        }
    }

    /// Connect once, register, then pump frames in both directions until the
    /// socket dies. Returns the reason it ended.
    ///
    /// `outbound` borrows the receiver that **survives across reconnects**: a
    /// disconnect destroys nothing, and reconnecting only swaps in another
    /// socket that keeps taking from the same queue. Passing ownership in
    /// would mean a new queue on every reconnect, and the frames queued in
    /// between are lost.
    ///
    /// `epoch` is the monotonic daemon-local identity for this socket attempt.
    /// Every inbound event carries it so a slow daemon cannot process an old
    /// socket's queued instruction under a newer socket's feature ACK.
    pub async fn run_once(
        &self,
        epoch: u64,
        register: RcRegister,
        // Unbounded: the outbound queue cannot be throttled by backpressure,
        // because it carries RPC **responses** — dropping one is a duplicate
        // execution (this machine already did the work, the caller times out
        // and sends it again). Throttling belongs on the producing side and
        // drops only replayable stream events. See `OUTBOUND_EVENT_SOFT_CAP`
        // in the `daemon` main pump.
        outbound: &mut crate::rc::outbound::OutboundRx,
        events: mpsc::Sender<LinkEvent>,
        on_registered: impl FnOnce(&RcRegisterResult),
    ) -> String {
        self.run_once_within(
            epoch,
            register,
            outbound,
            events,
            on_registered,
            REGISTER_DEADLINE,
        )
        .await
    }

    /// [`run_once`](Self::run_once), with the registration deadline given by
    /// the caller.
    ///
    /// This seam exists for the same reason as the one in
    /// [`deliver_registration_within`]: the whole content of the test is
    /// "replace the link if registration has not completed by some instant",
    /// and testing against the real [`REGISTER_DEADLINE`] means either waiting
    /// that long for real or racing a virtual clock against a **real**
    /// socket — and such a test proves nothing when it is green. Production
    /// has one entry point, `run_once`.
    async fn run_once_within(
        &self,
        epoch: u64,
        register: RcRegister,
        outbound: &mut crate::rc::outbound::OutboundRx,
        events: mpsc::Sender<LinkEvent>,
        on_registered: impl FnOnce(&RcRegisterResult),
        register_deadline: std::time::Duration,
    ) -> String {
        let url = ws_url(&self.hub);
        let req = match build_request(&url, &self.token) {
            Ok(r) => r,
            Err(e) => return format!("bad hub url {url}: {e}"),
        };

        let (stream, _resp) = match tokio_tungstenite::connect_async(req).await {
            Ok(x) => x,
            Err(e) => return format!("cannot reach the hub at {url}: {e}"),
        };
        let (mut sink, mut source) = stream.split();

        // Register first; nothing else is valid before it.
        let reg_frame = Frame::request(method::RC_REGISTER, &register);
        let reg_id = reg_frame.id.clone();
        if let Err(e) = sink.send(Message::Text(reg_frame.to_json().into())).await {
            return format!("register failed to send: {e}");
        }

        let mut registered = false;
        let mut agent_identity_v1 = false;
        let mut on_registered = Some(on_registered);
        let mut hb = tokio::time::interval(std::time::Duration::from_secs(
            crate::protocol::HEARTBEAT_SECS,
        ));
        hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // **How long without hearing from the far side before it counts as
        // dead.**
        //
        // Heartbeats only go out. On a half-open TCP connection (NAT timeout,
        // a sleeping machine, a middlebox dropping the connection silently)
        // the bytes written go into a black hole **without erroring** —
        // `send` keeps succeeding until the write buffer fills. So `online`
        // stays true: no reconnect, no warning, the machine shows as online
        // in the web interface, and every instruction spins until it times
        // out. All the user can do is restart the daemon, with no way to see
        // why.
        //
        // Any inbound byte counts as "heard": a relayed instruction, the
        // hub's Close, and the Pong that comes back for our own Ping (see the
        // heartbeat branch below — that is the only steady echo on an idle
        // link). Three heartbeat periods without one means reconnect.
        let deadline = std::time::Duration::from_secs(crate::protocol::HEARTBEAT_SECS * 3);
        // **Recomputed at every hearing, not sampled on a fixed grid.**
        //
        // `interval(deadline)` samples on fixed ticks: when the last Pong
        // lands just after a tick, that tick reads "not expired yet" and the
        // next one is a whole period away — the real time to disconnect
        // approaches twice `deadline`, twice the limit claimed here.
        // `sleep_until(when it was heard + deadline)` recomputes every round,
        // so the deadline means what it says.
        let mut heard = tokio::time::Instant::now();

        // **Registration needs a deadline of its own.**
        //
        // The silence test above does not cover "connected but never
        // registered": `heard` is refreshed by **any** inbound message, and
        // the heartbeat branch sends a WebSocket Ping every `HEARTBEAT_SECS`,
        // **before** `if !registered { continue }`, so the Pong the far side
        // returns automatically per the protocol refreshes it inside the
        // `deadline` window again and again — forever.
        //
        // And that state does happen: the registration reply relayed as a
        // notification, an `id` that does not match, a skewed version making
        // `Frame::from_json` fail (the `continue // malformed frame` above)
        // ... and `registered` stays false. The consequences compound: the
        // whole outbound arm is held shut by `if registered`, so every RPC
        // response and stream frame sits in the queue;
        // `LinkEvent::Connected` is never sent, so this machine shows as
        // offline in the web interface; and `run_once` never returns, so the
        // reconnect loop never reconnects. The only way out is restarting
        // agitd, again with no way to see why.
        //
        let registration_deadline = tokio::time::Instant::now() + register_deadline;

        loop {
            tokio::select! {
                incoming = source.next() => {
                    heard = tokio::time::Instant::now();
                    let Some(msg) = incoming else { return "hub closed the connection".into() };
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => return format!("socket error: {e}"),
                    };
                    let text = match msg {
                        Message::Text(t) => t.to_string(),
                        Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        Message::Close(c) => {
                            return c.map(|f| format!("hub closed: {} {}", f.code, f.reason))
                                .unwrap_or_else(|| "hub closed".into());
                        }
                        Message::Frame(_) => continue,
                    };
                    let Ok(frame) = Frame::from_json(&text) else {
                        continue; // malformed frame from the hub: ignore, don't die
                    };

                    if !registered && frame.id == reg_id && frame.is_response() {
                        match frame.result_as::<RcRegisterResult>() {
                            Ok(res) => {
                                let identity_acked = res
                                    .accepted_features
                                    .iter()
                                    .any(|feature| feature == crate::protocol::feature::AGENT_IDENTITY_V1);
                                let Some(callback) = on_registered.take() else {
                                    return "hub sent more than one registration result".into();
                                };
                                if let Err(reason) = deliver_registration_within(
                                    &events,
                                    epoch,
                                    res,
                                    callback,
                                    EVENT_DELIVERY_TIMEOUT,
                                )
                                .await
                                {
                                    return reason;
                                }
                                agent_identity_v1 = identity_acked;
                                registered = true;
                            }
                            Err(e) => return format!("hub rejected the registration: {e}"),
                        }
                        continue;
                    }
                    if let Err(reason) = deliver_event(
                        &events,
                        LinkEvent::Frame {
                            epoch,
                            frame: Box::new(frame),
                        },
                    )
                    .await
                    {
                        return reason;
                    }
                }

                out = outbound.next_write(), if registered => {
                    let Some(pending) = out else { return "outbound channel closed".into() };
                    if !connection_allows(pending.frame(), epoch, agent_identity_v1) {
                        if let Some(delivery) = pending.frame().connection_delivery.clone() {
                            delivery.invalidate();
                        }
                        // An old feature-sensitive frame is not a retry on this
                        // socket. Remove this queue copy; every journal clone
                        // shares the invalidated state and is discarded too.
                        pending.commit();
                        continue;
                    }
                    let delivery = pending.frame().connection_delivery.clone();
                    // **Writes need a cap too.**
                    //
                    // Once a half-open connection fills the kernel
                    // send buffer, `send` stays Pending — and select
                    // runs the arm it picked to completion, so the
                    // silence deadline above is never polled again.
                    // One wedged write routes around that liveness
                    // test and `online` stays true. Not being able to
                    // write is the same thing as not hearing: this
                    // link is gone.
                    if let Err(e) =
                        write_within(&mut sink, pending.to_json(), left(heard, deadline)).await
                    {
                        return e;
                    }
                    // Removed for good only on a confirmed write; on
                    // Err/timeout/cancel the guard drops and the next
                    // run_once resends the same frame with the same
                    // request id once registration completes.
                    pending.commit();
                    if let Some(delivery) = delivery {
                        delivery.mark_delivered();
                    }
                }

                _ = tokio::time::sleep_until(heard + deadline) => {
                    return format!(
                        "no traffic from the hub for {}s — the connection is half-open; reconnecting",
                        heard.elapsed().as_secs()
                    );
                }

                _ = tokio::time::sleep_until(registration_deadline), if !registered => {
                    return format!(
                        "the hub never answered rc.register in {}s — reconnecting",
                        register_deadline.as_secs()
                    );
                }

                _ = hb.tick() => {
                    // **The Ping goes first, and does not wait for
                    // registration.**
                    //
                    // The silence test above needs the far side to
                    // **answer** something, and the hub does not answer
                    // `rc.heartbeat` (it only refreshes the online
                    // state). An idle link carries no inbound traffic
                    // at all, so the test alone would kick a healthy
                    // connection every `deadline`.
                    //
                    // A WebSocket Ping needs no line of code on the far
                    // side: the protocol requires an automatic Pong,
                    // and that Pong travels the very TCP path we are
                    // worried about — on a half-open connection it does
                    // not arrive. This is the only way to ask "are you
                    // still there" that does not change the protocol.
                    if let Err(e) = write_ping_within(&mut sink, left(heard, deadline)).await {
                        return e;
                    }
                    if !registered { continue }
                    let f = Frame::notification(method::RC_HEARTBEAT, serde_json::json!({}));
                    if let Err(e) = write_within(&mut sink, f.to_json(), left(heard, deadline)).await {
                        return e;
                    }
                }
            }
        }
    }
}

/// How much is left before the link counts as dead.
///
/// Handing every write the whole `deadline` again is wrong: the deadline says
/// "how long since the far side was last confirmed alive", not "how long this
/// one write may stall". A blocking write starting just before
/// `heard + deadline` would, on a fresh clock, push the real verdict out to
/// twice that; the heartbeat right after a successful Ping stalls again and
/// pushes it further. With the remainder, several writes together still stay
/// inside that line.
fn left(heard: tokio::time::Instant, deadline: std::time::Duration) -> std::time::Duration {
    (heard + deadline).saturating_duration_since(tokio::time::Instant::now())
}

/// Write one frame to the link, **with a cap**.
///
/// Once a half-open connection fills the kernel send buffer `send` stays
/// Pending, and select runs the arm it picked to completion — the silence
/// deadline arm is never polled again. Not being able to write is the same
/// thing as not hearing: this link is gone; report it and let the reconnect
/// loop take another.
async fn write_within<S>(
    sink: &mut S,
    text: String,
    within: std::time::Duration,
) -> Result<(), String>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    match tokio::time::timeout(within, sink.send(Message::Text(text.into()))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("send failed: {e}")),
        Err(_) => Err(format!(
            "the hub link accepted nothing for {}s — treating it as dead",
            within.as_secs()
        )),
    }
}

/// Same as above, for a WebSocket Ping.
async fn write_ping_within<S>(sink: &mut S, within: std::time::Duration) -> Result<(), String>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    match tokio::time::timeout(within, sink.send(Message::Ping(vec![].into()))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("ping failed: {e}")),
        Err(_) => Err(format!(
            "the hub link accepted no ping for {}s — treating it as dead",
            within.as_secs()
        )),
    }
}

/// Build the upgrade request with the RC bearer token.
///
/// A daemon is not a browser, so the plain `Authorization` header works and we
/// do not need the `Sec-WebSocket-Protocol` smuggling that browsers force.
fn build_request(
    url: &str,
    token: &str,
) -> crate::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid token characters"))?,
    );
    req.headers_mut().insert(
        "x-agit-protocol",
        VERSION
            .to_string()
            .parse()
            .expect("digits are a valid header value"),
    );
    Ok(req)
}

/// Build the error a hub sends when it sees a hole in our sequence. Here rather
/// than in the hub so both sides agree on the wording.
pub fn sequence_gap(stream: &str, expected: u64, got: u64) -> RpcError {
    RpcError::new(
        ErrorCode::SequenceGap,
        format!("stream {stream} jumped from {expected} to {got}"),
    )
    .with_hint("this means frames were lost or the daemon restarted without its watermark; reconnect to replay")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration() -> RcRegisterResult {
        RcRegisterResult {
            connection_id: "conn-1".into(),
            accepted_features: vec![crate::protocol::feature::AGENT_IDENTITY_V1.into()],
            workspaces: vec![],
            persisted_seq: Default::default(),
            server_time: "2026-08-22T00:00:00Z".into(),
        }
    }

    /// A malicious or simply faster hub can fill the daemon queue while the
    /// daemon is in one slow dispatch. Registration authority must not begin
    /// in that state, and the only socket pump must return promptly so its
    /// caller can revoke the epoch and reconnect.
    #[tokio::test]
    async fn a_full_daemon_queue_fails_fast_without_granting_the_ack() {
        let (events, mut receiver) = mpsc::channel(1);
        events
            .send(LinkEvent::Disconnected("occupied".into()))
            .await
            .unwrap();
        let granted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_flag = granted.clone();
        let started = std::time::Instant::now();
        let result = deliver_registration_within(
            &events,
            7,
            registration(),
            move |_| callback_flag.store(true, std::sync::atomic::Ordering::SeqCst),
            std::time::Duration::from_millis(10),
        )
        .await;

        assert!(result.unwrap_err().contains("queue stayed full"));
        assert!(!granted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(matches!(
            receiver.recv().await,
            Some(LinkEvent::Disconnected(reason)) if reason == "occupied"
        ));
    }

    /// The epoch that guards `Connected` must already be in effect the moment
    /// `Connected` can be seen.
    ///
    /// The daemon-side branch handling this event is guarded by
    /// `connection_epoch_is_current`, and what makes that guard hold is the
    /// callback here (`set_connection_features`). On a multi-threaded runtime
    /// `send` wakes the receiver **before it returns**: send first and call
    /// back second, and another worker can take the event before the callback
    /// runs. The epoch is then still the previous socket's (revoked with `+1`
    /// at disconnect), so the event lands in the trailing `=> {}` of
    /// `on_link_event` and is dropped silently — and `Connected` is sent once
    /// per socket, with no retry.
    ///
    /// After that drop the connection looks fine (`Frame`s carry the same
    /// epoch and are all accepted once the callback has run), yet it never
    /// reconciles once in its whole lifetime: `adopt_persisted` does not run
    /// (epochs start at 0 and the first socket is 1, so the first connection
    /// after a restart is on this track too, which is exactly where sequence
    /// numbers collide — viewers swallow new frames as old ones),
    /// `mirror.adopt` does not run (a directory the owner unbound from the web
    /// interface while offline stays live for the whole connection), and
    /// `refresh_confinement` does not run either.
    #[tokio::test]
    async fn connected_is_not_observable_before_the_epoch_that_guards_it() {
        let (events, mut receiver) = mpsc::channel(1);
        let mut visible_when_authority_began = None;
        deliver_registration_within(
            &events,
            7,
            registration(),
            |_| visible_when_authority_began = Some(receiver.try_recv().is_ok()),
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();

        assert_eq!(
            visible_when_authority_began,
            Some(false),
            "the daemon could dequeue Connected before the epoch guarding it was published; \
             deliver_registration_within must reserve the queue slot, grant the epoch, and only \
             then push the event — otherwise the daemon silently drops the one Connected this \
             socket ever sends"
        );
        assert!(
            matches!(
                receiver.recv().await,
                Some(LinkEvent::Connected { epoch: 7, .. })
            ),
            "reserving the slot must still actually deliver the event"
        );
    }

    /// Authority may begin only once the `Connected` slot is **claimed**.
    ///
    /// The test is not "the event is queued" — then the receiver could see it
    /// before the callback, as the test above shows. It is "the slot for this
    /// event is in hand and delivery can no longer fail": with no slot not one
    /// feature may be leased, or on a link with a wedged queue the lease is
    /// live while the pump cannot get back to select.
    #[tokio::test]
    async fn registration_authority_begins_only_after_the_connected_slot_is_secured() {
        let (events, mut receiver) = mpsc::channel(1);
        let observed_queued = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_observation = observed_queued.clone();
        let capacity = events.clone();
        deliver_registration_within(
            &events,
            7,
            registration(),
            move |_| {
                callback_observation.store(
                    capacity.capacity() == 0,
                    std::sync::atomic::Ordering::SeqCst,
                );
            },
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();

        assert!(
            observed_queued.load(std::sync::atomic::Ordering::SeqCst),
            "the connection features were leased while the Connected slot was still unclaimed; \
             reserve the slot first, or a wedged queue leaves the lease live on a pump that can \
             no longer poll Close or its liveness deadline"
        );
        assert!(
            matches!(
                receiver.recv().await,
                Some(LinkEvent::Connected { epoch: 7, result }) if result.connection_id == "conn-1"
            ),
            "securing the slot must still deliver this socket's one Connected event"
        );
    }

    #[test]
    fn a_connection_bound_frame_cannot_borrow_another_socket_ack() {
        let delivery = crate::protocol::ConnectionDelivery::new(
            7,
            crate::protocol::ConnectionFeature::AgentIdentityV1,
        );
        let mut frame = Frame::notification(
            crate::protocol::method::COMMIT_SETTLED,
            serde_json::json!({}),
        );
        frame.connection_delivery = Some(delivery.clone());

        assert!(connection_allows(&frame, 7, true));
        assert!(
            !connection_allows(&frame, 8, true),
            "a fast reconnect must not lend its ACK to the old queued frame"
        );
        assert!(
            !connection_allows(&frame, 7, false),
            "the matching epoch still needs the negotiated feature"
        );

        delivery.mark_delivered();
        assert!(
            !connection_allows(&frame, 7, true),
            "journal clones sharing a delivered fence must not emit twice"
        );
    }

    /// An unreadable registration reply must not leave the link hanging
    /// unregistered **forever**.
    ///
    /// The silence test structurally cannot cover this half: `heard` is
    /// refreshed by **any** inbound message (protocol Pongs included), and the
    /// heartbeat branch sends a WebSocket Ping every `HEARTBEAT_SECS`, before
    /// `if !registered { continue }` — so the Pong the far side returns
    /// automatically per the protocol keeps pushing the liveness deadline out.
    /// While unregistered the outbound arm is held shut by `if registered` and
    /// `LinkEvent::Connected` is never sent: every RPC response sits in the
    /// queue, this machine shows as offline in the web interface, and the
    /// reconnect loop never reconnects because `run_once` never returns at all.
    #[tokio::test]
    async fn an_unreadable_register_reply_does_not_wedge_the_link_unregistered_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // A hub that answers with something this client cannot read: a
        // skewed version, a relay that changed the `id`, a reply that came
        // back as a notification — all of them land on the same
        // `continue // malformed frame`. It still answers Pongs (tungstenite
        // does that automatically), so the link keeps echoing and the
        // liveness deadline never expires.
        let hub = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if msg.is_text() {
                    let _ = ws.send(Message::Text("{ not a frame".into())).await;
                }
            }
        });

        let link = Link::new(&format!("http://{addr}"), "token");
        let (_outbound_tx, mut outbound_rx) = crate::rc::outbound::channel();
        let (events, mut received) = mpsc::channel(4);
        let register = RcRegister {
            protocol_version: VERSION,
            machine_fingerprint: "fp".into(),
            display_name: "test".into(),
            agit_version: "0".into(),
            platform: "test".into(),
            capabilities: vec![],
            features: vec![],
            workspaces: vec![],
            last_seq: Default::default(),
        };

        // Real clock: the other end of this test is a real socket, and `heard`
        // is refreshed by the bytes arriving on it. The seam sets the
        // registration deadline far below `HEARTBEAT_SECS * 3`, so the
        // liveness deadline is out of reach in this test.
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            link.run_once_within(
                1,
                register,
                &mut outbound_rx,
                events,
                |_| panic!("an unreadable reply must never grant registration"),
                std::time::Duration::from_millis(200),
            ),
        )
        .await
        .expect("run_once must give up on the unregistered socket, not sit on it forever");

        assert!(
            reason.contains("rc.register"),
            "the reason must name the half that actually failed: {reason}"
        );
        assert!(
            received.try_recv().is_err(),
            "nothing was ever registered, so no Connected may have been delivered"
        );
        hub.abort();
    }

    /// The production entry point uses exactly the deadline that is shorter
    /// than the liveness one. A test that tuned that parameter through the
    /// seam cannot prove it.
    #[test]
    fn the_registration_deadline_expires_before_the_half_open_one() {
        let half_open = std::time::Duration::from_secs(crate::protocol::HEARTBEAT_SECS * 3);
        assert!(
            REGISTER_DEADLINE < half_open,
            "the two tests must not expire together, or the reported reason is a coin flip"
        );
        assert!(
            REGISTER_DEADLINE > std::time::Duration::from_secs(crate::protocol::HEARTBEAT_SECS),
            "one heartbeat period at minimum, so a normally slow registration is not kicked"
        );
    }

    #[test]
    fn scheme_maps_to_ws_and_a_bare_host_defaults_to_tls() {
        assert_eq!(ws_url("https://agent-git.com"), "wss://agent-git.com/rc/ws");
        assert_eq!(ws_url("http://127.0.0.1:8177"), "ws://127.0.0.1:8177/rc/ws");
        assert_eq!(
            ws_url("https://hub.example.com/"),
            "wss://hub.example.com/rc/ws"
        );
        // Bare host must not silently become plaintext — it carries a token.
        assert_eq!(ws_url("hub.example.com"), "wss://hub.example.com/rc/ws");
    }

    #[test]
    fn backoff_doubles_with_jitter_and_is_clamped() {
        let a = next_backoff(1000, 0);
        assert!((1500..=2500).contains(&a), "got {a}");
        // Saturates at the ceiling no matter how many times it doubles.
        let mut d = 1000;
        for i in 0..20 {
            d = next_backoff(d, i);
        }
        assert!((BACKOFF_MIN_MS..=BACKOFF_MAX_MS).contains(&d));
        // Jitter actually varies with the seed, so a hub restart doesn't bring
        // every daemon back in the same millisecond.
        assert_ne!(next_backoff(4000, 1), next_backoff(4000, 777));
    }
}
