//! `agit cherry-pick` — lightweight picking with no agent.
//!
//! Picks specific turns or events from another branch into the target branch's **VIEW** and
//! lands one view commit. Objects travel along (into the target branch's log), and the source
//! is marked automatically (a `__cherry_pick_start__/__cherry_pick_end__` pair wraps the picked
//! stretch, and a picked event keeps its original envelope — `_session_id` is the source
//! marking).
//!
//! Refused while a merge transaction is open (`agit merge pick` is the way there).

use super::CmdResult;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Turn-level refs: `<ref>#n`, `<ref>#a..#b` or `<ref>#n.k` — repeatable.
    pub picks: Vec<String>,
    /// Target branch (default: the context branch).
    #[arg(long, value_name = "owner/repo@branch")]
    pub into: Option<String>,
    /// Note (goes into the commit message).
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

pub fn run(args: Args) -> CmdResult {
    if args.picks.is_empty() {
        ui::error("nothing to pick was given.");
        ui::hint("e.g.: agit cherry-pick try-b#3..#5 try-b#8.2");
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
        (repo, args.into.clone().unwrap_or(ctx.branch))
    };

    // Only the locked branch is blocked: the transaction CASes the head of its target branch,
    // and a cherry-pick into any other branch never touches that head, so there is no reason to
    // freeze it too.
    if let Some(tx) = crate::domain::mergetx::locking(repo.root(), &target) {
        ui::error(&format!(
            "`{}` is inside an open merge transaction — cherry-pick is disabled on it; \
             use `agit merge pick` within the transaction.",
            tx.target
        ));
        ui::hint("`agit merge --status` shows progress");
        return Ok(ExitCode::Precondition);
    }
    if super::branch::is_sealed(&repo, &target) {
        ui::error(&format!("`{target}` is sealed."));
        return Ok(ExitCode::Policy);
    }

    // Gather the envelope lines to pick.
    let mut lines: Vec<String> = vec![];
    for p in &args.picks {
        match expand_one(&repo, p)? {
            Some(ls) => lines.extend(ls),
            None => return Ok(ExitCode::Ref),
        }
    }

    // Markers and picked events are both ordinary events, so both are appended to LOG and to
    // VIEW. snapshot_files encodes the full envelope into an id sequence plus events/ objects,
    // so a cross-repo source never depends on a blob that happens to already sit in the target
    // object database.
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
        ui::error(&format!(
            "`{target}` is the file line — no VIEW to pick into."
        ));
        return Ok(ExitCode::Precondition);
    }
    let what = args.picks.join(" ");
    let log = materialize_optional(&repo, &head, meta::LOG_FILE, &snap)?;
    let view = materialize_optional(&repo, &head, meta::VIEW_FILE, &snap)?;
    let (log, view) = picked_snapshot(log, view, &lines, &snap.runtime, &snap.session, &what)?;

    let mut s = snap.clone();
    s.kind = meta::Kind::View;
    s.layout = meta::LayoutVersion::CURRENT;
    let snap_text = meta::to_text(&s)?;
    let tree = super::plumbing::session_snapshot_tree(&repo, &head, &log, &view, &snap_text)?;
    let msg = format!(
        "agit: cherry-pick {}\n\n{}",
        what,
        args.message.clone().unwrap_or_default()
    );
    let commit = super::plumbing::commit_tree(&repo, &tree, &[&head], &msg)?;
    super::plumbing::update_branch_cas_and_refresh(&repo, &target, &commit, &head, false)?;

    ui::success(&format!(
        "picked {} events → {target} (view commit {})",
        lines.len(),
        &commit[..9.min(commit.len())]
    ));
    Ok(ExitCode::Ok)
}

/// Expand one turn-level ref into the matching envelope lines of that source branch's transcript.
fn expand_one(target_repo: &Repo, p: &str) -> crate::Result<Option<Vec<String>>> {
    let spec = refs::parse(p)?;
    // Source repo: with an explicit owner/repo@..., it may be a different local repo.
    let (repo, head) = match &spec.repo {
        refs::RepoSel::Slug(o, n) => {
            match Repo::open(crate::infra::config::repo_dir(o, n)?) {
                Some(r) => {
                    // Source branch head: the base left once the tail is stripped.
                    let head = match &spec.base {
                        refs::Base::Name(b) => r
                            .git(&["rev-parse", &format!("refs/heads/{b}")])
                            .map(|s| s.trim().to_string())?,
                        _ => {
                            ui::error(
                                "cherry-pick sources must name a branch (`owner/repo@branch#n`).",
                            );
                            return Ok(None);
                        }
                    };
                    (r, head)
                }
                None => {
                    ui::error(&format!("{o}/{n} doesn’t exist locally."));
                    ui::hint(&format!("fetch it first: `agit fetch {o}/{n}`"));
                    return Ok(None);
                }
            }
        }
        refs::RepoSel::Context if spec.base == refs::Base::At => {
            // `@` belongs to the session that supplied the source ref, not to
            // the target repo selected by `--into`.  Resolve both repo and
            // branch together before opening the source history.
            let (repo, branch) = match context_source() {
                Ok(v) => v,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(None);
                }
            };
            let head = repo
                .git(&["rev-parse", &format!("refs/heads/{branch}")])
                .map(|s| s.trim().to_string())?;
            (repo, head)
        }
        _ => {
            let head = match &spec.base {
                refs::Base::Name(b) => target_repo
                    .git(&["rev-parse", &format!("refs/heads/{b}")])
                    .map(|s| s.trim().to_string())?,
                refs::Base::At => match super::context::substitute_at(spec.clone()) {
                    Ok(refs::RefSpec {
                        base: refs::Base::Name(b),
                        ..
                    }) => target_repo
                        .git(&["rev-parse", &format!("refs/heads/{b}")])
                        .map(|s| s.trim().to_string())?,
                    Ok(_) => unreachable!("`@` is substituted into a branch name"),
                    Err(e) => {
                        ui::error(&format!("{e:#}"));
                        return Ok(None);
                    }
                },
                refs::Base::Default => {
                    ui::error("a branch name is needed, e.g. `B#3`.");
                    return Ok(None);
                }
            };
            (Repo::at(target_repo.root()), head)
        }
    };

    let log = match storage::materialize_at(repo.root(), &head, meta::LOG_FILE) {
        Ok(log) => log,
        Err(error) => {
            ui::error(&format!("cannot read the source LOG for `{p}`: {error:#}"));
            return Ok(None);
        }
    };
    let mut out = vec![];
    match &spec.tail {
        refs::Tail::Turn(n) => {
            let n = crate::domain::refs::real_turn(&repo, &head, *n)?;
            for i in super::merge::turn_lines(&repo, &head, n)? {
                out.push(line_at(&log, i)?);
            }
        }
        refs::Tail::Range { a, b } => {
            let (a, b) = (
                crate::domain::refs::real_turn(&repo, &head, *a)?,
                crate::domain::refs::real_turn(&repo, &head, *b)?,
            );
            for n in a..=b {
                for i in super::merge::turn_lines(&repo, &head, n)? {
                    out.push(line_at(&log, i)?);
                }
            }
        }
        refs::Tail::Event { turn, index } => {
            let n = crate::domain::refs::real_turn(&repo, &head, *turn)?;
            let ls = super::merge::turn_lines(&repo, &head, n)?;
            match ls.get((*index as usize).saturating_sub(1)) {
                Some(&i) => {
                    out.push(line_at(&log, i)?);
                }
                None => {
                    ui::error(&format!("`{p}`: turn {n} has no event {index}."));
                    return Ok(None);
                }
            }
        }
        _ => {
            ui::error(&format!("`{p}` is not a turn-level ref."));
            return Ok(None);
        }
    }
    Ok(Some(out))
}

fn context_source() -> crate::Result<(Repo, String)> {
    // Repo and branch must come out of one read — an identity must not be split into two
    // observations.
    let ctx = super::context::at_context()?;
    let branch = ctx.branch.clone();
    let (owner, name) = super::parse_slug(&ctx.repo)?;
    let repo_dir = crate::infra::config::repo_dir(&owner, &name)?;
    let repo = Repo::open(&repo_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "{} doesn’t exist locally; fetch it first: `agit clone {}`",
            ctx.repo,
            ctx.repo
        )
    })?;
    Ok((repo, branch))
}

/// Line i (0-based) of a materialized transcript, keeping the trailing LF that the event-id
/// covers.
fn line_at(log: &str, i: usize) -> crate::Result<String> {
    log.split_inclusive('\n')
        .nth(i)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("selected LOG coordinate {i} is out of bounds"))
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

fn picked_snapshot(
    mut log: String,
    mut view: String,
    picked: &[String],
    runtime: &str,
    session: &str,
    what: &str,
) -> crate::Result<(String, String)> {
    let mut addition = canonical_marker("__cherry_pick_start__", runtime, session, what)?;
    for line in picked {
        addition.push_str(line.strip_suffix('\n').unwrap_or(line));
        addition.push('\n');
    }
    addition.push_str(&canonical_marker(
        "__cherry_pick_end__",
        runtime,
        session,
        what,
    )?);
    append_jsonl(&mut log, &addition);
    append_jsonl(&mut view, &addition);
    Ok((log, view))
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
    fn picked_events_and_markers_are_reachable_from_log_and_view() {
        let target = claim('a');
        let source = claim('b');
        let existing = event("existing", &target);
        let picked = event("picked", &source);
        let (log, view) = picked_snapshot(
            existing.clone(),
            existing,
            &[picked.clone(), picked],
            "codex",
            &target,
            "source#2",
        )
        .unwrap();

        // Existing + start + two selected occurrences + end. Repeated occurrences
        // stay in both sequences even though snapshot_files stores one event object.
        assert_eq!(log.split_inclusive('\n').count(), 5);
        assert_eq!(view, log);
        let files = storage::snapshot_files(&log, &view).unwrap();
        let log_ids = storage::parse_sequence(
            std::str::from_utf8(files.get(meta::LOG_FILE).unwrap()).unwrap(),
        )
        .unwrap();
        let view_ids = storage::parse_sequence(
            std::str::from_utf8(files.get(meta::VIEW_FILE).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(log_ids, view_ids);
        assert_eq!(log_ids[2], log_ids[3], "selected multiplicity must survive");
        for id in log_ids {
            assert!(files.contains_key(&meta::event_path(&id).unwrap()));
        }
    }

    #[test]
    fn selected_log_coordinates_must_exist() {
        let error = line_at("one\n", 1).unwrap_err();
        assert!(error.to_string().contains("out of bounds"));
    }

    #[test]
    fn optional_materialization_only_allows_an_unclaimed_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("r")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

        let root = meta::Meta::new_session_line("codex".into(), "/w".into());
        meta::write(repo.root(), &root).unwrap();
        repo.add_all().unwrap();
        repo.commit("unclaimed root").unwrap();
        let root_commit = repo.git(&["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            materialize_optional(&repo, root_commit.trim(), meta::LOG_FILE, &root).unwrap(),
            ""
        );

        let claimed = meta::Meta::new(claim('a'), "codex".into(), "/w".into());
        meta::write(repo.root(), &claimed).unwrap();
        repo.add_all().unwrap();
        repo.commit("claimed without storage").unwrap();
        let claimed_commit = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let error = materialize_optional(&repo, claimed_commit.trim(), meta::LOG_FILE, &claimed)
            .unwrap_err();
        assert!(error.to_string().contains("missing required LOG"));

        std::fs::write(repo.root().join(meta::LOG_FILE), [0xff]).unwrap();
        repo.add_all().unwrap();
        repo.commit("non UTF-8 storage").unwrap();
        let corrupt_commit = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let error = materialize_optional(&repo, corrupt_commit.trim(), meta::LOG_FILE, &claimed)
            .unwrap_err();
        assert!(error.to_string().contains("not UTF-8"));
    }

    /// This pins that a pick sources the transcript even when the tree also carries a root
    /// user file named `LOG` / `VIEW`.
    ///
    /// v0 lets the two coexist. Once a same-named entry steals the logical read,
    /// `materialize_optional` hands back plain text whose every line then has to parse as an
    /// Envelope — and `agit cherry-pick` fails on a completely healthy history.
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
                "the picked {which} must be the transcript"
            );
        }

        // What taking the wrong one costs: that user file cannot compute even one event id.
        assert!(storage::event_id(crate::domain::repo::V0_SHADOWING_USER_LOG).is_err());
    }
    #[test]
    fn at_source_uses_the_context_repo_when_target_has_the_same_branch_name() {
        let home = tempfile::tempdir().unwrap();
        let target_path = home.path().join("repos/alice/payments");
        let source_path = home.path().join("repos/bob/notes");
        let target = Repo::init(&target_path).unwrap();
        let source = Repo::init(&source_path).unwrap();

        let _env = crate::infra::config::env_lock();
        let old_home = std::env::var_os("AGIT_HOME");
        let old_session = std::env::var_os("AGIT_SESSION");
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
            std::env::set_var("AGIT_SESSION", "bob/notes@main");
        }
        let selected = context_source().unwrap();
        unsafe {
            match old_home {
                Some(value) => std::env::set_var("AGIT_HOME", value),
                None => std::env::remove_var("AGIT_HOME"),
            }
            match old_session {
                Some(value) => std::env::set_var("AGIT_SESSION", value),
                None => std::env::remove_var("AGIT_SESSION"),
            }
        }

        assert_eq!(selected.1, "main");
        assert_eq!(selected.0.root(), source.root());
        assert_ne!(selected.0.root(), target.root());
    }
}
