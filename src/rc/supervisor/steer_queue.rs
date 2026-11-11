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
    /// When it was queued.
    arrived: tokio::time::Instant,
}

/// An owner's message the plugin holds for the running turn's next tool step: the conversation
/// and id its delivery record is kept under, and the turn it was held for.
struct StepSteer {
    queued: QueuedTurn,
    conversation: String,
    record: String,
    turn: Option<String>,
    /// Its place among held messages: what goes back to wait goes in the order it arrived.
    order: u64,
}

/// What the terminal reported of a turn being over.
#[derive(Clone, Copy)]
pub(super) enum TurnOver<'a> {
    /// This turn ended.
    Ended(&'a str),
    /// This turn began, so any other is over.
    Started(&'a str),
}

/// How many messages may wait for the next turn; one more is refused at once.
const MAX_WAITING: usize = 16;

/// How long a non-owner's waiting message stands on what admitted it. The device learns that a
/// sender lost access to a session only through the requests it is sent, so past this, without
/// an admitted request from that account since (a turn, a steer, or a keep for its waiting
/// messages), the message is dropped rather than delivered.
pub(super) const RECONFIRM_WITHIN: std::time::Duration = std::time::Duration::from_secs(120);

/// How a queued message's turn start came out, as its receipt says.
type Outcome = Result<crate::Result<TurnStartOutcome>, tokio::sync::oneshot::error::RecvError>;
/// A queued message's client message id, with how its turn start came out.
type Started = (MessageAttribution, Outcome);

pub(super) struct SteerQueue {
    /// Held by the plugin, by operation id.
    held: HashMap<String, StepSteer>,
    /// The held message whose receipt `pending_steer_command` awaits.
    receipt: Option<String>,
    /// The turn that message was held for is over, and whether it was stopped: once the receipt
    /// says the plugin held it, it went undelivered.
    receipt_turn_over: Option<bool>,
    /// The next held message's place.
    next_order: u64,
    /// Waiting for the next turn, in the order they arrived.
    waiting: VecDeque<QueuedTurn>,
    /// When this session last admitted a request from each account.
    confirmed: HashMap<String, tokio::time::Instant>,
    /// Turn starts of queued messages as their receipts come back, by client message id.
    pub(super) outcomes: (
        mpsc::UnboundedSender<Started>,
        mpsc::UnboundedReceiver<Started>,
    ),
}

impl SteerQueue {
    /// This session just admitted `command`: when it names its sender's account, that account
    /// is still there. A waiting message the session took up itself (`from_queue`) was admitted
    /// when it arrived and says nothing about now, so one waiting message never vouches for
    /// another.
    pub(super) fn admitted(&mut self, command: &Command, from_queue: bool) {
        if from_queue {
            return;
        }
        if let Some(account) = command.sender_account() {
            self.confirmed
                .insert(account.to_owned(), tokio::time::Instant::now());
        }
    }

    /// Whether `queued` may still go on what admitted it: an owner's always; anyone else's
    /// while it is fresh, or while its account keeps being admitted.
    fn still_confirmed(&self, queued: &QueuedTurn) -> bool {
        if queued.owner || queued.arrived.elapsed() <= RECONFIRM_WITHIN {
            return true;
        }
        queued
            .attribution
            .sender
            .as_ref()
            .and_then(|sender| self.confirmed.get(&sender.account_id))
            .is_some_and(|at| at.elapsed() <= RECONFIRM_WITHIN)
    }
}

impl Default for SteerQueue {
    fn default() -> Self {
        Self {
            held: HashMap::new(),
            receipt: None,
            receipt_turn_over: None,
            next_order: 0,
            waiting: VecDeque::new(),
            confirmed: HashMap::new(),
            outcomes: mpsc::unbounded_channel(),
        }
    }
}

impl Session {
    /// A steer into a running turn on a terminal whose plugin can hold it for the next tool step:
    /// it waits there, whoever sent it.
    pub(super) async fn steer_at_step(&mut self, message: String, pending: PendingSteerCommand) {
        let queued = QueuedTurn {
            message,
            attribution: pending.attribution.clone(),
            authority: pending.reply.authority(),
            owner: pending.reply.owner_control(),
            arrived: tokio::time::Instant::now(),
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
        let recorded = conversation.as_deref().is_some_and(|conversation| {
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
        let turn = self.driver.running_native_turn();
        let (dispatch, operation) = self.driver.steer_at_step(&framed);
        match operation {
            Some(operation) => {
                self.steer_queue.receipt = Some(operation.clone());
                self.steer_queue.receipt_turn_over = None;
                let order = self.steer_queue.next_order;
                self.steer_queue.next_order += 1;
                self.steer_queue.held.insert(
                    operation,
                    StepSteer {
                        queued,
                        conversation,
                        record,
                        turn,
                        order,
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

    /// The plugin's receipt for a held message came back: one it didn't take isn't held, and one
    /// it took for a turn this session already saw end went undelivered, as that turn ended.
    /// Whether the turn is still running isn't judged from the terminal's latest state: the
    /// receipt is read ahead of the events that came with it, which may say it was delivered.
    pub(super) async fn step_steer_resolved(&mut self, accepted: bool) {
        let over = self.steer_queue.receipt_turn_over.take();
        let Some(operation) = self.steer_queue.receipt.take() else {
            return;
        };
        if !accepted {
            if let Some(steer) = self.steer_queue.held.remove(&operation) {
                forget_record(&steer.conversation, &steer.record);
            }
            return;
        }
        if let Some(interrupted) = over {
            self.step_steer_undelivered(operation, interrupted).await;
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

    /// The terminal reported a turn over: `ended` finished, or `started` began. The plugin hands
    /// a held message over only in the turn it was held for, and says so before that turn ends,
    /// so whatever is still held for a turn that is over went undelivered, whether or not the
    /// plugin says so. Each goes as an undelivered one does: it waits for the next turn, or, when
    /// `interrupted`, is dropped, in the order they were held. One whose receipt hasn't come back
    /// waits for it, which then decides it as this turn's end would have.
    pub(super) async fn held_turn_over(&mut self, over: TurnOver<'_>, interrupted: bool) {
        let mut stranded: Vec<(u64, String)> = self
            .steer_queue
            .held
            .iter()
            .filter(|(_, steer)| match over {
                TurnOver::Ended(turn) => steer.turn.as_deref() == Some(turn),
                TurnOver::Started(turn) => steer.turn.as_deref() != Some(turn),
            })
            .map(|(operation, steer)| (steer.order, operation.clone()))
            .collect();
        stranded.sort();
        for (_, operation) in stranded {
            if self.steer_queue.receipt.as_ref() == Some(&operation) {
                self.steer_queue.receipt_turn_over = Some(interrupted);
                continue;
            }
            self.step_steer_undelivered(operation, interrupted).await;
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
        self.steer_queue.receipt_turn_over = None;
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
            let attribution = queued.attribution.clone();
            let outcomes = self.steer_queue.outcomes.0.clone();
            tokio::spawn(async move {
                let outcome = receipt.wait_until_closed().await;
                let _ = outcomes.send((attribution, outcome));
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
        if queued.authority.check().is_err() || !self.steer_queue.still_confirmed(queued) {
            return Some("access_ended");
        }
        None
    }

    /// A queued message's turn start came back: one that didn't start is reported dropped.
    pub(super) async fn queued_turn_outcome(
        &mut self,
        attribution: &MessageAttribution,
        outcome: Outcome,
    ) {
        let reason = match outcome {
            Ok(Ok(
                TurnStartOutcome::NativeSubmission { .. } | TurnStartOutcome::Accepted { .. },
            )) => {
                return;
            }
            Ok(_) => "not_sent",
            // Never taken: the request's authority no longer admitted it.
            Err(_) => "access_ended",
        };
        self.report_steer_dropped(attribution, reason).await;
    }

    /// Tells the sender a message the device took won't reach a turn: ids, who sent it, and a
    /// code only.
    async fn report_steer_dropped(&self, attribution: &MessageAttribution, reason: &str) {
        if let Some(client_msg_id) = &attribution.client_msg_id {
            self.emit(
                method::TURN_STEER_DROPPED,
                crate::protocol::TurnSteerDropped {
                    client_msg_id: client_msg_id.clone(),
                    reason: reason.into(),
                    sender: attribution.sender.clone(),
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
            arrived: tokio::time::Instant::now(),
        }
    }

    fn sent_by(account: Option<&str>) -> MessageAttribution {
        MessageAttribution {
            sender: account.map(|account| crate::protocol::MessageSender {
                account_id: account.into(),
                username: String::new(),
            }),
            ..Default::default()
        }
    }

    /// How the device names a Hub account: the issuer and the account, as one string.
    const MEMBER: &str = r#"["https://cloud.example","member"]"#;

    fn from_member(message: &str) -> QueuedTurn {
        let mut queued = queued(message, false, Default::default());
        queued.attribution.sender = sent_by(Some(MEMBER)).sender;
        queued
    }

    fn keep(account: Option<&str>) -> Command {
        Command::Keep {
            attribution: sent_by(account),
        }
    }

    fn steer(account: Option<&str>) -> Command {
        Command::Steer {
            message: "more".into(),
            attribution: sent_by(account),
            reply: crate::rc::ticket::ticket().0,
        }
    }

    fn idle_session() -> (Session, mpsc::Receiver<Frame>) {
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (session, out, _notes) = super::super::tests::harness_test_session_with_channels(
            AnyDriver::Codex(Box::new(driver)),
            "codex",
            SessionStatus::Idle,
        );
        (session, out)
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
                turn: Some("turn".into()),
                order: 0,
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
    /// when it goes, not as they stood when it came: one whose sender's access ended meanwhile is
    /// dropped, and its sender told. A colleague's goes on as the owner's does, in the owner's home
    /// folder and in whatever mode the next turn runs in (here Auto, set after the message was
    /// taken). A queue that kept the answer a message got on arrival would carry words in after
    /// their sender's access ended.
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
        assert_eq!(turn_message(session.next_queued_turn().await), "home");
        assert_eq!(
            dropped(&mut out),
            [("revoked".to_owned(), "access_ended".to_owned())]
        );
        assert_eq!(turn_message(session.next_queued_turn().await), "owner");

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
        assert_eq!(turn_message(session.next_queued_turn().await), "auto");
        assert!(dropped(&mut out).is_empty());
        session.driver.shutdown().await.unwrap();
    }

    /// A member's message waits through a turn longer than the limit, and the member, removed
    /// meanwhile, sends nothing more. The device never hears of the removal, so what decides is
    /// that nothing from that account was admitted here for the whole limit: the message is
    /// dropped as access_ended, while the owner's goes. Requests that name no sender, or another
    /// account, vouch for nobody here. Trusting the stamp the message got on arrival would carry
    /// a removed member's words into the next turn.
    #[tokio::test(start_paused = true)]
    async fn a_member_silent_past_the_limit_loses_the_waiting_message() {
        let (mut session, mut out) = idle_session();
        session.steer_queue.waiting.push_back(from_member("member"));
        session
            .steer_queue
            .waiting
            .push_back(queued("owner", true, Default::default()));
        for _ in 0..4 {
            tokio::time::advance(RECONFIRM_WITHIN / 3).await;
            for command in [
                keep(None),
                steer(None),
                keep(Some("other")),
                steer(Some("other")),
            ] {
                session.steer_queue.admitted(&command, false);
            }
        }
        assert_eq!(turn_message(session.next_queued_turn().await), "owner");
        // The drop names its sender as turn.started does, so only that account is told.
        let drops = std::iter::from_fn(|| out.try_recv().ok())
            .filter(|frame| frame.method() == method::TURN_STEER_DROPPED)
            .map(|frame| frame.params.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            drops,
            [serde_json::json!({
                "client_msg_id": "member",
                "reason": "access_ended",
                "sender": {"account_id": MEMBER, "username": ""},
            })]
        );
        session.driver.shutdown().await.unwrap();
    }

    /// Holds `id` for `turn`, as the plugin's receipt for it would, after what is held already.
    /// It names no conversation, so dropping its delivery record reads no ledger.
    fn hold(session: &mut Session, id: &str, turn: &str) {
        let order = session.steer_queue.next_order;
        session.steer_queue.next_order += 1;
        session.steer_queue.held.insert(
            id.into(),
            StepSteer {
                queued: queued(id, true, Default::default()),
                conversation: String::new(),
                record: id.into(),
                turn: Some(turn.into()),
                order,
            },
        );
    }

    /// Nadia's order: a message is held for the running turn's next tool step, the turn ends
    /// before another step comes, and the terminal never reports it undelivered (the report came
    /// with a later turn's start, or not at all). The turn's end alone moves it to wait for the
    /// next turn, which it then starts; a held message is never left behind across later turns.
    /// One held for a turn that is still running stays held, and one whose turn a stop ended is
    /// dropped and its sender told.
    #[tokio::test]
    async fn a_message_held_for_a_turn_that_ends_starts_the_next_turn() {
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (mut session, mut out, _notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Idle,
            );
        hold(&mut session, "follow-up", "finishing");
        hold(&mut session, "later", "next");
        session
            .held_turn_over(TurnOver::Ended("finishing"), false)
            .await;
        assert!(session.steer_queue.held.contains_key("later"));
        assert!(!session.steer_queue.held.contains_key("follow-up"));
        assert_eq!(turn_message(session.next_queued_turn().await), "follow-up");
        assert!(dropped(&mut out).is_empty());

        // A turn that began while one was still held ends that one's chance too.
        session
            .held_turn_over(TurnOver::Started("third"), false)
            .await;
        assert!(session.steer_queue.held.is_empty());
        assert_eq!(turn_message(session.next_queued_turn().await), "later");

        hold(&mut session, "stopped", "third");
        session.held_turn_over(TurnOver::Ended("third"), true).await;
        assert!(session.steer_queue.held.is_empty());
        assert!(session.steer_queue.waiting.is_empty());
        assert_eq!(
            dropped(&mut out),
            [("stopped".to_owned(), "interrupted".to_owned())]
        );
        session.driver.shutdown().await.unwrap();
    }

    /// A member who still has access and keeps being admitted here, by a keep for the waiting
    /// message or by any other request of theirs, gets the message delivered however long the
    /// turn before it ran.
    #[tokio::test(start_paused = true)]
    async fn a_member_still_admitted_past_the_limit_gets_the_waiting_message_delivered() {
        for confirm in [keep(Some(MEMBER)), steer(Some(MEMBER))] {
            let (mut session, mut out) = idle_session();
            session.steer_queue.waiting.push_back(from_member("member"));
            for _ in 0..4 {
                tokio::time::advance(RECONFIRM_WITHIN / 2).await;
                session.steer_queue.admitted(&confirm, false);
            }
            tokio::time::advance(RECONFIRM_WITHIN / 2).await;
            assert_eq!(turn_message(session.next_queued_turn().await), "member");
            assert!(dropped(&mut out).is_empty());
            session.driver.shutdown().await.unwrap();
        }
    }

    /// The terminal can take a message, hand it to a tool step and end the turn between two
    /// reports, so its receipt arrives with the delivery and the turn's end, read ahead of both,
    /// while the terminal's latest state already shows no turn running. That state doesn't make
    /// the message undelivered: it was delivered, and runs once. When that turn was stopped
    /// instead, the message is dropped as interrupted. A receipt read after its turn's end
    /// decides the message as that end did.
    #[tokio::test]
    async fn a_late_receipt_never_runs_a_delivered_message_twice() {
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (mut session, mut out, _notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Idle,
            );
        let awaiting = |session: &mut Session, id: &str| {
            hold(session, id, "finishing");
            session.steer_queue.receipt = Some(id.into());
        };

        // Delivered in the same batch as its receipt and the turn's end.
        awaiting(&mut session, "delivered");
        session.step_steer_resolved(true).await;
        session.step_steer_delivered("delivered".into());
        session
            .held_turn_over(TurnOver::Ended("finishing"), false)
            .await;
        assert!(session.next_queued_turn().await.is_none(), "it ran once");
        assert!(dropped(&mut out).is_empty());

        // Stopped in the same batch as its receipt.
        awaiting(&mut session, "stopped");
        session.step_steer_resolved(true).await;
        session
            .held_turn_over(TurnOver::Ended("finishing"), true)
            .await;
        assert!(session.next_queued_turn().await.is_none());
        assert_eq!(
            dropped(&mut out),
            [("stopped".to_owned(), "interrupted".to_owned())]
        );

        // The turn's end read first, then the receipt: decided as that end was.
        for (id, interrupted) in [("after-end", false), ("after-stop", true)] {
            awaiting(&mut session, id);
            session
                .held_turn_over(TurnOver::Ended("finishing"), interrupted)
                .await;
            assert!(session.steer_queue.held.contains_key(id));
            session.step_steer_resolved(true).await;
            assert!(session.steer_queue.held.is_empty());
        }
        assert_eq!(turn_message(session.next_queued_turn().await), "after-end");
        assert_eq!(
            dropped(&mut out),
            [("after-stop".to_owned(), "interrupted".to_owned())]
        );
        session.driver.shutdown().await.unwrap();
    }

    /// Held messages whose turn is over wait for the next turn in the order they were held, and
    /// start turns in that order.
    #[tokio::test]
    async fn held_messages_whose_turn_is_over_keep_their_order() {
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (mut session, _out, _notes) = super::super::tests::harness_test_session_with_channels(
            AnyDriver::Codex(Box::new(driver)),
            "codex",
            SessionStatus::Idle,
        );
        let sent: Vec<String> = (0..8).map(|index| format!("message-{index}")).collect();
        for id in &sent {
            hold(&mut session, id, "finishing");
        }
        session
            .held_turn_over(TurnOver::Ended("finishing"), false)
            .await;
        let mut started = Vec::new();
        while let Some(command) = session.next_queued_turn().await {
            started.push(turn_message(Some(command)));
        }
        assert_eq!(started, sent);
        session.driver.shutdown().await.unwrap();
    }

    /// A member's two messages wait for later turns. Taking up the first starts its turn, but
    /// says nothing about whether the member is still there: past the limit after the member's
    /// last admitted request, the second is dropped as access_ended. One waiting message
    /// vouching for the next would keep a removed member's words going turn after turn.
    #[tokio::test(start_paused = true)]
    async fn a_members_waiting_message_does_not_keep_the_next_one_admitted() {
        let (mut session, mut out) = idle_session();
        session.steer_queue.admitted(&steer(Some(MEMBER)), false);
        session.steer_queue.waiting.push_back(from_member("first"));
        session.steer_queue.waiting.push_back(from_member("second"));
        tokio::time::advance(RECONFIRM_WITHIN / 2).await;
        let first = session.next_queued_turn().await;
        let first = first.expect("the first message is taken up within the limit");
        session.steer_queue.admitted(&first, true);
        assert_eq!(turn_message(Some(first)), "first");
        tokio::time::advance(RECONFIRM_WITHIN).await;
        assert!(session.next_queued_turn().await.is_none());
        assert_eq!(
            dropped(&mut out),
            [("second".to_owned(), "access_ended".to_owned())]
        );
        session.driver.shutdown().await.unwrap();
    }
}
