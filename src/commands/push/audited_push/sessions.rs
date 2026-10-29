//! Publication selects registered session authority independently of repository privacy flags.

use super::*;
use crate::domain::privacy_git::sessions::SessionPublicationPlan;
use crate::domain::repo::publication::{PublicationPlan, is_raw_ancestor};

pub(super) fn run(
    args: &Args,
    client: Client,
    repo: Repo,
    branches: &[String],
    retry_command: &str,
    destination: Destination,
) -> CmdResult {
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
        !automatic || repo.auto_push_enabled()?,
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
    // A new destination's first publication carries the whole history, which is checked here;
    // an existing one only needs the selected heads, whatever the length of their history. A
    // cross-Hub delivery checks every selected commit for plain local history, so it captures
    // the whole history too.
    let original = if matches!(intent.action, Action::Create) || intent.explicit_hub {
        PublicationPlan::freeze(&repo, branches)?
    } else {
        PublicationPlan::freeze_tips(&repo, branches)?
    };
    if intent.explicit_hub {
        for commit in original.commit_objects() {
            let metadata = crate::domain::storage::metadata_local(repo.root(), commit)?;
            ensure!(
                crate::domain::privacy_recovery::committed_envelope(&repo, commit)?.is_none()
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
        for commit in original.commit_objects() {
            crate::domain::storage::metadata_local(repo.root(), commit)?;
            ensure!(
                crate::domain::privacy_recovery::committed_envelope(&repo, commit)?.is_none(),
                "a new destination requires preserved source authority for inherited ciphertext"
            );
        }
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
    let remote =
        crate::hub::git::FrozenPublication::advertised_refs_for(&repo, &intent.url, &identity)
            .context("cannot establish accepted publication history")?;
    let implicit_main = !args.all && !branches.iter().any(|branch| branch == "main");
    let mut selected_branches = Vec::new();
    for head in original.heads() {
        let branch = head
            .name()
            .strip_prefix("refs/heads/")
            .context("invalid publication branch")?;
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
    let selected = SessionPublicationPlan::resolve(
        &repo,
        &selected_branches,
        &client,
        &repository,
        identity.clone(),
        remote,
        !args.dry_run,
        intent.explicit_hub,
    )?;
    let _ = warmed.join();
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
    let mut projected = ProjectedHistory::prepare_sessions(&repo, &selected_branches, &selected, &[])
        .map_err(|error| {
            if error.downcast_ref::<crate::domain::privacy_git::MissingAcceptedObjects>().is_some() {
                #[cfg(windows)]
                use crate::ui::quote_powershell_argument as quote_arg;
                #[cfg(not(windows))]
                use crate::ui::session::shell_arg as quote_arg;
                error.context(format!("fetch the accepted public history with `agit fetch {}` and retry `{retry_command}`; keep this checkout to preserve unpublished commits", quote_arg(&repository)))
            } else {
                error
            }
        })?;
    let binding = ui::publication::stage("Reading prepared snapshot bindings");
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
            return Ok(inspection_failure_code(blocked.reason()));
        }
    };
    inspecting.finish();
    let status = ui::publication::stage("Checking publication status");
    let synchronized = !args.dry_run
        && !args.audit
        && !args.show_preview
        && complete.captured().pointers().is_empty()
        && receipts_match_current(&repo, &receipts)?
        && (supervisor.is_some() || no_pending_publication_receipts(&repo, &receipts)?)
        // The advertisement this publication was planned from decides "already published"; a
        // second listing only adds a Hub round trip, since nothing is sent on this path.
        && FrozenPublication::refs_match(projected.plan(), &selected.remote);
    status.finish();
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
        let (repo, remote) = intent.materialize(&client, repo)?;
        if intent.explicit_hub {
            intent.bind_delivered_origin(&repo, &remote)?;
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
    previewing.finish();
    println!("Publication destination: {}", intent.describe());
    println!("Privacy preview: {}", preview.path.display());
    if !automatic {
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
            let question = if person_only {
                format!(
                    "{repository} is public and you write to it as a member of its organization. Publish the {} reviewed session(s) and file changes to everyone?",
                    receipts.len()
                )
            } else {
                format!(
                    "Publish the reviewed sessions to {repository} on {} ({})?",
                    intent.hub, intent.visibility
                )
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
            ui::info("Publication cancelled.");
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
    checking.finish();
    let report = prepared.publish_with_session_receipts(
        projected.policy_digest(),
        projected.session_manifest(),
        &intent.visibility,
        &selected.remote,
    ).with_context(|| format!("no branch or tag refs were published by this push; rerunning `{retry_command}` is safe after resolving the reported error"))?;
    let recording = ui::publication::stage("Recording publication acknowledgements");
    projected.confirm_acceptance(&report)?;
    show_publication(&report);
    if !report.ok() {
        ui::error(&publication_recovery(&report, retry_command, &repository));
        return Ok(publication_failure_code(&report));
    }
    verify_receipt_acknowledgements(&report, &receipts)?;
    if intent.explicit_hub {
        intent.bind_delivered_origin(&repo, &remote)?;
    }
    for receipt in &receipts {
        receipt.save(&repo)?;
    }
    if let (Some(reply), Some(candidate)) = (supervisor, supervised) {
        reply.complete(&repo, candidate)?;
    }
    recording.finish();
    ui::success(&format!("Published reviewed sessions to {repository}."));
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
