//! `agit logout` — sign out.
//!
//! Revoke the server-side session first, then delete the local credentials; the store is left
//! alone — captured sessions are local assets and signing out must not affect them. `--all` does
//! this once per hub that has been signed in to, and one failure does not hold up the rest.

use super::CmdResult;
use crate::infra::config;
use crate::infra::credentials;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

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
    // Tell the server to revoke the session first, then delete the local credentials.
    //
    // The order matters: the revoke needs the access token, which is gone once the local
    // credentials are deleted. A failure does not block — with the hub unreachable the user still
    // gets to sign out of this machine (the local credentials must be cleared). The cost is that
    // the server-side row stays until it expires, so this says so.
    if credentials::current().is_some()
        && let Err(e) = crate::hub::Client::from_env().logout()
    {
        ui::warning(&format!("server-side revoke failed: {e:#}"));
        ui::hint("local credentials are still deleted; the server session expires on its own");
    }

    if credentials::remove(&hub)? {
        ui::success(&format!("logged out of {hub}"));
        // Say outright that the store is untouched — a user may fear that signing out loses
        // sessions.
        println!(
            "{}",
            ui::dim("  locally captured sessions are unaffected (in $AGIT_HOME/store)")
        );
    } else {
        println!("not logged in to {hub}.");
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
/// The current hub's address is always known, so it is not subject to this limit.
fn run_all() -> CmdResult {
    let all = credentials::all();
    if all.is_empty() {
        println!("no saved credentials.");
        return Ok(ExitCode::Ok);
    }
    let current = config::hub_url();
    let current_key = crate::infra::config::hub_host_key(&current);
    for (host, cred) in &all {
        let Some(cred) = cred else {
            ui::warning(&format!(
                "{host}: this credential file can’t be read, so its server session can’t be revoked from here"
            ));
            ui::hint("the file is still deleted; the server session expires on its own");
            continue;
        };
        let hub = if *host == current_key {
            Some(current.clone())
        } else {
            cred.hub.clone()
        };
        match hub {
            Some(hub) => match crate::hub::Client::for_credential(&hub, cred).logout() {
                Ok(()) => ui::success(&format!("revoked the server session at {hub}")),
                // 401 = the server no longer accepts this token (and refresh cannot trade it
                // back): the session is gone already, so there is nothing to revoke.
                Err(e)
                    if e.downcast_ref::<crate::hub::client::ApiError>()
                        .is_some_and(|api| api.status == 401) =>
                {
                    println!(
                        "{}",
                        ui::dim(&format!("  {hub}: session already expired or revoked"))
                    );
                }
                Err(e) => {
                    ui::warning(&format!("server-side revoke failed for {hub}: {e:#}"));
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
    let n = credentials::remove_all()?;
    ui::success(&format!("removed credentials for {n} hubs"));
    println!(
        "{}",
        ui::dim("  locally captured sessions are unaffected (in $AGIT_HOME/store)")
    );
    Ok(ExitCode::Ok)
}
