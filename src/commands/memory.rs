//! `agit memory` — how memory flows between the runtime directory, the session branch and main.
//!
//! # The mirror model
//!
//! The runtime's own memory directory (Claude Code's `~/.claude/projects/<project>/memory/`) is
//! the **live copy** and agit does not take it over; `memory/` in the session branch tree is its
//! **versioned snapshot**. The two sync at two fixed moments:
//!
//! ```text
//! start (new / resume / run / fork) branch memory/ ──union──▶ <mem dir>/agit/<owner>/<name>/<branch>/
//! settle (agit commit)              <mem dir> ──changed since baseline──▶ branch memory/ (a file commit)
//! ```
//!
//! The union never overwrites this machine's own files: branch files go into a per-branch mirror
//! subdirectory and the runtime's `MEMORY.md` only gains one marked index section pointing at
//! them; a local top-level file with the same name and the same content is not placed again.
//!
//! # Only what this session changed
//!
//! What enters the branch at settlement is the change **against the baseline**: at start, the
//! hash of every top-level file and of every mirrored file is recorded (the baseline lives in the
//! git directory of this branch's worktree, independent of the link — a session started by
//! `agit new` has no link until its first settlement). Personal memory that was already there
//! before the start and was not touched this session does not enter the branch, even when it
//! differs from the branch: this is where the privacy boundary is drawn. With no baseline (the
//! session was not started through agit) settlement only records one and collects nothing; an
//! explicit `agit memory sync` takes in everything currently at the top level.
//!
//! Collection computes a "plan" ([`plan_collect`]) first and then executes it; `status` speaks
//! from the same plan, so every row of its table is what the next `commit` will do. Files the
//! agent edited or deleted in the mirror are judged against the baseline the same way: an edit
//! goes into the branch, a deletion is deleted from the branch. When materialization at start
//! finds mirror edits that never reached the branch (never collected, or refused by the secret
//! scan) it keeps that copy and reports a conflict — branch bytes never overwrite the only copy.
//!
//! # Only top-level Markdown
//!
//! The runtime directory and the branch `memory/` both look only at the first level's `*.md`;
//! subdirectories and other extensions neither enter the branch nor materialize. Claude Code's
//! memory has exactly this shape, and one boundary on both sides is what keeps a file from being
//! "on the branch but impossible to materialize". The runtime's own `MEMORY.md` is this machine's
//! index: neither collected nor mirrored.
//!
//! # main advances only by explicit promotion
//!
//! A session branch snapshots itself; the `main` file line advances only through
//! `agit memory distill` (or a merge): main gets pushed and inherited by a colleague's
//! `agit new`, while a runtime memory directory naturally holds personal feedback and privacy, so
//! every file passes a secret scan and a per-item confirmation before it enters main. A file
//! inherited from main and deleted on the branch is carried over as a deletion as long as main is
//! still at the inherited version; a file main changed itself after the fork is a modify/delete
//! conflict — reported, never deleted automatically. `commit --milestone` and `push` remind you
//! how many items are not distilled yet.
//!
//! Only Claude Code has a per-project memory directory on disk (see
//! [`crate::infra::runtime_memory`]); on the other runtimes both steps are no-ops, and `distill`
//! and `status` work as usual.

use super::CmdResult;
use crate::domain::meta::{self, Kind};
use crate::domain::repo::Repo;
use crate::domain::secrets;
use crate::{ExitCode, ui};
use anyhow::Context as _;
use clap::Args as ClapArgs;
use sha2::Digest as _;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// The memory directory in the branch tree.
pub const TREE_DIR: &str = "memory";
/// The subdirectory of the runtime directory that holds branch memory.
pub const MIRROR_DIR: &str = "agit";
/// The runtime's own index file: Claude Code reads it at the start of every session and edits it
/// when it writes memory. It describes **this machine's** directory (relative links to local
/// files, plus the index section agit writes itself), so it is neither collected into the branch
/// nor placed into the mirror — index lines for branch files are regenerated from each file's own
/// frontmatter.
pub const INDEX_FILE: &str = "MEMORY.md";
/// Markers of the index section agit maintains inside the runtime `MEMORY.md`.
const INDEX_BEGIN: &str = "<!-- agit:memory:begin";
const INDEX_END: &str = "<!-- agit:memory:end -->";
const BASELINE_FILE: &str = "agit-memory-baseline.json";
/// The `memory.track` setting: `session` (the default) or `off`.
const TRACK_KEY: &str = "memory.track";

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    /// Target `<owner/repo>@<branch>` (default: the current session).
    #[arg(long, value_name = "owner/repo@branch", global = true)]
    pub into: Option<String>,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// What differs between the runtime directory, this branch and main.
    Status,
    /// Diff of memory files between this branch and main (or one file).
    Diff { path: Option<String> },
    /// Carry memory changes from this branch into main (secret-scanned, confirmed one by one).
    Distill {
        /// Only these files (names under memory/). Default: every change against main.
        paths: Vec<String>,
        /// Skip the per-file confirmation.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Sync now: runtime directory → branch, then branch → runtime directory.
    Sync,
}

/// `agit distill` — the top-level alias for `agit memory distill`.
#[derive(ClapArgs)]
pub struct DistillArgs {
    /// Only these files (names under memory/). Default: every change against main.
    pub paths: Vec<String>,
    /// Skip the per-file confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Target `<owner/repo>@<branch>` (default: the current session).
    #[arg(long, value_name = "owner/repo@branch")]
    pub into: Option<String>,
}

pub fn run_distill(args: DistillArgs) -> CmdResult {
    run(Args {
        cmd: Some(Cmd::Distill {
            paths: args.paths,
            yes: args.yes,
        }),
        into: args.into,
    })
}

/// The counts from one sync.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// The runtime memory directory.
    pub dir: PathBuf,
    /// Files written into the runtime directory (the mirror subdirectory).
    pub placed: usize,
    /// Files skipped because the local top level already has the same name and content.
    pub already_local: usize,
    /// Files collected into the branch (added + changed + deleted).
    pub collected: usize,
    /// Files not collected because of a suspected secret.
    pub refused: Vec<String>,
    /// The commit that landed.
    pub commit: Option<String>,
    /// Collection is turned off by `memory.track = off`.
    pub tracking_off: bool,
    /// An error that arrives once the commit has already landed (the baseline was not written):
    /// not a failure, but said out loud.
    pub warnings: Vec<String>,
    /// A mirror edit that could not enter the branch; materialization kept it instead of
    /// overwriting it with the branch content.
    pub conflicts: Vec<String>,
}

/// How settlement treats a directory with no baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Collect only changes against the baseline; with no baseline, record one and collect
    /// nothing (`agit commit`).
    SinceBaseline,
    /// With no baseline, take in everything currently at the top level (an explicit
    /// `agit memory sync`).
    EverythingIfNoBaseline,
}

/// The collection policy: the gate and the scope. The gate comes from config by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub track: bool,
    pub scope: Scope,
}

impl Policy {
    fn from_config(scope: Scope) -> Policy {
        Policy {
            track: super::config::get(TRACK_KEY).as_deref() != Some("off"),
            scope,
        }
    }
}

// ─────────────────────── Branch side ──────────────────────

fn tree_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// Only top-level Markdown is a managed memory file.
fn is_memory_file(name: &str) -> bool {
    name.ends_with(".md") && name != INDEX_FILE && !name.starts_with('.')
}

/// The bytes of every `*.md` at the first level of `memory/` on one ref.
fn branch_files(repo: &Repo, git_ref: &str) -> crate::Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for path in repo.ls_tree_result(git_ref)? {
        let Some(name) = path.strip_prefix(&format!("{TREE_DIR}/")) else {
            continue;
        };
        if name.contains('/') || !is_memory_file(name) {
            continue;
        }
        let bytes = repo
            .git_bytes_result(&["show", &format!("{git_ref}:{path}")])
            .with_context(|| format!("cannot read {path} at {git_ref}"))?;
        out.insert(name.to_string(), bytes);
    }
    Ok(out)
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Land a file commit on one branch that changes only `memory/`, returning the new commit.
///
/// It goes through plumbing plus an expected-old CAS and, like settlement, never through a real
/// index; a worktree with this branch checked out is refreshed by
/// [`super::plumbing::update_branch_cas_and_refresh`].
fn commit_memory(
    primary: &Repo,
    branch: &str,
    edits: BTreeMap<String, Option<Vec<u8>>>,
    message: &str,
) -> crate::Result<String> {
    commit_memory_at(primary, branch, None, edits, message)
}

/// [`commit_memory`], additionally requiring that the branch tip right now is `expected`: the
/// decision was made against that tip, a moved tip voids it, and better to fail than to land a
/// stale conclusion.
fn commit_memory_at(
    primary: &Repo,
    branch: &str,
    expected: Option<&str>,
    edits: BTreeMap<String, Option<Vec<u8>>>,
    message: &str,
) -> crate::Result<String> {
    let refname = tree_ref(branch);
    let tip = primary.git(&["rev-parse", "--verify", &format!("{refname}^{{commit}}")])?;
    let tip = tip.trim().to_string();
    if let Some(expected) = expected
        && expected != tip
    {
        anyhow::bail!(
            "`{branch}` moved while the change was being prepared; run the command again"
        );
    }
    let mut snap = meta::read_at_ref(primary, &tip)
        .ok_or_else(|| anyhow::anyhow!("`{branch}` carries no {}", meta::FILE))?;
    snap.kind = Kind::File;
    snap.milestone = None;
    let mut all: Vec<(String, Option<Vec<u8>>)> = edits
        .into_iter()
        .map(|(name, bytes)| (format!("{TREE_DIR}/{name}"), bytes))
        .collect();
    all.push((
        meta::FILE.to_string(),
        Some(meta::to_text(&snap)?.into_bytes()),
    ));
    let tree = super::plumbing::tree_apply_owned(primary, &tip, all)?;
    let commit = super::plumbing::commit_tree(primary, &tree, &[&tip], message)?;
    let checkout = super::worktree::existing(primary, branch)?.unwrap_or_else(|| primary.clone());
    super::plumbing::update_branch_cas_and_refresh(&checkout, branch, &commit, &tip, true)?;
    Ok(commit)
}

// ─────────────────────── Runtime side ───────────────────────

/// The `*.md` at the first level of a directory (the index file excluded).
///
/// A missing directory means "no memory yet" and answers empty; a directory entry or file that
/// **cannot be read** is an error and must not answer empty — an empty snapshot reads as "these
/// files were deleted" and lands in the branch as deletions, so one permission or transient I/O
/// error deletes memory that is still there.
fn md_files(dir: &Path) -> crate::Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot list {}", dir.display()));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot list {}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_memory_file(&name) {
            continue;
        }
        let kind = entry
            .file_type()
            .with_context(|| format!("cannot stat {}", path.display()))?;
        if !kind.is_file() {
            continue;
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        out.insert(name, bytes);
    }
    Ok(out)
}

/// The mirror subdirectory: split by repo **and** branch, so two parallel sessions each see
/// their own.
fn mirror_dir(mem_dir: &Path, slug: &str, branch: &str) -> PathBuf {
    let (owner, name) = slug.split_once('/').unwrap_or(("local", slug));
    mem_dir.join(MIRROR_DIR).join(owner).join(name).join(branch)
}

fn mirror_relative(slug: &str, branch: &str, file: &str) -> String {
    let (owner, name) = slug.split_once('/').unwrap_or(("local", slug));
    format!("{MIRROR_DIR}/{owner}/{name}/{branch}/{file}")
}

fn index_scope(slug: &str, branch: &str) -> String {
    format!("{slug}@{branch}")
}

/// What both sides looked like at the last sync.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct Baseline {
    #[serde(default)]
    dir: String,
    /// Top-level file name → sha256.
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// File name in the mirror subdirectory → sha256 (the content as we placed it).
    #[serde(default)]
    mirror: BTreeMap<String, String>,
}

impl Baseline {
    fn of(
        mem_dir: &Path,
        top: &BTreeMap<String, Vec<u8>>,
        mirror: &BTreeMap<String, Vec<u8>>,
    ) -> Baseline {
        Baseline {
            dir: mem_dir.to_string_lossy().into_owned(),
            files: top.iter().map(|(n, b)| (n.clone(), sha(b))).collect(),
            mirror: mirror.iter().map(|(n, b)| (n.clone(), sha(b))).collect(),
        }
    }
}

fn baseline_path(checkout: &Repo) -> crate::Result<PathBuf> {
    checkout.git_path(BASELINE_FILE)
}

fn read_baseline(checkout: &Repo, mem_dir: &Path) -> crate::Result<Option<Baseline>> {
    let path = baseline_path(checkout)?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let baseline: Baseline = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a memory baseline", path.display()))?;
    // The memory directory moved (a different `autoMemoryDirectory`, a different project root):
    // the old baseline is not about this place.
    if baseline.dir != mem_dir.to_string_lossy() {
        return Ok(None);
    }
    Ok(Some(baseline))
}

fn store_baseline(checkout: &Repo, baseline: &Baseline) -> crate::Result<()> {
    let path = baseline_path(checkout)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(baseline)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// The `name` / `description` from frontmatter (agit and Claude Code both write these two); the
/// file name when neither is there.
fn index_line(file: &str, bytes: &[u8], relative: &str) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut name = None;
    let mut description = None;
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("description:") {
                description = Some(v.trim().to_string());
            }
        }
    }
    let title = name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| file.trim_end_matches(".md").to_string());
    match description.filter(|d| !d.is_empty()) {
        Some(d) => format!("- [{title}]({relative}) — {d}"),
        None => format!("- [{title}]({relative})"),
    }
}

/// Replace agit's index section in `MEMORY.md` with `lines` (empty deletes the section). Nothing
/// else moves, down to the word.
fn upsert_index(existing: &str, scope: &str, lines: &[String]) -> String {
    let begin_tag = format!("{INDEX_BEGIN} {scope} -->");
    let mut kept = String::new();
    let mut skipping = false;
    for line in existing.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed == begin_tag {
            skipping = true;
            continue;
        }
        if skipping {
            if trimmed == INDEX_END {
                skipping = false;
            }
            continue;
        }
        kept.push_str(line);
    }
    if lines.is_empty() {
        return kept;
    }
    let mut out = if kept.trim().is_empty() {
        "# Memory Index\n\n".to_string()
    } else {
        kept
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&begin_tag);
    out.push('\n');
    out.push_str(&format!(
        "Shared memory from the AgentGit session {scope}. Managed by agit; edits here are collected back on `agit commit`.\n"
    ));
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(INDEX_END);
    out.push('\n');
    out
}

/// A suspected secret: the built-in rules plus the allowlist, plus the literals registered with
/// `agit secrets` (they can be low-entropy enough to hit no heuristic rule). A failed scan counts
/// as "there are hits" — better to collect one file less.
fn suspected_secret(bytes: &[u8], allowlist: &HashSet<String>) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    match secrets::scan_text_registered(&text, allowlist) {
        Ok(hits) if hits.is_empty() => None,
        Ok(hits) => Some(
            hits.iter()
                .map(|h| h.rule.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Err(error) => Some(format!("scan failed: {error:#}")),
    }
}

// ────────────────────── Collection plan ─────────────────────

/// What the next collection will do: `status` shows and `collect` executes the same plan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Newly written or changed at the top level against the baseline (name → content).
    pub top_changed: BTreeMap<String, Vec<u8>>,
    /// Changed in the mirror against what we placed there (a top-level file of the same name
    /// wins and is not here).
    pub mirror_changed: BTreeMap<String, Vec<u8>>,
    /// At the top level or in the mirror last time, gone from both now, still on the branch: the
    /// branch deletes it too.
    pub deleted: BTreeSet<String>,
    /// At the top level, in the baseline, untouched: stays local.
    pub untouched: BTreeSet<String>,
    /// No baseline: this session was not started through agit.
    pub no_baseline: bool,
}

/// Compute the plan against the baseline. `scope` decides how the top level counts when there is
/// no baseline.
fn plan_collect(
    baseline: Option<&Baseline>,
    top: &BTreeMap<String, Vec<u8>>,
    mirror: &BTreeMap<String, Vec<u8>>,
    in_branch: &BTreeMap<String, Vec<u8>>,
    scope: Scope,
) -> Plan {
    let mut plan = Plan::default();
    let Some(baseline) = baseline else {
        plan.no_baseline = true;
        if scope == Scope::EverythingIfNoBaseline {
            for (name, bytes) in top {
                if in_branch.get(name) != Some(bytes) {
                    plan.top_changed.insert(name.clone(), bytes.clone());
                }
            }
        }
        return plan;
    };
    for (name, bytes) in top {
        let same_as_baseline = baseline.files.get(name) == Some(&sha(bytes));
        if same_as_baseline {
            plan.untouched.insert(name.clone());
        } else if in_branch.get(name) != Some(bytes) {
            plan.top_changed.insert(name.clone(), bytes.clone());
        }
    }
    for (name, bytes) in mirror {
        if top.contains_key(name) {
            continue;
        }
        let same_as_placed = baseline.mirror.get(name) == Some(&sha(bytes));
        if !same_as_placed && in_branch.get(name) != Some(bytes) {
            plan.mirror_changed.insert(name.clone(), bytes.clone());
        }
    }
    for name in baseline.files.keys().chain(baseline.mirror.keys()) {
        if !top.contains_key(name) && !mirror.contains_key(name) && in_branch.contains_key(name) {
            plan.deleted.insert(name.clone());
        }
    }
    plan
}

// ──────────────────── The two directions ────────────────────

/// Before start: branch `memory/` → the runtime directory. None means this runtime has no memory
/// directory.
pub fn materialize(
    primary: &Repo,
    branch: &str,
    slug: &str,
    runtime: &str,
    cwd: &Path,
) -> crate::Result<Option<Report>> {
    let Some(mem) = crate::infra::runtime_memory::locate(runtime, cwd) else {
        return Ok(None);
    };
    materialize_with(
        primary,
        branch,
        slug,
        &mem.dir,
        Policy::from_config(Scope::SinceBaseline),
    )
    .map(Some)
}

/// The body of [`materialize`], with the directory and the policy given explicitly.
///
/// When the mirror still holds edits that never reached the branch (a settlement that failed, a
/// hook that did not run, a refusal by the secret scan) it collects once against the baseline
/// first; whatever cannot be collected stays where it is and is reported as a conflict, never
/// overwritten with the branch content.
pub fn materialize_with(
    primary: &Repo,
    branch: &str,
    slug: &str,
    mem_dir: &Path,
    policy: Policy,
) -> crate::Result<Report> {
    let checkout = super::worktree::checkout(primary, branch)?;
    let mirror = mirror_dir(mem_dir, slug, branch);
    let mut report = Report {
        dir: mem_dir.to_path_buf(),
        ..Report::default()
    };
    if policy.track && read_baseline(&checkout, mem_dir)?.is_some() {
        let collected = collect_with(primary, branch, slug, mem_dir, policy)?;
        report.warnings.extend(collected.warnings);
        report.refused.extend(collected.refused);
    }
    let baseline = read_baseline(&checkout, mem_dir)?.unwrap_or_default();
    let files = branch_files(&checkout, &tree_ref(branch))?;
    let top = md_files(mem_dir)?;
    let mirrored = md_files(&mirror)?;
    let mut index: Vec<String> = Vec::new();
    let mut keep: BTreeSet<String> = BTreeSet::new();
    // A conflicting file keeps the hash we placed in the baseline: it never entered the branch,
    // so the next collection still counts it as changed.
    let mut placed_hashes: BTreeMap<String, String> = BTreeMap::new();

    for (name, bytes) in &files {
        if top.get(name).is_some_and(|local| local == bytes) {
            report.already_local += 1;
            continue;
        }
        keep.insert(name.clone());
        let current = mirrored.get(name);
        if current == Some(bytes) {
            index.push(index_line(
                name,
                bytes,
                &mirror_relative(slug, branch, name),
            ));
            placed_hashes.insert(name.clone(), sha(bytes));
            continue;
        }
        if let Some(current) = current
            && baseline.mirror.get(name) != Some(&sha(current))
        {
            report.conflicts.push(name.clone());
            index.push(index_line(
                name,
                current,
                &mirror_relative(slug, branch, name),
            ));
            if let Some(placed) = baseline.mirror.get(name) {
                placed_hashes.insert(name.clone(), placed.clone());
            }
            continue;
        }
        std::fs::create_dir_all(&mirror)?;
        let dst = mirror.join(name);
        std::fs::write(&dst, bytes).with_context(|| format!("cannot write {}", dst.display()))?;
        report.placed += 1;
        index.push(index_line(
            name,
            bytes,
            &mirror_relative(slug, branch, name),
        ));
        placed_hashes.insert(name.clone(), sha(bytes));
    }
    // A mirrored file no longer on the branch is taken away — this subdirectory belongs to agit.
    // One edited without reaching the branch is kept all the same.
    for (name, current) in &mirrored {
        if keep.contains(name) {
            continue;
        }
        if baseline.mirror.get(name) != Some(&sha(current)) {
            report.conflicts.push(name.clone());
            index.push(index_line(
                name,
                current,
                &mirror_relative(slug, branch, name),
            ));
            if let Some(placed) = baseline.mirror.get(name) {
                placed_hashes.insert(name.clone(), placed.clone());
            }
            continue;
        }
        let _ = std::fs::remove_file(mirror.join(name));
    }
    if mirror.exists() && md_files(&mirror)?.is_empty() {
        let _ = std::fs::remove_dir(&mirror);
    }

    // The index section: written only when there are mirrored files, otherwise the old section
    // is deleted.
    let index_path = mem_dir.join(INDEX_FILE);
    let existing = std::fs::read_to_string(&index_path).unwrap_or_default();
    let next = upsert_index(&existing, &index_scope(slug, branch), &index);
    if next != existing && (!next.trim().is_empty() || index_path.exists()) {
        std::fs::create_dir_all(mem_dir)?;
        std::fs::write(&index_path, next)?;
    }

    let mut next_baseline = Baseline::of(mem_dir, &md_files(mem_dir)?, &BTreeMap::new());
    next_baseline.mirror = placed_hashes;
    store_baseline(&checkout, &next_baseline)?;
    Ok(report)
}

/// At settlement: the runtime directory → branch `memory/`. None means this runtime has no memory
/// directory.
pub fn collect(
    primary: &Repo,
    branch: &str,
    slug: &str,
    runtime: &str,
    cwd: &Path,
) -> crate::Result<Option<Report>> {
    let Some(mem) = crate::infra::runtime_memory::locate(runtime, cwd) else {
        return Ok(None);
    };
    if !mem.dir.is_dir() {
        return Ok(None);
    }
    collect_with(
        primary,
        branch,
        slug,
        &mem.dir,
        Policy::from_config(Scope::SinceBaseline),
    )
    .map(Some)
}

/// The body of [`collect`], with the directory and the policy given explicitly. Every collection
/// entry point passes the gate here.
///
/// Once the commit lands it goes into the report; a baseline that cannot be written afterwards is
/// only a warning — what the caller (the receipt of a strict RC settlement) wants is "the branch
/// tip now", and that is already a fact.
pub fn collect_with(
    primary: &Repo,
    branch: &str,
    slug: &str,
    mem_dir: &Path,
    policy: Policy,
) -> crate::Result<Report> {
    let mut report = Report {
        dir: mem_dir.to_path_buf(),
        ..Report::default()
    };
    if !policy.track {
        report.tracking_off = true;
        return Ok(report);
    }
    let checkout = super::worktree::checkout(primary, branch)?;
    let in_branch = branch_files(&checkout, &tree_ref(branch))?;
    let top = md_files(mem_dir)?;
    let mirror = md_files(&mirror_dir(mem_dir, slug, branch))?;
    let baseline = read_baseline(&checkout, mem_dir)?;
    let plan = plan_collect(baseline.as_ref(), &top, &mirror, &in_branch, policy.scope);
    let allowlist = secrets::load_allowlist(&crate::infra::config::agit_home()?);

    let mut edits: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    // Refused file names are tracked separately (the display string carries the rule name and is
    // not parseable): the baseline must not record them as "already placed", so the next
    // collection and the next materialization both see them again.
    let mut refused_names: BTreeSet<String> = BTreeSet::new();
    for (name, bytes) in plan.top_changed.iter().chain(plan.mirror_changed.iter()) {
        match suspected_secret(bytes, &allowlist) {
            Some(rule) => {
                report.refused.push(format!("{name} ({rule})"));
                refused_names.insert(name.clone());
            }
            None => {
                edits.insert(name.clone(), Some(bytes.clone()));
            }
        }
    }
    for name in &plan.deleted {
        edits.insert(name.clone(), None);
    }

    report.collected = edits.len();
    if !edits.is_empty() {
        let message = format!("agit: memory sync ({} changes)", edits.len());
        report.commit = Some(commit_memory(primary, branch, edits, &message)?);
    }
    let mut next = Baseline::of(mem_dir, &top, &mirror);
    if let Some(previous) = &baseline {
        for name in &refused_names {
            if mirror.contains_key(name) {
                match previous.mirror.get(name) {
                    Some(placed) => {
                        next.mirror.insert(name.clone(), placed.clone());
                    }
                    None => {
                        next.mirror.remove(name);
                    }
                }
            }
            if top.contains_key(name) {
                match previous.files.get(name) {
                    Some(seen) => {
                        next.files.insert(name.clone(), seen.clone());
                    }
                    None => {
                        next.files.remove(name);
                    }
                }
            }
        }
    }
    if let Err(error) = store_baseline(&checkout, &next) {
        report
            .warnings
            .push(format!("memory baseline was not updated: {error:#}"));
    }
    Ok(report)
}

// ──────────────────────── Distill ───────────────────────

/// One change waiting to be distilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// On the branch, missing from main or different there: carry it over.
    Carry(String),
    /// Inherited from main and later deleted on the branch, with main still at the inherited
    /// version: main deletes it too.
    Remove(String),
    /// Deleted on the branch and changed on main after the fork: a modify/delete conflict,
    /// reported and left alone.
    Conflict(String),
}

impl Pending {
    pub fn name(&self) -> &str {
        match self {
            Pending::Carry(n) | Pending::Remove(n) | Pending::Conflict(n) => n,
        }
    }
}

/// A distill plan: the candidates computed against **one** session tip and **one** main tip.
///
/// The scan, the confirmation and the landing all read bytes at those tips; if either tip moves,
/// the plan is void.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistillPlan {
    pub branch_tip: String,
    pub main_tip: String,
    pub items: Vec<Pending>,
}

fn tip_of(primary: &Repo, git_ref: &str) -> crate::Result<String> {
    Ok(primary
        .git(&["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")])?
        .trim()
        .to_string())
}

/// Compute the distill plan from the two tips as they stand right now.
pub fn distill_plan(primary: &Repo, branch: &str) -> crate::Result<DistillPlan> {
    let branch_tip = tip_of(primary, &tree_ref(branch))?;
    let main_tip = tip_of(primary, "refs/heads/main")?;
    let ours = branch_files(primary, &branch_tip)?;
    let main = branch_files(primary, &main_tip)?;
    let base = primary
        .git_opt(&["merge-base", &main_tip, &branch_tip])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let inherited = match &base {
        Some(base) => branch_files(primary, base)?,
        None => BTreeMap::new(),
    };
    let mut items = Vec::new();
    for (name, bytes) in &ours {
        if main.get(name) != Some(bytes) {
            items.push(Pending::Carry(name.clone()));
        }
    }
    for (name, on_main) in &main {
        if ours.contains_key(name) {
            continue;
        }
        match inherited.get(name) {
            None => {}
            Some(at_fork) if at_fork == on_main => items.push(Pending::Remove(name.clone())),
            Some(_) => items.push(Pending::Conflict(name.clone())),
        }
    }
    Ok(DistillPlan {
        branch_tip,
        main_tip,
        items,
    })
}

/// Changes in the branch `memory/` against main (the candidates of [`distill_plan`]).
pub fn distill_pending(primary: &Repo, branch: &str) -> crate::Result<Vec<Pending>> {
    if branch == "main" || !primary.has_ref("refs/heads/main") {
        return Ok(vec![]);
    }
    Ok(distill_plan(primary, branch)?.items)
}

/// Carry the chosen changes into main. A conflict item is not accepted. Returns the commit that
/// landed (None when there is nothing to carry).
///
/// The options were computed against the two tips in `plan`, and either side can advance while
/// the user confirms: once the session branch moves, the bytes to carry are no longer the ones
/// that were scanned and confirmed; once main moves, the three-way decision is void. Before
/// landing, this checks that the session tip has not moved, reads the bytes at that same tip, and
/// does a CAS on the main tip.
pub fn distill(
    primary: &Repo,
    plan: &DistillPlan,
    chosen: &[Pending],
) -> crate::Result<Option<String>> {
    if chosen.is_empty() {
        return Ok(None);
    }
    for item in chosen {
        anyhow::ensure!(
            plan.items.contains(item),
            "memory/{} is not part of this distill plan",
            item.name()
        );
    }
    let branch = primary
        .git_opt(&[
            "for-each-ref",
            "--points-at",
            &plan.branch_tip,
            "--format=%(refname:short)",
            "refs/heads/",
        ])
        .unwrap_or_default();
    let session_moved = !branch
        .lines()
        .map(str::trim)
        .any(|b| !b.is_empty() && b != "main");
    if session_moved {
        anyhow::bail!(
            "the session branch moved after this distill was planned; run `agit memory status` and choose again"
        );
    }
    let ours = branch_files(primary, &plan.branch_tip)?;
    let mut edits: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    for item in chosen {
        match item {
            Pending::Carry(name) => {
                let bytes = ours
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("the plan has no memory/{name}"))?;
                edits.insert(name.clone(), Some(bytes.clone()));
            }
            Pending::Remove(name) => {
                edits.insert(name.clone(), None);
            }
            Pending::Conflict(name) => {
                anyhow::bail!(
                    "memory/{name} was changed on main after this branch forked and deleted here; resolve it by hand"
                );
            }
        }
    }
    let message = format!(
        "agit: distill memory from {} ({} changes)",
        &plan.branch_tip[..9.min(plan.branch_tip.len())],
        edits.len()
    );
    commit_memory_at(primary, "main", Some(&plan.main_tip), edits, &message).map(Some)
}

/// A reminder of how many items on this branch are not distilled into main yet. It fails
/// silently — a reminder must not interrupt the main flow.
pub fn remind_pending(primary: &Repo, branch: &str) {
    let Ok(pending) = distill_pending(primary, branch) else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    ui::hint(&format!(
        "{} memory change{} on `{branch}` not yet in main: `agit memory status` / `agit distill`",
        pending.len(),
        if pending.len() == 1 { "" } else { "s" }
    ));
}

// ─────────────────────── Commands ───────────────────────

struct Target {
    primary: Repo,
    slug: String,
    branch: String,
}

fn target_of(into: Option<&str>) -> crate::Result<Option<Target>> {
    let cwd = std::env::current_dir()?;
    let (slug, branch) = match into {
        Some(raw) => {
            let t = super::target::branch_only(raw)?;
            let Some(slug) = t.repo else {
                anyhow::bail!("`--into` needs the full `<owner/repo>@<branch>` form");
            };
            (slug, t.base.expect("branch_only guarantees a branch"))
        }
        None => {
            let ctx = super::context::resolve(&cwd)?;
            (super::context::qualify(&ctx.repo), ctx.branch)
        }
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    let Some(primary) = Repo::open(&dir) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        return Ok(None);
    };
    Ok(Some(Target {
        primary,
        slug,
        branch,
    }))
}

pub fn run(args: Args) -> CmdResult {
    let Some(target) = target_of(args.into.as_deref())? else {
        return Ok(ExitCode::Precondition);
    };
    match args.cmd.unwrap_or(Cmd::Status) {
        Cmd::Status => status(&target),
        Cmd::Diff { path } => diff(&target, path.as_deref()),
        Cmd::Distill { paths, yes } => distill_cmd(&target, &paths, yes),
        Cmd::Sync => sync(&target),
    }
}

/// The runtime and working directory of this branch: read the meta at the tip (every session line
/// writes one); without one this is a file line and there is no runtime side to speak of.
fn runtime_of(target: &Target) -> Option<(String, PathBuf)> {
    let snap = meta::read_at_ref(&target.primary, &tree_ref(&target.branch))?;
    if snap.is_file_line() || snap.runtime.is_empty() {
        return None;
    }
    Some((snap.runtime, PathBuf::from(snap.cwd)))
}

fn status(target: &Target) -> CmdResult {
    let primary = &target.primary;
    let branch = target.branch.as_str();
    let ours = branch_files(primary, &tree_ref(branch))?;
    let main = if primary.has_ref("refs/heads/main") {
        branch_files(primary, "refs/heads/main")?
    } else {
        BTreeMap::new()
    };
    let pending = distill_pending(primary, branch)?;
    let runtime = runtime_of(target)
        .and_then(|(rt, cwd)| crate::infra::runtime_memory::locate(&rt, &cwd).map(|m| (rt, m.dir)));
    // An ordinary settlement does nothing when the memory directory does not exist at all (see
    // [`collect`]): status must not read that as an empty snapshot either and report a pile of
    // "will delete".
    let dir_missing = runtime.as_ref().is_some_and(|(_, dir)| !dir.is_dir());
    let (top, mirror, plan) = match &runtime {
        Some((_, dir)) if !dir_missing => {
            let top = md_files(dir)?;
            let mirror = md_files(&mirror_dir(dir, &target.slug, branch))?;
            let baseline = match super::worktree::existing(primary, branch)? {
                Some(checkout) => read_baseline(&checkout, dir)?,
                None => None,
            };
            let plan = plan_collect(
                baseline.as_ref(),
                &top,
                &mirror,
                &ours,
                Scope::SinceBaseline,
            );
            (top, mirror, plan)
        }
        _ => Default::default(),
    };

    println!("{}", ui::dim(&format!("  {} @ {branch}", target.slug)));
    match &runtime {
        Some((rt, dir)) => println!(
            "{}",
            ui::dim(&format!(
                "  runtime {rt}: {} (mirror under {})",
                ui::tilde(dir),
                mirror_relative(&target.slug, branch, "")
            ))
        ),
        None => println!(
            "{}",
            ui::dim("  runtime: no per-project memory directory for this session’s runtime")
        ),
    }

    let mut names: BTreeSet<&String> = ours.keys().collect();
    names.extend(main.keys());
    names.extend(top.keys());
    names.extend(mirror.keys());
    let mut rows = Vec::new();
    let pending_sync = plan.top_changed.len() + plan.mirror_changed.len() + plan.deleted.len();
    for name in names {
        let in_branch = ours.get(name);
        let in_main = main.get(name);
        let branch_col = match pending.iter().find(|p| p.name() == name) {
            Some(Pending::Carry(_)) if in_main.is_some() => "differs from main",
            Some(Pending::Carry(_)) => "not in main",
            Some(Pending::Remove(_)) => "deleted here",
            Some(Pending::Conflict(_)) => "deleted here, changed on main",
            None if in_branch.is_some() => "= main",
            None => "—",
        }
        .to_string();
        // The runtime column says exactly what the next `commit` will do.
        let runtime_col = if runtime.is_none() {
            String::new()
        } else if dir_missing {
            "—".to_string()
        } else if plan.deleted.contains(name) {
            "deleted locally → will delete".to_string()
        } else if plan.top_changed.contains_key(name) {
            if in_branch.is_some() {
                "changed locally"
            } else {
                "new locally"
            }
            .to_string()
        } else if plan.mirror_changed.contains_key(name) {
            "edited in mirror".to_string()
        } else if plan.untouched.contains(name) {
            "local, untouched".to_string()
        } else if top.get(name).is_some_and(|b| Some(b) == in_branch) {
            "= branch".to_string()
        } else if top.contains_key(name) && plan.no_baseline {
            "local (no baseline)".to_string()
        } else if mirror.get(name).is_some_and(|b| Some(b) == in_branch) {
            "mirrored".to_string()
        } else if mirror.contains_key(name) {
            "stale mirror".to_string()
        } else {
            "—".to_string()
        };
        rows.push(vec![
            name.clone(),
            branch_col,
            if in_main.is_some() {
                "yes".into()
            } else {
                "—".into()
            },
            runtime_col,
        ]);
    }
    if rows.is_empty() {
        println!("  no memory files anywhere yet.");
    } else {
        println!(
            "{}",
            ui::table::render(&["file", "branch", "main", "runtime"], &rows)
        );
    }
    println!();
    if dir_missing {
        println!(
            "  the runtime memory directory does not exist yet — nothing is collected until it appears"
        );
    }
    if plan.no_baseline && runtime.is_some() && !top.is_empty() {
        println!(
            "  this session has no memory baseline (it was not started through agit): `agit commit` only records one; `agit memory sync` adopts the local files"
        );
    }
    if pending_sync > 0 {
        println!(
            "  {pending_sync} local change{} not yet on `{branch}` — collected at the next `agit commit` (or `agit memory sync`)",
            if pending_sync == 1 { "" } else { "s" }
        );
    }
    let conflicts = pending
        .iter()
        .filter(|p| matches!(p, Pending::Conflict(_)))
        .count();
    let carry = pending.len() - conflicts;
    if carry > 0 {
        println!(
            "  {carry} change{} on `{branch}` not yet in main — `agit distill` carries them (secret-scanned, confirmed one by one)",
            if carry == 1 { "" } else { "s" }
        );
    }
    if conflicts > 0 {
        println!(
            "  {conflicts} modify/delete conflict{} with main — edit `$(agit repo path {}@main)` by hand",
            if conflicts == 1 { "" } else { "s" },
            target.slug
        );
    }
    if pending_sync == 0 && pending.is_empty() && !rows.is_empty() {
        println!(
            "  {} everything in sync",
            ui::ok(ui::theme::symbols().check)
        );
    }
    Ok(ExitCode::Ok)
}

fn diff(target: &Target, path: Option<&str>) -> CmdResult {
    if !target.primary.has_ref("refs/heads/main") {
        ui::error("this repo has no main file line to compare against.");
        return Ok(ExitCode::Precondition);
    }
    let scope = match path {
        Some(p) => format!("{TREE_DIR}/{p}"),
        None => TREE_DIR.to_string(),
    };
    let out = target.primary.git(&[
        "diff",
        "--stat",
        "-p",
        "refs/heads/main",
        &tree_ref(&target.branch),
        "--",
        &scope,
    ])?;
    if out.trim().is_empty() {
        println!("  memory on `{}` matches main.", target.branch);
    } else {
        print!("{out}");
    }
    Ok(ExitCode::Ok)
}

fn distill_cmd(target: &Target, paths: &[String], yes: bool) -> CmdResult {
    let primary = &target.primary;
    let branch = target.branch.as_str();
    if branch == "main" {
        ui::error("`main` is the file line itself — distill from a session branch.");
        return Ok(ExitCode::Usage);
    }
    if !primary.has_ref("refs/heads/main") {
        ui::error("this repo has no main file line.");
        return Ok(ExitCode::Precondition);
    }
    let plan = distill_plan(primary, branch)?;
    let pending = plan.items.clone();
    let wanted: Vec<Pending> = if paths.is_empty() {
        pending.clone()
    } else {
        let mut picked = Vec::new();
        for p in paths {
            match pending.iter().find(|item| item.name() == p) {
                Some(item) => picked.push(item.clone()),
                None => {
                    ui::error(&format!(
                        "memory/{p} is not a pending change on `{branch}` (already matches main, or never existed)."
                    ));
                    return Ok(ExitCode::Ref);
                }
            }
        }
        picked
    };
    if wanted.is_empty() {
        println!("  memory on `{branch}` already matches main — nothing to distill.");
        return Ok(ExitCode::Ok);
    }

    // Every file to carry passes the secret scan first: main gets pushed and inherited by
    // others. The scan reads the bytes at the tip in the plan, and the landing carries those.
    let ours = branch_files(primary, &plan.branch_tip)?;
    let allowlist = secrets::load_allowlist(&crate::infra::config::agit_home()?);
    let mut chosen = Vec::new();
    for item in &wanted {
        let question = match item {
            Pending::Carry(name) => {
                if let Some(rule) = suspected_secret(&ours[name], &allowlist) {
                    ui::warning(&format!(
                        "memory/{name} skipped — suspected secret ({rule}); clean it up first"
                    ));
                    continue;
                }
                format!("carry memory/{name} into main?")
            }
            Pending::Remove(name) => format!("delete memory/{name} from main?"),
            Pending::Conflict(name) => {
                ui::warning(&format!(
                    "memory/{name} skipped — deleted here but changed on main after this branch forked; edit main by hand"
                ));
                continue;
            }
        };
        if yes {
            chosen.push(item.clone());
            continue;
        }
        match ui::prompt::confirm(&question, true)? {
            Some(true) => chosen.push(item.clone()),
            Some(false) => {}
            None => {
                ui::error("distilling needs confirmation; pass `-y` to apply every change listed.");
                for item in &wanted {
                    println!(
                        "  {} memory/{}",
                        match item {
                            Pending::Carry(_) => "carry   ",
                            Pending::Remove(_) => "delete  ",
                            Pending::Conflict(_) => "conflict",
                        },
                        item.name()
                    );
                }
                return Ok(ExitCode::Interactive);
            }
        }
    }
    if chosen.is_empty() {
        println!("  nothing carried.");
        return Ok(ExitCode::Ok);
    }
    let commit = distill(primary, &plan, &chosen)?.expect("chosen is non-empty");
    ui::success(&format!(
        "distilled {} change{} from `{branch}` into main ({})",
        chosen.len(),
        if chosen.len() == 1 { "" } else { "s" },
        &commit[..9.min(commit.len())]
    ));
    ui::hint("publish it: `agit push -b main`");
    Ok(ExitCode::Ok)
}

fn sync(target: &Target) -> CmdResult {
    let Some((runtime, cwd)) = runtime_of(target) else {
        ui::error(&format!(
            "`{}` is not a session branch — nothing to sync with a runtime.",
            target.branch
        ));
        return Ok(ExitCode::Precondition);
    };
    let Some(mem) = crate::infra::runtime_memory::locate(&runtime, &cwd) else {
        println!("  {runtime} keeps no per-project memory directory — nothing to sync.");
        return Ok(ExitCode::Ok);
    };
    let policy = Policy::from_config(Scope::EverythingIfNoBaseline);
    let collected = collect_with(
        &target.primary,
        &target.branch,
        &target.slug,
        &mem.dir,
        policy,
    )?;
    let placed = materialize_with(
        &target.primary,
        &target.branch,
        &target.slug,
        &mem.dir,
        policy,
    )?;
    report_collect(&collected, &target.branch);
    report_materialize(&placed);
    remind_pending(&target.primary, &target.branch);
    Ok(ExitCode::Ok)
}

/// The one-line report on the settlement path.
pub fn report_collect(report: &Report, branch: &str) {
    if report.tracking_off {
        println!(
            "{}",
            ui::dim(&format!("  memory: not collected ({TRACK_KEY} = off)"))
        );
        return;
    }
    if let Some(commit) = &report.commit {
        println!(
            "  {} memory: {} change{} → `{branch}` ({})",
            ui::ok(ui::theme::symbols().check),
            report.collected,
            if report.collected == 1 { "" } else { "s" },
            &commit[..9.min(commit.len())]
        );
    }
    for refused in &report.refused {
        ui::warning(&format!(
            "memory/{refused}: suspected secret, not collected"
        ));
    }
    for warning in &report.warnings {
        ui::warning(warning);
    }
}

/// The one-line report on the start path.
pub fn report_materialize(report: &Report) {
    if report.placed > 0 {
        println!(
            "  {} memory: {} file{} → {}",
            ui::ok(ui::theme::symbols().check),
            report.placed,
            if report.placed == 1 { "" } else { "s" },
            ui::tilde(&report.dir.join(MIRROR_DIR))
        );
    }
    for name in &report.conflicts {
        ui::warning(&format!(
            "memory/{name}: the mirror copy has edits that are not on the branch yet; kept as is (`agit memory status`)"
        ));
    }
    for refused in &report.refused {
        ui::warning(&format!(
            "memory/{refused}: suspected secret, not collected"
        ));
    }
    for warning in &report.warnings {
        ui::warning(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::Meta;

    const SLUG: &str = "me/qa";
    const ON: Policy = Policy {
        track: true,
        scope: Scope::SinceBaseline,
    };
    const ALL: Policy = Policy {
        track: true,
        scope: Scope::EverythingIfNoBaseline,
    };

    fn fixture() -> (tempfile::TempDir, Repo, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().canonicalize().unwrap();
        let repo = Repo::init(&base.join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        std::fs::create_dir_all(repo.root().join(TREE_DIR)).unwrap();
        std::fs::write(
            repo.root().join(TREE_DIR).join("team.md"),
            "---\nname: team\ndescription: how the team works\n---\nuid, not user_id\n",
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("agit: init").unwrap();
        // Session line s1: grown from main's tree, with the meta marked as a session.
        repo.git(&["branch", "s1", "main"]).unwrap();
        let mut snap = Meta::new_session_line("claude-code".into(), "/w".into());
        snap.session = format!("{}{}", meta::ID_PREFIX, "b".repeat(meta::ID_HEX_LEN));
        let tree = super::super::plumbing::tree_apply_owned(
            &repo,
            "refs/heads/s1",
            vec![(
                meta::FILE.into(),
                Some(meta::to_text(&snap).unwrap().into_bytes()),
            )],
        )
        .unwrap();
        let head = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
        let c =
            super::super::plumbing::commit_tree(&repo, &tree, &[head.trim()], "session").unwrap();
        repo.git(&["update-ref", "refs/heads/s1", &c]).unwrap();
        let mem = base.join("claude-memory");
        std::fs::create_dir_all(&mem).unwrap();
        (d, repo, mem)
    }

    /// Read a memory file at a ref by bytes: `Repo::show` strips the trailing newline, and this
    /// wants it verbatim.
    fn file(repo: &Repo, git_ref: &str, name: &str) -> Option<String> {
        repo.git_bytes(&["show", &format!("{git_ref}:{TREE_DIR}/{name}")])
            .and_then(|b| String::from_utf8(b).ok())
    }

    fn mirror(mem: &Path) -> PathBuf {
        mirror_dir(mem, SLUG, "s1")
    }

    /// Branch memory lands in the runtime directory's per-branch mirror subdirectory and
    /// MEMORY.md gains one index section; the local top level does not move.
    #[test]
    fn materialize_places_branch_memory_under_the_mirror_dir_and_indexes_it() {
        let (_d, repo, mem) = fixture();
        std::fs::write(
            mem.join("MEMORY.md"),
            "# Memory Index\n\n- [mine](mine.md) — local\n",
        )
        .unwrap();
        std::fs::write(mem.join("mine.md"), "local fact\n").unwrap();

        let r = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.placed, 1);
        assert_eq!(r.dir, mem);
        assert_eq!(
            std::fs::read_to_string(mirror(&mem).join("team.md")).unwrap(),
            "---\nname: team\ndescription: how the team works\n---\nuid, not user_id\n"
        );
        let index = std::fs::read_to_string(mem.join("MEMORY.md")).unwrap();
        assert!(index.contains("- [mine](mine.md) — local"), "{index}");
        assert!(
            index.contains("- [team](agit/me/qa/s1/team.md) — how the team works"),
            "{index}"
        );
        assert!(index.contains("<!-- agit:memory:begin me/qa@s1 -->") && index.contains(INDEX_END));
        assert_eq!(
            std::fs::read_to_string(mem.join("mine.md")).unwrap(),
            "local fact\n"
        );

        // Again: nothing changes.
        let again = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(again.placed, 0);
        assert_eq!(
            std::fs::read_to_string(mem.join("MEMORY.md")).unwrap(),
            index
        );
    }

    /// A file the local top level already has with the same name and content (a round trip on
    /// one machine) is not placed a second time.
    #[test]
    fn a_file_already_present_locally_is_not_mirrored() {
        let (_d, repo, mem) = fixture();
        std::fs::write(
            mem.join("team.md"),
            "---\nname: team\ndescription: how the team works\n---\nuid, not user_id\n",
        )
        .unwrap();
        let r = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.placed, 0);
        assert_eq!(r.already_local, 1);
        assert!(!mem.join("agit").exists());
        assert!(
            !mem.join("MEMORY.md").exists(),
            "no index section without mirrored files"
        );
    }

    /// Personal memory that was there before the start and untouched this session does not enter
    /// the branch, even when the branch lacks it; only what was newly written or changed enters.
    #[test]
    fn collect_takes_only_what_changed_since_the_baseline() {
        let (_d, repo, mem) = fixture();
        std::fs::write(mem.join("private.md"), "was here before the session\n").unwrap();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();

        std::fs::write(mem.join("new.md"), "---\nname: new\n---\na new fact\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 1);
        assert!(
            file(&repo, "refs/heads/s1", "private.md").is_none(),
            "untouched local memory stays local"
        );
        assert_eq!(
            file(&repo, "refs/heads/s1", "new.md").unwrap(),
            "---\nname: new\n---\na new fact\n"
        );
        assert!(
            file(&repo, "refs/heads/main", "new.md").is_none(),
            "main never moves by itself"
        );
        assert_eq!(
            meta::read_at_ref(&repo, "refs/heads/s1").unwrap().kind,
            Kind::File
        );

        // No change, no commit.
        let idle = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(idle.collected, 0);
        assert!(idle.commit.is_none());

        // The one that was untouched is edited afterwards: touched this session, so it enters.
        std::fs::write(mem.join("private.md"), "edited during the session\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 1);
        assert_eq!(
            file(&repo, "refs/heads/s1", "private.md").unwrap(),
            "edited during the session\n"
        );

        // The user deletes the top-level file: the branch deletes it too.
        std::fs::remove_file(mem.join("new.md")).unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 1);
        assert!(file(&repo, "refs/heads/s1", "new.md").is_none());
    }

    /// With no baseline, settlement only records one and collects nothing; an explicit sync
    /// takes in everything currently at the top level.
    #[test]
    fn without_a_baseline_commit_collects_nothing_and_sync_collects_everything() {
        let (_d, repo, mem) = fixture();
        std::fs::write(mem.join("mine.md"), "local\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 0);
        assert!(file(&repo, "refs/heads/s1", "mine.md").is_none());

        let (_d, repo, mem) = fixture();
        std::fs::write(mem.join("mine.md"), "local\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ALL).unwrap();
        assert_eq!(r.collected, 1);
        assert_eq!(file(&repo, "refs/heads/s1", "mine.md").unwrap(), "local\n");
    }

    /// What the agent edited in the mirror enters the branch; what it deleted is deleted from the
    /// branch and does not grow back at the next materialization.
    #[test]
    fn mirror_edits_and_deletions_flow_back_to_the_branch() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(mirror(&mem).join("team.md"), "edited by the agent\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 1);
        assert_eq!(
            file(&repo, "refs/heads/s1", "team.md").unwrap(),
            "edited by the agent\n"
        );

        std::fs::remove_file(mirror(&mem).join("team.md")).unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 1);
        assert!(file(&repo, "refs/heads/s1", "team.md").is_none());
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert!(
            !mirror(&mem).join("team.md").exists(),
            "a deleted file does not grow back"
        );
    }

    /// With uncollected edits left in the mirror by a settlement that failed, materializing again
    /// collects them into the branch first instead of overwriting them with branch bytes.
    #[test]
    fn materialize_collects_uncollected_mirror_edits_before_placing() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(mirror(&mem).join("team.md"), "the only copy of this edit\n").unwrap();

        let r = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert!(r.conflicts.is_empty());
        assert_eq!(
            file(&repo, "refs/heads/s1", "team.md").unwrap(),
            "the only copy of this edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(mirror(&mem).join("team.md")).unwrap(),
            "the only copy of this edit\n"
        );
    }

    /// A mirror edit refused by the secret scan is not overwritten by materialization but
    /// reported as a conflict; once the edit is cleaned up it enters the branch as usual.
    #[test]
    fn a_refused_mirror_edit_is_kept_and_reported_as_a_conflict() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        let leak = format!("token: agit_at_{}\n", "0123456789abcdef".repeat(4));
        std::fs::write(mirror(&mem).join("team.md"), &leak).unwrap();

        let r = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.conflicts, vec!["team.md".to_string()]);
        assert!(!r.refused.is_empty());
        assert_eq!(
            std::fs::read_to_string(mirror(&mem).join("team.md")).unwrap(),
            leak
        );
        assert!(
            file(&repo, "refs/heads/s1", "team.md")
                .unwrap()
                .contains("uid, not user_id")
        );

        std::fs::write(mirror(&mem).join("team.md"), "cleaned up\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 1);
        assert_eq!(
            file(&repo, "refs/heads/s1", "team.md").unwrap(),
            "cleaned up\n"
        );
    }

    /// When the commit has landed but the baseline cannot be written, the report carries both
    /// the commit and a warning, and the caller can still record a receipt.
    #[test]
    fn a_landed_commit_survives_a_failed_baseline_write() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(mem.join("new.md"), "a new fact\n").unwrap();
        let checkout = super::super::worktree::checkout(&repo, "s1").unwrap();
        let path = baseline_path(&checkout).unwrap();
        std::fs::remove_file(&path).unwrap();
        // A directory occupies the baseline's name: writing the file must fail.
        std::fs::create_dir_all(&path).unwrap();
        // With a directory under that name the baseline cannot be read; put the baseline content
        // back somewhere readable so this decision can be made.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&Baseline::of(
                &mem,
                &BTreeMap::new(),
                &md_files(&mirror(&mem)).unwrap(),
            ))
            .unwrap(),
        )
        .unwrap();
        // Swap it back to a directory once the baseline has been read — a read-only parent
        // cannot do this (the git directory must stay writable), so `store_baseline` is made to
        // hit a directory of the same name.
        let r = {
            let base = read_baseline(&checkout, &mem).unwrap();
            assert!(base.is_some());
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir_all(&path).unwrap();
            let top = md_files(&mem).unwrap();
            let mirror_files = md_files(&mirror(&mem)).unwrap();
            let in_branch = branch_files(&checkout, "refs/heads/s1").unwrap();
            let plan = plan_collect(
                base.as_ref(),
                &top,
                &mirror_files,
                &in_branch,
                Scope::SinceBaseline,
            );
            assert_eq!(plan.top_changed.keys().collect::<Vec<_>>(), vec!["new.md"]);
            let commit = commit_memory(
                &repo,
                "s1",
                plan.top_changed
                    .into_iter()
                    .map(|(n, b)| (n, Some(b)))
                    .collect(),
                "memory",
            )
            .unwrap();
            let mut report = Report {
                commit: Some(commit),
                ..Report::default()
            };
            if let Err(error) = store_baseline(&checkout, &Baseline::of(&mem, &top, &mirror_files))
            {
                report
                    .warnings
                    .push(format!("memory baseline was not updated: {error:#}"));
            }
            report
        };
        assert!(r.commit.is_some());
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/s1"]).unwrap().trim(),
            r.commit.unwrap()
        );
    }

    /// An unreadable directory entry is an error, not "empty": memory that is still there must
    /// not be taken for deleted because of it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_aborts_collection_instead_of_deleting() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, repo, mem) = fixture();
        std::fs::write(mem.join("keep.md"), "still here\n").unwrap();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        collect_with(&repo, "s1", SLUG, &mem, ALL).unwrap();
        assert!(
            file(&repo, "refs/heads/s1", "keep.md").is_none(),
            "untouched at sync"
        );
        std::fs::write(mem.join("keep.md"), "edited\n").unwrap();
        collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(file(&repo, "refs/heads/s1", "keep.md").unwrap(), "edited\n");

        let locked = mem.join("keep.md");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let outcome = collect_with(&repo, "s1", SLUG, &mem, ON);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        if nix_is_root() {
            return;
        }
        assert!(outcome.is_err(), "an unreadable file is an error");
        assert_eq!(
            file(&repo, "refs/heads/s1", "keep.md").unwrap(),
            "edited\n",
            "nothing was deleted"
        );
    }

    #[cfg(unix)]
    fn nix_is_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false)
    }

    /// `memory.track = off`: no collection entry point collects, and the report says why.
    #[test]
    fn tracking_off_collects_nothing_on_every_path() {
        let (_d, repo, mem) = fixture();
        let off = Policy {
            track: false,
            scope: Scope::EverythingIfNoBaseline,
        };
        std::fs::write(mem.join("mine.md"), "local\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, off).unwrap();
        assert!(r.tracking_off);
        assert!(r.commit.is_none());
        assert!(file(&repo, "refs/heads/s1", "mine.md").is_none());
    }

    /// A file with a suspected secret does not enter the branch and is named in the report.
    #[test]
    fn a_file_with_a_secret_is_refused() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(
            mem.join("leak.md"),
            format!("token: agit_at_{}\n", "0123456789abcdef".repeat(4)),
        )
        .unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 0);
        assert!(r.commit.is_none());
        assert_eq!(r.refused.len(), 1, "{:?}", r.refused);
        assert!(r.refused[0].starts_with("leak.md"));
    }

    /// Distill: what is new on the branch is carried into main; what was inherited from main,
    /// deleted here and untouched on main is carried over as a deletion; what main added after
    /// the fork does not count; what main changed after the fork and the branch deleted is a
    /// conflict, reported and left alone.
    #[test]
    fn distill_carries_additions_and_inherited_deletions_and_flags_conflicts() {
        let (_d, repo, mem) = fixture();
        commit_memory(
            &repo,
            "main",
            [("shared.md".to_string(), Some(b"v1\n".to_vec()))]
                .into_iter()
                .collect(),
            "main adds shared.md",
        )
        .unwrap();
        repo.git(&["update-ref", "refs/heads/s1", "refs/heads/main"])
            .unwrap();
        // Give s1 a session meta again (update-ref pointed it at main's file-line commit).
        let mut snap = Meta::new_session_line("claude-code".into(), "/w".into());
        snap.session = format!("{}{}", meta::ID_PREFIX, "b".repeat(meta::ID_HEX_LEN));
        let tree = super::super::plumbing::tree_apply_owned(
            &repo,
            "refs/heads/s1",
            vec![(
                meta::FILE.into(),
                Some(meta::to_text(&snap).unwrap().into_bytes()),
            )],
        )
        .unwrap();
        let head = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
        let c =
            super::super::plumbing::commit_tree(&repo, &tree, &[head.trim()], "session").unwrap();
        repo.git(&["update-ref", "refs/heads/s1", &c]).unwrap();

        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(mem.join("new.md"), "a new fact\n").unwrap();
        std::fs::remove_file(mirror(&mem).join("team.md")).unwrap();
        std::fs::remove_file(mirror(&mem).join("shared.md")).unwrap();
        collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        // main after the fork: one file added (the branch is merely behind) and shared.md
        // changed (conflicting with the branch's deletion).
        commit_memory(
            &repo,
            "main",
            [
                (
                    "later.md".to_string(),
                    Some(b"added on main later\n".to_vec()),
                ),
                ("shared.md".to_string(), Some(b"v2 on main\n".to_vec())),
            ]
            .into_iter()
            .collect(),
            "main moves on",
        )
        .unwrap();

        let pending = distill_pending(&repo, "s1").unwrap();
        assert_eq!(
            pending,
            vec![
                Pending::Carry("new.md".into()),
                Pending::Conflict("shared.md".into()),
                Pending::Remove("team.md".into()),
            ]
        );
        let plan = distill_plan(&repo, "s1").unwrap();
        assert!(
            distill(&repo, &plan, &pending).is_err(),
            "a conflict is never applied"
        );

        let safe: Vec<Pending> = pending
            .iter()
            .filter(|p| !matches!(p, Pending::Conflict(_)))
            .cloned()
            .collect();
        let commit = distill(&repo, &plan, &safe).unwrap().unwrap();
        assert_eq!(
            file(&repo, "refs/heads/main", "new.md").unwrap(),
            "a new fact\n"
        );
        assert!(file(&repo, "refs/heads/main", "team.md").is_none());
        assert_eq!(
            file(&repo, "refs/heads/main", "shared.md").unwrap(),
            "v2 on main\n"
        );
        assert!(file(&repo, "refs/heads/main", "later.md").is_some());
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/main"]).unwrap().trim(),
            commit
        );
        assert!(
            meta::read_at_ref(&repo, "refs/heads/main")
                .unwrap()
                .is_file_line()
        );
        assert_eq!(
            distill_pending(&repo, "s1").unwrap(),
            vec![Pending::Conflict("shared.md".into())]
        );
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert_eq!(
            std::fs::read_to_string(repo.root().join(TREE_DIR).join("new.md")).unwrap(),
            "a new fact\n",
            "the main checkout is refreshed"
        );
    }

    /// A name with spaces is no different: refused names are tracked structurally, so
    /// materialization does not take one for unchanged and overwrite it.
    #[test]
    fn a_refused_name_with_spaces_is_still_protected() {
        let (_d, repo, mem) = fixture();
        commit_memory(
            &repo,
            "s1",
            [("team note.md".to_string(), Some(b"shared\n".to_vec()))]
                .into_iter()
                .collect(),
            "spaced name",
        )
        .unwrap();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        let leak = format!("token: agit_at_{}\n", "0123456789abcdef".repeat(4));
        std::fs::write(mirror(&mem).join("team note.md"), &leak).unwrap();
        let r = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.conflicts, vec!["team note.md".to_string()]);
        assert_eq!(
            std::fs::read_to_string(mirror(&mem).join("team note.md")).unwrap(),
            leak
        );
    }

    /// When main advances and changes a file of the same name while the user confirms, the
    /// decision is redone before landing and the deletion does not wipe out the new content.
    #[test]
    fn distill_revalidates_against_the_current_main_before_landing() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::remove_file(mirror(&mem).join("team.md")).unwrap();
        collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        let plan = distill_plan(&repo, "s1").unwrap();
        assert_eq!(plan.items, vec![Pending::Remove("team.md".into())]);

        // The user is still confirming; main has already changed team.md.
        commit_memory(
            &repo,
            "main",
            [("team.md".to_string(), Some(b"revised on main\n".to_vec()))]
                .into_iter()
                .collect(),
            "main revises team.md",
        )
        .unwrap();
        let error = distill(&repo, &plan, &plan.items).unwrap_err().to_string();
        assert!(error.contains("moved"), "{error}");
        assert_eq!(
            file(&repo, "refs/heads/main", "team.md").unwrap(),
            "revised on main\n"
        );
    }

    /// When the session branch settles the same file again while the user confirms, the plan is
    /// void, and unconfirmed, unscanned content is never carried into main.
    #[test]
    fn distill_refuses_when_the_session_branch_moved_after_planning() {
        let (_d, repo, mem) = fixture();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(mem.join("new.md"), "confirmed version\n").unwrap();
        collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        let plan = distill_plan(&repo, "s1").unwrap();
        assert_eq!(plan.items, vec![Pending::Carry("new.md".into())]);

        // The user is still confirming; the same branch settled another version.
        std::fs::write(mem.join("new.md"), "unreviewed version\n").unwrap();
        collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        let error = distill(&repo, &plan, &plan.items).unwrap_err().to_string();
        assert!(error.contains("moved"), "{error}");
        assert!(file(&repo, "refs/heads/main", "new.md").is_none());

        // Planning again carries the new version.
        let plan = distill_plan(&repo, "s1").unwrap();
        distill(&repo, &plan, &plan.items).unwrap();
        assert_eq!(
            file(&repo, "refs/heads/main", "new.md").unwrap(),
            "unreviewed version\n"
        );
    }

    /// Only top-level Markdown is managed: subdirectories and other extensions in the branch are
    /// neither materialized nor collected.
    #[test]
    fn only_top_level_markdown_is_managed() {
        let (_d, repo, mem) = fixture();
        commit_memory(
            &repo,
            "s1",
            [
                ("nested/deep.md".to_string(), Some(b"nested\n".to_vec())),
                ("notes.txt".to_string(), Some(b"txt\n".to_vec())),
            ]
            .into_iter()
            .collect(),
            "unmanaged shapes",
        )
        .unwrap();
        let r = materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.placed, 1, "only team.md");
        assert!(!mirror(&mem).join("nested").exists());
        assert!(!mirror(&mem).join("notes.txt").exists());
        std::fs::write(mem.join("scratch.txt"), "not markdown\n").unwrap();
        std::fs::create_dir_all(mem.join("sub")).unwrap();
        std::fs::write(mem.join("sub/inner.md"), "nested locally\n").unwrap();
        let r = collect_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        assert_eq!(r.collected, 0);
    }

    /// The plan and status say the same thing: an untouched file is not pending collection, a
    /// deleted one is.
    #[test]
    fn the_plan_reports_untouched_changed_and_deleted_files() {
        let (_d, repo, mem) = fixture();
        std::fs::write(mem.join("old.md"), "before\n").unwrap();
        materialize_with(&repo, "s1", SLUG, &mem, ON).unwrap();
        std::fs::write(mem.join("new.md"), "after\n").unwrap();
        std::fs::remove_file(mirror(&mem).join("team.md")).unwrap();
        let checkout = super::super::worktree::checkout(&repo, "s1").unwrap();
        let baseline = read_baseline(&checkout, &mem).unwrap();
        let plan = plan_collect(
            baseline.as_ref(),
            &md_files(&mem).unwrap(),
            &md_files(&mirror(&mem)).unwrap(),
            &branch_files(&checkout, "refs/heads/s1").unwrap(),
            Scope::SinceBaseline,
        );
        assert_eq!(plan.untouched.iter().collect::<Vec<_>>(), vec!["old.md"]);
        assert_eq!(plan.top_changed.keys().collect::<Vec<_>>(), vec!["new.md"]);
        assert_eq!(plan.deleted.iter().collect::<Vec<_>>(), vec!["team.md"]);
        assert!(!plan.no_baseline);
    }

    /// Replacing the index section touches only that section.
    #[test]
    fn the_index_section_is_replaced_in_place() {
        let before = "# Memory Index\n\n- [mine](mine.md)\n\n<!-- agit:memory:begin me/qa@s1 -->\nold\n<!-- agit:memory:end -->\n\n- [after](after.md)\n";
        let next = upsert_index(
            before,
            "me/qa@s1",
            &["- [team](agit/me/qa/s1/team.md)".into()],
        );
        assert!(next.contains("- [mine](mine.md)"));
        assert!(next.contains("- [after](after.md)"));
        assert!(!next.contains("\nold\n"));
        assert!(next.contains("- [team](agit/me/qa/s1/team.md)"));
        let removed = upsert_index(&next, "me/qa@s1", &[]);
        assert!(!removed.contains(INDEX_BEGIN));
        assert!(removed.contains("- [after](after.md)"));
    }
}
