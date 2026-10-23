//! Publication selects registered session authority independently of repository privacy flags.

use super::*;
use crate::domain::privacy_git::sessions::SessionPublicationPlan;
use crate::domain::repo::publication::PublicationPlan;

pub(super) fn run(
    args: &Args,
    client: Client,
    repo: Repo,
    branches: &[String],
    destination: Destination,
) -> CmdResult {
    let Destination {
        mut intent,
        local_target,
    } = destination;
    let repository = format!("{}/{}", intent.owner, intent.name);
    let automatic = std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV).is_some();
    ensure!(
        !automatic || !args.audit,
        "automatic publication cannot replace an interactive audit decision"
    );
    ensure!(
        !automatic || repo.auto_push_enabled()?,
        "automatic publication is disabled for this repository"
    );
    // Starting the local privacy worker overlaps its startup with the destination checks. It is
    // joined before preparation, so the preparation's own branch-name check never contends for it.
    let warmed = {
        let (root, names) = (repo.root().to_path_buf(), branches.to_vec());
        std::thread::spawn(move || {
            if let Ok(redactor) = crate::domain::redact::Redactor::try_this_machine()
                .and_then(|redactor| redactor.with_repository(&root))
            {
                for name in &names {
                    let _ = redactor.try_scrub(name);
                }
            }
        })
    };
    intent.verify_account(&client)?;
    intent.verify_before_mutation(&client, &repo)?;
    let original = PublicationPlan::freeze(&repo, branches)?;
    if matches!(intent.action, Action::Create) {
        ensure!(
            !args.dry_run,
            "a new destination needs an immutable repository identity before a session publication preview; publish without --dry-run to confirm its creation"
        );
        for commit in original.commit_objects() {
            crate::domain::storage::metadata_local(repo.root(), commit)?;
            ensure!(
                repo.show_result(commit, "privacy/envelope.json")?.is_none(),
                "a new destination requires preserved source authority for inherited ciphertext"
            );
        }
        let prompt = format!(
            "Create the {} repository {repository} on {} to prepare its session publication preview?",
            intent.visibility, intent.hub
        );
        let confirmed = if automatic || std::env::var_os("AGIT_YES").is_some() {
            true
        } else {
            ui::prompt::confirm(&prompt, false)?.ok_or_else(|| {
                crate::commands::InteractionRequired(
                    "first publication requires confirmation of repository creation; use --yes or an interactive terminal".into(),
                )
            })?
        };
        if !confirmed {
            ui::info("Repository creation cancelled.");
            return Ok(ExitCode::Ok);
        }
        original.verify(&repo, branches)?;
        intent.verify_account(&client)?;
        intent.verify_before_mutation(&client, &repo)?;
        if let Some(selection) = &local_target {
            selection.verify(&repo)?;
        }
        let created = super::super::super::remote_request(client.publish(&PublishRequest {
            name: intent.name.clone(),
            owner: (intent.owner != intent.account).then(|| intent.owner.clone()),
            public: intent.visibility == "public",
            repo_origins: Vec::new(),
        }))?;
        ensure!(
            created.owner == intent.owner
                && created.name == intent.name
                && created.push_url == intent.url,
            "created repository differs from the confirmed destination"
        );
        let observed = lookup(&client, &intent.owner, &intent.name)?
            .context("created repository is unavailable")?;
        intent.verify_observed(&observed, Some(&created.agent_id))?;
        intent.action = Action::Existing(observed);
    }
    let Action::Existing(remote) = &intent.action else {
        anyhow::bail!("session publication requires a registered repository destination");
    };
    let identity = RemoteIdentity::new(&intent.hub, &remote.agent_id)?;
    intent.verify_write_access(&client, &identity.agent_id)?;
    let selected_branches = original
        .heads()
        .iter()
        .map(|head| {
            head.name()
                .strip_prefix("refs/heads/")
                .context("invalid publication branch")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    let remote =
        crate::hub::git::FrozenPublication::advertised_refs_for(&repo, &intent.url, &identity)
            .context("cannot establish accepted publication history")?;
    let selected = SessionPublicationPlan::resolve(
        &repo,
        &selected_branches,
        &client,
        &repository,
        identity.clone(),
        remote,
        !args.dry_run,
    )?;
    let _ = warmed.join();
    let mut projected =
        ProjectedHistory::prepare_sessions(&repo, &selected_branches, &selected, &[])?;
    let receipts = projected.session_receipts(&repository, &identity, &intent.url)?;
    let mut supervisor = SupervisorReply::from_env()?;
    let supervised = supervisor
        .as_ref()
        .map(|reply| {
            ensure!(
                branches.len() == 1,
                "supervised publication requires one session branch"
            );
            let candidate = receipts
                .iter()
                .find(|receipt| receipt.branch == reply.request.branch)
                .context("supervised publication has no session candidate")?
                .clone();
            reply.request.verify(&candidate)?;
            if reply.request.notification_id.is_some() {
                crate::domain::privacy_receipt::outbox::Entry::load(&repo, &reply.request)?
                    .context("publication intent is missing")?;
            }
            Ok::<_, anyhow::Error>(candidate)
        })
        .transpose()?;
    let limits = secrets::ScanLimits::DEFAULT;
    let baseline = inspection_destination(args, &intent)?;
    let spinner = ui::spinner("inspecting outgoing session history…");
    let captured = CapturedPublication::capture_projected_for(
        &projected,
        limits.budget_bytes,
        &repo,
        baseline
            .as_ref()
            .map(|identity| (intent.url.as_str(), identity)),
        baseline
            .as_ref()
            .filter(|identity| **identity == selected.identity)
            .map(|_| selected.remote.clone()),
    )?;
    let inspected = captured.inspect(limits);
    spinner.finish_and_clear();
    let complete = match inspected {
        ContentInspection::Complete(complete) => complete,
        ContentInspection::Blocked(blocked) => {
            ui::error(&blocked.reason().to_string());
            return Ok(inspection_failure_code(blocked.reason()));
        }
    };
    if !args.dry_run
        && !args.audit
        && !args.show_preview
        && complete.captured().pointers().is_empty()
        && receipts_match_current(&repo, &receipts)?
        && (supervisor.is_some() || no_pending_publication_receipts(&repo, &receipts)?)
        && FrozenPublication::advertised_refs_match(&repo, projected.plan(), &intent.url, &identity)
            == Some(true)
    {
        projected.verify_source(&repo)?;
        complete.verify_source(projected.repo())?;
        intent.verify_account(&client)?;
        intent.verify_before_mutation(&client, &repo)?;
        intent.verify_write_access(&client, &identity.agent_id)?;
        if let Some(selection) = &local_target {
            selection.verify(&repo)?;
        }
        selected.verify_configs(&client, &repository)?;
        if let (Some(mut reply), Some(candidate)) = (supervisor, supervised) {
            reply.prepare(&repo, &candidate)?;
            reply.complete(&repo, candidate)?;
        }
        ui::info(format_args!("{repository}: already up to date."));
        return Ok(ExitCode::Ok);
    }
    let displayed = intent.json();
    let preview = projected.write_preview(&displayed, &repo, complete.captured())?;
    println!("Publication destination: {displayed}");
    println!("Privacy preview: {}", preview.display());
    if !automatic {
        super::super::preview::show(&projected, complete.captured(), args.show_preview)?;
    }
    let reviewed = if args.audit {
        Some(super::super::audit::review(
            complete.captured(),
            &displayed,
        )?)
    } else {
        None
    };
    let encrypted = receipts.iter().any(|receipt| receipt.mode.is_encrypted());
    let decision = confirm_publication(
        reviewed.as_ref().is_none_or(|review| review.complete()),
        args.dry_run,
        || {
            if automatic
                || (!args.audit
                    && (std::env::var_os("AGIT_YES").is_some()
                        || (!encrypted && !ui::prompt::can_ask())))
            {
                return Ok(Some(true));
            }
            let answer = ui::prompt::confirm(
                &format!(
                    "Publish the reviewed sessions to {repository} on {} ({})?",
                    intent.hub, intent.visibility
                ),
                false,
            )?;
            if answer.is_none() && !args.audit {
                return Err(crate::commands::InteractionRequired("session publication requires confirmation of its preview; use --yes or an interactive terminal".into()).into());
            }
            Ok(answer)
        },
    )?;
    match decision {
        Decision::ReviewedOnly => {
            ui::info("Session preview complete; nothing was published.");
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
    projected.verify_source(&repo)?;
    complete.verify_source(projected.repo())?;
    intent.verify_account(&client)?;
    intent.verify_before_mutation(&client, &repo)?;
    selected.verify_configs(&client, &repository)?;
    if let Some(selection) = &local_target {
        selection.verify(&repo)?;
    }
    ensure!(
        !automatic || repo.auto_push_enabled()?,
        "automatic publication was disabled during inspection"
    );
    let (repo, remote) = intent.materialize(&client, repo)?;
    intent.verify_remote(&remote)?;
    if let Some(selection) = &local_target {
        selection.bind(&repo, &remote.identity)?;
    }
    projected.verify_source(&repo)?;
    let prepared = complete.bind_projected_destination_with_client(
        projected.repo(),
        &repo,
        &remote.push_url,
        &remote.identity,
        client,
    )?;
    projected.prepare_acceptance()?;
    if let (Some(reply), Some(candidate)) = (&mut supervisor, &supervised) {
        reply.prepare(&repo, candidate)?;
    }
    let report = prepared.publish_with_session_receipts(
        projected.policy_digest(),
        projected.session_manifest(),
        &intent.visibility,
    )?;
    projected.confirm_acceptance(&report)?;
    show_publication(&report);
    if !report.ok() {
        ui::error("publication did not complete; unconfirmed attempts may have taken effect");
        return Ok(publication_failure_code(&report));
    }
    verify_receipt_acknowledgements(&report, &receipts)?;
    for receipt in &receipts {
        receipt.save(&repo)?;
    }
    if let (Some(reply), Some(candidate)) = (supervisor, supervised) {
        reply.complete(&repo, candidate)?;
    }
    ui::success(&format!("Published reviewed sessions to {repository}."));
    Ok(ExitCode::Ok)
}
