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
//! Performance discipline: the per-turn view reads session/meta.json once per commit (one object
//! read); the branch-level view reads the first commit's gist once per branch. Neither opens a
//! live transcript.

use super::CmdResult;
use crate::domain::meta::{self, Kind};
use crate::domain::repo::Repo;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

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
    #[arg(long)]
    pub graph: bool,
    /// Branch-level view.
    #[arg(long)]
    pub branches: bool,
    /// Only one kind.
    #[arg(long, value_name = "turn|merge|view|file")]
    pub kind: Option<String>,
    /// Filter by message.
    #[arg(long, value_name = "pat")]
    pub grep: Option<String>,
    /// Only recent ones (24h / 7d / 4w).
    #[arg(long, value_name = "duration")]
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
    // `owner/repo` keeps the historical branch-list shorthand.  Once `@ref`
    // is present it is a normal explicit target and is rendered turn by turn.
    if let Some(parsed) = &parsed_target
        && parsed.repo.is_some()
        && parsed.base.is_none()
        && parsed.tail == crate::domain::refs::Tail::None
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
        // The branch-level and graph views ask only which repo, and the directory binding
        // answers that; they do not require this directory to also resolve a current branch.
        None => match ctx_repo(&cwd, args.target.is_some() || args.branches || args.graph) {
            Some(v) => v,
            None => return Ok(ExitCode::Ref),
        },
    };
    // With no arguments, and when the criterion holds, this enters Timeline
    // (`docs/07_tui.md` §3.3).
    //
    // Not one narrowing argument may be present: `--kind` / `--grep` / `--since` / `--oneline` /
    // `-n` each say "the text of this one slice is what I want", and that is a different thing
    // from opening a full-screen browser. `--graph` / `--branches` likewise — they have their
    // own shape.
    //
    // Every verdict is handled: being blocked inside an agent session must say so, and asking
    // for `--tui` explicitly with no terminal must report `Interactive`. The three entry points
    // must not have two behaviors.
    if args.target.is_none()
        && !args.branches
        && !args.graph
        && !args.oneline
        && args.kind.is_none()
        && args.grep.is_none()
        && args.since.is_none()
        && args.paths.is_empty()
        && args.limit == DEFAULT_LIMIT
        && let Some(b) = branch.clone()
    {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                // A branch that exists only on the remote (never checked out) does not enter
                // this screen: Timeline's starting point must be a commit `rev-parse` can
                // produce. The path below falls back to HEAD on its own.
                if let Some(head) = repo.git_opt(&["rev-parse", &format!("refs/heads/{b}")]) {
                    return crate::tui::screens::timeline::run(&repo, &slug, &b, head.trim());
                }
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }

    if args.branches {
        return branch_view_of(&repo, args.limit);
    }
    if args.graph {
        let out = repo.git(&[
            "log",
            "--graph",
            "--all",
            "--oneline",
            "--decorate",
            "--format=%h %d %s",
        ])?;
        print!("{out}");
        return Ok(ExitCode::Ok);
    }

    // Per-turn: resolve the starting ref.
    let head = match &args.target {
        Some(t) => match resolve_head(&repo, t) {
            Some(h) => h,
            None => return Ok(ExitCode::Ref),
        },
        None => match repo.git_opt(&[
            "rev-parse",
            &format!(
                "refs/heads/{}",
                branch.expect("targetless log has a context branch")
            ),
        ]) {
            Some(h) => h.trim().to_string(),
            None => {
                // A context branch that is not local falls back to HEAD.
                match repo.git_opt(&["rev-parse", "HEAD"]) {
                    Some(h) => h.trim().to_string(),
                    None => {
                        println!("no commits yet.");
                        return Ok(ExitCode::Ok);
                    }
                }
            }
        },
    };
    let since_git = args.since.as_deref().map(parse_since_git);

    let rows = match turns(
        &repo,
        &head,
        args.limit,
        args.kind.as_deref(),
        args.grep.as_deref(),
        since_git.as_deref(),
        &args.paths,
    ) {
        Ok(rows) => rows,
        Err(e) => {
            ui::error(&format!("cannot read this branch's history: {e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    if rows.is_empty() {
        println!("no turns match.");
        return Ok(ExitCode::Ok);
    }
    for r in &rows {
        if args.oneline {
            println!(
                "{} {} {} {}",
                turn_label(r.turn),
                r.short,
                kind_badge(&r.kind),
                r.subject
            );
        } else {
            let mut line = format!(
                "{} {} {} {}",
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
    pub short: String,
    pub kind: Kind,
    pub subject: String,
    pub tags: Vec<String>,
    pub code: Option<String>,
    pub milestone: Option<String>,
    /// Commit time. The text rendering does not use it; Timeline shows "how long ago" from it.
    pub at: std::time::SystemTime,
}

fn kind_badge(k: &Kind) -> &'static str {
    match k {
        Kind::Turn => "[turn ]",
        Kind::Merge => "[merge]",
        Kind::View => "[view ]",
        Kind::File => "[file ]",
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

/// Rows for the per-turn view: first-parent, counted from the root. **This is the only place
/// that fetches them**; rendering belongs to the caller.
///
/// History that cannot be read (git failed, some commit's `session/meta.json` is corrupt) is an
/// error, reported at the command boundary — flattened into an empty list, the outer layer says
/// "no turns match" and exits successfully, and one corrupt spot makes the whole history vanish
/// with no symptom.
pub fn turns(
    repo: &Repo,
    head: &str,
    limit: usize,
    kind: Option<&str>,
    grep: Option<&str>,
    since_git: Option<&str>,
    paths: &[String],
) -> crate::Result<Vec<Turn>> {
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
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
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
        rows.push(Turn {
            turn: idx.and_then(|i| chain.label(i)),
            short: sha[..9.min(sha.len())].to_string(),
            kind: k,
            subject: subject.to_string(),
            tags: tag_of.get(sha).cloned().unwrap_or_default(),
            code: snap.as_ref().and_then(|s| s.code.clone()),
            milestone: snap.and_then(|s| s.milestone),
            at,
        });
    }
    // Take the last `limit` rows from the tail (`--reverse` plus `limit` means "most recent",
    // not "earliest").
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    Ok(rows)
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

fn resolve_head(repo: &Repo, t: &str) -> Option<String> {
    let spec = crate::commands::target::parse_spec_for_repo(repo, t).ok()?;
    let spec = match super::context::substitute_at(spec) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return None;
        }
    };
    match crate::domain::refs::resolve(repo, &spec) {
        Ok(r) => Some(r.sha),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            None
        }
    }
}

/// `24h`/`7d`/`4w` → the form `git --since` accepts.
fn parse_since_git(s: &str) -> String {
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let unit = match unit {
        "h" => "hours",
        "d" => "days",
        "w" => "weeks",
        _ => unit,
    };
    format!("{num} {unit} ago")
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
    branch_view_of(&repo, _limit)
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
    // Local branches first (including ones just made by import / fork / new that have not been
    // pushed), then the remote-tracking-only ones (just cloned).
    let local = repo.local_branches();
    let mut branches = local.clone();
    let remote: Vec<String> = repo.remote_branches();
    for b in &remote {
        if !branches.contains(b) {
            branches.push(b.clone());
        }
    }
    let has_local: std::collections::HashSet<String> = local.into_iter().collect();
    let heads: Vec<String> = branches
        .iter()
        .map(|b| {
            if has_local.contains(b) {
                format!("refs/heads/{b}")
            } else {
                format!("refs/remotes/origin/{b}")
            }
        })
        .collect();
    let snaps = meta::at_refs(repo, &heads);
    let facts = ref_facts(repo);
    let graph = Graph::read(repo);
    let remote: std::collections::HashSet<String> = remote.into_iter().collect();
    branches
        .iter()
        .zip(heads.iter())
        .zip(snaps)
        .map(|((b, head), snap)| {
            let f = facts.get(head.as_str());
            let upstream = f
                .map(|f| f.upstream.clone())
                .filter(|upstream| !upstream.is_empty())
                .or_else(|| {
                    (has_local.contains(b) && remote.contains(b))
                        .then(|| format!("refs/remotes/origin/{b}"))
                });
            BranchRow {
                name: b.clone(),
                head: head.clone(),
                turns: branch_turns(snap.as_ref()),
                when: f.map(|f| f.when.clone()).unwrap_or_default(),
                gist: graph
                    .opening(head)
                    .map(|g| ui::truncate(&g, 60))
                    .unwrap_or_default(),
                file_line: snap.as_ref().is_some_and(|m| m.is_file_line()),
                ahead_behind: upstream
                    .map(|upstream| graph.divergence(head, &upstream))
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// The facts about one ref that `for-each-ref` answers directly.
struct RefFacts {
    /// git's relative time (the same rendering as `%cr`).
    when: String,
    /// The full upstream ref; an empty string means no upstream is configured.
    upstream: String,
}

/// Each ref's time and tracking state, asked for in one `for-each-ref`. Local and remote refs
/// are both collected.
fn ref_facts(repo: &Repo) -> std::collections::HashMap<String, RefFacts> {
    let Some(out) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname)%09%(committerdate:relative)%09%(upstream)",
        "refs/heads/",
        "refs/remotes/origin/",
    ]) else {
        return Default::default();
    };
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let name = f.next()?;
            let when = f.next().unwrap_or("").trim().to_string();
            let upstream = f.next().unwrap_or("").trim().to_string();
            Some((name.to_string(), RefFacts { when, upstream }))
        })
        .collect()
}

/// The whole repo's commit graph: every commit's parents and subject, read in one
/// `git log --all`.
///
/// The opening prompt is "the subject of the second commit from the root along the first-parent
/// chain". Walking that per branch traverses the shared stretch of history over and over — and a
/// branch is usually forked off that shared history. One walk of the whole graph is strictly
/// less work, and spawns a single process.
struct Graph {
    /// sha → (all parents, subject)
    nodes: std::collections::HashMap<String, (Vec<String>, String)>,
    /// ref → the sha it points at
    tips: std::collections::HashMap<String, String>,
}

impl Graph {
    fn read(repo: &Repo) -> Graph {
        let nodes = repo
            .git_opt(&["log", "--all", "--format=%H%x00%P%x00%s"])
            .map(|out| {
                out.lines()
                    .filter_map(|line| {
                        let mut f = line.split('\0');
                        let sha = f.next()?.to_string();
                        let parents = f
                            .next()
                            .unwrap_or("")
                            .split_whitespace()
                            .map(str::to_string)
                            .collect();
                        Some((sha, (parents, f.next().unwrap_or("").to_string())))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tips = repo
            .git_opt(&[
                "for-each-ref",
                "--format=%(refname)%09%(objectname)",
                "refs/heads/",
                "refs/remotes/",
            ])
            .map(|out| {
                out.lines()
                    .filter_map(|l| l.split_once('\t'))
                    .map(|(r, o)| (r.to_string(), o.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Graph { nodes, tips }
    }

    /// This ref's opening prompt: the subject of the second commit from the root along the
    /// first-parent chain.
    fn opening(&self, head: &str) -> Option<String> {
        let mut sha = self.tips.get(head)?.clone();
        let mut chain = vec![sha.clone()];
        // A git commit graph has no cycles, but this reads external data — a cap keeps one
        // bad dataset from hanging this screen in a loop.
        while chain.len() <= self.nodes.len() {
            let Some((parents, _)) = self.nodes.get(&sha) else {
                break;
            };
            let Some(p) = parents.first().cloned() else {
                break;
            };
            chain.push(p.clone());
            sha = p;
        }
        // The second one counting back from the root.
        let second = chain.get(chain.len().checked_sub(2)?)?;
        self.nodes.get(second).map(|(_, subject)| subject.clone())
    }

    /// The difference between two refs' reachable sets; nothing is shown when the far side does
    /// not exist or the two are aligned.
    fn divergence(&self, head: &str, upstream: &str) -> String {
        let (Some(left), Some(right)) = (self.tips.get(head), self.tips.get(upstream)) else {
            return String::new();
        };
        let left = self.reachable(left);
        let right = self.reachable(right);
        let ahead = left.difference(&right).count();
        let behind = right.difference(&left).count();
        if ahead == 0 && behind == 0 {
            String::new()
        } else {
            format!("{ahead}/{behind}")
        }
    }

    fn reachable(&self, tip: &str) -> std::collections::HashSet<String> {
        let mut seen = std::collections::HashSet::new();
        let mut pending = vec![tip.to_string()];
        while let Some(sha) = pending.pop() {
            if !seen.insert(sha.clone()) {
                continue;
            }
            if let Some((parents, _)) = self.nodes.get(&sha) {
                pending.extend(parents.iter().cloned());
            }
        }
        seen
    }
}

fn branch_view_of(repo: &Repo, _limit: usize) -> CmdResult {
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
    fn parses_since() {
        assert_eq!(super::parse_since_git("24h"), "24 hours ago");
        assert_eq!(super::parse_since_git("7d"), "7 days ago");
        assert_eq!(super::parse_since_git("4w"), "4 weeks ago");
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
