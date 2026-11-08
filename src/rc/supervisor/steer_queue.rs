//! Messages for a running native Claude Code turn: an owner's waits in agit's plugin for that
//! turn's next tool step, and whatever can't reach the running turn waits here for the next one.
//!
//! The plugin never submits a message itself. What a turn ends without comes back here by id, and
//! a message that waits for the next turn goes only when no turn runs, through the same Turn path
//! as any other, after the session's gates are asked again with what holds then: the request's
//! authority, Default or Plan only for a non-owner, and the owner's home folder. A message
//! admitted when it arrived is not admitted when it goes on that strength alone: the mode or the
//! sender's access can change while a turn runs.

use super::*;
use crate::rc::step_delivery;
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;

/// A message that waits for a turn: the words as sent, whose they are, and the authority of the
/// request that brought them.
pub(super) struct QueuedTurn {
    message: String,
    attribution: MessageAttribution,
    authority: crate::rc::authority::Guard,
    owner: bool,
}

/// An owner's message the plugin holds for the running turn's next tool step: the conversation
/// and id its delivery record is kept under.
struct StepSteer {
    queued: QueuedTurn,
    conversation: String,
    record: String,
}

/// How many messages may wait for the next turn; one more is refused at once.
const MAX_WAITING: usize = 16;

/// How a queued message's turn start came out, as its receipt says.
type Outcome = Result<crate::Result<TurnStartOutcome>, tokio::sync::oneshot::error::RecvError>;
/// A queued message's client message id, with how its turn start came out.
type Started = (Option<String>, Outcome);

pub(super) struct SteerQueue {
    /// Held by the plugin, by operation id.
    held: HashMap<String, StepSteer>,
    /// The held message whose receipt `pending_steer_command` awaits.
    receipt: Option<String>,
    /// Waiting for the next turn, in the order they arrived.
    waiting: VecDeque<QueuedTurn>,
    /// Turn starts of queued messages as their receipts come back, by client message id.
    pub(super) outcomes: (
        mpsc::UnboundedSender<Started>,
        mpsc::UnboundedReceiver<Started>,
    ),
}

impl Default for SteerQueue {
    fn default() -> Self {
        Self {
            held: HashMap::new(),
            receipt: None,
            waiting: VecDeque::new(),
            outcomes: mpsc::unbounded_channel(),
        }
    }
}

impl Session {
    /// A steer into a running turn on a terminal whose plugin can hold it for the next tool step.
    /// An owner's waits there; anyone else's starts the next turn.
    pub(super) async fn steer_at_step(&mut self, message: String, pending: PendingSteerCommand) {
        let queued = QueuedTurn {
            message,
            attribution: pending.attribution.clone(),
            authority: pending.reply.authority(),
            owner: pending.reply.owner_control(),
        };
        let framed = step_delivery::framed(&queued.message);
        // The record comes first, so it exists before the plugin can hand the text over. Without
        // one, the words would reach the model and never count as the owner's: such a message
        // starts the next turn instead.
        let conversation = self.driver.runtime_thread_id();
        let record = queued
            .attribution
            .client_msg_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let recorded = queued.owner
            && conversation.as_deref().is_some_and(|conversation| {
                step_delivery::record(conversation, &record, &framed).is_ok()
            });
        let Some(conversation) = conversation.filter(|_| recorded) else {
            if self.steer_queue.waiting.len() >= MAX_WAITING {
                self.resolve_steer(
                    pending,
                    Err(anyhow::anyhow!(
                        "too many messages already wait for this turn to end"
                    )),
                )
                .await;
                return;
            }
            self.steer_queue.waiting.push_back(queued);
            self.resolve_steer(pending, Ok(Delivery::WhenIdle)).await;
            return;
        };
        let (dispatch, operation) = self.driver.steer_at_step(&framed);
        match operation {
            Some(operation) => {
                self.steer_queue.receipt = Some(operation.clone());
                self.steer_queue.held.insert(
                    operation,
                    StepSteer {
                        queued,
                        conversation,
                        record,
                    },
                );
            }
            None => forget_record(&conversation, &record),
        }
        match dispatch {
            SteerDispatch::Resolved(result) => self.resolve_steer(pending, result).await,
            SteerDispatch::Awaiting => self.pending_steer_command = Some(pending),
        }
    }

    /// The plugin's receipt for a held message came back: one it didn't take isn't held.
    pub(super) fn step_steer_resolved(&mut self, accepted: bool) {
        if let Some(operation) = self.steer_queue.receipt.take()
            && !accepted
            && let Some(steer) = self.steer_queue.held.remove(&operation)
        {
            forget_record(&steer.conversation, &steer.record);
        }
    }

    /// The plugin handed a held message to the model; its record, already written, is what lets
    /// that step's context count as the owner's words.
    pub(super) fn step_steer_delivered(&mut self, operation_id: String) {
        self.steer_queue.held.remove(&operation_id);
    }

    /// A held message its turn ended without: its record goes, and it waits for the next turn,
    /// unless the turn was stopped, which drops it.
    pub(super) async fn step_steer_undelivered(&mut self, operation_id: String, interrupted: bool) {
        let Some(steer) = self.steer_queue.held.remove(&operation_id) else {
            return;
        };
        forget_record(&steer.conversation, &steer.record);
        if interrupted {
            self.report_steer_dropped(&steer.queued.attribution, "interrupted")
                .await;
        } else {
            self.steer_queue.waiting.push_back(steer.queued);
        }
    }

    /// The session ends. What waits here for the next turn goes nowhere now, so each sender is
    /// told. A message the plugin holds is the terminal's: the terminal runs on without this
    /// session and may still hand it over at its next tool step, or may already have done so
    /// unreported. Its record stays, so a hand-over still counts as the owner's words, and its
    /// sender is told nothing the device can't confirm. A record whose message never goes vouches
    /// only for that exact text, which nothing else writes.
    pub(super) async fn abandon_steers(&mut self) {
        self.steer_queue.receipt = None;
        self.steer_queue.held.clear();
        let waiting: Vec<QueuedTurn> = self.steer_queue.waiting.drain(..).collect();
        for queued in waiting {
            self.report_steer_dropped(&queued.attribution, "session_ended")
                .await;
        }
    }

    /// The next waiting message as a Turn command, once no turn runs or waits to start; the ones
    /// the session's gates refuse now are dropped and their senders told.
    pub(super) async fn next_queued_turn(&mut self) -> Option<Command> {
        loop {
            if self.steer_queue.waiting.is_empty()
                || self.turn_running()
                || self.driver.has_active_turn()
                || self.info.status == SessionStatus::Ended
                || self.stopping_turn.is_some()
                || self.pending_turn_command.is_some()
                || self.pending_steer_command.is_some()
            {
                return None;
            }
            let queued = self.steer_queue.waiting.pop_front()?;
            if let Some(reason) = self.refused_at_delivery(&queued) {
                self.report_steer_dropped(&queued.attribution, reason).await;
                continue;
            }
            let (ticket, mut receipt) = crate::rc::ticket::ticket_authorized(queued.authority);
            let client_msg_id = queued.attribution.client_msg_id.clone();
            let outcomes = self.steer_queue.outcomes.0.clone();
            tokio::spawn(async move {
                let outcome = receipt.wait_until_closed().await;
                let _ = outcomes.send((client_msg_id, outcome));
            });
            return Some(Command::Turn {
                message: queued.message,
                attribution: queued.attribution,
                guard_attempt: None,
                reply: ticket.with_control_ceiling(queued.owner),
            });
        }
    }

    /// Why the session's gates refuse `queued` now, read as they stand when it goes.
    fn refused_at_delivery(&self, queued: &QueuedTurn) -> Option<&'static str> {
        if queued.authority.check().is_err() {
            return Some("access_ended");
        }
        if crate::rc::turn_mode::require(queued.owner, self.driver.turn_mode(true)).is_err() {
            return Some("mode_owner_only");
        }
        if !queued.owner
            && crate::rc::policy::home_folder_only(&self.cwd, &self.confinement.borrow().roots)
        {
            return Some("home_folder");
        }
        None
    }

    /// A queued message's turn start came back: one that didn't start is reported dropped.
    pub(super) async fn queued_turn_outcome(
        &mut self,
        client_msg_id: Option<String>,
        outcome: Outcome,
    ) {
        let reason = match outcome {
            Ok(Ok(
                TurnStartOutcome::NativeSubmission { .. } | TurnStartOutcome::Accepted { .. },
            )) => {
                return;
            }
            Ok(Ok(TurnStartOutcome::ModeOwnerOnly { .. })) => "mode_owner_only",
            Ok(_) => "not_sent",
            // Never taken: the request's authority no longer admitted it.
            Err(_) => "access_ended",
        };
        self.report_dropped(client_msg_id.as_deref(), reason).await;
    }

    async fn report_steer_dropped(&self, attribution: &MessageAttribution, reason: &str) {
        self.report_dropped(attribution.client_msg_id.as_deref(), reason)
            .await;
    }

    /// Tells the sender a message the device took won't reach a turn: ids and a code only.
    async fn report_dropped(&self, client_msg_id: Option<&str>, reason: &str) {
        if let Some(client_msg_id) = client_msg_id {
            self.emit(
                method::TURN_STEER_DROPPED,
                crate::protocol::TurnSteerDropped {
                    client_msg_id: client_msg_id.to_owned(),
                    reason: reason.into(),
                },
            )
            .await;
        }
    }
}

fn forget_record(conversation: &str, record: &str) {
    if step_delivery::forget_one(conversation, record).is_err() {
        tracing_note(
            "a delivery record couldn't be removed; it vouches only for the exact words sent",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PermissionMode as M;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Revocable(Arc<AtomicBool>);
    impl crate::rc::authority::Authority for Revocable {
        fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool {
            self.0.load(Ordering::SeqCst) && accept()
        }
    }

    fn queued(message: &str, owner: bool, authority: crate::rc::authority::Guard) -> QueuedTurn {
        QueuedTurn {
            message: message.into(),
            attribution: MessageAttribution {
                client_msg_id: Some(message.into()),
                ..Default::default()
            },
            authority,
            owner,
        }
    }

    fn dropped(out: &mut mpsc::Receiver<Frame>) -> Vec<(String, String)> {
        std::iter::from_fn(|| out.try_recv().ok())
            .filter(|frame| frame.method() == method::TURN_STEER_DROPPED)
            .map(|frame| {
                let notice: crate::protocol::TurnSteerDropped = frame.params_as().unwrap();
                (notice.client_msg_id, notice.reason)
            })
            .collect()
    }

    fn turn_message(command: Option<Command>) -> String {
        match command {
            Some(Command::Turn { message, .. }) => message,
            _ => panic!("expected a Turn command"),
        }
    }

    /// When the session ends, a message the plugin holds keeps its delivery record and its sender
    /// is told nothing, while one waiting here for the next turn is reported dropped. The
    /// terminal runs on without the session and may still hand the held message over, or may have
    /// done so unreported: reporting it dropped and removing its record would tell the sender it
    /// was not sent while the model reads it, and its words would then be withheld as unvouched
    /// context.
    #[tokio::test]
    async fn a_session_end_keeps_what_the_terminal_holds_and_drops_what_waits_here() {
        let home = tempfile::tempdir().unwrap();
        let conversation = "4e6c2b1a-0d0e-4c43-9a51-8f0e2f4a7b10";
        let framed = step_delivery::framed("held words");
        crate::rc::with_agit_home(home.path(), || {
            step_delivery::record(conversation, "held", &framed).unwrap();
        });
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (mut session, mut out, _notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Running,
            );
        session.steer_queue.held.insert(
            "operation".into(),
            StepSteer {
                queued: queued("held", true, Default::default()),
                conversation: conversation.into(),
                record: "held".into(),
            },
        );
        session
            .steer_queue
            .waiting
            .push_back(queued("waiting", false, Default::default()));
        session.abandon_steers().await;
        assert_eq!(
            dropped(&mut out),
            [("waiting".to_owned(), "session_ended".to_owned())]
        );
        assert!(session.steer_queue.held.is_empty());
        crate::rc::with_agit_home(home.path(), || {
            let mut deliveries = step_delivery::Deliveries::load(conversation);
            assert_eq!(deliveries.take(&framed), Some("held words"));
        });
        session.driver.shutdown().await.unwrap();
    }

    /// A message that waited for the next turn is admitted by the session's gates as they stand
    /// when it goes, not as they stood when it came: a non-owner's is dropped, and its sender
    /// told, when the sender's access ended meanwhile, when the session works in the owner's home
    /// folder, or when the next turn runs in a mode only the owner may use (here Auto, set after
    /// the message was taken); the owner's goes on as a Turn command. A queue that kept the
    /// answer a message got on arrival would carry a non-owner's words into an Auto turn.
    #[tokio::test]
    async fn a_waiting_message_is_admitted_by_the_gates_as_they_stand_when_it_goes() {
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (mut session, mut out, _notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Idle,
            );
        let home = ["HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(std::env::var_os)
            .find_map(|home| std::fs::canonicalize(home).ok())
            .expect("a home directory");
        let roots = |root: PathBuf| crate::rc::Confinement {
            roots: crate::rc::policy::CanonicalRoots::from_untrusted([root]),
            ..Default::default()
        };

        // The workspace reaches the session's folder only through the home directory.
        session.cwd = home.clone();
        session.confinement = tokio::sync::watch::channel(roots(home.clone())).1;
        let access = Arc::new(AtomicBool::new(true));
        let revocable = crate::rc::authority::Guard::new(Revocable(access.clone()));
        for message in [
            queued("revoked", false, revocable),
            queued("home", false, Default::default()),
            queued("owner", true, Default::default()),
        ] {
            session.steer_queue.waiting.push_back(message);
        }
        access.store(false, Ordering::SeqCst);
        assert_eq!(turn_message(session.next_queued_turn().await), "owner");
        assert_eq!(
            dropped(&mut out),
            [
                ("revoked".to_owned(), "access_ended".to_owned()),
                ("home".to_owned(), "home_folder".to_owned()),
            ]
        );

        // The next turn's mode changes to Auto after the message was taken.
        let project = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(project.path()).unwrap();
        session.cwd = project.clone();
        session.confinement = tokio::sync::watch::channel(roots(project)).1;
        session
            .steer_queue
            .waiting
            .push_back(queued("auto", false, Default::default()));
        assert!(matches!(
            session.driver.set_permission_mode(M::Auto).await,
            Ok(crate::protocol::PermissionApply::NextTurn)
        ));
        assert!(session.next_queued_turn().await.is_none());
        assert_eq!(
            dropped(&mut out),
            [("auto".to_owned(), "mode_owner_only".to_owned())]
        );
        session.driver.shutdown().await.unwrap();
    }
}
