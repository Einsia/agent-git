//! `agit scan` — the review before anything goes out.
//!
//! * `--secrets` (default): an advisory scan of the publish surface; `push` neither scans nor
//!   blocks. It lists the high-confidence findings (provider signatures, private keys, credential
//!   assignments, registered values), each with file / line / redacted snippet and the record id
//!   `agit secrets allow` accepts, and states how many there are. Entropy-only values are not
//!   reported by default: runtime ids, hashes and encrypted carriers are opaque too, and listing
//!   them buries the credentials. `--low-confidence` lists those in what push publishes, grouped
//!   by shape ([`secrets::published_low_confidence`]).
//!
//!   The scan surface is [`secrets::scan_agent_repo_allowing`], run **once**. With no ref,
//!   `<repo>` alone, or `--all`, it is what any push of the repository may send: the workspace
//!   plus every branch's reachable blobs, commits and tags the Hub doesn't hold. A surface that
//!   skips the workspace misses the most common shape of all, a secret already pushed and still
//!   sitting in a workspace file. With branches named, it is what `agit push` of those branches
//!   would send, from the rule the push sends by
//!   ([`crate::domain::repo::publication::sent_branches`]): a session branch's push carries
//!   `main` when it fast-forwards the Hub's, so a scan of it covers `main` too. The workspace is
//!   scanned there only when one of those branches is checked out in it. The first line of the
//!   report says which of the two was scanned. Values the user allowed in the privacy dictionary
//!   are skipped, so an allow stops a finding the way the scan's own hint says it does.
//!
//!   Every ref is resolved first, so a mistyped ref errors on the spot rather than quietly
//!   scanning something else.
//!
//! * `--sensitive`: **brings up a local review agent** to vet the transcript (content off the
//!   session's topic, out-of-scope sensitive information, directory structure leaked through
//!   absolute paths). Every entry in the report carries an `@#n.k` locator and a directly
//!   runnable remedy guarded by the reviewed head. AgentGit reports without changing LOG or VIEW;
//!   the trusted native runtime retains its authentication housekeeping and administrator policies.
//!
//!   With no model available it reports the unmet precondition explicitly (exit 4) and points
//!   at `--secrets`, which still works — it never pretends a machine reviewed anything.

use super::CmdResult;
use super::echo::{self, Selection, Source};
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::secrets;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::collections::HashSet;

mod json;
mod review;
mod runtime;
pub(in crate::commands) use runtime::foreground::{ForegroundExit, ForegroundReview};
mod scope;

pub(super) use scope::freeze_review_log;

#[derive(ClapArgs)]
pub struct Args {
    /// Refs (repeatable). Default: the context branch.
    #[arg(value_name = "owner/repo@ref")]
    pub refs: Vec<String>,
    /// Structural secret scan (default).
    #[arg(long)]
    pub secrets: bool,
    /// Bring up a local review agent to vet sensitive content.
    #[arg(long)]
    pub sensitive: bool,
    /// Also list entropy-only values in what push publishes, grouped by shape.
    #[arg(long)]
    pub low_confidence: bool,
    /// Scan the whole repository even when a branch is named.
    #[arg(long)]
    pub all: bool,
    /// Machine-readable report.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: Args) -> CmdResult {
    if args.sensitive && args.secrets {
        ui::error("`--secrets` and `--sensitive` are two different checks; run them separately.");
        return Ok(ExitCode::Usage);
    }
    if args.sensitive && args.all {
        ui::error("`--all` widens a secret scan; it does not apply to `--sensitive`.");
        return Ok(ExitCode::Usage);
    }
    if args.sensitive && args.low_confidence {
        ui::error(
            "`--low-confidence` lists secret scan findings; it does not apply to `--sensitive`.",
        );
        return Ok(ExitCode::Usage);
    }
    let cwd = std::env::current_dir()?;
    let mut parsed_refs: Vec<refs::RefSpec> = match args
        .refs
        .iter()
        .map(
            |raw| match crate::commands::target::parse_spec_preferring_local(&cwd, raw) {
                Ok(spec) => Ok(spec),
                Err(e) => {
                    ui::error(&super::terminal_error_message(&e));
                    Err(ExitCode::Usage)
                }
            },
        )
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(specs) => specs,
        Err(code) => return Ok(code),
    };
    for spec in &mut parsed_refs {
        *spec = super::target::resolve_local_repo(spec.clone())?;
    }
    let sources: Vec<Source> = parsed_refs
        .iter()
        .map(|spec| match &spec.repo {
            refs::RepoSel::Slug(_, _) | refs::RepoSel::Local(_) => Source::Explicit,
            refs::RepoSel::Context => Source::for_spec(spec),
        })
        .collect();
    // `@` becomes a branch name before anyone reads these specs — including the repo derivation
    // below. The resolver reads no environment, so a `Base::At` handed down to it is refused.
    let parsed_refs: Vec<refs::RefSpec> = match parsed_refs
        .into_iter()
        .map(super::context::substitute_at)
        .collect::<anyhow::Result<Vec<_>>>()
    {
        Ok(specs) => specs,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    // An explicit ref only needs the repo resolved; only the zero-argument form has to resolve
    // the "current branch".
    let (slug, default_branch, source) = if args.refs.is_empty() {
        match super::context::resolve(&cwd) {
            Ok(c) => (
                super::context::qualify(&c.repo),
                Some(c.branch),
                Source::Environment,
            ),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        }
    } else {
        // An explicit `<owner/repo>@ref` also identifies the repository.  If
        // only a local ref was written, retain the legacy cwd-based lookup.
        let explicit_repos: Vec<(String, Source)> = parsed_refs
            .iter()
            .zip(&sources)
            .filter_map(|(spec, source)| match &spec.repo {
                refs::RepoSel::Slug(owner, name) => Some((format!("{owner}/{name}"), *source)),
                refs::RepoSel::Local(name) => Some((name.clone(), *source)),
                refs::RepoSel::Context => None,
            })
            .collect();
        if explicit_repos.windows(2).any(|w| w[0].0 != w[1].0) {
            ui::error("all scan targets must belong to the same agent repo.");
            return Ok(ExitCode::Usage);
        }
        match explicit_repos.first() {
            Some((r, source)) if r.contains('/') => (r.clone(), None, *source),
            Some((r, source)) => (super::context::qualify(r), None, *source),
            None => match super::context::repo_for(&cwd) {
                Ok(r) => (
                    super::context::qualify(&r),
                    None,
                    if sources.iter().all(|source| *source == Source::Environment) {
                        Source::Environment
                    } else {
                        Source::Mixed
                    },
                ),
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Ref);
                }
            },
        }
    };
    let (o, n) = super::parse_slug(&slug)?;
    let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
        ui::error(&format!("{slug} does not exist locally."));
        return Ok(ExitCode::Precondition);
    };

    let targets: Vec<refs::RefSpec> = match default_branch {
        // The context branch is a branch name, never repository syntax — a
        // slash in it (`topic/foo`) must stay inside the name.
        Some(b) => vec![
            crate::commands::target::parse_local(&b).expect("context branch names are valid refs"),
        ],
        None => parsed_refs,
    };
    if args.sensitive {
        let repo = repo.local_objects_only();
        if let Err(error) = super::migration::check_readonly_repo_startup_local(&repo) {
            ui::error(&format!(
                "sensitive review requires settled local storage: {error:#}"
            ));
            if let Some(step) = super::migration::interrupted_step(&error) {
                ui::hint(step);
            }
            return Ok(ExitCode::Precondition);
        }
        return run_sensitive(&repo, &slug, &targets, args.json);
    }

    // --secrets, scanned **once**. Every ref is resolved first: a mistyped ref says so before a
    // scan is spent on it, since swallowing a ref that does not exist and then reporting clean is
    // far worse than erroring.
    for spec in &targets {
        if let Err(e) = refs::resolve(&repo, spec) {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    }
    // Branches named on the command line ask what their push sends; no ref (the zero-argument
    // form included), `<repo>` alone, a ref that names no branch, or `--all` asks about the whole
    // repository.
    let named: Option<Vec<String>> = if args.all || args.refs.is_empty() {
        None
    } else {
        targets
            .iter()
            .map(|spec| match (&spec.base, &spec.tail) {
                (refs::Base::Name(name) | refs::Base::SessionBranch(name), refs::Tail::None)
                    if repo.has_ref(&format!("refs/heads/{name}")) =>
                {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect()
    };
    let allowed = privacy_records(&repo, "allowed_values", None)
        .and_then(|values| Ok(serde_json::from_value::<HashSet<String>>(values)?));
    let privacy_store = allowed.is_ok();
    let allowed = allowed.unwrap_or_default();
    let found = collect(&repo, &n, &allowed, named.as_deref())?;
    let low = args
        .low_confidence
        .then(|| secrets::published_low_confidence(&repo, &allowed, &found.plan))
        .transpose()?;
    super::report_binary_carriers(found.binary_carriers);
    echo::emit("scan", &[Selection::new(slug.clone(), source).role("repo")]);
    if !args.json {
        println!("{}", scope_line(&slug, named.as_deref(), &found.scope));
    }
    let records = if privacy_store && !found.hits.is_empty() {
        record_ids(&repo, &found.hits)
    } else {
        None
    };
    let listed: Vec<(&(String, secrets::Hit), Option<&str>)> = found
        .hits
        .iter()
        .enumerate()
        .map(|(index, hit)| {
            let record = records
                .as_ref()
                .and_then(|ids| ids.get(index))
                .and_then(Option::as_deref);
            (hit, record)
        })
        .collect();
    let at_least = if found.counted_all { "" } else { "at least " };
    if args.json {
        let findings: Vec<serde_json::Value> = listed
            .iter()
            .map(|((at, h), record)| finding_json(at, h, "high", *record))
            .collect();
        let mut report = serde_json::json!({
            "check": "secrets",
            "advisory": true,
            "scope": match &found.scope.branches {
                Some(_) => "branches",
                None => "repository",
            },
            "branches": found.scope.branches,
            "hub_asked": found.scope.hub_asked,
            "working_tree": found.scope.worktree,
            "count": found.count,
            "count_exact": found.counted_all,
            "findings": findings,
        });
        // Absent unless asked for: low-confidence values are not counted without the flag.
        if let Some(low) = &low {
            report["low_confidence"] = low_confidence_json(low);
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for ((at, h), record) in &listed {
            // Print only the redacted snippet: this output goes into CI logs.
            let suffix = record
                .map(|id| format!("  record {id}"))
                .unwrap_or_default();
            println!(
                "  {}  {at}:{}  {}{suffix}",
                h.rule,
                h.line,
                ui::dim(&h.redacted)
            );
        }
        if let Some(low) = &low {
            print_low_confidence(low);
        }
    }

    if !found.hits.is_empty() {
        if !found.unscanned.is_empty() {
            super::report_unscanned(&found.unscanned);
        }
        ui::error(&format!(
            "{at_least}{} high-confidence secret findings. Review them before sharing this repo:",
            found.count
        ));
        // This one comes first: it decides how many times the actions below have to be
        // repeated. Unsaid, a capped list looks exactly like "this is all of it".
        if found.truncated {
            ui::hint(&format!(
                "· this list shows the first {} — handle these, then run `agit scan` again to see the rest",
                found.hits.len()
            ));
        }
        ui::hint(
            "· this scan is advisory: `agit push` doesn't run it. A push replaces what today's rules recognize in the session records and commit messages it adds, and refuses to send a text file it adds that holds one; binary files, tag messages and low-confidence values go out as they are",
        );
        match &records {
            Some(_) => ui::hint(&format!(
                "· only if you know the value is yours and it is not a credential: `agit secrets allow <record> --repo {}`, then scan again",
                repo.root().display()
            )),
            None => ui::hint(
                "· record ids are unavailable because the local privacy store did not answer; `agit secrets review --repo <path>` lists the records it holds",
            ),
        }
        super::hint_secret_hit_remedies(found.hits.iter().map(|(_, hit)| hit));
        // Findings exit 7, not the generic failure 1: CI has to tell "a secret was found" from
        // "the command broke".
        Ok(ExitCode::Policy)
    } else if !found.counted_all {
        // This must **not** say clean. What was never scanned and what is clean look identical in
        // an empty hit list, so this exits non-zero too.
        super::report_unscanned(&found.unscanned);
        if found.unscanned.is_empty() {
            ui::error(&format!(
                "the scan stopped after counting {} low-confidence values; content behind them was not scanned, so no finding does not mean no credential.",
                found.low_confidence_count
            ));
        }
        Ok(ExitCode::Policy)
    } else {
        if !args.json {
            // Never "N refs": what was scanned is the surface the scope line names, not a count
            // of the refs on the command line.
            ui::success_result(if found.binary_carriers == 0 {
                "no high-confidence secret findings"
            } else {
                "no high-confidence text findings; binary artifacts were not text-scanned"
            });
        }
        Ok(ExitCode::Ok)
    }
}

/// One finding as `--json` reports it.
fn finding_json(
    at: &str,
    hit: &secrets::Hit,
    confidence: &str,
    record: Option<&str>,
) -> serde_json::Value {
    // `source` is the **structured** carrier, so a consumer never parses the prefix of `at`: a
    // user can create a directory genuinely called `commit object ab12cd34/...`.
    serde_json::json!({
        "rule": hit.rule,
        "at": at,
        "line": hit.line,
        "preview": hit.redacted,
        "source": hit.source.as_str(),
        "confidence": confidence,
        "record_id": record,
    })
}

/// The values of each shape that were found, the first ones listed.
fn low_confidence_groups(
    low: &secrets::PublishedLowConfidence,
) -> impl Iterator<Item = (secrets::Shape, usize, Vec<&secrets::Hit>)> {
    secrets::Shape::ALL.into_iter().filter_map(|shape| {
        let count = low.count_of(shape);
        (count > 0).then(|| {
            let listed = low
                .listed
                .iter()
                .filter(|hit| hit.shape() == shape)
                .collect();
            (shape, count, listed)
        })
    })
}

fn low_confidence_json(low: &secrets::PublishedLowConfidence) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = low_confidence_groups(low)
        .map(|(shape, count, listed)| {
            let findings: Vec<serde_json::Value> = listed
                .into_iter()
                .map(|hit| finding_json(hit.file.as_deref().unwrap_or("?"), hit, "low", None))
                .collect();
            serde_json::json!({ "shape": shape.as_str(), "count": count, "findings": findings })
        })
        .collect();
    serde_json::json!({
        "count": low.count(),
        "count_exact": low.counted_all,
        "files_scope": "branch_tips",
        "groups": groups,
    })
}

fn print_low_confidence(low: &secrets::PublishedLowConfidence) {
    let at_least = if low.counted_all { "" } else { "at least " };
    let values = |count: usize| if count == 1 { "value" } else { "values" };
    // A push also carries earlier versions of each file; this listing reads them as each branch
    // holds them now, and says so under its header.
    let files_scope = "· files are read as each branch holds them now, not their earlier versions";
    if low.count() == 0 {
        println!("no low-confidence values in what push publishes");
        println!("{files_scope}");
    } else {
        println!(
            "{at_least}{} low-confidence {} in what push publishes, by shape (entropy alone also matches runtime ids, hashes and encrypted carriers):",
            low.count(),
            values(low.count())
        );
        println!("{files_scope}");
        for (shape, count, listed) in low_confidence_groups(low) {
            println!("  {}: {at_least}{count} {}", shape.label(), values(count));
            for hit in &listed {
                println!(
                    "    {}:{}  {}",
                    hit.file.as_deref().unwrap_or("?"),
                    hit.line,
                    ui::dim(&hit.redacted)
                );
            }
            if count > listed.len() {
                println!("    · {} more not listed", count - listed.len());
            }
        }
    }
    let unread = &low.unscanned;
    let mut parts: Vec<&str> = unread
        .oversized
        .iter()
        .chain(&unread.oversized_files)
        .map(|(label, _)| label.as_str())
        .chain(unread.unsupported.iter().map(String::as_str))
        .take(5)
        .collect();
    if unread.over_budget.is_some() {
        parts.push("the rest, past the local scan budget");
    }
    if !parts.is_empty() {
        ui::hint(&format!(
            "· not read, so their low-confidence values are not counted: {}",
            parts.join(", ")
        ));
    }
}

/// One explicit privacy management request against this repo's records.
fn privacy_records(
    repo: &Repo,
    action: &str,
    secret: Option<String>,
) -> crate::Result<serde_json::Value> {
    crate::domain::privacy::service::manage(
        Some(repo.root()),
        crate::domain::privacy::management::Command {
            action: action.into(),
            global: false,
            id: None,
            name: None,
            secret,
        },
    )
}

/// The record id of each listed finding, in order. `None` when the privacy store does not answer:
/// the report then says ids are unavailable instead of printing a placeholder a user would try.
fn record_ids(repo: &Repo, hits: &[(String, secrets::Hit)]) -> Option<Vec<Option<String>>> {
    let values: Vec<&str> = hits.iter().map(|(_, hit)| hit.value.expose()).collect();
    // The request owns the serialized values and wipes them on drop.
    let identities =
        privacy_records(repo, "identify", Some(serde_json::to_string(&values).ok()?)).ok()?;
    let identities = identities.as_array()?;
    (identities.len() == hits.len()).then(|| {
        identities
            .iter()
            .map(|identity| identity.as_str().map(str::to_owned))
            .collect()
    })
}

fn run_sensitive(repo: &Repo, slug: &str, targets: &[refs::RefSpec], json: bool) -> CmdResult {
    match sensitive_report(repo, slug, targets) {
        Ok(report) => {
            report.print(json)?;
            Ok(report.exit_code())
        }
        Err(failure) => {
            failure.print(json);
            Ok(failure.code)
        }
    }
}

/// A completed report describes immutable evidence; the caller decides whether to publish it.
pub(super) struct SensitiveReport {
    scopes: Vec<serde_json::Value>,
    findings: Vec<review::Finding>,
    count: usize,
    complete: bool,
    historical: bool,
}

impl SensitiveReport {
    pub(super) fn complete(&self) -> bool {
        self.complete
    }

    pub(super) fn value(&self) -> serde_json::Value {
        serde_json::json!({
            "check": "sensitive", "status": if self.complete() { "reviewed" } else { "incomplete" },
            "complete": self.complete(), "scopes": self.scopes, "findings": self.findings,
            "evidence_log_unchanged": true,
            "runtime": if self.count > 0 { Some("claude-code") } else { None },
            "runtime_boundary": "model tools and user/project customizations are disabled; native authentication housekeeping and administrator policies remain active",
        })
    }

    pub(super) fn print(&self, json: bool) -> crate::Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(&self.value())?);
        } else {
            for finding in &self.findings {
                println!(
                    "  {} {}: {}",
                    &finding.snapshot[..12],
                    finding.locator,
                    finding.explanation
                );
                for remedy in &finding.remedies {
                    println!("    {}", remedy.command);
                }
            }
            if self.count == 0 {
                println!("no located events to review in the selected VIEW");
            } else if self.findings.is_empty() && self.complete() {
                println!(
                    "no sensitive findings reported in {} selected VIEW events",
                    self.count
                );
            }
        }
        if !self.complete() {
            ui::error(
                "sensitive review is incomplete: some selected VIEW events have no safe turn/event location.",
            );
        }
        if !self.findings.is_empty() {
            ui::hint(
                "review each suspicion before removing it from the VIEW; revert preserves the evidence LOG",
            );
            if self.historical {
                ui::hint(
                    "historical snapshots have inspection remedies only; fork explicitly before changing their VIEW",
                );
            }
        }
        Ok(())
    }

    fn exit_code(&self) -> ExitCode {
        if !self.complete() {
            ExitCode::Precondition
        } else if self.findings.is_empty() {
            ExitCode::Ok
        } else {
            ExitCode::Policy
        }
    }
}

pub(super) struct SensitiveFailure {
    pub(super) code: ExitCode,
    reason: String,
    message: String,
    hint: Option<&'static str>,
    scopes: Vec<serde_json::Value>,
}

impl SensitiveFailure {
    pub(super) fn print(&self, json: bool) {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "check": "sensitive", "status": "incomplete", "complete": false,
                    "reason": self.reason, "findings": [], "scopes": self.scopes,
                })
            );
        }
        ui::error(&self.message);
        if let Some(hint) = self.hint {
            ui::hint(hint);
        }
    }
}

pub(super) fn sensitive_report(
    repo: &Repo,
    slug: &str,
    targets: &[refs::RefSpec],
) -> std::result::Result<SensitiveReport, SensitiveFailure> {
    let selected = scope::collect(repo, targets).map_err(|error| {
        if let Some(ambiguity) = error.downcast_ref::<refs::Ambiguous>() {
            SensitiveFailure {
                code: super::terminal_error_code(&error, ExitCode::Precondition),
                reason: "selected reference requires an explicit choice".into(),
                message: ambiguity.to_string(),
                hint: None,
                scopes: Vec::new(),
            }
        } else {
            SensitiveFailure {
                code: if refs::is_not_found(&error) {
                    ExitCode::Ref
                } else {
                    ExitCode::Precondition
                },
                reason: "selected VIEW/LOG could not be read within the review limits".into(),
                message: "sensitive review is incomplete: the selected committed VIEW/LOG is unavailable, invalid, or exceeds the review limits.".into(),
                hint: Some("select a valid session ref or a smaller turn range; `agit scan --secrets` remains a separate check"),
                scopes: Vec::new(),
            }
        }
    })?;
    let count: usize = selected.iter().map(|scope| scope.events.len()).sum();
    let complete = selected
        .iter()
        .all(|scope| scope.unlocated_events == 0 && scope.missing_events == 0);
    let mut scopes: Vec<serde_json::Value> = selected.iter().map(|scope| serde_json::json!({
        "snapshot": scope.sha, "branch": scope.branch, "events_selected": scope.events.len(),
        "events_reviewed": 0,
        "unlocated_events": scope.unlocated_events, "missing_events": scope.missing_events,
    })).collect();
    let reviewed = if count == 0 {
        Ok(Vec::new())
    } else {
        ui::hint(
            "the configured local runtime/provider performs this review; native authentication housekeeping and administrator policies remain active",
        );
        review::prompt(&selected)
            .and_then(|prompt| runtime::review(&prompt, &review::schema()))
            .and_then(|response| review::validate(&response, &selected, slug))
    };
    let mut findings = reviewed.map_err(|error| SensitiveFailure {
        code: ExitCode::Precondition,
        reason: error.to_string(),
        message: format!("sensitive review is incomplete: {error}"),
        hint: Some(
            "check the configured local runtime; `agit scan --secrets` remains a separate deterministic check",
        ),
        scopes: scopes.clone(),
    })?;
    for (report, scope) in scopes.iter_mut().zip(&selected) {
        report["events_reviewed"] = serde_json::json!(scope.events.len());
    }
    for finding in &mut findings {
        if selected[finding.scope]
            .branch
            .as_deref()
            .is_some_and(|branch| super::branch::is_sealed(repo, branch))
        {
            finding.remedies.retain(|remedy| remedy.action == "inspect");
        }
    }
    Ok(SensitiveReport {
        scopes,
        findings,
        count,
        complete,
        historical: selected.iter().any(|scope| scope.branch.is_none()),
    })
}

/// Every hit from one `--secrets` scan, each carrying **the location it really belongs to**.
/// What a secret scan answered for.
struct Scope {
    /// The branches of the one push it checked; `None` for the whole repository.
    branches: Option<Vec<String>>,
    /// Whether the Hub said what it holds, so history it holds was left out.
    hub_asked: bool,
    /// Whether the checkout's working tree was scanned.
    worktree: bool,
}

/// The first line of a secret scan's report: which question it answered, so a clean scan of one
/// push never reads as a clean repository.
fn scope_line(slug: &str, named: Option<&[String]>, scope: &Scope) -> String {
    let history = if scope.hub_asked {
        "their commits the Hub doesn't hold yet"
    } else {
        "their whole history, since the Hub couldn't be asked"
    };
    match (&scope.branches, named) {
        (Some(sent), Some(named)) => {
            let push = match named {
                [branch] => format!("`agit push {slug}@{branch}`"),
                _ => format!("pushing {} of {slug}", listed(named)),
            };
            let tree = if scope.worktree {
                ", and the working tree"
            } else {
                ""
            };
            // The branches named come first, in the order written, and then what the push adds.
            let mut sending: Vec<String> = named
                .iter()
                .filter(|branch| sent.contains(branch))
                .cloned()
                .collect();
            sending.extend(
                sent.iter()
                    .filter(|branch| !named.contains(branch))
                    .cloned(),
            );
            format!(
                "Scanned what {push} would send: {}, {history}{tree}.",
                listed(&sending)
            )
        }
        _ => {
            let history = if scope.hub_asked {
                "every branch's commits the Hub doesn't hold yet"
            } else {
                "every branch's whole history, since the Hub couldn't be asked"
            };
            format!("Scanned all of {slug}: {history}, and the working tree.")
        }
    }
}

/// `a`, `a and b`, `a, b and c`.
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

struct Found {
    scope: Scope,
    /// The surface scanned, which `--low-confidence` lists within too.
    plan: secrets::ScanPlan,
    binary_carriers: u64,
    /// `(display location, hit)` of the first high-confidence hits. The location is the hit's
    /// own carrier label, never prefixed with a ref name.
    hits: Vec<(String, secrets::Hit)>,
    /// High-confidence hits exist that `hits` does not list — say so.
    truncated: bool,
    count: usize,
    /// Entropy-only hits the scan counted; only the warning that counting them stopped the scan
    /// names this number.
    low_confidence_count: usize,
    /// Both counts are exact; otherwise they are lower bounds.
    counted_all: bool,
    /// Some part was **never scanned**. An empty hit list plus this does not mean clean.
    unscanned: secrets::Unscanned,
}

/// Run the **one** repo-wide pass and produce a report.
///
/// # Why there is no per-target pass on top
///
/// The scan surface ([`secrets::scan_agent_repo`]) is the **whole repo**, independent of which
/// ref was written on the command line — the publish surface is not a function of any one ref.
/// A per-ref pass is wrong in both directions:
///
/// * **Misses**: it looks only at that ref's tree, while `agit push -b other` has the publish
///   plan bring `main` along; with the secret in a current file on `main`, `agit scan other`
///   reports clean while push blocks.
/// * **Rescans, re-reports**: `agit scan main other` scans the same objects twice, the same hit
///   appears twice in the report and the user believes there are two places to fix; each is also
///   prefixed with the target of that pass, so a hit that exists only on `main` shows up as
///   `other:...` — pointing at a ref that does not have it, and chasing that is a dead end.
///
/// So a hit's location is its own carrier label (`blob object <sha8>/<path>`,
/// `commit object <sha8>`, `tag object <sha8>`), never prefixed with a ref name. The caller
/// still **parses and validates** every ref (a mistyped ref must error on the spot), but those
/// shas never reach here — off the signature, they give no one the chance to narrow the scan
/// surface with them.
/// The secret scan of `repo`: what the push of the `named` branches sends, or, with none, what
/// any push of the repository may send.
fn collect(
    repo: &Repo,
    agent: &str,
    allowed: &HashSet<String>,
    named: Option<&[String]>,
) -> crate::Result<Found> {
    // `agent` is the third element of the destination identity: the name this repo resolves to,
    // which is the one `agit push` publishes to from here. Without it, when `origin` points at
    // another agent under the same account, that agent's history is subtracted wholesale from
    // the scan surface.
    let destination = super::publish_destination(repo, agent, false);
    let hub_asked = destination.scan.narrows();
    // The branches come from the rule the push itself sends by, main included as it decides, so
    // the scan can't check less than the push sends.
    let plan = match named {
        None => secrets::ScanPlan::to(destination.scan),
        Some(named) => secrets::ScanPlan::of_branches(
            destination.scan,
            crate::domain::repo::publication::sent_branches(repo, named, &destination.heads)?,
        ),
    };
    let scope = Scope {
        worktree: plan.scans_worktree(repo),
        branches: plan.branches.clone(),
        hub_asked,
    };
    let wide = secrets::scan_agent_repo_allowing(repo, &plan, allowed)?;
    let labelled = |hits: Vec<secrets::Hit>| {
        hits.into_iter()
            .map(|h| (h.file.clone().unwrap_or_else(|| "?".into()), h))
            .collect()
    };
    Ok(Found {
        scope,
        plan,
        binary_carriers: wide.binary_carriers,
        hits: labelled(wide.hits),
        truncated: wide.truncated,
        count: wide.count,
        low_confidence_count: wide.low_confidence_count,
        counted_all: wide.counted_all,
        unscanned: wide.unscanned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWS: &str = "AKIA4X7QZ2M5RT6VW3JH";

    /// Build a repo with two branches: `main` carries a commit message with a secret in it,
    /// `other` splits off before that commit, and both branches' trees are clean.
    fn two_branch_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "other"]);
        // The secret is only on main, and only in the message — the tree is empty, so a per-ref
        // path never produces it.
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("leak {AWS}"),
        ]);

        d
    }

    /// A scan of named branches answers for their push. `main` named alone sends main only, so
    /// a secret on another session branch isn't its finding. A session branch's push carries
    /// `main` too, so its scan covers main's history as well as its own. No branch, or `--all`,
    /// is the whole repository. Each says which it scanned.
    #[test]
    fn a_named_branch_is_scanned_as_its_push_sends_it() {
        const TALK: &str = "AKIAQ3W7ZRT2M5X6VJ4H";
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "talk"]);
        run(&["branch", "other"]);
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("leak {AWS}"),
        ]);
        let on_main = run(&["rev-parse", "HEAD"]);
        run(&["checkout", "-q", "talk"]);
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("leak {TALK}"),
        ]);
        let on_talk = run(&["rev-parse", "HEAD"]);
        run(&["checkout", "-q", "main"]);
        let repo = Repo::at(d.path());
        let scan = |named: Option<&[&str]>| {
            let named: Option<Vec<String>> =
                named.map(|named| named.iter().map(|branch| branch.to_string()).collect());
            let found = collect(&repo, "photo", &HashSet::new(), named.as_deref())
                .expect("the repo is well-formed");
            let line = scope_line("me/photo", named.as_deref(), &found.scope);
            let mut commits: Vec<&str> = [&on_main, &on_talk]
                .into_iter()
                .filter(|commit| found.hits.iter().any(|(at, _)| at.contains(&commit[..8])))
                .map(String::as_str)
                .collect();
            commits.sort_unstable();
            (
                commits.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                line,
            )
        };
        let sorted = |commits: &[&String]| {
            let mut commits: Vec<String> = commits.iter().map(|c| c.to_string()).collect();
            commits.sort_unstable();
            commits
        };

        let (found, line) = scan(Some(&["main"]));
        assert_eq!(found, [on_main.clone()]);
        assert_eq!(
            line,
            "Scanned what `agit push me/photo@main` would send: main, their whole history, since the Hub couldn't be asked, and the working tree."
        );
        let (found, line) = scan(Some(&["other"]));
        assert_eq!(found, [on_main.clone()], "{line}");
        assert!(line.contains(": other and main, "), "{line}");
        let (found, _) = scan(Some(&["talk"]));
        assert_eq!(found, sorted(&[&on_main, &on_talk]));
        let (found, line) = scan(Some(&["talk", "other"]));
        assert_eq!(found, sorted(&[&on_main, &on_talk]));
        assert!(
            line.starts_with("Scanned what pushing talk and other of me/photo would send: "),
            "{line}"
        );
        let (found, line) = scan(None);
        assert_eq!(found, sorted(&[&on_main, &on_talk]));
        assert_eq!(
            line,
            "Scanned all of me/photo: every branch's whole history, since the Hub couldn't be asked, and the working tree."
        );
        // A push of a session branch from a checkout of another one sends no working tree.
        run(&["checkout", "-q", "other"]);
        let (_, line) = scan(Some(&["talk"]));
        assert!(!line.contains("working tree"), "{line}");
    }

    /// `--low-confidence` lists within the same surface as the scan: a value only in another
    /// branch's commit message, or in the working tree of a checkout of another branch, is no
    /// value the push of `main` sends, while the whole repository's list still names both.
    #[test]
    fn low_confidence_values_are_listed_within_the_named_push() {
        const IN_MESSAGE: &str = "6f1c2d3e-4a5b-4c6d-8e7f-9a0b1c2d3e4f";
        const IN_TREE: &str = "0a9b8c7d-6e5f-4a3b-9c2d-1e0f9a8b7c6d";
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["checkout", "-q", "-b", "other"]);
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("run {IN_MESSAGE}"),
        ]);
        std::fs::write(d.path().join("notes.txt"), format!("id {IN_TREE}\n")).unwrap();
        let repo = Repo::at(d.path());
        let listed = |named: Option<&[String]>| {
            let found = collect(&repo, "photo", &HashSet::new(), named).unwrap();
            let low =
                secrets::published_low_confidence(&repo, &HashSet::new(), &found.plan).unwrap();
            let shown: Vec<String> = low
                .listed
                .iter()
                .map(|hit| format!("{} {}", hit.file.clone().unwrap_or_default(), hit.redacted))
                .collect();
            (low.count(), shown)
        };
        let (whole, shown) = listed(None);
        assert!(whole >= 2, "{shown:?}");
        let (main, shown) = listed(Some(&["main".to_owned()]));
        assert_eq!(main, 0, "{shown:?}");
    }

    /// This pins that one hit is reported once, and that the size of the report is not a
    /// function of how many refs the user wrote.
    ///
    /// The scan surface is the whole repo's publish surface. A per-ref scan has
    /// `agit scan main other` scan the same objects twice and report them twice — the user
    /// believes there are two places to fix, and the count does not drop after fixing one.
    #[test]
    fn a_repo_wide_hit_is_reported_exactly_once() {
        let d = two_branch_repo();
        let repo = Repo::at(d.path());

        let found =
            collect(&repo, "photo", &HashSet::new(), None).expect("the repo is well-formed");
        let leaks: Vec<&(String, secrets::Hit)> = found
            .hits
            .iter()
            .filter(|(_, h)| h.rule == "aws-access-token")
            .collect();
        assert_eq!(
            leaks.len(),
            1,
            "the same commit hit must be reported exactly once: {leaks:?}"
        );
    }

    /// This pins that a hit is never prefixed with the name of a ref.
    ///
    /// The secret is only on `main`, and `other` splits off before it. Showing it as
    /// `other:commit object ...` points the user at a ref that does not have that commit, and
    /// chasing that is a dead end.
    #[test]
    fn a_repo_wide_hit_is_not_attributed_to_a_ref_that_lacks_it() {
        let d = two_branch_repo();
        let repo = Repo::at(d.path());

        let found =
            collect(&repo, "photo", &HashSet::new(), None).expect("the repo is well-formed");
        let (at, _) = found
            .hits
            .iter()
            .find(|(_, h)| h.rule == "aws-access-token")
            .expect("this hit must be reported");
        assert!(
            at.starts_with("commit object "),
            "a repo-wide hit's location is its own carrier, never a ref name: {at:?}"
        );
    }

    /// This pins that `agit scan` and `agit push` return the **same** verdict on the same repo.
    ///
    /// # What this pins
    ///
    /// Two entry points behind the two gates — `agit scan` on `scan_repo_wide` (reachable
    /// objects only), `agit push` on `scan_agent_repo` (which also walks the whole workspace) —
    /// split on the **most common** shape of all: a secret already pushed and still lying in a
    /// workspace file. scan says `clean scan` (exit 0), push says
    /// `1 suspected secrets found — publish blocked`. The user cannot reproduce locally why they
    /// were refused, and two entry points keep drifting for as long as both exist.
    ///
    /// Both go through the **same function** and the same [`secrets::ScanPlan`], so what is
    /// asserted here is that the full hit lists are equal, not merely that "both are non-empty":
    /// the same verdict is the conclusion, the same scan surface is the reason.
    #[test]
    fn scan_and_push_agree_on_the_same_repo() {
        let d = tempfile::tempdir().unwrap();
        let work = d.path().join("work");
        let hub = d.path().join("hub.git");
        std::fs::create_dir_all(&work).unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&work)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // The secret sits in a workspace file and has **already been pushed** — the most common
        // shape.
        std::fs::write(work.join("config.env"), format!("AWS_KEY={AWS}\n")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "settings"]);
        std::process::Command::new("git")
            .args(["init", "-q", "--bare", &hub.to_string_lossy()])
            .output()
            .unwrap();
        run(&["remote", "add", "origin", &hub.to_string_lossy()]);
        run(&["push", "-q", "origin", "main"]);

        // Precondition: the secret really is in a workspace file, and really has been pushed.
        assert!(
            std::fs::read_to_string(work.join("config.env"))
                .unwrap()
                .contains(AWS),
            "precondition: the secret must be in the workspace file"
        );
        assert!(
            !run(&["for-each-ref", "--format=%(refname)", "refs/remotes/origin"]).is_empty(),
            "precondition: it must have been pushed, or the two paths cannot drift apart at all"
        );

        let repo = Repo::at(&work);
        // Both paths ask about the **same** destination, so the third element of the identity
        // (which agent to publish to) has to be the same as well: different names send them to
        // two different far sides, and this test then no longer pins "the same scan surface".
        const AGENT: &str = "photo";
        // The scan path.
        let scanned =
            collect(&repo, AGENT, &HashSet::new(), None).expect("the repo is well-formed");
        // The push path: the same destination, the same plan.
        let plan =
            secrets::ScanPlan::to(crate::commands::publish_destination(&repo, AGENT, true).scan);
        let pushed = secrets::scan_agent_repo(&repo, &plan).expect("the repo is well-formed");

        assert!(
            !scanned.hits.is_empty(),
            "`agit scan` must see the secret in the workspace file — reporting clean is the bug"
        );
        assert_eq!(
            scanned.hits.len(),
            pushed.hits.len(),
            "the two gates must find the same number of hits: scan={:?} push={:?}",
            scanned.hits,
            pushed.hits
        );
        let scan_hits: Vec<&secrets::Hit> = scanned.hits.iter().map(|(_, h)| h).collect();
        let push_hits: Vec<&secrets::Hit> = pushed.hits.iter().collect();
        assert_eq!(
            scan_hits, push_hits,
            "the two gates must look at the same scan surface, not merely both be non-empty"
        );
    }
}
