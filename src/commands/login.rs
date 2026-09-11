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

use super::{CmdResult, InteractionRequired, remote_request};
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
    let hub = args.hub.clone().unwrap_or_else(config::hub_url);
    if crate::infra::hub_authority::HubAuthority::parse(&hub).is_err() {
        ui::error(
            "the Hub must be a valid HTTP or HTTPS address without user information, query, or fragment",
        );
        return Ok(ExitCode::Usage);
    }
    let hub = hub.trim().trim_end_matches('/').to_string();
    ui::info(format_args!("hub: {}", ui::accent(&hub)));

    let result = if args.with_token {
        login_with_token(&hub)
    } else if args.device {
        login_device(&hub)
    } else {
        login_interactive(&hub)
    };

    match result {
        Ok(Some((cred, who))) => {
            if let Err(error) = credentials::save(&hub, &cred) {
                ui::error(&format!("cannot save the signed-in credentials: {error:#}"));
                return Ok(ExitCode::Precondition);
            }
            // A hub named explicitly with --hub is most likely the one the user keeps using,
            // so remember it.
            if args.hub.is_some() {
                let _ = config::set_global("hub.url", Some(&hub));
            }
            ui::success(&format!("signed in as {}", ui::bold(&who)));
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
        Err(error) => Err(error),
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
    let choice =
        ui::prompt::input("choice [1]", None).map_err(|error| error.context(LocalInputFailure))?;
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
    let session: CliSession =
        remote_request(client.post_public("api/auth/cli/session", &serde_json::json!({})))?;

    println!();
    println!("  open this link to authorize the CLI:");
    println!("    {}", ui::accent(&session.url));
    if open_browser(&session.url) {
        ui::info(format_args!("  {}", ui::dim("(opened in your browser)")));
    }
    ui::info("  waiting for approval… (ctrl-c to cancel)");

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
    let dev: DeviceCode =
        remote_request(client.post_public("api/auth/device/code", &serde_json::json!({})))?;

    println!();
    println!("  on any device with a browser, open:");
    println!("    {}", ui::accent(&dev.verification_uri));
    println!(
        "  and enter this code:  {}",
        ui::accent(&ui::bold(&dev.user_code))
    );
    ui::info("  waiting… (ctrl-c to cancel)");

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
            return Err(anyhow::Error::new(InteractionRequired(
                "the sign-in request expired before it was approved; run `agit login` again".into(),
            )));
        }
        let response: serde_json::Value =
            remote_request(client.post_public(path, &serde_json::json!({ key: value })))?;
        if matches!(
            response.get("status").and_then(|status| status.as_str()),
            Some("pending" | "authorization_pending")
        ) {
            continue;
        }
        // An invalid response can contain credentials; diagnostics expose its shape, not values.
        let session = remote_request(
            serde_json::from_value::<crate::hub::LoginResponse>(response)
                .map_err(|_| anyhow::anyhow!("the Hub returned an invalid sign-in response")),
        )?;
        return Ok(Some(session_credential(session)));
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

#[derive(Debug)]
struct LocalInputFailure;

impl std::fmt::Display for LocalInputFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot read sign-in input")
    }
}

/// Read a PAT from stdin and exchange it for a session at the selected Hub.
fn login_with_token(hub: &str) -> crate::Result<Option<(HubCredential, String)>> {
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
    let client = crate::hub::Client::for_hub_with_token(hub, &token);
    let response = remote_request(client.login_with_pat(&token)).map_err(|error| {
        if super::terminal_error_code(&error, ExitCode::Usage) == ExitCode::Auth {
            error.context("the PAT was not accepted")
        } else {
            error
        }
    })?;
    Ok(Some(session_credential(response)))
}

fn session_credential(response: crate::hub::LoginResponse) -> (HubCredential, String) {
    (
        HubCredential {
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
