//! Privacy publication binds generated content and destination before confirmation.

use super::consent::AutoConsent;
use super::*;
use crate::commands::clone::Checkout;
use crate::domain::privacy_git::ProjectedHistory;
use crate::domain::privacy_receipt::{PublicationMode, PublicationReceipt, SupervisorReply};
use crate::hub::git::{
    CapturedPublication, ContentInspection, FrozenPublication, InspectionFailure, InspectionReport,
    PublicationReport, PublicationStatus,
};
use crate::hub::identity::RemoteIdentity;
use crate::hub::{Client, RemoteAgent};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

mod ordinary;
mod separate;

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
    selection_source: super::super::echo::Source,
) -> CmdResult {
    ensure!(
        client.credential_username().as_deref() == Some(me),
        "the selected account changed while preparing publication"
    );
    let device_local = crate::rc::local_repository::publication::is_device_local(&repo)?;
    let separate_target = if let Some(requested) = args.to.as_deref()
        && (args.separate || !device_local)
    {
        Some(separate::Selection::prepare(
            &repo,
            &checkout.slug(),
            requested,
            client.base(),
        )?)
    } else {
        None
    };
    let local_target = if separate_target.is_some() {
        None
    } else {
        crate::rc::local_repository::publication::Selection::prepare(
            &repo,
            args.to.as_deref(),
            client.base(),
        )?
    };
    let destination_checkout = local_target
        .as_ref()
        .map(|selection| selection.checkout(&checkout.path))
        .transpose()?
        .or(separate_target
            .as_ref()
            .map(|selection| selection.checkout(&checkout.path))
            .transpose()?);
    let intent = Intent::resolve(
        args,
        &client,
        me,
        destination_checkout.as_ref().unwrap_or(&checkout),
        &repo,
        separate_target,
    )?;
    ensure!(
        local_target.is_none() || !matches!(&intent.action, Action::Copy(_)),
        "a local RC publication requires a writable destination"
    );
    if !intent.encryption_enabled {
        return ordinary::run(
            args,
            client,
            checkout,
            repo,
            branches,
            selection_source,
            Destination {
                intent,
                local_target,
            },
        );
    }
    if intent.separate_target.is_some() {
        let original =
            crate::domain::repo::publication::PublicationPlan::freeze_selected(&repo, branches)?;
        ordinary::require_original_history(&repo, &original)?;
    }
    println!("Destination encryption: enabled (repository viewing key required).");
    let mut session_branches = Vec::new();
    for branch in branches {
        if meta::read_at_ref_result(&repo, &format!("refs/heads/{branch}"))?
            .is_some_and(|metadata| metadata.is_file_line())
        {
            if !args.all {
                ui::error(
                    "encrypted session publication excludes repository file lines; select a session branch",
                );
                return Ok(ExitCode::Precondition);
            }
            ui::info(format_args!("Skipping repository file line: {branch}"));
        } else {
            session_branches.push(branch.clone());
        }
    }
    if session_branches.is_empty() {
        ui::info("No session branches selected for encrypted publication.");
        return Ok(ExitCode::Ok);
    }
    let branches = session_branches.as_slice();
    let target = format!("{}/{}", intent.owner, intent.name);
    let target_id = match &intent.action {
        Action::Existing(remote) => Some(remote.agent_id.clone()),
        Action::Create => anyhow::bail!(
            "repository viewing key is not configured; run `agit privacy init {target}` before publication"
        ),
        Action::Copy(_) => anyhow::bail!(
            "encrypted repository promotion requires a supported historical-key delivery contract; publish available original history with --to owner/new-repository while retaining the source identity"
        ),
    };
    let destination_identity = RemoteIdentity::new(
        client.base(),
        target_id.as_deref().expect("existing destination"),
    )?;
    if let Some(code) = synchronize_declarations(&repo, &client, &intent, args.dry_run)? {
        return Ok(code);
    }
    let mut hub_policies = vec![(
        target.clone(),
        target_id.clone(),
        client.privacy_policy_sources(&target, target_id.as_deref())?,
    )];
    if let Action::Copy(source) = &intent.action {
        let repository = format!("{}/{}", source.owner, source.name);
        let policy = client.privacy_policy_sources(&repository, Some(&source.agent_id))?;
        hub_policies.push((repository, Some(source.agent_id.clone()), policy));
    }
    let sources = super::super::privacy::sources::Sources::bound_repository(
        &repo,
        &local_target
            .as_ref()
            .map_or_else(|| checkout.slug(), |target| target.repository.clone()),
        &client,
        &hub_policies
            .iter()
            .map(|(repository, id, _)| (repository.as_str(), id.as_deref()))
            .collect::<Vec<_>>(),
    )?;
    let mut additional =
        hub_policies
            .iter()
            .try_fold(Vec::new(), |mut rules, (_, _, source)| -> Result<_> {
                rules.extend(source.additional_rules()?);
                Ok(rules)
            })?;
    additional.extend(sources.additional_rules()?);
    let mut supervisor = SupervisorReply::from_env()?;
    let viewing = client
        .repository_publishing_key(&target, &destination_identity)?
        .require_current(&target)?;
    let recipient = viewing.viewing_recipient()?;
    let mut projected = ProjectedHistory::prepare_with_sources(
        &repo,
        branches,
        &recipient,
        &intent.url,
        &additional,
        &destination_identity,
    )?;
    let existing_receipts = match &intent.action {
        Action::Existing(remote) => Some(publication_receipts(
            &projected,
            &intent,
            &RemoteIdentity::new(&intent.hub, &remote.agent_id)?,
        )?),
        Action::Create | Action::Copy(_) => None,
    };
    if let Some(reply) = &supervisor {
        let Some(receipts) = &existing_receipts else {
            anyhow::bail!("supervised publication requires an existing destination");
        };
        ensure!(
            projected.source_heads().len() == 1 && receipts.len() == 1,
            "supervised publication requires one session branch"
        );
        reply.request.verify(&receipts[0])?;
        if reply.request.notification_id.is_some() {
            crate::domain::privacy_receipt::outbox::Entry::load(&repo, &reply.request)?
                .context("publication intent is missing")?;
        }
    }
    let automatic = std::env::var_os(super::super::auto_push::AUTOMATIC_ENV).is_some();
    ensure!(
        !automatic || !args.audit,
        "automatic publication cannot replace an interactive audit decision"
    );
    let auto_enabled = intent.separate_target.is_none() && repo.auto_push_enabled()?;
    let consent = |agent_id: &str| -> Result<AutoConsent> {
        let credential = credentials::load_checked(&intent.hub)?
            .context("the selected account is no longer signed in")?;
        Ok(AutoConsent {
            version: 1,
            mode: Default::default(),
            hub: intent.hub.clone(),
            account: intent.account.clone(),
            account_id: credential.account_id,
            agent_id: agent_id.into(),
            url: intent.url.clone(),
            visibility: intent.visibility.clone(),
            policy_digest: Some(projected.policy_digest().into()),
            recipient: Some(recipient.fingerprint()?),
        })
    };
    let automatic_consent = if automatic {
        let expected = match &intent.action {
            Action::Existing(remote) if auto_enabled => consent(&remote.agent_id)?,
            _ => return Err(super::super::InteractionRequired(
                "automatic privacy publication requires an enabled policy and a confirmed existing destination; run an explicit push first".into(),
            ).into()),
        };
        if !expected.matches(&repo)? {
            return Err(super::super::InteractionRequired(
                "automatic privacy publication requires renewed confirmation of its policy, recipient and destination; run an explicit push first".into(),
            ).into());
        }
        Some(expected)
    } else {
        None
    };
    let manual_auto_consent = if !automatic && auto_enabled {
        match &intent.action {
            Action::Existing(remote) => consent(&remote.agent_id)?.matches(&repo)?,
            _ => false,
        }
    } else {
        true
    };

    projected.verify_accepted_policy()?;
    let limits = secrets::ScanLimits::DEFAULT;
    let spinner = ui::spinner("inspecting privacy-projected outgoing history…");
    let baseline = inspection_destination(args, &intent)?;
    let captured = CapturedPublication::capture_projected_for(
        &projected,
        limits.budget_bytes,
        &repo,
        baseline
            .as_ref()
            .map(|identity| (intent.url.as_str(), identity)),
    )?;
    let captured = apply_copy_policy(captured, &repo, &client, &intent)?;
    let inspected = captured.inspect(limits);
    spinner.finish_and_clear();
    let complete = match inspected {
        ContentInspection::Complete(complete) => complete,
        ContentInspection::Blocked(blocked) => {
            emit_push_target(&checkout, branches, selection_source);
            show_inspection(blocked.report());
            ui::error(&blocked.reason().to_string());
            return Ok(inspection_failure_code(blocked.reason()));
        }
    };

    let skip_allowed = !args.dry_run
        && !args.show_preview
        && !args.audit
        && manual_auto_consent
        && matches!(&intent.action, Action::Existing(_));
    if skip_allowed
        && projected.verify_source(&repo).is_ok()
        && complete.verify_source(projected.repo()).is_ok()
        && !complete.has_findings()
        && complete.captured().pointers().is_empty()
        && existing_receipts.as_ref().is_some_and(|receipts| {
            receipts_match_current(&repo, receipts, &intent).unwrap_or(false)
        })
        && existing_receipts.as_ref().is_some_and(|receipts| {
            supervisor.is_some()
                || no_pending_publication_receipts(&repo, receipts).unwrap_or(false)
        })
        && current_publication_state_matches(
            &repo,
            &projected,
            &intent,
            (&client, &viewing),
            &hub_policies,
            &sources,
            local_target.as_ref(),
        )?
    {
        if let Some(mut reply) = supervisor {
            let receipt = existing_receipts
                .as_ref()
                .and_then(|receipts| receipts.first())
                .context("supervised publication has no candidate")?;
            reply.prepare(&repo, receipt)?;
            reply.complete(&repo, receipt.clone())?;
        }
        let display_target = if let [branch] = branches {
            format!("{target}@{branch}")
        } else {
            target.clone()
        };
        ui::info(format_args!(
            "{display_target} ({}): already up to date.",
            intent.visibility
        ));
        return Ok(ExitCode::Ok);
    }

    emit_push_target(&checkout, branches, selection_source);
    show_inspection(complete.report());
    if complete.has_findings() {
        ui::error(
            "privacy publication contains credential findings; update the policy or secret registrations before publishing",
        );
        return Ok(ExitCode::Policy);
    }

    let mut destination = intent.json();
    destination["authorize_automatic_policy"] = json!(auto_enabled && !automatic);
    destination["recipient_fingerprint"] = json!(recipient.fingerprint()?);
    println!(
        "Publication destination: {}",
        serde_json::to_string(&destination)?
    );
    println!(
        "Publication target: {}/{} ({})",
        intent.owner, intent.name, intent.visibility
    );
    let preview = projected.write_preview(&destination, &repo)?;
    println!("Privacy preview: {}", preview.display());
    println!(
        "Publication scope: session records and metadata; repository files and standalone attachments are excluded."
    );
    println!(
        "Privacy processing: {} snapshots; {} omitted items.",
        projected.reports().len(),
        projected
            .reports()
            .iter()
            .map(|r| r.omissions.len())
            .sum::<usize>()
    );
    if auto_enabled && !automatic {
        ui::info(
            "Confirmation also authorizes future automatic pushes under this repository policy, recipient and destination.",
        );
    }
    if !automatic {
        super::preview::show(&projected, &preview, args.show_preview)?;
    }
    // This owner retains the readable report through confirmation and publication.
    let reviewed = if args.audit {
        Some(super::audit::review(complete.captured(), &destination)?)
    } else {
        None
    };
    let decision = confirm_publication(
        reviewed.as_ref().is_none_or(|review| review.complete()),
        args.dry_run,
        || {
            if automatic_consent.is_some() {
                return Ok(Some(true));
            }
            if !args.audit && std::env::var_os("AGIT_YES").is_some() {
                return Ok(Some(true));
            }
            let answer = ui::prompt::confirm(&intent.confirmation(), false)?;
            if answer.is_none() && !args.audit {
                return Err(super::super::InteractionRequired("privacy publication requires confirmation of the generated preview; use --yes or an interactive terminal".into()).into());
            }
            Ok(answer)
        },
    )?;
    match decision {
        Decision::ReviewedOnly => {
            ui::info("Privacy preview complete; nothing was published.");
            return Ok(ExitCode::Ok);
        }
        Decision::Declined => {
            ui::info("Publication cancelled.");
            return Ok(ExitCode::Ok);
        }
        Decision::Incomplete => {
            ui::error("audit review was not complete; publication is blocked");
            return Ok(ExitCode::Policy);
        }
        Decision::Publish => {}
    }

    // Review and consent do not authorize a different account, source or destination.
    projected.verify_source(&repo)?;
    complete.verify_source(projected.repo())?;
    intent.verify_account(&client)?;
    intent.verify_before_mutation(&client, &repo)?;
    if let Some(local_target) = &local_target {
        local_target.verify(&repo)?;
    }
    verify_hub_policies(&client, &hub_policies, None)?;
    sources.verify()?;
    if let Some(expected) = &automatic_consent {
        ensure!(
            repo.auto_push_enabled()?
                && expected.matches(&repo)?
                && consent(&expected.agent_id)? == *expected,
            "automatic publication consent changed after preview"
        );
    }
    // The concrete destination ID is installed only after creation has been confirmed.
    let mut accepted = consent("")?;
    let (repo, remote) = intent.materialize(&client, &checkout, repo)?;
    projected.relocate(&repo)?;
    complete.verify_source(projected.repo())?;
    intent.verify_remote(&remote)?;
    let observed = lookup(&client, &intent.owner, &intent.name)?
        .context("the confirmed publication destination is unavailable")?;
    intent.verify_observed(&observed, Some(&remote.identity.agent_id))?;
    intent.verify_write_access(&client, &remote.identity.agent_id)?;
    verify_hub_policies(&client, &hub_policies, Some(&remote.identity.agent_id))?;
    sources.verify_remote()?;
    projected.verify_source(&repo)?;
    intent.verify_source_binding(&repo)?;
    drop(sources);
    let current_key = client
        .repository_publishing_key(&target, &remote.identity)?
        .require_current(&target)?;
    ensure!(
        current_key.public_key == viewing.public_key && current_key.recipient == viewing.recipient,
        "viewing recipient changed after preview"
    );
    if let Some(local_target) = &local_target {
        local_target.bind(&repo, &remote.identity)?;
    }
    projected.repo().set_remote(&remote.push_url)?;
    identity::pin(projected.repo(), &remote.identity)?;
    let receipts = publication_receipts(&projected, &intent, &remote.identity)?;
    let prepared = complete.bind_destination_with_client(
        projected.repo(),
        &remote.push_url,
        &remote.identity,
        client,
    )?;
    projected.prepare_acceptance()?;
    if let Some(reply) = &mut supervisor {
        reply.prepare(
            &repo,
            receipts
                .first()
                .context("supervised publication has no candidate")?,
        )?;
    }
    let report = prepared.publish_with_privacy_receipts(
        projected.policy_digest(),
        &recipient.fingerprint()?,
        &intent.visibility,
        &hub_policies[0].2.content_digest()?,
    )?;
    projected.confirm_acceptance(&report)?;
    show_publication(&report);
    if report.ok() {
        verify_receipt_acknowledgements(&report, &receipts)?;
        for receipt in &receipts {
            intent.save_receipt(&repo, receipt)?;
        }
        if let Some(reply) = supervisor {
            reply.complete(
                &repo,
                receipts
                    .into_iter()
                    .next()
                    .context("supervised publication has no result")?,
            )?;
        }
        if auto_enabled && !automatic {
            accepted.agent_id = remote.identity.agent_id.clone();
            accepted.save(&repo)?;
        }
        if remote.first_publish {
            println!("Visibility: {}", visibility_label(&remote.visibility));
        }
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

fn apply_copy_policy(
    captured: CapturedPublication,
    repo: &Repo,
    client: &Client,
    intent: &Intent,
) -> Result<CapturedPublication> {
    if intent.separate_target.is_none() {
        return Ok(captured);
    }
    let identities = match &intent.action {
        Action::Existing(remote) => super::super::secret_vault::copy_policy_identities(
            client,
            &intent.owner,
            &intent.name,
            &remote.agent_id,
        )?,
        Action::Create => Default::default(),
        Action::Copy(_) => anyhow::bail!("a separate publication cannot promote its source"),
    };
    captured.with_copy_policy(repo, identities)
}

fn inspection_destination(args: &Args, intent: &Intent) -> Result<Option<RemoteIdentity>> {
    if args.audit {
        return Ok(None);
    }
    match &intent.action {
        Action::Existing(remote) => Ok(Some(RemoteIdentity::new(&intent.hub, &remote.agent_id)?)),
        Action::Create | Action::Copy(_) => Ok(None),
    }
}

fn synchronize_declarations(
    repo: &Repo,
    client: &Client,
    intent: &Intent,
    dry_run: bool,
) -> Result<Option<ExitCode>> {
    if intent.separate_target.is_some() {
        return Ok(None);
    }
    if let Action::Existing(remote) = &intent.action {
        let identity = RemoteIdentity::new(client.base(), &remote.agent_id)?;
        if let Some(code) = super::super::secret_vault::synchronize_push_target(
            repo,
            client,
            &intent.owner,
            &intent.name,
            &identity,
            dry_run,
            intent.accept_secret_findings,
        )? {
            return Ok(Some(code));
        }
    }
    super::super::secret_vault::report_pending_declarations(repo)?;
    Ok(None)
}

fn verify_hub_policies(
    client: &Client,
    policies: &[(
        String,
        Option<String>,
        crate::hub::privacy::sources::PolicySources,
    )],
    created_target: Option<&str>,
) -> Result<()> {
    for (index, (repository, original_id, policy)) in policies.iter().enumerate() {
        let agent_id = if index == 0 {
            created_target.or(original_id.as_deref())
        } else {
            original_id.as_deref()
        };
        policy.verify_refresh(&client.privacy_policy_sources(repository, agent_id)?)?;
    }
    Ok(())
}

fn publication_receipts(
    projected: &ProjectedHistory,
    intent: &Intent,
    destination: &RemoteIdentity,
) -> Result<Vec<PublicationReceipt>> {
    projected
        .source_heads()
        .iter()
        .map(|source| {
            let published = projected
                .plan()
                .heads()
                .iter()
                .find(|head| head.name() == source.name())
                .context("generated publication is missing a selected branch")?;
            let branch = source
                .name()
                .strip_prefix("refs/heads/")
                .context("invalid publication branch")?;
            let Some((policy_digest, receipt_recipient)) = projected.receipt_binding(branch)?
            else {
                return Ok(None);
            };
            let receipt = PublicationReceipt {
                version: 1,
                mode: Default::default(),
                repository: format!("{}/{}", intent.owner, intent.name),
                branch: source
                    .name()
                    .strip_prefix("refs/heads/")
                    .context("invalid publication branch")?
                    .into(),
                source: source.oid().into(),
                published: published.oid().into(),
                projected_session_id: Some(
                    crate::domain::storage::metadata_local(
                        projected.repo().root(),
                        published.oid(),
                    )?
                    .session,
                ),
                destination: destination.clone(),
                url: intent.url.clone(),
                policy_digest: Some(policy_digest.into()),
                recipient: Some(receipt_recipient.into()),
            };
            receipt.validate()?;
            Ok(Some(receipt))
        })
        .collect::<Result<Vec<_>>>()
        .map(|receipts| receipts.into_iter().flatten().collect())
}

fn receipts_match_current(
    repo: &Repo,
    expected: &[PublicationReceipt],
    intent: &Intent,
) -> Result<bool> {
    for candidate in expected {
        let saved = if intent.separate_target.is_some() {
            PublicationReceipt::load_for_destination(
                repo,
                &candidate.branch,
                &candidate.destination,
            )?
        } else {
            PublicationReceipt::load(repo, &candidate.branch)?
        };
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

fn current_publication_state_matches(
    repo: &Repo,
    projected: &ProjectedHistory,
    intent: &Intent,
    key_check: (
        &Client,
        &crate::hub::privacy::repository_keys::PublishingKey,
    ),
    hub_policies: &[(
        String,
        Option<String>,
        crate::hub::privacy::sources::PolicySources,
    )],
    sources: &super::super::privacy::sources::Sources<'_>,
    local_target: Option<&crate::rc::local_repository::publication::Selection>,
) -> Result<bool> {
    let (client, viewing) = key_check;
    let Action::Existing(remote) = &intent.action else {
        return Ok(false);
    };
    intent.verify_account(client)?;
    intent.verify_before_mutation(client, repo)?;
    intent.verify_write_access(client, &remote.agent_id)?;
    if let Some(local_target) = local_target {
        local_target.verify(repo)?;
    }
    verify_hub_policies(client, hub_policies, Some(&remote.agent_id))?;
    sources.verify()?;
    let identity = RemoteIdentity::new(&intent.hub, &remote.agent_id)?;
    let current = client
        .repository_publishing_key(&format!("{}/{}", intent.owner, intent.name), &identity)?
        .require_current(&format!("{}/{}", intent.owner, intent.name))?;
    ensure!(
        current.public_key == viewing.public_key && current.recipient == viewing.recipient,
        "viewing recipient changed after preview"
    );
    Ok(
        FrozenPublication::advertised_refs_match(repo, projected.plan(), &intent.url, &identity)
            .unwrap_or(false),
    )
}

fn emit_push_target(checkout: &Checkout, branches: &[String], source: super::super::echo::Source) {
    let selections: Vec<_> = branches
        .iter()
        .map(|branch| {
            super::super::echo::Selection::new(format!("{}@{branch}", checkout.slug()), source)
                .role("source")
        })
        .collect();
    super::super::echo::emit("push", &selections);
}

fn show_inspection(report: &InspectionReport) {
    let scan = report.scan();
    super::super::report_binary_carriers(scan.binary_carriers);
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
    encryption_enabled: bool,
    accept_secret_findings: bool,
    action: Action,
    separate_target: Option<separate::Selection>,
}

impl Intent {
    fn resolve(
        args: &Args,
        client: &Client,
        me: &str,
        checkout: &Checkout,
        repo: &Repo,
        separate_target: Option<separate::Selection>,
    ) -> Result<Self> {
        let hub = identity::normalize_hub(client.base())?;
        let expected = identity::expected_for_transport(repo, client.base())?;
        let requires_existing =
            expected.is_some() || (separate_target.is_none() && identity::read(repo)?.is_some());
        let source = if requires_existing {
            Some(super::super::remote_request(
                client.get_agent(&checkout.owner, &checkout.name),
            )?)
        } else {
            lookup(client, &checkout.owner, &checkout.name)?
        };
        if let Some(selected) = &separate_target {
            selected.verify_lookup(source.as_ref())?;
        }
        let accept_secret_findings = std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV)
            .is_none()
            && (args.allow_secrets || config::allow_secrets());
        let copy = if is_read_only(me, &checkout.owner, repo.upstream_url().as_deref()) {
            match &source {
                Some(remote) => !matches!(
                    super::super::remote_request(client.push_access_with_secret_acceptance(
                        &checkout.owner,
                        &checkout.name,
                        &remote.agent_id,
                        accept_secret_findings && !remote.require_encryption_enabled()?,
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
            separate_target.is_none() || !copy,
            "the explicit publication destination requires write access; refusing to choose another repository"
        );
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
                expected.is_none()
                    && (separate_target.is_some() || identity::read(repo)?.is_none()),
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
        let encryption_enabled = match &action {
            Action::Existing(remote) => {
                let enabled = remote.require_encryption_enabled()?;
                ensure!(
                    args.encryption.is_none_or(|selected| selected == enabled),
                    "repository encryption mode is fixed at creation; create a different repository for the requested mode"
                );
                if separate_target.is_none()
                    && let Some(pinned) = identity::read(repo)?
                {
                    ensure!(
                        pinned == RemoteIdentity::new(&hub, &remote.agent_id)?,
                        "the publication destination differs from the pinned repository identity"
                    );
                }
                enabled
            }
            Action::Create | Action::Copy(_) => {
                if let Some(selected) = &separate_target {
                    selected.encryption_for_creation(repo, args.encryption)?
                } else {
                    repo.encryption_for_creation(args.encryption)?
                }
            }
        };
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
            encryption_enabled,
            accept_secret_findings: accept_secret_findings && !encryption_enabled,
            action,
            separate_target,
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
        json!({"hub":self.hub,"account":self.account,"owner":self.owner,"name":self.name,"url":self.url,"visibility":self.visibility,"encryption_enabled":self.encryption_enabled,"separate_destination":self.separate_target.is_some(),"action":action,"agent_id":identity,"source":source,"repo_origins":[],"publication":if self.encryption_enabled { "reviewed session history and encrypted originals" } else { "ordinary source history, shared files, tags and LFS" }})
    }

    fn confirmation(&self) -> String {
        let action = if self.separate_target.is_some() {
            "Publish a separate copy of the reviewed refs"
        } else {
            match &self.action {
                Action::Existing(_) => "Publish only the reviewed refs",
                Action::Create => {
                    "Ensure the destination exists with this audience, then publish only the reviewed refs"
                }
                Action::Copy(_) => {
                    "Ensure the destination exists with this audience, relocate this local checkout, then publish only the reviewed refs (no server history copy)"
                }
            }
        };
        format!(
            "{action} to {}/{} on {} ({}, encryption {})?",
            self.owner,
            self.name,
            self.hub,
            self.visibility,
            if self.encryption_enabled {
                "enabled"
            } else {
                "disabled"
            }
        )
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
            remote.require_encryption_enabled()? == self.encryption_enabled,
            "the publication encryption mode changed; repository mode is fixed at creation"
        );
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
        if let Some(selected) = &self.separate_target {
            selected.verify(repo)?;
        }
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
                    "the copy source changed during publication"
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
                        "the pinned copy source changed during publication"
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
                    encryption_enabled: self.encryption_enabled,
                    first_publish: false,
                }
            }
            Action::Create => {
                self.ensure_destination(client, &repo, self.separate_target.is_none())?
            }
            Action::Copy(_) => {
                unreachable!("copy materialization returns before existing or new publication")
            }
        };
        self.verify_remote(&remote)?;
        self.verify_write_access(client, &remote.identity.agent_id)?;
        if let Some(selected) = &self.separate_target {
            selected.bind(&repo, &remote)?;
        } else {
            bind_publication_origin(
                &repo,
                &format!("{}/{}", self.owner, self.name),
                &remote.identity,
                &remote.push_url,
            )?;
        }
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
                encryption_enabled: self.encryption_enabled,
            },
        )
    }

    fn verify_source_binding(&self, repo: &Repo) -> Result<()> {
        if let Some(selected) = &self.separate_target {
            selected.verify_source(repo)?;
        }
        Ok(())
    }

    fn save_receipt(&self, repo: &Repo, receipt: &PublicationReceipt) -> Result<()> {
        if self.separate_target.is_some() {
            receipt.save_for_destination(repo)
        } else {
            receipt.save(repo)
        }
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
                && remote.encryption_enabled == self.encryption_enabled
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

/// Readiness uses the same account, recipient, policy and endpoint inputs as protected push.
pub(crate) fn automatic_publication_consent(
    repo: &Repo,
    repository: &str,
    expected: &RemoteIdentity,
) -> Result<bool> {
    if !repo.auto_push_enabled()? {
        return Ok(false);
    }
    publication_consent(repo, repository, expected)?.matches(repo)
}

/// Explicit project enrollment reviews the same policy that unattended push later rechecks.
pub(crate) fn publication_consent(
    repo: &Repo,
    repository: &str,
    expected: &RemoteIdentity,
) -> Result<AutoConsent> {
    let client = Client::from_env_with_timeout(std::time::Duration::from_secs(5));
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
    if !remote.require_encryption_enabled()? {
        return Ok(ordinary::consent(
            &expected.hub,
            &credential,
            &remote.agent_id,
            &remote.clone_url,
            &remote.visibility,
        ));
    }
    let policy_sources = client.privacy_policy_sources(repository, Some(&expected.agent_id))?;
    let sources = super::super::privacy::sources::Sources::bound_repository(
        repo,
        repository,
        &client,
        &[(repository, Some(&expected.agent_id))],
    )?;
    let mut policy = crate::domain::privacy::PrivacyPolicy::load(repo)?;
    policy.mandatory.extend(policy_sources.additional_rules()?);
    policy.mandatory.extend(sources.additional_rules()?);
    let viewing = client
        .repository_publishing_key(repository, expected)?
        .require_current(repository)?;
    let recipient = viewing.viewing_recipient()?;
    Ok(AutoConsent {
        version: 1,
        mode: Default::default(),
        hub: expected.hub.clone(),
        account: credential.username,
        account_id: credential.account_id,
        agent_id: expected.agent_id.clone(),
        url: remote.clone_url,
        visibility: remote.visibility,
        policy_digest: Some(policy.digest()?),
        recipient: Some(recipient.fingerprint()?),
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
