//! `agit branch` — branch listing and governance.
//!
//! Branch names are mere aliases; rename freely. **There is no branch-creation command** —
//! branches are born only from import / fork / new / run: no empty session, no empty branch
//! (PRD).
//!
//! Listing opens no transcript: git refs plus session/meta.json are enough, so it stays instant
//! however far the session count scales.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    /// Show extra columns.
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Include remote-tracking branches.
    #[arg(long)]
    pub all: bool,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Rename. Branch names are mere aliases — rename freely.
    Rename { old: String, new: String },
    /// Delete a local ref (unpushed ones need --force; history published on the hub cannot be deleted).
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Seal: not resumable, only forkable/viewable (hand-off done, distillation source).
    Seal { name: String },
}

/// Filename of the seal marker (committed into the branch tree; its content is the seal notice).
pub const SEAL_FILE: &str = ".agit-sealed";

/// Checked by resume / run: whether this branch is sealed.
pub fn is_sealed(repo: &Repo, branch: &str) -> bool {
    repo.show(&format!("refs/heads/{branch}"), SEAL_FILE)
        .is_some()
}

/// Resolve the context → open the local repo. On failure it prints the reason and returns None.
///
/// Branch operations need only "which repo", which the directory binding answers on its own; they
/// do not require this directory to resolve a current branch as well — a directory fresh from
/// `agit init`, with no session yet, must still be able to list branches.
fn ctx_repo() -> Option<(Repo, String)> {
    let cwd = std::env::current_dir().ok()?;
    let slug = match super::context::repo_for(&cwd) {
        Ok(r) => super::context::qualify(&r),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return None;
        }
    };
    let (owner, name) = super::parse_slug(&slug).ok()?;
    let dir = crate::infra::config::repo_dir(&owner, &name).ok()?;
    match Repo::open(&dir) {
        Some(r) => Some((r, slug)),
        None => {
            ui::error(&format!("{slug} doesn’t exist locally."));
            ui::hint(&format!("fetch it first: `agit clone {slug}`"));
            None
        }
    }
}

pub fn run(args: Args) -> CmdResult {
    match args.cmd {
        Some(Cmd::Rename { old, new }) => {
            let Some((repo, slug)) = ctx_repo() else {
                return Ok(ExitCode::Precondition);
            };
            if !repo.has_ref(&format!("refs/heads/{old}")) {
                ui::error(&format!("{slug} has no branch `{old}`."));
                return Ok(ExitCode::Ref);
            }
            if let Err(e) = crate::domain::repo::valid_branch_name(&new) {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
            repo.git(&["branch", "-m", &old, &new])?;
            super::worktree::rename(&repo, &old, &new)?;
            ui::success(&format!("{old} → {new}"));
            println!(
                "{}",
                ui::dim(
                    "  branch names are mere aliases: session identity, history and tags don’t move"
                )
            );
            Ok(ExitCode::Ok)
        }
        Some(Cmd::Rm { name, force }) => {
            let Some((repo, slug)) = ctx_repo() else {
                return Ok(ExitCode::Precondition);
            };
            if !repo.has_ref(&format!("refs/heads/{name}")) {
                ui::error(&format!("{slug} has no branch `{name}`."));
                return Ok(ExitCode::Ref);
            }
            // Only the local ref is deleted. An unpushed branch is really gone once deleted, so
            // that takes --force; a pushed one gets a line of reassurance (history on the hub is
            // unaffected).
            if !force {
                let pushed = repo.remote("origin").is_some()
                    && repo
                        .git_opt(&[
                            "ls-remote",
                            "--heads",
                            "origin",
                            &format!("refs/heads/{name}"),
                        ])
                        .map(|o| !o.trim().is_empty())
                        == Some(true);
                if pushed {
                    ui::warning("this branch has a copy on the hub — deleting is local-only.");
                } else {
                    ui::error(&format!(
                        "`{name}` was never pushed — deleting destroys it (there may be unshared memory in here)."
                    ));
                    ui::hint(&format!(
                        "to really delete: `agit branch rm {name} --force`"
                    ));
                    ui::hint(&format!("or publish first: `agit push -b {name}`"));
                    return Ok(ExitCode::Precondition);
                }
            }
            // Release its checkout first: git refuses to delete a branch a worktree (or the main
            // checkout) is holding. Uncommitted work in the worktree is dropped only under
            // --force; a branch locked by a merge transaction is not released.
            super::worktree::release(&repo, &name, force)?;
            repo.git(&["branch", "-D", &name])?;
            ui::success(&format!("local branch {name} deleted"));
            println!(
                "{}",
                ui::dim("  published history on the hub is unaffected")
            );
            Ok(ExitCode::Ok)
        }
        Some(Cmd::Seal { name }) => {
            let Some((repo, slug)) = ctx_repo() else {
                return Ok(ExitCode::Precondition);
            };
            if !repo.has_ref(&format!("refs/heads/{name}")) {
                ui::error(&format!("{slug} has no branch `{name}`."));
                return Ok(ExitCode::Ref);
            }
            if is_sealed(&repo, &name) {
                println!("`{name}` is already sealed.");
                return Ok(ExitCode::Ok);
            }
            // Sealing is a commit: the marker lands in the tree (auditable history, propagates
            // with push). Plumbing throughout, leaving the working tree alone — another branch
            // may be checked out right now.
            let head = repo.git(&["rev-parse", &format!("refs/heads/{name}")])?;
            let head = head.trim().to_string();
            let blob = raw_git(
                &repo,
                &["hash-object", "-w", "--stdin"],
                Some(
                    "this branch is sealed (agit branch seal): not resumable, forkable/viewable only.\n",
                ),
            )?;
            let new_tree = with_temp_index(&repo, &head, blob.trim())?;
            let commit = raw_git(
                &repo,
                &[
                    "commit-tree",
                    new_tree.trim(),
                    "-p",
                    &head,
                    "-m",
                    &format!("agit: seal {name}"),
                ],
                None,
            )?;
            let commit = commit.trim().to_string();
            // expected-head CAS: refuse when someone pushed a new commit meanwhile, rather than
            // half-seal.
            repo.git(&["update-ref", &format!("refs/heads/{name}"), &commit, &head])?;
            ui::success(&format!("sealed {name} — fork and view only from now on"));
            Ok(ExitCode::Ok)
        }
        None => {
            let Some((repo, slug)) = ctx_repo() else {
                return Ok(ExitCode::Precondition);
            };
            list(&repo, &slug, args.verbose, args.all)
        }
    }
}

/// Add a blob (`.agit-sealed`) to a commit's tree without touching the working tree.
fn with_temp_index(repo: &Repo, head: &str, blob: &str) -> crate::Result<String> {
    let idx = repo.git_path("agit-seal-index")?;
    let _ = std::fs::remove_file(&idx);
    let run = |args: &[&str]| -> crate::Result<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.root())
            .env("GIT_INDEX_FILE", &idx)
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    run(&["read-tree", head])?;
    run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},{SEAL_FILE}"),
    ])?;
    let tree = run(&["write-tree"])?;
    let _ = std::fs::remove_file(&idx);
    Ok(tree)
}

/// A git call with optional stdin (for `hash-object --stdin` / `commit-tree`).
fn raw_git(repo: &Repo, args: &[&str], stdin: Option<&str>) -> crate::Result<String> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .args(args)
        .current_dir(repo.root())
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn list(repo: &Repo, slug: &str, verbose: bool, all: bool) -> CmdResult {
    let branches = repo.local_branches();
    if branches.is_empty() {
        println!("{slug} has no branches yet.");
        ui::hint(
            "branches are born only via `agit import` / `fork` / `new` / `run` — no empty session, no empty branch",
        );
        return Ok(ExitCode::Ok);
    }
    let cur = repo.current_branch();
    for b in &branches {
        let star = if Some(b) == cur.as_ref() { "*" } else { " " };
        let head = format!("refs/heads/{b}");
        // Session metadata: shape, session short code, runtime, turns, last activity.
        let snap = meta::read_at_ref(repo, &head);
        // An empty session means "this session line has not settled a first turn yet"; it
        // displays as the same dash as "no identity", but the shape column must tell the truth.
        let sid_of = |m: &meta::Meta| {
            if m.session.is_empty() {
                "-".to_string()
            } else {
                m.session.clone()
            }
        };
        let turns = repo
            .git_opt(&["rev-list", "--first-parent", "--count", &head])
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let when = repo
            .git_opt(&["log", "-1", "--format=%cr", &head])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let sealed = if is_sealed(repo, b) { " [sealed]" } else { "" };
        let file_line = match snap.as_ref().map(|m| m.line) {
            Some(meta::Line::File) => " [file line]",
            Some(meta::Line::Session) => "",
            None => " [no line declared]",
        };
        if verbose {
            let ab = repo
                .git_opt(&[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{head}...origin/{b}"),
                ])
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "{star} {b}{sealed}{file_line}\n    session {} · runtime {} · {turns} turns · {when} · ahead/behind {ab}",
                snap.as_ref().map(sid_of).unwrap_or_else(|| "-".into()),
                snap.as_ref()
                    .map(|s| s.runtime.clone())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "-".into()),
            );
            if let Some(code) = snap.and_then(|s| s.code) {
                println!("    code anchor {code}");
            }
        } else {
            let sid = snap
                .as_ref()
                .map(sid_of)
                .map(|s| s[..s.len().min(13)].to_string())
                .unwrap_or_else(|| "-".into());
            println!("{star} {b:<24} {sid} {turns:>4} turns · {when}{sealed}{file_line}");
        }
    }
    if all
        && repo.remote("origin").is_some()
        && let Some(rs) = repo.git_opt(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/remotes/origin/",
        ])
    {
        let extra: Vec<_> = rs
            .lines()
            .filter(|l| !l.is_empty() && *l != "origin/HEAD")
            .collect();
        if !extra.is_empty() {
            println!("\nremote:");
            for r in extra {
                println!("  {r}");
            }
        }
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_file_is_a_stable_name() {
        // The VIEW / exclusion rules, doctor and resume all find the marker by this name;
        // renaming it is a breaking change.
        assert_eq!(SEAL_FILE, ".agit-sealed");
    }
}
