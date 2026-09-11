//! A local prefix report provides evidence and explicit commands, never an adoption decision.

use super::Args;
use crate::adapter::native_snapshot;
use crate::commands::fix::FixCommand;
use crate::domain::{import_lineage, link, repo, store::Store};
use crate::{ExitCode, adapter, infra::config, ui};
use anyhow::{Context, ensure};
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;

mod acceptance;
pub(super) use acceptance::{Accepted, PathIdentity, verify_git_routing};

const MAX_LINK_BYTES: u64 = 64 * 1024;

#[derive(Serialize)]
struct Native {
    runtime: &'static str,
    session_id: String,
}

#[derive(Serialize)]
struct Candidate {
    #[serde(skip)]
    observed: import_lineage::Candidate,
    commit: String,
    reachable_from: Vec<String>,
    completed_turns: usize,
    records: usize,
    evidence: &'static str,
    apply: Option<FixCommand>,
}

#[derive(Serialize)]
struct Unavailable {
    commit: Option<String>,
    reason: &'static str,
}

#[derive(Serialize)]
struct Report {
    schema: &'static str,
    schema_version: u32,
    operation: &'static str,
    native: Native,
    destination: String,
    scan_state: &'static str,
    semantic_discovery_available: bool,
    candidates: Vec<Candidate>,
    unavailable: Vec<Unavailable>,
    independent: Option<FixCommand>,
}

#[derive(Clone)]
struct Destination {
    slug: String,
    branch: String,
    directory: PathBuf,
}

impl Destination {
    fn arguments(args: &Args) -> crate::Result<(String, String, String, String)> {
        let target = crate::commands::target::parse(
            args.repo
                .as_deref()
                .context("lineage inspection requires --into <owner/repo@branch>")?,
        )?;
        ensure!(
            target.tail == crate::domain::refs::Tail::None,
            "lineage inspection requires a destination branch, not a historic selector"
        );
        ensure!(
            target.base.is_none() || args.branch.is_none(),
            "a branch in --into cannot be combined with -b"
        );
        let slug = target
            .repo
            .context("lineage inspection requires a qualified owner/repository")?;
        let (owner, name) = crate::commands::parse_slug(&slug)?;
        repo::valid_name(&owner)?;
        repo::valid_name(&name)?;
        crate::commands::canonical_owner(&owner)?;
        let branch = target
            .base
            .or_else(|| args.branch.clone())
            .context("lineage inspection requires an explicit destination branch")?;
        repo::valid_branch_name(&branch)?;
        ensure!(
            branch != "@" && branch != "main",
            "the destination must name an explicit session branch"
        );
        Ok((slug, owner, name, branch))
    }

    fn parse(args: &Args) -> crate::Result<Self> {
        let (slug, owner, name, branch) = crate::input_argument(Self::arguments(args))?;
        let directory = std::env::current_dir()?.join(config::repo_dir(&owner, &name)?);
        let probe = repo::Repo::at(std::env::current_dir()?);
        let (status, _, _) =
            probe.git_status_local(&["check-ref-format", &format!("refs/heads/{branch}")])?;
        if status != Some(0) {
            let failure = Err(anyhow::anyhow!(
                "the destination session branch is not a valid Git ref"
            ));
            return if status == Some(1) {
                crate::input_argument(failure)
            } else {
                failure
            };
        }
        Ok(Self {
            slug,
            branch,
            directory,
        })
    }

    fn qualified(&self) -> String {
        format!("{}@{}", self.slug, self.branch)
    }
}

impl Report {
    fn new(runtime: &'static str, session_id: &str, destination: &Destination) -> Self {
        let destination = destination.qualified();
        Self {
            schema: "import-lineage",
            schema_version: 1,
            operation: "preview",
            native: Native {
                runtime,
                session_id: session_id.to_owned(),
            },
            independent: action(runtime, session_id, &destination, None),
            destination,
            scan_state: "complete",
            semantic_discovery_available: false,
            candidates: Vec::new(),
            unavailable: Vec::new(),
        }
    }

    fn unavailable(&mut self, commit: Option<String>, reason: &'static str) {
        self.scan_state = "incomplete";
        self.unavailable.push(Unavailable { commit, reason });
    }

    fn include(&mut self, proposals: import_lineage::Proposals) {
        for candidate in proposals.candidates {
            let apply = action(
                self.native.runtime,
                &self.native.session_id,
                &self.destination,
                Some(&candidate.commit),
            );
            self.candidates.push(Candidate {
                observed: candidate.clone(),
                commit: candidate.commit,
                reachable_from: candidate.reachable_from,
                completed_turns: candidate.completed_turns,
                records: candidate.records,
                evidence: match candidate.evidence {
                    import_lineage::Evidence::ExactNativeRecords => "exact_native_records",
                    import_lineage::Evidence::VerifiedMaterializedSource => {
                        "verified_materialized_source"
                    }
                },
                apply,
            });
        }
        for unavailable in proposals.unavailable {
            use import_lineage::UnavailableReason as Reason;
            self.unavailable(
                unavailable.commit,
                match unavailable.reason {
                    Reason::ReferenceScan => "reference_scan",
                    Reason::CommitWalk => "commit_walk",
                    Reason::ScanBudget => "scan_budget",
                    Reason::NativeRecords => "native_records",
                    Reason::StoredEvidence => "stored_evidence",
                    Reason::SecretMapping => "secret_mapping",
                    Reason::Materialization => "materialization",
                },
            );
        }
    }
}

fn action(
    runtime: &str,
    session_id: &str,
    destination: &str,
    commit: Option<&str>,
) -> Option<FixCommand> {
    let runtime = format!("--from={runtime}");
    let destination = format!("--into={destination}");
    let choice = commit
        .map(|commit| format!("--onto={commit}"))
        .unwrap_or_else(|| "--independent".to_owned());
    FixCommand::current(
        &["import", &runtime, &destination, &choice, "--", session_id],
        true,
    )
}

struct Prepared {
    report: Report,
    snapshot: Option<acceptance::Snapshot>,
}

pub(super) enum Decision {
    Stop(ExitCode),
    SameClaim(Box<link::Link>),
    Apply(Box<Accepted>),
}

pub(super) fn preview(args: &Args, json: bool) -> crate::commands::CmdResult {
    let Some(prepared) = prepare(args)? else {
        return Ok(ExitCode::Ref);
    };
    display(&prepared.report, json)?;
    Ok(ExitCode::Ok)
}

fn selected_args(args: &Args) -> crate::Result<(Destination, &str, Box<dyn adapter::Adapter>)> {
    let destination = Destination::parse(args)?;
    let (id, runtime) = crate::input_argument((|| {
        let id = args
            .session
            .as_deref()
            .context("lineage inspection requires an explicit native session ID")?;
        ensure!(
            id != "@",
            "lineage inspection requires a native session ID, not current-session inference"
        );
        native_snapshot::validate_id(id, native_snapshot::Limits::default())?;
        let runtime = adapter::normalize(
            args.from
                .as_deref()
                .context("lineage inspection requires --from <runtime>")?,
        )?;
        Ok((id, runtime))
    })())?;
    let adapter = adapter::get(runtime)?;
    Ok((destination, id, adapter))
}

fn prepare(args: &Args) -> crate::Result<Option<Prepared>> {
    let (destination, id, adapter) = selected_args(args)?;
    let mut report = Report::new(adapter.id(), id, &destination);
    let source = match adapter.lookup_native_readonly(id, native_snapshot::Limits::default()) {
        Ok(source) => source,
        Err(
            error @ (native_snapshot::Unavailable::NotFound
            | native_snapshot::Unavailable::Ambiguous),
        ) => {
            ui::error(&error.to_string());
            return Ok(None);
        }
        Err(error) => {
            report.unavailable(None, native_reason(error));
            return Ok(Some(Prepared {
                report,
                snapshot: None,
            }));
        }
    };
    let store = Store::at(std::env::current_dir()?.join(config::store_root()?));
    let selected = match read_link(&store, source.runtime, &source.session_id) {
        Ok(selected) => selected,
        Err(_) => {
            report.unavailable(None, "session_link");
            return Ok(Some(Prepared {
                report,
                snapshot: None,
            }));
        }
    };
    if let Some(repo) = repo::Repo::open(&destination.directory)
        && let Err(error) = crate::commands::migration::check_readonly_repo_startup_local(&repo)
    {
        report.unavailable(None, repository_reason(&error, "recovery"));
        return Ok(Some(Prepared {
            report,
            snapshot: None,
        }));
    }
    let snapshot = match acceptance::Snapshot::capture(
        destination.clone(),
        store,
        source,
        selected,
        adapter.as_ref(),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report.unavailable(
                None,
                error
                    .downcast_ref::<native_snapshot::Unavailable>()
                    .copied()
                    .map(native_reason)
                    .unwrap_or_else(|| repository_reason(&error, "stored_evidence")),
            );
            return Ok(Some(Prepared {
                report,
                snapshot: None,
            }));
        }
    };
    if let Some(repo) = snapshot.repo() {
        match import_lineage::discover(
            &repo,
            &destination.slug,
            &snapshot.selected_link(),
            snapshot.native(),
            import_lineage::Limits::default(),
        ) {
            Ok(proposals) => report.include(proposals),
            Err(_) => report.unavailable(None, "stored_evidence"),
        }
    } else {
        report.unavailable(None, "local_repository");
    }
    Ok(Some(Prepared {
        report,
        snapshot: Some(snapshot),
    }))
}

pub(super) fn choose(args: &mut Args, json: bool) -> crate::Result<Decision> {
    let (destination, id, adapter) = match selected_args(args) {
        Ok(selected) => selected,
        Err(error) => {
            ui::error(&crate::commands::terminal_error_message(&error));
            return Ok(Decision::Stop(crate::commands::terminal_error_code(
                &error,
                ExitCode::Failure,
            )));
        }
    };
    let store = Store::at(std::env::current_dir()?.join(config::store_root()?));
    if let Ok(Some((_, selected))) = read_link(&store, adapter.id(), id) {
        let (owner, name) = destination
            .slug
            .split_once('/')
            .context("the selected destination has no owner")?;
        if selected.is_active()
            && selected.owner.as_deref() == Some(owner)
            && selected.agent.as_deref() == Some(name)
            && selected.branch.as_deref() == Some(destination.branch.as_str())
        {
            return Ok(Decision::SameClaim(Box::new(selected)));
        }
    }
    let Some(mut prepared) = prepare(args)? else {
        return Ok(Decision::Stop(ExitCode::Ref));
    };
    prepared.report.operation = "choice_required";
    let signals = crate::tui::Signals::from_process();
    let may_prompt = !json
        && signals.interactive
        && signals.off.is_none()
        && signals.agent_session.is_none()
        && std::env::var_os("CI").is_none();
    if !may_prompt || prepared.snapshot.is_none() {
        for action in prepared
            .report
            .candidates
            .iter()
            .filter_map(|candidate| candidate.apply.as_ref())
            .chain(prepared.report.independent.as_ref())
        {
            crate::commands::fix::register(|| Some(action.clone()));
        }
        display(&prepared.report, json)?;
        return Ok(Decision::Stop(ExitCode::Interactive));
    }
    print_report(&prepared.report);
    let mut options = vec!["Cancel without importing".to_owned()];
    options.extend(prepared.report.candidates.iter().map(|candidate| {
        format!(
            "Attach at {}: {} completed turns, {} records ({})",
            candidate.commit, candidate.completed_turns, candidate.records, candidate.evidence
        )
    }));
    options.push("Import independently without selecting an earlier session history".to_owned());
    let labels = options.iter().map(String::as_str).collect::<Vec<_>>();
    let Some(choice) = ui::prompt::select("Choose the import lineage", &labels)? else {
        return Ok(Decision::Stop(ExitCode::Ok));
    };
    if choice == 0 {
        return Ok(Decision::Stop(ExitCode::Ok));
    }
    let candidate = if choice == options.len() - 1 {
        None
    } else {
        Some(
            prepared
                .report
                .candidates
                .get(choice - 1)
                .context("the selected candidate is not in the prepared report")?
                .observed
                .clone(),
        )
    };
    let onto = candidate.as_ref().map(|candidate| candidate.commit.clone());
    match prepared
        .snapshot
        .context("the selected input has no complete observation")?
        .accept(candidate)
    {
        Ok(accepted) => {
            args.onto = onto;
            args.independent = args.onto.is_none();
            Ok(Decision::Apply(Box::new(accepted)))
        }
        Err(error) => {
            ui::error(&format!(
                "the import choice is stale: {error:#}; inspect and choose again"
            ));
            Ok(Decision::Stop(ExitCode::Policy))
        }
    }
}

fn display(report: &Report, json: bool) -> crate::Result<()> {
    if json {
        println!("{}", serde_json::to_string(report)?);
    } else {
        print_report(report);
    }
    Ok(())
}

fn native_reason(error: native_snapshot::Unavailable) -> &'static str {
    use native_snapshot::Unavailable as Reason;
    match error {
        Reason::NotFound => "native_not_found",
        Reason::Ambiguous => "native_ambiguous",
        Reason::Unsupported => "native_unsupported",
        Reason::BudgetExceeded => "native_budget",
        Reason::Read => "native_read",
        Reason::Database => "native_database",
        Reason::Incomplete => "native_incomplete",
        Reason::Changed => "native_changed",
    }
}

fn repository_reason(error: &anyhow::Error, fallback: &'static str) -> &'static str {
    if error.is::<repo::GitWorktreeFormatUnavailable>() {
        "git_worktree_format"
    } else {
        fallback
    }
}

fn read_link(
    store: &Store,
    runtime: &str,
    id: &str,
) -> crate::Result<Option<(Vec<u8>, link::Link)>> {
    let path = link::link_path(store, runtime, id);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "the session link is not an ordinary file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(&path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !metadata.is_symlink(),
        "the session link is not an ordinary file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0,
            "the session link is a reparse point"
        );
    }
    ensure!(
        metadata.len() <= MAX_LINK_BYTES,
        "the session link exceeds its inspection budget"
    );
    let mut bytes = Vec::new();
    file.take(MAX_LINK_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_LINK_BYTES,
        "the session link exceeds its inspection budget"
    );
    let selected = link::Link::from_json(runtime, id, &bytes)?;
    Ok(Some((bytes, selected)))
}

fn print_report(report: &Report) {
    eprintln!(
        "Local lineage inspection: {} ({})",
        report.destination, report.scan_state
    );
    for candidate in &report.candidates {
        eprintln!(
            "  {}: {} completed turns, {} records ({})",
            candidate.commit, candidate.completed_turns, candidate.records, candidate.evidence
        );
        if let Some(action) = &candidate.apply {
            print_action(action);
        }
    }
    if report.candidates.is_empty() {
        eprintln!("No verified local prefix found; semantic comparison is unavailable.");
    }
    for unavailable in &report.unavailable {
        eprintln!(
            "  unavailable: {}{}",
            unavailable.reason,
            unavailable
                .commit
                .as_deref()
                .map(|commit| format!(" at {commit}"))
                .unwrap_or_default()
        );
        if unavailable.reason == "git_worktree_format" {
            eprintln!("  {}", repo::GitWorktreeFormatUnavailable);
        }
    }
    if let Some(action) = &report.independent {
        eprintln!("Explicit independent import:");
        print_action(action);
    }
}

fn print_action(action: &FixCommand) {
    if let Some(command) = action.human_command() {
        #[cfg(windows)]
        eprintln!("  PowerShell 7.3 or newer:");
        eprintln!("  {command}");
    } else {
        eprintln!("  Use --json for the exact command arguments and routing.");
    }
}
