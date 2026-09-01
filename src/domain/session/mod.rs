//! Transcript content inside a repo: listing and selection.
//!
//! # This is not about the store
//!
//! The store holds only links (see [`crate::domain::link`]); list them with `link::list`.
//!
//! This module covers the place that **actually holds transcript content**: the repo. One branch
//! is one session, so the session itself lives at these fixed logical paths (see
//! [`crate::domain::meta`]):
//!
//! ```text
//! session/meta.json   session metadata (including the storage layout version)
//! LOG / VIEW          v1 event-id sequence
//! events/**           v1 full envelope objects
//! ```
//!
//! One session branch is one session, so "listing" is listing by branch: a session line whose
//! tip has readable meta and a claimed identity is one entry. Content is read by ref
//! (`storage::materialize_at`), independent of which branch happens to be checked out. VIEW is
//! never listed — it is what [`crate::domain::transcript`] cuts out of the transcript, a
//! derivative and not a second session.
//!
//! When no branch lists at all, fall back to the checkout root (a v0 repo, a detached legacy
//! checkout): the one entry is there when the current `LOG` (or v0's `session/log.jsonl`) exists
//! and meta reads.

use crate::adapter;
use crate::domain::meta;
use std::path::{Path, PathBuf};

/// Cap on a single object when batch-reading `session/meta.json`: a meta is far below it, and
/// anything over the cap is a corrupt object.
const META_READ_CAP: usize = 1024 * 1024;

/// One materialized session's content.
#[derive(Debug, Clone)]
pub struct Stored {
    /// This branch's session identity: meta's `session` (`agit-` + 40 hex).
    pub id: String,
    pub path: PathBuf,
    pub runtime: String,
    pub mtime: std::time::SystemTime,
    /// The session branch it sits on; content is read by ref. Absent for a direct file path,
    /// and for a fallback listing from the checkout root.
    pub branch: Option<String>,
}

/// List the session content under a repo root: at most one.
pub fn list_in(root: &Path) -> Vec<Stored> {
    let current = root.join(meta::LOG_FILE);
    let legacy = root.join(meta::LEGACY_LOG_FILE);
    let p = if current.is_file() {
        current
    } else if legacy.is_file() {
        legacy
    } else {
        return vec![];
    };
    // A log whose meta does not read, or whose meta has not claimed an identity yet = legacy
    // layout, a partial checkout, a write torn midway; none of them is "content you can select".
    let Some(snap) = meta::read(root).filter(|m| !m.session.is_empty()) else {
        return vec![];
    };
    vec![Stored {
        id: snap.session,
        runtime: snap.runtime,
        mtime: std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        path: p,
        branch: None,
    }]
}

/// List the session content in a repo: one entry per session branch with a claimed identity,
/// newest first.
///
/// Branches whose metadata does not read are not here — [`list_checked`] returns them together
/// with the reason.
pub fn list(repo: &crate::domain::repo::Repo) -> Vec<Stored> {
    list_checked(repo).0
}

/// [`list`] plus the branches whose metadata does not read (one reason each).
///
/// The branch count is the session count, so the whole enumeration starts two git processes: one
/// `for-each-ref` for the tips and their times, one `cat-file --batch` to read
/// `session/meta.json` on every tip — the process count does not grow with the session count.
pub fn list_checked(repo: &crate::domain::repo::Repo) -> (Vec<Stored>, Vec<String>) {
    let mut errors = Vec::new();
    let tips: Vec<(String, String, u64)> = repo
        .git_opt(&[
            "for-each-ref",
            "--format=%(refname:short)%00%(objectname)%00%(committerdate:unix)",
            "refs/heads/",
        ])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let branch = parts.next()?.trim();
            let sha = parts.next()?.trim();
            let secs = parts.next()?.trim().parse::<u64>().ok()?;
            (!branch.is_empty() && !sha.is_empty())
                .then(|| (branch.to_string(), sha.to_string(), secs))
        })
        .collect();

    // Ask `--batch-check` first which tips carry a meta: a branch pushed by plain git may not
    // have `session/meta.json` yet — a legal state, not an error; the body batch reads only the
    // ones that exist.
    let names: Vec<String> = tips
        .iter()
        .map(|(_, sha, _)| format!("{sha}:{}", meta::FILE))
        .collect();
    let mut present: Vec<bool> = Vec::with_capacity(names.len());
    if let Err(error) = repo.git_cat_file_batch_check(names.clone(), |_, kind, _| {
        present.push(kind != "missing");
        Ok(())
    }) {
        errors.push(format!("cannot read session metadata: {error:#}"));
        return (list_in(repo.root()), errors);
    }
    let existing: Vec<String> = names
        .iter()
        .zip(&present)
        .filter(|(_, present)| **present)
        .map(|(name, _)| name.clone())
        .collect();
    let mut read_bodies: Vec<Result<String, String>> = Vec::with_capacity(existing.len());
    let read = repo.git_cat_file_batch(existing, META_READ_CAP, |_, _, body| {
        read_bodies.push(match body {
            crate::domain::repo::ObjectBody::Read(bytes) => String::from_utf8(bytes.to_vec())
                .map_err(|_| format!("{} is not valid UTF-8", meta::FILE)),
            crate::domain::repo::ObjectBody::TooLarge(size) => {
                Err(format!("{} is {size} bytes, over the limit", meta::FILE))
            }
        });
        Ok(())
    });
    if let Err(error) = read {
        errors.push(format!("cannot read session metadata: {error:#}"));
        return (list_in(repo.root()), errors);
    }
    let mut read_bodies = read_bodies.into_iter();
    let bodies: Vec<Option<Result<String, String>>> = present
        .iter()
        .map(|present| present.then(|| read_bodies.next()).flatten())
        .collect();

    let mut out: Vec<Stored> = Vec::new();
    for ((branch, _sha, secs), body) in tips.into_iter().zip(bodies) {
        let refname = format!("refs/heads/{branch}");
        let snap = match body {
            None => continue,
            Some(Err(reason)) => {
                errors.push(format!("branch `{branch}`: {reason}"));
                continue;
            }
            Some(Ok(text)) => match meta::parse_strict(&text, &refname) {
                Ok(snap) => snap,
                Err(error) => {
                    errors.push(format!("branch `{branch}`: {error:#}"));
                    continue;
                }
            },
        };
        if !snap.is_session_line() || snap.session.is_empty() {
            continue;
        }
        out.push(Stored {
            id: snap.session,
            runtime: snap.runtime,
            mtime: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            path: repo.root().join(meta::LOG_FILE),
            branch: Some(branch),
        });
    }
    if out.is_empty() && errors.is_empty() {
        return (list_in(repo.root()), errors);
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.branch.cmp(&b.branch)));
    (out, errors)
}

/// The newest one. At most one exists, so it is that one.
pub fn latest(repo: &crate::domain::repo::Repo) -> Option<Stored> {
    list(repo).into_iter().next()
}

/// Find one by session identity or prefix.
///
/// An ambiguous prefix **must error** rather than take the first — the wrong session resumes the
/// user into a completely unrelated context, and they do not notice right away.
pub fn find(repo: &crate::domain::repo::Repo, selector: &str) -> crate::Result<Stored> {
    let sel = selector.trim();
    if sel.is_empty() {
        anyhow::bail!("session selector must not be empty");
    }

    // A file path is accepted directly (zero-config viewing of one transcript).
    let p = Path::new(sel);
    if p.is_file() {
        let runtime = std::fs::read_to_string(p)
            .ok()
            .and_then(|t| adapter::infer_runtime(&t))
            .unwrap_or("claude-code");
        return Ok(Stored {
            id: p
                .file_stem()
                .map(|s| adapter::session_id_from_stem(&s.to_string_lossy()))
                .unwrap_or_default(),
            path: p.to_path_buf(),
            runtime: runtime.to_string(),
            mtime: std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            branch: None,
        });
    }

    // An exact branch-name match beats an identity prefix: branch names are chosen by a person
    // and get typed straight off `agit log`.
    let all = list(repo);
    if let Some(exact) = all.iter().find(|s| s.branch.as_deref() == Some(sel)) {
        return Ok(exact.clone());
    }
    let matches: Vec<Stored> = all.into_iter().filter(|s| s.id.starts_with(sel)).collect();

    match matches.as_slice() {
        [] => anyhow::bail!("no session matches `{sel}`.\n  `agit log` lists what you have."),
        [only] => Ok(only.clone()),
        // With at most one transcript per repo this arm is unreachable; reaching it means the
        // data is corrupt — error out instead of picking for the user.
        n => anyhow::bail!("`{sel}` matches {} sessions; give a longer prefix", n.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::Meta;
    use crate::domain::repo::Repo;

    /// Build a repo: optionally place a log, a VIEW and a meta.
    fn repo_with(log: bool, view: bool, snap: Option<&str>) -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        meta::ensure_session_dir(r.root()).unwrap();
        if log {
            std::fs::write(r.root().join(meta::LOG_FILE), "{}").unwrap();
        }
        if view {
            std::fs::write(r.root().join(meta::VIEW_FILE), "{}").unwrap();
        }
        if let Some(session) = snap {
            crate::domain::meta::write(
                r.root(),
                &Meta::new(session.into(), "codex".into(), "/r".into()),
            )
            .unwrap();
        }
        (d, r)
    }

    fn claim() -> String {
        format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN))
    }

    /// A repo's content is that one branch's log: with the log and the meta present, the answer
    /// is exactly one.
    #[test]
    fn the_branch_log_is_the_only_listable_content() {
        let claim = claim();
        let (_d, r) = repo_with(true, true, Some(&claim));
        let all = list(&r);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, claim);
        assert_eq!(
            all[0].runtime, "codex",
            "runtime comes from meta, not guessed from content"
        );
        assert!(all[0].path.ends_with(meta::LOG_FILE), "{:?}", all[0].path);
        assert_eq!(latest(&r).unwrap().id, claim);
    }

    /// VIEW is a derivative and is never treated as a selectable session.
    #[test]
    fn the_view_file_is_never_listed() {
        let claim = claim();
        let (_d, r) = repo_with(true, true, Some(&claim));
        assert!(
            list(&r).iter().all(|s| s.path.ends_with(meta::LOG_FILE)),
            "VIEW must not appear in a listing"
        );
    }

    /// Without readable meta a log has no identity to belong to — the legacy layout
    /// (`sessions/<runtime>/<id>.jsonl`) and a meta-only repo both list nothing.
    #[test]
    fn anything_but_the_branch_pair_lists_nothing() {
        let (_d, r) = repo_with(true, false, None);
        assert!(list(&r).is_empty(), "a log without meta has no identity");

        let (_d, r) = repo_with(false, false, Some(&claim()));
        assert!(list(&r).is_empty(), "meta alone is not content");

        let (_d, r) = repo_with(false, false, None);
        std::fs::create_dir_all(r.root().join("sessions/codex")).unwrap();
        std::fs::write(r.root().join("sessions/codex/AB.jsonl"), "{}").unwrap();
        assert!(
            list(&r).is_empty(),
            "the legacy sessions/ layout is not recognized"
        );
    }

    /// A file line has meta but never a log, so it lists nothing; a newborn session line is the
    /// same (no identity claimed yet, and its log does not exist yet either).
    #[test]
    fn a_file_line_lists_nothing() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        crate::domain::meta::write(r.root(), &Meta::new_file_line()).unwrap();
        assert!(list(&r).is_empty());

        // Even when someone drops a log onto the file line, a meta with no identity must not
        // produce an unnamed entry.
        std::fs::write(r.root().join(meta::LOG_FILE), "{}").unwrap();
        assert!(
            list(&r).is_empty(),
            "with no claimed identity there is no selectable content"
        );
    }

    /// The selector matches a prefix of the branch's session identity (`agit-...`).
    #[test]
    fn the_selector_prefix_matches_the_claim() {
        let claim = claim();
        let (_d, r) = repo_with(true, false, Some(&claim));
        let prefix: String = claim.chars().take(10).collect();
        assert_eq!(find(&r, &prefix).unwrap().id, claim);

        let e = find(&r, "agit-0000").unwrap_err().to_string();
        assert!(
            e.contains("agit log"),
            "the error message gives the next step: {e}"
        );
        assert!(find(&r, "").is_err(), "an empty selector must error");
    }

    /// One session per ref: two session lines list one entry each, newest first, and the file
    /// line does not count; corrupt metadata is reported separately instead of vanishing.
    #[test]
    fn branches_are_listed_newest_first_and_corrupt_metadata_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(r.root(), &Meta::new_file_line()).unwrap();
        r.add_all().unwrap();
        r.commit("init").unwrap();
        let session_at = |branch: &str, id_char: &str, date: &str| {
            r.git(&["checkout", "--quiet", "-b", branch, "main"])
                .unwrap();
            let mut snap = Meta::new_session_line("codex".into(), "/w".into());
            snap.session = format!("{}{}", meta::ID_PREFIX, id_char.repeat(meta::ID_HEX_LEN));
            meta::write(r.root(), &snap).unwrap();
            r.add_all().unwrap();
            let out = std::process::Command::new("git")
                .args(["-C"])
                .arg(r.root())
                .args(["commit", "--quiet", "-m", branch])
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_AUTHOR_DATE", date)
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        session_at("older", "a", "2024-01-01T00:00:00Z");
        session_at("newer", "b", "2024-02-01T00:00:00Z");
        r.git(&["checkout", "--quiet", "-b", "broken", "main"])
            .unwrap();
        std::fs::write(meta::path_in(r.root()), "{not json").unwrap();
        r.add_all().unwrap();
        r.commit("broken").unwrap();
        // A branch pushed by plain git: no session/meta.json yet. Legal, skipped, and it does
        // not drag the rest down.
        r.git(&["checkout", "--quiet", "-b", "bare", "main"])
            .unwrap();
        r.git(&["rm", "--quiet", meta::FILE]).unwrap();
        r.commit("no metadata yet").unwrap();
        r.git(&["checkout", "--quiet", "main"]).unwrap();

        let (listed, errors) = list_checked(&r);
        let branches: Vec<_> = listed.iter().map(|s| s.branch.clone().unwrap()).collect();
        assert_eq!(
            branches,
            ["newer", "older"],
            "session lines only, newest first"
        );
        assert!(listed[0].id.starts_with("agit-bbbb"));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("broken"), "{errors:?}");
        assert_eq!(find(&r, "older").unwrap().branch.as_deref(), Some("older"));
    }

    /// A direct file path still resolves (zero-config viewing of one transcript).
    #[test]
    fn a_direct_file_path_still_resolves() {
        let (_d, r) = repo_with(false, false, None);
        let p = r.root().join("anywhere.jsonl");
        std::fs::write(&p, "{}").unwrap();
        let got = find(&r, p.to_str().unwrap()).unwrap();
        assert_eq!(got.path, p);
        assert_eq!(got.id, "anywhere");
    }
}
