//! A congested reader catches up from the session journal without reopening its transport.

use crate::protocol::{Frame, RequestId};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

/// A cursor costs a few words, so a reader may lag on every stream it can see.
const MAX_LAGGED_STREAMS: usize = 1024;
/// Recovery requests share the client's request admission with its own RPCs.
const MAX_RECOVERY_REQUESTS: usize = 8;
const MAX_TERMINAL_GAPS: usize = 256;
const TERMINAL_GAP_NOTICE: &str =
    "\r\n[agit] this viewer fell behind; a stretch of terminal output could not be delivered.\r\n";

#[derive(Default)]
pub(super) struct Recovery {
    streams: Mutex<BTreeMap<String, Cursor>>,
    terminals: Mutex<BTreeMap<String, TerminalGap>>,
    pub changed: Notify,
}

/// A terminal whose output was dropped; later output is dropped until its notice leaves.
struct TerminalGap {
    notice: Frame,
    exited: Option<Frame>,
}

struct Cursor {
    after: u64,
    latest: u64,
    request: Option<RequestId>,
    retry: Instant,
}

pub(super) struct Completed {
    recovery: Arc<Recovery>,
    stream: String,
    through: u64,
}

impl Drop for Completed {
    fn drop(&mut self) {
        let mut streams = self.recovery.streams.lock().unwrap();
        if let Some(cursor) = streams.get_mut(&self.stream) {
            cursor.after = cursor.after.max(self.through);
            cursor.request = None;
            if cursor.after >= cursor.latest {
                streams.remove(&self.stream);
            }
        }
        self.recovery.changed.notify_one();
    }
}

impl Recovery {
    pub fn follows(&self, stream: &str, seq: u64) -> bool {
        let mut streams = self.streams.lock().unwrap();
        let Some(cursor) = streams.get_mut(stream) else {
            return false;
        };
        cursor.latest = cursor.latest.max(seq);
        true
    }

    pub fn lagged(&self, stream: &str, seq: u64) -> Result<(), ()> {
        let mut streams = self.streams.lock().unwrap();
        if streams.len() >= MAX_LAGGED_STREAMS {
            return Err(());
        }
        streams.insert(
            stream.into(),
            Cursor {
                after: seq.saturating_sub(1),
                latest: seq,
                request: None,
                retry: Instant::now(),
            },
        );
        self.changed.notify_one();
        Ok(())
    }

    pub fn next(&self) -> Option<Frame> {
        let mut streams = self.streams.lock().unwrap();
        if outstanding(&streams) >= MAX_RECOVERY_REQUESTS {
            return None;
        }
        let (stream, cursor) = streams
            .iter_mut()
            .find(|(_, cursor)| cursor.request.is_none() && cursor.retry <= Instant::now())?;
        let request = Frame::request(
            "session.subscribe",
            serde_json::json!({
                "workspace_id":super::super::WORKSPACE, "session_id":stream, "after_seq":cursor.after,
            }),
        );
        cursor.request = request.id.clone();
        Some(request)
    }

    pub fn deadline(&self) -> Option<Instant> {
        let streams = self.streams.lock().unwrap();
        // A completed request wakes the reader, so a full request window needs no timer.
        if outstanding(&streams) >= MAX_RECOVERY_REQUESTS {
            return None;
        }
        streams
            .values()
            .filter(|cursor| cursor.request.is_none())
            .map(|cursor| cursor.retry)
            .min()
    }

    /// A recovery whose response cannot be queued yet asks the journal again later.
    pub fn postpone(&self, frame: &Frame) {
        let mut streams = self.streams.lock().unwrap();
        if let Some(cursor) = streams
            .values_mut()
            .find(|cursor| cursor.request.is_some() && cursor.request == frame.id)
        {
            cursor.request = None;
            cursor.retry = Instant::now() + Duration::from_secs(1);
        }
        self.changed.notify_one();
    }

    /// True when the frame belongs to a terminal whose gap notice is still queued. Its output is
    /// dropped; its exit is kept to follow the notice.
    pub fn terminal_gapped(&self, terminal: &str, frame: &Frame) -> bool {
        let mut terminals = self.terminals.lock().unwrap();
        let Some(gap) = terminals.get_mut(terminal) else {
            return false;
        };
        if frame.method() == crate::protocol::method::TERMINAL_EXITED {
            gap.exited = Some(frame.clone());
        }
        true
    }

    pub fn terminal_lagged(&self, terminal: &str, frame: &Frame) -> Result<(), ()> {
        let mut terminals = self.terminals.lock().unwrap();
        if terminals.len() >= MAX_TERMINAL_GAPS {
            return Err(());
        }
        let mut notice = Frame::notification(
            crate::protocol::method::TERMINAL_OUTPUT,
            serde_json::json!({"terminal_id":terminal, "data":TERMINAL_GAP_NOTICE}),
        );
        notice.stream = frame.stream.clone();
        notice.seq = frame.seq;
        let exited =
            (frame.method() == crate::protocol::method::TERMINAL_EXITED).then(|| frame.clone());
        terminals.insert(terminal.into(), TerminalGap { notice, exited });
        self.changed.notify_one();
        Ok(())
    }

    pub fn next_terminal(&self) -> Option<Vec<Frame>> {
        let mut terminals = self.terminals.lock().unwrap();
        let terminal = terminals.keys().next()?.clone();
        let gap = terminals.remove(&terminal)?;
        Some(std::iter::once(gap.notice).chain(gap.exited).collect())
    }

    pub fn contains(&self, id: Option<&RequestId>) -> bool {
        id.is_some_and(|id| {
            self.streams
                .lock()
                .unwrap()
                .values()
                .any(|cursor| cursor.request.as_ref() == Some(id))
        })
    }

    pub fn response(self: &Arc<Self>, frame: &Frame, frames: &mut Vec<Frame>) -> Option<Completed> {
        let mut streams = self.streams.lock().unwrap();
        let (stream, cursor) = streams
            .iter_mut()
            .find(|(_, cursor)| cursor.request == frame.id)?;
        let Some(result) = frame.result.as_ref() else {
            if frame.error.as_ref().is_some_and(|error| {
                error.is(crate::protocol::ErrorCode::SessionBusy)
                    || error.is(crate::protocol::ErrorCode::RuntimeUnavailable)
            }) {
                cursor.request = None;
                cursor.retry = Instant::now() + Duration::from_secs(1);
                self.changed.notify_one();
                return None;
            }
            // A removed watch cannot be subscribed again. Preserve the missing range
            // for history reconciliation without retaining an endless retry cursor.
            frames.push(gap(stream, cursor.latest));
            return Some(Completed {
                recovery: self.clone(),
                stream: stream.clone(),
                through: cursor.latest,
            });
        };
        let through = frames
            .iter()
            .filter_map(|frame| frame.seq)
            .max()
            .unwrap_or(cursor.after)
            .max(
                result["session"]["last_seq"]
                    .as_u64()
                    .unwrap_or(cursor.after),
            );
        let from = result["from_seq"].as_u64().unwrap_or(0);
        if from > cursor.after.saturating_add(1) {
            frames.insert(0, gap(stream, from - 1));
        }
        Some(Completed {
            recovery: self.clone(),
            stream: stream.clone(),
            through,
        })
    }
}

fn outstanding(streams: &BTreeMap<String, Cursor>) -> usize {
    streams
        .values()
        .filter(|cursor| cursor.request.is_some())
        .count()
}

fn gap(stream: &str, through: u64) -> Frame {
    let mut frame = Frame::notification("session.gap", serde_json::json!({"through_seq":through}));
    frame.stream = Some(stream.to_owned());
    frame.seq = Some(through);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ErrorCode, RpcError};
    use serde_json::json;

    #[tokio::test]
    async fn recovery_keeps_a_cursor_until_delivery_and_reports_evicted_history() {
        let recovery = Arc::new(Recovery::default());
        recovery.lagged("session", 10).unwrap();
        let request = recovery.next().unwrap();
        assert_eq!(request.params.as_ref().unwrap()["after_seq"], 9);
        assert!(recovery.next().is_none());
        let mut frames = Vec::new();
        let busy = Frame::error_response(
            request.id.unwrap(),
            RpcError::new(ErrorCode::SessionBusy, "backfill slots are busy"),
        );
        assert!(recovery.response(&busy, &mut frames).is_none());
        assert!(recovery.next().is_none());
        tokio::time::sleep_until(recovery.deadline().unwrap()).await;
        let request = recovery.next().unwrap();
        assert_eq!(request.params.as_ref().unwrap()["after_seq"], 9);
        assert!(recovery.follows("session", 30));
        let reply = Frame::response(
            request.id.unwrap(),
            json!({"session":{"last_seq":20},"from_seq":21}),
        );
        let delivered = recovery.response(&reply, &mut frames).unwrap();
        assert_eq!(frames[0].method(), "session.gap");
        assert_eq!(frames[0].params.as_ref().unwrap()["through_seq"], 20);
        assert!(recovery.next().is_none());
        drop(delivered);
        let request = recovery.next().unwrap();
        assert_eq!(request.params.as_ref().unwrap()["after_seq"], 20);
        let reply = Frame::response(
            request.id.unwrap(),
            json!({"session":{"last_seq":30},"from_seq":21}),
        );
        drop(recovery.response(&reply, &mut Vec::new()).unwrap());
        assert!(!recovery.follows("session", 31));
        assert!(recovery.deadline().is_none());

        recovery.lagged("removed-watch", 40).unwrap();
        let request = recovery.next().unwrap();
        let reply = Frame::error_response(
            request.id.unwrap(),
            RpcError::new(ErrorCode::SessionNotFound, "watch ended"),
        );
        let mut frames = Vec::new();
        drop(recovery.response(&reply, &mut frames).unwrap());
        assert_eq!(frames[0].params.as_ref().unwrap()["through_seq"], 40);
        assert!(recovery.deadline().is_none());
    }
}
