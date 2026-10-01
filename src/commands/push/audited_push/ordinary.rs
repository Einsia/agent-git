//! Ordinary publication retains inspected source IDs and uses the same destination authority.

use super::*;
use crate::domain::repo::publication::PublicationPlan;
use crate::hub::git::SecretFindingsAcceptance;

pub(super) fn consent(
    hub: &str,
    credential: &credentials::HubCredential,
    agent_id: &str,
    url: &str,
    visibility: &str,
) -> AutoConsent {
    AutoConsent {
        version: 2,
        mode: PublicationMode::Ordinary,
        hub: hub.into(),
        account: credential.username.clone(),
        account_id: credential.account_id.clone(),
        agent_id: agent_id.into(),
        url: url.into(),
        visibility: visibility.into(),
        policy_digest: None,
        recipient: None,
    }
}

pub(super) fn run(
    args: &Args,
    client: Client,
    checkout: Checkout,
    repo: Repo,
    branches: &[String],
    selection_source: crate::commands::echo::Source,
    destination: Destination,
) -> CmdResult {
    let Destination {
        intent,
        local_target,
    } = destination;
    ensure!(
        !intent.encryption_enabled,
        "ordinary publication requires encryption disabled"
    );
    ensure!(
        !matches!(intent.action, Action::Copy(_)),
        "publish original local data to a separate destination with --to owner/new-repository; repository promotion cannot change encryption mode"
    );
    println!(
        "Destination encryption: disabled (ordinary source history; no viewing password required)."
    );
    let target = format!("{}/{}", intent.owner, intent.name);
    let automatic = std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV).is_some();
    ensure!(
        !automatic || !args.audit,
        "automatic publication cannot replace an interactive audit decision"
    );
    // Automatic ordinary publication needs only the repository's push.auto choice. It passes the
    // same identity, write-access and integrity checks as an explicit push, and a missing destination
    // is created with the non-interactive visibility default. Encrypted publication keeps its
    // saved-consent requirement.
    if automatic && !repo.auto_push_enabled()? {
        return Err(crate::commands::InteractionRequired(
            "automatic publication is disabled for this repository; enable push.auto or run an explicit push".into(),
        )
        .into());
    }

    // An existing separate destination keeps its own file line. The source's `main` would either
    // be refused as a non-fast-forward, failing the whole copy, or fast-forward the destination's
    // shared files to the source's. Only a destination this publication creates receives it.
    let plan = if intent.separate_target.is_some() && matches!(intent.action, Action::Existing(_)) {
        PublicationPlan::freeze_selected(&repo, branches)?
    } else {
        PublicationPlan::freeze(&repo, branches)?
    };
    require_original_history(&repo, &plan)?;
    let mut supervisor = SupervisorReply::from_env()?;
    if let Some(reply) = &supervisor {
        let Action::Existing(remote) = &intent.action else {
            anyhow::bail!("supervised publication requires an existing destination");
        };
        ensure!(
            branches.len() == 1,
            "supervised publication requires one session branch"
        );
        let candidates = receipts(
            &repo,
            &plan,
            &intent,
            &RemoteIdentity::new(&intent.hub, &remote.agent_id)?,
        )?;
        let candidate = candidates
            .iter()
            .find(|receipt| receipt.branch == reply.request.branch)
            .context("supervised publication has no session candidate")?;
        reply.request.verify(candidate)?;
        if reply.request.notification_id.is_some() {
            crate::domain::privacy_receipt::outbox::Entry::load(&repo, &reply.request)?
                .context("publication intent is missing")?;
        }
    }

    let limits = secrets::ScanLimits::DEFAULT;
    let spinner = ui::spinner("inspecting ordinary outgoing history…");
    let baseline = inspection_destination(args, &intent)?;
    let captured = CapturedPublication::capture_for_destination(
        &repo,
        &plan,
        limits.budget_bytes,
        baseline
            .as_ref()
            .map(|identity| (intent.url.as_str(), identity)),
    )?;
    let inspected = captured.inspect(limits);
    spinner.finish_and_clear();
    emit_push_target(&checkout, branches, selection_source);
    let complete = match inspected {
        ContentInspection::Complete(complete) => complete,
        ContentInspection::Blocked(blocked) => {
            show_inspection(blocked.report());
            ui::error(&blocked.reason().to_string());
            return Ok(inspection_failure_code(blocked.reason()));
        }
    };
    show_inspection(complete.report());
    let accept_findings = intent.accept_secret_findings;
    let displayed = intent.json();
    println!("Publication destination: {displayed}");
    println!("Publication refs: {}", serde_json::to_string(&plan)?);
    if args.show_preview {
        show_preview(complete.captured())?;
    }
    let reviewed = if args.audit {
        Some(super::super::audit::review(
            complete.captured(),
            &displayed,
        )?)
    } else {
        None
    };
    match confirm_publication(
        reviewed.as_ref().is_none_or(|review| review.complete()),
        args.dry_run,
        || {
            // A person at a terminal confirms the destination, and cancelling that prompt
            // declines. With nobody to ask, an ordinary push proceeds the way `git push` does;
            // the choice is made before prompting, so an unanswered prompt never publishes.
            // An audit always waits for its reviewer.
            if automatic
                || (!args.audit
                    && (std::env::var_os("AGIT_YES").is_some() || !ui::prompt::can_ask()))
            {
                return Ok(Some(true));
            }
            let answer = ui::prompt::confirm(&intent.confirmation(), false)?;
            Ok(answer)
        },
    )? {
        Decision::ReviewedOnly => {
            ui::info("Ordinary publication inspection complete; nothing was published.");
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

    complete.verify_source(&repo)?;
    intent.verify_account(&client)?;
    intent.verify_before_mutation(&client, &repo)?;
    if let Some(selection) = &local_target {
        selection.verify(&repo)?;
    }
    // Turning automatic publication off while inspection runs stops the push before any remote
    // write.
    ensure!(
        !automatic || repo.auto_push_enabled()?,
        "automatic publication was disabled during inspection"
    );
    let (repo, remote) = intent.materialize(&client, &checkout, repo)?;
    intent.verify_remote(&remote)?;
    let observed = lookup(&client, &intent.owner, &intent.name)?
        .context("the confirmed publication destination is unavailable")?;
    intent.verify_observed(&observed, Some(&remote.identity.agent_id))?;
    intent.verify_write_access(&client, &remote.identity.agent_id)?;
    complete.verify_source(&repo)?;
    if let Some(selection) = &local_target {
        selection.bind(&repo, &remote.identity)?;
    }
    intent.verify_source_binding(&repo)?;
    let receipts = receipts(&repo, &plan, &intent, &remote.identity)?;
    let selected = supervisor
        .as_ref()
        .map(|reply| {
            receipts
                .iter()
                .find(|receipt| receipt.branch == reply.request.branch)
                .cloned()
                .context("supervised publication has no candidate")
        })
        .transpose()?;
    if let (Some(reply), Some(candidate)) = (&mut supervisor, &selected) {
        reply.prepare(&repo, candidate)?;
    }
    // Receive advertisement reconciles legacy admissions, so cached refs or receipts cannot skip it.
    let report = complete
        .bind_destination_with_client(&repo, &remote.push_url, &remote.identity, client)?
        .publish(if accept_findings {
            SecretFindingsAcceptance::Accept
        } else {
            SecretFindingsAcceptance::Reject
        });
    show_publication(&report);
    if !report.ok() {
        ui::error(
            "publication did not complete; some content may already be published and unconfirmed attempts may have taken effect",
        );
        return Ok(publication_failure_code(&report));
    }
    verify_receipt_acknowledgements(&report, &receipts)?;
    for receipt in &receipts {
        intent.save_receipt(&repo, receipt)?;
    }
    if let (Some(reply), Some(candidate)) = (supervisor, selected) {
        reply.complete(&repo, candidate)?;
    }
    if intent.separate_target.is_some() {
        intent.verify_source_binding(&repo)?;
        ui::success(&format!(
            "Published ordinary history to the separate destination {target}."
        ));
        return Ok(ExitCode::Ok);
    }
    ensure!(
        repo.remote_url().as_deref() == Some(remote.push_url.as_str())
            && identity::read(&repo)?.as_ref() == Some(&remote.identity),
        "local destination changed during publication; remote refs were accepted but local tracking was not updated"
    );
    for head in plan.heads() {
        let branch = head
            .name()
            .strip_prefix("refs/heads/")
            .context("invalid publication head")?;
        repo.git(&[
            "update-ref",
            &format!("refs/remotes/origin/{branch}"),
            head.oid(),
        ])?;
    }
    ui::success(&format!("Published ordinary history to {target}."));
    Ok(ExitCode::Ok)
}

fn receipts(
    repo: &Repo,
    plan: &PublicationPlan,
    intent: &Intent,
    identity: &RemoteIdentity,
) -> Result<Vec<PublicationReceipt>> {
    let mut receipts = Vec::new();
    for head in plan.heads() {
        let metadata = crate::domain::storage::metadata_local(repo.root(), head.oid())?;
        if metadata.is_file_line() {
            continue;
        }
        let receipt = PublicationReceipt {
            version: 2,
            mode: PublicationMode::Ordinary,
            repository: format!("{}/{}", intent.owner, intent.name),
            branch: head
                .name()
                .strip_prefix("refs/heads/")
                .context("invalid publication head")?
                .into(),
            source: head.oid().into(),
            published: head.oid().into(),
            projected_session_id: Some(metadata.session),
            destination: identity.clone(),
            url: intent.url.clone(),
            policy_digest: None,
            recipient: None,
        };
        receipt.validate()?;
        receipts.push(receipt);
    }
    Ok(receipts)
}

pub(super) fn require_original_history(repo: &Repo, plan: &PublicationPlan) -> Result<()> {
    for commit in plan.commit_objects() {
        ensure!(
            repo.show_result(commit, "privacy/envelope.json")?.is_none(),
            "selected history contains encrypted snapshots; recover the complete required original history before publishing it to a different repository. Ciphertext-only historical conversion is not available"
        );
    }
    Ok(())
}

fn show_preview(captured: &CapturedPublication) -> Result<()> {
    use std::io::Read;
    let repo = Repo::at(captured.snapshot_git_dir()).exact_bare_root_inspection();
    for commit in captured.plan().commit_objects() {
        println!("Ordinary snapshot {commit}");
        let paths = repo.git_bytes_result(&["ls-tree", "-r", "-z", "--name-only", commit])?;
        for path in paths
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path =
                std::str::from_utf8(path).context("ordinary preview requires UTF-8 file names")?;
            let bytes =
                repo.git_bytes_result(&["cat-file", "blob", &format!("{commit}:{path}")])?;
            match std::str::from_utf8(&bytes) {
                Ok(text) => println!("{}", json!({"path":path,"text":text})),
                Err(_) => println!("{}", json!({"path":path,"binary_bytes":bytes.len()})),
            }
        }
    }
    for pointer in captured.pointers() {
        let mut bytes = Vec::new();
        captured.open_payload(pointer)?.read_to_end(&mut bytes)?;
        match std::str::from_utf8(&bytes) {
            Ok(text) => println!("{}", json!({"lfs_oid":pointer.oid,"text":text})),
            Err(_) => println!(
                "{}",
                json!({"lfs_oid":pointer.oid,"binary_bytes":bytes.len()})
            ),
        }
    }
    Ok(())
}
