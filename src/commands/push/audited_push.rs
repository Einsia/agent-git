//! Privacy publication binds generated content and destination before confirmation.

use super::consent::AutoConsent;
use super::*;
use crate::commands::clone::Checkout;
use crate::domain::privacy_git::ProjectedHistory;
use crate::domain::privacy_receipt::{PublicationReceipt, SupervisorReply};
use crate::hub::git::{
    CapturedPublication, ContentInspection, FrozenPublication, InspectionFailure,
    PublicationReport, PublicationStatus,
};
use crate::hub::identity::RemoteIdentity;
use crate::hub::{Client, RemoteAgent};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

mod sessions;

struct Destination {
    intent: Intent,
    local_target: Option<crate::rc::local_repository::publication::Selection>,
}

/// Publication cannot inherit routing overrides from an unrelated Git operation.
pub(super) fn check_environment(
    environment: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Result<()> {
    for (name, _) in environment {
        let name = name.to_string_lossy();
        #[cfg(windows)]
        let name = name.to_ascii_uppercase();
        let key: &str = name.as_ref();
        ensure!(
            !matches!(
                key,
                "GIT_DIR"
                    | "GIT_COMMON_DIR"
                    | "GIT_WORK_TREE"
                    | "GIT_INDEX_FILE"
                    | "GIT_OBJECT_DIRECTORY"
                    | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                    | "GIT_NAMESPACE"
                    | "GIT_SHALLOW_FILE"
                    | "GIT_GRAFT_FILE"
                    | "GIT_PREFIX"
                    | "GIT_CONFIG"
                    | "GIT_CONFIG_PARAMETERS"
                    | "GIT_CONFIG_COUNT"
            ) && !key.starts_with("GIT_CONFIG_KEY_")
                && !key.starts_with("GIT_CONFIG_VALUE_"),
            "push cannot inherit {key}; clear Git routing and injected configuration before retrying"
        );
    }
    Ok(())
}

pub(super) fn run(
    args: &Args,
    client: Client,
    me: &str,
    checkout: Checkout,
    repo: Repo,
    branches: &[String],
) -> CmdResult {
    let retry_command = retry_push_command(args, &checkout.slug(), branches);
    ensure!(
        client.credential_username().as_deref() == Some(me),
        "the selected account changed while preparing publication"
    );
    let local_target = crate::rc::local_repository::publication::Selection::prepare(
        &repo,
        args.to.as_deref(),
        client.base(),
    )?;
    let destination_checkout = local_target
        .as_ref()
        .map(|selection| selection.checkout(&checkout.path))
        .transpose()?;
    let intent = Intent::resolve(
        args,
        &client,
        me,
        destination_checkout.as_ref().unwrap_or(&checkout),
        &repo,
    )?;
    sessions::run(
        args,
        client,
        repo,
        branches,
        &retry_command,
        Destination {
            intent,
            local_target,
        },
    )
}

fn retry_push_command(args: &Args, slug: &str, branches: &[String]) -> String {
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    let mut command = format!("agit push {}", quote_arg(slug));
    for branch in branches {
        command.push_str(&format!(" --branch={}", quote_arg(branch)));
    }
    if let Some(destination) = &args.to {
        command.push_str(&format!(" --to={}", quote_arg(destination)));
    }
    for (selected, flag) in [
        (args.audit, "--audit"),
        (args.dry_run, "--dry-run"),
        (args.show_preview, "--show-preview"),
        (args.public, "--public"),
        (args.private, "--private"),
    ] {
        if selected {
            command.push(' ');
            command.push_str(flag);
        }
    }
    command
}

fn publication_recovery(report: &PublicationReport, retry: &str, repository: &str) -> String {
    let attempts: Vec<_> = [&report.heads, &report.tags]
        .into_iter()
        .flatten()
        .flat_map(|phase| &phase.attempts)
        .collect();
    let unknown = attempts.iter().any(|attempt| {
        !attempt.complete
            || attempt
                .refs
                .iter()
                .any(|reference| reference.status == PublicationStatus::Unconfirmed)
    });
    if unknown {
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        return format!(
            "publication did not complete; some ref updates are unconfirmed; run `agit fetch {}` and inspect `agit status` before rerunning `{retry}`",
            quote_arg(repository)
        );
    }
    if attempts
        .iter()
        .flat_map(|attempt| &attempt.refs)
        .any(|reference| reference.status == PublicationStatus::Updated)
    {
        format!(
            "publication did not complete; some ref updates were acknowledged; rerunning `{retry}` is safe and rechecks the destination"
        )
    } else {
        format!(
            "publication did not complete; no branch or tag refs were updated; rerunning `{retry}` is safe after resolving the reported error"
        )
    }
}

fn verify_receipt_acknowledgements(
    report: &PublicationReport,
    receipts: &[PublicationReceipt],
) -> Result<()> {
    for receipt in receipts {
        let acknowledged = report
            .heads
            .as_ref()
            .into_iter()
            .flat_map(|phase| &phase.attempts)
            .flat_map(|attempt| &attempt.refs)
            .rfind(|reference| {
                reference.reference.name() == format!("refs/heads/{}", receipt.branch)
            })
            .context("publication has no acknowledgement for the selected branch")?;
        ensure!(
            acknowledged.reference.oid() == receipt.published
                && matches!(
                    acknowledged.status,
                    PublicationStatus::Updated | PublicationStatus::UpToDate
                ),
            "publication acknowledgement differs from the reviewed session"
        );
    }
    Ok(())
}

fn inspection_destination(args: &Args, intent: &Intent) -> Result<Option<RemoteIdentity>> {
    if args.audit {
        return Ok(None);
    }
    match &intent.action {
        Action::Existing(remote) => Ok(Some(RemoteIdentity::new(&intent.hub, &remote.agent_id)?)),
        Action::Create => Ok(None),
    }
}

fn receipts_match_current(repo: &Repo, expected: &[PublicationReceipt]) -> Result<bool> {
    for candidate in expected {
        let saved = PublicationReceipt::load(repo, &candidate.branch)?;
        let Some(saved) = saved else {
            return Ok(false);
        };
        if saved != *candidate {
            return Ok(false);
        }
    }
    Ok(true)
}

fn no_pending_publication_receipts(repo: &Repo, receipts: &[PublicationReceipt]) -> Result<bool> {
    Ok(receipts.iter().all(|receipt| {
        !crate::domain::privacy_receipt::outbox::Entry::pending_for_publication(repo, receipt)
            .unwrap_or(true)
    }))
}

fn inspection_failure_code(reason: InspectionFailure) -> ExitCode {
    match reason {
        InspectionFailure::Configuration => ExitCode::Usage,
        InspectionFailure::LocalState | InspectionFailure::Content => ExitCode::Precondition,
        InspectionFailure::Incomplete => ExitCode::Policy,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Incomplete,
    ReviewedOnly,
    Declined,
    Publish,
}

/// A completed model report is separate from the user's permission to publish it.
fn confirm_publication(
    complete: bool,
    dry_run: bool,
    confirm: impl FnOnce() -> Result<Option<bool>>,
) -> Result<Decision> {
    if !complete {
        return Ok(Decision::Incomplete);
    }
    if dry_run {
        return Ok(Decision::ReviewedOnly);
    }
    Ok(match confirm()? {
        Some(true) => Decision::Publish,
        Some(false) | None => Decision::Declined,
    })
}

#[derive(Debug)]
enum Action {
    Existing(RemoteAgent),
    Create,
}

#[derive(Debug)]
struct Intent {
    hub: String,
    account: String,
    owner: String,
    name: String,
    url: String,
    visibility: String,
    accept_secret_findings: bool,
    action: Action,
}

impl Intent {
    fn resolve(
        args: &Args,
        client: &Client,
        me: &str,
        checkout: &Checkout,
        repo: &Repo,
    ) -> Result<Self> {
        let hub = identity::normalize_hub(client.base())?;
        let expected = identity::expected_for_transport(repo, client.base())?;
        let requires_existing = expected.is_some() || identity::read(repo)?.is_some();
        let source = if requires_existing {
            Some(super::super::remote_request(
                client.get_agent(&checkout.owner, &checkout.name),
            )?)
        } else {
            lookup(client, &checkout.owner, &checkout.name)?
        };
        let accept_secret_findings = std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV)
            .is_none()
            && (args.allow_secrets || config::allow_secrets());
        let write_denied = if is_read_only(me, &checkout.owner, repo.upstream_url().as_deref()) {
            match &source {
                Some(remote) => !matches!(
                    super::super::remote_request(client.push_access_with_secret_acceptance(
                        &checkout.owner,
                        &checkout.name,
                        &remote.agent_id,
                        accept_secret_findings,
                    ))?,
                    crate::hub::PushAccess::Writable
                ),
                None => {
                    let org = super::super::remote_request(client.get_org(&checkout.owner))?;
                    ensure!(
                        org.role == "owner",
                        "the selected namespace is not writable"
                    );
                    false
                }
            }
        } else {
            false
        };
        ensure!(
            !write_denied,
            "the selected account cannot publish to this repository; cross-repository publication requires preserved source authority"
        );
        let (owner, visibility, url, action) = if let Some(remote) = source {
            verify_agent_location(&hub, &checkout.owner, &checkout.name, &remote)?;
            identity::verify_transport_target(repo, &RemoteIdentity::new(&hub, &remote.agent_id)?)?;
            (
                checkout.owner.clone(),
                remote.visibility.clone(),
                remote.clone_url.clone(),
                Action::Existing(remote),
            )
        } else {
            ensure!(
                expected.is_none() && identity::read(repo)?.is_none(),
                "the pinned remote is unavailable; refusing to create a replacement"
            );
            let public = match wanted_visibility(args, repo).value() {
                Some(public) => public,
                None => first_visibility(None, ask_visibility(&checkout.name)?),
            };
            (
                checkout.owner.clone(),
                visibility_word(public).into(),
                format!("{hub}/{}/{}.git", checkout.owner, checkout.name),
                Action::Create,
            )
        };
        ensure!(
            matches!(visibility.as_str(), "public" | "private"),
            "the publication audience is unsupported"
        );
        verify_url(&hub, &url)?;
        if let Action::Existing(remote) = &action
            && let Some(pinned) = identity::read(repo)?
        {
            ensure!(
                pinned == RemoteIdentity::new(&hub, &remote.agent_id)?,
                "the publication destination differs from the pinned repository identity"
            );
        }
        if !matches!(action, Action::Create) && (args.private || args.public) {
            ui::warning(
                "visibility flags only affect creation; the publication destination retains its existing audience",
            );
        }
        let intent = Self {
            hub,
            account: me.into(),
            owner,
            name: checkout.name.clone(),
            url,
            visibility,
            accept_secret_findings,
            action,
        };
        intent.verify_account(client)?;
        Ok(intent)
    }

    fn json(&self) -> Value {
        let (action, identity) = match &self.action {
            Action::Create => ("ensure", Value::Null),
            Action::Existing(remote) => ("publish", json!(remote.agent_id)),
        };
        json!({"hub":self.hub,"account":self.account,"owner":self.owner,"name":self.name,"url":self.url,"visibility":self.visibility,"action":action,"agent_id":identity,"repo_origins":[],"publication":"registered session modes with immutable accepted ancestry"})
    }

    fn verify_account(&self, client: &Client) -> Result<()> {
        ensure!(
            identity::normalize_hub(client.base())? == self.hub
                && identity::normalize_hub(&config::hub_url())? == self.hub,
            "the selected Hub changed during publication"
        );
        let credential = credentials::load_checked(&self.hub)?
            .context("the selected account is no longer signed in")?;
        ensure!(
            credential.username == self.account
                && client.credential_username().as_deref() == Some(self.account.as_str()),
            "the selected account changed during publication"
        );
        Ok(())
    }

    fn verify_observed(&self, remote: &RemoteAgent, expected_id: Option<&str>) -> Result<()> {
        verify_agent_location(&self.hub, &self.owner, &self.name, remote)?;
        ensure!(
            remote.clone_url == self.url && remote.visibility == self.visibility,
            "the publication endpoint or audience changed during publication"
        );
        ensure!(
            expected_id.is_none_or(|id| remote.agent_id == id),
            "the publication identity changed during publication"
        );
        Ok(())
    }

    fn verify_before_mutation(&self, client: &Client, repo: &Repo) -> Result<()> {
        match &self.action {
            Action::Existing(expected) => {
                let observed = lookup(client, &self.owner, &self.name)?
                    .context("the publication destination disappeared")?;
                self.verify_observed(&observed, Some(&expected.agent_id))?;
                identity::verify_transport_target(
                    repo,
                    &RemoteIdentity::new(&self.hub, &observed.agent_id)?,
                )?;
            }
            Action::Create => {
                if let Some(observed) = lookup(client, &self.owner, &self.name)? {
                    self.verify_observed(&observed, None)?;
                    self.verify_write_access(client, &observed.agent_id)?;
                }
                ensure!(
                    identity::expected_for_transport(repo, &self.hub)?.is_none(),
                    "the RC identity forbids creating a replacement"
                );
            }
        }
        Ok(())
    }

    fn materialize(&self, client: &Client, repo: Repo) -> Result<(Repo, Remote)> {
        let Action::Existing(expected) = &self.action else {
            anyhow::bail!("session publication destination was not registered before preparation");
        };
        let observed = lookup(client, &self.owner, &self.name)?
            .context("the publication destination disappeared; refusing to create a replacement")?;
        self.verify_observed(&observed, Some(&expected.agent_id))?;
        let identity = RemoteIdentity::new(&self.hub, &observed.agent_id)?;
        identity::verify_transport_target(&repo, &identity)?;
        let remote = Remote {
            owner: observed.owner,
            name: observed.name,
            push_url: observed.clone_url,
            identity,
            visibility: observed.visibility,
        };
        self.verify_remote(&remote)?;
        self.verify_write_access(client, &remote.identity.agent_id)?;
        bind_publication_origin(
            &repo,
            &format!("{}/{}", self.owner, self.name),
            &remote.identity,
            &remote.push_url,
        )?;
        Ok((repo, remote))
    }

    fn verify_write_access(&self, client: &Client, agent_id: &str) -> Result<()> {
        ensure!(
            matches!(
                super::super::remote_request(client.push_access_with_secret_acceptance(
                    &self.owner,
                    &self.name,
                    agent_id,
                    self.accept_secret_findings,
                ))?,
                crate::hub::PushAccess::Writable
            ),
            "the selected account cannot write to the reviewed destination"
        );
        Ok(())
    }

    fn verify_remote(&self, remote: &Remote) -> Result<()> {
        ensure!(
            remote.owner == self.owner
                && remote.name == self.name
                && remote.push_url == self.url
                && remote.visibility == self.visibility
                && remote.identity.hub == self.hub,
            "the actual destination does not match the reviewed publication"
        );
        if let Action::Existing(expected) = &self.action {
            ensure!(
                remote.identity.agent_id == expected.agent_id,
                "the publication identity changed during publication"
            );
        }
        Ok(())
    }
}

fn lookup(client: &Client, owner: &str, name: &str) -> Result<Option<RemoteAgent>> {
    match super::super::remote_request(client.get_agent(owner, name)) {
        Ok(remote) => Ok(Some(remote)),
        Err(error)
            if error
                .downcast_ref::<crate::hub::client::ApiError>()
                .is_some_and(|api| api.status == 404) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn verify_agent_location(hub: &str, owner: &str, name: &str, remote: &RemoteAgent) -> Result<()> {
    ensure!(
        remote.owner == owner && remote.name == name,
        "the Hub returned another publication destination"
    );
    RemoteIdentity::new(hub, &remote.agent_id)?;
    verify_url(hub, &remote.clone_url)
}

/// Unattended delivery rechecks account and destination authority; push validates each snapshot.
pub(crate) fn automatic_publication_consent(
    repo: &Repo,
    repository: &str,
    expected: &RemoteIdentity,
) -> Result<bool> {
    let client = Client::from_env_with_timeout(std::time::Duration::from_secs(5));
    automatic_publication_consent_with_client(repo, repository, expected, &client)
}

pub(crate) fn automatic_publication_consent_with_client(
    repo: &Repo,
    repository: &str,
    expected: &RemoteIdentity,
    client: &Client,
) -> Result<bool> {
    if !repo.auto_push_enabled()? {
        return Ok(false);
    }
    publication_consent_with_client(repo, repository, expected, client)?;
    Ok(true)
}

/// Explicit enrollment binds a destination without choosing a repository-wide session mode.
pub(crate) fn publication_consent(
    repo: &Repo,
    repository: &str,
    expected: &RemoteIdentity,
) -> Result<AutoConsent> {
    let client = Client::from_env_with_timeout(std::time::Duration::from_secs(5));
    publication_consent_with_client(repo, repository, expected, &client)
}

pub(crate) fn publication_consent_with_client(
    _repo: &Repo,
    repository: &str,
    expected: &RemoteIdentity,
    client: &Client,
) -> Result<AutoConsent> {
    ensure!(
        identity::normalize_hub(client.base())? == expected.hub,
        "publication Hub changed"
    );
    let (owner, name) = super::super::parse_slug(repository)?;
    let remote = client.get_agent(&owner, &name)?;
    verify_agent_location(&expected.hub, &owner, &name, &remote)?;
    ensure!(
        remote.agent_id == expected.agent_id,
        "publication repository identity changed"
    );
    ensure!(
        matches!(
            client.push_access(&owner, &name, &expected.agent_id)?,
            crate::hub::PushAccess::Writable
        ),
        "publication write authority is unavailable"
    );
    let credential =
        credentials::load_checked(&expected.hub)?.context("publication account is unavailable")?;
    let account = client.session_account()?;
    ensure!(
        client.credential_username().as_deref() == Some(credential.username.as_str())
            && credential
                .account_id
                .as_ref()
                .is_none_or(|id| id == &account.account_id),
        "publication account changed"
    );
    Ok(AutoConsent {
        version: 3,
        hub: expected.hub.clone(),
        account: credential.username,
        account_id: account.account_id,
        agent_id: expected.agent_id.clone(),
        url: remote.clone_url,
        visibility: remote.visibility,
    })
}

fn verify_url(hub: &str, url: &str) -> Result<()> {
    ensure!(
        identity::normalize_hub(url)? == url
            && url.starts_with(&format!("{hub}/"))
            && crate::infra::hub_authority::HubAuthority::parse(hub)?.matches(url),
        "the publication URL is outside the selected Hub"
    );
    ensure!(
        !url.chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '\\' | '%'))
            && !url.split('/').any(|part| matches!(part, "." | "..")),
        "the publication URL is not canonical"
    );
    Ok(())
}

fn show_publication(report: &PublicationReport) {
    show_phase_error("publication", report.error.as_deref());
    for (name, phase) in [("heads", &report.heads), ("tags", &report.tags)] {
        if let Some(phase) = phase {
            show_phase_error(name, phase.error.as_deref());
            for attempt in &phase.attempts {
                if !attempt.ok() {
                    show_phase_error(name, attempt.error.as_deref());
                    println!(
                        "{name}: Git exit {}, HTTP status {:?}",
                        attempt.outcome.code,
                        attempt.outcome.http_status()
                    );
                }
                for reference in &attempt.refs {
                    let status = match reference.status {
                        PublicationStatus::Updated => "updated",
                        PublicationStatus::UpToDate => "up to date",
                        PublicationStatus::Unconfirmed => "unconfirmed",
                    };
                    println!(
                        "{name} batch {}: {} {status}",
                        attempt.batch,
                        reference.reference.name()
                    );
                }
            }
            for reference in &phase.unattempted {
                println!("{name}: {} not attempted", reference.name());
            }
        } else {
            println!("{name}: not attempted");
        }
    }
    if let Some(lfs) = &report.lfs {
        show_phase_error("LFS", lfs.error.as_deref());
        for attempt in &lfs.attempts {
            if !attempt.ok() {
                show_phase_error("LFS", attempt.error.as_deref());
                println!(
                    "LFS: Git exit {}, HTTP status {:?}",
                    attempt.outcome.code,
                    attempt.outcome.http_status()
                );
            }
        }
        println!(
            "LFS publication: {}; {} objects not attempted",
            if lfs.ok() { "complete" } else { "incomplete" },
            lfs.unattempted.len()
        );
    }
}

fn show_phase_error(phase: &str, error: Option<&str>) {
    if let Some(error) = error {
        println!("{phase}: {}", json!(error));
    }
}

fn publication_failure_code(report: &PublicationReport) -> ExitCode {
    if let Some(attempt) = report
        .lfs
        .as_ref()
        .and_then(|phase| phase.attempts.last())
        .filter(|attempt| !attempt.ok())
    {
        return branch_failure_code(&attempt.outcome);
    }
    for phase in [&report.heads, &report.tags].into_iter().flatten() {
        if let Some(attempt) = phase.attempts.last().filter(|attempt| !attempt.ok()) {
            return branch_failure_code(&attempt.outcome);
        }
    }
    ExitCode::Failure
}

#[cfg(test)]
mod tests;
