//! `agit logout` — sign out.
//!
//! Revoke the server-side session first, then delete the local credentials; the store is left
//! alone — captured sessions are local assets and signing out must not affect them. `--all` does
//! this once per hub that has been signed in to, and one failure does not hold up the rest.
//!
//! A sign-in can finish while a sign-out runs. The sign-out first forgets the sign-in requests
//! waiting for approval, so a claim of one either committed before and its credentials are read
//! and revoked here, or it saves nothing and signs its own session out (see
//! [`credentials::pending::commit`]). Credentials another sign-in saves after they were read are
//! among those the removal returns, and are revoked then, so no session a sign-out removes
//! outlives it on the Hub.

use super::CmdResult;
use crate::infra::config;
use crate::infra::credentials::{self, HubCredential};
use crate::infra::hub_authority::{HubAuthority, safe_label};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::collections::HashSet;

#[derive(ClapArgs)]
pub struct Args {
    /// Delete credentials for every hub, not just the current one
    #[arg(long)]
    pub all: bool,
}

pub fn run(args: Args) -> CmdResult {
    if args.all {
        return run_all();
    }

    let hub = config::hub_url();
    credentials::forget_request(&hub)?;
    // Tell the server to revoke the session first, then delete the local credentials.
    //
    // The order matters: the revoke needs the access token, which is gone once the local
    // credentials are deleted. A failure does not block — with the hub unreachable the user still
    // gets to sign out of this machine (the local credentials must be cleared). The cost is that
    // the server-side row stays until it expires, so this says so.
    let mut attempted = HashSet::new();
    if let Some(cred) = credentials::load(&hub)
        && let Err(e) = revoke(&hub, &cred, &mut attempted)
    {
        ui::warning(&format!("server-side revoke failed: {e:#}"));
        ui::hint("local credentials are still deleted; the server session expires on its own");
    }

    let removed = credentials::remove(&hub)?;
    revoke_saved_meanwhile(&removed, &mut attempted, false);
    if removed.records > 0 {
        ui::success(&format!("logged out of {hub}"));
        // Say outright that the store is untouched — a user may fear that signing out loses
        // sessions.
        ui::info(ui::dim(
            "  locally captured sessions are unaffected (in $AGIT_HOME/store)",
        ));
    } else {
        ui::info(format_args!("not logged in to {hub}."));
        let others = credentials::logged_in_hosts();
        if !others.is_empty() {
            ui::hint(&format!("logged-in hubs: {}", others.join(", ")));
            ui::hint("use --all to log out of everything");
        }
    }
    Ok(ExitCode::Ok)
}

/// `--all`: revoke the server-side session hub by hub, then clear the local credentials in one
/// pass.
///
/// A credential file name keeps only the host key, which does not reverse into an address; the
/// address comes from the `hub` field inside the credential. A credential file without that field
/// gets its local half deleted only, and says plainly that the server-side row expires on its own.
fn run_all() -> CmdResult {
    let forgot = credentials::forget_pending()?;
    let all = credentials::all_checked()?;
    let mut attempted = HashSet::new();
    for (host, cred) in &all {
        let Some(cred) = cred else {
            ui::warning(&format!(
                "{host}: this credential file can’t be read, so its server session can’t be revoked from here"
            ));
            ui::hint("the file is still deleted; the server session expires on its own");
            continue;
        };
        let hub = cred
            .hub
            .as_deref()
            .filter(|hub| HubAuthority::parse(hub).is_ok());
        match hub {
            Some(hub) => match revoke(hub, cred, &mut attempted) {
                Ok(()) => ui::success(&format!(
                    "revoked the server session at {}",
                    safe_label(hub)
                )),
                Err(e) if unauthorized(&e) => {
                    ui::info(ui::dim(&format!(
                        "  {}: session already expired or revoked",
                        safe_label(hub)
                    )));
                }
                Err(e) => {
                    ui::warning(&format!(
                        "server-side revoke failed for {}: {e:#}",
                        safe_label(hub)
                    ));
                    ui::hint(
                        "local credentials are still deleted; that server session expires on its own",
                    );
                }
            },
            None => {
                ui::warning(&format!(
                    "{host}: this credential file doesn’t record the hub address, so the server session can’t be revoked from here"
                ));
                ui::hint(
                    "it expires on its own; sign in and out once more if you need it gone now",
                );
            }
        }
    }
    let removed = credentials::remove_all()?;
    revoke_saved_meanwhile(&removed, &mut attempted, true);
    if all.is_empty() && removed.records == 0 {
        if forgot {
            ui::info("forgot the sign-in requests waiting for approval.");
        }
        ui::info("no saved credentials.");
        return Ok(ExitCode::Ok);
    }
    ui::success(&format!("removed credentials for {} hubs", removed.records));
    ui::info(ui::dim(
        "  locally captured sessions are unaffected (in $AGIT_HOME/store)",
    ));
    Ok(ExitCode::Ok)
}

/// Revoke `cred`'s session at `hub`, renewing an expired access token first, and note every
/// access token the attempt used, renewed or not, as one this sign-out already revoked.
fn revoke(hub: &str, cred: &HubCredential, attempted: &mut HashSet<String>) -> crate::Result<()> {
    let client = crate::hub::Client::for_credential(hub, cred);
    let result = client.logout();
    attempted.insert(cred.access_token.clone());
    attempted.extend(client.credential_snapshot().map(|used| used.access_token));
    result
}

/// Revoke the sessions of removed credentials that no revoke of this sign-out attempted: a
/// sign-in saved them after the sign-out read the credentials. Each is signed out at its own Hub
/// with its own access token and is never renewed, since a renewal would have to save its result.
/// A session the Hub no longer accepts is already gone.
fn revoke_saved_meanwhile(
    removed: &credentials::Removed,
    attempted: &mut HashSet<String>,
    announce: bool,
) {
    for cred in &removed.bound {
        let Some(hub) = cred.hub.as_deref() else {
            continue;
        };
        if !attempted.insert(cred.access_token.clone()) {
            continue;
        }
        match crate::hub::Client::for_hub_with_token(hub, &cred.access_token).logout() {
            Ok(()) if announce => ui::success(&format!(
                "revoked the server session at {}",
                safe_label(hub)
            )),
            Ok(()) => {}
            Err(e) if unauthorized(&e) => {}
            Err(e) => {
                ui::warning(&format!(
                    "server-side revoke failed for {}: {e:#}",
                    safe_label(hub)
                ));
                ui::hint(
                    "local credentials are still deleted; that server session expires on its own",
                );
            }
        }
    }
}

/// 401 = the server no longer accepts this token (and refresh cannot trade it back): the session
/// is gone already, so there is nothing to revoke.
fn unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::hub::client::ApiError>()
        .is_some_and(|api| api.status == 401)
}
