//! Publication selects registered session authority independently of repository privacy flags.

use super::*;
use crate::domain::privacy_git::sessions::SessionPublicationPlan;
use crate::domain::repo::publication::{PublicationPlan, is_raw_ancestor};

pub(super) fn run(
    args: &Args,
    client: Client,
    repo: Repo,
    branches: &[String],
    named: bool,
    slug: &str,
    destination: Destination,
) -> CmdResult {
    let retry_command = super::retry_push_command(args, slug, branches);
    // Read again before every part of a publication sent in several, so that dropping the project
    // or turning automatic publication off stops the parts not yet sent.
    let still_publishable = |repo: &Repo, branch: &str| -> Result<()> {
        if let Some(reason) = crate::commands::project::withheld(slug, branch)? {
            anyhow::bail!(
                "`{slug}@{branch}` will not be uploaded from this computer: {}",
                reason.describe()
            );
        }
        ensure!(
            std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV).is_none()
                || !crate::commands::project::blocks_auto_push(repo, slug, branch)?,
            "automatic publication of `{slug}@{branch}` was turned off"
        );
        Ok(())
    };
    let Destination {
        mut intent,
        local_target,
    } = destination;
    let repository = format!("{}/{}", intent.owner, intent.name);
    let reading = ui::publication::stage("Checking destination and publication history");
    let automatic = std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV).is_some();
    ensure!(
        !automatic || !args.audit,
        "automatic publication cannot replace an interactive audit decision"
    );
    ensure!(
        !automatic || crate::commands::auto_push::publication_allowed(&repo, &repository)?,
        "{}",
        crate::commands::auto_push::DISABLED_FOR_REPOSITORY
    );
    if automatic && intent.visibility == "public" {
        let enrolled = match &intent.action {
            Action::Existing(remote) => super::super::consent::confirms_public(
                &repo,
                &RemoteIdentity::new(&intent.hub, &remote.agent_id)?,
            )?,
            Action::Create => false,
        };
        if !enrolled {
            return Err(crate::commands::InteractionRequired(format!(
                "automatic upload into the public repository {repository} was not switched on by the person; ask them to run `agit config --repo {repository} push.auto true` themselves in a terminal, or publish this once with `agit push {repository}@<branch>`"
            ))
            .into());
        }
    }
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
    // A destination needs only the selected heads up front, whatever the length of their
    // history; a new one's first publication carries the whole history, which is listed and
    // checked below without the bound of one publication, since it may go in parts. A cross-Hub
    // delivery checks every selected commit for plain local history and is sent whole, so it
    // captures the whole history.
    let original = if intent.explicit_hub {
        PublicationPlan::freeze(&repo, branches)?
    } else {
        PublicationPlan::freeze_tips(&repo, branches)?
    };
    if intent.explicit_hub {
        let enveloped =
            crate::domain::privacy_git::enveloped_commits(&repo, original.commit_objects())?;
        for commit in original.commit_objects() {
            let metadata = crate::domain::storage::metadata_local(repo.root(), commit)?;
            ensure!(
                !enveloped.contains(commit)
                    && metadata
                        .session_privacy
                        .as_ref()
                        .is_none_or(|privacy| privacy.scheme
                            == crate::domain::session_privacy::SessionScheme::Plain),
                "cross-Hub delivery requires local plain session history"
            );
        }
    }
    let delivery_confirmed = if intent.explicit_hub && !args.dry_run && !args.audit {
        reading.finish();
        let prompt = format!(
            "Deliver branches {} as plain sessions to {repository} on {} ({}) and switch this checkout's Hub binding after upload?",
            branches.join(", "),
            intent.hub,
            intent.visibility
        );
        let confirmed = std::env::var_os("AGIT_YES").is_some()
            || ui::prompt::confirm(&prompt, false)?.ok_or_else(|| {
                crate::commands::InteractionRequired(
                    "cross-Hub delivery requires confirmation; use --yes or an interactive terminal".into()
                )
            })?;
        if !confirmed {
            ui::info("Cross-Hub delivery cancelled.");
            return Ok(ExitCode::Ok);
        }
        true
    } else {
        reading.finish();
        false
    };
    if matches!(intent.action, Action::Create) {
        ensure!(
            !args.dry_run,
            "a new destination needs an immutable repository identity before a session publication preview; publish without --dry-run to confirm its creation"
        );
        let heads: Vec<String> = original
            .heads()
            .iter()
            .map(|head| head.oid().to_owned())
            .collect();
        let history: Vec<String> = PublicationPlan::raw_history(&repo, &heads)?
            .into_iter()
            .collect();
        for commit in &history {
            crate::domain::storage::metadata_local(repo.root(), commit)?;
        }
        ensure!(
            crate::domain::privacy_git::enveloped_commits(&repo, &history)?.is_empty(),
            "a new destination requires preserved source authority for inherited ciphertext"
        );
        let prompt = format!(
            "Create the {} repository {repository} on {} to prepare its session publication preview?",
            intent.visibility, intent.hub
        );
        let confirmed = if automatic || delivery_confirmed || std::env::var_os("AGIT_YES").is_some()
        {
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
        let creating = ui::publication::stage("Creating publication destination");
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
        creating.finish();
    }
    let resolving = ui::publication::stage("Reading remote history and session modes");
    let Action::Existing(remote) = &intent.action else {
        anyhow::bail!("session publication requires a registered repository destination");
    };
    let identity = RemoteIdentity::new(&intent.hub, &remote.agent_id)?;
    intent.verify_write_access(&client, &identity.agent_id)?;
    // Writing a public repository only through the organization's base permission is the
    // person's own act: nothing goes public through it unless the person says so.
    let person_only = intent.visibility == "public"
        && intent.owner != intent.account
        && super::super::super::remote_request(client.writes_only_through_base(
            &intent.owner,
            &intent.name,
            &identity.agent_id,
        ))?;
    ensure!(
        !(person_only && automatic),
        "{repository} is public and you write to it as a member of its organization; automatic upload does not publish there, push it yourself"
    );
    let mut remote =
        crate::hub::git::FrozenPublication::advertised_refs_for(&repo, &intent.url, &identity)
            .context("cannot establish accepted publication history")?;
    let implicit_main = !args.all && !branches.iter().any(|branch| branch == "main");
    // A branch the Hub refused as withdrawn stays in this checkout and out of later
    // publications, or every push would meet the same refusal. Naming it asks again, since the
    // withdrawal may have been restored, and so asks again for every refused version tag, even
    // when only tags were refused after the branch itself was published. Asking plans them in
    // this publication only: the record lets a ref go once the Hub has accepted it, so whatever
    // is still withdrawn, or was never answered for, stays set aside. An automatic push names its
    // branch too, but never asks again, and fails rather than report a push it did not make.
    let target = match branches {
        [branch] => format!("{repository}@{branch}"),
        _ => repository.clone(),
    };
    let withheld = crate::domain::privacy_git::withheld_refs(&repo, &identity)
        .map_err(|error| inconsistent(error, unsent_outcome(&target, 0, 0)))?;
    let ask_again = named && !automatic;
    let mut asked_again = Vec::new();
    let mut set_aside = Vec::new();
    let mut selected_branches = Vec::new();
    for head in original.heads() {
        let branch = head
            .name()
            .strip_prefix("refs/heads/")
            .context("invalid publication branch")?;
        if withheld.contains(head.name()) {
            if !ask_again {
                ui::info(format_args!(
                    "{}",
                    ui::dim(&format!("  skipping {branch} (withdrawn on the Hub)"))
                ));
                set_aside.push(branch.to_owned());
                continue;
            }
            asked_again.push(head.name().to_owned());
        }
        // A session push cannot require reconciling an independently advanced shared-file line.
        // Explicit selections still admit main through the ordinary publication checks.
        if branch == "main"
            && implicit_main
            && let Some(remote_head) = remote.refs.get(head.name())
            && !is_raw_ancestor(&repo, remote_head, head.oid())?
        {
            #[cfg(windows)]
            use crate::ui::quote_powershell_argument as quote_arg;
            #[cfg(not(windows))]
            use crate::ui::session::shell_arg as quote_arg;
            let target = quote_arg(&format!("{repository}@main"));
            ui::info(format_args!(
                "main left as on the Hub; run `agit pull {target}` to update it"
            ));
            continue;
        }
        selected_branches.push(branch.to_owned());
    }
    if ask_again {
        asked_again.extend(
            withheld
                .iter()
                .filter(|name| name.starts_with("refs/tags/"))
                .cloned(),
        );
    }
    // Only an automatic push sets aside a branch it names. The shared-file line may still be
    // publishable, but the branch the push was for is not, so it fails before any request.
    if named && !set_aside.is_empty() {
        let _ = warmed.join();
        resolving.finish();
        ui::error(&format!(
            "{repository}@{} is withdrawn on the Hub; it stays in this checkout and was not pushed",
            set_aside.join(", ")
        ));
        return Ok(ExitCode::Policy);
    }
    if selected_branches.is_empty() {
        let _ = warmed.join();
        resolving.finish();
        ui::info(format_args!("{repository}: nothing to publish."));
        return Ok(ExitCode::Ok);
    }
    let mut supervisor = SupervisorReply::from_env()?;
    // The reviewed heads bound the whole run: no part goes past them, so a turn saved after the
    // confirmation waits for the next push.
    let reviewed_heads: std::collections::BTreeMap<String, String> = original
        .heads()
        .iter()
        .filter_map(|head| {
            let branch = head.name().strip_prefix("refs/heads/")?;
            Some((branch.to_owned(), head.oid().to_owned()))
        })
        .collect();
    let push_limits = client
        .push_limits()
        .unwrap_or(crate::hub::PushLimits::UNPUBLISHED);
    let explicit_hub = intent.explicit_hub;
    let plan_parts = |repo: &Repo,
                      remote: &crate::hub::git::RemoteRefs,
                      branch: &str|
     -> Result<(Vec<String>, usize)> {
        if explicit_hub {
            return Ok((vec![reviewed_heads[branch].clone()], 0));
        }
        SessionPublicationPlan::plan_parts(
            repo,
            branch,
            &reviewed_heads[branch],
            &identity,
            remote,
            push_limits.max_new_commits,
        )
    };
    // A history longer than one publication goes in parts, one branch at a time; otherwise every
    // selected branch goes in one publication, as always.
    let mut plans = Vec::new();
    let mut added = 0;
    for branch in &selected_branches {
        // Planning reads the accepted record, so its refusal comes before anything is sent.
        let (cuts, commits) = plan_parts(&repo, &remote, branch)
            .map_err(|error| inconsistent(error, unsent_outcome(&target, 0, 0)))?;
        added += commits;
        plans.push(cuts);
    }
    let in_parts = plans.iter().any(|cuts| cuts.len() > 1);
    ensure!(
        !in_parts || supervisor.is_none(),
        "this session has more history than one publication carries, so it is published in parts; publish it once with `{retry_command}`"
    );
    let total: usize = if in_parts {
        plans.iter().map(Vec::len).sum()
    } else {
        1
    };
    if in_parts {
        ui::publication::notice(format!(
            "{added} new commits go to {repository} in {total} parts, cut at {}",
            plans
                .iter()
                .flatten()
                .map(|cut| cut.chars().take(12).collect::<String>())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // With nobody to confirm the push (no terminal and no --yes, and no automatic upload or
    // confirmed delivery standing in for the person), a part goes out only if it may on its own:
    // plain sessions to a private repository. The first part's yes then covers no later part that
    // carries encrypted sessions; that part stops before it is sent.
    let nobody_to_ask = !automatic
        && !delivery_confirmed
        && !args.audit
        && std::env::var_os("AGIT_YES").is_none()
        && !ui::prompt::can_ask();
    let _ = warmed.join();
    // The first publication resolves its sessions under the stage that read the remote history,
    // so a refusal there stops that stage; each later part opens its own.
    let mut reading_history = Some(resolving);
    let mut branch_index = if in_parts {
        plans.iter().position(|cuts| !cuts.is_empty()).unwrap_or(0)
    } else {
        0
    };
    let mut cuts = plans.get(branch_index).cloned().unwrap_or_default();
    // Branches with no new history may still owe tags (one added to an earlier commit, or a
    // version tag the Hub lacks); they go together after the branches sent in parts.
    let mut idle: Vec<String> = if in_parts {
        selected_branches
            .iter()
            .zip(&plans)
            .filter(|(_, cuts)| cuts.is_empty())
            .map(|(branch, _)| branch.clone())
            .collect()
    } else {
        Vec::new()
    };
    let mut finishing = false;
    let mut published = 0usize;
    let mut repo = repo;
    loop {
        let part = published + 1;
        let sent = published;
        // Nothing of this part reaches the Hub before its upload; the parts before it did.
        let unsent =
            |error: anyhow::Error| inconsistent(error, unsent_outcome(&target, sent, total));
        let part_branches: Vec<String> = if finishing {
            idle.clone()
        } else if in_parts {
            vec![selected_branches[branch_index].clone()]
        } else {
            selected_branches.clone()
        };
        let resolving = reading_history
            .take()
            .unwrap_or_else(|| ui::publication::stage(format!("Preparing part {part} of {total}")));
        // In parts, every publication stops at what was confirmed: a part at its cut, and the
        // branches that only owe tags at their reviewed heads.
        let pins: std::collections::BTreeMap<String, String> = if finishing {
            part_branches
                .iter()
                .map(|branch| (branch.clone(), reviewed_heads[branch].clone()))
                .collect()
        } else if in_parts {
            part_branches
                .iter()
                .map(|branch| (branch.clone(), cuts[0].clone()))
                .collect()
        } else {
            std::collections::BTreeMap::new()
        };
        if part > 1 {
            // A later part checks the destination as it is now: a repository answer the client
            // reuses from before the previous part landed would hide an audience that changed
            // since, and the first part's confirmation covers only the audience it named.
            client.forget_probes();
            for branch in &part_branches {
                still_publishable(&repo, branch).with_context(|| {
                    format!("{published} of {total} parts are published; nothing more was sent")
                })?;
            }
        }
        let mut selected = SessionPublicationPlan::resolve(
            &repo,
            &part_branches,
            &client,
            &repository,
            identity.clone(),
            remote.clone(),
            !args.dry_run,
            intent.explicit_hub,
            pins,
        )
        .map_err(unsent)?;
        selected.asking_again = asked_again.iter().cloned().collect();
        resolving.finish();
        ui::publication::notice(format!(
            "Repository visibility: {}; session modes: {} encrypted, {} plain (new publication)",
            intent.visibility,
            selected
                .configs
                .values()
                .filter(|config| config.config.privacy.scheme
                    == crate::domain::session_privacy::SessionScheme::SessionOwnerV2)
                .count(),
            selected
                .configs
                .values()
                .filter(|config| config.config.privacy.scheme
                    == crate::domain::session_privacy::SessionScheme::Plain)
                .count(),
        ));
        let mut projected = ProjectedHistory::prepare_sessions(&repo, &part_branches, &selected, &[])
        .map_err(|error| {
            if error.downcast_ref::<crate::domain::privacy_git::MissingAcceptedObjects>().is_some() {
                #[cfg(windows)]
                use crate::ui::quote_powershell_argument as quote_arg;
                #[cfg(not(windows))]
                use crate::ui::session::shell_arg as quote_arg;
                error.context(format!("fetch the accepted public history with `agit fetch {}` and retry `{retry_command}`; keep this checkout to preserve unpublished commits", quote_arg(&repository)))
            } else {
                unsent(error)
            }
        })?;
        let binding = ui::publication::stage("Reading prepared snapshot bindings");
        let receipts = projected.session_receipts(&repository, &identity, &intent.url)?;
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
        binding.finish();
        let inspecting = ui::publication::stage("Checking outgoing objects and file payloads");
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
        let complete = match inspected {
            ContentInspection::Complete(complete) => complete,
            ContentInspection::Blocked(blocked) => {
                ui::error(&blocked.reason().to_string());
                if in_parts {
                    ui::hint(&format!(
                        "{published} of {total} parts are published; part {part} was not sent"
                    ));
                }
                return Ok(inspection_failure_code(blocked.reason()));
            }
        };
        inspecting.finish();
        let status = ui::publication::stage("Checking publication status");
        let synchronized = (!in_parts || finishing)
            && !args.dry_run
            && !args.audit
            && !args.show_preview
            && complete.captured().pointers().is_empty()
            && receipts_match_current(&repo, &receipts)?
            && (supervisor.is_some() || no_pending_publication_receipts(&repo, &receipts)?)
            // The advertisement this publication was planned from decides "already published"; a
            // second listing only adds a Hub round trip, since nothing is sent on this path.
            && FrozenPublication::refs_match(projected.plan(), &selected.remote);
        status.finish();
        if synchronized && finishing {
            break;
        }
        if synchronized {
            let checking = ui::publication::stage("Rechecking synchronized publication");
            projected.verify_source(&repo)?;
            complete.verify_source(projected.repo())?;
            intent.verify_account(&client)?;
            intent.verify_before_mutation(&client, &repo)?;
            intent.verify_write_access(&client, &identity.agent_id)?;
            if let Some(selection) = &local_target {
                selection.verify(&repo)?;
            }
            // Nothing is sent on this path, so a registration can only matter to the receipt a
            // supervisor records from it.
            if supervisor.is_some() {
                selected.verify_configs(&client, &repository)?;
            }
            let (repo, destination) = intent.materialize(&client, repo)?;
            if intent.explicit_hub {
                intent.bind_delivered_origin(&repo, &destination)?;
            }
            if let (Some(mut reply), Some(candidate)) = (supervisor, supervised) {
                reply.prepare(&repo, &candidate)?;
                reply.complete(&repo, candidate)?;
            }
            checking.finish();
            ui::info(format_args!("{repository}: already up to date."));
            return Ok(ExitCode::Ok);
        }
        let displayed = intent.json();
        let previewing = ui::publication::stage("Writing publication preview");
        let mut preview = super::super::preview::prepare(
            &projected,
            &displayed,
            &repo,
            complete.captured(),
            args.show_preview && !automatic,
        )?;
        if in_parts {
            // Every part keeps its own preview, as auditable as a single publication's.
            let numbered = preview
                .path
                .with_file_name(format!("publication-preview-part-{part}.json"));
            std::fs::rename(&preview.path, &numbered)?;
            preview.path = numbered;
        }
        previewing.finish();
        let interactive_part = part == 1 || args.audit || (args.show_preview && !automatic);
        if interactive_part {
            println!("Publication destination: {}", intent.describe());
        }
        println!("Privacy preview: {}", preview.path.display());
        if !automatic && interactive_part {
            preview.show(&projected)?;
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
        let public = intent.visibility == "public";
        let decision = confirm_publication(
            reviewed.as_ref().is_none_or(|review| review.complete()),
            args.dry_run,
            || {
                if !interactive_part && encrypted && nobody_to_ask {
                    return Err(crate::commands::InteractionRequired(format!(
                        "{published} of {total} parts are published; part {part} carries encrypted sessions, which need the person's confirmation: run `{retry_command}` in a terminal, or pass --yes once they have confirmed it in this conversation"
                    ))
                    .into());
                }
                // The first part's answer covers every later part.
                if !interactive_part {
                    return Ok(Some(true));
                }
                let answered = Answer {
                    person_only,
                    public,
                    delivery_confirmed,
                    automatic,
                    audit: args.audit,
                    agit_yes: std::env::var_os("AGIT_YES").is_some(),
                    encrypted,
                    can_ask: ui::prompt::can_ask(),
                }
                .without_asking();
                if answered == Some(true) {
                    if person_only {
                        crate::hub::git::confirm_person_publication();
                    }
                    return Ok(Some(true));
                }
                if answered == Some(false) {
                    return Err(crate::commands::InteractionRequired(format!(
                        "{repository} is public and you write to it as a member of its organization; confirm what goes public at an interactive terminal or with --yes"
                    ))
                    .into());
                }
                if public && !args.audit && !ui::prompt::can_ask() {
                    return Err(crate::commands::InteractionRequired("publishing to a public repository needs the person's confirmation: run it in a terminal, or pass --yes once they have confirmed it in this conversation".into()).into());
                }
                let question = match (person_only, in_parts, part == 1) {
                    (true, false, _) => format!(
                        "{repository} is public and you write to it as a member of its organization. Publish the {} reviewed session(s) and file changes to everyone?",
                        receipts.len()
                    ),
                    (true, true, true) => format!(
                        "{repository} is public and you write to it as a member of its organization. Publish the reviewed sessions and file changes to everyone, in {total} parts starting with this one?"
                    ),
                    (true, true, false) => format!(
                        "{repository} is public and you write to it as a member of its organization. Publish part {part} of {total} of the reviewed sessions and file changes to everyone?"
                    ),
                    (false, true, true) => format!(
                        "Publish the reviewed sessions to {repository} on {} ({}) in {total} parts, starting with this one?",
                        intent.hub, intent.visibility
                    ),
                    (false, true, false) => format!(
                        "Publish part {part} of {total} of the reviewed sessions to {repository} on {} ({})?",
                        intent.hub, intent.visibility
                    ),
                    (false, false, _) => format!(
                        "Publish the reviewed sessions to {repository} on {} ({})?",
                        intent.hub, intent.visibility
                    ),
                };
                let answer = ui::prompt::confirm(&question, false)?;
                if answer.is_none() && !args.audit {
                    return Err(crate::commands::InteractionRequired("session publication requires confirmation of its preview; use --yes or an interactive terminal".into()).into());
                }
                if person_only && answer == Some(true) {
                    crate::hub::git::confirm_person_publication();
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
                if published > 0 {
                    ui::info(format_args!(
                        "Publication stopped; {published} of {total} parts are published."
                    ));
                } else {
                    ui::info("Publication cancelled.");
                }
                return Ok(ExitCode::Ok);
            }
            Decision::Incomplete => {
                ui::error("audit review was not complete; publication is blocked");
                return Ok(ExitCode::Policy);
            }
            Decision::Publish => {}
        }
        let checking = ui::publication::stage("Rechecking source and destination before upload");
        projected.verify_source(&repo)?;
        complete.verify_source(projected.repo())?;
        intent.verify_account(&client)?;
        intent.verify_before_mutation(&client, &repo)?;
        selected.verify_configs(&client, &repository)?;
        if let Some(selection) = &local_target {
            selection.verify(&repo)?;
        }
        ensure!(
            !automatic || crate::commands::auto_push::publication_allowed(&repo, &repository)?,
            "automatic publication was disabled during inspection"
        );
        // The policy checked before the first part is read again before every later one.
        if part > 1 {
            for branch in &part_branches {
                still_publishable(&repo, branch)?;
            }
        }
        let (materialized, destination) = intent.materialize(&client, repo)?;
        repo = materialized;
        let hub = client.base().to_owned();
        intent.verify_remote(&destination)?;
        if let Some(selection) = &local_target {
            selection.bind(&repo, &destination.identity)?;
        }
        projected.verify_source(&repo)?;
        let prepared = complete.bind_projected_destination_with_client(
            projected.repo(),
            &repo,
            &destination.push_url,
            &destination.identity,
            client.clone(),
        )?;
        projected.prepare_acceptance().map_err(unsent)?;
        if let (Some(reply), Some(candidate)) = (&mut supervisor, &supervised) {
            reply.prepare(&repo, candidate)?;
        }
        checking.finish();
        let report = prepared.publish_with_session_receipts(
        projected.policy_digest(),
        projected.session_manifest(),
        &intent.visibility,
        &selected.remote,
    ).with_context(|| {
            // The parts before this one are acknowledged and stay published, and a rerun plans only
            // what the Hub lacks, so the message names them instead of saying nothing was published.
            if published > 0 {
                format!("{published} of {total} parts are published; part {part} was not sent. Rerunning `{retry_command}` continues from what the Hub holds after resolving the reported error")
            } else {
                format!("no branch or tag refs were published by this push; rerunning `{retry_command}` is safe after resolving the reported error")
            }
        })?;
        // A branch the Hub acknowledged is uploaded whatever fails after it, in the tag phase or
        // while saving receipts, and a retry finds it up to date; so it counts now.
        let updated = report
            .heads
            .iter()
            .flat_map(|phase| &phase.attempts)
            .flat_map(|attempt| &attempt.refs)
            .any(|reference| matches!(reference.status, PublicationStatus::Updated));
        if updated {
            crate::telemetry::activation::reached(
                crate::telemetry::activation::Milestone::FirstUpload,
                &hub,
            );
        }
        let recording = ui::publication::stage("Recording publication acknowledgements");
        projected.confirm_acceptance(&report).map_err(|error| {
            inconsistent(
                error,
                format!("{target} may already be published on the Hub, but agit could not record which local commits it published"),
            )
        })?;
        show_publication(&report);
        let accepted: Vec<String> = [&report.heads, &report.tags]
            .into_iter()
            .flatten()
            .flat_map(|phase| &phase.attempts)
            .flat_map(|attempt| &attempt.refs)
            .filter(|published| published.status != PublicationStatus::Unconfirmed)
            .map(|published| published.reference.name().to_owned())
            .filter(|name| selected.asking_again.contains(name))
            .collect();
        if !accepted.is_empty() {
            crate::domain::privacy_git::release_refs(&repo, &identity, &accepted).map_err(|error| {
                inconsistent(
                    error,
                    format!("{target} is published on the Hub, but agit could not record that the Hub accepted the refs it had set aside as withdrawn"),
                )
            })?;
        }
        let withdrawn: Vec<String> = [&report.heads, &report.tags]
            .into_iter()
            .flatten()
            .flat_map(|phase| phase.withdrawn.iter().cloned())
            .collect();
        if !withdrawn.is_empty() {
            crate::domain::privacy_git::withhold_refs(&repo, &identity, &withdrawn).map_err(|error| {
                inconsistent(
                    error,
                    format!("the Hub refused refs of {target} as withdrawn, but agit could not record them, so the next push asks the Hub about them again"),
                )
            })?;
            if in_parts {
                ui::error(&format!(
                    "part {part} of {total} did not complete; {published} parts before it are published"
                ));
            }
            ui::error(&format!(
                "withdrawn on the Hub, so not published: {}",
                withdrawn.join(", ")
            ));
            #[cfg(windows)]
            use crate::ui::quote_powershell_argument as quote_arg;
            #[cfg(not(windows))]
            use crate::ui::session::shell_arg as quote_arg;
            let again = withdrawn
                .iter()
                .find_map(|name| name.strip_prefix("refs/heads/"))
                .map(|branch| {
                    format!(
                        "`agit push {}` publishes it",
                        quote_arg(&format!("{slug}@{branch}"))
                    )
                })
                .unwrap_or_else(|| "naming its branch publishes them".into());
            ui::hint(&format!(
                "they stay in this checkout and later pushes leave them out; once the withdrawal is restored on the Hub, {again} again"
            ));
            let remaining: Vec<String> = branches
                .iter()
                .filter(|branch| !withdrawn.contains(&format!("refs/heads/{branch}")))
                .cloned()
                .collect();
            if !remaining.is_empty() {
                let retry = super::retry_push_command(args, slug, &remaining);
                ui::error(&publication_recovery(&report, &retry, &repository));
            }
            return Ok(ExitCode::Policy);
        }
        if !report.ok() {
            if in_parts {
                ui::error(&format!(
                    "part {part} of {total} did not complete; {published} parts before it are published"
                ));
            }
            ui::error(&publication_recovery(&report, &retry_command, &repository));
            return Ok(publication_failure_code(&report));
        }
        verify_receipt_acknowledgements(&report, &receipts)?;
        if intent.explicit_hub {
            intent.bind_delivered_origin(&repo, &destination)?;
        }
        for receipt in &receipts {
            receipt.save(&repo)?;
            crate::domain::privacy_receipt::save_acknowledged_tags(
                &repo,
                receipt,
                projected.source_tags(),
            )?;
        }
        if let (Some(reply), Some(candidate)) = (supervisor.take(), supervised) {
            reply.complete(&repo, candidate)?;
        }
        recording.finish();
        published += 1;
        if !in_parts {
            break;
        }
        // The next part is planned from what the Hub now holds, not from the first plan.
        let previous = cuts.first().cloned();
        remote =
            crate::hub::git::FrozenPublication::advertised_refs_for(&repo, &intent.url, &identity)
                .context("cannot establish accepted publication history")?;
        if cuts.len() > 1 {
            (cuts, _) = plan_parts(&repo, &remote, &part_branches[0])
                .map_err(|error| inconsistent(error, unsent_outcome(&target, published, total)))?;
            ensure!(
                cuts.first()
                    .zip(previous.as_ref())
                    .is_some_and(|(next, previous)| {
                        next != previous && is_raw_ancestor(&repo, previous, next).unwrap_or(false)
                    }),
                "{published} of {total} parts are published, and the Hub's copy of the branch moved since; rerun `{retry_command}` to continue"
            );
            continue;
        }
        if finishing {
            break;
        }
        // This branch is done; the next one with anything left to publish follows.
        loop {
            branch_index += 1;
            let Some(branch) = selected_branches.get(branch_index) else {
                break;
            };
            (cuts, _) = plan_parts(&repo, &remote, branch)
                .map_err(|error| inconsistent(error, unsent_outcome(&target, published, total)))?;
            if !cuts.is_empty() {
                break;
            }
            if !idle.contains(branch) {
                idle.push(branch.clone());
            }
        }
        if branch_index == selected_branches.len() {
            if idle.is_empty() {
                break;
            }
            finishing = true;
        }
    }
    if in_parts {
        ui::success(&format!(
            "Published reviewed sessions to {repository} in {published} parts."
        ));
    } else {
        ui::success(&format!("Published reviewed sessions to {repository}."));
    }
    Ok(ExitCode::Ok)
}

/// Who answers the question of whether reviewed sessions publish. `Some(true)` publishes without
/// asking and `Some(false)` refuses for want of a person; `None` asks at the terminal.
#[derive(Clone, Copy)]
struct Answer {
    /// The destination is public and the account writes it only through its organization's base
    /// permission.
    person_only: bool,
    /// The destination is public: with nobody to ask, plain sessions do not publish there.
    public: bool,
    delivery_confirmed: bool,
    automatic: bool,
    audit: bool,
    agit_yes: bool,
    encrypted: bool,
    can_ask: bool,
}

impl Answer {
    fn without_asking(self) -> Option<bool> {
        // Rule (d): nothing goes public through the base permission unless a person says so: at
        // the terminal, or with an explicit `--yes` on this one-off push. An automatic upload never
        // says so, and neither does a terminal that cannot ask.
        if self.person_only {
            if self.automatic {
                return Some(false);
            }
            if self.agit_yes {
                return Some(true);
            }
            return (!self.can_ask).then_some(false);
        }
        (self.delivery_confirmed
            || self.automatic
            || (!self.audit
                && (self.agit_yes || (!self.encrypted && !self.public && !self.can_ask))))
            .then_some(true)
    }
}

/// Names what the push did and did not publish when the accepted ledger refuses it; that
/// refusal carries no repository or branch of its own.
fn inconsistent(error: anyhow::Error, outcome: String) -> anyhow::Error {
    if error
        .downcast_ref::<crate::domain::privacy_git::InconsistentAcceptedMapping>()
        .is_some()
    {
        error.context(outcome)
    } else {
        error
    }
}

/// What a push refused before an upload leaves on the Hub: nothing, or the parts it already sent.
fn unsent_outcome(target: &str, published: usize, total: usize) -> String {
    if published == 0 {
        format!("cannot push {target}; nothing was sent to the Hub")
    } else {
        format!(
            "cannot push {target}; {published} of {total} parts are published, nothing more was sent"
        )
    }
}

#[cfg(test)]
mod answer_tests {
    use super::Answer;

    /// A public repository written only through the base permission is answered by the person:
    /// at a terminal, or by an explicit `--yes` (`AGIT_YES`) on a one-off push. An automatic upload,
    /// even with `AGIT_YES`, and a terminal that cannot ask refuse it, while any other destination
    /// keeps its unattended answers. A rule that let an automatic upload through for the member,
    /// ignored the member's `--yes`, or started asking everyone fails here.
    #[test]
    fn a_member_publishes_to_a_public_repository_only_when_the_person_answers() {
        let base = Answer {
            person_only: false,
            public: false,
            delivery_confirmed: false,
            automatic: false,
            audit: false,
            agit_yes: false,
            encrypted: false,
            can_ask: true,
        };
        let member = Answer {
            person_only: true,
            public: true,
            ..base
        };
        assert_eq!(member.without_asking(), None);
        for (answer, expected) in [
            (
                Answer {
                    agit_yes: true,
                    can_ask: false,
                    ..member
                },
                Some(true),
            ),
            (
                Answer {
                    automatic: true,
                    agit_yes: true,
                    ..member
                },
                Some(false),
            ),
            (
                Answer {
                    can_ask: false,
                    ..member
                },
                Some(false),
            ),
        ] {
            assert_eq!(answer.without_asking(), expected);
        }
        assert_eq!(base.without_asking(), None);
        for unattended in [
            Answer {
                agit_yes: true,
                ..base
            },
            Answer {
                automatic: true,
                ..base
            },
            Answer {
                can_ask: false,
                ..base
            },
        ] {
            assert_eq!(unattended.without_asking(), Some(true));
        }
    }
}
