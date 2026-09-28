//! Session reuse receipts: `agit run` tells the Hub which session it starts from.
//!
//! The receipt is bookkeeping for the Hub, not part of the run. It is sent on a background
//! thread so the launch never waits for the Hub, and every failure is dropped: a Hub that is
//! unreachable, refuses the receipt or predates the endpoint leaves the run exactly as it
//! would have been without one.
//!
//! A receipt never renews credentials. It is the kind of request a run can cut off at exit, and
//! a renewal cut off after the Hub rotated the single-use refresh token signs the user out.
//! A signed-in account whose access token has expired sends no receipt at all (see
//! [`Client::from_env_with_live_access`]).

use super::Client;
use crate::domain::meta;
use std::time::Duration;

/// The request timeout of a receipt. A receipt is one request with no renewal and no retry,
/// and it overlaps the launch instead of delaying it, so this also bounds how long a run that
/// does not launch waits for it before exiting.
pub const TIMEOUT: Duration = Duration::from_secs(3);

/// How a run takes up a session: on the same line, or on a new branch forked from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReuseMode {
    Continue,
    Fork,
}

impl ReuseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Fork => "fork",
        }
    }
}

/// One receipt: the session at `commit` of `owner/name`, taken up as `mode`.
///
/// The session id and commit become part of the request path and body, so only the shapes the
/// Hub accepts can be built: a settled `agit-` id and a full lowercase commit id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReuse {
    owner: String,
    name: String,
    session_id: String,
    commit: String,
    mode: ReuseMode,
}

impl SessionReuse {
    pub fn new(
        owner: &str,
        name: &str,
        session_id: &str,
        commit: &str,
        mode: ReuseMode,
    ) -> Option<Self> {
        // A version id is `agit-` plus a full commit id, so its check covers the commit too.
        let full_commit = meta::is_bare_id(&meta::id_from_sha(commit));
        (meta::is_bare_id(session_id) && full_commit).then(|| Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
            session_id: session_id.to_owned(),
            commit: commit.to_owned(),
            mode,
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn commit(&self) -> &str {
        &self.commit
    }

    pub fn mode(&self) -> ReuseMode {
        self.mode
    }
}

/// A receipt in flight. Dropping it waits for the request to finish, at most [`TIMEOUT`], so a
/// run that exits right away (one that does not launch) still delivers its receipt.
#[must_use = "dropping the handle waits for the receipt; keep it alive while the run proceeds"]
pub struct Pending(Option<std::thread::JoinHandle<()>>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

/// The client that carries receipts: it never renews, and it does not exist while a signed-in
/// account's access token is expired.
pub fn client() -> Option<Client> {
    Client::from_env_with_live_access(TIMEOUT)
}

/// Send `receipt` on a background thread through a client from [`client`], discarding the
/// outcome.
pub fn send(client: Client, receipt: SessionReuse) -> Pending {
    let worker = std::thread::Builder::new()
        .name("agit-reuse-receipt".into())
        .spawn(move || {
            let _ = client.record_session_reuse(&receipt);
        })
        .ok();
    Pending(worker)
}
