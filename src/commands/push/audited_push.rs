//! Audited publication binds captured content and destination before an explicit final decision.

use super::*;
use crate::commands::clone::Checkout;
use crate::domain::repo::publication::PublicationPlan;
use crate::hub::git::{
    CapturedPublication, ContentInspection, InspectionFailure, InspectionReport, PublicationReport,
    PublicationStatus, SecretFindingsAcceptance,
};
use crate::hub::{Client, RemoteAgent};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// Ordinary promotion may construct its own Git command; audit cannot inherit routing overrides.
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
            "push --audit cannot inherit {key}; clear Git routing and injected configuration before retrying"
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
    ensure!(
        client.credential_username().as_deref() == Some(me),
        "the selected account changed while preparing audit"
    );
    let plan = PublicationPlan::freeze(&repo, branches)?;
    let limits = secrets::ScanLimits::DEFAULT;
    let spinner = ui::spinner("capturing and inspecting outgoing history and LFS payloads…");
    let captured = CapturedPublication::capture(&repo, &plan, limits.budget_bytes)?;
    let inspected = captured.inspect(limits);
    spinner.finish_and_clear();
    let complete = match inspected {
        ContentInspection::Complete(complete) => complete,
        ContentInspection::Blocked(blocked) => {
            show_inspection(blocked.report());
            ui::error(&blocked.reason().to_string());
            return Ok(inspection_failure_code(blocked.reason()));
        }
    };
    show_inspection(complete.report());
    if complete.has_findings() && !args.allow_secrets {
        ui::error("audit blocked: complete credential findings require --allow-secrets");
        return Ok(ExitCode::Policy);
    }
    if complete.has_findings() {
        ui::warning(
            "--allow-secrets explicitly accepts these complete deterministic findings only",
        );
    }

    let intent = Intent::resolve(args, &client, me, &checkout, &repo)?;
    let destination = intent.json();
    println!(
        "Audit destination: {}",
        serde_json::to_string(&destination)?
    );
    ui::info(
        "The interactive model review receives outgoing readable content, including full LOG history.",
    );
    if args.dry_run {
        ui::info(
            "Audit dry run: model review only; no repository creation, promotion or publication.",
        );
    }
    // This owner retains the readable report through confirmation and publication.
    let reviewed = super::audit::review(complete.captured(), &destination)?;
    let decision = confirm_publication(reviewed.complete(), args.dry_run, || {
        ui::prompt::confirm(&intent.confirmation(), false)
    })?;
    match decision {
        Decision::ReviewedOnly => {
            ui::info("Audit dry run complete; nothing was published.");
            return Ok(ExitCode::Ok);
        }
        Decision::Declined => {
            ui::info("Audited push cancelled; no repository was created, promoted or published.");
            return Ok(ExitCode::Ok);
        }
        Decision::Incomplete => {
            ui::error("audit review was not complete; publication is blocked");
            return Ok(ExitCode::Policy);
        }
        Decision::Publish => {}
    }

    // Review and consent do not authorize a different account, source or destination.
    complete.verify_source(&repo)?;
    intent.verify_account(&client)?;
    intent.verify_before_mutation(&client, &repo)?;
    let (repo, remote) = intent.materialize(&client, &checkout, repo)?;
    complete.verify_source(&repo)?;
    intent.verify_remote(&remote)?;
    let observed = lookup(&client, &intent.owner, &intent.name)?
        .context("the confirmed publication destination is unavailable")?;
    intent.verify_observed(&observed, Some(&remote.identity.agent_id))?;
    intent.verify_write_access(&client, &remote.identity.agent_id)?;
    let target = format!("{}/{}", intent.owner, intent.name);
    let prepared =
        complete.bind_destination_with_client(&repo, &remote.push_url, &remote.identity, client)?;
    let acceptance = if args.allow_secrets {
        SecretFindingsAcceptance::Accept
    } else {
        SecretFindingsAcceptance::Reject
    };
    let report = prepared.publish(acceptance);
    show_publication(&report);
    if report.ok() {
        ui::info(format_args!("Published the reviewed snapshot to {target}."));
        ui::info(
            "Local branch tracking was not updated; the remote publication result is shown above.",
        );
        Ok(ExitCode::Ok)
    } else {
        ui::error(
            "publication did not complete; some content may already be published and unconfirmed attempts may have taken effect",
        );
        Ok(publication_failure_code(&report))
    }
}

fn show_inspection(report: &InspectionReport) {
    let scan = report.scan();
    if !scan.hits.is_empty() {
        report_hits(&scan.hits, scan.truncated);
    }
    if !scan.unscanned.is_empty() {
        super::super::report_unscanned(&scan.unscanned);
    }
    println!(
        "Deterministic inspection: {} findings; {} binary Git objects; {} binary LFS payloads.",
        scan.hits.len(),
        report.binary_git_objects(),
        report.binary_lfs().len()
    );
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
    Copy(RemoteAgent),
}

#[derive(Debug)]
struct Intent {
    hub: String,
    account: String,
    owner: String,
    name: String,
    url: String,
    visibility: String,
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
        let source = lookup(client, &checkout.owner, &checkout.name)?;
        let expected = identity::expected_for_transport(repo, client.base())?;
        let copy = if is_read_only(me, &checkout.owner, repo.upstream_url().as_deref()) {
            match &source {
                Some(remote) => !matches!(
                    super::super::remote_request(client.push_access(
                        &checkout.owner,
                        &checkout.name,
                        &remote.agent_id,
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
        let (owner, visibility, url, action) = if copy {
            let source = source.context("the source repository is unavailable")?;
            verify_agent_location(&hub, &checkout.owner, &checkout.name, &source)?;
            identity::verify_transport_target(repo, &RemoteIdentity::new(&hub, &source.agent_id)?)?;
            if let Some(pinned) = identity::read(repo)? {
                ensure!(
                    pinned == RemoteIdentity::new(&hub, &source.agent_id)?,
                    "the source identity changed"
                );
            }
            ensure!(
                lookup(client, me, &checkout.name)?.is_none(),
                "the intended copy already exists on the Hub"
            );
            let local = config::repo_dir(me, &checkout.name)?;
            ensure!(
                !local.exists(),
                "the intended copy already has a local checkout"
            );
            (
                me.to_owned(),
                source.visibility.clone(),
                format!("{hub}/{me}/{}.git", checkout.name),
                Action::Copy(source),
            )
        } else if let Some(remote) = source {
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
                expected.is_none(),
                "the RC remote is unavailable; refusing to create a replacement"
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
        if !matches!(action, Action::Create) && (args.private || args.public) {
            ui::warning(
                "visibility flags only affect creation; the audited destination retains its existing audience",
            );
        }
        let intent = Self {
            hub,
            account: me.into(),
            owner,
            name: checkout.name.clone(),
            url,
            visibility,
            action,
        };
        intent.verify_account(client)?;
        Ok(intent)
    }

    fn json(&self) -> Value {
        let (action, source, identity) = match &self.action {
            Action::Create => ("ensure", Value::Null, Value::Null),
            Action::Existing(remote) => ("publish", Value::Null, json!(remote.agent_id)),
            Action::Copy(source) => (
                "ensure_and_relocate",
                json!({"owner":source.owner,"name":source.name,"agent_id":source.agent_id,"url":source.clone_url,"visibility":source.visibility}),
                Value::Null,
            ),
        };
        json!({"hub":self.hub,"account":self.account,"owner":self.owner,"name":self.name,"url":self.url,"visibility":self.visibility,"action":action,"agent_id":identity,"source":source,"repo_origins":[],"publication":"only reviewed refs and captured LFS payloads"})
    }

    fn confirmation(&self) -> String {
        let action = match &self.action {
            Action::Existing(_) => "Publish only the reviewed refs",
            Action::Create => {
                "Ensure the destination exists with this audience, then publish only the reviewed refs"
            }
            Action::Copy(_) => {
                "Ensure the destination exists with this audience, relocate this local checkout, then publish only the reviewed refs (no server history copy)"
            }
        };
        format!(
            "{action} to {}/{} on {} ({})?",
            self.owner, self.name, self.hub, self.visibility
        )
    }

    fn verify_account(&self, client: &Client) -> Result<()> {
        ensure!(
            identity::normalize_hub(client.base())? == self.hub
                && identity::normalize_hub(&config::hub_url())? == self.hub,
            "the selected Hub changed during audit"
        );
        let credential = credentials::load_checked(&self.hub)?
            .context("the selected account is no longer signed in")?;
        ensure!(
            credential.username == self.account
                && client.credential_username().as_deref() == Some(self.account.as_str()),
            "the selected account changed during audit"
        );
        Ok(())
    }

    fn verify_observed(&self, remote: &RemoteAgent, expected_id: Option<&str>) -> Result<()> {
        verify_agent_location(&self.hub, &self.owner, &self.name, remote)?;
        ensure!(
            remote.clone_url == self.url && remote.visibility == self.visibility,
            "the publication endpoint or audience changed during audit"
        );
        ensure!(
            expected_id.is_none_or(|id| remote.agent_id == id),
            "the publication identity changed during audit"
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
            Action::Copy(source) => {
                let observed = lookup(client, &source.owner, &source.name)?
                    .context("the copy source disappeared")?;
                ensure!(
                    observed.agent_id == source.agent_id
                        && observed.owner == source.owner
                        && observed.name == source.name
                        && observed.clone_url == source.clone_url
                        && observed.visibility == source.visibility,
                    "the copy source changed during audit"
                );
                if let Some(observed) = lookup(client, &self.owner, &self.name)? {
                    self.verify_observed(&observed, None)?;
                    self.verify_write_access(client, &observed.agent_id)?;
                }
                ensure!(
                    !config::repo_dir(&self.owner, &self.name)?.exists(),
                    "the intended copy already has a local checkout"
                );
                if let Some(pinned) = identity::read(repo)? {
                    ensure!(
                        pinned == RemoteIdentity::new(&self.hub, &source.agent_id)?,
                        "the pinned copy source changed during audit"
                    );
                }
            }
        }
        Ok(())
    }

    fn materialize(
        &self,
        client: &Client,
        checkout: &Checkout,
        repo: Repo,
    ) -> Result<(Repo, Remote)> {
        if let Action::Copy(source) = &self.action {
            let remote = self.ensure_destination(client, &repo, false)?;
            self.verify_remote(&remote)?;
            self.verify_write_access(client, &remote.identity.agent_id)?;
            let observed = lookup(client, &self.owner, &self.name)?
                .context("the prepared copy destination is unavailable")?;
            self.verify_observed(&observed, Some(&remote.identity.agent_id))?;
            let plan = super::super::clone::promote_to_prepared_destination(
                &checkout.path,
                source,
                &observed,
                &self.hub,
            )?;
            ensure!(
                plan.owner == self.owner
                    && plan.name == self.name
                    && plan.origin == self.url
                    && plan.identity == remote.identity,
                "the local promotion does not match the reviewed destination"
            );
            let repo = Repo::open(&config::repo_dir(&plan.owner, &plan.name)?)
                .context("the promoted checkout is unavailable")?;
            return Ok((repo, remote));
        }
        let remote = match &self.action {
            Action::Existing(expected) => {
                let observed = lookup(client, &self.owner, &self.name)?.context(
                    "the publication destination disappeared; refusing to create a replacement",
                )?;
                self.verify_observed(&observed, Some(&expected.agent_id))?;
                let identity = RemoteIdentity::new(&self.hub, &observed.agent_id)?;
                identity::verify_transport_target(&repo, &identity)?;
                Remote {
                    owner: observed.owner,
                    name: observed.name,
                    push_url: observed.clone_url,
                    identity,
                    visibility: observed.visibility,
                    first_publish: false,
                }
            }
            Action::Create => self.ensure_destination(client, &repo, true)?,
            Action::Copy(_) => {
                unreachable!("copy materialization returns before existing or new publication")
            }
        };
        self.verify_remote(&remote)?;
        self.verify_write_access(client, &remote.identity.agent_id)?;
        repo.set_remote(&remote.push_url)?;
        Ok((repo, remote))
    }

    fn ensure_destination(&self, client: &Client, repo: &Repo, pin_first: bool) -> Result<Remote> {
        // Live worktree origins are outside the captured publication and are never submitted.
        ensure_remote_with_options(
            client,
            &self.owner,
            &self.name,
            Some(self.visibility == "public"),
            &meta::Meta::new_file_line(),
            repo,
            RemotePreparation {
                pin_identity: pin_first,
                own_namespace: Some(self.account == self.owner),
            },
        )
    }

    fn verify_write_access(&self, client: &Client, agent_id: &str) -> Result<()> {
        ensure!(
            matches!(
                super::super::remote_request(client.push_access(
                    &self.owner,
                    &self.name,
                    agent_id,
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
                "the publication identity changed during audit"
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
