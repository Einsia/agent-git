//! `agit login` — sign in to a hub.
//!
//! Three paths:
//!
//! * **Browser authorization (default, 1)**: the CLI opens a pending authorization request and
//!   opens the web interface; the user confirms there with GitHub (or an existing web session),
//!   and the CLI polls until it holds the session. The terminal touches no credential — this is
//!   the default path.
//! * **device code (2)**: the CLI prints a short code and the user types and confirms it in a
//!   browser on **any device**. SSH, containers and machines with no browser take this one.
//! * `--with-token`: read a PAT from stdin, for CI and agent environments (non-interactive).
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

use super::CmdResult;
use crate::infra::config;
use crate::infra::credentials::{self, HubCredential};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::io::Read as _;
use std::time::{Duration, Instant};

#[derive(ClapArgs)]
pub struct Args {
    /// Hub to sign in to (default: AGIT_HUB_URL → config hub.url → the built-in public hub).
    #[arg(long, value_name = "url")]
    pub hub: Option<String>,
    /// Read a PAT from stdin and sign in (CI / agent environments).
    #[arg(long)]
    pub with_token: bool,
    /// Skip the menu and use the device-code flow directly.
    #[arg(long)]
    pub device: bool,
}

pub fn run(args: Args) -> CmdResult {
    let hub = match &args.hub {
        Some(h) => {
            let h = h.trim().trim_end_matches('/').to_string();
            if !h.starts_with("http://") && !h.starts_with("https://") {
                ui::error(&format!(
                    "`--hub` needs a full address (https://…), got `{h}`"
                ));
                return Ok(ExitCode::Usage);
            }
            h
        }
        None => config::hub_url(),
    };
    println!("hub: {}", ui::accent(&hub));

    let result = if args.with_token {
        login_with_token(&hub)
    } else if args.device {
        login_device(&hub)
    } else {
        login_interactive(&hub)
    };

    match result {
        Ok(Some((cred, who))) => {
            credentials::save(&hub, &cred)?;
            // A hub named explicitly with --hub is most likely the one the user keeps using,
            // so remember it.
            if args.hub.is_some() {
                let _ = config::set_global("hub.url", Some(&hub));
            }
            ui::success(&format!("signed in as {}", ui::bold(&who)));
            Ok(ExitCode::Ok)
        }
        Ok(None) => {
            // Neither interactive flow asks for a password, so None can only come from a
            // non-interactive environment.
            ui::error("signing in needs an interactive terminal.");
            ui::hint(
                "CI / agents use `agit login --with-token < token.txt` (reads a PAT from stdin)",
            );
            Ok(ExitCode::Interactive)
        }
        Err(e) => Err(e),
    }
}

/// Interactive entry point: browser authorization by default, device code as the alternative.
///
/// A separate function so any other command that needs to "make sure we are signed in" reuses it.
pub fn login() -> crate::Result<Option<String>> {
    let hub = config::hub_url();
    Ok(login_interactive(&hub)?.map(|(_, who)| who))
}

fn login_interactive(hub: &str) -> crate::Result<Option<(HubCredential, String)>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    println!();
    println!("  how do you want to sign in?");
    println!(
        "    {} browser — open the hub in your browser (nothing to type here)",
        ui::accent("1.")
    );
    println!(
        "    {} device code — we show a code, you enter it on the website (SSH, containers, no browser)",
        ui::accent("2.")
    );
    let choice = ui::prompt::input("choice [1]", None)?;
    let Some(choice) = choice else {
        return Ok(None);
    };
    match choice.trim() {
        "" | "1" => login_browser(hub),
        "2" => login_device(hub),
        other => {
            ui::error(&format!("`{other}` isn’t 1 or 2."));
            Ok(None)
        }
    }
}

use std::io::IsTerminal as _;

// ──────────────────── browser authorization flow ────────────────────

#[derive(serde::Deserialize)]
struct CliSession {
    state: String,
    url: String,
    expires_in: u64,
}

fn login_browser(hub: &str) -> crate::Result<Option<(HubCredential, String)>> {
    let client = crate::hub::Client::for_hub(hub);
    let session: CliSession = client.post_public("api/auth/cli/session", &serde_json::json!({}))?;

    println!();
    println!("  open this link to authorize the CLI:");
    println!("    {}", ui::accent(&session.url));
    if open_browser(&session.url) {
        println!("  {}", ui::dim("(opened in your browser)"));
    }
    println!("  waiting for approval… (ctrl-c to cancel)");

    poll(
        hub,
        &client,
        "api/auth/cli/poll",
        "state",
        &session.state,
        2,
        session.expires_in,
    )
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

fn login_device(hub: &str) -> crate::Result<Option<(HubCredential, String)>> {
    let client = crate::hub::Client::for_hub(hub);
    let dev: DeviceCode = client.post_public("api/auth/device/code", &serde_json::json!({}))?;

    println!();
    println!("  on any device with a browser, open:");
    println!("    {}", ui::accent(&dev.verification_uri));
    println!(
        "  and enter this code:  {}",
        ui::accent(&ui::bold(&dev.user_code))
    );
    println!("  waiting… (ctrl-c to cancel)");

    poll(
        hub,
        &client,
        "api/auth/device/token",
        "device_code",
        &dev.device_code,
        dev.interval.max(2),
        dev.expires_in,
    )
}

/// Poll the authorization endpoint until a session arrives or the request expires. While the
/// request is pending the server answers 202 + {"status":"pending"}.
#[allow(clippy::too_many_arguments)]
fn poll(
    _hub: &str,
    client: &crate::hub::Client,
    path: &str,
    key: &str,
    value: &str,
    interval: u64,
    expires_in: u64,
) -> crate::Result<Option<(HubCredential, String)>> {
    let deadline = Instant::now() + Duration::from_secs(expires_in.min(600));
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        if Instant::now() > deadline {
            ui::error("the sign-in request expired before it was approved.");
            ui::hint("run `agit login` again — pending requests live 10 minutes");
            return Ok(None);
        }
        let v: serde_json::Value = client.post_public(path, &serde_json::json!({ key: value }))?;
        match v.get("status").and_then(|s| s.as_str()) {
            Some("pending") | Some("authorization_pending") => continue,
            _ => {
                let username = v
                    .get("username")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow::anyhow!("the hub answered without a username: {v}"))?
                    .to_string();
                let email = v.get("email").and_then(|s| s.as_str()).map(String::from);
                let get = |k: &str| {
                    v.get(k)
                        .and_then(|s| s.as_str())
                        .map(String::from)
                        .ok_or_else(|| anyhow::anyhow!("the hub answer is missing `{k}`"))
                };
                return Ok(Some((
                    HubCredential {
                        username: username.clone(),
                        email,
                        hub: None,
                        access_token: get("access_token")?,
                        access_expires_at: get("access_expires_at")?,
                        refresh_token: get("refresh_token")?,
                        refresh_expires_at: get("refresh_expires_at")?,
                    },
                    username,
                )));
            }
        }
    }
}

/// Open a browser where possible; failing is not fatal (the link is already on screen).
fn open_browser(url: &str) -> bool {
    let (cmd, arg) = if cfg!(target_os = "macos") {
        ("open", url)
    } else if cfg!(target_os = "windows") {
        return std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    } else {
        ("xdg-open", url)
    };
    std::process::Command::new(cmd)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read a PAT from stdin. The token is persisted as-is — validity is settled by the 401 on
/// first use; nothing local can (or should) pre-validate a credential we did not issue.
fn login_with_token(hub: &str) -> crate::Result<Option<(HubCredential, String)>> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let token = buf.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("stdin was empty. usage: `agit login --with-token < token.txt`");
    }
    let client = crate::hub::Client::for_hub_with_token(hub, &token);
    // Trade the token for a real session (when the server knows the PAT); when it does not,
    // say so plainly.
    match client.login_with_pat(&token) {
        Ok(resp) => Ok(Some((
            HubCredential {
                username: resp.username.clone(),
                email: resp.email.clone(),
                hub: None,
                access_token: resp.access_token,
                access_expires_at: resp.access_expires_at,
                refresh_token: resp.refresh_token,
                refresh_expires_at: resp.refresh_expires_at,
            },
            resp.username,
        ))),
        Err(e) => {
            ui::error(&format!("the PAT wasn’t accepted: {e:#}"));
            ui::hint("mint a token by signing in on another machine, then paste it here");
            Ok(None)
        }
    }
}
