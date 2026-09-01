//! `agit revert` — drop: remove the given events from the target branch's VIEW.
//!
//! It lands a view commit and the agent stops seeing that content — a bad conclusion, sensitive
//! content that slipped in, a detour that went nowhere all go through here. **The evidence stays
//! in the log**; physical deletion is not offered (published history is immutable), and erasing
//! something for good means deleting the branch or the repo and distilling again (PRD).
//!
//! Refs are always written as one token: `@#7` is turn 7 of the current branch, or an explicit
//! `ref#7`.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Refs to drop: `<ref|@>#n[.k]`, repeatable.
    pub refs_: Vec<String>,
    /// Target branch (default: the context branch).
    #[arg(long, value_name = "owner/repo@branch")]
    pub into: Option<String>,
    /// Note (goes into the commit message).
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

/// When the ref itself spells out `owner/repo@branch`, that branch is where it lands (`--into`
/// with a bare branch name still takes the repo from the ref) — dropped events land on the same
/// line they live on, so there is no need to ask who owns this directory.
fn target_from_refs(refs_: &[String], into: Option<&str>) -> Option<(String, String)> {
    let spec = refs::parse(refs_.first()?).ok()?;
    let refs::RepoSel::Slug(o, n) = spec.repo else {
        return None;
    };
    let branch = match into {
        Some(b) => b.to_string(),
        None => match spec.base {
            refs::Base::Name(b) => b,
            _ => return None,
        },
    };
    Some((format!("{o}/{n}"), branch))
}

pub fn run(args: Args) -> CmdResult {
    if args.refs_.is_empty() {
        ui::error("no ref to drop was given.");
        ui::hint("e.g.: agit revert @#12.4  or  agit revert @#7");
        return Ok(ExitCode::Usage);
    }
    let cwd = std::env::current_dir()?;
    let (repo, target) = if let Some(raw) = args.into.as_deref()
        && raw.contains('@')
        && raw != "@"
    {
        let parsed = match crate::commands::target::branch_only(raw) {
            Ok(v) => v,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        };
        let slug = parsed
            .repo
            .ok_or_else(|| anyhow::anyhow!("target has no repository"))?;
        let branch = parsed
            .base
            .ok_or_else(|| anyhow::anyhow!("target has no branch"))?;
        let (o, n) = super::parse_slug(&slug)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{slug} doesn’t exist locally."));
            return Ok(ExitCode::Precondition);
        };
        (repo, branch)
    } else if let Some((slug, branch)) = target_from_refs(&args.refs_, args.into.as_deref()) {
        let (o, n) = super::parse_slug(&slug)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{slug} doesn’t exist locally."));
            return Ok(ExitCode::Precondition);
        };
        (repo, branch)
    } else {
        let ctx = match super::context::resolve(&cwd) {
            Ok(c) => c,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        };
        let (o, n) = super::parse_slug(&ctx.repo)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{} doesn’t exist locally.", ctx.repo));
            return Ok(ExitCode::Precondition);
        };
        (
            repo,
            args.into.clone().unwrap_or_else(|| ctx.branch.clone()),
        )
    };
    if super::branch::is_sealed(&repo, &target) {
        ui::error(&format!("`{target}` is sealed."));
        return Ok(ExitCode::Policy);
    }

    let head = repo.git(&["rev-parse", &format!("refs/heads/{target}")])?;
    let head = head.trim().to_string();
    let Some(snap) = meta::read_at_ref(&repo, &head) else {
        ui::error(&format!(
            "`{target}` carries no {} — its line was never declared.",
            meta::FILE
        ));
        return Ok(ExitCode::Precondition);
    };
    if snap.is_file_line() {
        ui::error(&format!("`{target}` is the file line — it has no VIEW."));
        ui::hint("file-line changes go through `agit commit -m`");
        return Ok(ExitCode::Precondition);
    }

    // ── Find the full-envelope event ids to remove, and how many times each ──
    //
    // `_object_hash` covers content only: full envelopes from different sessions / sources can
    // share it, so it cannot locate anything. The BTreeMap's counts must not degenerate into a
    // set either; otherwise selecting one duplicate event deletes every identical occurrence in
    // the VIEW.
    let mut doomed: std::collections::BTreeMap<String, usize> = Default::default();
    let mut selected_coordinates: std::collections::BTreeSet<(String, usize)> = Default::default();
    for r in &args.refs_ {
        let spec = refs::parse(r)?;
        let src_head = match &spec.base {
            refs::Base::At => head.clone(),
            refs::Base::Name(b) => repo
                .git(&["rev-parse", &format!("refs/heads/{b}")])
                .map(|s| s.trim().to_string())?,
            refs::Base::Default => {
                ui::error(&format!("`{r}` carries no branch."));
                return Ok(ExitCode::Ref);
            }
        };
        let raw = storage::materialize_at(repo.root(), &src_head, meta::LOG_FILE)?;
        let lines: Vec<&str> = raw.split_inclusive('\n').collect();
        match &spec.tail {
            refs::Tail::Turn(n) => {
                let n = crate::domain::refs::real_turn(&repo, &src_head, *n)?;
                for i in super::merge::turn_lines(&repo, &src_head, n)? {
                    let line = lines.get(i).ok_or_else(|| {
                        anyhow::anyhow!("selected LOG coordinate {i} is out of bounds")
                    })?;
                    add_occurrence_at(&mut selected_coordinates, &mut doomed, &src_head, i, line)?;
                }
            }
            refs::Tail::Event { turn, index } => {
                let n = crate::domain::refs::real_turn(&repo, &src_head, *turn)?;
                let ls = super::merge::turn_lines(&repo, &src_head, n)?;
                match ls.get((*index as usize).saturating_sub(1)) {
                    Some(&i) => {
                        let l = lines.get(i).ok_or_else(|| {
                            anyhow::anyhow!("selected LOG coordinate {i} is out of bounds")
                        })?;
                        add_occurrence_at(&mut selected_coordinates, &mut doomed, &src_head, i, l)?;
                    }
                    None => {
                        ui::error(&format!("`{r}`: turn {n} has no event {index}."));
                        return Ok(ExitCode::Ref);
                    }
                }
            }
            _ => {
                ui::error(&format!("`{r}` needs a turn-level ref (`#n` or `#n.k`)."));
                return Ok(ExitCode::Usage);
            }
        }
    }

    // ── Remove the selected multiplicity from the VIEW, and append the revert marker to
    // LOG / VIEW as an ordinary event ──
    let log = materialize_optional(&repo, &head, meta::LOG_FILE, &snap)?;
    let view = materialize_optional(&repo, &head, meta::VIEW_FILE, &snap)?;
    let what = args.refs_.join(" ");
    let (new_log, new_view, removed) =
        reverted_snapshot(log, view, doomed, &snap.runtime, &snap.session, &what)?;
    if removed == 0 {
        ui::error(
            "these events aren’t in the target branch’s VIEW (they may live only in the log — the agent never saw them).",
        );
        ui::hint("`agit view @ --json` shows the VIEW’s composition");
        return Ok(ExitCode::Precondition);
    }

    let mut s = snap.clone();
    s.kind = meta::Kind::View;
    s.layout = meta::LayoutVersion::CURRENT;
    let snap_text = meta::to_text(&s)?;
    let tree =
        super::plumbing::session_snapshot_tree(&repo, &head, &new_log, &new_view, &snap_text)?;
    let msg = format!(
        "agit: revert {}\n\n{}",
        what,
        args.message.clone().unwrap_or_default()
    );
    let commit = super::plumbing::commit_tree(&repo, &tree, &[&head], &msg)?;
    super::plumbing::update_branch_cas_and_refresh(&repo, &target, &commit, &head, false)?;

    ui::success(&format!(
        "dropped {removed} events from the VIEW of {target} (view commit {})",
        &commit[..9.min(commit.len())]
    ));
    println!(
        "{}",
        ui::dim(
            "  the evidence in the log is untouched — physical deletion would mean deleting the branch and re-distilling"
        )
    );
    Ok(ExitCode::Ok)
}

fn add_occurrence(
    occurrences: &mut std::collections::BTreeMap<String, usize>,
    line: &str,
) -> crate::Result<()> {
    let id = storage::event_id(line)?;
    *occurrences.entry(id).or_default() += 1;
    Ok(())
}

fn add_occurrence_at(
    selected: &mut std::collections::BTreeSet<(String, usize)>,
    occurrences: &mut std::collections::BTreeMap<String, usize>,
    source_head: &str,
    log_index: usize,
    line: &str,
) -> crate::Result<()> {
    if selected.insert((source_head.to_owned(), log_index)) {
        add_occurrence(occurrences, line)?;
    }
    Ok(())
}

fn materialize_optional(
    repo: &Repo,
    git_ref: &str,
    logical_path: &str,
    snapshot: &meta::Meta,
) -> crate::Result<String> {
    let raw_path = match (snapshot.layout, logical_path) {
        (meta::LayoutVersion::V0, meta::LOG_FILE) => meta::LEGACY_LOG_FILE,
        (meta::LayoutVersion::V0, meta::VIEW_FILE) => meta::LEGACY_VIEW_FILE,
        (_, path) => path,
    };
    match repo.show_result(git_ref, logical_path)? {
        Some(text) => Ok(text),
        None if snapshot.is_file_line() || snapshot.session.is_empty() => Ok(String::new()),
        None => anyhow::bail!("claimed session snapshot {git_ref} is missing required {raw_path}"),
    }
}

fn reverted_snapshot(
    mut log: String,
    view: String,
    mut doomed: std::collections::BTreeMap<String, usize>,
    runtime: &str,
    session: &str,
    what: &str,
) -> crate::Result<(String, String, usize)> {
    let mut kept = String::new();
    let mut removed = 0usize;
    for line in view.split_inclusive('\n') {
        let id = storage::event_id(line)?;
        match doomed.get_mut(&id) {
            Some(remaining) if *remaining > 0 => {
                *remaining -= 1;
                removed += 1;
            }
            _ => kept.push_str(line),
        }
    }
    if removed == 0 {
        return Ok((log, view, 0));
    }

    let marker = canonical_marker("__revert__", runtime, session, what)?;
    append_jsonl(&mut log, &marker);
    append_jsonl(&mut kept, &marker);
    Ok((log, kept, removed))
}

fn canonical_marker(kind: &str, runtime: &str, session: &str, what: &str) -> crate::Result<String> {
    let raw = super::merge::marker_envelope(kind, runtime, session, what);
    let envelope: crate::domain::transcript::Envelope = serde_json::from_str(&raw)?;
    Ok(storage::envelope_line(&envelope))
}

fn append_jsonl(target: &mut String, suffix: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(suffix);
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(c: char) -> String {
        format!(
            "{}{}",
            meta::ID_PREFIX,
            c.to_string().repeat(meta::ID_HEX_LEN)
        )
    }

    fn event(text: &str, session: &str) -> String {
        let raw = super::super::merge::envelope_line(
            &serde_json::json!({"type": "user", "message": {"content": text}}).to_string(),
            "codex",
            session,
        );
        let envelope: crate::domain::transcript::Envelope = serde_json::from_str(&raw).unwrap();
        storage::envelope_line(&envelope)
    }

    #[test]
    fn revert_uses_full_envelope_identity_and_removes_only_selected_multiplicity() {
        let target = claim('a');
        let other_session = claim('b');
        // Same content means the legacy `_object_hash` is identical, but the complete
        // envelopes (and therefore v1 event ids) are distinct across sessions.
        let duplicate = event("same content", &target);
        let same_content_other_session = event("same content", &other_session);
        let log = format!("{duplicate}{same_content_other_session}{duplicate}");
        let view = log.clone();
        let duplicate_id = storage::event_id(&duplicate).unwrap();
        let other_id = storage::event_id(&same_content_other_session).unwrap();
        assert_ne!(duplicate_id, other_id);

        let mut doomed = std::collections::BTreeMap::new();
        add_occurrence(&mut doomed, &duplicate).unwrap();
        let (new_log, new_view, removed) =
            reverted_snapshot(log, view, doomed, "codex", &target, "@#1.1").unwrap();
        assert_eq!(removed, 1);

        let files = storage::snapshot_files(&new_log, &new_view).unwrap();
        let log_ids = storage::parse_sequence(
            std::str::from_utf8(files.get(meta::LOG_FILE).unwrap()).unwrap(),
        )
        .unwrap();
        let view_ids = storage::parse_sequence(
            std::str::from_utf8(files.get(meta::VIEW_FILE).unwrap()).unwrap(),
        )
        .unwrap();

        // LOG keeps both duplicate occurrences and gains only the revert marker.
        assert_eq!(
            log_ids
                .iter()
                .filter(|id| id.as_str() == duplicate_id)
                .count(),
            2
        );
        assert_eq!(log_ids.len(), 4);
        // VIEW drops one occurrence, keeps the other and also keeps the envelope with
        // the same content from the other session. The marker is reachable from LOG.
        assert_eq!(
            view_ids
                .iter()
                .filter(|id| id.as_str() == duplicate_id)
                .count(),
            1
        );
        assert_eq!(
            view_ids.iter().filter(|id| id.as_str() == other_id).count(),
            1
        );
        assert_eq!(view_ids.len(), 3);
        assert_eq!(view_ids.last(), log_ids.last());
        for id in view_ids {
            assert!(files.contains_key(&meta::event_path(&id).unwrap()));
        }
    }

    #[test]
    fn repeated_or_overlapping_coordinates_are_selected_once() {
        let session = claim('a');
        let duplicate = event("same", &session);
        let mut selected = std::collections::BTreeSet::new();
        let mut doomed = std::collections::BTreeMap::new();

        add_occurrence_at(&mut selected, &mut doomed, "head", 7, &duplicate).unwrap();
        // The same coordinate can arrive once through `#n` and again through `#n.k`.
        add_occurrence_at(&mut selected, &mut doomed, "head", 7, &duplicate).unwrap();
        assert_eq!(doomed.values().copied().sum::<usize>(), 1);

        let log = format!("{duplicate}{duplicate}");
        let (new_log, new_view, removed) =
            reverted_snapshot(log.clone(), log, doomed, "codex", &session, "@#1 @#1.1").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(new_log.lines().count(), 3);
        assert_eq!(new_view.lines().count(), 2);
    }

    #[test]
    fn optional_materialization_does_not_turn_claimed_missing_storage_into_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("r")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let claimed = meta::Meta::new(claim('a'), "codex".into(), "/w".into());
        meta::write(repo.root(), &claimed).unwrap();
        repo.add_all().unwrap();
        repo.commit("claimed without storage").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let error =
            materialize_optional(&repo, head.trim(), meta::VIEW_FILE, &claimed).unwrap_err();
        assert!(error.to_string().contains("missing required VIEW"));
    }

    /// Revert draws from the transcript even when the tree also holds a root `LOG` / `VIEW` user
    /// file under the same name.
    ///
    /// v0 does not reserve those two names, so the coexistence is legal. Once a same-named entry
    /// takes over the logical read, [`reverted_snapshot`] gets ordinary text while it needs
    /// `storage::event_id` for every line of the VIEW — `agit revert` then errors out on a
    /// perfectly healthy history. The second half pins that consequence itself.
    #[test]
    fn optional_materialization_never_takes_a_same_named_user_file() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, head, transcript) =
            crate::domain::repo::v0_repo_with_shadowing_user_files(&dir.path().join("r"));
        let snapshot = meta::read_at_ref(&repo, &head).unwrap();

        for which in [meta::LOG_FILE, meta::VIEW_FILE] {
            assert_eq!(
                materialize_optional(&repo, &head, which, &snapshot).unwrap(),
                transcript,
                "the {which} revert draws from must be the transcript"
            );
        }

        // Right source: the whole revert chain runs and the first event really is removed.
        let first = transcript.split_inclusive('\n').next().unwrap();
        let mut doomed = std::collections::BTreeMap::new();
        add_occurrence(&mut doomed, first).unwrap();
        let (_, _, removed) = reverted_snapshot(
            transcript.clone(),
            transcript,
            doomed.clone(),
            &snapshot.runtime,
            &snapshot.session,
            "@#1.1",
        )
        .unwrap();
        assert_eq!(removed, 1);

        // Wrong source: the same chain fails outright on that user file.
        let error = reverted_snapshot(
            crate::domain::repo::V0_SHADOWING_USER_LOG.to_owned(),
            crate::domain::repo::V0_SHADOWING_USER_VIEW.to_owned(),
            doomed,
            &snapshot.runtime,
            &snapshot.session,
            "@#1.1",
        )
        .unwrap_err();
        assert!(error.to_string().contains("envelope"), "{error:#}");
    }
}
