//! Client writers pace replay independently while RPC receipts retain priority.
//!
//! Nothing here waits in the shared endpoint loop. A slow reader loses only what it can recover:
//! session events through its journal cursor, terminal output behind a visible notice. A reply
//! waits for budget in its own task while the reader drains, and the client closes only when it
//! stops draining.

use super::{CLIENT_QUEUE, MAX_FRAME, MAX_PENDING, Work};
use crate::protocol::Frame;
use anyhow::ensure;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

mod recovery;

/// Two largest frames fit at once, so one large reply cannot starve the next one.
const REPLY_BYTES: usize = 2 * MAX_FRAME;
/// Replies produced while the budget is held wait here, bounded like the budget itself.
const WAITING_REPLY_BYTES: usize = 2 * MAX_FRAME;
/// A reader that frees no reply budget for this long has stopped draining.
const REPLY_WAIT: Duration = Duration::from_secs(30);

struct ClientLease {
    _bytes: Option<OwnedSemaphorePermit>,
    _work: Option<Work>,
    _replay: Option<OwnedSemaphorePermit>,
    _recovery: Option<recovery::Completed>,
}

enum ClientRecords {
    Single(Option<String>),
    Replay(std::collections::VecDeque<Frame>),
}

impl ClientRecords {
    fn is_empty(&self) -> bool {
        match self {
            Self::Single(record) => record.is_none(),
            Self::Replay(frames) => frames.is_empty(),
        }
    }
}

struct ClientMessage {
    records: ClientRecords,
    lease: std::sync::Arc<ClientLease>,
}

impl ClientMessage {
    fn single(record: String, bytes: OwnedSemaphorePermit, work: Option<Work>) -> Self {
        Self {
            records: ClientRecords::Single(Some(record)),
            lease: std::sync::Arc::new(ClientLease {
                _bytes: Some(bytes),
                _work: work,
                _replay: None,
                _recovery: None,
            }),
        }
    }
}

pub(super) enum Next {
    Write(ClientWrite),
    Recover(Box<Frame>),
}

pub(super) struct ClientWrite {
    pub(super) record: String,
    _lease: std::sync::Arc<ClientLease>,
    _replay_bytes: Option<OwnedSemaphorePermit>,
}

impl ClientMessage {
    async fn pop(
        &mut self,
        bytes: &std::sync::Arc<Semaphore>,
    ) -> anyhow::Result<Option<ClientWrite>> {
        let (record, permit) = match &mut self.records {
            ClientRecords::Single(record) => (record.take(), None),
            ClientRecords::Replay(frames) => {
                let Some(frame) = frames.pop_front() else {
                    return Ok(None);
                };
                let record = frame.to_json();
                ensure!(
                    record.len() <= MAX_FRAME,
                    "replay RPC frame exceeds its limit"
                );
                let permit = bytes
                    .clone()
                    .acquire_many_owned(record.len().max(1) as u32)
                    .await?;
                (Some(record), Some(permit))
            }
        };
        Ok(record.map(|record| ClientWrite {
            record,
            _lease: self.lease.clone(),
            _replay_bytes: permit,
        }))
    }
}

pub(super) struct ClientMessages {
    replies: mpsc::Receiver<ClientMessage>,
    backfills: mpsc::Receiver<ClientMessage>,
    recovery: std::sync::Arc<recovery::Recovery>,
    events: mpsc::Receiver<ClientMessage>,
    active: Option<ClientMessage>,
    replay_bytes: std::sync::Arc<Semaphore>,
}

impl ClientMessages {
    pub(super) async fn next(&mut self) -> anyhow::Result<Option<Next>> {
        loop {
            // RPC receipts may pass replay; live stream frames must retain their order.
            if let Ok(mut response) = self.replies.try_recv() {
                return Ok(response.pop(&self.replay_bytes).await?.map(Next::Write));
            }
            if let Some(batch) = &mut self.active {
                let record = batch.pop(&self.replay_bytes).await?;
                if batch.records.is_empty() {
                    self.active = None;
                }
                if record.is_some() {
                    return Ok(record.map(Next::Write));
                }
            }
            if let Ok(batch) = self.backfills.try_recv() {
                self.active = Some(batch);
                continue;
            }
            if let Ok(message) = self.events.try_recv() {
                self.active = Some(message);
                continue;
            }
            // A terminal's notice follows every frame queued before its gap, and its later
            // output was dropped until the notice leaves.
            if let Some(frames) = self.recovery.next_terminal() {
                self.active = Some(ClientMessage {
                    records: ClientRecords::Replay(frames.into()),
                    lease: std::sync::Arc::new(ClientLease {
                        _bytes: None,
                        _work: None,
                        _replay: None,
                        _recovery: None,
                    }),
                });
                continue;
            }
            // The queued prefix leaves before a cursor asks the journal for its missing suffix.
            if let Some(request) = self.recovery.next() {
                return Ok(Some(Next::Recover(Box::new(request))));
            }
            if self.replies.is_closed() && self.events.is_closed() && self.backfills.is_closed() {
                return Ok(None);
            }
            let deadline = self.recovery.deadline();
            tokio::select! {
                biased;
                Some(mut response) = self.replies.recv() => return Ok(response.pop(&self.replay_bytes).await?.map(Next::Write)),
                Some(batch) = self.backfills.recv() => self.active = Some(batch),
                Some(message) = self.events.recv() => self.active = Some(message),
                _ = self.recovery.changed.notified() => {},
                _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {},
                else => return Ok(None),
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct ClientOutput {
    sender: mpsc::Sender<ClientMessage>,
    replies: mpsc::Sender<ClientMessage>,
    bytes: std::sync::Arc<Semaphore>,
    reply_bytes: std::sync::Arc<Semaphore>,
    waiting_bytes: std::sync::Arc<Semaphore>,
    backfills: mpsc::Sender<ClientMessage>,
    recovery: std::sync::Arc<recovery::Recovery>,
    stop: watch::Sender<()>,
}
impl ClientOutput {
    pub(super) fn channel(stop: watch::Sender<()>) -> (Self, ClientMessages) {
        let (sender, events) = mpsc::channel(CLIENT_QUEUE);
        let (replies, responses) = mpsc::channel(MAX_PENDING);
        let (backfills, recovered) = mpsc::channel(MAX_PENDING);
        let recovery = std::sync::Arc::new(recovery::Recovery::default());
        (
            Self {
                sender,
                replies,
                bytes: std::sync::Arc::new(Semaphore::new(MAX_FRAME)),
                reply_bytes: std::sync::Arc::new(Semaphore::new(REPLY_BYTES)),
                waiting_bytes: std::sync::Arc::new(Semaphore::new(WAITING_REPLY_BYTES)),
                backfills,
                recovery: recovery.clone(),
                stop,
            },
            ClientMessages {
                replies: responses,
                backfills: recovered,
                recovery,
                events,
                active: None,
                replay_bytes: std::sync::Arc::new(Semaphore::new(MAX_FRAME)),
            },
        )
    }

    pub(super) fn send_event(&self, frame: &Frame, record: &str) -> Result<(), ()> {
        if let Some(stream) = frame.stream.as_deref()
            && stream.starts_with("term:")
            && let Some(terminal) = frame
                .params
                .as_ref()
                .and_then(|params| params["terminal_id"].as_str())
        {
            // Terminal bytes cannot be replayed; a reader that falls behind loses a stretch and
            // is told so in that terminal, instead of losing every stream with its connection.
            if self.recovery.terminal_gapped(terminal, frame)
                || self.enqueue(record.to_owned(), None).is_ok()
            {
                return Ok(());
            }
            return self.stopping(self.recovery.terminal_lagged(terminal, frame));
        }
        if let Some((stream, seq)) = frame.stream.as_deref().zip(frame.seq) {
            if self.recovery.follows(stream, seq) {
                return Ok(());
            }
            if self.enqueue(record.to_owned(), None).is_ok() {
                return Ok(());
            }
            // Journal cursors replace queued copies while this reader catches up. Other
            // streams and RPC receipts keep their own bounded lanes and continue immediately.
            return self.stopping(self.recovery.lagged(stream, seq));
        }
        self.try_send(record.to_owned())
    }

    pub(super) fn is_recovery(&self, frame: &Frame) -> bool {
        self.recovery.contains(frame.id.as_ref())
    }

    pub(super) fn recover(
        &self,
        frame: &Frame,
        replay: Option<crate::rc::outbound::ReplayBatch>,
        work: Work,
    ) -> Result<(), ()> {
        let Ok(bytes) = self.reply_bytes.clone().try_acquire_owned() else {
            // The cursor asks again once replies have drained.
            self.recovery.postpone(frame);
            return Ok(());
        };
        let (mut frames, slot) = replay
            .map(|replay| (replay.frames, Some(replay.slot)))
            .unwrap_or_default();
        let Some(completed) = self.recovery.response(frame, &mut frames) else {
            return Ok(());
        };
        self.backfills
            .try_send(ClientMessage {
                records: ClientRecords::Replay(frames.into()),
                lease: std::sync::Arc::new(ClientLease {
                    _bytes: Some(bytes),
                    _work: Some(work),
                    _replay: slot,
                    _recovery: Some(completed),
                }),
            })
            .map_err(|_| ())
    }

    /// Queues a reply without waiting in the caller. A reply that finds the budget held waits in
    /// its own task while the reader drains; the client stops only when it does not.
    pub(super) fn send_work(&self, record: String, work: Work) -> Result<(), ()> {
        let count = u32::try_from(record.len().max(1)).map_err(|_| ())?;
        if let Ok(permit) = self.reply_bytes.clone().try_acquire_many_owned(count)
            && let Ok(slot) = self.replies.try_reserve()
        {
            slot.send(ClientMessage::single(record, permit, Some(work)));
            return Ok(());
        }
        self.wait_for_reply_budget(record, count, work)
    }

    fn wait_for_reply_budget(&self, record: String, count: u32, work: Work) -> Result<(), ()> {
        let Ok(waiting) = self
            .waiting_bytes
            .clone()
            .try_acquire_many_owned(count.min(WAITING_REPLY_BYTES as u32))
        else {
            return self.stopping(Err(()));
        };
        let (budget, replies, stop) = (
            self.reply_bytes.clone(),
            self.replies.clone(),
            self.stop.clone(),
        );
        tokio::spawn(async move {
            let _waiting = waiting;
            let delivered = tokio::time::timeout(REPLY_WAIT, async {
                let permit = budget.acquire_many_owned(count).await.ok()?;
                let slot = replies.reserve().await.ok()?;
                slot.send(ClientMessage::single(record, permit, Some(work)));
                Some(())
            });
            tokio::select! {
                delivered = delivered => if !matches!(delivered, Ok(Some(()))) {
                    let _ = stop.send(());
                },
                () = replies.closed() => {},
            }
        });
        Ok(())
    }

    /// Notifications without a cursor have no recovery path; a full lane stops the client.
    pub(super) fn try_send(&self, record: String) -> Result<(), ()> {
        let result = self.enqueue(record, None).map_err(|_| ());
        self.stopping(result)
    }

    /// Waits in the caller for budget. Only a client's own task may use it.
    pub(super) async fn send_waiting(&self, record: String, timeout: Duration) -> Result<(), ()> {
        tokio::time::timeout(timeout, async {
            let count = u32::try_from(record.len().max(1)).map_err(|_| ())?;
            let permit = self
                .bytes
                .clone()
                .acquire_many_owned(count)
                .await
                .map_err(|_| ())?;
            self.sender
                .send(ClientMessage::single(record, permit, None))
                .await
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())?
    }

    fn stopping(&self, result: Result<(), ()>) -> Result<(), ()> {
        if result.is_err() {
            let _ = self.stop.send(());
        }
        result
    }

    pub(super) fn send_replay(
        &self,
        response: String,
        replay: crate::rc::outbound::ReplayBatch,
        work: Work,
    ) -> Result<(), Work> {
        let Ok(count) = u32::try_from(response.len().max(1)) else {
            return Err(work);
        };
        let Ok(permit) = self.reply_bytes.clone().try_acquire_many_owned(count) else {
            return Err(work);
        };
        let Ok(response_slot) = self.replies.try_reserve() else {
            return Err(work);
        };
        let Ok(replay_slot) = self.sender.try_reserve() else {
            return Err(work);
        };
        let lease = std::sync::Arc::new(ClientLease {
            _bytes: Some(permit),
            _work: Some(work),
            _replay: Some(replay.slot),
            _recovery: None,
        });
        // Reserve both lanes before publishing either; later live frames follow the batch.
        response_slot.send(ClientMessage {
            records: ClientRecords::Single(Some(response)),
            lease: lease.clone(),
        });
        replay_slot.send(ClientMessage {
            records: ClientRecords::Replay(replay.frames.into()),
            lease,
        });
        Ok(())
    }

    fn enqueue(&self, record: String, work: Option<Work>) -> Result<(), String> {
        let Ok(count) = u32::try_from(record.len().max(1)) else {
            return Err(record);
        };
        let Ok(permit) = self.bytes.clone().try_acquire_many_owned(count) else {
            return Err(record);
        };
        self.sender
            .try_send(ClientMessage::single(record, permit, work))
            .map_err(|error| match error.into_inner().records {
                ClientRecords::Single(Some(record)) => record,
                _ => unreachable!("an event carries one record"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream: &str, seq: u64, method: &str) -> Frame {
        let mut frame = Frame::notification(method, serde_json::json!({"terminal_id":"pty"}));
        frame.stream = Some(stream.into());
        frame.seq = Some(seq);
        frame
    }

    /// A reader that falls behind keeps its connection: a reply larger than the free budget
    /// waits for it, a terminal reports its lost stretch before later output, and a session
    /// stream asks its journal for the suffix. Closing on any of these would drop every other
    /// stream and request the reader holds.
    #[tokio::test]
    async fn a_lagging_reader_degrades_per_stream_without_stopping() {
        let (stop, stopped) = watch::channel(());
        let (output, mut messages) = ClientOutput::channel(stop);
        let large = format!("\"{}\"", "x".repeat(MAX_FRAME - 1024));
        for _ in 0..3 {
            output
                .send_work(
                    large.clone(),
                    super::super::Admission::default().enter().unwrap(),
                )
                .unwrap();
        }
        for seq in 1..=CLIENT_QUEUE as u64 + 2 {
            output
                .send_event(&frame("term:workspace", seq, "terminal.output"), "{}")
                .unwrap();
        }
        output
            .send_event(&frame("term:workspace", 1000, "terminal.exited"), "{}")
            .unwrap();
        let session = frame("session", 7, "item.delta");
        output.send_event(&session, &session.to_json()).unwrap();
        let (mut replies, mut frames, mut recovery) = (0, vec![], None);
        while replies < 3 || recovery.is_none() {
            match tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                Next::Write(write) if write.record.len() == large.len() => replies += 1,
                Next::Write(write) => {
                    frames.push(serde_json::from_str::<Frame>(&write.record).ok())
                }
                Next::Recover(request) => recovery = Some(request),
            }
        }
        let recovery = recovery.unwrap();
        assert_eq!(replies, 3);
        assert_eq!(frames.len(), CLIENT_QUEUE + 2);
        let notice = frames[CLIENT_QUEUE].clone().unwrap();
        assert_eq!(notice.method(), "terminal.output");
        assert!(
            notice.params.unwrap()["data"]
                .as_str()
                .unwrap()
                .contains("[agit]")
        );
        assert_eq!(
            frames[CLIENT_QUEUE + 1].as_ref().unwrap().method(),
            "terminal.exited"
        );
        assert_eq!(recovery.params.unwrap()["after_seq"], 6);
        assert!(!stopped.has_changed().unwrap());
    }
}
