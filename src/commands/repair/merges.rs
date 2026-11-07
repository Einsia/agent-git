//! A session branch whose merges recorded the source's copy of history the branch already held is
//! copied to a new branch without those copies.
//!
//! What a merge leaves out today (see [`crate::commands::merge::shared`]) an earlier merge recorded
//! again, so the session's page shows those turns twice. The repair proves each copy the same way
//! a merge does now, from the merge's own two parents, and leaves out only what that proves: a
//! record outside a merge's block, or one nothing proves, stays. Published history isn't
//! rewritten. The copy is a new branch from the last commit before the first such merge, every
//! later commit replayed with its message, authors and dates; the branch it was made from stays as
//! it is, and new turns keep going there until the person moves on.

use crate::commands::merge::shared::{self, Proof};
use crate::domain::{link, meta, privacy_git, repo::Repo, storage, transcript};
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::{Context as _, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// The branch a repair writes when none is named.
pub(crate) fn default_name(branch: &str) -> String {
    format!("{branch}-repaired")
}

/// One record a repair leaves out.
#[derive(Debug, serde::Serialize)]
pub(crate) struct Dropped {
    /// Its place in the branch's LOG, from 1.
    pub position: usize,
    /// The turn of the session's page it shows in, when the LOG reads as turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    pub event: String,
    /// The record it is a copy of.
    pub original: String,
}

/// One merge whose block holds copies.
#[derive(Debug, serde::Serialize)]
pub(crate) struct Repaired {
    pub merge: String,
    pub proof: serde_json::Value,
    pub dropped: Vec<Dropped>,
}

/// What a repair of a branch at one tip comes to.
pub(crate) struct Plan {
    pub merges: Vec<Repaired>,
    /// The first-parent commits replayed, oldest first: the first repaired merge and every commit
    /// after it.
    replayed: Vec<String>,
    /// LOG positions, from 0, left out of every replayed commit that holds them.
    left_out: BTreeSet<usize>,
    /// A copy's line to the original line a VIEW names in its place.
    originals: BTreeMap<String, String>,
}

fn log_lines(repo: &Repo, commit: &str) -> crate::Result<Vec<String>> {
    Ok(shared::lines(&storage::materialize_at(
        repo.root(),
        commit,
        meta::LOG_FILE,
    )?))
}

fn parents(repo: &Repo, commit: &str) -> crate::Result<Vec<String>> {
    Ok(repo
        .git(&["rev-list", "--no-walk", "--parents", commit])?
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect())
}

/// Whether `commit`'s own tree shows it is plain: nothing under `privacy/`.
fn shown_plain(repo: &Repo, commit: &str) -> bool {
    matches!(
        repo.git_status_local(&["ls-tree", "--name-only", commit, "--", "privacy"]),
        Ok((Some(0), stdout, stderr)) if stdout.trim().is_empty() && stderr.is_empty()
    )
}

/// The proof as the dry run names it: commits only, never a record's text.
fn named(proof: &Proof) -> serde_json::Value {
    match proof {
        Proof::Common(base) => serde_json::json!({"common": base}),
        Proof::Published { public, source } => {
            serde_json::json!({"published": public, "of": source})
        }
    }
}

/// The turn each LOG line shows in on the session's page, which starts a turn at every prompt.
fn turns(lines: &[String]) -> Option<Vec<Option<usize>>> {
    let session = transcript::display::parse(&lines.concat()).ok()?;
    let mut at = vec![None; lines.len()];
    for (turn, group) in crate::domain::turn::groups_of(&session).iter().enumerate() {
        for &line in group {
            *at.get_mut(line)? = Some(turn + 1);
        }
    }
    Some(at)
}

/// What a repair of `branch` at `tip` leaves out, merge by merge.
pub(crate) fn plan(repo: &Repo, branch: &str, tip: &str) -> crate::Result<Plan> {
    let chain = repo
        .git(&["rev-list", "--first-parent", "--reverse", tip])?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut merges = Vec::new();
    let mut left_out = BTreeSet::new();
    let mut originals = BTreeMap::new();
    let mut first = None;
    let tip_lines = log_lines(repo, tip)?;
    let at = turns(&tip_lines);
    for (index, commit) in chain.iter().enumerate() {
        let parents = parents(repo, commit)?;
        let [target, source] = parents.as_slice() else {
            continue;
        };
        let Some(snapshot) = meta::read_at_ref(repo, commit) else {
            continue;
        };
        if snapshot.kind != meta::Kind::Merge || !snapshot.is_session_line() {
            continue;
        }
        let (Ok(own), Ok(target_log), Ok(source_log)) = (
            log_lines(repo, commit),
            log_lines(repo, target),
            log_lines(repo, source),
        ) else {
            continue;
        };
        let base = repo
            .git_opt(&["merge-base", target, source])
            .unwrap_or_default();
        let proven = shared::shared_prefix(
            repo,
            repo,
            branch,
            target,
            source,
            base.trim(),
            &target_log,
            &source_log,
        );
        let Some(proof) = proven.proof.as_ref().filter(|_| proven.lines() > 0) else {
            continue;
        };
        // The block follows the target's LOG and its start marker, and opens with the copy.
        let start = target_log.len() + 1;
        let copied = start + proven.lines();
        if !own.starts_with(&target_log)
            || own.len() < copied
            || !own[target_log.len()].contains("agit:__merge_start__")
            || own[start..copied] != source_log[..proven.lines()]
        {
            continue;
        }
        let mut dropped = Vec::new();
        for (offset, original) in proven.originals.iter().enumerate() {
            let position = start + offset;
            left_out.insert(position);
            let copy = &own[position];
            if copy != original {
                originals.insert(copy.clone(), original.clone());
            }
            dropped.push(Dropped {
                position: position + 1,
                turn: at
                    .as_ref()
                    .and_then(|at| at.get(position).copied().flatten()),
                event: storage::canonical_event_id(copy),
                original: storage::canonical_event_id(original),
            });
        }
        first.get_or_insert(index);
        merges.push(Repaired {
            merge: commit.clone(),
            proof: named(proof),
            dropped,
        });
    }
    let replayed = first.map_or_else(Vec::new, |first| chain[first..].to_vec());
    Ok(Plan {
        merges,
        replayed,
        left_out,
        originals,
    })
}

/// `commit`'s raw object with `tree` and its first parent replaced, everything else as it was:
/// message, authors and dates. A signature no longer signs it, so it goes.
fn rewritten(raw: &str, tree: &str, first_parent: &str) -> crate::Result<String> {
    let (headers, message) = raw
        .split_once("\n\n")
        .context("a commit has no header boundary")?;
    let mut out = String::with_capacity(raw.len());
    let mut first = true;
    let mut signature = false;
    for line in headers.lines() {
        if signature && line.starts_with(' ') {
            continue;
        }
        signature = false;
        if line.starts_with("tree ") {
            out.push_str(&format!("tree {tree}\n"));
        } else if line.starts_with("parent ") && first {
            out.push_str(&format!("parent {first_parent}\n"));
            first = false;
        } else if line.starts_with("gpgsig") {
            signature = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    ensure!(!first, "a replayed commit has no parent");
    out.push('\n');
    out.push_str(message);
    Ok(out)
}

/// Writes the plan's copy and returns its tip. Nothing points at it yet.
pub(crate) fn replay(repo: &Repo, plan: &Plan) -> crate::Result<String> {
    let first = plan.replayed.first().context("nothing to replay")?;
    let mut previous = parents(repo, first)?
        .into_iter()
        .next()
        .context("a repaired merge has no parent")?;
    for commit in &plan.replayed {
        let snapshot = meta::read_at_ref(repo, commit)
            .with_context(|| format!("{commit} carries no session metadata"))?;
        ensure!(
            snapshot.layout == meta::LayoutVersion::V1,
            "{commit} is stored in an older layout; open the branch once with this agit and repair again"
        );
        let lines = log_lines(repo, commit)?;
        let log = lines
            .iter()
            .enumerate()
            .filter(|(position, _)| !plan.left_out.contains(position))
            .map(|(_, line)| line.as_str())
            .collect::<String>();
        let view = shared::lines(&storage::materialize_at(
            repo.root(),
            commit,
            meta::VIEW_FILE,
        )?)
        .into_iter()
        .map(|line| plan.originals.get(&line).cloned().unwrap_or(line))
        .collect::<String>();
        let mut edits = repo
            .ls_tree(commit)
            .into_iter()
            .filter(|path| meta::is_storage_path(path) && path != meta::FILE)
            .map(|path| (path, None))
            .collect::<BTreeMap<_, _>>();
        for (path, bytes) in storage::snapshot_files(&log, &view)? {
            edits.insert(path, Some(bytes));
        }
        let tree =
            crate::commands::plumbing::tree_apply_owned(repo, commit, edits.into_iter().collect())?;
        let raw = String::from_utf8(repo.git_bytes_result(&["cat-file", "commit", commit])?)
            .context("a replayed commit is not UTF-8")?;
        let object = rewritten(&raw, &tree, &previous)?;
        previous = crate::commands::plumbing::raw_git(
            repo,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(&object),
        )?
        .trim()
        .to_owned();
    }
    Ok(previous)
}

/// Why a branch can't be repaired, or `None`.
fn refusal(
    repo: &Repo,
    store: &crate::domain::store::Store,
    slug: &str,
    branch: &str,
    plan: &Plan,
) -> crate::Result<Option<String>> {
    // A hold is kept by branch, so a copy under a new name would carry the turns it holds back.
    if let Some(hold) = link::launch::held(store, slug, branch) {
        return Ok(Some(link::launch::explain(&hold)));
    }
    // A withdrawal is matched by session, and the copy keeps its sessions.
    if let Some(identity) = crate::hub::identity::read(repo)? {
        let withdrawn = privacy_git::withheld_refs(repo, &identity)
            .context("this checkout's record of withdrawn sessions can't be read")?;
        if withdrawn.contains(&format!("refs/heads/{branch}")) {
            return Ok(Some(format!(
                "{slug}@{branch} is withdrawn on the Hub; a repaired copy would carry the same sessions, so nothing was written"
            )));
        }
    }
    // A commit the copy replays is new to the Hub: one whose records went out encrypted only
    // would go out plain.
    if let Some(commit) = plan
        .replayed
        .iter()
        .find(|commit| !shown_plain(repo, commit))
    {
        return Ok(Some(format!(
            "{slug}@{branch} holds history published encrypted ({}), which a repaired copy can't carry; nothing was written",
            &commit[..12.min(commit.len())]
        )));
    }
    Ok(None)
}

pub(super) fn run(target: &str, into: Option<&str>, dry_run: bool) -> super::super::CmdResult {
    let json = crate::commands::json::requested();
    let parsed = crate::commands::target::parse(target)?;
    let (Some(slug), Some(branch)) = (parsed.repo, parsed.base) else {
        anyhow::bail!("name the session branch: `owner/repo@branch`");
    };
    let (owner, name) = crate::commands::parse_slug(&slug)?;
    let repo = Repo::open(config::repo_dir(&owner, &name)?)
        .with_context(|| format!("{slug} does not exist locally"))?;
    let store = crate::domain::store::Store::open_or_init()?;
    let _guard = link::lock_branch(&store, &slug, &branch)?;
    let tip = repo
        .git(&["rev-parse", "--verify", &format!("{branch}^{{commit}}")])?
        .trim()
        .to_owned();
    let plan = plan(&repo, &branch, &tip)?;
    let into = into.map_or_else(|| default_name(&branch), str::to_owned);
    if let Some(why) = refusal(&repo, &store, &slug, &branch, &plan)? {
        ui::error(&why);
        return Ok(ExitCode::Precondition);
    }
    let count: usize = plan.merges.iter().map(|merge| merge.dropped.len()).sum();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "branch": branch,
                "into": into,
                "dry_run": dry_run,
                "merges": plan.merges,
                "replayed": plan.replayed.len(),
            })
        );
    } else if plan.merges.is_empty() {
        ui::info(format_args!(
            "{slug}@{branch}: no merge recorded history the branch already held; nothing to repair"
        ));
    } else {
        for merge in &plan.merges {
            ui::info(format_args!(
                "merge {} ({}): {} record(s) recorded again",
                &merge.merge[..12.min(merge.merge.len())],
                merge.proof,
                merge.dropped.len()
            ));
            for dropped in &merge.dropped {
                let turn = dropped
                    .turn
                    .map_or_else(String::new, |turn| format!("turn #{turn}, "));
                ui::info(format_args!(
                    "  {turn}LOG line {}: event {} is a copy of {}",
                    dropped.position, dropped.event, dropped.original
                ));
            }
        }
    }
    if dry_run || plan.merges.is_empty() {
        if !json && dry_run && !plan.merges.is_empty() {
            ui::hint(&format!(
                "`agit repair merges {slug}@{branch}` writes these {count} record(s)' repaired copy to `{into}`"
            ));
        }
        return Ok(ExitCode::Ok);
    }
    if repo
        .git_opt(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{into}"),
        ])
        .is_some()
    {
        ui::error(&format!(
            "`{into}` already exists; name the repaired copy with `-b <new-name>`"
        ));
        return Ok(ExitCode::Precondition);
    }
    let copy = replay(&repo, &plan)?;
    repo.git(&[
        "update-ref",
        &format!("refs/heads/{into}"),
        &copy,
        &"0".repeat(copy.len()),
    ])?;
    if !json {
        ui::success(&format!(
            "repaired copy written to {slug}@{into}: {count} record(s) left out, {} commit(s) replayed",
            plan.replayed.len()
        ));
        ui::info(format_args!(
            "{slug}@{branch} is unchanged, and new turns keep going there until you continue on {into}"
        ));
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_plan(replayed: Vec<String>) -> Plan {
        Plan {
            merges: Vec::new(),
            replayed,
            left_out: BTreeSet::new(),
            originals: BTreeMap::new(),
        }
    }

    /// A branch that is held, withdrawn on the Hub, or holds history published encrypted is not
    /// copied: under a new name it would carry out what its own name keeps in.
    #[test]
    fn held_withdrawn_or_encrypted_branches_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("r")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "# shared\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("plain").unwrap();
        let plain = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        let store = crate::domain::store::Store::at(dir.path().join("home/store"));
        std::fs::create_dir_all(store.root()).unwrap();
        let refused =
            |repo: &Repo, plan: &Plan| refusal(repo, &store, "me/qa", "work", plan).unwrap();
        assert_eq!(refused(&repo, &empty_plan(vec![plain.clone()])), None);

        std::fs::create_dir_all(repo.root().join("privacy")).unwrap();
        std::fs::write(repo.root().join("privacy/envelope.json"), "{}").unwrap();
        repo.add_all().unwrap();
        repo.commit("encrypted").unwrap();
        let encrypted = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        let why = refused(&repo, &empty_plan(vec![plain.clone(), encrypted])).unwrap();
        assert!(why.contains("published encrypted"), "{why}");

        let identity = crate::hub::identity::RemoteIdentity::new(
            "https://hub.example",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        crate::hub::identity::pin(&repo, &identity).unwrap();
        privacy_git::withhold_refs(&repo, &identity, &["refs/heads/work".into()]).unwrap();
        let why = refused(&repo, &empty_plan(vec![plain.clone()])).unwrap();
        assert!(why.contains("withdrawn"), "{why}");

        let holds = dir.path().join("home/upload-holds");
        std::fs::create_dir_all(&holds).unwrap();
        let digest = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(b"me/qa@work"))
        };
        std::fs::write(
            holds.join(format!("{}.json", &digest[..32])),
            r#"{"slug":"me/qa","branch":"work","sessions":["codex/s"],"released":false}"#,
        )
        .unwrap();
        let why = refused(&repo, &empty_plan(vec![plain])).unwrap();
        assert!(why.contains(link::launch::REFUSAL), "{why}");
    }

    #[test]
    fn a_replayed_commit_keeps_its_message_authors_and_dates() {
        let raw = "tree aaaa\nparent bbbb\nparent cccc\nauthor A <a@x> 1700000000 +0100\ncommitter C <c@x> 1700000100 +0200\ngpgsig -----BEGIN-----\n line\n -----END-----\n\nagit: merge b → main\n\nsummary\n";
        let out = rewritten(raw, "dddd", "eeee").unwrap();
        assert_eq!(
            out,
            "tree dddd\nparent eeee\nparent cccc\nauthor A <a@x> 1700000000 +0100\ncommitter C <c@x> 1700000100 +0200\n\nagit: merge b → main\n\nsummary\n"
        );
    }
}
