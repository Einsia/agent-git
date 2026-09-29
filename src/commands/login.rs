//! `agit login` — sign in to a hub.
//!
//! Three paths:
//!
//! * **Browser authorization (default)**: the CLI opens a pending authorization request and
//!   opens the web interface; the user confirms there with GitHub (or an existing web session),
//!   and the CLI polls until it holds the session. The terminal touches no credential — this is
//!   the default path. Without a terminal it returns the link for a human to authorize,
//!   and `--complete` retrieves the approved session in a separate invocation.
//! * **device code**: the CLI prints a short code and the user types and confirms it in a
//!   browser on **any device**. SSH, containers and machines with no browser take this one.
//! * `--with-token`: read a PAT from stdin, for CI and agent environments (non-interactive).
//!
//! Every path proves first that the credentials can be saved under `AGIT_HOME`, before it asks
//! the Hub for anything: an approval the Hub has already consumed cannot be replayed once saving
//! fails, so a sandbox that forbids writing there must stop the login before the human approves.
//!
//! The process that asks for a browser or device-code request is not the only one that can
//! finish it. Agent runtimes stop a waiting command before the human answers, so every request is
//! recorded under `AGIT_HOME` the moment the Hub creates it (see
//! [`crate::infra::credentials::pending`]), and `agit login --complete` without a value claims
//! the recorded request from any later process, waiting a bounded time for the approval. A new
//! `agit login` claims a recorded request the human already approved instead of replacing it.
//! Signing out forgets the recorded request, and a claim of it that receives the session
//! afterward saves nothing and signs that session out again.
//!
//! The CLI has no username-and-password path: a password belongs in a browser only. The web
//! interface (GitHub OAuth or a form) is the only place that takes one; the CLI always uses one
//! of the two paths above, both of which finish in the browser.
//!
//! Signing in stores the access/refresh token locally, along with the account name and email —
//! that is where a commit's author field comes from, the same way GitHub records a commit's
//! author. Credentials are stored per hub, so switching hub switches identity (`--hub` picks the
//! target for this run and writes it into the `hub.url` config, so the next command after
//! signing in does not connect back to the default hub).

use super::{CmdResult, InteractionRequired, remote_request};
use crate::infra::config;
use crate::infra::credentials::HubCredential;
use crate::infra::credentials::pending::{self, Flow, PendingLogin, Withdrawal};
use crate::infra::local_state;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::io::Read as _;
use std::time::{Duration, Instant};

/// How long `--complete` waits for the approval unless `--wait` says otherwise. It stays below
/// the foreground limit of common agent runtimes, which stop a longer command and report nothing.
pub const DEFAULT_COMPLETE_WAIT: u64 = 90;

/// A waiting sign-in gives up when its request expires, and never later than this, so a Hub
/// that advertises a long lifetime, or a `--wait` beyond it, cannot hold a terminal without bound.
const LONGEST_WAIT: Duration = Duration::from_secs(10 * 60);

/// The request timeout of every poll. A poll can be the one that receives the approved session,
/// and a client that gives up on it first drops a session the Hub has already handed out, so no
/// caller polls with a shorter timeout. It stays below [`local_state::LOCK_WAIT`], so a process
/// waiting for the claim lock outlasts one slow poll.
const POLL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(ClapArgs)]
pub struct Args {
    /// Hub to sign in to (default: AGIT_HUB_URL → config hub.url → the built-in public hub).
    #[arg(long, value_name = "url")]
    pub hub: Option<String>,
    /// Read an explicitly supplied PAT from stdin and sign in.
    #[arg(long, conflicts_with_all = ["device", "complete"])]
    pub with_token: bool,
    /// Skip the menu and use the device-code flow directly.
    #[arg(long, conflicts_with = "complete")]
    pub device: bool,
    /// Finish a sign-in the human approved and save the credentials. Without a value it finishes
    /// the request `agit login` recorded for this Hub, including an interrupted device-code
    /// sign-in; with a value it finishes that browser authorization request.
    #[arg(long, value_name = "state", num_args = 0..=1)]
    pub complete: Option<Option<String>>,
    /// With --complete: seconds to wait for the human's approval before giving up (default: 90;
    /// 0 checks once).
    #[arg(long, value_name = "seconds", requires = "complete")]
    pub wait: Option<u64>,
}

pub fn run(args: Args) -> CmdResult {
    let hub = args.hub.clone().unwrap_or_else(config::hub_url);
    if crate::infra::hub_authority::HubAuthority::parse(&hub).is_err() {
        ui::error(
            "the Hub must be a valid HTTP or HTTPS address without user information, query, or fragment",
        );
        return Ok(ExitCode::Usage);
    }
    let hub = hub.trim().trim_end_matches('/').to_string();
    let new_request = !args.with_token && args.complete.is_none();
    let approved = new_request.then(|| approved_request(&hub)).flatten();
    if approved.is_none()
        && new_request
        && !args.device
        && (super::json::is_capturing() || !ui::prompt::can_ask())
    {
        return start_browser_handoff(&hub);
    }
    ui::info(format_args!("hub: {}", ui::accent(&hub)));

    let result = if let Some(signed_in) = approved {
        Ok(Some(signed_in))
    } else if args.with_token {
        login_with_token(&hub)
    } else if let Some(state) = &args.complete {
        complete(
            &hub,
            state.as_deref(),
            args.wait.unwrap_or(DEFAULT_COMPLETE_WAIT),
        )
    } else if args.device {
        login_device(&hub)
    } else {
        login_interactive(&hub)
    };

    match result {
        Ok(Some(mut signed_in)) => {
            let origin = signed_in.origin;
            if origin == Origin::Obtained {
                match save_obtained(&hub, &mut signed_in) {
                    Ok(None) => {}
                    Ok(Some(withdrawal)) => {
                        return Err(anyhow::Error::new(InteractionRequired(cancelled(
                            &hub,
                            &signed_in.credential,
                            withdrawal,
                        ))));
                    }
                    Err(error) => {
                        ui::error(&format!("cannot save the signed-in credentials: {error:#}"));
                        if revoke_unsaved(&hub, &signed_in.credential) {
                            ui::hint("the Hub session this login created was signed out again");
                        }
                        for hint in local_state::hints(&error) {
                            ui::hint(&hint);
                        }
                        return Ok(ExitCode::Precondition);
                    }
                }
            }
            let cred = &signed_in.credential;
            // A hub named explicitly with --hub is most likely the one the user keeps using,
            // so remember it.
            if args.hub.is_some() {
                let _ = config::set_global("hub.url", Some(&hub));
            }
            let who = ui::bold(&signed_in.who);
            ui::success(&match origin {
                Origin::Obtained => format!("signed in as {who}"),
                Origin::SavedElsewhere => {
                    format!("signed in as {who}; another agit process finished this sign-in")
                }
                Origin::AlreadySaved => {
                    format!("no sign-in request is waiting; already signed in as {who}")
                }
            });
            crate::telemetry::observe(crate::telemetry::Observation::Authentication(true));
            if origin == Origin::Obtained {
                crate::telemetry::account_saved(&hub, cred.account_id.as_deref());
            }
            Ok(ExitCode::Ok)
        }
        Ok(None) => {
            // An unavailable or cancelled prompt cannot select a sign-in flow.
            ui::error("signing in needs an interactive terminal.");
            ui::hint(
                "CI / agents use `agit login --with-token < token.txt` (reads a PAT from stdin)",
            );
            Ok(ExitCode::Interactive)
        }
        Err(error) if error.is::<LocalInputFailure>() => {
            ui::error(&format!("{error:#}"));
            Ok(ExitCode::Precondition)
        }
        Err(error) if error.is::<Unsaveable>() => Ok(refuse_unsaveable(&error)),
        Err(error) => Err(error),
    }
}

/// Credentials a sign-in produced, and whether this process still has to save them.
struct SignedIn {
    credential: HubCredential,
    who: String,
    origin: Origin,
    /// The poll of a sign-in request that received the credentials, until they are committed.
    claim: Option<Claim>,
}

impl SignedIn {
    fn new(credential: HubCredential, origin: Origin) -> Self {
        Self {
            who: credential.username.clone(),
            credential,
            origin,
            claim: None,
        }
    }

    /// Credentials a poll of `claim.request` received.
    fn claimed(credential: HubCredential, claim: Claim) -> Self {
        Self {
            claim: Some(claim),
            ..Self::new(credential, Origin::Obtained)
        }
    }
}

/// A poll that received the session of `request`. Its claim lock is held until the session is
/// committed, so a process finishing the same request meanwhile waits, then finds it finished,
/// and a new request cannot replace this one before then.
struct Claim {
    request: PendingLogin,
    /// Whether `request` was its Hub's recorded request when the claim began. Only then can a
    /// sign-out or a newer request withdraw it.
    recorded: bool,
    _lock: std::fs::File,
}

/// Save the credentials this process obtained. Credentials a claim received are committed
/// together with the claimed request (see [`pending::commit`]): if a sign-out or a newer
/// request withdrew it meanwhile, nothing is saved and the withdrawal is returned. The claim
/// lock is released once this returns, so nothing the caller does next holds it.
fn save_obtained(hub: &str, signed_in: &mut SignedIn) -> crate::Result<Option<Withdrawal>> {
    let claim = signed_in.claim.take();
    let credential = &signed_in.credential;
    let mut withdrawal = None;
    crate::telemetry::acquisition::save_login_with(hub, credential, || match &claim {
        Some(claim) => {
            withdrawal = pending::commit(hub, &claim.request, claim.recorded, credential)?;
            Ok(withdrawal.is_none())
        }
        None => crate::infra::credentials::save(hub, credential).map(|()| true),
    })?;
    Ok(withdrawal)
}

/// What to say about credentials a claim received for a request `withdrawal` withdrew before
/// they were saved. The session is signed out again, since keeping it anywhere would outlive the
/// sign-out or the request that replaced it.
fn cancelled(hub: &str, credential: &HubCredential, withdrawal: Withdrawal) -> String {
    let session = if revoke_unsaved(hub, credential) {
        "the Hub session it received was signed out again"
    } else {
        "the Hub session it received expires on its own"
    };
    let (why, next) = match withdrawal {
        Withdrawal::Forgotten => (
            "`agit logout` cancelled it",
            format!("Run `{}` to sign in again.", login_command(hub)),
        ),
        Withdrawal::Replaced => (
            "a newer sign-in request replaced it",
            format!(
                "Finish the newer request with `{}`.",
                complete_command(hub, None)
            ),
        ),
    };
    format!(
        "the human approved the sign-in, but {why} before its credentials were saved, so nothing was signed in; {session}. {next}"
    )
}

/// The failure for a recorded request that `withdrawal` withdrew before a poll received its
/// session.
fn withdrawn(hub: &str, withdrawal: Withdrawal) -> anyhow::Error {
    anyhow::Error::new(InteractionRequired(match withdrawal {
        Withdrawal::Forgotten => format!(
            "the sign-in request was cancelled before it was finished: `agit logout` forgot it, or another agit process found it no longer valid. Run `{}` again and ask the human to approve the new request.",
            login_command(hub)
        ),
        Withdrawal::Replaced => format!(
            "a newer sign-in request replaced this one before it was finished. Finish the newer request with `{}`.",
            complete_command(hub, None)
        ),
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// This process received the credentials from the Hub and has to save them.
    Obtained,
    /// Another agit process claimed the same request and saved its credentials.
    SavedElsewhere,
    /// No request was waiting; these are the credentials already saved for the Hub.
    AlreadySaved,
}

/// Saving must be possible before the Hub is asked for anything: a one-time approval consumed
/// by a process that then cannot save the tokens is lost, and the Hub keeps a session nobody
/// holds.
///
/// The marker is context above the local state failure, so that failure stays in the error chain:
/// wherever the refusal is reported, its own next steps and exit category still apply.
#[derive(Debug)]
enum Unsaveable {
    /// Refused before any login request existed.
    BeforeRequest,
    /// Refused before claiming a request an earlier `agit login` created. Its approval is still
    /// unused, so the same `--complete` command finishes it later; a new login would ask the
    /// human to approve again. `state` is the value the command named, if it named one.
    Pending { hub: String, state: Option<String> },
}

impl std::fmt::Display for Unsaveable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BeforeRequest => "cannot sign in, and no login request was created",
            Self::Pending { .. } => {
                "cannot finish signing in, and the approved sign-in was not claimed"
            }
        })
    }
}

fn ensure_saveable(refusal: impl FnOnce() -> Unsaveable) -> crate::Result<()> {
    crate::infra::credentials::preflight_writable().map_err(|error| error.context(refusal()))
}

fn refuse_unsaveable(error: &anyhow::Error) -> ExitCode {
    ui::error(&format!("{error:#}"));
    for hint in local_state::hints(error) {
        ui::hint(&hint);
    }
    if let Some(Unsaveable::Pending { hub, state }) = error.downcast_ref::<Unsaveable>() {
        ui::hint(&format!(
            "once AGIT_HOME is writable, rerun `{}` to claim the approved sign-in; a new `agit login` would need the human to approve again",
            complete_command(hub, state.as_deref())
        ));
    }
    ExitCode::Precondition
}

/// The command that finishes a sign-in, printed for this platform's shell. Without `state` it
/// claims the Hub's recorded request, so no polling value is printed.
fn complete_command(hub: &str, state: Option<&str>) -> String {
    let mut command = format!("agit login --hub {} --complete", quote_argument(hub));
    if let Some(state) = state {
        command.push(' ');
        command.push_str(&quote_argument(state));
    }
    command
}

/// The command that starts a new sign-in at `hub`, printed for this platform's shell. Guidance
/// names the Hub, because a `--complete` or a claim may concern a Hub other than the configured
/// one.
fn login_command(hub: &str) -> String {
    format!("agit login --hub {}", quote_argument(hub))
}

/// The next step while `record` still waits for the human's approval.
fn still_pending(record: &PendingLogin) -> String {
    let (approve, what) = match (&record.approval, record.flow) {
        (Some(approval), _) => (
            format!(
                "Ask the human to open {} and enter the code {} to approve the sign-in",
                approval.url, approval.user_code
            ),
            "code",
        ),
        (None, Flow::Device) => (
            "Ask the human to finish approving the sign-in code".to_owned(),
            "code",
        ),
        (None, Flow::Browser) => (
            "Ask the human to finish approving the login link".to_owned(),
            "link",
        ),
    };
    format!(
        "{approve}, then retry the same `agit login --complete` command. If the {what} expired, run `{}` again to request a new {what}.",
        login_command(&record.hub)
    )
}

/// The failure for a request the Hub will never approve.
fn no_longer_valid(hub: &str) -> anyhow::Error {
    anyhow::Error::new(InteractionRequired(format!(
        "the sign-in request is no longer valid: it expired or was already used. Run `{}` again and ask the human to approve the new request.",
        login_command(hub)
    )))
}

/// The next step for a command that found no credentials while a sign-in request for the Hub
/// waits for the human's approval.
pub(crate) fn complete_hint(hub: &str) -> String {
    format!(
        "a sign-in for this Hub is waiting for the human's approval; after they approve it, run `{}` to finish it instead of starting a new `agit login`",
        complete_command(hub, None)
    )
}

/// One argument of a command line printed for the human or agent to run in this platform's shell.
#[cfg(windows)]
fn quote_argument(value: &str) -> String {
    ui::quote_powershell_argument(value)
}

#[cfg(not(windows))]
fn quote_argument(value: &str) -> String {
    ui::quote_posix_argument(value)
}

/// Sign out a session whose tokens could not be saved. It uses the new access token directly and
/// never refreshes, because a refresh would have to save its result too. Failures are ignored:
/// the session then expires on its own.
fn revoke_unsaved(hub: &str, credential: &HubCredential) -> bool {
    crate::hub::Client::for_hub_with_token(hub, &credential.access_token)
        .logout()
        .is_ok()
}

/// Interactive entry point: browser authorization by default, device code as the alternative.
///
/// A separate function so any other command that needs to "make sure we are signed in" reuses it.
pub fn login() -> crate::Result<Option<String>> {
    let hub = config::hub_url();
    let mut signed_in = match approved_request(&hub) {
        Some(signed_in) => signed_in,
        None => match login_interactive(&hub)? {
            Some(signed_in) => signed_in,
            None => return Ok(None),
        },
    };
    if signed_in.origin == Origin::Obtained {
        let saved = save_obtained(&hub, &mut signed_in);
        let credential = &signed_in.credential;
        match saved {
            Ok(None) => {}
            Ok(Some(withdrawal)) => {
                return Err(anyhow::Error::new(InteractionRequired(cancelled(
                    &hub, credential, withdrawal,
                ))));
            }
            Err(error) => {
                return Err(error.context(if revoke_unsaved(&hub, credential) {
                    "cannot save the signed-in credentials; the Hub session this login created was signed out again"
                } else {
                    "cannot save the signed-in credentials"
                }));
            }
        }
        crate::telemetry::account_saved(&hub, credential.account_id.as_deref());
    }
    crate::telemetry::observe(crate::telemetry::Observation::Authentication(true));
    Ok(Some(signed_in.who))
}

fn login_interactive(hub: &str) -> crate::Result<Option<SignedIn>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    println!();
    println!("  how do you want to sign in?");
    println!(
        "    {} browser — press Enter to sign in through your browser",
        ui::accent("1.")
    );
    println!(
        "    {} device code — we show a code, you enter it on the website (SSH, containers, no browser)",
        ui::accent("2.")
    );
    loop {
        let choice = ui::prompt::input("choice", Some("1"))
            .map_err(|error| error.context(LocalInputFailure))?;
        let Some(choice) = choice else {
            return Ok(None);
        };
        match choice.trim() {
            "" | "1" => return login_browser(hub),
            "2" => return login_device(hub),
            _ => ui::error("choose 1 for browser sign-in or 2 for a device code."),
        }
    }
}

use std::io::IsTerminal as _;

// ──────────────────────── recorded requests ────────────────────────

/// A sign-in request this process polls.
struct Request {
    record: PendingLogin,
    /// Whether `record` was saved as its Hub's recorded request when this process began to
    /// finish it. Only then does finishing it remove the record, and only then does the record's
    /// removal or replacement withdraw it.
    recorded: bool,
}

impl Request {
    /// Record a request the Hub has just created, so any later `agit login --complete` can claim
    /// it. A request that cannot be recorded still works in this process, and for the browser
    /// handoff through the printed command, so failing to record it only warns. Recording waits
    /// for a claim of the earlier request that is in flight (see [`pending::save`]).
    fn record(record: PendingLogin) -> Self {
        let (recorded, replaces) = match pending::save(&record) {
            Ok(replaces) => (true, replaces),
            Err(error) => {
                ui::warning(&format!(
                    "cannot record the sign-in request, so another agit process cannot finish it: {error:#}"
                ));
                (false, false)
            }
        };
        if replaces {
            ui::hint(
                "this sign-in request replaces an earlier one that was still waiting for approval",
            );
        }
        Self { record, recorded }
    }

    /// Remove the record once the request can no longer be approved. The caller holds the claim
    /// lock (see [`pending::remove`]).
    fn forget(&self) {
        if self.recorded {
            let _ = pending::remove(&self.record.hub, &self.record.secret);
        }
    }

    /// The failure for a request the Hub will never approve, after forgetting it.
    fn gone(&self) -> anyhow::Error {
        self.forget();
        no_longer_valid(&self.record.hub)
    }
}

/// `agit login --complete [state]`: claim an approved request, waiting at most `wait` seconds for
/// the human's approval.
fn complete(hub: &str, state: Option<&str>, wait: u64) -> crate::Result<Option<SignedIn>> {
    let wait = Duration::from_secs(wait).min(LONGEST_WAIT);
    let request = match state {
        Some(state) if state.trim().is_empty() => {
            return crate::input_argument(Err(anyhow::anyhow!(
                "the sign-in state must not be empty"
            )));
        }
        Some(state) => match pending::load(hub).ok().flatten() {
            Some(record) if record.flow == Flow::Browser && record.secret == state => Request {
                record,
                recorded: true,
            },
            // A request this AGIT_HOME did not record has no known lifetime; only the wait
            // bounds it.
            _ => Request {
                record: PendingLogin::new(
                    hub,
                    Flow::Browser,
                    state,
                    pending::MIN_INTERVAL,
                    wait.as_secs(),
                ),
                recorded: false,
            },
        },
        // A claim forgets the record only in the step that saves its credentials, so finding none
        // never hides a sign-in whose credentials are still unsaved; a query that finds nothing
        // therefore takes no claim lock and creates no lock file.
        None => match pending::load(hub)? {
            Some(record) => Request {
                record,
                recorded: true,
            },
            None => return nothing_to_complete(hub),
        },
    };
    ensure_saveable(|| Unsaveable::Pending {
        hub: hub.to_owned(),
        state: state.map(str::to_owned),
    })?;
    let start = Instant::now();
    let mut until = start + wait;
    // A recorded request that has expired gets one claim, which settles it under the claim lock
    // without polling (see [`settled`]).
    if request.recorded {
        until = until.min(start + request.record.remaining());
    }
    let notice = (!wait.is_zero()).then(|| {
        let at = request
            .record
            .approval
            .as_ref()
            .map(|approval| format!(" at {} with the code {}", approval.url, approval.user_code))
            .unwrap_or_default();
        format!(
            "waiting up to {} seconds for the human to approve the sign-in{at}…",
            until.saturating_duration_since(start).as_secs()
        )
    });
    match await_approval(hub, &request, until, false, notice, true)? {
        Some(signed_in) => Ok(Some(signed_in)),
        None if request.recorded && request.record.expired() => {
            let _lock = pending::claim_lock(hub)?;
            settled(hub, &request).unwrap_or_else(|| Err(request.gone()))
        }
        None => Err(anyhow::Error::new(InteractionRequired(still_pending(
            &request.record,
        )))),
    }
}

/// `--complete` without a value when no request is recorded for the Hub. A request recorded for
/// another Hub is most likely the one meant, since a login with `--hub` does not change the
/// configured Hub before it succeeds. A request another process already finished leaves saved
/// credentials, and the sign-in the agent is after has happened; otherwise only a new
/// `agit login` can sign in.
fn nothing_to_complete(hub: &str) -> crate::Result<Option<SignedIn>> {
    let label = crate::infra::hub_authority::safe_label(hub);
    if let Some(other) = pending::waiting_elsewhere(hub).first() {
        return Err(anyhow::Error::new(InteractionRequired(format!(
            "no sign-in request is waiting for {label}, but one is waiting for {}. Finish it with `{}`.",
            crate::infra::hub_authority::safe_label(other),
            complete_command(other, None)
        ))));
    }
    if let Some(credential) =
        crate::infra::credentials::load(hub).filter(|credential| !credential.refresh_expired())
    {
        return Ok(Some(SignedIn::new(credential, Origin::AlreadySaved)));
    }
    Err(anyhow::Error::new(InteractionRequired(format!(
        "no sign-in request is waiting for {label}. Run `{}` to request a new login link and ask the human to approve it.",
        login_command(hub)
    ))))
}

/// Poll `request` until the human approves it or `until` passes; `Ok(None)` means it still
/// waits for approval at `until`. `pause_first` waits one interval before the first poll, as a
/// flow that has just shown the human its link does. `notice` is said once when the wait goes
/// on after the first poll.
///
/// With `retry`, a poll the Hub did not answer (see [`unanswered`]) does not end the wait: the
/// request is unchanged, so a later poll can still receive the approval. Its failure is returned
/// only if no later poll is answered before `until`.
fn await_approval(
    hub: &str,
    request: &Request,
    until: Instant,
    pause_first: bool,
    mut notice: Option<String>,
    retry: bool,
) -> crate::Result<Option<SignedIn>> {
    let client = crate::hub::Client::for_hub_with_timeout(&request.record.hub, POLL_TIMEOUT);
    let interval = request.record.poll_interval();
    let mut pause = pause_first;
    let mut failure = None;
    loop {
        if pause {
            if Instant::now()
                .checked_add(interval)
                .is_none_or(|next| next > until)
            {
                return failure.map_or(Ok(None), Err);
            }
            if let Some(notice) = notice.take() {
                ui::progress(notice);
            }
            std::thread::sleep(interval);
        }
        pause = true;
        match claim(hub, &client, request) {
            Ok(Some(signed_in)) => return Ok(Some(signed_in)),
            Ok(None) => failure = None,
            Err(error) if retry && unanswered(&error) => failure = Some(error),
            Err(error) => return Err(error),
        }
    }
}

/// Poll the Hub once for `request`: the credentials once the human approved it, `None` while
/// the approval is still missing.
///
/// The claim lock is taken whether or not a record names the request: a process finishing the
/// same request can hold it with the session received and not yet saved, and a poll then only
/// hears from the Hub that the request was used. Under the lock the request is polled only if
/// the record and the claim marker cannot settle it (see [`settled`]).
fn claim(
    hub: &str,
    client: &crate::hub::Client,
    request: &Request,
) -> crate::Result<Option<SignedIn>> {
    let lock = pending::claim_lock(hub)?;
    if let Some(standing) = settled(hub, request) {
        return standing;
    }
    match poll_once(client, &request.record) {
        Ok(Some(session)) => {
            let (credential, _) = session_credential(session);
            Ok(Some(SignedIn::claimed(
                credential,
                Claim {
                    request: request.record.clone(),
                    recorded: request.recorded,
                    _lock: lock,
                },
            )))
        }
        Ok(None) => Ok(None),
        Err(error) => match refusal(&error) {
            Some(Refusal::Gone) => Err(request.gone()),
            Some(Refusal::Refused) => {
                request.forget();
                Err(error)
            }
            None => Err(error),
        },
    }
}

/// How `request` stands without polling it, read while this process holds the claim lock:
/// finished by another process, withdrawn by a sign-out or a newer request, or expired, in which
/// case its record is forgotten. `None` when only a poll can tell.
///
/// The claim marker is read first, whatever the record says: the process that finished the
/// request forgets the record only after saving the credentials, and only on a best-effort basis,
/// so a record that is still present does not make the request unfinished.
fn settled(hub: &str, request: &Request) -> Option<crate::Result<Option<SignedIn>>> {
    if let Some(credential) = pending::signed_in_by(hub, &request.record.secret) {
        return Some(Ok(Some(SignedIn::new(credential, Origin::SavedElsewhere))));
    }
    if !request.recorded {
        return None;
    }
    let current = pending::load(hub).ok().flatten();
    if let Some(withdrawal) = Withdrawal::of(&request.record, current.as_ref()) {
        return Some(Err(if request.record.expired() {
            no_longer_valid(&request.record.hub)
        } else {
            withdrawn(&request.record.hub, withdrawal)
        }));
    }
    request.record.expired().then(|| Err(request.gone()))
}

/// A Hub answer that ends a request for good.
enum Refusal {
    /// The Hub no longer knows the request: it expired or a poll already consumed it.
    Gone,
    /// The Hub refused the poll for another reason that retrying cannot change.
    Refused,
}

/// Whether a failed poll ends its request. Only the Hub's own API ends a request, and its error
/// answers carry a kind: a refusal without one comes from a proxy, a firewall or a captive portal
/// in front of the Hub and says nothing about the request. Transport failures, server errors,
/// timeouts and rate limits leave the request claimable as well.
fn refusal(error: &anyhow::Error) -> Option<Refusal> {
    let api = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::hub::client::ApiError>())?;
    if api.kind.is_empty() {
        None
    } else if api.status == 400 && api.kind == "expired_token" {
        Some(Refusal::Gone)
    } else if (400..500).contains(&api.status) && !matches!(api.status, 408 | 429) {
        Some(Refusal::Refused)
    } else {
        None
    }
}

/// Whether a failed poll left its request as it was, so polling again can still succeed: the Hub
/// was not reached, or the answer did not end the request (see [`refusal`]). An invalid sign-in
/// answer is final, because the poll that received it may have used the approval.
fn unanswered(error: &anyhow::Error) -> bool {
    error.is::<super::RemoteRequest>() && !error.is::<InvalidSession>() && refusal(error).is_none()
}

/// The Hub answered a poll with something that is neither "pending" nor a session.
#[derive(Debug)]
struct InvalidSession;

impl std::fmt::Display for InvalidSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Hub returned an invalid sign-in response")
    }
}

impl std::error::Error for InvalidSession {}

fn poll_once(
    client: &crate::hub::Client,
    record: &PendingLogin,
) -> crate::Result<Option<crate::hub::LoginResponse>> {
    authorization_response(
        client,
        record.flow.poll_path(),
        record.flow.poll_key(),
        &record.secret,
    )
}

/// What a command that found no credentials for a Hub learns from the Hub's recorded request.
pub(crate) enum PendingSignIn {
    /// No request waits for approval.
    Absent,
    /// A request waits: the human has not approved it, or it cannot be claimed right now.
    Waiting,
    /// The human had approved it, and its credentials are saved now.
    SignedIn,
}

/// The outcome of one claim of the Hub's recorded request that does not wait.
enum Recorded {
    Absent,
    Waiting,
    Approved(Box<SignedIn>),
}

/// Claim the Hub's recorded request with one poll, never waiting for the human. With `wait` it
/// waits, within the bound of [`pending::claim_lock`], for another process claiming the same
/// request and then reports what that process left; without it, a request another process is
/// claiming is still waiting. A failed claim leaves the request for `agit login --complete`.
///
/// A missing record is read without the lock: a claim forgets the record only in the step that
/// saves its credentials, so no claim in flight hides behind it.
fn claim_recorded_once(hub: &str, wait: bool) -> Recorded {
    let Ok(Some(record)) = pending::load(hub) else {
        return Recorded::Absent;
    };
    let request = Request {
        record,
        recorded: true,
    };
    // Claiming spends the approval, so credentials that cannot be saved would lose it.
    if !request.record.expired() && crate::infra::credentials::preflight_writable().is_err() {
        return Recorded::Waiting;
    }
    let lock = if wait {
        pending::claim_lock(hub).ok()
    } else {
        pending::try_claim_lock(hub).ok().flatten()
    };
    let Some(lock) = lock else {
        return Recorded::Waiting;
    };
    match settled(hub, &request) {
        Some(Ok(Some(signed_in))) => return Recorded::Approved(Box::new(signed_in)),
        Some(_) => return Recorded::Absent,
        None => {}
    }
    let client = crate::hub::Client::for_hub_with_timeout(&request.record.hub, POLL_TIMEOUT);
    match poll_once(&client, &request.record) {
        Ok(Some(session)) => {
            let (credential, _) = session_credential(session);
            Recorded::Approved(Box::new(SignedIn::claimed(
                credential,
                Claim {
                    request: request.record,
                    recorded: true,
                    _lock: lock,
                },
            )))
        }
        Ok(None) => Recorded::Waiting,
        Err(error) if refusal(&error).is_some() => {
            request.forget();
            Recorded::Absent
        }
        Err(_) => Recorded::Waiting,
    }
}

/// A sign-in the human approved for the request an earlier `agit login` recorded. A new login
/// claims it first, and waits for another process claiming it: replacing the recorded request
/// would discard the approval and ask the human to approve again.
fn approved_request(hub: &str) -> Option<SignedIn> {
    match claim_recorded_once(hub, true) {
        Recorded::Approved(signed_in) => {
            if signed_in.origin == Origin::Obtained {
                ui::progress(
                    "the human already approved the sign-in request that was waiting; finishing it instead of starting a new one",
                );
            }
            Some(*signed_in)
        }
        Recorded::Absent | Recorded::Waiting => None,
    }
}

/// Claim the Hub's recorded sign-in request with one poll, for a command that needs credentials
/// and found none.
///
/// An agent runtime can stop `agit login` before the human approves, and the agent then runs
/// the command it wanted to run. Claiming here lets that command continue signed in, instead of
/// sending the agent back to a new login the human has to approve again.
pub(crate) fn claim_recorded(hub: &str) -> PendingSignIn {
    let mut signed_in = match claim_recorded_once(hub, false) {
        Recorded::Absent => return PendingSignIn::Absent,
        Recorded::Waiting => return PendingSignIn::Waiting,
        Recorded::Approved(signed_in) => signed_in,
    };
    if signed_in.origin == Origin::Obtained {
        let saved = save_obtained(hub, &mut signed_in);
        let credential = &signed_in.credential;
        match saved {
            Ok(None) => {}
            // The command goes on as it would without the approval: not signed in.
            Ok(Some(withdrawal)) => {
                ui::warning(&cancelled(hub, credential, withdrawal));
                return PendingSignIn::Absent;
            }
            Err(error) => {
                let revoked = revoke_unsaved(hub, credential);
                ui::warning(&format!(
                    "the human approved the sign-in, but its credentials cannot be saved{}: {error:#}",
                    if revoked {
                        "; the Hub session was signed out again"
                    } else {
                        ""
                    }
                ));
                return PendingSignIn::Absent;
            }
        }
        crate::telemetry::account_saved(hub, credential.account_id.as_deref());
    }
    ui::progress(format_args!(
        "finished the sign-in the human approved: signed in as {}",
        ui::bold(&signed_in.who)
    ));
    PendingSignIn::SignedIn
}

/// Whether a sign-in request for the Hub waits for approval. It reads the local record only.
pub(crate) fn is_waiting(hub: &str) -> bool {
    pending::load(hub)
        .ok()
        .flatten()
        .is_some_and(|record| !record.expired())
}

// ──────────────────── browser authorization flow ────────────────────

#[derive(serde::Deserialize)]
struct CliSession {
    state: String,
    url: String,
    expires_in: u64,
}

fn start_browser_handoff(hub: &str) -> CmdResult {
    if let Err(error) = ensure_saveable(|| Unsaveable::BeforeRequest) {
        return Ok(refuse_unsaveable(&error));
    }
    let client = crate::hub::Client::for_hub(hub);
    let session: CliSession =
        remote_request(client.post_public("api/auth/cli/session", &serde_json::json!({})))?;
    Request::record(PendingLogin::new(
        hub,
        Flow::Browser,
        &session.state,
        pending::MIN_INTERVAL,
        session.expires_in,
    ));
    let url = crate::telemetry::acquisition::authorization_url(&session.url, hub);
    crate::telemetry::observe(crate::telemetry::Observation::Authentication(false));
    let message = "Ask the human to open the login link, sign in, and approve CLI access.";
    let complete = ["agit", "login", "--hub", hub, "--complete", &session.state];
    if super::json::is_capturing() {
        println!(
            "{}",
            serde_json::json!({
                "status": "authorization_required",
                "authorization_url": url,
                "expires_in": session.expires_in,
                "message": message,
                "complete_command": complete,
            })
        );
    } else {
        println!("{}", url);
        println!(
            "After the human approves, run: {}",
            complete_command(hub, Some(&session.state))
        );
        println!("The login link expires in {} seconds.", session.expires_in);
    }
    ui::hint(message);
    Ok(ExitCode::Interactive)
}

fn login_browser(hub: &str) -> crate::Result<Option<SignedIn>> {
    ensure_saveable(|| Unsaveable::BeforeRequest)?;
    let client = crate::hub::Client::for_hub(hub);
    let session: CliSession =
        remote_request(client.post_public("api/auth/cli/session", &serde_json::json!({})))?;
    let request = Request::record(PendingLogin::new(
        hub,
        Flow::Browser,
        &session.state,
        pending::MIN_INTERVAL,
        session.expires_in,
    ));

    println!();
    let url = crate::telemetry::acquisition::authorization_url(&session.url, hub);
    println!("  open this link to authorize the CLI:");
    println!("    {}", ui::accent(&url));
    if open_browser(&url) {
        ui::info(format_args!("  {}", ui::dim("(opened in your browser)")));
    }
    wait_for_approval(hub, &request, "  waiting for approval… (ctrl-c to cancel)")
}

// ────────────────────────── device code flow ──────────────────────────

#[derive(serde::Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

fn login_device(hub: &str) -> crate::Result<Option<SignedIn>> {
    ensure_saveable(|| Unsaveable::BeforeRequest)?;
    let client = crate::hub::Client::for_hub(hub);
    let dev: DeviceCode =
        remote_request(client.post_public("api/auth/device/code", &serde_json::json!({})))?;
    let url = crate::telemetry::acquisition::authorization_url(&dev.verification_uri, hub);
    let request = Request::record(
        PendingLogin::new(
            hub,
            Flow::Device,
            &dev.device_code,
            dev.interval,
            dev.expires_in,
        )
        .with_approval(&url, &dev.user_code),
    );

    println!();
    println!("  on any device with a browser, open:");
    println!("    {}", ui::accent(&url));
    println!(
        "  and enter this code:  {}",
        ui::accent(&ui::bold(&dev.user_code))
    );
    wait_for_approval(hub, &request, "  waiting… (ctrl-c to cancel)")
}

/// Wait in this process until the human approves `request` or it expires. A recorded request
/// outlives this process, so the human or agent is told how to finish it if the wait is cut off.
fn wait_for_approval(
    hub: &str,
    request: &Request,
    waiting: &str,
) -> crate::Result<Option<SignedIn>> {
    if request.recorded {
        println!(
            "  if this command is interrupted, finish signing in with `{}`",
            complete_command(hub, None)
        );
    }
    ui::info(waiting);
    let until = Instant::now() + request.record.remaining().min(LONGEST_WAIT);
    match await_approval(hub, request, until, true, None, false)? {
        Some(signed_in) => Ok(Some(signed_in)),
        None => {
            let _lock = pending::claim_lock(hub)?;
            if let Some(credential) = pending::signed_in_by(hub, &request.record.secret) {
                return Ok(Some(SignedIn::new(credential, Origin::SavedElsewhere)));
            }
            request.forget();
            Err(anyhow::Error::new(InteractionRequired(format!(
                "the sign-in request expired before it was approved; run `{}` again",
                login_command(hub)
            ))))
        }
    }
}

/// Poll once. While the human has not approved the request, the Hub answers 202 with a pending
/// status.
fn authorization_response(
    client: &crate::hub::Client,
    path: &str,
    key: &str,
    value: &str,
) -> crate::Result<Option<crate::hub::LoginResponse>> {
    let response: serde_json::Value =
        remote_request(client.post_public(path, &serde_json::json!({ key: value })))?;
    if matches!(
        response.get("status").and_then(|status| status.as_str()),
        Some("pending" | "authorization_pending")
    ) {
        return Ok(None);
    }
    // An invalid response can contain credentials; diagnostics expose its shape, not values.
    remote_request(
        serde_json::from_value(response)
            .map(Some)
            .map_err(|_| anyhow::Error::new(InvalidSession)),
    )
}

#[cfg(any(windows, test))]
fn browser_url_wide(url: &str) -> Option<Vec<u16>> {
    (!url.contains('\0')).then(|| url.encode_utf16().chain([0]).collect())
}

/// Pass URLs directly to the system association API so query separators never reach a shell.
#[cfg(windows)]
fn open_browser(url: &str) -> bool {
    use windows_sys::Win32::{
        System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        },
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };
    let Some(wide) = browser_url_wide(url) else {
        return false;
    };
    let verb: Vec<u16> = "open".encode_utf16().chain([0]).collect();
    unsafe {
        let initialized = CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let result = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        if initialized >= 0 {
            CoUninitialize();
        }
        result as isize > 32
    }
}

/// Open a browser where possible; failing is not fatal because the link is already on screen.
#[cfg(not(windows))]
fn open_browser(url: &str) -> bool {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Debug)]
struct LocalInputFailure;

impl std::fmt::Display for LocalInputFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot read sign-in input")
    }
}

/// Read a PAT from stdin and exchange it for a session at the selected Hub.
fn login_with_token(hub: &str) -> crate::Result<Option<SignedIn>> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|error| anyhow::Error::new(error).context(LocalInputFailure))?;
    let token = buf.trim().to_string();
    if token.is_empty() {
        return crate::input_argument(Err(anyhow::anyhow!(
            "stdin was empty. usage: `agit login --with-token < token.txt`"
        )));
    }
    ensure_saveable(|| Unsaveable::BeforeRequest)?;
    let client = crate::hub::Client::for_hub_with_token(hub, &token);
    let response = remote_request(client.login_with_pat(&token)).map_err(|error| {
        if super::terminal_error_code(&error, ExitCode::Usage) == ExitCode::Auth {
            error.context("the PAT was not accepted")
        } else {
            error
        }
    })?;
    let (credential, _) = session_credential(response);
    Ok(Some(SignedIn::new(credential, Origin::Obtained)))
}

fn session_credential(response: crate::hub::LoginResponse) -> (HubCredential, String) {
    (
        HubCredential {
            account_id: response.account_id,
            username: response.username.clone(),
            email: response.email,
            hub: None,
            access_token: response.access_token,
            access_expires_at: response.access_expires_at,
            refresh_token: response.refresh_token,
            refresh_expires_at: response.refresh_expires_at,
        },
        response.username,
    )
}

#[cfg(test)]
mod identity_tests {
    #[test]
    fn windows_url_buffer_keeps_query_and_fragment_as_one_system_argument() {
        let url = "https://agent-git.com/auth/cli?state=synthetic&installation_id=opaque#fragment";
        let wide = super::browser_url_wide(url).unwrap();
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(String::from_utf16(&wide[..wide.len() - 1]).unwrap(), url);
        assert!(super::browser_url_wide("https://agent-git.com/\0truncated").is_none());
    }

    #[test]
    fn login_keeps_authoritative_account_identity_without_a_username_fallback() {
        for account in [None, Some("account-authoritative")] {
            let response = serde_json::from_value(serde_json::json!({
                "account_id": account,
                "username": "mutable-name",
                "access_token": "synthetic-access",
                "access_expires_at": "2030-01-01T00:00:00Z",
                "refresh_token": "synthetic-refresh",
                "refresh_expires_at": "2030-02-01T00:00:00Z"
            }))
            .unwrap();
            let (credential, username) = super::session_credential(response);
            assert_eq!(credential.account_id.as_deref(), account);
            assert_eq!(username, "mutable-name");
        }
    }
}
