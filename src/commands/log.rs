//! `agit log` — per-turn history and the branch-level view.
//!
//! With no arguments it lists the context branch's history turn by turn: the `#n` ordinal, the
//! short sha, the kind badge, the message (the first line of the user prompt), the code anchor,
//! tags — **the full detail locally, without going to the web interface** (PRD).
//!
//! * `--branches`, or `owner/repo` on its own: the branch (session) level view — name, opening
//!   prompt, turn count, last activity, ahead/behind.
//! * `--graph`: a cross-branch ASCII graph (fork points and merge parents are both on it).
//! * `-- <path>`: only the commits that touched one shared file.
//!
//! History and evidence reads share batched metadata, ref facts, and commit graphs.
//! Neither view opens a live transcript.

use super::CmdResult;
use crate::domain::meta::{self, Kind};
use crate::domain::repo::Repo;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use serde::Serialize;

/// How many turns to show when `-n` is absent.
///
/// It is a named constant because **the criterion reads it**: Timeline opens only when no
/// narrowing argument is present, and since `-n` has a default, "did the user give one" can only
/// be answered by comparing against that default. Two literals moving apart makes the criterion
/// silently wrong — a test pins this one.
pub const DEFAULT_LIMIT: usize = 20;

#[derive(ClapArgs)]
pub struct Args {
    /// `<owner/repo>@<ref>` (per-turn) or `<owner/repo>` (branch-level). Default: context branch.
    #[arg(value_name = "owner/repo@ref | ref")]
    pub target: Option<String>,

    /// Show at most n turns.
    #[arg(short = 'n', long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
    /// Cross-branch ASCII graph.
    #[arg(long, conflicts_with = "branches")]
    pub graph: bool,
    /// Branch-level view.
    #[arg(long)]
    pub branches: bool,
    /// Only one kind.
    #[arg(long, value_name = "turn|merge|view|file|archive")]
    pub kind: Option<String>,
    /// Filter by message.
    #[arg(long, value_name = "pat")]
    pub grep: Option<String>,
    /// Only recent ones (24h / 7d / 4w).
    #[arg(long, value_name = "duration", value_parser = parse_since_git)]
    pub since: Option<String>,
    /// One line per turn.
    #[arg(long)]
    pub oneline: bool,
    /// Only commits touching this shared file.
    #[arg(last = true, value_name = "path")]
    pub paths: Vec<String>,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    if wants_tui(&args) {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                let Some(picked) = crate::tui::screens::history::pick(&cwd, "agit log")? else {
                    return Ok(ExitCode::Ok);
                };
                let repo = Repo::open(&picked.path)
                    .ok_or_else(|| anyhow::anyhow!("{} is no longer available", picked.slug))?;
                let head = repo.git(&["rev-parse", &format!("refs/heads/{}", picked.branch)])?;
                return crate::tui::screens::timeline::run(
                    &repo,
                    &picked.slug,
                    &picked.branch,
                    head.trim(),
                );
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }
    let parsed_target = match args.target.as_deref() {
        Some(raw) => {
            // `--branches` and `--graph` explicitly ask for the
            // repository-level view, so a slash string keeps its repository
            // reading there; the local-branch preference applies only to the
            // ordinary positional target.
            let parsed = if args.branches || args.graph {
                crate::commands::target::parse(raw)
            } else {
                crate::commands::target::parse_preferring_local(&cwd, raw)
            };
            match parsed {
                Ok(parsed) => Some(parsed),
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Usage);
                }
            }
        }
        None => None,
    };
    let source = parsed_target
        .as_ref()
        .map(|target| super::echo::Source::for_spec(&super::target::to_spec(target.clone())))
        .unwrap_or(super::echo::Source::Environment);
    // `owner/repo` keeps the historical branch-list shorthand.  Once `@ref`
    // is present it is a normal explicit target and is rendered turn by turn.
    if let Some(parsed) = &parsed_target
        && parsed.repo.is_some()
        && parsed.base.is_none()
        && parsed.tail == crate::domain::refs::Tail::None
        && !args.graph
    {
        let slug = parsed.repo.clone().unwrap();
        let (o, n) = match super::parse_slug(&slug) {
            Ok(v) => v,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        };
        return branch_view(&o, &n, args.limit);
    }

    // The context repo.
    let explicit_repo = parsed_target
        .as_ref()
        .and_then(|parsed| parsed.repo.clone());
    let (repo, slug, branch) = match explicit_repo {
        Some(slug) => {
            let (o, n) = match super::parse_slug(&slug) {
                Ok(v) => v,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Usage);
                }
            };
            let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
                ui::error(&format!("{slug} doesn’t exist locally."));
                return Ok(ExitCode::Precondition);
            };
            (repo, slug, None)
        }
        // Aggregate views need the repository supplied by AGIT_SESSION, while ordinary history
        // also uses its explicitly selected branch.
        None => match ctx_repo(&cwd, args.target.is_some() || args.branches || args.graph) {
            Some(v) => v,
            None => return Ok(ExitCode::Ref),
        },
    };
    if args.branches {
        super::echo::emit("log", &[super::echo::Selection::new(&slug, source)]);
        return branch_view_of(&repo, &slug, args.limit);
    }
    if args.graph {
        if super::json::requested() {
            let facts = ref_facts(&repo)?;
            let graph = Graph::read(&repo, &facts)?;
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "view": "graph",
                    "repo": slug,
                    "commits": graph.order.iter().filter_map(|oid| graph.nodes.get(oid).map(|node| {
                        serde_json::json!({"oid": oid, "parents": node.parents, "subject": node.subject,
                            "committed_at": node.committed_at})
                    })).collect::<Vec<_>>(),
                    "refs": facts.iter().map(|(name, fact)| {
                        serde_json::json!({"ref": name, "oid": fact.oid, "peeled_oid": fact.peeled_oid})
                    }).collect::<Vec<_>>()
                })
            );
            return Ok(ExitCode::Ok);
        }
        let out = repo.git(&[
            "log",
            "--graph",
            "--all",
            "--oneline",
            "--decorate",
            "--format=%h %d %s",
        ])?;
        super::echo::emit("log", &[super::echo::Selection::new(&slug, source)]);
        print!("{out}");
        return Ok(ExitCode::Ok);
    }

    // Per-turn: resolve the starting ref.
    let (head, selected_ref) = match &args.target {
        Some(t) => match resolve_head(&repo, t) {
            Some(resolved) => {
                let selected = if parsed_target
                    .as_ref()
                    .is_some_and(|target| target.tail == crate::domain::refs::Tail::None)
                {
                    resolved
                        .branch
                        .clone()
                        .unwrap_or_else(|| resolved.sha.clone())
                } else {
                    resolved.sha.clone()
                };
                (resolved.sha, selected)
            }
            None => return Ok(ExitCode::Ref),
        },
        None => {
            let branch = branch.expect("targetless log has a context branch");
            match repo.git_opt(&["rev-parse", &format!("refs/heads/{branch}")]) {
                Some(h) => (h.trim().to_string(), branch),
                None => {
                    ui::error(&format!(
                        "selected session branch `{slug}@{branch}` does not exist locally."
                    ));
                    return Ok(ExitCode::Ref);
                }
            }
        }
    };
    let rows = match read_turn_history(
        &repo,
        &head,
        args.limit,
        args.kind.as_deref(),
        args.grep.as_deref(),
        args.since.as_deref(),
        &args.paths,
    )
    .and_then(|history| {
        if super::json::requested() {
            Ok(history.metadata())
        } else {
            history.with_activity(&repo)
        }
    }) {
        Ok(rows) => rows,
        Err(e) => {
            ui::error(&format!("cannot read this branch's history: {e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    if super::json::requested() {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "view": "turns",
                "repo": slug,
                "target": format!("{slug}@{head}"),
                "head_oid": head,
                "turns": rows.iter().map(|row| serde_json::json!({
                    "oid": row.oid,
                    "turn": row.turn,
                    "kind": row.kind,
                    "subject": row.subject,
                    "tags": row.tags,
                    "code_anchor": row.code,
                    "milestone": row.milestone,
                    "committed_at": chrono::DateTime::<chrono::Utc>::from(row.at).timestamp(),
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(ExitCode::Ok);
    }
    super::echo::emit(
        "log",
        &[super::echo::Selection::new(
            format!("{slug}@{selected_ref}"),
            source,
        )],
    );
    if rows.is_empty() {
        println!("no turns match.");
        return Ok(ExitCode::Ok);
    }
    for r in &rows {
        let activity = r.activity_label();
        let activity = if activity.is_empty() {
            activity
        } else {
            format!("{activity} ")
        };
        if args.oneline {
            println!(
                "{} {} {} {activity}{}",
                turn_label(r.turn),
                r.short,
                kind_badge(&r.kind),
                r.subject
            );
        } else {
            let mut line = format!(
                "{} {} {} {activity}{}",
                turn_label(r.turn),
                r.short,
                kind_badge(&r.kind),
                r.subject
            );
            if !r.tags.is_empty() {
                line.push_str(&format!("  ⌂ {}", r.tags.join(",")));
            }
            println!("{line}");
            if let Some(c) = &r.code {
                println!("      {}", ui::dim(&format!("code {c}")));
            }
            if let Some(m) = &r.milestone {
                println!("      ★ {}", m);
            }
        }
    }
    Ok(ExitCode::Ok)
}

fn wants_tui(args: &Args) -> bool {
    args.target.is_none()
        && !args.branches
        && !args.graph
        && !args.oneline
        && args.kind.is_none()
        && args.grep.is_none()
        && args.since.is_none()
        && args.paths.is_empty()
        && args.limit == DEFAULT_LIMIT
}

/// One row of the per-turn view.
///
/// `log`'s text rendering and the Timeline screen share this one — two separate fetches would
/// drift apart sooner or later over something like "where does `#n` start counting", and that
/// drift has no symptom: both sides look right.
#[derive(Debug, Clone)]
pub struct Turn {
    /// The turn ordinal: only a `kind: turn` commit has one. Birth, fork, file and merge take
    /// no number.
    pub turn: Option<u32>,
    pub oid: String,
    pub short: String,
    pub kind: Kind,
    pub subject: String,
    pub tags: Vec<String>,
    pub code: Option<String>,
    pub milestone: Option<String>,
    pub activity: Option<crate::domain::turn::activity::Activity>,
    /// Commit time. The text rendering does not use it; Timeline shows "how long ago" from it.
    pub at: std::time::SystemTime,
}

impl Turn {
    pub fn activity_label(&self) -> String {
        match self.activity {
            Some(a) => format!("{} events {} ToolUse", a.events, a.tools),
            None if self.kind == Kind::Turn => "events ? ToolUse ?".into(),
            None => String::new(),
        }
    }
}

fn kind_badge(k: &Kind) -> &'static str {
    match k {
        Kind::Turn => "[turn ]",
        Kind::Merge => "[merge]",
        Kind::View => "[view ]",
        Kind::File => "[file ]",
        Kind::Archive => "[archive]",
    }
}

/// The left column: `#n` is printed only on the commit that settled that turn, every other
/// commit stays blank — so the number in the left column and what `<ref>#n` resolves to are the
/// same commit.
fn turn_label(turn: Option<u32>) -> String {
    match turn {
        Some(t) => format!("#{t:>3}"),
        None => "    ".into(),
    }
}

/// Text and Timeline rows include activity derived from the selected immutable turn additions.
pub fn turns(
    repo: &Repo,
    head: &str,
    limit: usize,
    kind: Option<&str>,
    grep: Option<&str>,
    since_git: Option<&str>,
    paths: &[String],
) -> crate::Result<Vec<Turn>> {
    read_turn_history(repo, head, limit, kind, grep, since_git, paths)?.with_activity(repo)
}

struct TurnHistory {
    chain: crate::domain::refs::Chain,
    rows: Vec<(Option<usize>, Turn)>,
}

impl TurnHistory {
    fn metadata(self) -> Vec<Turn> {
        self.rows.into_iter().map(|(_, row)| row).collect()
    }

    fn with_activity(self, repo: &Repo) -> crate::Result<Vec<Turn>> {
        let selected: Vec<usize> = self.rows.iter().filter_map(|(i, _)| *i).collect();
        let activity = crate::domain::turn::activity::read(repo, &self.chain, &selected)?;
        Ok(self
            .rows
            .into_iter()
            .map(|(i, mut row)| {
                row.activity = i.and_then(|i| activity.get(&i).copied());
                row
            })
            .collect())
    }
}

/// Commit metadata is independent of event availability; activity readers load their own evidence.
fn read_turn_history(
    repo: &Repo,
    head: &str,
    limit: usize,
    kind: Option<&str>,
    grep: Option<&str>,
    since_git: Option<&str>,
    paths: &[String],
) -> crate::Result<TurnHistory> {
    let mut cmd: Vec<String> = vec![
        "log".into(),
        "--first-parent".into(),
        "--reverse".into(),
        "--format=%H%x00%s%x00%ct".into(),
        head.into(),
    ];
    if let Some(s) = since_git {
        cmd.push(format!("--since={s}"));
    }
    if !paths.is_empty() {
        cmd.push("--".into());
        for p in paths {
            cmd.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    let out = repo.git(&arg_refs)?;
    // Numbering comes from the **unfiltered** chain: a commit dropped by `--since` / `--grep` /
    // a path still holds its place, which is what makes the left-column number and what
    // `<ref>#n` resolves to the same commit.
    let chain = crate::domain::refs::Chain::read(repo, head)?;
    let by_sha: std::collections::HashMap<&str, usize> = chain
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.sha.as_str(), i))
        .collect();

    // Tags are asked for once. One `git tag --points-at` per commit costs in proportion to the
    // length of the history, and this path runs again on every Timeline repaint.
    let tag_of = tag_map(repo);
    let mut rows = vec![];
    for line in out.lines() {
        let mut parts = line.split('\0');
        let (Some(sha), Some(subject)) = (parts.next(), parts.next()) else {
            continue;
        };
        // `%ct` is part of the format string — the commit time costs no extra `git`.
        let at = parts
            .next()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .and_then(|secs| {
                let duration = std::time::Duration::from_secs(secs.unsigned_abs());
                if secs >= 0 {
                    std::time::UNIX_EPOCH.checked_add(duration)
                } else {
                    std::time::UNIX_EPOCH.checked_sub(duration)
                }
            })
            .unwrap_or(std::time::UNIX_EPOCH);
        let idx = by_sha.get(sha).copied();
        let snap = idx.and_then(|i| chain.entries[i].meta.clone());
        let k = snap.as_ref().map(|s| s.kind).unwrap_or(Kind::Turn);
        let kind_s = format!("{:?}", k).to_lowercase();
        if let Some(want) = kind
            && kind_s != want
        {
            continue;
        }
        if let Some(g) = grep
            && !subject.contains(g)
        {
            continue;
        }
        rows.push((
            idx,
            Turn {
                turn: idx.and_then(|i| chain.label(i)),
                oid: sha.to_string(),
                short: sha[..9.min(sha.len())].to_string(),
                kind: k,
                subject: subject.to_string(),
                tags: tag_of.get(sha).cloned().unwrap_or_default(),
                code: snap.as_ref().and_then(|s| s.code.clone()),
                milestone: snap.and_then(|s| s.milestone),
                activity: None,
                at,
            },
        ));
    }
    // Take the last `limit` rows from the tail (`--reverse` plus `limit` means "most recent",
    // not "earliest").
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    Ok(TurnHistory { chain, rows })
}

/// Every tag in the repo, grouped by the commit it points at, asked for in one `for-each-ref`.
///
/// The `*` column dereferences an annotated tag, and only what that points at is a commit; a
/// lightweight tag comes from `%(objectname)`. Both are needed — asking only for the latter
/// makes annotated tags disappear as a batch, and **silently**: one `⌂` fewer in the output,
/// with no error at all.
fn tag_map(repo: &Repo) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let Some(out) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname:short)%00%(objectname)%00%(*objectname)",
        "refs/tags/",
    ]) else {
        return map;
    };
    for line in out.lines() {
        let mut parts = line.split('\0');
        let (Some(name), Some(direct)) = (parts.next(), parts.next()) else {
            continue;
        };
        let target = parts.next().filter(|s| !s.is_empty()).unwrap_or(direct);
        if target.is_empty() || name.is_empty() {
            continue;
        }
        map.entry(target.to_string())
            .or_default()
            .push(name.to_string());
    }
    map
}

fn resolve_head(repo: &Repo, t: &str) -> Option<crate::domain::refs::Resolved> {
    let spec = crate::commands::target::parse_spec_for_repo(repo, t).ok()?;
    let spec = match super::context::substitute_at(spec) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return None;
        }
    };
    match crate::domain::refs::resolve(repo, &spec) {
        Ok(r) => Some(r),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            None
        }
    }
}

/// `24h`/`7d`/`4w` → the form `git --since` accepts.
fn parse_since_git(s: &str) -> Result<String, String> {
    let invalid = || "expected a non-negative whole number followed by h, d, or w".to_owned();
    let (num, unit) = [("h", "hours"), ("d", "days"), ("w", "weeks")]
        .into_iter()
        .find_map(|(suffix, unit)| s.strip_suffix(suffix).map(|num| (num, unit)))
        .ok_or_else(invalid)?;
    if num.is_empty() || !num.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let count = num.parse::<u32>().map_err(|_| invalid())?;
    Ok(format!("{count} {unit} ago"))
}

/// The context repo (slug + branch).
fn ctx_repo(
    cwd: &std::path::Path,
    explicit_target: bool,
) -> Option<(Repo, String, Option<String>)> {
    let resolved = if explicit_target {
        super::context::repo_for(cwd).map(|repo| (super::context::qualify(&repo), None))
    } else {
        super::context::resolve(cwd).map(|c| (super::context::qualify(&c.repo), Some(c.branch)))
    };
    match resolved {
        Ok((slug, branch)) => {
            let (o, n) = super::parse_slug(&slug).ok()?;
            let repo = Repo::open(crate::infra::config::repo_dir(&o, &n).ok()?)?;
            Some((repo, slug, branch))
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ui::hint("or name it: `agit log <owner/repo>` or `agit log <ref>`");
            None
        }
    }
}

/// The branch-level view for a given slug.
fn branch_view(owner: &str, name: &str, _limit: usize) -> CmdResult {
    let dir = crate::infra::config::repo_dir(owner, name)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!("{owner}/{name} doesn’t exist locally."));
        ui::hint(&format!("fetch it first: `agit clone {owner}/{name}`"));
        return Ok(ExitCode::Precondition);
    };
    super::echo::emit(
        "log",
        &[super::echo::Selection::new(
            format!("{owner}/{name}"),
            super::echo::Source::Explicit,
        )],
    );
    branch_view_of(&repo, &format!("{owner}/{name}"), _limit)
}

/// One row of the branch-level view. Fetching and rendering are separate for the same reason as
/// [`Turn`].
#[derive(Debug, Clone)]
pub struct BranchRow {
    pub name: String,
    /// The full ref this line is read through; a remote-only branch must not impersonate a
    /// local head.
    pub head: String,
    pub turns: u32,
    /// git's relative time (`%cr`) — already human-readable, never formatted a second time.
    pub when: String,
    /// The opening prompt: the subject of the first turn commit.
    pub gist: String,
    /// A file line claims no session and cannot be resumed.
    pub file_line: bool,
    /// `ahead/behind` relative to `origin/<b>`; an empty string when aligned or with no
    /// upstream.
    pub ahead_behind: String,
}

/// Branch-level fetching. **This is the only place that fetches them**; rendering belongs to the
/// caller.
///
/// A fixed number of `git` calls per repo, independent of the branch count (`docs/07_tui.md`
/// §4.1): the shapes in one batch, the ref facts in one batch, the whole commit graph once.
/// Asking per branch spawns processes linearly in the branch count when this page opens, and
/// every branch walks the shared stretch of history again.
pub fn branch_rows(repo: &Repo) -> Vec<BranchRow> {
    branch_details(repo, BranchSelection::Preferred, BranchDetail::History)
        .unwrap_or_default()
        .into_iter()
        .map(|row| BranchRow {
            name: row.name,
            head: row.reference,
            turns: row.turns,
            when: row.when,
            gist: ui::truncate(&row.opening_subject, 60),
            file_line: row.line == Some(meta::Line::File),
            ahead_behind: row.sync.display(),
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) enum BranchSelection {
    Local,
    All,
    Preferred,
}

#[derive(Clone, Copy)]
pub(super) enum BranchDetail {
    Summary,
    Sync,
    History,
}

#[derive(Debug, Serialize)]
pub(super) struct BranchDetails {
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub oid: String,
    pub local: bool,
    pub current: bool,
    pub line: Option<meta::Line>,
    pub session_id: Option<String>,
    pub runtime: Option<String>,
    pub turns: u32,
    pub committed_at: Option<i64>,
    pub opening_subject: String,
    pub sealed: bool,
    pub code_anchor: Option<String>,
    pub sync: BranchSync,
    #[serde(skip)]
    pub when: String,
}

#[derive(Debug, Serialize)]
pub(super) struct BranchSync {
    pub upstream_ref: Option<String>,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
}

impl BranchSync {
    fn display(&self) -> String {
        match (self.ahead, self.behind) {
            (Some(0), Some(0)) | (None, _) | (_, None) => String::new(),
            (Some(ahead), Some(behind)) => format!("{ahead}/{behind}"),
        }
    }
}

/// Branch reads share one ref snapshot and commit graph. Metadata and seal markers are read
/// through immutable object IDs so a concurrent branch update cannot combine different tips.
pub(super) fn branch_details(
    repo: &Repo,
    selection: BranchSelection,
    detail: BranchDetail,
) -> crate::Result<Vec<BranchDetails>> {
    let facts = ref_facts(repo)?;
    let graph = match detail {
        BranchDetail::Summary => None,
        BranchDetail::Sync | BranchDetail::History => Some(Graph::read(repo, &facts)?),
    };
    let refs: Vec<_> = facts
        .iter()
        .filter_map(|(reference, fact)| {
            reference
                .strip_prefix("refs/heads/")
                .map(|name| (reference, fact, name, true))
                .or_else(|| {
                    reference
                        .strip_prefix("refs/remotes/origin/")
                        .filter(|name| {
                            *name != "HEAD"
                                && match selection {
                                    BranchSelection::Local => false,
                                    BranchSelection::All => true,
                                    BranchSelection::Preferred => {
                                        !facts.contains_key(&format!("refs/heads/{name}"))
                                    }
                                }
                        })
                        .map(|name| (reference, fact, name, false))
                })
        })
        .collect();
    let oids: Vec<String> = refs
        .iter()
        .map(|(_, fact, _, _)| fact.oid.clone())
        .collect();
    let ancestry = match (&graph, detail) {
        (Some(graph), BranchDetail::History) => graph.first_parent_union(&oids),
        _ => oids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let metadata = meta::at_refs_result(repo, &ancestry)?;
    let metadata: std::collections::HashMap<_, _> = ancestry.into_iter().zip(metadata).collect();
    let openings = match (&graph, detail) {
        (Some(graph), BranchDetail::History) => graph.opening_subjects(&metadata),
        _ => Default::default(),
    };
    let mut sealed = Vec::with_capacity(oids.len());
    repo.git_cat_file_batch_check(
        oids.iter()
            .map(|oid| format!("{oid}:{}", super::branch::SEAL_FILE))
            .collect(),
        |_, kind, _| {
            sealed.push(kind != "missing");
            Ok(())
        },
    )?;
    anyhow::ensure!(
        sealed.len() == refs.len(),
        "incomplete branch seal marker response"
    );
    Ok(refs
        .into_iter()
        .zip(sealed)
        .map(|((reference, fact, name, local), sealed)| {
            let snap = metadata.get(&fact.oid).and_then(Option::as_ref);
            let upstream_ref = local.then(|| fact.upstream.clone()).flatten().or_else(|| {
                (local && facts.contains_key(&format!("refs/remotes/origin/{name}")))
                    .then(|| format!("refs/remotes/origin/{name}"))
            });
            let counts = graph.as_ref().and_then(|graph| {
                upstream_ref
                    .as_deref()
                    .and_then(|upstream| graph.divergence(reference, upstream))
            });
            BranchDetails {
                name: name.to_owned(),
                reference: reference.clone(),
                oid: fact.oid.clone(),
                local,
                current: fact.current,
                line: snap.map(|m| m.line),
                session_id: snap.map(|m| m.session.clone()).filter(|id| !id.is_empty()),
                runtime: snap
                    .map(|m| m.runtime.clone())
                    .filter(|runtime| !runtime.is_empty()),
                turns: branch_turns(snap),
                committed_at: fact.committed_at,
                opening_subject: openings
                    .get(fact.oid.as_str())
                    .copied()
                    .unwrap_or_default()
                    .to_owned(),
                sealed,
                code_anchor: snap.and_then(|m| m.code.clone()),
                sync: BranchSync {
                    upstream_ref,
                    ahead: counts.map(|(ahead, _)| ahead),
                    behind: counts.map(|(_, behind)| behind),
                },
                when: fact.when.clone(),
            }
        })
        .collect())
}

struct RefFacts {
    oid: String,
    peeled_oid: Option<String>,
    commit_oid: Option<String>,
    when: String,
    committed_at: Option<i64>,
    upstream: Option<String>,
    current: bool,
}

fn ref_facts(repo: &Repo) -> crate::Result<std::collections::BTreeMap<String, RefFacts>> {
    let out = repo.git(&[
        "for-each-ref",
        "--format=%(refname)%09%(objectname)%09%(committerdate:relative)%09%(committerdate:unix)%09%(upstream)%09%(HEAD)%09%(*objectname)%09%(objecttype)%09%(*objecttype)",
        "refs/heads/", "refs/remotes/", "refs/tags/",
    ])?;
    out.lines()
        .map(|line| {
            let mut f = line.split('\t');
            let name = f.next().unwrap_or_default();
            let oid = f.next().unwrap_or_default().to_string();
            anyhow::ensure!(
                !name.is_empty() && !oid.is_empty(),
                "incomplete branch ref response"
            );
            let when = f.next().unwrap_or_default().to_string();
            let committed_at = f.next().and_then(|at| at.parse().ok());
            let upstream = f.next().filter(|r| !r.is_empty()).map(str::to_owned);
            let current = f.next() == Some("*");
            let peeled_oid = f.next().filter(|oid| !oid.is_empty()).map(str::to_owned);
            let kind = f.next().unwrap_or_default();
            let peeled_kind = f.next().unwrap_or_default();
            let commit_oid = if kind == "commit" {
                Some(oid.clone())
            } else if peeled_kind == "commit" {
                peeled_oid.clone()
            } else {
                None
            };
            Ok((
                name.to_owned(),
                RefFacts {
                    oid,
                    peeled_oid,
                    commit_oid,
                    when,
                    committed_at,
                    upstream,
                    current,
                },
            ))
        })
        .collect()
}

/// The captured ref snapshot's commit graph, shared by every branch comparison and opener.
/// Commit OIDs enter Git through stdin so branch renames cannot change the traversed history.
struct Graph {
    nodes: std::collections::HashMap<String, GraphNode>,
    order: Vec<String>,
    tips: std::collections::HashMap<String, String>,
}

struct GraphNode {
    parents: Vec<String>,
    subject: String,
    committed_at: Option<i64>,
}

impl Graph {
    fn read(
        repo: &Repo,
        facts: &std::collections::BTreeMap<String, RefFacts>,
    ) -> crate::Result<Graph> {
        let tips = facts
            .iter()
            .map(|(name, fact)| (name.clone(), fact.oid.clone()))
            .collect();
        let mut graph = Graph {
            nodes: Default::default(),
            order: vec![],
            tips,
        };
        if facts.is_empty() {
            return Ok(graph);
        }
        use std::io::{Seek as _, Write as _};
        let mut input = tempfile::tempfile()?;
        let revisions: std::collections::BTreeSet<_> = facts
            .values()
            .filter_map(|fact| fact.commit_oid.as_ref())
            .collect();
        if revisions.is_empty() {
            return Ok(graph);
        }
        for oid in revisions {
            writeln!(input, "{oid}")?;
        }
        input.rewind()?;
        let out = repo.git_with_stdin_file(
            &[
                "log",
                "--stdin",
                "--topo-order",
                "--format=%H%x00%P%x00%s%x00%ct",
            ],
            input,
        )?;
        for line in out.lines() {
            let mut fields = line.split('\0');
            let oid = fields.next().unwrap_or_default().to_owned();
            let parents = fields
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let subject = fields.next().unwrap_or_default().to_owned();
            let committed_at = fields.next().and_then(|at| at.parse().ok());
            graph.order.push(oid.clone());
            graph.nodes.insert(
                oid,
                GraphNode {
                    parents,
                    subject,
                    committed_at,
                },
            );
        }
        Ok(graph)
    }

    fn first_parent_union(&self, tips: &[String]) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        for tip in tips {
            let mut next = Some(tip.as_str());
            while let Some(oid) = next {
                if !seen.insert(oid.to_owned()) {
                    break;
                }
                next = self
                    .nodes
                    .get(oid)
                    .and_then(|node| node.parents.first())
                    .map(String::as_str);
            }
        }
        self.order
            .iter()
            .filter(|oid| seen.contains(*oid))
            .cloned()
            .collect()
    }

    fn opening_subjects<'a>(
        &'a self,
        metadata: &std::collections::HashMap<String, Option<meta::Meta>>,
    ) -> std::collections::HashMap<&'a str, &'a str> {
        let mut openings = std::collections::HashMap::new();
        for oid in self.order.iter().rev() {
            let Some(current) = metadata.get(oid).and_then(Option::as_ref) else {
                continue;
            };
            if current.is_file_line() || current.session.is_empty() {
                continue;
            }
            let node = &self.nodes[oid];
            let inherited = node.parents.first().and_then(|parent| {
                let previous = metadata.get(parent)?.as_ref()?;
                (previous.session == current.session)
                    .then(|| openings.get(parent.as_str()).copied())
                    .flatten()
            });
            if let Some(subject) = inherited {
                openings.insert(oid.as_str(), subject);
            } else if current.kind == Kind::Turn && current.turn.is_some() {
                openings.insert(oid.as_str(), node.subject.as_str());
            }
        }
        openings
    }

    fn divergence(&self, head: &str, upstream: &str) -> Option<(usize, usize)> {
        let (left, right) = (self.tips.get(head)?, self.tips.get(upstream)?);
        if left == right {
            return Some((0, 0));
        }
        let left = self.reachable(left);
        let right = self.reachable(right);
        Some((
            left.difference(&right).count(),
            right.difference(&left).count(),
        ))
    }

    fn reachable(&self, tip: &str) -> std::collections::HashSet<String> {
        let mut seen = std::collections::HashSet::new();
        let mut pending = vec![tip.to_string()];
        while let Some(sha) = pending.pop() {
            if !seen.insert(sha.clone()) {
                continue;
            }
            if let Some(node) = self.nodes.get(&sha) {
                pending.extend(node.parents.iter().cloned());
            }
        }
        seen
    }
}

fn branch_view_of(repo: &Repo, slug: &str, _limit: usize) -> CmdResult {
    if super::json::requested() {
        let branches = branch_details(repo, BranchSelection::Preferred, BranchDetail::History)?;
        println!(
            "{}",
            serde_json::json!({"schema_version": 1, "view": "branches", "repo": slug, "branches": branches})
        );
        return Ok(ExitCode::Ok);
    }
    let rows = branch_rows(repo);
    if rows.is_empty() {
        println!("no branches yet — they’re born only via import / fork / new / run.");
        return Ok(ExitCode::Ok);
    }
    for r in &rows {
        println!(
            "{:<24} {:>4} turns · {}  “{}”{}{}",
            r.name,
            r.turns,
            r.when,
            r.gist,
            if r.file_line { " [file line]" } else { "" },
            if r.ahead_behind.is_empty() {
                String::new()
            } else {
                format!("  ↑↓ {}", r.ahead_behind)
            },
        );
    }
    Ok(ExitCode::Ok)
}

fn branch_turns(meta: Option<&meta::Meta>) -> u32 {
    meta.and_then(|m| m.turn).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-n`'s default must actually be [`DEFAULT_LIMIT`].
    ///
    /// This pins a failure that **shows no error**: Timeline opens only when no narrowing
    /// argument is present, and "did the user give `-n`" can only be answered by comparing
    /// against the default. Once the two split, the criterion goes silently wrong — nothing on
    /// screen changes; the screen that should open just does not open, or the one that should
    /// yield does not yield.
    #[test]
    fn the_default_limit_is_the_one_the_tui_criteria_compare_against() {
        #[derive(clap::Parser)]
        struct Only {
            #[command(flatten)]
            args: Args,
        }
        let parsed = <Only as clap::Parser>::parse_from(["agit-log"]);
        assert_eq!(parsed.args.limit, DEFAULT_LIMIT);
    }

    /// The tag table takes in both lightweight and annotated tags.
    ///
    /// `git tag --points-at <sha>` peels an annotated tag's shell on its own; the batched
    /// `for-each-ref` form does not. Asking it only for `%(objectname)` makes **annotated tags
    /// disappear as a batch** — and silently: one `⌂` fewer in the output, with no error at
    /// all.
    #[test]
    fn the_tag_table_peels_annotated_tags_and_keeps_lightweight_ones() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(d.path().join("a.txt"), "one\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("first").unwrap();
        let first = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("second").unwrap();
        let second = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        repo.git(&["tag", "light", &first]).unwrap();
        repo.git(&["tag", "-a", "heavy", "-m", "note", &second])
            .unwrap();

        let map = tag_map(&repo);
        assert_eq!(
            map.get(&first).map(Vec::as_slice),
            Some(["light".to_string()].as_slice())
        );
        assert_eq!(
            map.get(&second).map(Vec::as_slice),
            Some(["heavy".to_string()].as_slice()),
            "an annotated tag peels to the commit it points at, never to the tag object itself"
        );
        let heavy_oid = repo
            .git(&["rev-parse", "refs/tags/heavy"])
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(
            heavy_oid, second,
            "an annotated tag's oid differs from the commit's"
        );
        assert!(!map.contains_key(&heavy_oid));
    }

    #[test]
    fn branch_rows_count_divergence_without_human_git_output() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        std::fs::write(d.path().join("history"), "root\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("root").unwrap();

        repo.git(&["checkout", "-q", "-b", "remote-work"]).unwrap();
        std::fs::write(d.path().join("history"), "remote\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("remote turn").unwrap();
        let remote_head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        repo.git(&["checkout", "-q", "main"]).unwrap();
        std::fs::write(d.path().join("history"), "local\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("local turn").unwrap();
        let local_head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["branch", "-D", "remote-work"]).unwrap();

        for name in ["main", "tracked", "only"] {
            repo.git(&[
                "update-ref",
                &format!("refs/remotes/origin/{name}"),
                &remote_head,
            ])
            .unwrap();
        }
        repo.git(&["update-ref", "refs/heads/tracked", &local_head])
            .unwrap();
        repo.git(&["config", "branch.tracked.remote", "origin"])
            .unwrap();
        repo.git(&["config", "branch.tracked.merge", "refs/heads/tracked"])
            .unwrap();

        let rows = branch_rows(&repo);
        let row = |name: &str| rows.iter().find(|row| row.name == name).unwrap();
        assert_eq!(row("main").ahead_behind, "1/1");
        assert_eq!(row("tracked").ahead_behind, "1/1");
        assert_eq!(row("only").ahead_behind, "");
        assert_eq!(row("only").head, "refs/remotes/origin/only");
    }

    #[test]
    fn captured_graph_survives_ref_removal_and_skips_non_commit_tags() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("note"), "root").unwrap();
        repo.add_all().unwrap();
        repo.commit("root").unwrap();
        let root = repo.git(&["rev-parse", "HEAD"]).unwrap();
        std::fs::write(dir.path().join("note"), "child").unwrap();
        repo.add_all().unwrap();
        repo.commit("child").unwrap();
        let child = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let blob = repo.git(&["rev-parse", "HEAD:note"]).unwrap();
        repo.git(&["tag", "note-blob", &blob]).unwrap();
        let facts = ref_facts(&repo).unwrap();
        repo.git(&["update-ref", "-d", "refs/heads/main"]).unwrap();

        let captured = Graph::read(&repo, &facts).unwrap();
        assert_eq!(captured.nodes.len(), 2);
        assert_eq!(captured.nodes[&child].parents, vec![root]);
        assert_eq!(captured.tips["refs/heads/main"], child);
        assert!(!captured.nodes.contains_key(&blob));
        let current = Graph::read(&repo, &ref_facts(&repo).unwrap()).unwrap();
        assert!(current.nodes.is_empty());
    }

    #[test]
    fn parses_since() {
        assert_eq!(super::parse_since_git("24h").unwrap(), "24 hours ago");
        assert_eq!(super::parse_since_git("7d").unwrap(), "7 days ago");
        assert_eq!(super::parse_since_git("4w").unwrap(), "4 weeks ago");
        assert_eq!(super::parse_since_git("0d").unwrap(), "0 days ago");
        for invalid in [
            "",
            "h",
            "-1d",
            "+1d",
            "1.5d",
            "24hours",
            "4294967296d",
            "\u{5929}",
            "1\u{5929}",
        ] {
            assert!(super::parse_since_git(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn archive_kind_filter_reports_evidence_without_a_turn_label() {
        let (_temporary, repo) = crate::domain::refs::fixtures::forked_history();
        let head = crate::domain::refs::fixtures::archive_tail(&repo);
        let rows = turns(&repo, &head, 50, Some("archive"), None, None, &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, Kind::Archive);
        assert_eq!(rows[0].turn, None);
        assert_eq!(rows[0].short, head[..9]);
        assert_eq!(kind_badge(&rows[0].kind), "[archive]");
        assert_eq!(turn_label(rows[0].turn), "    ");
        let ordinary = turns(&repo, &head, 50, Some("turn"), None, None, &[]).unwrap();
        assert_eq!(
            ordinary.iter().map(|row| row.turn).collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3), Some(4)]
        );
    }

    #[test]
    fn branch_turns_ignore_non_conversation_commits() {
        let init = meta::Meta::new_file_line();
        assert_eq!(branch_turns(Some(&init)), 0, "init");
        let claim = meta::Meta::new_session_line("codex".into(), "/work".into());
        assert_eq!(branch_turns(Some(&claim)), 0, "claim");

        let mut head = meta::Meta::new(
            format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN)),
            "codex".into(),
            "/work".into(),
        );
        head.turn = Some(1);
        for (kind, operation) in [
            (Kind::Turn, "fork"),
            (Kind::File, "file"),
            (Kind::Merge, "merge"),
            (Kind::Archive, "archive"),
        ] {
            head.kind = kind;
            assert_eq!(branch_turns(Some(&head)), 1, "{operation}");
        }
    }

    /// The left column numbers only the commits that settled a turn: a fork identity, a file
    /// and a merge commit all carry over the head's turn ordinal, so printing it would collide
    /// with the real turn — and `<ref>#n` resolves to that turn.
    #[test]
    fn only_turn_commits_get_a_number() {
        let (_d, r) = crate::domain::refs::fixtures::forked_history();
        let head = r.git(&["rev-parse", "f1"]).unwrap().trim().to_string();
        let rows = turns(&r, &head, 50, None, None, None, &[]).unwrap();
        assert_eq!(rows.len(), 9);
        let numbered: Vec<u32> = rows.iter().filter_map(|r| r.turn).collect();
        assert_eq!(numbered, vec![1, 2, 3, 4]);
        for r in rows.iter().filter(|r| r.turn.is_some()) {
            assert_eq!(r.kind, Kind::Turn, "{}", r.subject);
        }
        assert!(
            rows.iter()
                .all(|r| r.turn.is_some() || r.kind != Kind::Turn),
            "every turn commit carries its number"
        );
        assert_eq!(turn_label(Some(3)), "#  3");
        assert_eq!(turn_label(None), "    ");
        // Filtering rows out leaves the remaining numbers unchanged: `--grep` picks only the
        // "turn 3" row, and it is still #3.
        let rows = turns(&r, &head, 50, None, Some("turn 3"), None, &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turn, Some(3));
        let rows = turns(&r, &head, 50, Some("file"), None, None, &[]).unwrap();
        assert!(
            rows.iter().all(|r| r.turn.is_none()),
            "file commits never get a number"
        );
    }

    /// One corrupt `session/meta.json` is an error, not "no turns match".
    #[test]
    fn corrupt_meta_is_an_error_not_an_empty_log() {
        let (_d, r) = crate::domain::refs::fixtures::forked_history();
        std::fs::write(r.root().join(meta::FILE), "{ not json").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("corrupt").unwrap());
        let head = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let e = match turns(&r, &head, 50, None, None, None, &[]) {
            Ok(rows) => panic!("corrupt meta produced {} rows", rows.len()),
            Err(e) => e,
        };
        assert!(e.to_string().contains("invalid"), "{e:#}");
    }

    /// A branch just made by import / fork that has not been pushed is in the branch view too.
    #[test]
    fn branch_view_lists_local_branches() {
        let (_d, r) = crate::domain::refs::fixtures::forked_history();
        // This has to count with no remote. Asking only `refs/remotes/origin/*` leaves it
        // empty until the first push — the branch view then says "no branches" while local
        // branches exist. `branches()` is local ∪ remote.
        assert!(r.remote_branches().is_empty(), "precondition: no remote");
        let mut all = r.branches();
        all.sort();
        assert_eq!(all, vec!["f1", "main", "s1"]);
        assert_eq!(all, r.local_branches());
    }
}
