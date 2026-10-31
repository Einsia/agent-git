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
//!
//! A device enrolled for remote control keeps its last team activity report on the Hub, and the
//! daemon cannot replace it once the credential is gone. So before revoking, the sign-out sends
//! the Hub an empty report for the device, with the access token as it stands: never renewed and
//! never after the revoke. It is best effort: a send that fails, is refused or times out changes
//! nothing about the sign-out, and the Hub's own staleness rules remain the backstop.

use super::CmdResult;
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
/// access token the attempt used, renewed or not, as one this sign-out already revoked. The
/// device's team activity is cleared first, while the token still works ([`clear_activity`]).
fn revoke(hub: &str, cred: &HubCredential, attempted: &mut HashSet<String>) -> crate::Result<()> {
    clear_activity(hub, cred);
    let client = crate::hub::Client::for_credential(hub, cred);
    let result = client.logout();
    attempted.insert(cred.access_token.clone());
    attempted.extend(client.credential_snapshot().map(|used| used.access_token));
    result
}

/// Send `hub` an empty team activity report for this device's enrollment there, if it has one
/// owned by `cred`'s account, so the team stops seeing its live sessions. The report lists no
/// session and is sent with `cred`'s access token as it stands; any failure is ignored.
fn clear_activity(hub: &str, cred: &HubCredential) {
    let Ok(Some(enrollment)) = crate::rc::cloud::store::load(hub) else {
        return;
    };
    let device = &enrollment.credential.device;
    if cred
        .account_id
        .as_deref()
        .is_some_and(|account| account != device.owner.account_id)
    {
        return;
    }
    let _ = crate::hub::Client::for_hub_with_token_and_timeout(
        hub,
        &cred.access_token,
        CLEAR_ACTIVITY_TIMEOUT,
    )
    .report_device_activity(&device.id, &[]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::access::Principal;
    use agit_peer::cloud::{Device, DeviceCredential, Secret};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "access-token";

    /// A request as the fake Hub saw it: method and target, bearer, body.
    type Seen = (String, Option<String>, String);

    /// A Hub that answers every request with `{}` and records it, a JSON body in compact form,
    /// skipping version probes.
    fn fake_hub() -> (String, Arc<Mutex<Vec<Seen>>>) {
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
                if !request.contains("/api/cli/version") {
                    record.lock().unwrap().push((
                        request,
                        bearer,
                        serde_json::from_slice::<serde_json::Value>(&body).map_or_else(
                            |_| String::from_utf8_lossy(&body).into_owned(),
                            |body| body.to_string(),
                        ),
                    ));
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        (hub, seen)
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

    fn enroll(hub: &str, account: &str) {
        let identity = agit_peer::Identity::generate().unwrap();
        let device = Device {
            id: "executor".into(),
            owner: Principal {
                issuer: hub.into(),
                account_id: account.into(),
            },
            machine_id: "machine".into(),
            display_name: "Fixture".into(),
            certificate: identity.certificate().clone(),
            credential_epoch: 1,
        };
        crate::rc::cloud::store::save(&crate::rc::cloud::store::Enrollment {
            identity,
            inbound_enabled: true,
            credential: DeviceCredential {
                device,
                token: Secret::new("fixture-device-token".into()),
            },
        })
        .unwrap();
    }

    /// Pins the order and content of a sign-out's requests for an enrolled device: first an
    /// empty team activity report for that device, with the access token about to be revoked,
    /// then the revoke, and nothing after it. The report body lists no session and carries no
    /// other key. A device enrolled under another account gets no report, and the revoke still
    /// runs.
    #[test]
    fn sign_out_empties_the_devices_activity_report_before_revoking() {
        let (hub, seen) = fake_hub();
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            enroll(&hub, "owner");
            let _ = revoke(&hub, &credential(&hub), &mut HashSet::new());
            let bearer = Some(format!("Bearer {TOKEN}"));
            assert_eq!(
                *seen.lock().unwrap(),
                vec![
                    (
                        "PUT /api/peer/devices/executor/activity".to_owned(),
                        bearer.clone(),
                        r#"{"sessions":[]}"#.to_owned(),
                    ),
                    ("POST /api/auth/logout".to_owned(), bearer, "{}".to_owned()),
                ]
            );

            seen.lock().unwrap().clear();
            enroll(&hub, "someone-else");
            let _ = revoke(&hub, &credential(&hub), &mut HashSet::new());
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "{seen:?}");
            assert_eq!(seen[0].0, "POST /api/auth/logout");
        });
    }

    /// A Hub that cannot be reached holds up neither the empty report nor the revoke that
    /// follows: both fail at once, and the sign-out goes on to remove the credential.
    #[test]
    fn an_unreachable_hub_does_not_hold_up_the_sign_out() {
        let hub = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            enroll(&hub, "owner");
            let started = std::time::Instant::now();
            assert!(revoke(&hub, &credential(&hub), &mut HashSet::new()).is_err());
            assert!(started.elapsed() < CLEAR_ACTIVITY_TIMEOUT);
        });
    }
}
