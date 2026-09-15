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
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::io::Read as _;
use std::time::{Duration, Instant};

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
    /// Check a browser authorization request and save credentials if the human approved it.
    #[arg(long, value_name = "state")]
    pub complete: Option<String>,
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
    if !args.with_token
        && args.complete.is_none()
        && !args.device
        && (super::json::is_capturing() || !ui::prompt::interactive())
    {
        return start_browser_handoff(&hub);
    }
    ui::info(format_args!("hub: {}", ui::accent(&hub)));

    let result = if args.with_token {
        login_with_token(&hub)
    } else if let Some(state) = &args.complete {
        complete_browser(&hub, state)
    } else if args.device {
        login_device(&hub)
    } else {
        login_interactive(&hub)
    };

    match result {
        Ok(Some((cred, who))) => {
            if let Err(error) = crate::telemetry::acquisition::save_login(&hub, &cred) {
                ui::error(&format!("cannot save the signed-in credentials: {error:#}"));
                return Ok(ExitCode::Precondition);
            }
            // A hub named explicitly with --hub is most likely the one the user keeps using,
            // so remember it.
            if args.hub.is_some() {
                let _ = config::set_global("hub.url", Some(&hub));
            }
            ui::success(&format!("signed in as {}", ui::bold(&who)));
            crate::telemetry::observe(crate::telemetry::Observation::Authentication(true));
            crate::telemetry::account_saved(&hub, cred.account_id.as_deref());
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

// ──────────────────── browser authorization flow ────────────────────

#[derive(serde::Deserialize)]
struct CliSession {
    state: String,
    url: String,
    expires_in: u64,
}

fn start_browser_handoff(hub: &str) -> CmdResult {
    let client = crate::hub::Client::for_hub(hub);
    let session: CliSession =
        remote_request(client.post_public("api/auth/cli/session", &serde_json::json!({})))?;
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
        #[cfg(windows)]
        let quote = ui::quote_powershell_argument;
        #[cfg(not(windows))]
        let quote = ui::quote_posix_argument;
        println!(
            "After the human approves, run: agit login --hub {} --complete {}",
            quote(hub),
            quote(&session.state)
        );
        println!("The login link expires in {} seconds.", session.expires_in);
    }
    ui::hint(message);
    Ok(ExitCode::Interactive)
}

fn complete_browser(hub: &str, state: &str) -> crate::Result<Option<(HubCredential, String)>> {
    if state.trim().is_empty() {
        return crate::input_argument(Err(anyhow::anyhow!("the sign-in state must not be empty")));
    }
    let client = crate::hub::Client::for_hub(hub);
    let response = authorization_response(&client, "api/auth/cli/poll", "state", state)?;
    match response {
        Some(session) => Ok(Some(session_credential(session))),
        None => Err(anyhow::Error::new(InteractionRequired(
            "Ask the human to finish approving the login link, then retry the same `agit login --complete` command. If the link expired, run `agit login` again to request a new link.".into(),
        ))),
    }
}

fn login_browser(hub: &str) -> crate::Result<Option<(HubCredential, String)>> {
    let client = crate::hub::Client::for_hub(hub);
    let session: CliSession =
        remote_request(client.post_public("api/auth/cli/session", &serde_json::json!({})))?;

    println!();
    let url = crate::telemetry::acquisition::authorization_url(&session.url, hub);
    println!("  open this link to authorize the CLI:");
    println!("    {}", ui::accent(&url));
    if open_browser(&url) {
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
    println!(
        "    {}",
        ui::accent(&crate::telemetry::acquisition::authorization_url(
            &dev.verification_uri,
            hub
        ))
    );
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
        if let Some(session) = authorization_response(client, path, key, value)? {
            return Ok(Some(session_credential(session)));
        }
    }
}

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
            .map_err(|_| anyhow::anyhow!("the Hub returned an invalid sign-in response")),
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
