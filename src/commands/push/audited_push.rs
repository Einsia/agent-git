//! Privacy publication binds generated content and destination before confirmation.

use super::consent::AutoConsent;
use super::*;
use crate::commands::clone::Checkout;
use crate::domain::privacy_git::ProjectedHistory;
use crate::domain::privacy_receipt::{PublicationReceipt, SupervisorReply};
use crate::hub::git::{
    CapturedPublication, ContentInspection, FrozenPublication, InspectionFailure,
    PublicationAttempt, PublicationReport, PublicationStatus,
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

/// Whether a publication may still be run again from its preview when the Hub refuses its receipt
/// because the repository's refs moved meanwhile.
#[derive(Debug)]
pub(super) enum Attempt {
    First,
    /// Carries the part the refused attempt sent. The retry plans from what the Hub holds, so its
    /// first part is what remained of that one; a `--yes` given for it answers the retry only
    /// while that first preview is the same publication.
    Retry(Previewed),
}

/// This push's `--yes`. A retry whose preview is not the publication it was given for withdraws it,
/// and from then on it answers nothing this push publishes: not that retry, not the branches of a
/// failed group published again one at a time, and not a later group. Read afresh by each
/// attempt, it would answer again what the person never confirmed.
pub(super) struct Yes {
    given: bool,
    withdrawn: std::cell::Cell<bool>,
}

impl Yes {
    pub(super) fn from_environment() -> Self {
        Self {
            given: std::env::var_os("AGIT_YES").is_some(),
            withdrawn: std::cell::Cell::new(false),
        }
    }

    fn given(&self) -> bool {
        self.given
    }

    fn answers(&self) -> bool {
        self.given && !self.withdrawn.get()
    }

    fn withdraw(&self) {
        self.withdrawn.set(true);
    }
}

/// What a publication preview puts before the person: where it goes, and the sessions and policy
/// its receipt binds. Equal values are the same publication; any difference is one nobody
/// answered for yet.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Previewed {
    visibility: String,
    policy_digest: String,
    snapshot_digest: String,
}

impl Previewed {
    fn of(projected: &ProjectedHistory, visibility: &str) -> Result<Self> {
        Ok(Self {
            visibility: visibility.to_owned(),
            policy_digest: projected.policy_digest().to_owned(),
            snapshot_digest: crate::hub::privacy::publication::snapshot_digest(
                projected.session_manifest(),
            )?,
        })
    }
}

/// The kind the Hub gives a publication receipt it refuses because the repository's refs changed
/// after the publication was previewed, as another member's push to any branch does. A new
/// preview of the same publication can pass; no other receipt refusal promises that.
const REFS_MOVED: &str = "publication_refs_moved";

/// A first attempt the Hub refused as [`REFS_MOVED`]. Its caller runs the publication once more
/// from the start: destination, preview, every gate and confirmation, receipt and pack.
#[derive(Debug)]
pub(super) struct RefsMoved {
    refused: Previewed,
}

impl RefsMoved {
    pub(super) fn retry(self) -> Attempt {
        Attempt::Retry(self.refused)
    }
}

impl std::fmt::Display for RefsMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the repository changed on the Hub while this publication was being checked")
    }
}

impl std::error::Error for RefsMoved {}

fn refs_moved(report: &PublicationReport) -> bool {
    [&report.heads, &report.tags]
        .into_iter()
        .flatten()
        .flat_map(|phase| &phase.attempts)
        .any(|attempt| !attempt.ok() && attempt.outcome.refusal_kind() == Some(REFS_MOVED))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    args: &Args,
    client: Client,
    me: &str,
    checkout: Checkout,
    repo: Repo,
    branches: &[String],
    named: bool,
    attempt: Attempt,
    yes: &Yes,
    policy_checked: bool,
) -> CmdResult {
    let slug = checkout.slug();
    ensure!(
        client.credential_username().as_deref() == Some(me),
        "the selected account changed while preparing publication"
    );
    let device_local = crate::rc::local_repository::publication::is_device_local(&repo)?;
    let local_target = if args.to_hub.is_some() || !device_local {
        None
    } else {
        crate::rc::local_repository::publication::Selection::prepare(
            &repo,
            args.to.as_deref(),
            client.base(),
        )?
    };
    // An ordinary checkout publishes to another repository only when told to, and that repository
    // never becomes its default destination.
    let explicit_repository = if device_local {
        None
    } else {
        args.to
            .as_deref()
            .map(|slug| explicit_checkout(slug, &checkout))
            .transpose()?
            .flatten()
    };
    let destination_checkout = match &local_target {
        Some(selection) => Some(selection.checkout(&checkout.path)?),
        None => explicit_repository,
    };
    let explicit_repository = local_target.is_none() && destination_checkout.is_some();
    let intent = Intent::resolve(
        args,
        &client,
        me,
        destination_checkout.as_ref().unwrap_or(&checkout),
        &repo,
        explicit_repository,
    )?;
    sessions::run(
        args,
        client,
        repo,
        branches,
        named,
        &slug,
        Destination {
            intent,
            local_target,
        },
        attempt,
        yes,
        policy_checked,
    )
}

/// The repository `--to` names for an ordinary checkout, or `None` when it names the checkout's own
/// repository: that one stays the default destination, guarded by the checkout's pinned identity.
fn explicit_checkout(slug: &str, checkout: &Checkout) -> Result<Option<Checkout>> {
    let (owner, name) = crate::commands::parse_slug(slug)?;
    ensure!(
        format!("{owner}/{name}") == slug,
        "--to requires an explicit owner/repository"
    );
    if owner == checkout.owner && name == checkout.name {
        return Ok(None);
    }
    Ok(Some(Checkout {
        owner,
        name,
        path: checkout.path.clone(),
    }))
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
    if let Some(destination) = &args.to_hub {
        command.push_str(&format!(" --to-hub={}", quote_arg(destination)));
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

/// What stopped a publication, in the transport's words: the Hub's refusal, the step's own error,
/// a ref the Hub rejected with its reason, or the last line Git printed. A caller that runs push
/// as a child reads only the final error line, so the reason has to be in it.
fn publication_failure(report: &PublicationReport) -> Option<String> {
    let from_git = |code: i32, stderr: &str| {
        stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map_or_else(|| format!("Git exit {code}"), str::to_owned)
    };
    // Git's last line after a refusal is "failed to push some refs"; the Hub's own line says why,
    // and often what to do, so it is kept whole up to a longer bound. It is printed as the Hub
    // wrote it, and any Hub can write it, so control characters never reach the terminal.
    if let Some(refusal) = refused(report).find_map(|attempt| attempt.outcome.refusal()) {
        let printable: String = refusal.chars().filter(|c| !c.is_control()).collect();
        return Some(crate::ui::truncate(&printable, 400));
    }
    let lfs = report.lfs.as_ref().and_then(|lfs| {
        lfs.error.clone().or_else(|| {
            lfs.attempts
                .iter()
                .rfind(|attempt| !attempt.ok())
                .map(|attempt| {
                    attempt
                        .error
                        .clone()
                        .unwrap_or_else(|| from_git(attempt.outcome.code, &attempt.outcome.stderr))
                })
        })
    });
    let refs = [&report.heads, &report.tags]
        .into_iter()
        .flatten()
        .find_map(|phase| {
            phase.error.clone().or_else(|| {
                phase
                    .attempts
                    .iter()
                    .rfind(|attempt| !attempt.ok())
                    .map(|attempt| {
                        attempt
                            .error
                            .clone()
                            .or_else(|| attempt.refs.iter().find_map(|r| r.detail.clone()))
                            .unwrap_or_else(|| {
                                from_git(attempt.outcome.code, &attempt.outcome.stderr)
                            })
                    })
            })
        });
    let reason = report.error.clone().or(lfs).or(refs)?;
    Some(crate::ui::truncate(&reason, 200))
}

/// The ref attempts that failed, in order.
fn refused(report: &PublicationReport) -> impl Iterator<Item = &PublicationAttempt> {
    [&report.heads, &report.tags]
        .into_iter()
        .flatten()
        .flat_map(|phase| &phase.attempts)
        .filter(|attempt| !attempt.ok())
}

fn publication_recovery(report: &PublicationReport, retry: &str, repository: &str) -> String {
    let stopped = publication_failure(report).map_or_else(
        || "publication did not complete".to_owned(),
        |reason| format!("publication did not complete ({reason})"),
    );
    let attempts: Vec<_> = [&report.heads, &report.tags]
        .into_iter()
        .flatten()
        .flat_map(|phase| &phase.attempts)
        .collect();
    if refused(report).any(|attempt| {
        attempt.outcome.refusal_kind() == Some("provenance_rejected")
            && attempt
                .outcome
                .stderr
                .contains(crate::commands::push::UNSUPPORTED_BLOCK)
    }) {
        return crate::commands::push::ENCRYPTED_REFUSED.to_owned();
    }
    // The same push meets the same refusal, so rerunning it is no way forward.
    if report.error.is_none()
        && refused(report).next().is_some()
        && refused(report).all(|attempt| attempt.outcome.refused_for_good())
    {
        let done = if attempts
            .iter()
            .flat_map(|attempt| &attempt.refs)
            .any(|reference| reference.status == PublicationStatus::Updated)
        {
            "some ref updates were acknowledged"
        } else {
            "no branch or tag refs were updated"
        };
        if refused(report)
            .any(|attempt| attempt.outcome.refusal_kind() == Some("agentgit_upgrade_required"))
        {
            return format!(
                "{stopped}; {done}; {} `{retry}`",
                crate::commands::push::UPGRADE_FIRST
            );
        }
        return format!(
            "{stopped}; {done}; {}",
            crate::commands::push::REFUSED_AGAIN
        );
    }
    // An attempt the Hub turned down before its ref transaction changed nothing, although Git
    // reports none of its refs.
    let unknown = attempts.iter().any(|attempt| {
        !attempt.outcome.refused_before_refs_moved()
            && (!attempt.complete
                || attempt
                    .refs
                    .iter()
                    .any(|reference| reference.status == PublicationStatus::Unconfirmed))
    });
    if unknown {
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        return format!(
            "{stopped}; some ref updates are unconfirmed; run `agit fetch {}` and inspect `agit status` before rerunning `{retry}`",
            quote_arg(repository)
        );
    }
    if attempts
        .iter()
        .flat_map(|attempt| &attempt.refs)
        .any(|reference| reference.status == PublicationStatus::Updated)
    {
        format!(
            "{stopped}; some ref updates were acknowledged; rerunning `{retry}` is safe and rechecks the destination"
        )
    } else {
        format!(
            "{stopped}; no branch or tag refs were updated; rerunning `{retry}` is safe after resolving it"
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
    explicit_hub: bool,
    /// A repository on this Hub named by `--to`: checked by its own identity, never against the
    /// checkout's pinned one, and never bound to the checkout.
    explicit_repository: bool,
    source_identity: Option<RemoteIdentity>,
    action: Action,
}

impl Intent {
    fn resolve(
        args: &Args,
        client: &Client,
        me: &str,
        checkout: &Checkout,
        repo: &Repo,
        explicit_repository: bool,
    ) -> Result<Self> {
        let hub = identity::normalize_hub(client.base())?;
        let cross_hub = args.to_hub.is_some();
        // The checkout's pinned identity guards only its own repository.
        let pinned_destination = !cross_hub && !explicit_repository;
        let expected = if pinned_destination {
            identity::expected_for_transport(repo, client.base())?
        } else {
            None
        };
        let requires_existing =
            pinned_destination && (expected.is_some() || identity::read(repo)?.is_some());
        let source = if requires_existing {
            Some(super::super::remote_request(
                client.get_agent(&checkout.owner, &checkout.name),
            )?)
        } else {
            lookup(client, &checkout.owner, &checkout.name)?
        };
        let accept_secret_findings = !crate::commands::auto_push::is_automatic()
            && (args.allow_secrets || config::allow_secrets());
        // The checkout's upstream says nothing about another repository's write access.
        let upstream = if explicit_repository {
            None
        } else {
            repo.upstream_url()
        };
        let write_denied = if is_read_only(me, &checkout.owner, upstream.as_deref()) {
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
                        org.creates_repositories(),
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
            if pinned_destination {
                identity::verify_transport_target(
                    repo,
                    &RemoteIdentity::new(&hub, &remote.agent_id)?,
                )?;
            }
            (
                checkout.owner.clone(),
                remote.visibility.clone(),
                remote.clone_url.clone(),
                Action::Existing(remote),
            )
        } else {
            if pinned_destination {
                ensure!(
                    expected.is_none() && identity::read(repo)?.is_none(),
                    "the pinned remote is unavailable; refusing to create a replacement"
                );
            }
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
            && pinned_destination
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
            explicit_hub: cross_hub,
            explicit_repository,
            source_identity: identity::read(repo)?,
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

    /// The destination as a person confirms it; [`Self::json`] stays the machine form.
    fn describe(&self) -> String {
        let action = match &self.action {
            Action::Create => "creates the repository",
            Action::Existing(_) => "existing repository",
        };
        format!(
            "{}/{} on {} ({}, {action}) as {}",
            self.owner, self.name, self.hub, self.visibility, self.account
        )
    }

    fn verify_account(&self, client: &Client) -> Result<()> {
        ensure!(
            identity::normalize_hub(client.base())? == self.hub
                && (self.explicit_hub || identity::normalize_hub(&config::hub_url())? == self.hub),
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

    /// Whether the destination is the checkout's own repository, which its pinned identity guards
    /// against a reused owner/name.
    fn pinned_destination(&self) -> bool {
        !self.explicit_hub && !self.explicit_repository
    }

    fn verify_before_mutation(&self, client: &Client, repo: &Repo) -> Result<()> {
        if self.explicit_hub {
            ensure!(
                identity::read(repo)? == self.source_identity,
                "the source repository binding changed during cross-Hub review"
            );
        }
        match &self.action {
            Action::Existing(expected) => {
                let observed = lookup(client, &self.owner, &self.name)?
                    .context("the publication destination disappeared")?;
                self.verify_observed(&observed, Some(&expected.agent_id))?;
                if self.pinned_destination() {
                    identity::verify_transport_target(
                        repo,
                        &RemoteIdentity::new(&self.hub, &observed.agent_id)?,
                    )?;
                }
            }
            Action::Create => {
                if let Some(observed) = lookup(client, &self.owner, &self.name)? {
                    self.verify_observed(&observed, None)?;
                    self.verify_write_access(client, &observed.agent_id)?;
                }
                if self.pinned_destination() {
                    ensure!(
                        identity::expected_for_transport(repo, &self.hub)?.is_none(),
                        "the RC identity forbids creating a replacement"
                    );
                }
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
        if self.pinned_destination() {
            identity::verify_transport_target(&repo, &identity)?;
        }
        let remote = Remote {
            owner: observed.owner,
            name: observed.name,
            push_url: observed.clone_url,
            identity,
            visibility: observed.visibility,
        };
        self.verify_remote(&remote)?;
        self.verify_write_access(client, &remote.identity.agent_id)?;
        if self.pinned_destination() {
            bind_publication_origin(
                &repo,
                &format!("{}/{}", self.owner, self.name),
                &remote.identity,
                &remote.push_url,
            )?;
        }
        Ok((repo, remote))
    }

    fn bind_delivered_origin(&self, repo: &Repo, remote: &Remote) -> Result<()> {
        self.verify_remote(remote)?;
        super::write_checkout_config(&format!("{}/{}", self.owner, self.name), || {
            ensure!(
                identity::read(repo)? == self.source_identity,
                "the source repository binding changed during cross-Hub publication"
            );
            if let Some(previous) = &self.source_identity {
                identity::rebind(repo, previous, &remote.identity)?;
            } else {
                identity::pin(repo, &remote.identity)?;
            }
            repo.set_remote(&remote.push_url)
        })
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
    client.require_push_access(&owner, &name, &expected.agent_id)?;
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
