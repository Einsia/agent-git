//! Offline search inspects immutable saved history, never a runtime or an inferred workspace.

use super::Args;
use crate::adapter::{self, Event, EventKind};
use crate::domain::{
    meta,
    query::{EventScope, Query},
    repo::Repo,
    storage, transcript,
};
use crate::infra::config;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_DIRENTS: usize = 512;
const MAX_REFS: usize = 128;
const MAX_COMMITS: usize = 256;
const MAX_OBJECT_BYTES: usize = 2 * 1024 * 1024;
const MAX_READ_BYTES: usize = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 20_000;
const MAX_HITS: usize = 1_024;
const MAX_REF_BYTES: usize = 128 * 1024;
const MAX_COMMIT_BYTES: usize = 64 * 1024;
const MAX_META_BYTES: usize = 64 * 1024;

pub(super) fn validate(args: &Args, queries: &[String]) -> Result<Vec<Query>, String> {
    if args.kind != "sessions" || args.counts {
        return Err("--local supports saved sessions only; --type agents/prs/people and --counts require the Hub".into());
    }
    if args.author.is_some() || args.since.is_some() || args.before.is_some() {
        return Err("--local does not support --author, --since or --before".into());
    }
    queries.iter().map(|text| {
        let q = Query::parse(text);
        if q.is_empty() || !text.matches('"').count().is_multiple_of(2) || !q.unknown.is_empty()
            || q.category.is_some() || q.state.is_some() || q.visibility.is_some() || q.fork.is_some()
        {
            return Err("--local requires balanced phrases and supported session filters: owner, repo/agent, runtime, in, tool, path and turns; Hub-only or unknown qualifiers are not supported".into());
        }
        Ok(q)
    }).collect()
}

struct Budget {
    deadline: crate::infra::local_git::Deadline,
    dirents: usize,
    refs: usize,
    commits: usize,
    bytes: usize,
    work: storage::LocalReadBudget,
    hits: usize,
    reasons: BTreeSet<&'static str>,
}

impl Default for Budget {
    fn default() -> Self {
        let deadline = crate::infra::local_git::Deadline::new();
        Self {
            deadline,
            dirents: 0,
            refs: 0,
            commits: 0,
            bytes: 0,
            work: storage::LocalReadBudget::with_deadline(MAX_RECORDS, deadline),
            hits: 0,
            reasons: BTreeSet::new(),
        }
    }
}

impl Budget {
    fn incomplete(&mut self, reason: &'static str) {
        self.reasons.insert(reason);
    }

    fn reserve(&mut self, maximum: usize) -> Option<usize> {
        if self.deadline.expired() {
            self.incomplete("time_budget");
            return None;
        }
        let available = MAX_READ_BYTES.saturating_sub(self.bytes);
        if available == 0 {
            self.incomplete("read_budget");
            return None;
        }
        let amount = maximum.min(available);
        // Reserve before a read: failures and repeated immutable passes also consume work.
        self.bytes += amount;
        Some(amount)
    }

    fn stopped(&self) -> bool {
        self.deadline.expired()
            || self.commits >= MAX_COMMITS
            || self.work.remaining() == 0
            || self.hits >= MAX_HITS
            || self.bytes >= MAX_READ_BYTES
            || self.refs >= MAX_REFS
    }
}

#[derive(Clone)]
struct SavedRef {
    name: String,
    oid: String,
    kind: String,
}

struct Version {
    oid: String,
    git_ref: String,
    saved_at: i64,
    snapshot: meta::Meta,
}

#[derive(Clone)]
struct Hit {
    value: Value,
    saved_at: i64,
    turns: usize,
    score: usize,
    key: (String, String, usize),
}

pub(super) fn execute(args: &Args, texts: &[String], queries: &[Query]) -> Result<Vec<Value>> {
    let mut budget = Budget::default();
    let mut hits = vec![Vec::<Hit>::new(); queries.len()];
    let repos = repositories(queries, &mut budget)?;
    for (slug, path) in repos {
        if budget.stopped() {
            budget.incomplete("scan_budget");
            break;
        }
        let repo = match admit_repository(&path, &mut budget) {
            Ok(repo) => repo,
            Err(_) => {
                budget.incomplete("unavailable_repository");
                continue;
            }
        };
        let initial = match refs(&repo, &mut budget) {
            Ok(refs) => refs,
            Err(_) => {
                budget.incomplete("unavailable_refs");
                continue;
            }
        };
        let Some(mut verification) = reserve_verification(&initial, &mut budget) else {
            budget.incomplete("verification_budget");
            continue;
        };
        let mut versions = versions(&repo, &initial, &mut budget);
        versions.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then_with(|| a.oid.cmp(&b.oid)));
        let mut dedup: Vec<BTreeSet<(String, String)>> = vec![BTreeSet::new(); queries.len()];
        let mut repo_hits = vec![Vec::<Hit>::new(); queries.len()];
        for version in versions {
            if budget.work.remaining() == 0 || budget.hits >= MAX_HITS {
                budget.incomplete("scan_budget");
                break;
            }
            let eligible: Vec<usize> = queries
                .iter()
                .enumerate()
                .filter(|(_, q)| {
                    repository_matches(q, &slug)
                        && q.runtime.as_deref().is_none_or(|runtime| {
                            runtime.eq_ignore_ascii_case(&version.snapshot.runtime)
                        })
                })
                .map(|(i, _)| i)
                .collect();
            if eligible.is_empty() {
                continue;
            }
            // The sequence and its referenced bodies each spend the materialization ceiling.
            let Some(reserved) = budget.reserve(MAX_OBJECT_BYTES * 2) else {
                break;
            };
            let limit = reserved / 2;
            if limit == 0 {
                budget.incomplete("read_budget");
                break;
            }
            let read_before = budget.work.read_bytes();
            let log = match storage::materialize_log_local(
                repo.root(),
                &version.oid,
                version.snapshot.layout,
                limit,
                budget.work.remaining(),
                &mut budget.work,
            ) {
                Ok(log) => log,
                Err(_) => {
                    budget.incomplete("unavailable_saved_log");
                    continue;
                }
            };
            let read_bytes = budget.work.read_bytes() - read_before;
            ensure!(read_bytes <= reserved, "saved read exceeds its reservation");
            budget.bytes -= reserved - read_bytes;
            let log_hash = hex::encode(Sha256::digest(log.as_bytes()));
            let (projected, turns) = match project(&log, &mut budget.work) {
                Ok(events) => events,
                Err(_) => {
                    budget.incomplete("unavailable_projection");
                    continue;
                }
            };
            let turns_incomplete = projected.iter().any(|event| event.turns_incomplete);
            if projected.iter().any(|event| event.incomplete) {
                budget.incomplete("unrepresented_event_content");
            }
            let eligible: Vec<_> = eligible
                .into_iter()
                .filter(|i| {
                    queries[*i]
                        .turns
                        .is_none_or(|filter| !turns_incomplete && filter.matches(turns))
                })
                .filter(|i| dedup[*i].insert((version.snapshot.session.clone(), log_hash.clone())))
                .collect();
            if eligible.is_empty() {
                continue;
            }
            let mut representatives: Vec<Option<Hit>> = vec![None; queries.len()];
            for event in projected {
                if event.incomplete {
                    budget.incomplete("unrepresented_event_content");
                }
                let Some(scope) = event.scope else {
                    continue;
                };
                for &i in &eligible {
                    if budget.hits == MAX_HITS {
                        budget.incomplete("hit_budget");
                        break;
                    }
                    let q = &queries[i];
                    if !q.allows(scope)
                        || q.tool.as_deref().is_some_and(|tool| {
                            !event.event.tool.as_deref().is_some_and(|name| {
                                name.to_lowercase().contains(&tool.to_lowercase())
                            })
                        })
                        || q.path.as_deref().is_some_and(|path| {
                            !event
                                .event
                                .paths
                                .iter()
                                .any(|p| p.to_lowercase().contains(&path.to_lowercase()))
                        })
                        || !q.matches_text(&event.searchable)
                    {
                        continue;
                    }
                    let line = event.event.line.context("saved event has no coordinate")?;
                    let excerpt = excerpt(&event.searchable, q);
                    let value = json!({
                        "agent": slug, "session_id": version.snapshot.session,
                        "commit": version.oid, "ref": version.git_ref,
                        "excerpt": excerpt, "scope": scope.as_str(), "secondhand": scope.is_secondhand(),
                        "runtime": event.runtime, "tool": event.event.tool,
                        "paths": event.event.paths, "turns": turns, "turns_incomplete": turns_incomplete, "line": line + 1,
                        "timestamp": event.event.timestamp, "saved_at": version.saved_at,
                        "outcome": "unknown", "confidence": "low",
                        "outcome_reason": "Offline saved-history search does not infer execution outcomes.",
                        "url": Value::Null, "group_size": 1, "grouped": [], "other_hits": 0,
                    });
                    let mut hit = Hit {
                        value,
                        saved_at: version.saved_at,
                        turns,
                        score: q.terms.len() + usize::from(!scope.is_secondhand()),
                        key: (slug.clone(), version.oid.clone(), line),
                    };
                    match &mut representatives[i] {
                        Some(previous) => {
                            let other_hits = previous.value["other_hits"].as_u64().unwrap_or(0) + 1;
                            if hit.score > previous.score {
                                *previous = hit;
                            }
                            previous.value["other_hits"] = json!(other_hits);
                        }
                        slot @ None => {
                            hit.value["other_hits"] = json!(0);
                            *slot = Some(hit);
                        }
                    }
                    budget.hits += 1;
                }
            }
            for (out, found) in repo_hits.iter_mut().zip(representatives) {
                if let Some(found) = found {
                    out.push(found);
                }
            }
        }
        // A moving ref set cannot label old objects as the current local corpus snapshot.
        let verified = verify_refs(&repo, &initial, &mut verification);
        budget.bytes -= MAX_READ_BYTES - verification.bytes;
        budget.refs -= MAX_REFS - verification.refs;
        match verified {
            Ok(()) => {
                for (out, found) in hits.iter_mut().zip(repo_hits) {
                    out.extend(found);
                }
            }
            _ => budget.incomplete("unverified_snapshot"),
        }
    }
    if budget.deadline.expired() {
        budget.incomplete("time_budget");
    }
    let incomplete = !budget.reasons.is_empty();
    Ok(hits.into_iter().enumerate().map(|(i, mut rows)| {
        rows.sort_by(|a, b| match args.sort.as_deref().unwrap_or("best") {
            "recent" => b.saved_at.cmp(&a.saved_at),
            "turns" => b.turns.cmp(&a.turns).then_with(|| b.saved_at.cmp(&a.saved_at)),
            _ => b.score.cmp(&a.score).then_with(|| b.saved_at.cmp(&a.saved_at)),
        }.then_with(|| a.key.cmp(&b.key)));
        let total = rows.len();
        let start = args.page.saturating_sub(1).saturating_mul(args.limit);
        let page: Vec<_> = rows.into_iter().skip(start).take(args.limit).map(|h| h.value).collect();
        json!({
            "query": texts[i], "type": "sessions", "corpus": "local_saved_history",
            "total": total, "total_unit": "saved_versions", "page": args.page, "per": args.limit,
            "has_more": start.saturating_add(args.limit) < total,
            "incomplete": incomplete, "incomplete_reasons": budget.reasons,
            "unknown": [], "terms": queries[i].terms, "applied_filters": {}, "hits": page,
            "coverage": {"repositories_and_refs": "local refs/heads and refs/remotes with raw reachable commit parents", "native_transcripts": false, "hub_permissions": "not revalidated offline"},
        })
    }).collect())
}

fn reserve_verification(initial: &[SavedRef], budget: &mut Budget) -> Option<Budget> {
    let bytes = MAX_REF_BYTES + MAX_META_BYTES * 3;
    let refs = initial.len().checked_add(1)?;
    if bytes > MAX_READ_BYTES.saturating_sub(budget.bytes)
        || refs > MAX_REFS.saturating_sub(budget.refs)
    {
        return None;
    }
    // Reserve from the command budget before scanning so partial hits can still be verified.
    budget.bytes += bytes;
    budget.refs += refs;
    Some(Budget {
        deadline: budget.deadline,
        work: storage::LocalReadBudget::with_deadline(MAX_RECORDS, budget.deadline),
        bytes: MAX_READ_BYTES - bytes,
        refs: MAX_REFS - refs,
        ..Default::default()
    })
}

fn repository_matches(query: &Query, slug: &str) -> bool {
    let Some((owner, name)) = slug.split_once('/') else {
        return false;
    };
    query
        .owner
        .as_deref()
        .is_none_or(|want| want.eq_ignore_ascii_case(owner))
        && query.agent.as_deref().is_none_or(|want| {
            if want.contains('/') {
                want.eq_ignore_ascii_case(slug)
            } else {
                want.eq_ignore_ascii_case(name)
            }
        })
}

fn directories(path: &Path, budget: &mut Budget, missing_ok: bool) -> Result<Vec<PathBuf>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if missing_ok && e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Ok(m) => ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "local corpus path is not an ordinary directory"
        ),
        Err(_) => anyhow::bail!("local corpus directory is unavailable"),
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(path)? {
        if budget.dirents == MAX_DIRENTS {
            budget.incomplete("directory_budget");
            break;
        }
        budget.dirents += 1;
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                budget.incomplete("unavailable_directory");
                continue;
            }
        };
        let kind = match entry.file_type() {
            Ok(k) => k,
            Err(_) => {
                budget.incomplete("unavailable_directory");
                continue;
            }
        };
        if kind.is_symlink() {
            budget.incomplete("symlinked_directory");
            continue;
        }
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name.to_str().is_none_or(|name| {
            name.trim() != name || crate::domain::repo::valid_name(name).is_err()
        }) {
            budget.incomplete("invalid_repository_name");
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn repositories(queries: &[Query], budget: &mut Budget) -> Result<Vec<(String, PathBuf)>> {
    let home = config::agit_home()?;
    match std::fs::symlink_metadata(&home) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Ok(m) => ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "local AgentGit home is not an ordinary directory"
        ),
        Err(_) => anyhow::bail!("local AgentGit home is unavailable"),
    }
    let mut repos = Vec::new();
    for owner in directories(&config::repos_dir()?, budget, true)? {
        let owner_name = owner
            .file_name()
            .and_then(|x| x.to_str())
            .context("invalid local owner")?;
        if queries.iter().all(|q| {
            q.owner
                .as_deref()
                .is_some_and(|want| !want.eq_ignore_ascii_case(owner_name))
                || q.agent
                    .as_deref()
                    .and_then(|a| a.split_once('/'))
                    .is_some_and(|(want, _)| !want.eq_ignore_ascii_case(owner_name))
        }) {
            continue;
        }
        let children = match directories(&owner, budget, false) {
            Ok(children) => children,
            Err(_) => {
                budget.incomplete("unavailable_directory");
                continue;
            }
        };
        for path in children {
            let name = path
                .file_name()
                .and_then(|x| x.to_str())
                .context("invalid local repo")?;
            let slug = format!("{owner_name}/{name}");
            if queries.iter().any(|q| repository_matches(q, &slug)) {
                repos.push((slug, path));
            }
        }
    }
    Ok(repos)
}

fn admit_repository(path: &Path, budget: &mut Budget) -> Result<Repo> {
    let root_metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "local repository is not an ordinary directory"
    );
    let carrier = std::fs::symlink_metadata(path.join(".git"))?;
    ensure!(
        !carrier.file_type().is_symlink() && (carrier.is_dir() || carrier.is_file()),
        "local repository Git carrier is unavailable"
    );
    ensure!(
        !carrier.is_file() || carrier.len() <= MAX_META_BYTES as u64,
        "local Git carrier exceeds the metadata budget"
    );
    let root = path.canonicalize()?;
    let repo = Repo::at(&root).local_objects_only();
    let cap = budget
        .reserve(MAX_META_BYTES)
        .context("local repository admission exceeds the read budget")?;
    let output = repo.inspection_output_in_repository(
        &["rev-parse", "--show-toplevel"],
        cap,
        budget.deadline,
    )?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "local repository root is unavailable"
    );
    let reported = std::str::from_utf8(
        output
            .stdout
            .strip_suffix(b"\n")
            .context("local repository root is incomplete")?,
    )?;
    ensure!(
        !reported.contains(['\r', '\n', '\0']) && Path::new(reported).canonicalize()? == root,
        "local Git root differs from the selected repository"
    );
    budget.bytes -= cap.saturating_sub(output.stdout.len());
    if carrier.is_file() {
        let cap = budget
            .reserve(MAX_META_BYTES)
            .context("local worktree admission exceeds the read budget")?;
        let output = repo.inspection_output_in_repository(
            &["rev-parse", "--absolute-git-dir"],
            cap,
            budget.deadline,
        )?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "local worktree Git directory is unavailable"
        );
        let text = std::str::from_utf8(
            output
                .stdout
                .strip_suffix(b"\n")
                .context("local worktree Git directory is incomplete")?,
        )?;
        ensure!(
            !text.contains(['\r', '\n', '\0']),
            "local worktree Git directory is invalid"
        );
        let gitdir = Path::new(text).canonicalize()?;
        budget.bytes -= cap.saturating_sub(output.stdout.len());
        let cap = budget
            .reserve(MAX_META_BYTES)
            .context("local worktree backlink exceeds the read budget")?;
        let bytes = crate::adapter::native_snapshot::read_file_bytes(
            &gitdir.join("gitdir"),
            crate::adapter::native_snapshot::Limits {
                bytes: cap,
                working_bytes: cap,
                ..Default::default()
            },
        )?;
        let backlink = std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']);
        ensure!(
            !backlink.is_empty() && !backlink.contains(['\r', '\n', '\0']),
            "local worktree backlink is invalid"
        );
        let backlink = Path::new(backlink);
        let backlink = if backlink.is_absolute() {
            backlink.to_owned()
        } else {
            gitdir.join(backlink)
        };
        ensure!(
            backlink.canonicalize()? == root.join(".git").canonicalize()?,
            "local worktree does not own its Git carrier"
        );
        budget.bytes -= cap.saturating_sub(bytes.len());
    }
    Ok(repo)
}

fn oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_refs_capped(
    repo: &Repo,
    maximum: usize,
    byte_limit: usize,
    deadline: crate::infra::local_git::Deadline,
) -> Result<Vec<SavedRef>> {
    let count = format!("--count={maximum}");
    let output = repo.inspection_output_in_repository(
        &[
            "for-each-ref",
            &count,
            "--format=%(refname)%00%(objectname)%00%(objecttype)",
            "refs/heads/",
            "refs/remotes/",
        ],
        byte_limit,
        deadline,
    )?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "local refs are unreadable"
    );
    let text = std::str::from_utf8(&output.stdout)?;
    ensure!(
        text.is_empty() || text.ends_with('\n'),
        "local refs have incomplete framing"
    );
    let mut refs = Vec::new();
    for row in text.lines() {
        let fields: Vec<_> = row.split('\0').collect();
        ensure!(
            fields.len() == 3
                && oid(fields[1])
                && fields[0].len() <= 1024
                && (fields[0].starts_with("refs/heads/") || fields[0].starts_with("refs/remotes/"))
                && !fields[0].chars().any(char::is_control),
            "local ref identity is invalid"
        );
        refs.push(SavedRef {
            name: fields[0].into(),
            oid: fields[1].into(),
            kind: fields[2].into(),
        });
    }
    Ok(refs)
}

fn refs(repo: &Repo, budget: &mut Budget) -> Result<Vec<SavedRef>> {
    let remaining = MAX_REFS.saturating_sub(budget.refs);
    if remaining == 0 {
        budget.incomplete("ref_budget");
        anyhow::bail!("local ref observation budget is exhausted");
    }
    // Failed framing, overflow and rechecks consume the same reserved observation quota.
    budget.refs += remaining;
    let cap = budget
        .reserve(MAX_REF_BYTES)
        .context("local refs exceed the read budget")?;
    let refs = read_refs_capped(repo, remaining, cap, budget.deadline)?;
    if refs.len() >= remaining {
        budget.incomplete("ref_budget");
        anyhow::bail!("local ref inventory fills the remaining observation budget");
    }
    let bytes: usize = refs
        .iter()
        .map(|r| r.name.len() + r.oid.len() + r.kind.len() + 3)
        .sum();
    budget.bytes -= cap.saturating_sub(bytes);
    budget.refs -= remaining - refs.len();
    Ok(refs)
}

#[cfg(test)]
fn read_refs(repo: &Repo, maximum: usize) -> Result<Vec<SavedRef>> {
    read_refs_capped(
        repo,
        maximum,
        MAX_REF_BYTES,
        crate::infra::local_git::Deadline::new(),
    )
}

fn ref_identity(refs: &[SavedRef]) -> Vec<(&str, &str, &str)> {
    refs.iter()
        .map(|r| (r.name.as_str(), r.oid.as_str(), r.kind.as_str()))
        .collect()
}

fn verify_refs(repo: &Repo, initial: &[SavedRef], budget: &mut Budget) -> Result<()> {
    let current = refs(repo, budget)?;
    admit_repository(repo.root(), budget)?;
    ensure!(
        ref_identity(initial) == ref_identity(&current),
        "local refs changed during inspection"
    );
    ensure!(
        !budget.deadline.expired(),
        "local ref verification deadline expired"
    );
    Ok(())
}

fn read_object(
    repo: &Repo,
    kind: &str,
    spec: &str,
    cap: usize,
    deadline: crate::infra::local_git::Deadline,
) -> Result<Vec<u8>> {
    let out = repo.inspection_output_in_repository(&["cat-file", kind, spec], cap, deadline)?;
    ensure!(
        out.status.success() && out.stderr.is_empty(),
        "saved object is unavailable"
    );
    Ok(out.stdout)
}

fn versions(repo: &Repo, refs: &[SavedRef], budget: &mut Budget) -> Vec<Version> {
    let mut queue: VecDeque<_> = refs
        .iter()
        .filter_map(|r| {
            if r.kind != "commit" {
                budget.incomplete("noncommit_ref");
                None
            } else {
                Some((r.oid.clone(), r.name.clone()))
            }
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut versions = Vec::new();
    while let Some((commit, git_ref)) = queue.pop_front() {
        if budget.work.remaining() == 0 {
            budget.incomplete("record_budget");
            break;
        }
        if !seen.insert(commit.clone()) {
            continue;
        }
        if budget.commits == MAX_COMMITS {
            budget.incomplete("commit_budget");
            break;
        }
        budget.commits += 1;
        let Some(cap) = budget.reserve(MAX_COMMIT_BYTES) else {
            break;
        };
        let raw = match read_object(repo, "commit", &commit, cap, budget.deadline) {
            Ok(raw) => raw,
            Err(_) => {
                budget.incomplete("unavailable_commit");
                continue;
            }
        };
        budget.bytes -= cap.saturating_sub(raw.len());
        let (parents, saved_at) = match commit_headers(&raw) {
            Ok(headers) => headers,
            Err(_) => {
                budget.incomplete("invalid_commit");
                continue;
            }
        };
        for parent in parents {
            if queue.len() + seen.len() >= MAX_COMMITS + MAX_REFS {
                budget.incomplete("commit_budget");
                break;
            }
            queue.push_back((parent, git_ref.clone()));
        }
        let Some(cap) = budget.reserve(MAX_META_BYTES) else {
            break;
        };
        let raw = match read_object(
            repo,
            "blob",
            &format!("{commit}:{}", meta::FILE),
            cap,
            budget.deadline,
        ) {
            Ok(raw) => raw,
            Err(_) => {
                budget.incomplete("unavailable_metadata");
                continue;
            }
        };
        budget.bytes -= cap.saturating_sub(raw.len());
        let snapshot = match std::str::from_utf8(&raw).ok().and_then(|text| {
            budget.work.json(text).ok()?;
            crate::domain::metadata_facts::JsonFacts::parse(text).ok()?;
            meta::parse_strict(text, &commit).ok()
        }) {
            Some(meta) => meta,
            None => {
                budget.incomplete("invalid_metadata");
                continue;
            }
        };
        if snapshot.line != meta::Line::Session || snapshot.session.is_empty() {
            continue;
        }
        versions.push(Version {
            oid: commit,
            git_ref,
            saved_at,
            snapshot,
        });
    }
    versions
}

fn commit_headers(raw: &[u8]) -> Result<(Vec<String>, i64)> {
    let header = raw
        .split(|b| *b == b'\n')
        .take_while(|line| !line.is_empty());
    let mut parents = Vec::new();
    let mut saved_at = None;
    let mut tree = None;
    for line in header {
        if let Some(value) = line.strip_prefix(b"parent ") {
            let value = std::str::from_utf8(value)?;
            ensure!(oid(value), "saved parent identity is invalid");
            parents.push(value.to_owned());
        } else if let Some(value) = line.strip_prefix(b"tree ") {
            let value = std::str::from_utf8(value)?;
            ensure!(
                tree.is_none() && oid(value),
                "saved tree identity is invalid"
            );
            tree = Some(value);
        } else if let Some(value) = line.strip_prefix(b"committer ") {
            ensure!(saved_at.is_none(), "saved committer is repeated");
            let value = std::str::from_utf8(value)?;
            let mut fields = value.rsplitn(3, ' ');
            let _zone = fields.next().context("saved timezone is absent")?;
            saved_at = Some(fields.next().context("saved time is absent")?.parse()?);
        }
    }
    ensure!(
        tree.is_some() && raw.windows(2).any(|w| w == b"\n\n"),
        "saved commit headers are incomplete"
    );
    Ok((parents, saved_at.context("saved commit time is absent")?))
}

struct Projected {
    event: Event,
    runtime: String,
    scope: Option<EventScope>,
    searchable: String,
    incomplete: bool,
    turns_incomplete: bool,
}

fn is_merge_summary(content: &Value) -> bool {
    content["type"] == "user"
        && content["agit"] == "merge_summary"
        && content["message"]["role"] == "user"
        && content["message"]["content"].is_string()
}

/// Identity records have no searchable body and do not open a user turn.
fn identity_record(envelope: &transcript::Envelope) -> bool {
    let content = &envelope.content;
    envelope.source == "codex"
        && content["type"] == "session_meta"
        && ["id", "cwd"].iter().all(|key| {
            content["payload"][*key]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        })
}

/// Missing display content only invalidates turn counts when it might hide a user prompt.
/// Unknown record kinds remain uncertain; an explicit non-user kind is not a lost turn.
fn non_turn_record(envelope: &transcript::Envelope) -> bool {
    if identity_record(envelope) {
        return true;
    }
    if envelope.source != "codex" {
        return false;
    }
    let content = &envelope.content;
    let payload = &content["payload"];
    match content["type"].as_str() {
        Some("turn_context" | "world_state") => payload.is_object(),
        Some("response_item") => match payload["type"].as_str() {
            Some(
                "reasoning"
                | "function_call"
                | "custom_tool_call"
                | "local_shell_call"
                | "function_call_output"
                | "custom_tool_call_output"
                | "local_shell_call_output",
            ) => true,
            Some("message") => matches!(
                payload["role"].as_str(),
                Some("assistant" | "developer" | "system")
            ),
            _ => false,
        },
        Some("event_msg") => matches!(
            payload["type"].as_str(),
            Some("token_count" | "context_compacted" | "task_complete" | "patch_apply_end")
        ),
        _ => false,
    }
}

fn project(log: &str, work: &mut storage::LocalReadBudget) -> Result<(Vec<Projected>, usize)> {
    work.lines(log)?;
    let envelopes = storage::parse_envelopes(log)?;
    // Native parsers can expand block arrays; reserve their input structure before either pass.
    work.lines(log)?;
    work.lines(log)?;
    work.lines(log)?;
    let mut session = transcript::display::parse(log)?;
    for event in &mut session.events {
        if event
            .line
            .and_then(|line| envelopes.get(line))
            .is_some_and(|env| is_merge_summary(&env.content))
        {
            event.kind = EventKind::CompactSummary;
        }
    }
    let turns = crate::domain::turn::groups_of(&session).len();
    let represented: BTreeSet<_> = session.events.iter().filter_map(|e| e.line).collect();
    let unrepresented = envelopes
        .iter()
        .enumerate()
        .any(|(line, envelope)| !represented.contains(&line) && !identity_record(envelope));
    let turns_incomplete = envelopes
        .iter()
        .enumerate()
        .any(|(line, envelope)| !represented.contains(&line) && !non_turn_record(envelope))
        || session.events.iter().any(|event| {
            event.kind == EventKind::Other
                && event
                    .line
                    .and_then(|line| envelopes.get(line))
                    .is_none_or(|envelope| !non_turn_record(envelope))
        });
    let mut groups: BTreeMap<transcript::display::SourceKey, (String, Vec<usize>)> =
        BTreeMap::new();
    for (line, envelope) in envelopes.iter().enumerate() {
        if is_merge_summary(&envelope.content) {
            continue;
        }
        if envelope.source == "codex"
            && envelope.content["payload"]["type"] == "function_call"
            && let Some(arguments) = envelope.content["payload"]["arguments"].as_str()
        {
            // Encoded argument JSON has its own structure; malformed raw arguments remain text.
            let admitted = work.json(arguments);
            if work.remaining() == 0 {
                admitted?;
                anyhow::bail!("local history exceeds the shared work budget");
            }
        }
        let group = groups
            .entry(transcript::display::source_key(envelope))
            .or_default();
        group.0.push_str(&serde_json::to_string(&envelope.content)?);
        group.0.push('\n');
        group.1.push(line);
    }
    let mut details = BTreeMap::new();
    for (key, (raw, positions)) in groups {
        let adapter = adapter::get(&key.source)?;
        work.lines(&raw)?;
        work.lines(&raw)?;
        let parsed = adapter.parse(&raw)?;
        work.lines(&raw)?;
        let enriched = adapter::enrich::tool_inputs(adapter.format(), &raw, &parsed);
        let mut slots = BTreeMap::<usize, usize>::new();
        for (index, event) in parsed.events.iter().enumerate() {
            if !matches!(event.kind, EventKind::ToolUse | EventKind::FileEdit) {
                continue;
            }
            let line = event.line.context("saved tool has no source coordinate")?;
            let global = *positions
                .get(line)
                .context("saved tool coordinate is invalid")?;
            let slot = slots.entry(global).or_default();
            if let Some(detail) = enriched.get(index) {
                details.insert((global, *slot), detail.clone());
            }
            *slot += 1;
        }
    }
    let mut slots = BTreeMap::<usize, usize>::new();
    ensure!(
        !session.events.is_empty() || envelopes.is_empty(),
        "saved records have no semantic projection"
    );
    let mut output = Vec::new();
    for event in session.events {
        let line = event.line.context("saved event has no source coordinate")?;
        let envelope = envelopes
            .get(line)
            .context("saved event coordinate is invalid")?;
        let mut scope = match event.kind {
            EventKind::UserPrompt | EventKind::UserInterjection => Some(EventScope::Prompt),
            EventKind::AssistantReply => Some(EventScope::Reply),
            EventKind::ToolUse => Some(EventScope::Tool),
            EventKind::ToolResult => Some(EventScope::Output),
            EventKind::FileEdit => Some(EventScope::Edit),
            EventKind::CompactFiltered | EventKind::CompactSummary => Some(EventScope::Summary),
            EventKind::TurnEnd | EventKind::Other => None,
        };
        if is_merge_summary(&envelope.content) {
            scope = Some(EventScope::Summary);
        }
        let mut searchable = event.text.clone().unwrap_or_default();
        let mut incomplete = unrepresented || event.kind == EventKind::Other;
        if matches!(event.kind, EventKind::ToolUse | EventKind::FileEdit) {
            let slot = slots.entry(line).or_default();
            match details.get(&(line, *slot)).and_then(|d| d.input.as_ref()) {
                Some(input) => {
                    searchable.push('\n');
                    searchable.push_str(&input.to_string());
                }
                None => incomplete = true,
            }
            *slot += 1;
        }
        if scope.is_some() && searchable.is_empty() {
            incomplete = true;
        }
        output.push(Projected {
            event,
            runtime: envelope.source.clone(),
            scope,
            searchable,
            incomplete,
            turns_incomplete,
        });
    }
    Ok((output, turns))
}

fn excerpt(text: &str, query: &Query) -> String {
    let start = query.first_hit(text).unwrap_or(0);
    // Lowercasing can change byte width; find a valid boundary in the original evidence.
    let mut boundary = start.min(text.len());
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let prefix = text[..boundary].chars().rev().take(60).collect::<String>();
    let before: String = prefix.chars().rev().collect();
    let after: String = text[boundary..].chars().take(260).collect();
    before
        .chars()
        .chain(after.chars())
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_parent_headers_ignore_mutable_graph_descriptions_and_refuse_bad_identity() {
        let parent = "a".repeat(40);
        let tree = "b".repeat(40);
        let raw = format!(
            "tree {tree}\nparent {parent}\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 2 +0000\n\nbody"
        );
        assert_eq!(commit_headers(raw.as_bytes()).unwrap(), (vec![parent], 2));
        assert!(commit_headers(raw.replace("parent a", "parent x").as_bytes()).is_err());
        assert!(commit_headers(raw.replace("\n\nbody", "").as_bytes()).is_err());
    }

    #[test]
    fn budget_is_reserved_before_reads_and_cannot_be_reset_by_another_query() {
        let mut budget = Budget {
            bytes: MAX_READ_BYTES - 3,
            ..Default::default()
        };
        assert_eq!(budget.reserve(10), Some(3));
        assert_eq!(budget.reserve(1), None);
        assert!(budget.stopped());
        assert!(budget.reasons.contains("read_budget"));
    }

    #[test]
    fn projection_preserves_occurrences_sources_and_tool_argument_scope() {
        let sid = format!("agit-{}", "a".repeat(40));
        let user = json!({"type":"user", "message":{"role":"user", "content":"needle"}});
        let call = json!({"type":"assistant", "agit":"merge_summary", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"Bash", "input":{"command":"argument-only-needle"}}]}});
        let log = transcript::wrap_lines(&format!("{user}\n{call}\n{user}\n"), "claude-code", &sid);
        let (events, turns) =
            project(&log, &mut storage::LocalReadBudget::new(MAX_RECORDS)).unwrap();
        assert_eq!(turns, 2);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.line, Some(0));
        assert_eq!(events[2].event.line, Some(2));
        assert_eq!(events[1].scope, Some(EventScope::Tool));
        assert!(events[1].searchable.contains("argument-only-needle"));
        assert!(events.iter().all(|event| !event.incomplete));
        assert!(events.iter().all(|event| event.runtime == "claude-code"));
    }

    #[test]
    fn agentgit_merge_summary_uses_its_own_schema_with_a_native_source_label() {
        let sid = format!("agit-{}", "a".repeat(40));
        let summary = json!({"type":"user", "agit":"merge_summary", "message":{"role":"user", "content":"saved reconciliation"}});
        let log = transcript::wrap_lines(&format!("{summary}\n"), "opencode", &sid);
        let (events, turns) =
            project(&log, &mut storage::LocalReadBudget::new(MAX_RECORDS)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scope, Some(EventScope::Summary));
        assert_eq!(events[0].searchable, "saved reconciliation");
        assert_eq!(turns, 0);
        assert!(!events[0].incomplete);
    }

    #[test]
    fn unrepresented_saved_records_cannot_establish_complete_absence() {
        let sid = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(
            "{\"type\":\"future-record\",\"text\":\"needle\"}\n",
            "claude-code",
            &sid,
        );
        if let Ok((events, _)) = project(&log, &mut storage::LocalReadBudget::new(MAX_RECORDS)) {
            assert!(events.iter().any(|event| event.incomplete));
        }
    }

    #[test]
    fn ref_recheck_refuses_real_namespace_and_object_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        std::fs::write(temp.path().join("fixture"), "first").unwrap();
        repo.add_all().unwrap();
        repo.commit("first").unwrap();
        let initial = read_refs(&repo, MAX_REFS + 1).unwrap();
        assert!(verify_refs(&repo, &initial, &mut Budget::default()).is_ok());
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/main", &head])
            .unwrap();
        assert!(verify_refs(&repo, &initial, &mut Budget::default()).is_err());
        let second = read_refs(&repo, MAX_REFS + 1).unwrap();
        assert!(verify_refs(&repo, &second, &mut Budget::default()).is_ok());
        std::fs::write(temp.path().join("fixture"), "changed").unwrap();
        repo.add_all().unwrap();
        repo.commit("changed").unwrap();
        assert!(verify_refs(&repo, &second, &mut Budget::default()).is_err());
    }
    #[test]
    fn carrier_admission_refuses_ancestor_discovery_and_keeps_linked_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let outer = Repo::init(temp.path()).unwrap();
        std::fs::write(temp.path().join("fixture"), "outer").unwrap();
        outer.add_all().unwrap();
        outer.commit("owned outer").unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        admit_repository(&canonical, &mut Budget::default()).unwrap_or_else(|error| {
            let inspection = Repo::at(&canonical).inspection_output_in_repository(
                &["rev-parse", "--show-toplevel"],
                MAX_META_BYTES,
                crate::infra::local_git::Deadline::new(),
            );
            panic!(
                "ordinary repository admission failed: {error:#}; root={canonical:?}; inspection={inspection:?}"
            );
        });
        let child = temp.path().join("repos/alice/phantom");
        std::fs::create_dir_all(&child).unwrap();
        assert!(admit_repository(&child, &mut Budget::default()).is_err());
        let output = Repo::at(&child)
            .inspection_output_in_repository(
                &["for-each-ref"],
                MAX_REF_BYTES,
                crate::infra::local_git::Deadline::new(),
            )
            .unwrap();
        assert!(
            !output.status.success(),
            "a missing selected carrier cannot discover the parent"
        );
        let linked = temp.path().join("linked");
        outer
            .git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()])
            .unwrap();
        let admitted = admit_repository(&linked, &mut Budget::default()).unwrap();
        assert_eq!(read_refs(&admitted, MAX_REFS).unwrap().len(), 2);
        std::fs::write(
            child.join(".git"),
            format!("gitdir: {}\n", temp.path().join(".git").display()),
        )
        .unwrap();
        assert!(
            admit_repository(&child, &mut Budget::default()).is_err(),
            "a gitfile cannot borrow a primary repository without an owned backlink"
        );
        std::fs::write(child.join(".git"), "invalid carrier").unwrap();
        assert!(admit_repository(&child, &mut Budget::default()).is_err());
    }
    #[test]
    fn dense_native_blocks_are_refused_before_projection_and_keep_the_shared_limit() {
        let sid = format!("agit-{}", "a".repeat(40));
        let content = json!({"type":"assistant", "message":{"role":"assistant", "content":vec![json!({"type":"text", "text":"x"}); 60_000]}});
        let log = transcript::wrap_lines(&format!("{content}\n"), "claude-code", &sid);
        assert!(log.len() < MAX_OBJECT_BYTES);
        let mut work = storage::LocalReadBudget::new(MAX_RECORDS);
        assert!(project(&log, &mut work).is_err());
        assert_eq!(work.remaining(), 0);
        let small = transcript::wrap_lines(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"needle\"}}\n",
            "claude-code",
            &sid,
        );
        assert!(project(&small, &mut work).is_err());
    }
    #[test]
    fn encoded_codex_arguments_spend_structure_before_enrichment() {
        let sid = format!("agit-{}", "a".repeat(40));
        let arguments = format!("[{}0]", "0,".repeat(30_000));
        let call = json!({"type":"response_item", "payload":{"type":"function_call", "call_id":"call", "name":"exec_command", "arguments":arguments}});
        let log = transcript::wrap_lines(&format!("{call}\n"), "codex", &sid);
        let mut work = storage::LocalReadBudget::new(MAX_RECORDS);
        assert!(project(&log, &mut work).is_err());
        assert_eq!(work.remaining(), 0);
    }
    #[test]
    fn failed_ref_inventory_spends_its_quota_before_another_repository() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        std::fs::write(temp.path().join("fixture"), "owned").unwrap();
        repo.add_all().unwrap();
        repo.commit("owned ref inventory").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        for n in 0..MAX_REFS {
            repo.git(&["update-ref", &format!("refs/heads/owned-{n}"), &head])
                .unwrap();
        }
        let mut budget = Budget::default();
        assert!(refs(&repo, &mut budget).is_err());
        assert_eq!(budget.refs, MAX_REFS);
        assert_eq!(budget.bytes, MAX_REF_BYTES);
        let absent = Repo::at(temp.path().join("does-not-exist"));
        assert!(
            refs(&absent, &mut budget)
                .err()
                .unwrap()
                .to_string()
                .contains("observation budget is exhausted")
        );
        assert_eq!(budget.refs, MAX_REFS);
    }

    #[test]
    fn successful_ref_recheck_spends_the_same_command_budget() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        std::fs::write(temp.path().join("fixture"), "owned").unwrap();
        repo.add_all().unwrap();
        repo.commit("owned ref recheck").unwrap();
        let mut budget = Budget::default();
        let initial = refs(&repo, &mut budget).unwrap();
        assert_eq!(budget.refs, 1);
        let before_bytes = budget.bytes;
        verify_refs(&repo, &initial, &mut budget).unwrap();
        assert_eq!(budget.refs, 2);
        assert!(budget.bytes > before_bytes);
        budget.refs = MAX_REFS - 1;
        assert!(verify_refs(&repo, &initial, &mut budget).is_err());
        assert_eq!(budget.refs, MAX_REFS);
    }
    #[test]
    fn reserved_verification_keeps_the_expired_command_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        std::fs::write(temp.path().join("fixture"), "owned").unwrap();
        repo.add_all().unwrap();
        repo.commit("owned deadline control").unwrap();
        let initial = read_refs(&repo, MAX_REFS).unwrap();
        let deadline = crate::infra::local_git::Deadline::at(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        let mut budget = Budget {
            deadline,
            work: storage::LocalReadBudget::with_deadline(MAX_RECORDS, deadline),
            ..Default::default()
        };
        let mut verification = reserve_verification(&initial, &mut budget).unwrap();
        assert!(verification.deadline.expired());
        assert!(verification.work.spend(1).is_err());
        assert!(verify_refs(&repo, &initial, &mut verification).is_err());
        assert!(verification.reasons.contains("time_budget"));
        assert!(budget.stopped());
        assert!(verify_refs(&repo, &initial, &mut Budget::default()).is_ok());
    }
}
