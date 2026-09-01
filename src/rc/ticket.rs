//! The receipt for one instruction: **either it is taken and executed, or it is abandoned and
//! discarded — never both.**
//!
//! # Why it exists
//!
//! The daemon queues an instruction onto a session and then waits for the answer, and that wait
//! must be bounded. It waits under the session's own serial gate, and never while holding the
//! daemon's global state lock.
//!
//! But "time out and forget it" has a trap in each direction, and **both are real**:
//!
//! * **Drop it**: a `SetPermissionMode(bypass)` has already persisted `ever_dangerous` before it
//!   is queued (persist first, widen second — otherwise a roster that cannot be written lets a
//!   session survive a restart running with no checks while the ledger reads clean). Drop the
//!   command and that bit stays true — the session is permanently marked dangerous when nothing
//!   happened in it, and from then on an ordinary operator inexplicably cannot touch it.
//! * **Run it anyway**: the web interface has already received `SessionBusy` and taken it for a
//!   failure, and may have picked a different mode or retried; one `SESSION_REPLY_TIMEOUT` later
//!   that bypass takes effect as a widening nobody expected.
//!
//! The root cause is that **the timeout counts execution when it must only count queueing**: an
//! instruction nobody has picked up yet can be cancelled safely, one already picked up cannot —
//! its side effect is on its way, and the only option is to wait.
//!
//! # How "never both" is guaranteed
//!
//! One three-state atomic, one CAS from each side, **whoever wins decides**:
//!
//! * The executor, on dequeue: `QUEUED → TAKEN`. It executes only if it wins.
//! * The caller, on timeout: `QUEUED → ABANDONED`. It reports failure only if it wins.
//!
//! `reply.is_closed()` alone is not enough: the caller can give up in the window between the
//! check and the start of execution, so it believes nothing ran while it ran. The CAS has no
//! such window — both sides contend for the same cell.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::oneshot;

const QUEUED: u8 = 0;
const TAKEN: u8 = 1;
const ABANDONED: u8 = 2;

/// The executor's half of the receipt; it travels into the queue with the `Command`.
pub struct Ticket<T> {
    state: Arc<AtomicU8>,
    done: oneshot::Sender<crate::Result<T>>,
}

/// The caller's half of the receipt.
pub struct Receipt<T> {
    state: Arc<AtomicU8>,
    done: oneshot::Receiver<crate::Result<T>>,
}

/// The outcome of abandoning.
#[derive(Debug, PartialEq, Eq)]
pub enum Abandon {
    /// Abandoned: this instruction **never ran at all**, and never will. The caller can safely
    /// say "nothing happened".
    NeverRan,
    /// Not abandonable: it has already been taken and the side effect is on its way. The only
    /// option is to wait for the result.
    AlreadyTaken,
}

pub fn ticket<T>() -> (Ticket<T>, Receipt<T>) {
    let state = Arc::new(AtomicU8::new(QUEUED));
    let (tx, rx) = oneshot::channel();
    (
        Ticket {
            state: state.clone(),
            done: tx,
        },
        Receipt { state, done: rx },
    )
}

impl<T> Ticket<T> {
    /// Called once on dequeue. `false` = the caller has already abandoned it, **do not
    /// execute**.
    pub fn accept(&self) -> bool {
        self.state
            .compare_exchange(QUEUED, TAKEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Execution is done; hand the result back. The caller may no longer be listening (the
    /// ticket was taken, but its wait ran out), and that is fine — an undeliverable result is
    /// dropped. What matters is that the caller **knows** it failed to abandon.
    pub fn finish(self, v: crate::Result<T>) {
        let _ = self.done.send(v);
    }
}

impl<T> Receipt<T> {
    /// Try to abandon. See [`Abandon`].
    ///
    /// **Asking a second time gives the same answer.** Two parties ask along this path:
    /// `reply_within` abandons once when it times out (only a successful abandon earns the right
    /// to answer "nothing happened"), and the caller, holding that error, asks again whether the
    /// danger bit is to be taken back. The second CAS starts from `ABANDONED` and necessarily
    /// fails; reading every failure as "already taken" makes that bit unrecoverable — a session
    /// that never ran `bypass` is permanently marked owner-only, and written into the ledger.
    ///
    /// "Already abandoned" and "just abandoned" are **the same fact**: the executor's `accept()`
    /// starts only from `QUEUED`, so once the state falls to `ABANDONED` nobody can take it.
    pub fn abandon(&self) -> Abandon {
        match self
            .state
            .compare_exchange(QUEUED, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Abandon::NeverRan,
            Err(ABANDONED) => Abandon::NeverRan,
            Err(_) => Abandon::AlreadyTaken,
        }
    }

    /// Wait for the result, at most this long.
    pub async fn wait(
        &mut self,
        how_long: std::time::Duration,
    ) -> Option<Result<crate::Result<T>, oneshot::error::RecvError>> {
        tokio::time::timeout(how_long, &mut self.done).await.ok()
    }

    /// Wait until an already-taken instruction either reports a result or its
    /// executor disappears. Used after the bounded RPC response window: the
    /// caller has already been told the outcome is unknown, but the session's
    /// serial gate must remain held until the side effect is no longer in
    /// flight.
    pub async fn wait_until_closed(
        &mut self,
    ) -> Result<crate::Result<T>, oneshot::error::RecvError> {
        (&mut self.done).await
    }
}

#[cfg(test)]
mod tests {

    /// **Abandon on timeout, then drop the ticket**: asking again must still answer "never ran".
    ///
    /// Two parties ask on the real path: `reply_within` abandons once when it times out (only a
    /// successful abandon earns the right to answer "nothing happened"), and the caller, holding
    /// that `SessionBusy`, asks again to learn whether to take back the `ever_dangerous` that was
    /// persisted **before** the instruction was queued. Answering that second question with
    /// "already taken" leaves the bit set forever — a session where nothing happened is marked
    /// owner-only.
    #[tokio::test]
    async fn asking_twice_after_a_timeout_still_says_it_never_ran() {
        let (t, mut r) = ticket::<()>();
        // First leg: `reply_within` times out and abandons once.
        assert_eq!(r.abandon(), Abandon::NeverRan);
        // Second leg: the caller asks again, to take the danger bit back.
        assert_eq!(
            r.abandon(),
            Abandon::NeverRan,
            "abandoning once does not mean it ran"
        );
        // Abandoned stays abandoned: the executor dequeues only now and must be blocked.
        assert!(!t.accept(), "an abandoned instruction must not be taken");
        drop(t);
        assert_eq!(r.abandon(), Abandon::NeverRan);
        assert!(r.wait(std::time::Duration::from_millis(20)).await.is_some());
    }

    /// A ticket already **taken** answers "cannot abandon" however often it is asked — that bit
    /// cannot be cleared.
    #[tokio::test]
    async fn a_taken_ticket_never_reports_never_ran() {
        let (t, r) = ticket::<()>();
        assert!(t.accept());
        assert_eq!(r.abandon(), Abandon::AlreadyTaken);
        assert_eq!(r.abandon(), Abandon::AlreadyTaken);
    }

    /// Abandoning still counts when **the executor is discarded along with the ticket**.
    ///
    /// This is the path abandoning does not reach: after the instruction is queued and before it
    /// is picked up, the session task ends by itself (the harness exits, the driver hits EOF) —
    /// the `Receiver` is dropped together with the `Ticket` still sitting in the queue, and
    /// `accept()` is never called. The dangerous-mode path persists `ever_dangerous` **before**
    /// queueing, so something must still be able to answer "did it run or not".
    ///
    /// The CAS answers that by construction: it asks only "has anyone taken it", and not taken
    /// is not run.
    #[tokio::test]
    async fn a_ticket_dropped_without_being_accepted_still_counts_as_never_run() {
        let (t, r) = ticket::<()>();
        // The session task is gone: the ticket is discarded with the queue, never accepted.
        drop(t);
        assert_eq!(
            r.abandon(),
            Abandon::NeverRan,
            "an unanswerable \"did it run\" leaves the danger mark stuck on forever"
        );
    }
    use super::*;

    /// Both sides act at once and only one wins — the whole reason this type exists.
    #[tokio::test]
    async fn a_command_is_either_taken_or_abandoned_never_both() {
        let (t, r) = ticket::<()>();
        assert!(t.accept(), "the first accept wins");
        assert_eq!(
            r.abandon(),
            Abandon::AlreadyTaken,
            "a taken ticket cannot be abandoned"
        );

        let (t, r) = ticket::<()>();
        assert_eq!(r.abandon(), Abandon::NeverRan);
        assert!(!t.accept(), "an abandoned ticket cannot be accepted");
    }

    /// When abandoning answers `NeverRan`, the caller is entitled to state "this had no side
    /// effect at all" — the dangerous-mode rollback rests on exactly that.
    #[tokio::test]
    async fn abandoning_before_pickup_proves_nothing_happened() {
        let (t, r) = ticket::<u8>();
        assert_eq!(r.abandon(), Abandon::NeverRan);
        // The executor dequeues later and sees "do not execute".
        assert!(!t.accept());
    }

    /// A caller that stops listening after the ticket is taken is harmless: `finish` neither
    /// panics nor blocks.
    #[tokio::test]
    async fn finishing_into_a_gone_caller_is_harmless() {
        let (t, r) = ticket::<u8>();
        assert!(t.accept());
        drop(r);
        t.finish(Ok(7));
    }

    #[tokio::test]
    async fn a_finished_ticket_hands_the_value_back() {
        let (t, mut r) = ticket::<u8>();
        assert!(t.accept());
        t.finish(Ok(7));
        let got = r.wait(std::time::Duration::from_secs(1)).await;
        assert!(matches!(got, Some(Ok(Ok(7)))));
    }
}
