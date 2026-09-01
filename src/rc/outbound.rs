//! Outbound queues: one for responses, one for replayable stream events.
//!
//! # Why three lanes
//!
//! Losing one of these frame kinds **does not cost what losing another costs**, and one shared
//! queue ties them together:
//!
//! * An **RPC response** carries no seq, so no replay brings it back. Losing one means this
//!   machine **has already done** the bind / send-message / mode-switch side effect while the
//!   caller waits out its timeout and retries, doing it a second time. And "lost" is not the
//!   only danger — **arriving late** is the same thing: a response delivered after the caller
//!   has already timed out is no different from one that never arrived, and the side effect
//!   repeats either way.
//! * A **stream event** has been through the journal's ring; one `session.subscribe` from the
//!   viewer brings it back.
//! * A **subscribe replay** sits between the two: it must not be lost (it **is** the answer to
//!   that subscribe, and no second path can supply it), but it must not cut in front of
//!   responses either — one replay is at most 8192 frames, and pushed whole into the response
//!   queue it makes the `turn.start` / interrupt / approval response behind it queue after the
//!   entire batch; on a slow link the caller simply times out, then retries a side effect that
//!   **already happened**.
//!   A replay must be enqueued as **one batch**: the batch is registered synchronously before
//!   the daemon takes live events again, and once the consumer starts a batch it stops pulling
//!   the live lane, which keeps a live frame with a larger seq from overtaking the replay.
//!
//! On one shared FIFO, two thousand events backed up on a slow link push responses behind them:
//! the response is not lost, but it queues behind those two thousand and the caller timed out
//! long ago. Hence the split into lanes, and **responses go first** on the way out.
//!
//! # Why the event lane is bounded
//!
//! A hand-written counter as the watermark of an unbounded queue does not work, for two reasons:
//! it increments on enqueue and clears only on reconnect, so a **healthy** long-lived connection
//! that accumulates to the watermark never sends a live event again; and an unbounded channel
//! cannot answer "how many are left", so the count can only be approximate.
//!
//! A bounded channel solves both at once: capacity is the watermark, a failed `try_send` is
//! "full", and taking one out frees one slot — exact, self-healing, no hand-written state.

use crate::protocol::Frame;
use std::collections::VecDeque;
use tokio::sync::{OwnedSemaphorePermit, mpsc};

/// How many replayable events may back up before this lane starts yielding.
///
/// Affects only "how long the live stream stalls while the link is blocked", never the
/// transcript: these frames are already in the journal.
pub const EVENT_CAP: usize = 2000;

struct ReplayBatch {
    frames: VecDeque<Frame>,
    // The slot lives with the whole batch until it has been consumed; so although the batch
    // channel never blocks a send, memory is still hard-capped by the daemon's
    // REPLAY_SLOTS × REPLAY_CAP.
    _slot: OwnedSemaphorePermit,
}

/// The producer side. Clone it freely — both channels are mpsc.
#[derive(Clone)]
pub struct OutboundTx {
    /// Responses and other non-replayable frames. **Unbounded**: backpressure here is data loss.
    replies: mpsc::UnboundedSender<Frame>,
    /// Subscribe replays are registered as batches. The number of batches is capped by the
    /// semaphore permit each one carries.
    replay: mpsc::UnboundedSender<ReplayBatch>,
    /// Replayable stream events. Bounded; dropped when full.
    events: mpsc::Sender<Frame>,
}

/// The consumer side, held by the link task and reused across reconnects.
pub struct OutboundRx {
    /// The one frame already taken from the three lanes whose WebSocket write is not yet
    /// confirmed.
    ///
    /// `Link` is rebuilt on every reconnect while the receiver is reused across them, so the
    /// lease has to live here. On a write error or a timeout the original Frame is kept
    /// (including the RequestId the hub minted), and the next connection resends it first.
    pending: Option<Frame>,
    replies: mpsc::UnboundedReceiver<Frame>,
    replay: mpsc::UnboundedReceiver<ReplayBatch>,
    active_replay: Option<ReplayBatch>,
    events: mpsc::Receiver<Frame>,
}

/// Which queue one frame takes.
///
/// The test is "can it be recovered once lost": a stream event that went through the journal
/// (a notification carrying `stream`) can be, nothing else can. A request-response pair carries
/// no `stream`, so this decision is structural, not a list.
fn replayable(f: &Frame) -> bool {
    f.is_notification() && f.stream.is_some()
}

/// The drain loop for the tail that **must not be dropped and must not cut the line**.
///
/// # Order is **not** enforced here
///
/// Taking a permit first and then waiting for a frame does not hold: an `OwnedPermit` reserves
/// **capacity**, not a **position** — position is decided at the moment `send()` is called.
/// While the permit is held, the main loop can still `try_send` a larger seq on the same stream,
/// and the resent frame still lands behind it.
///
/// Order is enforced upstream: a stream that has dropped a frame is **sealed** (see the `daemon`
/// outbound branch), after which its frames no longer enter the event queue and only the
/// must-not-drop frames take this tail. So no larger seq on the same stream can squeeze in
/// beside them, and this tail is a single-producer single-consumer FIFO: what goes in in one
/// order comes out in that order.
pub async fn drain_ordered(
    tx: OutboundTx,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    acknowledged: mpsc::UnboundedSender<String>,
) {
    while let Some(f) = rx.recv().await {
        let stream = f.stream.clone().unwrap_or_default();
        if matches!(tx.send_ordered(f).await, Sent::Closed) {
            return;
        }
        if acknowledged.send(stream).is_err() {
            return;
        }
    }
}

pub fn channel() -> (OutboundTx, OutboundRx) {
    let (replies_tx, replies_rx) = mpsc::unbounded_channel();
    // The batch channel never blocks by itself: the daemon has to be able to register a whole
    // batch atomically before it handles live events again. The memory cap is enforced by the
    // semaphore permit the ReplayBatch carries.
    let (replay_tx, replay_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::channel(EVENT_CAP);
    (
        OutboundTx {
            replies: replies_tx,
            replay: replay_tx,
            events: events_tx,
        },
        OutboundRx {
            pending: None,
            replies: replies_rx,
            replay: replay_rx,
            active_replay: None,
            events: events_rx,
        },
    )
}

/// The result of one enqueue.
#[derive(Debug, PartialEq)]
pub enum Sent {
    /// It went into a queue.
    Queued,
    /// A replayable event; the queue is full, so it was dropped.
    ///
    /// **The frame comes back**: the caller decides the consequence from what it is. A session
    /// event really can be recovered (the journal's ring plus `session.subscribe`), a terminal
    /// stream has no such path — dropped terminal bytes have to leave a mark on screen, and the
    /// "terminal ended" frame must not be dropped at all; it goes to the ordered tail to be
    /// resent.
    DroppedReplayable(Box<Frame>),
    /// The far side is gone (the link task exited).
    Closed,
}

impl OutboundTx {
    /// Registers one subscribe replay atomically. The caller must call this before letting the
    /// daemon main loop handle live events again; the consumer blocks the events lane until the
    /// whole batch has drained, while still letting RPC responses cut in.
    pub fn send_replay_batch(&self, mut frames: Vec<Frame>, slot: OwnedSemaphorePermit) -> Sent {
        for frame in &mut frames {
            frame.reliable = true;
        }
        if self
            .replay
            .send(ReplayBatch {
                frames: frames.into(),
                _slot: slot,
            })
            .is_ok()
        {
            Sent::Queued
        } else {
            Sent::Closed
        }
    }

    /// Is the event queue **really empty** right now.
    ///
    /// Deciding "the congestion has passed" needs this, not "a frame just enqueued": on a link
    /// that can pull only one frame at a time the queue bounces between "full" and "full minus
    /// one", and every bounce back reads as "back to normal". That turns "one notice per
    /// terminal per congestion" into "one notice per dropped frame" — the pane floods with
    /// notices, and every notice takes a cell that should have carried real output.
    pub fn events_drained(&self) -> bool {
        self.events.capacity() == self.events.max_capacity()
    }

    /// Sends one stream event that **must not be dropped and must not cut the line**: it queues
    /// at the **tail** of the event queue and waits while it is full.
    ///
    /// # Why it cannot take the reliable queue instead
    ///
    /// Marking the frame `reliable` to get "must not be dropped" does not work: it then takes
    /// the unbounded response queue, and that queue **leaves first**, overtaking smaller-seq
    /// frames from the same stream still sitting in the event queue. The hub identifies frames
    /// by `(stream, seq)`, and a larger number arriving first is a gap in the sequence — for a
    /// terminal, the consequence is that the web interface gets "the terminal exited" first and
    /// the last lines of output from before that exit second.
    ///
    /// Queueing at the tail has no such problem: tokio's bounded mpsc hands the freed slot
    /// **straight to the waiting sender**, never through a counter, so a later `try_send` cannot
    /// take it away. **Who** waits matters just as much — the caller must be an independent
    /// task, never the daemon's main loop (which would stop link events, watermark persistence
    /// and Ctrl-C along with it). See the tail task in `daemon`.
    pub async fn send_ordered(&self, f: Frame) -> Sent {
        debug_assert!(
            f.stream.is_some() && f.seq.is_some(),
            "ordered resend holds only for stream events carrying (stream, seq): method={:?}",
            f.method
        );
        match self.events.send(f).await {
            Ok(()) => Sent::Queued,
            Err(_) => Sent::Closed,
        }
    }

    pub fn send(&self, f: Frame) -> Sent {
        // **A frame with a stream must carry a seq.**
        //
        // The hub decides whether to fan a frame out with `Frame::is_event()` (a notification
        // with a stream and a seq), and a notification missing its seq lands in `_ =>` over
        // there and is silently dropped. The failure shape on this link: a terminal stream that
        // is not numbered, a gap mark minted out of nowhere — local tests all green, the web
        // interface blank.
        //
        // The test sits at this one outbound gate: whoever mints a frame, wherever, forgetting
        // the numbering blows up right here instead of becoming a "the far side cannot see it"
        // mystery.
        debug_assert!(
            f.stream.is_none() || f.seq.is_some(),
            "a frame with a stream must carry a seq; otherwise the hub silently drops it: method={:?}",
            f.method
        );
        if f.stream.is_some() && f.seq.is_none() {
            // A release build does not panic, and it does not pretend nothing happened
            // either: sending this frame is the same as throwing it away.
            eprintln!(
                "agitd: refusing to send a stream frame with no seq (method {:?}) — the hub would drop it silently",
                f.method
            );
            return Sent::DroppedReplayable(Box::new(f));
        }

        // A single frame marked "must not be dropped" takes the response queue: it is
        // unbounded, so a synchronous entry point can always get it in, and backpressure never
        // evicts it.
        //
        // (Batched subscribe replays go through `send_replay_batch` — every batch carries a
        // capacity permit. This is a different thing: one lone notification, such as the mark
        // saying "the terminal broke off here". It has to be recognized as `reliable` right
        // here — down the ordinary event path below it hits the `debug_assert` above: a debug
        // build panics on the spot, a release build stuffs it back into the already-full queue
        // and drops it a second time, so the mark never reaches the screen.)
        if f.reliable {
            return if self.replies.send(f).is_ok() {
                Sent::Queued
            } else {
                Sent::Closed
            };
        }
        if replayable(&f) {
            match self.events.try_send(f) {
                Ok(()) => Sent::Queued,
                Err(mpsc::error::TrySendError::Full(f)) => Sent::DroppedReplayable(Box::new(f)),
                Err(mpsc::error::TrySendError::Closed(_)) => Sent::Closed,
            }
        } else if self.replies.send(f).is_ok() {
            Sent::Queued
        } else {
            Sent::Closed
        }
    }
}

impl OutboundRx {
    /// Leases the next frame, **responses first**; only [`PendingWrite::commit`] removes it for
    /// good.
    ///
    /// `biased` is not an optimization: without it select picks a branch at random, so a backlog
    /// of events and the responses leave the queue in turns and a response still waits for about
    /// half the backlog before its turn. With it, a response goes out whenever one is pending —
    /// events are recoverable anyway.
    pub async fn next_write(&mut self) -> Option<PendingWrite<'_>> {
        if self.pending.is_none() {
            loop {
                if let Some(batch) = self.active_replay.as_mut() {
                    // A response still waits at most one replay frame; a live event has to wait
                    // for the whole batch, or a larger seq makes the hub drop the backfill that
                    // arrives after it as an old frame.
                    if let Ok(reply) = self.replies.try_recv() {
                        self.pending = Some(reply);
                        break;
                    }
                    if let Some(frame) = batch.frames.pop_front() {
                        self.pending = Some(frame);
                        if batch.frames.is_empty() {
                            self.active_replay = None;
                        }
                        break;
                    }
                    self.active_replay = None;
                    continue;
                }

                let open = tokio::select! {
                    biased;
                    Some(frame) = self.replies.recv() => {
                        self.pending = Some(frame);
                        true
                    },
                    Some(batch) = self.replay.recv() => {
                        self.active_replay = Some(batch);
                        true
                    },
                    Some(frame) = self.events.recv() => {
                        self.pending = Some(frame);
                        true
                    },
                    else => false,
                };
                if !open || self.pending.is_some() {
                    break;
                }
            }
        }
        self.pending.as_ref()?;
        Some(PendingWrite { rx: self })
    }
}

/// A two-phase lease on one WebSocket write.
///
/// Dropping the guard means "the write result is unknown or failed" and the frame stays in
/// `OutboundRx::pending`; only a `commit` after the send has definitely succeeded clears it. Do
/// not `send` a failed frame back into the queue: that puts it behind newer frames, and a replay
/// frame would be reclassified on the way.
#[must_use = "a pending write must be committed only after the socket send succeeds"]
pub struct PendingWrite<'a> {
    rx: &'a mut OutboundRx,
}

impl PendingWrite<'_> {
    pub fn frame(&self) -> &Frame {
        self.rx
            .pending
            .as_ref()
            .expect("PendingWrite always owns one pending frame")
    }

    pub fn to_json(&self) -> String {
        self.frame().to_json()
    }

    pub fn commit(self) {
        self.rx
            .pending
            .take()
            .expect("committing an existing pending frame");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: u64) -> Frame {
        let mut f = Frame::notification("session.event", json!({}));
        f.stream = Some("s-1".into());
        f.seq = Some(seq);
        f
    }

    fn term(method: &str, seq: u64) -> Frame {
        let mut f = Frame::notification(method, json!({ "terminal_id": "t-1" }));
        f.stream = Some("term:ws-1".into());
        f.seq = Some(seq);
        f
    }

    async fn recv_committed(rx: &mut OutboundRx) -> Frame {
        let pending = rx.next_write().await.expect("outbound frame");
        let frame = pending.frame().clone();
        pending.commit();
        frame
    }

    fn replay_slot() -> OwnedSemaphorePermit {
        std::sync::Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap()
    }

    /// **When the queue is full, the frame that must not be dropped must not cut the line
    /// either.**
    ///
    /// A lost `terminal.exited` leaves the pane waiting forever for an end that never comes, so
    /// it has to be resent. Resending it by marking it `reliable` puts it on the queue that
    /// **leaves first**, so it overtakes smaller-seq output from the same terminal still sitting
    /// in the event queue: the web interface gets "exited" first and the last lines before the
    /// exit second, and since the hub identifies frames by `(stream, seq)`, seeing the larger
    /// number first is a phantom gap.
    ///
    /// Queueing at the tail and waiting for a slot satisfies both. This test pins that order.
    #[tokio::test]
    async fn a_terminal_end_never_overtakes_output_that_was_already_queued() {
        let (tx, mut rx) = channel();
        // Fill this terminal's output up to the cap so the last frame cannot get in — this is
        // exactly where the real path drops a frame.
        for seq in 1..=EVENT_CAP as u64 {
            assert_eq!(tx.send(term("terminal.output", seq)), Sent::Queued);
        }
        let end = term("terminal.exited", EVENT_CAP as u64 + 1);
        assert!(matches!(tx.send(end.clone()), Sent::DroppedReplayable(_)));

        // Hand it to the "queue at the tail" path, the way the daemon's tail task does.
        let ordered = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send_ordered(end).await }
        });

        let mut seen = Vec::new();
        for _ in 0..=EVENT_CAP {
            seen.push(recv_committed(&mut rx).await.seq.unwrap());
        }
        assert_eq!(ordered.await.unwrap(), Sent::Queued);

        let ordered_seqs: Vec<u64> = (1..=EVENT_CAP as u64 + 1).collect();
        assert_eq!(
            seen, ordered_seqs,
            "seqs on one stream must arrive in order"
        );
    }

    /// A frame queued at the tail **beats a later `try_send`**.
    ///
    /// This is the mechanism the test above rests on: tokio's bounded mpsc hands the freed slot
    /// straight to the waiting sender, never through a counter. If the slot went back into a
    /// counter first, later output could cut in front of the waiting end frame — order breaks
    /// again, and the symptom shows only on a link that is genuinely blocked.
    #[tokio::test]
    async fn a_waiting_sender_gets_the_freed_slot_before_a_later_try_send() {
        let (tx, mut rx) = channel();
        for seq in 1..=EVENT_CAP as u64 {
            assert_eq!(tx.send(event(seq)), Sent::Queued);
        }
        let waiting = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send_ordered(event(EVENT_CAP as u64 + 1)).await }
        });
        // Let it actually get into the wait queue.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !waiting.is_finished(),
            "the queue is full, so it must still be waiting"
        );

        // Take one slot out: the slot must go **straight** to the waiting sender.
        assert_eq!(recv_committed(&mut rx).await.seq, Some(1));
        assert_eq!(waiting.await.unwrap(), Sent::Queued);

        // So a later `try_send` cannot get in — the queue is full again. If the slot went back
        // into a counter first, this frame could cut in front of the waiting one.
        assert!(
            matches!(
                tx.send(event(EVENT_CAP as u64 + 2)),
                Sent::DroppedReplayable(_)
            ),
            "a later sender took the slot; queueing at the tail does not hold"
        );

        let mut last = 0;
        while let Ok(f) = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            recv_committed(&mut rx),
        )
        .await
        {
            last = f.seq.unwrap();
        }
        assert_eq!(last, EVENT_CAP as u64 + 1, "the waiting frame comes last");
    }

    fn reply(id: &str) -> Frame {
        Frame::response(
            crate::protocol::RequestId::Str(id.into()),
            json!({"ok": true}),
        )
    }

    /// With the queue filled by two thousand events, a response still leaves **immediately** —
    /// it does not queue behind them.
    ///
    /// This is why the module exists: a late response and a lost response are the same thing; a
    /// response the caller receives after timing out saves nothing, and the side effect in front
    /// of it has already happened.
    #[tokio::test]
    async fn a_reply_never_waits_behind_a_backlog_of_events() {
        let (tx, mut rx) = channel();
        for i in 0..EVENT_CAP as u64 {
            assert_eq!(tx.send(event(i)), Sent::Queued);
        }
        assert_eq!(tx.send(reply("call-1")), Sent::Queued);

        let first = recv_committed(&mut rx).await;
        assert_eq!(
            first.id.map(|i| i.to_string()).as_deref(),
            Some("call-1"),
            "a response must not queue behind {} events",
            EVENT_CAP
        );
    }

    /// A full queue drops only the **replayable** kind, and dropping is per frame — take a few
    /// out and a few more fit. A counter that only ever goes up cannot do that: a healthy
    /// long-lived connection that accumulates to the watermark never sends a live event again.
    #[tokio::test]
    async fn a_full_event_queue_recovers_as_it_drains() {
        let (tx, mut rx) = channel();
        for i in 0..EVENT_CAP as u64 {
            assert_eq!(tx.send(event(i)), Sent::Queued);
        }
        assert!(matches!(tx.send(event(9999)), Sent::DroppedReplayable(_)));

        // Take one out and one more fits — no reconnect, and nobody has to clear a counter.
        recv_committed(&mut rx).await;
        assert_eq!(tx.send(event(9999)), Sent::Queued);
    }

    /// The response lane is never full: it is unbounded, because backpressure here is data loss.
    #[tokio::test]
    async fn replies_are_never_dropped_for_backpressure() {
        let (tx, _rx) = channel();
        for i in 0..(EVENT_CAP * 3) {
            assert_eq!(tx.send(reply(&format!("call-{i}"))), Sent::Queued);
        }
    }

    /// One subscribe replay **must not** be droppable, and **must not** stand in front of
    /// responses.
    ///
    /// It looks like a stream event (a notification with a stream), but it is the answer to that
    /// `session.subscribe`: once lost no second path supplies it, while the `from_seq` in the
    /// response claims the hole is already filled. The viewer sees a stretch of history that
    /// stays missing, cut off at the same place on every retry.
    #[tokio::test]
    async fn a_subscribe_replay_is_not_droppable_even_when_the_event_queue_is_full() {
        let (tx, mut rx) = channel();
        for i in 0..EVENT_CAP as u64 {
            assert_eq!(tx.send(event(i)), Sent::Queued);
        }
        // The queue is full, so an ordinary event yields — it can be recovered.
        assert!(matches!(tx.send(event(9999)), Sent::DroppedReplayable(_)));

        // A replay frame does not yield. Its capacity is capped by the permit the batch carries,
        // so it never takes a slot from live events.
        assert_eq!(
            tx.send_replay_batch(vec![event(1234)], replay_slot()),
            Sent::Queued
        );

        // It goes out **ahead** of the backlog of two thousand events — it is the answer to a
        // request, and the caller is waiting.
        let first = recv_committed(&mut rx).await;
        assert_eq!(first.seq, Some(1234));

        // But **behind responses**: one replay is at most 8192 frames, and letting it cut in
        // front of responses makes the `turn.start` / interrupt response behind it wait out the
        // whole batch; on a slow link the caller times out and then retries a side effect that
        // already happened.
        assert_eq!(
            tx.send_replay_batch(vec![event(4321)], replay_slot()),
            Sent::Queued
        );
        assert_eq!(tx.send(reply("call-9")), Sent::Queued);
        let next = recv_committed(&mut rx).await;
        assert_eq!(
            next.id.map(|i| i.to_string()).as_deref(),
            Some("call-9"),
            "a response must not queue behind a replay"
        );
    }

    /// Once subscribe dispatch has taken its journal snapshot it registers the response and the
    /// replay batch synchronously, and only then can the daemon main pump put a new live event
    /// into the events lane. The consumer has to treat that batch as a barrier; with every frame
    /// sent one by one from a freshly spawned, not-yet-polled task, seq=103 leaves first and the
    /// hub's watermark then dedupes 101 and 102 away as old frames.
    #[tokio::test]
    async fn a_registered_replay_batch_is_a_barrier_in_front_of_new_live_events() {
        let (tx, mut rx) = channel();
        assert_eq!(tx.send(reply("subscribe-1")), Sent::Queued);
        assert_eq!(
            tx.send_replay_batch(vec![event(101), event(102)], replay_slot()),
            Sent::Queued
        );
        assert_eq!(tx.send(event(103)), Sent::Queued);

        assert_eq!(
            recv_committed(&mut rx)
                .await
                .id
                .map(|id| id.to_string())
                .as_deref(),
            Some("subscribe-1")
        );
        assert_eq!(recv_committed(&mut rx).await.seq, Some(101));

        // RPC responses retain priority even in the middle of a replay batch.
        assert_eq!(tx.send(reply("interrupt-1")), Sent::Queued);
        assert_eq!(
            recv_committed(&mut rx)
                .await
                .id
                .map(|id| id.to_string())
                .as_deref(),
            Some("interrupt-1")
        );
        assert_eq!(recv_committed(&mut rx).await.seq, Some(102));
        assert_eq!(recv_committed(&mut rx).await.seq, Some(103));
    }

    /// A **single** notification marked "must not be dropped" — the terminal gap mark, for
    /// example — also goes out through the synchronous entry point, and backpressure never
    /// evicts it.
    ///
    /// The `event()` helper here carries **a seq**, exactly the condition the mark has to
    /// satisfy in production (see `daemon::gap_notice`). This test covers only what the queue
    /// keeps and what it drops; whether the mark itself is well formed is pinned over there.
    #[tokio::test]
    async fn a_one_off_reliable_notice_goes_out_through_the_sync_entry() {
        let (tx, mut rx) = channel();
        // Fill the event queue so ordinary events start yielding.
        for i in 0..EVENT_CAP as u64 {
            assert_eq!(tx.send(event(i)), Sent::Queued);
        }
        assert!(matches!(tx.send(event(9999)), Sent::DroppedReplayable(_)));

        // This one does not yield.
        let mut notice = event(7777);
        notice.reliable = true;
        assert_eq!(tx.send(notice), Sent::Queued);

        // And it goes out ahead of the backlog — what it says is "a stretch broke off back
        // there", and saying it late is pointless.
        let first = recv_committed(&mut rx).await;
        assert_eq!(first.seq, Some(7777));
    }

    /// A failed WebSocket write drops only the lease, never the frame; a new connection has to
    /// get the old response first — byte-identical, same request id — and only then the
    /// responses enqueued while the link was down.
    #[tokio::test]
    async fn an_uncommitted_write_is_retried_before_newer_replies_with_the_same_id() {
        let (tx, mut rx) = channel();
        let first = reply("call-1");
        let first_json = first.to_json();
        assert_eq!(tx.send(first), Sent::Queued);

        let failed = rx.next_write().await.expect("first write lease");
        assert_eq!(failed.to_json(), first_json);
        drop(failed); // Simulate a send error / a timeout / a cancelled Link future.

        assert_eq!(tx.send(reply("call-2")), Sent::Queued);
        let retry = rx.next_write().await.expect("retry lease");
        assert_eq!(retry.to_json(), first_json);
        assert_eq!(
            retry
                .frame()
                .id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("call-1")
        );
        retry.commit();

        let newer = rx.next_write().await.expect("newer reply");
        assert_eq!(
            newer
                .frame()
                .id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("call-2")
        );
        newer.commit();
    }

    /// With every sender closed, a frame already leased but unconfirmed still goes out on the
    /// next connection; the queue reports itself closed only after that frame commits.
    #[tokio::test]
    async fn closed_channels_still_flush_the_pending_write_once() {
        let (tx, mut rx) = channel();
        assert_eq!(tx.send(event(7)), Sent::Queued);
        let failed = rx.next_write().await.expect("pending event");
        drop(failed);
        drop(tx);

        let retry = rx
            .next_write()
            .await
            .expect("pending survives sender close");
        assert_eq!(retry.frame().seq, Some(7));
        retry.commit();
        assert!(rx.next_write().await.is_none());
    }

    /// A gone far side has to be reported — the caller decides from it whether to stop.
    #[tokio::test]
    async fn a_closed_link_is_reported_not_swallowed() {
        let (tx, rx) = channel();
        drop(rx);
        assert_eq!(tx.send(reply("call-1")), Sent::Closed);
        assert_eq!(tx.send(event(1)), Sent::Closed);
    }
}
