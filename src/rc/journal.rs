//! Per-session sequence allocation, replay buffer, and the durable watermark.
//!
//! Three jobs, all in service of one promise: *close the page, come back, lose
//! nothing.*
//!
//! 1. **Allocate `seq`** — monotonic and gap-free per stream (a stream is one
//!    logical session). See the module docs on [`super`] for why the producer
//!    and not the hub allocates it.
//! 2. **Buffer for replay** — a ring buffer per session, deliberately deeper
//!    than the hub's Redis retention window, so a client that reconnects after
//!    the hub's window has rolled can still be served from the machine.
//! 3. **Survive a daemon restart** — the next `seq` after a restart must be
//!    greater than anything the hub already persisted, or the hub will see a
//!    regression and (correctly) reject it as a hole. The watermark file is
//!    written on a debounce, and on register the hub's `persisted_seq` is
//!    folded in, so the counter only ever moves forward.
//!
//! The buffer holds *frames*, not events, because that's what replay needs to
//! re-send verbatim.

use crate::protocol::Frame;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

/// How many frames to keep per session. A long turn produces a few thousand
/// deltas; 8k covers "phone went through a tunnel" comfortably, and the memory
/// cost is bounded by the delta cap in the supervisor.
pub const RING_CAPACITY: usize = 8192;

/// Debounce for watermark persistence. Losing <1s of counter on a hard kill is
/// fine — the recovery path bumps past the hub's persisted seq anyway.
pub const WATERMARK_DEBOUNCE_MS: u64 = 500;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Watermark {
    #[serde(default)]
    seq: BTreeMap<String, u64>,
}

/// One session's event log.
#[derive(Debug)]
struct Stream {
    next: u64,
    ring: VecDeque<Frame>,
    /// Whether this stream's frames stay in the ring.
    ///
    /// Terminal streams do not: `session.subscribe` recognizes sessions and
    /// read-only following, **not `term:`**, so once those frames pile up they
    /// can never get out again — pure memory, pushing genuinely replayable
    /// session frames out of their own rings by LRU, and occupying the
    /// per-stream ring cap on top. PTY bytes are not "the agent's work record"
    /// in the first place (see the opening of `rc::terminal`); they do not
    /// enter version control.
    ///
    /// The seq is stamped either way: the hub deduplicates on `(stream, seq)`,
    /// and this field governs retention only.
    retained: bool,
}

impl Default for Stream {
    fn default() -> Stream {
        Stream {
            next: 0,
            ring: VecDeque::new(),
            // **Retain** by default: terminal streams are the only exception,
            // and an exception is written out explicitly (see `retained`).
            retained: true,
        }
    }
}

impl Stream {
    /// Stamp the seq, retain a copy when retention is on, and **hand the
    /// stamped frame back**.
    ///
    /// Handing it back rather than letting the caller read it off the tail of
    /// the ring: a terminal stream keeps no ring (see `retained`), so that read
    /// comes back empty.
    fn push(&mut self, mut f: Frame) -> Frame {
        self.next += 1;
        f.seq = Some(self.next);
        if self.retained {
            if self.ring.len() == RING_CAPACITY {
                self.ring.pop_front();
            }
            self.ring.push_back(f.clone());
        }
        f
    }

    /// Frames with `seq > after`. Second value is the lowest seq we still hold,
    /// so the caller can tell the client "history before N is gone".
    fn since(&self, after: u64) -> (Vec<Frame>, u64) {
        let lowest = self
            .ring
            .front()
            .and_then(|f| f.seq)
            .unwrap_or(self.next + 1);
        let out = self
            .ring
            .iter()
            .filter(|f| f.seq.is_some_and(|s| s > after))
            .cloned()
            .collect();
        (out, lowest)
    }
}

#[derive(Debug, Default)]
pub struct Journal {
    streams: BTreeMap<String, Stream>,
    dirty: bool,
}

impl Journal {
    pub fn new() -> Journal {
        Journal::default()
    }

    /// Load persisted watermarks so a restarted daemon never re-issues a seq.
    pub fn restored() -> Journal {
        let mut j = Journal::new();
        if let Some(w) = read_watermark() {
            for (k, v) in w.seq {
                j.streams.entry(k).or_default().next = v;
            }
        }
        j
    }

    /// Fold in the hub's high-water marks (register result). Only ever raises.
    pub fn adopt_persisted(&mut self, persisted: &BTreeMap<String, u64>) {
        for (stream, seq) in persisted {
            let s = self.streams.entry(stream.clone()).or_default();
            if *seq > s.next {
                s.next = *seq;
                self.dirty = true;
            }
        }
    }

    /// Stamp a frame with the next seq for its stream and buffer it.
    pub fn record(&mut self, stream: &str, frame: Frame) -> Frame {
        // `retained` is decided only when the stream is **created**: `forget`
        // turns it off, and that is exactly the state the next frame must not
        // overwrite (see `forget`).
        let terminal = stream.starts_with("term:");
        // Terminal streams are **numbered as usual, but kept out of the ring** —
        // two things, each with its own reason.
        //
        // The number is required: the hub decides whether to fan a frame out by
        // `Frame::is_event()` (a notification + a stream + a seq) and
        // deduplicates on `(stream, seq)`. A terminal frame with no seq is not
        // an event over there at all; it falls silently into `_ =>` and is
        // dropped.
        //
        // Keeping it out of the ring is deliberate: a PTY is a stream, not a
        // record, and replaying a stretch of bytes detached from the screen
        // state of the time means nothing. The cost is that what is lost is
        // lost, so the `after_seq` backfill does not hold for `term:` — see that
        // exception in the `crate::protocol` module docs.
        let s = self
            .streams
            .entry(stream.to_string())
            .or_insert_with(|| Stream {
                retained: !terminal,
                ..Stream::default()
            });
        // `restored` / `adopt_persisted` can create the entry before the first
        // frame arrives, when all we know is a persisted sequence watermark.
        // Those paths use `Stream::default()` (`retained: true`), so relying on
        // `or_insert_with` alone would make a pre-created terminal stream retain
        // PTY bytes forever. The stream name is authoritative at this single
        // write entrance: terminal streams are never replayable, regardless of
        // how their counter came into existence.
        if terminal && s.retained {
            s.retained = false;
            s.ring.clear();
            s.ring.shrink_to_fit();
        }
        let f = s.push(frame);
        self.dirty = true;
        f
    }

    pub fn last_seq(&self, stream: &str) -> u64 {
        self.streams.get(stream).map(|s| s.next).unwrap_or(0)
    }

    pub fn all_last_seq(&self) -> BTreeMap<String, u64> {
        self.streams
            .iter()
            .map(|(k, v)| (k.clone(), v.next))
            .collect()
    }

    /// Replay. Returns the frames after `after`, and the lowest seq still held
    /// (frames in `(after, lowest)` are permanently gone from the live path —
    /// the committed transcript still has them).
    pub fn replay(&self, stream: &str, after: u64) -> (Vec<Frame>, u64) {
        self.streams
            .get(stream)
            .map(|s| s.since(after))
            .unwrap_or((vec![], 0))
    }

    /// This stream will get no more frames: **release its ring, but remember
    /// where the numbering got to**.
    ///
    /// Each half has its own reason:
    ///
    /// * The ring goes. A session that has run to the end holds a full ring, and
    ///   a daemon stays up for days — every session that ends permanently
    ///   occupies one more, and nobody ever comes to fetch it.
    /// * The number stays. The same logical id can be pulled back up by
    ///   `session.resume`, and the seq must keep climbing: restart it from 1 and
    ///   the hub, deduplicating on `(stream, seq)`, swallows the whole opening
    ///   of the new session as a repeat of old frames. This is also why `next`
    ///   is persisted.
    pub fn forget(&mut self, stream: &str) {
        if let Some(s) = self.streams.get_mut(stream) {
            s.ring.clear();
            s.ring.shrink_to_fit();
            // **Turn the "still retaining?" switch off at the same time.**
            //
            // Clearing once is not enough: the end notification and the event
            // frames travel on two independent queues, consumed competitively
            // by the same select loop — enqueuing first on the sender side does
            // not mean the receiver finishes the frames first. When the
            // notification is selected first, the trailing frames already in the
            // queue still enter the ring afterwards, so it grows back, and
            // nobody comes to clear it a second time.
            //
            // With the switch off, those trailing frames are numbered as usual
            // and go out on the wire as usual; they just stop piling into the
            // buffer of a stream that has already ended — and the point of that
            // buffer (replaying a stretch of a **live** session after a
            // disconnect) no longer holds for it.
            s.retained = false;
        }
        self.dirty = true;
    }

    /// This stream is alive again (the same logical id pulled back up by
    /// `session.resume`): **reopen the buffer**.
    ///
    /// The switch `forget` turns off is meant for "a stream that has ended", but
    /// a logical session can come back — without reopening it, **not one frame**
    /// of the restored session enters the ring, so across its whole lifetime
    /// `session.subscribe` can backfill nothing, and every viewer reconnect
    /// counts on it.
    ///
    /// The numbering carries on as before (`forget` never touched it): starting
    /// over makes the hub swallow the opening of the new stretch as a duplicate
    /// on `(stream, seq)`.
    pub fn resume(&mut self, stream: &str) {
        // Terminal streams are never retained; this does not apply to them.
        if stream.starts_with("term:") {
            return;
        }
        if let Some(s) = self.streams.get_mut(stream) {
            s.retained = true;
        }
    }

    /// Persist watermarks if anything changed. Cheap enough to call on a timer.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let w = Watermark {
            seq: self.all_last_seq(),
        };
        if write_watermark(&w).is_ok() {
            self.dirty = false;
        }
    }
}

fn watermark_path() -> crate::Result<PathBuf> {
    Ok(super::rc_dir()?.join("watermark.json"))
}

fn read_watermark() -> Option<Watermark> {
    let p = watermark_path().ok()?;
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

fn write_watermark(w: &Watermark) -> crate::Result<()> {
    let p = watermark_path()?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(w)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

#[cfg(test)]
mod tests {

    /// Terminal frames are **numbered as usual** but never enter the ring.
    ///
    /// The seq is mandatory: `Frame::is_event()` is defined as "a notification +
    /// a stream + **a seq**", and the hub uses exactly that to decide whether to
    /// fan a frame out — a terminal frame with no seq is dropped outright over
    /// there, and the panel goes black.
    ///
    /// The ring is not: `session.subscribe` does not recognize `term:`, so once
    /// those frames pile up they can never get out again.
    #[test]
    fn terminal_frames_are_numbered_but_never_buffered() {
        let mut j = Journal::default();
        for i in 1..=3 {
            let f = j.record(
                "term:ws-1",
                Frame::notification("terminal.output", serde_json::json!({})),
            );
            assert_eq!(
                f.seq,
                Some(i),
                "a frame with no seq is not an event to the hub"
            );
        }
        assert!(
            j.replay("term:ws-1", 0).0.is_empty(),
            "terminal bytes must never stay in the ring"
        );

        // A session stream is retained as usual.
        j.record(
            "s-1",
            Frame::notification("item.completed", serde_json::json!({})),
        );
        assert_eq!(j.replay("s-1", 0).0.len(), 1);
    }

    /// A hub watermark can pre-create the stream before its first local frame.
    /// That must not accidentally opt a terminal stream into the replay ring.
    #[test]
    fn a_restored_terminal_watermark_never_enables_retention() {
        let mut j = Journal::default();
        j.adopt_persisted(&BTreeMap::from([("term:ws-1".into(), 99)]));

        let f = j.record(
            "term:ws-1",
            Frame::notification("terminal.output", serde_json::json!({})),
        );
        assert_eq!(f.seq, Some(100), "the persisted watermark still advances");
        assert!(
            j.replay("term:ws-1", 0).0.is_empty(),
            "a pre-created terminal stream must not retain PTY bytes"
        );
    }

    /// A stream that ended and is then pulled back up by resume reopens its
    /// buffer.
    ///
    /// Without reopening it, not one frame of the restored session enters the
    /// ring, so across its whole lifetime `session.subscribe` can backfill
    /// nothing — and every viewer reconnect counts on it.
    #[test]
    fn resuming_a_forgotten_stream_starts_buffering_again() {
        let mut j = Journal::default();
        j.record(
            "s-1",
            Frame::notification("item.completed", serde_json::json!({})),
        );
        j.forget("s-1");
        assert!(j.replay("s-1", 0).0.is_empty());

        j.resume("s-1");
        let f = j.record(
            "s-1",
            Frame::notification("item.completed", serde_json::json!({})),
        );
        assert_eq!(f.seq, Some(2), "the numbering carries on");
        assert_eq!(
            j.replay("s-1", 0).0.len(),
            1,
            "buffering restarts after a resume"
        );

        // Terminal streams are out of scope: they are never retained.
        j.record(
            "term:ws-1",
            Frame::notification("terminal.output", serde_json::json!({})),
        );
        j.resume("term:ws-1");
        j.record(
            "term:ws-1",
            Frame::notification("terminal.output", serde_json::json!({})),
        );
        assert!(j.replay("term:ws-1", 0).0.is_empty());
    }

    /// Ending a stream releases its replay buffer **but keeps the numbering
    /// climbing**.
    ///
    /// If the seq restarted from 1, the hub deduplicates on `(stream, seq)`, so
    /// when the same logical id is pulled back up by `session.resume` the
    /// opening of the new session is swallowed whole as a repeat of old
    /// frames.
    #[test]
    fn forgetting_a_stream_frees_the_buffer_but_keeps_the_counter() {
        let mut j = Journal::default();
        for _ in 0..10 {
            j.record(
                "s-1",
                Frame::notification("item.completed", serde_json::json!({})),
            );
        }
        assert_eq!(j.replay("s-1", 0).0.len(), 10);

        j.forget("s-1");
        assert!(
            j.replay("s-1", 0).0.is_empty(),
            "the buffer must be released"
        );

        // A **late trailing frame**: it is numbered as usual (carrying on from
        // where the counter stood, not restarting from 1 — starting over makes
        // the hub swallow the opening of the new session whole as a duplicate on
        // `(stream, seq)`), but it must not let the buffer grow back.
        //
        // The end notification and the event frames travel on two independent
        // queues, consumed competitively by the same select: when the
        // notification is selected first, the trailing frames already in the
        // queue still enter the ring afterwards, and nobody comes to clear it a
        // second time.
        let f = j.record(
            "s-1",
            Frame::notification("item.completed", serde_json::json!({})),
        );
        assert_eq!(
            f.seq,
            Some(11),
            "a restarted counter reads to the hub as a duplicate"
        );
        assert!(
            j.replay("s-1", 0).0.is_empty(),
            "a late trailing frame must not refill the released buffer"
        );
    }

    use super::*;
    use crate::protocol::method;

    fn ev(text: &str) -> Frame {
        Frame::notification(
            method::ITEM_DELTA,
            serde_json::json!({ "item_id": "i", "text": text }),
        )
    }

    #[test]
    fn seq_is_monotonic_and_gap_free_per_stream() {
        let mut j = Journal::new();
        for i in 1..=5 {
            let f = j.record("a", ev(&i.to_string()));
            assert_eq!(f.seq, Some(i));
        }
        // A second stream numbers independently.
        assert_eq!(j.record("b", ev("x")).seq, Some(1));
        assert_eq!(j.last_seq("a"), 5);
        assert_eq!(j.last_seq("b"), 1);
    }

    #[test]
    fn replay_returns_only_frames_after_the_clients_position() {
        let mut j = Journal::new();
        for i in 1..=10 {
            j.record("a", ev(&i.to_string()));
        }
        let (frames, lowest) = j.replay("a", 7);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames.first().unwrap().seq, Some(8));
        assert_eq!(lowest, 1);
    }

    #[test]
    fn ring_drops_oldest_and_reports_the_truncation_point() {
        let mut j = Journal::new();
        for i in 1..=(RING_CAPACITY + 10) {
            j.record("a", ev(&i.to_string()));
        }
        let (frames, lowest) = j.replay("a", 0);
        assert_eq!(frames.len(), RING_CAPACITY);
        assert_eq!(lowest, 11, "the first 10 rolled out of the ring");
    }

    #[test]
    fn adopting_the_hubs_watermark_never_lowers_the_counter() {
        let mut j = Journal::new();
        for _ in 0..3 {
            j.record("a", ev("x"));
        }
        // Hub is behind us — must not rewind.
        j.adopt_persisted(&BTreeMap::from([("a".into(), 2)]));
        assert_eq!(j.last_seq("a"), 3);
        // Hub is ahead (we restarted and lost the tail) — jump forward, so the
        // next frame cannot collide with something the hub already stored.
        j.adopt_persisted(&BTreeMap::from([("a".into(), 99)]));
        assert_eq!(j.last_seq("a"), 99);
        assert_eq!(j.record("a", ev("x")).seq, Some(100));
    }
}
