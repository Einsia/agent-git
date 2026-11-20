//! Organization invitations use explicit targets and the selected Hub's existing login.

use super::CmdResult;
use crate::hub::client::ApiError;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Invite an existing account as a member; membership starts when they accept.
    ///
    /// Requires an organization owner's normal `agit login`. Personal access tokens cannot
    /// invite. Review or revoke invitations in the Hub's organization settings.
    Invite {
        #[arg(value_name = "org", value_parser = explicit_name)]
        org: String,
        /// The recipient's account username, not an email address.
        #[arg(long, value_name = "username", value_parser = explicit_name)]
        user: String,
    },
}

fn explicit_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("use an explicit name containing letters, digits, '-' or '_'".into());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn run(args: Args, json: bool) -> CmdResult {
    match args.cmd {
        Cmd::Invite { org, user } => invite(&org, &user, json),
    }
}

fn invite(org: &str, user: &str, json: bool) -> CmdResult {
    let client = super::require_login()?;
    let account = client
        .credential_username()
        .ok_or_else(|| anyhow::anyhow!("the selected account is unavailable; run `agit login`"))?;
    let settings = format!("{}/settings/organizations?org={org}", client.base());
    if !json {
        println!(
            "Hub: {}\nAccount: {account}\nOrganization: {org}\nRecipient: {user}",
            client.base()
        );
    }
    let result = match client.invite_org_member(org, user) {
        Ok(result) => result,
        Err(error) => {
            super::fix::register_terminal_api_error(&error);
            if let Some(api) = error.downcast_ref::<ApiError>() {
                match api.status {
                    405 | 501 => ui::error(
                        "this Hub does not support CLI organization invitations; ask its administrator to upgrade",
                    ),
                    404 => ui::error(
                        "the organization or recipient is unavailable, you are not its owner, or this Hub does not support CLI organization invitations",
                    ),
                    403 => ui::error(
                        "invitation refused; use an organization owner's normal `agit login` (personal access tokens cannot invite)",
                    ),
                    500.. => ui::error(
                        "invitation outcome is unconfirmed; check organization settings before trying again",
                    ),
                    _ => ui::error(&api.to_string()),
                }
            } else {
                ui::error(
                    "invitation outcome is unconfirmed; check organization settings before trying again",
                );
            }
            ui::hint(&format!("Review invitations: {settings}"));
            let fallback = match error.downcast_ref::<ApiError>().map(|api| api.status) {
                Some(403) => ExitCode::Policy,
                Some(404 | 405 | 409 | 501) => ExitCode::Precondition,
                Some(400 | 422) => ExitCode::Usage,
                _ => ExitCode::Network,
            };
            return Ok(super::terminal_error_code(&error, fallback));
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "hub": client.base(), "account": account, "status": "pending",
                "created": result.created, "invitation": result.invitation, "settings_url": settings,
            }))?
        );
    } else {
        println!(
            "{} invitation pending acceptance: {} → {} (member).\nExpires: {}",
            if result.created { "New" } else { "Existing" },
            result.invitation.org,
            result.invitation.username,
            result.invitation.expires_at
        );
        if !result.created {
            println!("The existing invitation and expiry are unchanged.");
        }
        println!("Review or revoke: {settings}");
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neither a repository context nor an email-shaped argument selects an invitation target.
    #[test]
    fn invitations_require_an_explicit_org_and_username() {
        use clap::Parser as _;
        let parsed = crate::commands::Cli::try_parse_from([
            "agit", "org", "invite", "Acme", "--user", "Carol",
        ])
        .unwrap();
        assert!(matches!(parsed.command,
            Some(super::super::Commands::Org(Args { cmd: Cmd::Invite { org, user } }))
                if org == "acme" && user == "carol"));
        for args in [
            vec!["agit", "org", "invite", "--user", "carol"],
            vec!["agit", "org", "invite", "acme"],
            vec!["agit", "org", "invite", "acme/repo", "--user", "carol"],
            vec![
                "agit",
                "org",
                "invite",
                "acme",
                "--user",
                "carol@example.com",
            ],
            vec![
                "agit", "org", "invite", "acme", "--user", "carol", "--role", "owner",
            ],
        ] {
            assert!(crate::commands::Cli::try_parse_from(args).is_err());
        }
    }
}
