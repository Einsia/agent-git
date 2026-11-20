//! `agit logout` — sign out.
//!
//! Revoke the server-side session first, then delete the local credentials that held it; the
//! store is left alone — captured sessions are local assets and signing out must not affect them.
//! `--all` does this once per hub that has been signed in to, and one failure does not hold up
//! the rest.
//!
//! Credentials are deleted only once the Hub has settled their session: it answered the revoke,
//! or it refused both the access token and the refresh token that could renew it. Anything less —
//! no answer, a server error, a refusal that may come from something in front of the Hub — leaves
//! the session possibly valid, so the credentials stay for another try and the sign-out fails.
//! `--local-only` deletes them without asking the Hub, and says the session was not revoked. A
//! deletion matches the tokens the Hub settled, under the lock every save takes: credentials a
//! sign-in or a renewal saves meanwhile hold a session this sign-out did not end, and stay.
//!
//! A sign-in can finish while a sign-out runs. The sign-out first forgets the sign-in requests
//! waiting for approval, so a claim of one either committed before and its credentials are read
//! and revoked here, or it saves nothing and signs its own session out (see
//! [`credentials::pending::commit`]).
//!
//! A device enrolled for remote control keeps its last team activity report on the Hub, and the
//! daemon cannot replace it once the credential is gone. So before revoking, the sign-out sends
//! the Hub an empty report for the device, with the access token as it stands: never renewed and
//! never after the revoke. A device can be enrolled under both daemon namespaces, `rc` and the
//! `desktop-rc` that `agit rc start` uses, each as its own device, so each gets its report. It is
//! best effort: a send that fails, is refused or times out, or an enrollment that cannot be read,
//! is said but changes nothing about the sign-out, and the Hub's own staleness rules remain the
//! backstop. A `--local-only` sign-out sends nothing.

use super::CmdResult;
use crate::hub::client::{ApiError, Remediation, Saving, SignOutRenewal};
use crate::infra::config;
use crate::infra::credentials::{self, HubCredential};
use crate::infra::hub_authority::{HubAuthority, safe_label};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::collections::HashSet;
use std::time::Duration;

/// How long the empty team activity report may hold up a sign-out.
const CLEAR_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(ClapArgs)]
pub struct Args {
    /// Delete credentials for every hub, not just the current one
    #[arg(long)]
    pub all: bool,
    /// Delete this computer's credentials without asking the hub to revoke the session
    #[arg(long)]
    pub local_only: bool,
}

/// What a sign-out established about a Hub session; a JSON reader gets it as `revocation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Revocation {
    /// The Hub answered the revoke.
    Confirmed,
    /// The Hub had ended the session already: it refused the sign-out's access token and the
    /// refresh token that could renew it.
    AlreadyInvalid,
    /// Nothing settled it, so the session may still be valid: no answer, a server error, or a
    /// refusal that does not show the session ended.
    Unconfirmed,
    /// The Hub was not asked: `--local-only`, or credentials that cannot say where or how to ask.
    NotAttempted,
}

/// One Hub's part of a sign-out, as a JSON reader gets it.
#[derive(Debug, serde::Serialize)]
struct HubOutcome {
    /// The Hub as it may be shown; `None` for a credential record that names no Hub.
    hub: Option<String>,
    revocation: Revocation,
    /// Whether the credentials the sign-out found for the Hub are gone from this computer.
    credentials_removed: bool,
}

pub fn run(args: Args) -> CmdResult {
    // A JSON reader gets one value with each Hub's outcome, so the routine lines go to the
    // diagnostics instead of the result.
    let capturing = super::json::is_capturing();
    let (code, hubs) = ui::with_info_on_stderr(capturing, || match (args.all, args.local_only) {
        (false, false) => sign_out_here(),
        (false, true) => delete_here(),
        (true, false) => sign_out_everywhere(),
        (true, true) => delete_everywhere(),
    })?;
    if capturing {
        println!("{}", serde_json::json!({ "hubs": hubs }));
    }
    Ok(code)
}

/// `agit logout`: the current Hub's sign-out.
fn sign_out_here() -> crate::Result<(ExitCode, Vec<HubOutcome>)> {
    let hub = config::hub_url();
    credentials::forget_request(&hub)?;
    let Some(HubSignOut {
        outcome,
        failure,
        signed_in_meanwhile,
    }) = sign_out(&hub)
    else {
        not_signed_in(&hub);
        return Ok((ExitCode::Ok, vec![]));
    };
    // Signed out only when every session found is settled and no sign-in saved meanwhile is left:
    // credentials gone from disk say nothing about the Hub.
    match failure {
        None if signed_in_meanwhile => ui::info(format_args!(
            "revoked the server session this sign-out found at {hub}; the sign-in saved meanwhile stays"
        )),
        None => {
            leaving_device();
            ui::success(&format!("logged out of {hub}"));
            if outcome.revocation == Revocation::AlreadyInvalid {
                ui::info(ui::dim("  the session had already ended on the hub"));
            }
            store_unaffected();
        }
        Some(_) if !outcome.credentials_removed => ui::hint(
            "run `agit logout` again, or `agit logout --local-only` to delete them without revoking",
        ),
        Some(_) => {}
    }
    Ok((failure.unwrap_or(ExitCode::Ok), vec![outcome]))
}

/// `agit logout --local-only`: delete the current Hub's credentials without asking it anything.
fn delete_here() -> crate::Result<(ExitCode, Vec<HubOutcome>)> {
    let hub = config::hub_url();
    let attachments = credentials::bound_records(&hub)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|credential| {
            credential
                .account_id
                .clone()
                .map(|account| (credential, account))
        })
        .map(|(credential, account)| {
            (
                credential,
                crate::rc::input_attachments::snapshot_account(&hub, &account),
            )
        })
        .collect::<Vec<_>>();
    let removed = credentials::remove(&hub)?;
    for (credential, attachment) in attachments {
        if removed
            .bound
            .iter()
            .any(|removed| same_attachment_owner_session(removed, &credential))
        {
            attachment.finish();
        }
    }
    if removed.records == 0 {
        not_signed_in(&hub);
        return Ok((ExitCode::Ok, vec![]));
    }
    leaving_device();
    ui::success(&format!("deleted this computer's credentials for {hub}"));
    not_revoked();
    store_unaffected();
    Ok((
        ExitCode::Ok,
        vec![HubOutcome {
            hub: Some(safe_label(&hub)),
            revocation: Revocation::NotAttempted,
            credentials_removed: true,
        }],
    ))
}

/// `agit logout --all`: the sign-out of every Hub a credential record names, one Hub at a time.
/// The address comes from the `hub` field inside the record: a file name keeps only the host key,
/// which does not reverse into an address. A record that cannot be read or names no Hub cannot
/// have its session revoked, so it stays.
fn sign_out_everywhere() -> crate::Result<(ExitCode, Vec<HubOutcome>)> {
    let forgot = credentials::forget_pending()?;
    let mut outcomes = Vec::new();
    let mut failure = None;
    let mut signed_out = 0;
    let mut meanwhile = false;
    let mut asked = HashSet::new();
    let mut snapshots = Vec::new();
    for (label, cred) in credentials::all_checked()? {
        let Some(hub) = cred
            .and_then(|cred| cred.hub)
            .filter(|hub| HubAuthority::parse(hub).is_ok())
        else {
            ui::warning(&format!(
                "{label}: this credential file can’t be read or doesn’t record the hub address, so its server session can’t be revoked from here; it was kept"
            ));
            failure = combined(failure, Some(ExitCode::Precondition));
            outcomes.push(HubOutcome {
                hub: None,
                revocation: Revocation::NotAttempted,
                credentials_removed: false,
            });
            continue;
        };
        if !asked.insert(HubAuthority::parse(&hub)?.storage_key()) {
            continue;
        }
        let found = credentials::bound_records(&hub);
        snapshots.push((hub, found));
    }
    // Reading every Hub before the first request keeps a login saved while another Hub waits
    // outside the sessions this sign-out revokes. Read errors stay tied to their Hub as well.
    for (hub, found) in snapshots {
        let Some(HubSignOut {
            outcome,
            failure: hub_failure,
            signed_in_meanwhile,
        }) = sign_out_found(&hub, found)
        else {
            continue;
        };
        match (hub_failure, outcome.revocation) {
            (None, Revocation::AlreadyInvalid) => ui::info(ui::dim(&format!(
                "  {}: session already expired or revoked",
                safe_label(&hub)
            ))),
            (None, _) => ui::success(&format!(
                "revoked the server session at {}",
                safe_label(&hub)
            )),
            (Some(_), _) => {}
        }
        if hub_failure.is_none() && !signed_in_meanwhile {
            signed_out += 1;
        }
        meanwhile |= signed_in_meanwhile;
        failure = combined(failure, hub_failure);
        outcomes.push(outcome);
    }
    if outcomes.is_empty() {
        if forgot {
            ui::info("forgot the sign-in requests waiting for approval.");
        }
        ui::info("no saved credentials.");
        return Ok((ExitCode::Ok, outcomes));
    }
    // What the device keeps under an account stays while a sign-in saved meanwhile is here.
    if signed_out > 0 && !meanwhile {
        leaving_device();
        ui::success(&format!("removed credentials for {signed_out} hubs"));
        store_unaffected();
    }
    if outcomes.iter().any(|outcome| !outcome.credentials_removed) {
        ui::hint(
            "run `agit logout --all` again, or `agit logout --all --local-only` to delete the credentials kept without revoking",
        );
    }
    Ok((failure.unwrap_or(ExitCode::Ok), outcomes))
}

/// Hubs that fell short for the same reason exit with its code; for different reasons, with
/// [`ExitCode::Failure`], since no one category describes the result.
fn combined(failure: Option<ExitCode>, other: Option<ExitCode>) -> Option<ExitCode> {
    match (failure, other) {
        (Some(code), Some(other)) if code != other => Some(ExitCode::Failure),
        (failure, other) => failure.or(other),
    }
}

/// `agit logout --all --local-only`: delete every credential record without asking any Hub.
fn delete_everywhere() -> crate::Result<(ExitCode, Vec<HubOutcome>)> {
    let forgot = credentials::forget_pending()?;
    let attachments = credentials::all_checked()?
        .into_iter()
        .filter_map(|(_, credential)| credential)
        .filter_map(|credential| {
            Some((credential.clone(), credential.hub?, credential.account_id?))
        })
        .map(|(credential, hub, account)| {
            (
                credential,
                crate::rc::input_attachments::snapshot_account(&hub, &account),
            )
        })
        .collect::<Vec<_>>();
    let removed = credentials::remove_all()?;
    for (credential, attachment) in attachments {
        if removed
            .bound
            .iter()
            .any(|removed| same_attachment_owner_session(removed, &credential))
        {
            attachment.finish();
        }
    }
    if removed.records == 0 {
        if forgot {
            ui::info("forgot the sign-in requests waiting for approval.");
        }
        ui::info("no saved credentials.");
        return Ok((ExitCode::Ok, vec![]));
    }
    leaving_device();
    ui::success("deleted every credential saved on this computer");
    not_revoked();
    store_unaffected();
    let mut outcomes: Vec<HubOutcome> = Vec::new();
    let hubs = removed
        .bound
        .iter()
        .map(|cred| cred.hub.as_deref().map(safe_label));
    let unbound = removed.records.saturating_sub(removed.bound.len());
    for hub in hubs.chain(std::iter::repeat_n(None, unbound)) {
        if hub.is_some() && outcomes.iter().any(|known| known.hub == hub) {
            continue;
        }
        outcomes.push(HubOutcome {
            hub,
            revocation: Revocation::NotAttempted,
            credentials_removed: true,
        });
    }
    Ok((ExitCode::Ok, outcomes))
}

/// One Hub's sign-out (see [`sign_out`]).
struct HubSignOut {
    outcome: HubOutcome,
    /// When it fell short, the code to exit with.
    failure: Option<ExitCode>,
    /// Whether a sign-in saved while it ran is still on this computer: the account is still
    /// signed in here, so nothing that leaves with its sign-in goes.
    signed_in_meanwhile: bool,
}

/// Sign out of `hub`: revoke each session the credential records bound to it held when the
/// sign-out began, from the pair read then, and delete the records holding a session once the Hub
/// has settled it. A record a sign-in saves meanwhile holds another session: it is left alone, and
/// it settles nothing about the ones found. The first session the Hub leaves unsettled stops it,
/// and stays with the sessions not reached. `None` when nothing is saved for `hub`. What fell
/// short is said here, success is left to the caller.
fn sign_out(hub: &str) -> Option<HubSignOut> {
    sign_out_found(hub, credentials::bound_records(hub))
}

fn sign_out_found(hub: &str, found: crate::Result<Vec<HubCredential>>) -> Option<HubSignOut> {
    let label = safe_label(hub);
    let found = match found {
        Ok(found) if found.is_empty() => return None,
        Ok(found) => found,
        Err(e) => {
            ui::warning(&format!(
                "{label}: the saved credentials can’t be read ({e:#}), so the server session can’t be revoked from here; they were kept"
            ));
            return Some(HubSignOut {
                outcome: HubOutcome {
                    hub: Some(label),
                    revocation: Revocation::NotAttempted,
                    credentials_removed: false,
                },
                failure: Some(super::terminal_error_code(&e, ExitCode::Precondition)),
                signed_in_meanwhile: false,
            });
        }
    };
    // The pairs this sign-out found, and those its renewals got in their place: a renewed pair
    // holds the same session, so it is no more settled than the one it replaced.
    let mut attachments = found
        .iter()
        .filter_map(|credential| {
            Some((
                credential.account_id.clone()?,
                crate::rc::input_attachments::snapshot_account(
                    hub,
                    credential.account_id.as_deref()?,
                ),
            ))
        })
        .collect::<Vec<_>>();
    let mut removed_accounts = HashSet::new();
    let mut ours = found.clone();
    let mut revocation = Revocation::NotAttempted;
    let mut failure = None;
    'found: for (turn, cred) in found.iter().enumerate() {
        if turn == 0 {
            clear_activity(hub, cred);
        }
        let (session, result) = revoke(hub, cred);
        if !holds_one_of(&ours, &session) {
            ours.push(session.clone());
        }
        let settled = match result {
            Ok(settled) => settled,
            Err(Unsettled { remote, unsaved }) => {
                ui::warning(&format!("{label} didn’t confirm the sign-out: {remote:#}"));
                if let Some(e) = &unsaved {
                    ui::warning(&format!(
                        "{label}: the renewed sign-in couldn’t be saved on this computer: {e:#}"
                    ));
                }
                let kept = credentials::bound_records(hub).map(|now| {
                    now.iter()
                        .any(|saved| credentials::same_session(saved, &session))
                });
                match &kept {
                    Ok(true) => ui::hint(
                        "the server session may still be valid, so this computer’s credentials for it were kept",
                    ),
                    Ok(false) => ui::hint(
                        "the server session may still be valid, and this computer no longer holds credentials to revoke it with; it ends when it expires",
                    ),
                    Err(e) => {
                        ui::warning(&format!(
                            "{label}: couldn’t tell whether this computer still holds credentials for that session: {e:#}"
                        ));
                        ui::hint("the server session may still be valid");
                    }
                }
                revocation = Revocation::Unconfirmed;
                failure = Some(match (&unsaved, &kept) {
                    (Some(e), _) | (None, Err(e)) => {
                        super::terminal_error_code(e, ExitCode::Failure)
                    }
                    (None, Ok(_)) => super::terminal_error_code(&remote, ExitCode::Network),
                });
                break;
            }
        };
        if revocation != Revocation::Confirmed {
            revocation = settled;
        }
        // The found pair goes with its session: a renewal that got the settled pair spent it.
        for pair in [cred, &session] {
            match credentials::remove_session(hub, pair) {
                Ok(count) if count > 0 => {
                    if let Some(account) = &pair.account_id {
                        removed_accounts.insert(account.clone());
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    ui::warning(&format!("{label}: couldn’t delete the credentials: {e:#}"));
                    failure = Some(super::terminal_error_code(&e, ExitCode::Failure));
                    break 'found;
                }
            }
        }
    }
    let mut signed_in_meanwhile = false;
    let credentials_removed = match credentials::bound_records(hub) {
        Ok(left) => {
            signed_in_meanwhile = left.iter().any(|cred| !holds_one_of(&ours, cred));
            if signed_in_meanwhile {
                ui::warning(&format!(
                    "{label}: a sign-in saved during this sign-out is still on this computer"
                ));
            }
            !left.iter().any(|cred| holds_one_of(&ours, cred))
        }
        Err(e) => {
            if failure.is_none() {
                ui::warning(&format!(
                    "{label}: couldn’t read the saved credentials to confirm they’re gone: {e:#}"
                ));
                failure = Some(super::terminal_error_code(&e, ExitCode::Failure));
            }
            false
        }
    };
    if !credentials_removed && failure.is_none() {
        ui::warning(&format!(
            "{label}: saved credentials whose server session wasn’t settled were kept"
        ));
        failure = Some(ExitCode::Precondition);
    }
    if credentials_removed && failure.is_none() {
        for (account, attachment) in attachments.drain(..) {
            if removed_accounts.contains(&account) {
                attachment.finish();
            }
        }
    }
    Some(HubSignOut {
        outcome: HubOutcome {
            hub: Some(label),
            revocation,
            credentials_removed,
        },
        failure,
        signed_in_meanwhile,
    })
}

/// Token equality alone cannot identify the account whose attachment snapshot may retire.
fn same_attachment_owner_session(left: &HubCredential, right: &HubCredential) -> bool {
    left.account_id
        .as_ref()
        .is_some_and(|account| !account.is_empty())
        && left.account_id == right.account_id
        && left
            .hub
            .as_deref()
            .and_then(|hub| HubAuthority::parse(hub).ok())
            .zip(
                right
                    .hub
                    .as_deref()
                    .and_then(|hub| HubAuthority::parse(hub).ok()),
            )
            .is_some_and(|(left, right)| left == right)
        && credentials::same_session(left, right)
}

/// Whether `cred` holds one of `sessions`.
fn holds_one_of(sessions: &[HubCredential], cred: &HubCredential) -> bool {
    sessions
        .iter()
        .any(|session| credentials::same_session(session, cred))
}

/// Why a sign-out did not settle a session.
struct Unsettled {
    /// What the Hub answered, or why nothing came.
    remote: anyhow::Error,
    /// Why the pair the sign-out's renewal got couldn't be saved, when that failed: nothing on
    /// this computer can then try that session again.
    unsaved: Option<anyhow::Error>,
}

impl From<anyhow::Error> for Unsettled {
    fn from(remote: anyhow::Error) -> Self {
        Self {
            remote,
            unsaved: None,
        }
    }
}

/// Ask `hub` to revoke `cred`'s session with its access token as it stands, and when that is
/// refused, renew the pair with its own refresh token and revoke with the renewed one (see
/// [`crate::hub::Client::renew_for_sign_out`]). Neither request renews on its own. Returns the
/// pair the attempt ended with — `cred`, or the one its renewal got — and how the Hub settled its
/// session, or why it did not.
///
/// The session ended already only when the Hub itself refused the access token and refused the
/// refresh token that could renew it: no holder of the pair can use it again. Each refusal is
/// judged by its own answer, so a refusal of the revoke from something in front of the Hub stays
/// unsettled whatever the renewal gets.
fn revoke(hub: &str, cred: &HubCredential) -> (HubCredential, Result<Revocation, Unsettled>) {
    let refusal = match crate::hub::Client::for_hub_with_token(hub, &cred.access_token).logout() {
        Ok(()) => return (cred.clone(), Ok(Revocation::Confirmed)),
        Err(e) if unauthorized(&e) => e,
        Err(e) => return (cred.clone(), Err(e.into())),
    };
    match crate::hub::Client::for_credential(hub, cred).renew_for_sign_out() {
        SignOutRenewal::Renewed { pair, saving } => {
            let revoked = crate::hub::Client::for_hub_with_token(hub, &pair.access_token)
                .logout()
                .map(|()| Revocation::Confirmed)
                .map_err(|remote| Unsettled {
                    remote,
                    unsaved: match saving {
                        Saving::Failed(e) => Some(e),
                        Saving::Saved | Saving::Replaced => None,
                    },
                });
            (pair, revoked)
        }
        SignOutRenewal::Refused if refused_by_hub(&refusal) => {
            (cred.clone(), Ok(Revocation::AlreadyInvalid))
        }
        SignOutRenewal::Refused => (cred.clone(), Err(refusal.into())),
        SignOutRenewal::Failed(e) => (cred.clone(), Err(e.into())),
    }
}

/// Whether a request was refused as unauthenticated, by the Hub or by something in front of it.
fn unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiError>()
        .is_some_and(|api| api.status == 401)
}

/// Whether the Hub itself refused a request's credentials: a 401 that asks for a new sign-in. A
/// 401 without that may come from something in front of the Hub.
fn refused_by_hub(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ApiError>().is_some_and(|api| {
        api.status == 401 && api.remedies().contains(&Remediation::Authenticate {})
    })
}

/// Send `hub` an empty team activity report for each of this device's enrollments there, in
/// every daemon namespace, owned by `cred`'s account, so the team stops seeing their live
/// sessions. Each report lists no session and is sent with `cred`'s access token as it stands.
/// One that cannot be sent, or an enrollment that cannot be read, is said and left to the Hub.
fn clear_activity(hub: &str, cred: &HubCredential) {
    let mut cleared = HashSet::new();
    for (namespace, enrollment) in crate::rc::cloud::store::enrollments_everywhere(hub) {
        let result = enrollment.and_then(|enrollment| {
            let device = enrollment.credential.device;
            if cred
                .account_id
                .as_deref()
                .is_some_and(|account| account != device.owner.account_id)
                || !cleared.insert(device.id.clone())
            {
                return Ok(());
            }
            crate::hub::Client::for_hub_with_token_and_timeout(
                hub,
                &cred.access_token,
                CLEAR_ACTIVITY_TIMEOUT,
            )
            .report_device_activity(&device.id, &[])
            .map(drop)
        });
        if let Err(e) = result {
            ui::warning(&format!(
                "couldn't clear this device's live sessions ({namespace}) from Team activity at {}: {e:#}",
                safe_label(hub)
            ));
            ui::hint("signing out anyway; the Hub marks the device stale within 3 minutes");
        }
    }
}

/// What this device keeps under an account leaves it with the account's credentials: records of
/// messages delivered under it, and Codex's copies of memory made under it.
fn leaving_device() {
    crate::rc::step_delivery::forget_all();
    crate::rc::waiting_messages::forget_all();
    crate::commands::memory::forget_codex_mirrors();
}

/// Says outright that the store is untouched — a user may fear that signing out loses sessions.
fn store_unaffected() {
    ui::info(ui::dim(
        "  locally captured sessions are unaffected (in $AGIT_HOME/store)",
    ));
}

fn not_revoked() {
    ui::warning(
        "the hub was not asked to revoke the session, so it may stay valid there until it expires",
    );
}

fn not_signed_in(hub: &str) {
    ui::info(format_args!("not logged in to {hub}."));
    let others = credentials::logged_in_hosts();
    if !others.is_empty() {
        ui::hint(&format!("logged-in hubs: {}", others.join(", ")));
        ui::hint("use --all to log out of everything");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::workspaces::test_hub::TestHome;
    use agit_peer::access::Principal;
    use agit_peer::cloud::{Device, DeviceCredential, Secret};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "access-token";

    /// A request as the fake Hub saw it: method and target, bearer, body.
    type Seen = (String, Option<String>, String);

    /// A Hub that answers each request with the status and JSON body `answer` gives for its method
    /// and target and its authorization header, and records it, a JSON body in compact form.
    /// Version probes are neither answered by `answer` nor recorded.
    fn scripted_hub(
        answer: impl Fn(&str, Option<&str>) -> (u16, String) + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Seen>>> = Default::default();
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request = line
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                let (mut length, mut bearer) = (0, None);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap_or((header, ""));
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap(),
                        "authorization" => bearer = Some(value.trim().to_owned()),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (status, answer) = if request.contains("/api/cli/version") {
                    (404, String::new())
                } else {
                    let answer = answer(&request, bearer.as_deref());
                    record.lock().unwrap().push((
                        request,
                        bearer,
                        serde_json::from_slice::<serde_json::Value>(&body).map_or_else(
                            |_| String::from_utf8_lossy(&body).into_owned(),
                            |body| body.to_string(),
                        ),
                    ));
                    answer
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        (hub, seen)
    }

    /// A Hub that answers every request as it answers a report, which a revoke also accepts.
    fn fake_hub() -> (String, Arc<Mutex<Vec<Seen>>>) {
        scripted_hub(|_, _| (200, r#"{"next_report_seconds":600}"#.to_owned()))
    }

    fn credential(hub: &str) -> HubCredential {
        HubCredential {
            account_id: Some("owner".into()),
            username: "owner".into(),
            email: None,
            hub: Some(hub.into()),
            access_token: TOKEN.into(),
            access_expires_at: "2999-01-01T00:00:00Z".into(),
            refresh_token: "refresh-token".into(),
            refresh_expires_at: "2999-01-01T00:00:00Z".into(),
        }
    }

    /// Enroll this device on `hub` as `id`, owned by `account`, under the daemon `namespace`.
    fn enroll(hub: &str, namespace: &str, id: &str, account: &str) {
        let identity = agit_peer::Identity::generate().unwrap();
        let device = Device {
            id: id.into(),
            owner: Principal {
                issuer: hub.into(),
                account_id: account.into(),
            },
            machine_id: "machine".into(),
            display_name: "Fixture".into(),
            certificate: identity.certificate().clone(),
            credential_epoch: 1,
        };
        crate::rc::cloud::store::save_in(
            &crate::rc::state_dir(namespace).unwrap(),
            &crate::rc::cloud::store::Enrollment {
                identity,
                inbound_enabled: true,
                credential: DeviceCredential {
                    device,
                    token: Secret::new("fixture-device-token".into()),
                },
            },
        )
        .unwrap();
    }

    /// Pins the order and content of a sign-out's requests for an enrolled device: first an
    /// empty team activity report for each enrollment of the device, with the access token
    /// about to be revoked, then the revoke, and nothing after it. A device enrolled by
    /// `agit rc start`, under `desktop-rc` alone, gets its report whichever namespace this
    /// process selected; one enrolled under both gets one per namespace. Each body lists no
    /// session and carries no other key. A device enrolled under another account gets no
    /// report, and the revoke still runs.
    #[test]
    fn sign_out_empties_the_devices_activity_report_before_revoking() {
        let (hub, seen) = fake_hub();
        let _home = TestHome::new();
        let bearer = Some(format!("Bearer {TOKEN}"));
        let cleared = |device: &str| {
            (
                format!("PUT /api/peer/devices/{device}/activity"),
                bearer.clone(),
                r#"{"sessions":[]}"#.to_owned(),
            )
        };
        let revoked = (
            "POST /api/auth/logout".to_owned(),
            bearer.clone(),
            "{}".to_owned(),
        );
        let saved_and_signed_out = || {
            credentials::save(&hub, &credential(&hub)).unwrap();
            let HubSignOut {
                outcome, failure, ..
            } = sign_out(&hub).unwrap();
            assert_eq!(outcome.revocation, Revocation::Confirmed);
            assert!(outcome.credentials_removed && failure.is_none());
        };

        enroll(&hub, "desktop-rc", "desktop", "owner");
        saved_and_signed_out();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![cleared("desktop"), revoked.clone()]
        );

        seen.lock().unwrap().clear();
        enroll(&hub, "rc", "shared", "owner");
        saved_and_signed_out();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![cleared("shared"), cleared("desktop"), revoked]
        );

        seen.lock().unwrap().clear();
        enroll(&hub, "rc", "shared", "someone-else");
        enroll(&hub, "desktop-rc", "desktop", "someone-else");
        saved_and_signed_out();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, "POST /api/auth/logout");
    }

    /// A Hub that cannot be reached holds up neither the empty report nor the revoke that
    /// follows: both fail at once. The session may still be valid there, so the credential stays
    /// for another try and the sign-out fails as a network failure.
    #[test]
    fn an_unreachable_hub_keeps_the_credential_without_holding_up_the_sign_out() {
        let hub = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let _home = TestHome::new();
        enroll(&hub, "desktop-rc", "desktop", "owner");
        credentials::save(&hub, &credential(&hub)).unwrap();
        let started = std::time::Instant::now();
        let HubSignOut {
            outcome, failure, ..
        } = sign_out(&hub).unwrap();
        assert!(started.elapsed() < CLEAR_ACTIVITY_TIMEOUT);
        assert_eq!(
            (outcome.revocation, outcome.credentials_removed, failure),
            (Revocation::Unconfirmed, false, Some(ExitCode::Network))
        );
        let kept = credentials::load(&hub).unwrap();
        assert!(credentials::same_session(&kept, &credential(&hub)));
    }

    /// A credential goes only once the Hub has settled its session. A refused access token is
    /// renewed first with the credential's own refresh token, even when this computer's clock
    /// counts it as expired, and the pair the renewal saved is the one revoked and deleted. The
    /// session ended already only when the Hub itself refused both the revoke and the refresh
    /// token, now or before; a refresh token refused before is not offered again. Nothing else
    /// settles it: a 401 that does not ask for a new sign-in may come from something in front of
    /// the Hub, whether it answers the revoke or the renewal, and a Hub that refuses the renewed
    /// pair as well has not shown the session ended. Such a credential stays and the sign-out
    /// fails. A sign-out that took any 401 as the end, let the renewal's refusal stand in for the
    /// revoke's, or judged the refresh token by its expiry here, would delete a session that may
    /// still be valid. A renewed pair this computer could not save fails the sign-out as this
    /// computer's failure; reported as the Hub's, it would hide that nothing here can try again.
    /// Saved credentials that can't be read after a refused revoke leave it unknown whether they
    /// are kept: the sign-out fails with that local failure instead of saying they are or aren't.
    #[test]
    fn a_credential_goes_only_once_the_hub_settles_its_session() {
        let original = credential("https://synthetic.invalid");
        let canonical = HubCredential {
            hub: Some("https://SYNTHETIC.invalid/".into()),
            ..original.clone()
        };
        assert!(same_attachment_owner_session(&original, &canonical));
        let empty = HubCredential {
            account_id: Some(String::new()),
            ..original.clone()
        };
        assert!(!same_attachment_owner_session(&empty, &empty));
        for unrelated in [
            HubCredential {
                account_id: Some("another-owner".into()),
                ..original.clone()
            },
            HubCredential {
                hub: Some("https://another.invalid".into()),
                ..original.clone()
            },
            HubCredential {
                hub: Some("https://synthetic.invalid:443".into()),
                ..original.clone()
            },
            HubCredential {
                account_id: None,
                ..original.clone()
            },
        ] {
            assert!(credentials::same_session(&original, &unrelated));
            assert!(
                !same_attachment_owner_session(&original, &unrelated),
                "a matching pair cannot retire another Hub or account's snapshot"
            );
        }
        const REFUSED: &str =
            r#"{"error":"sign in again","kind":"unauthorized","fix":[{"kind":"authenticate"}]}"#;
        const GATEWAY: &str = r#"{"error":"synthetic gateway refusal"}"#;
        const RENEWED: &str = r#"{"access_token":"renewed-access","access_expires_at":"2999-01-01T00:00:00Z","refresh_token":"renewed-refresh","refresh_expires_at":"2999-01-01T00:00:00Z"}"#;
        let kept = (Revocation::Unconfirmed, false, Some(ExitCode::Auth));
        // The answer to the first revoke, to a revoke with the renewed pair, and to the renewal,
        // and whether the Hub refused the refresh token before.
        for (first, renewed, refresh, refused_before, settled) in [
            (
                REFUSED,
                200,
                (200, RENEWED),
                false,
                (Revocation::Confirmed, true, None),
            ),
            (
                REFUSED,
                200,
                (401, REFUSED),
                false,
                (Revocation::AlreadyInvalid, true, None),
            ),
            (REFUSED, 200, (401, GATEWAY), false, kept),
            (REFUSED, 401, (200, RENEWED), false, kept),
            (
                REFUSED,
                200,
                (200, RENEWED),
                true,
                (Revocation::AlreadyInvalid, true, None),
            ),
            (GATEWAY, 200, (200, RENEWED), true, kept),
            (GATEWAY, 200, (401, REFUSED), false, kept),
        ] {
            let (hub, seen) = scripted_hub(move |request, bearer| match (request, bearer) {
                ("POST /api/auth/logout", Some("Bearer renewed-access")) if renewed == 200 => {
                    (200, "{}".into())
                }
                ("POST /api/auth/logout", Some("Bearer renewed-access")) => (401, REFUSED.into()),
                ("POST /api/auth/logout", _) => (401, first.into()),
                ("POST /api/auth/refresh", None) => (refresh.0, refresh.1.into()),
                _ => (404, "{}".into()),
            });
            let _home = TestHome::new();
            let expired = HubCredential {
                refresh_expires_at: "2000-01-01T00:00:00Z".into(),
                ..credential(&hub)
            };
            credentials::save(&hub, &expired).unwrap();
            if refused_before {
                credentials::record_refused(&hub, &expired).unwrap();
            }
            let HubSignOut {
                outcome, failure, ..
            } = sign_out(&hub).unwrap();
            assert_eq!(
                (outcome.revocation, outcome.credentials_removed, failure),
                settled
            );
            assert_eq!(credentials::load(&hub).is_none(), settled.1);
            let requests: Vec<_> = seen.lock().unwrap().iter().map(|r| r.0.clone()).collect();
            assert_eq!(
                requests.contains(&"POST /api/auth/refresh".to_owned()),
                !refused_before,
                "{requests:?}"
            );
        }
        // The renewed pair's revoke is refused, and then the saved credentials can't be read (a
        // folder has taken the credential's place): whether this computer still holds them is
        // unknown, so the sign-out says so and fails with that local failure, not the Hub's.
        {
            let _home = TestHome::new();
            let address = Arc::new(std::sync::OnceLock::<String>::new());
            let known = address.clone();
            let (hub, _) = scripted_hub(move |request, bearer| match (request, bearer) {
                ("POST /api/auth/logout", Some("Bearer renewed-access")) => {
                    let canonical =
                        crate::infra::config::credentials_path(known.get().unwrap()).unwrap();
                    std::fs::remove_file(&canonical).unwrap();
                    std::fs::create_dir(&canonical).unwrap();
                    (401, REFUSED.into())
                }
                ("POST /api/auth/logout", _) => (401, REFUSED.into()),
                ("POST /api/auth/refresh", None) => (200, RENEWED.into()),
                _ => (404, "{}".into()),
            });
            address.set(hub.clone()).unwrap();
            credentials::save(&hub, &credential(&hub)).unwrap();
            let HubSignOut {
                outcome, failure, ..
            } = sign_out(&hub).unwrap();
            assert!(
                crate::infra::config::credentials_path(&hub)
                    .unwrap()
                    .is_dir(),
                "the renewed pair's revoke never came"
            );
            assert_eq!(
                (outcome.revocation, outcome.credentials_removed, failure),
                (Revocation::Unconfirmed, false, Some(ExitCode::Failure))
            );
        }
        // A renewed pair this computer can't save: when the Hub answers the renewal, a folder has
        // taken the credential lock's place, and the renewed pair's revoke is refused too. The
        // failure is this computer's, so the sign-out says so rather than a Hub's failure.
        {
            let _home = TestHome::new();
            let lock = crate::infra::config::credentials_dir()
                .unwrap()
                .with_extension("lock");
            let blocked = lock.clone();
            let (hub, _) = scripted_hub(move |request, _| match request {
                "POST /api/auth/refresh" => {
                    let _ = std::fs::remove_file(&blocked);
                    std::fs::create_dir(&blocked).unwrap();
                    (200, RENEWED.into())
                }
                "POST /api/auth/logout" => (401, REFUSED.into()),
                _ => (404, "{}".into()),
            });
            credentials::save(&hub, &credential(&hub)).unwrap();
            let HubSignOut {
                outcome, failure, ..
            } = sign_out(&hub).unwrap();
            assert!(lock.is_dir(), "the renewal never came");
            std::fs::remove_dir(&lock).unwrap();
            assert_eq!(
                (outcome.revocation, outcome.credentials_removed),
                (Revocation::Unconfirmed, false)
            );
            // A local failure, whichever category the platform's refusal to open a folder as the
            // lock file falls in; never the Hub's.
            assert!(
                matches!(failure, Some(ExitCode::Failure | ExitCode::Precondition)),
                "{failure:?}"
            );
        }
    }
}
