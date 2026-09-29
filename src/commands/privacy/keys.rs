//! Administrator operations retain the fetched configuration version through password entry.

use crate::commands::{CmdResult, InteractionRequired};
use crate::domain::{privacy_key::KeyInput, repo::Repo};
use crate::hub::{
    Client, RemoteAgent,
    identity::{self, RemoteIdentity},
    privacy::repository_keys::{KeyOperation, RepositoryKeyConfig},
};
use crate::{ExitCode, infra::config, ui};
use anyhow::{Context, Result, ensure};
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    Initialize,
    BrowserInitialize,
    Rewrap,
    Rotate,
}

pub(super) fn run(
    repository: &str,
    operation: Operation,
    public: bool,
    encryption: Option<bool>,
) -> CmdResult {
    let (owner, name) = crate::commands::parse_slug(repository)?;
    ensure!(
        [&owner, &name]
            .iter()
            .all(|part| crate::domain::privacy_envelope::valid_token(part)
                && part.as_str() != "."
                && part.as_str() != ".."),
        "use one explicit OWNER/REPO for repository key configuration"
    );
    let repository = format!("{owner}/{name}");
    let client = crate::commands::require_login()?;
    let path = config::repo_dir(&owner, &name)?;
    let local = Repo::open(path.clone());
    let remote = match client.get_agent(&owner, &name) {
        Ok(remote) => remote,
        Err(error)
            if matches!(
                operation,
                Operation::Initialize | Operation::BrowserInitialize
            ) && error
                .downcast_ref::<crate::hub::client::ApiError>()
                .is_some_and(|api| api.status == 404) =>
        {
            if let Some(repo) = &local {
                ensure!(
                    identity::read(repo)?.is_none(),
                    "the pinned repository is unavailable; refusing to initialize a replacement under the same name"
                );
            }
            let enabled = match &local {
                Some(repo) => repo.encryption_for_creation(encryption)?,
                None => match encryption {
                    Some(value) => value,
                    None => config::encryption_default()?,
                },
            };
            ensure!(
                enabled,
                "viewing passwords require encryption enabled at creation; use `agit repo create {name} --encryption=false` for an ordinary repository or `agit privacy init {repository} --encryption=true` for an encrypted repository"
            );
            if operation == Operation::BrowserInitialize {
                if std::env::var_os("AGIT_YES").is_none() {
                    return Err(InteractionRequired(
                        "browser password setup requires --yes to create the missing repository"
                            .into(),
                    )
                    .into());
                }
            } else {
                ensure_interactive(ui::prompt::interactive())?;
            }
            let visibility = if public { "public" } else { "private" };
            if operation != Operation::BrowserInitialize
                && ui::prompt::confirm(
                    &format!(
                        "Create {repository} on {} as {visibility} before initializing its viewing password?",
                        client.base()
                    ),
                    false,
                )? != Some(true)
            {
                return Ok(ExitCode::Ok);
            }
            crate::commands::repo::create_for_privacy(&client, &owner, &name, public, &path)?
        }
        Err(error) => return Err(error),
    };
    let expected = check_remote(&client, &owner, &name, &remote)?;
    let enabled = remote.require_encryption_enabled()?;
    ensure!(
        encryption.is_none_or(|selected| enabled == selected),
        "repository encryption mode is fixed at creation; create a different repository for the requested mode"
    );
    ensure!(
        enabled,
        "{repository} has encryption disabled; its mode is fixed at creation. Create a different encrypted repository before initializing a viewing password"
    );
    let local = Repo::open(path);
    if let Some(repo) = &local {
        if let Some(pinned) = identity::read(repo)? {
            ensure!(
                pinned == expected,
                "the repository name identifies a different immutable repository; refusing key configuration"
            );
        }
        identity::verify_transport_target(repo, &expected)?;
    }
    if operation == Operation::BrowserInitialize {
        let repo = local.as_ref().with_context(|| format!(
            "browser password setup requires a local repository to retain its identity; initialize or clone {repository} first"
        ))?;
        return browser_status(&client, &repository, &expected, repo);
    }
    configure_password(&client, &repository, &expected, operation)
}

fn configure_password(
    client: &Client,
    repository: &str,
    expected: &RemoteIdentity,
    operation: Operation,
) -> CmdResult {
    ensure_interactive(ui::prompt::interactive())?;
    let before = client.repository_key_config(repository, expected)?;
    let (input, recipient) = prepare(&before, operation, ui::prompt::repository_password)?;
    let mutation = match operation {
        Operation::Initialize | Operation::BrowserInitialize => {
            KeyOperation::Initialize { key: &input }
        }
        Operation::Rotate => KeyOperation::Rotate { key: &input },
        Operation::Rewrap => KeyOperation::Rewrap {
            recipient: recipient.as_deref().expect("current rewrap recipient"),
            key: &input,
        },
    };
    let after = client.mutate_repository_key(repository, expected, &before, mutation)?;
    let action = match operation {
        Operation::Initialize | Operation::BrowserInitialize => "initialize",
        Operation::Rewrap => "change_password",
        Operation::Rotate => "rotate_key",
    };
    if crate::commands::json::requested() {
        println!(
            "{}",
            serde_json::json!({"operation":format!("privacy_{action}"), "repository":repository, "agent_id":after.agent_id, "recipient":after.current_recipient, "config_version":after.config_version})
        );
    } else {
        ui::success(&format!(
            "repository viewing key configured for {repository} (version {})",
            after.config_version
        ));
        if operation == Operation::Initialize {
            println!("Next: review and publish with `agit push {repository}@<branch>`.");
        }
    }
    Ok(ExitCode::Ok)
}

fn browser_status(
    client: &Client,
    repository: &str,
    expected: &RemoteIdentity,
    repo: &Repo,
) -> CmdResult {
    let configuration = client.repository_publishing_key(repository, expected)?;
    pin_browser_identity(repo, repository, expected)?;
    let ready = configuration.current.is_some();
    let mut value = serde_json::json!({
        "operation": "privacy_initialize",
        "status": if ready { "ready" } else { "setup_required" },
        "repository": repository,
        "hub": client.base(),
        "agent_id": expected.agent_id,
        "config_version": configuration.config_version,
    });
    if !ready {
        let mut url = url::Url::parse(&format!("{}/@{repository}/settings", client.base()))?;
        url.query_pairs_mut()
            .append_pair("setup", "initialize")
            .append_pair("expected_agent_id", &expected.agent_id);
        url.set_fragment(Some("repository-password"));
        value["setup_url"] = url.as_str().into();
    }
    if crate::commands::json::requested() {
        println!("{value}");
    } else if ready {
        ui::success(&format!("repository viewing key is ready for {repository}"));
    } else {
        println!(
            "Set the repository viewing password in your browser: {}",
            value["setup_url"].as_str().expect("setup URL")
        );
        println!(
            "After setup, rerun privacy init for {repository} with --browser and the same AGIT_HUB_URL={} to confirm readiness.",
            client.base()
        );
    }
    Ok(if ready {
        ExitCode::Ok
    } else {
        ExitCode::Interactive
    })
}

fn pin_browser_identity(repo: &Repo, repository: &str, expected: &RemoteIdentity) -> Result<()> {
    if identity::read(repo)?.is_none() {
        let destination = format!("{}/{repository}.git", expected.hub);
        ensure!(
            repo.remote_url().is_none_or(|url| url == destination),
            "the local repository has another remote; refusing to bind browser setup to a different destination"
        );
    }
    identity::pin(repo, expected)
}

fn ensure_interactive(interactive: bool) -> Result<()> {
    if !interactive {
        return Err(InteractionRequired("repository password configuration requires an interactive terminal; --yes cannot supply a password".into()).into());
    }
    Ok(())
}

fn prepare(
    before: &RepositoryKeyConfig,
    operation: Operation,
    mut prompt: impl FnMut(&str, bool) -> Result<Zeroizing<String>>,
) -> Result<(KeyInput, Option<String>)> {
    ensure!(
        before.current_recipient.is_none() == (operation == Operation::Initialize),
        "repository viewing-key state does not match this operation; refresh the configuration"
    );
    let (private, recipient) = if operation == Operation::Rewrap {
        let current = before
            .current()
            .context("repository viewing key is not configured")?;
        let password = prompt("Current repository viewing password", false)?;
        (
            Some(current.unlock(&password)?),
            Some(current.recipient.clone()),
        )
    } else {
        (None, None)
    };
    let password = prompt("New repository viewing password", true)?;
    let input = match private.as_ref() {
        Some(key) => KeyInput::wrap(key, &password)?,
        None => KeyInput::generate(&password)?,
    };
    let reopened = input.unlock(&password)?;
    if let Some(private) = private {
        ensure!(
            *private == *reopened,
            "password rewrapping changed the repository private key"
        );
    }
    Ok((input, recipient))
}

fn check_remote(
    client: &Client,
    owner: &str,
    name: &str,
    remote: &RemoteAgent,
) -> Result<RemoteIdentity> {
    ensure!(
        remote.owner == owner && remote.name == name,
        "the Hub returned another repository for key configuration"
    );
    RemoteIdentity::new(client.base(), &remote.agent_id)
}

#[cfg(test)]
mod tests;
