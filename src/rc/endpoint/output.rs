//! Client writers pace replay independently while RPC receipts retain priority.

use super::{CLIENT_QUEUE, MAX_FRAME, MAX_PENDING, Work};
use crate::protocol::Frame;
use anyhow::ensure;
use tokio::sync::mpsc;

mod recovery;

struct ClientLease {
    _bytes: tokio::sync::OwnedSemaphorePermit,
    _work: Option<Work>,
    _replay: Option<tokio::sync::OwnedSemaphorePermit>,
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

pub(super) enum Next {
    Write(ClientWrite),
    Recover(Box<Frame>),
}

pub(super) struct ClientWrite {
    pub(super) record: String,
    _lease: std::sync::Arc<ClientLease>,
    _replay_bytes: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ClientMessage {
    async fn pop(
        &mut self,
        bytes: &std::sync::Arc<tokio::sync::Semaphore>,
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
    replay_bytes: std::sync::Arc<tokio::sync::Semaphore>,
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
    bytes: std::sync::Arc<tokio::sync::Semaphore>,
    reply_bytes: std::sync::Arc<tokio::sync::Semaphore>,
    backfills: mpsc::Sender<ClientMessage>,
    recovery: std::sync::Arc<recovery::Recovery>,
    stop: Option<tokio::sync::watch::Sender<()>>,
}
impl ClientOutput {
    pub(super) fn channel(stop: Option<tokio::sync::watch::Sender<()>>) -> (Self, ClientMessages) {
        let (sender, events) = mpsc::channel(CLIENT_QUEUE);
        let (replies, responses) = mpsc::channel(MAX_PENDING);
        let (backfills, recovered) = mpsc::channel(MAX_PENDING);
        let recovery = std::sync::Arc::new(recovery::Recovery::default());
        (
            Self {
                sender,
                replies,
                bytes: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_FRAME)),
                reply_bytes: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_FRAME)),
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
                replay_bytes: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_FRAME)),
            },
        )
    }

    fn budget(&self, reply: bool) -> &std::sync::Arc<tokio::sync::Semaphore> {
        if reply {
            &self.reply_bytes
        } else {
            &self.bytes
        }
    }

    pub(super) async fn send_event(&self, frame: &Frame, record: &str) -> Result<(), ()> {
        if self.stop.is_none() {
            return self
                .send_timeout(record.to_owned(), std::time::Duration::from_secs(2))
                .await;
        }
        let recoverable = frame
            .stream
            .as_deref()
            .zip(frame.seq)
            .filter(|(stream, _)| !stream.starts_with("term:") && !frame.reliable);
        if let Some((stream, seq)) = recoverable {
            if self.recovery.follows(stream, seq) {
                return Ok(());
            }
            if self.enqueue(record.to_owned(), None).is_ok() {
                return Ok(());
            }
            // Journal cursors replace queued copies while this reader catches up. Other
            // streams and RPC receipts keep their own bounded lanes and continue immediately.
            return self.recovery.lagged(stream, seq);
        }
        self.try_send_inner(record.to_owned(), None)
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
                    _bytes: self
                        .reply_bytes
                        .clone()
                        .try_acquire_owned()
                        .map_err(|_| ())?,
                    _work: Some(work),
                    _replay: slot,
                    _recovery: Some(completed),
                }),
            })
            .map_err(|_| ())
    }

    pub(super) async fn send_timeout(
        &self,
        record: String,
        timeout: std::time::Duration,
    ) -> Result<(), ()> {
        self.send_inner(record, timeout, None).await
    }
    pub(super) async fn send_work(
        &self,
        record: String,
        timeout: std::time::Duration,
        work: Work,
    ) -> Result<(), ()> {
        self.send_inner(record, timeout, Some(work)).await
    }
    async fn send_inner(
        &self,
        record: String,
        timeout: std::time::Duration,
        work: Option<Work>,
    ) -> Result<(), ()> {
        // A remote reader cannot stall owner RPC or the shared executor fanout.
        if self.stop.is_some() {
            return self.try_send_inner(record, work);
        }
        tokio::time::timeout(timeout, async {
            let count = u32::try_from(record.len().max(1)).map_err(|_| ())?;
            let permit = self
                .budget(work.is_some())
                .clone()
                .acquire_many_owned(count)
                .await
                .map_err(|_| ())?;
            let sender = if work.is_some() {
                &self.replies
            } else {
                &self.sender
            };
            sender
                .send(ClientMessage {
                    records: ClientRecords::Single(Some(record)),
                    lease: std::sync::Arc::new(ClientLease {
                        _bytes: permit,
                        _work: work,
                        _replay: None,
                        _recovery: None,
                    }),
                })
                .await
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())?
    }
    #[cfg(test)]
    pub(super) fn try_send(&self, record: String) -> Result<(), ()> {
        self.try_send_inner(record, None)
    }
    pub(super) fn try_send_work(&self, record: String, work: Work) -> Result<(), ()> {
        self.try_send_inner(record, Some(work))
    }
    fn try_send_inner(&self, record: String, work: Option<Work>) -> Result<(), ()> {
        let result = self.enqueue(record, work);
        if result.is_err()
            && let Some(stop) = &self.stop
        {
            let _ = stop.send(());
        }
        result
    }

    pub(super) fn send_replay(
        &self,
        response: String,
        replay: crate::rc::outbound::ReplayBatch,
        work: Work,
    ) -> Result<(), ()> {
        let count = u32::try_from(response.len().max(1)).map_err(|_| ())?;
        let permit = self
            .reply_bytes
            .clone()
            .try_acquire_many_owned(count)
            .map_err(|_| ())?;
        let response_slot = self.replies.try_reserve().map_err(|_| ())?;
        let replay_slot = self.sender.try_reserve().map_err(|_| ())?;
        let lease = std::sync::Arc::new(ClientLease {
            _bytes: permit,
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

    fn enqueue(&self, record: String, work: Option<Work>) -> Result<(), ()> {
        let count = u32::try_from(record.len().max(1)).map_err(|_| ())?;
        let permit = self
            .budget(work.is_some())
            .clone()
            .try_acquire_many_owned(count)
            .map_err(|_| ())?;
        let sender = if work.is_some() {
            &self.replies
        } else {
            &self.sender
        };
        sender
            .try_send(ClientMessage {
                records: ClientRecords::Single(Some(record)),
                lease: std::sync::Arc::new(ClientLease {
                    _bytes: permit,
                    _work: work,
                    _replay: None,
                    _recovery: None,
                }),
            })
            .map_err(|_| ())
    }
}
