//! `agit whoami` — who am I: the current hub, account, email, and credential expiry.
//!
//! **Zero network** by default (PRD): the credentials are local, and Expiry is readable straight
//! off them. Only `--check` verifies the token (one call to the hub). Not signed in exits 5
//! (Auth) and names the next command.
//!
//! `--check` calls an endpoint that **requires authentication**: the public health endpoint
//! answers 200 to a forged or revoked token just the same and cannot tell a live credential from
//! a dead one.

use super::CmdResult;
use crate::infra::{config, credentials};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Go online to verify the token is still valid.
    #[arg(long)]
    pub check: bool,
}

pub fn run(args: Args, json: bool) -> CmdResult {
    if json {
        return run_json(args);
    }
    run_human(args)
}

fn run_json(args: Args) -> CmdResult {
    let hub = config::hub_url();
    let Some(cred) = credentials::current() else {
        ui::error(&format!("not signed in to {hub}."));
        ui::hint("next: `agit login`");
        return Ok(ExitCode::Auth);
    };

    let mut check = Check::default();
    let mut result_code = ExitCode::Ok;
    if args.check {
        if cred.refresh_expired() {
            ui::error("the refresh token is expired — decidable locally, no round-trip needed.");
            ui::hint("next: sign in again with `agit login`");
            // No request was made, so reachability is unknown rather than false.
            result_code = ExitCode::Auth;
        } else {
            // In JSON mode stdout carries that one document only: the human success line must
            // not print.
            let (c, code) = verify_online(&hub, false);
            check = c;
            result_code = code;
        }
    }

    println!(
        "{}",
        serde_json::to_string(&json_report(&hub, &cred, args.check, &check))?
    );
    Ok(result_code)
}

/// What one online verification learns: whether the hub can be reached, and whether the token is
/// accepted.
#[derive(Default)]
struct Check {
    server_reachable: Option<bool>,
    /// `None` = never asked (unreachable).
    authenticated: Option<bool>,
}

/// Call an endpoint that requires authentication and tell the user the result.
///
/// With `human` off nothing is written to stdout — errors and hints go to stderr and never take
/// stdout; only the success line lands on stdout, and `--json` requires stdout to carry that one
/// document only.
fn verify_online(hub: &str, human: bool) -> (Check, ExitCode) {
    let client = crate::hub::Client::from_env();
    match client.me() {
        Ok(me) => {
            if human {
                ui::success(&format!(
                    "{hub} accepts the credentials — signed in as {}",
                    me.username
                ));
            }
            (
                Check {
                    server_reachable: Some(true),
                    authenticated: Some(true),
                },
                ExitCode::Ok,
            )
        }
        Err(e) => match e.downcast_ref::<crate::hub::client::ApiError>() {
            Some(api) if api.status == 401 || api.status == 403 => {
                ui::error(&format!("{hub} rejects the credentials: {}", api.detail));
                ui::hint("next: sign in again with `agit login`");
                (
                    Check {
                        server_reachable: Some(true),
                        authenticated: Some(false),
                    },
                    ExitCode::Auth,
                )
            }
            Some(api) => {
                ui::error(&format!("{hub} answered with an error: {api}"));
                (
                    Check {
                        server_reachable: Some(true),
                        authenticated: None,
                    },
                    ExitCode::Network,
                )
            }
            None => {
                ui::error(&format!("can’t reach {hub}: {e:#}"));
                ui::hint(
                    "fix the network first — doctor being offline by default exists for exactly these moments",
                );
                (
                    Check {
                        server_reachable: Some(false),
                        authenticated: None,
                    },
                    ExitCode::Network,
                )
            }
        },
    }
}

fn json_report(
    hub: &str,
    cred: &credentials::HubCredential,
    check_requested: bool,
    check: &Check,
) -> serde_json::Value {
    serde_json::json!({
        "hub": hub,
        "account": cred.username,
        "email": cred.email,
        "tokens": {
            "access": {
                "state": if cred.access_expired() { "expired" } else { "valid" },
                "expires_at": cred.access_expires_at,
            },
            "refresh": {
                "state": if cred.refresh_expired() { "expired" } else { "valid" },
                "expires_at": cred.refresh_expires_at,
            },
        },
        "check": {
            "requested": check_requested,
            "server_reachable": check.server_reachable,
            "authenticated": check.authenticated,
        },
    })
}

fn run_human(args: Args) -> CmdResult {
    let hub = config::hub_url();
    let Some(cred) = credentials::current() else {
        ui::error(&format!("not signed in to {hub}."));
        ui::hint("next: `agit login`");
        return Ok(ExitCode::Auth);
    };

    println!("hub   {hub}");
    println!("account  {}", cred.username);
    if let Some(email) = &cred.email {
        println!("email    {email}");
    }
    println!(
        "tokens   access {} · refresh {}",
        state(&cred.access_expires_at, cred.access_expired()),
        state(&cred.refresh_expires_at, cred.refresh_expired()),
    );

    if args.check {
        if cred.refresh_expired() {
            ui::error("the refresh token is expired — decidable locally, no round-trip needed.");
            ui::hint("next: sign in again with `agit login`");
            return Ok(ExitCode::Auth);
        }
        let (_, code) = verify_online(&hub, true);
        return Ok(code);
    }
    Ok(ExitCode::Ok)
}

fn state(expires_at: &str, expired: bool) -> String {
    format!(
        "{} ({})",
        if expired { "expired" } else { "valid" },
        &expires_at[..expires_at.len().min(19)]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_report_uses_named_fields_instead_of_aligned_text() {
        let cred = credentials::HubCredential {
            username: "alice".into(),
            email: Some("alice@example.test".into()),
            hub: None,
            access_token: "secret".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "secret".into(),
            refresh_expires_at: "2099-02-01T00:00:00Z".into(),
        };
        let check = Check {
            server_reachable: Some(true),
            authenticated: Some(true),
        };
        let report = json_report("https://hub.test", &cred, true, &check);
        assert_eq!(report["account"], "alice");
        assert_eq!(report["tokens"]["access"]["state"], "valid");
        assert_eq!(report["check"]["server_reachable"], true);
        assert_eq!(report["check"]["authenticated"], true);
        assert_eq!(report["access_token"], serde_json::Value::Null);
    }

    /// Reachable but rejected: reachability and acceptance are reported separately, so a script
    /// can tell "network" from "credentials".
    #[test]
    fn json_report_separates_reachability_from_acceptance() {
        let cred = credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: None,
            access_token: "secret".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "secret".into(),
            refresh_expires_at: "2099-02-01T00:00:00Z".into(),
        };
        let check = Check {
            server_reachable: Some(true),
            authenticated: Some(false),
        };
        let report = json_report("https://hub.test", &cred, true, &check);
        assert_eq!(report["check"]["server_reachable"], true);
        assert_eq!(report["check"]["authenticated"], false);
    }

    #[test]
    fn json_report_keeps_server_reachability_unknown_without_a_check() {
        let cred = credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: None,
            access_token: "secret".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "secret".into(),
            refresh_expires_at: "2000-01-01T00:00:00Z".into(),
        };
        let report = json_report("https://hub.test", &cred, true, &Check::default());
        assert_eq!(report["check"]["server_reachable"], serde_json::Value::Null);
        assert_eq!(report["check"]["authenticated"], serde_json::Value::Null);
    }
}
