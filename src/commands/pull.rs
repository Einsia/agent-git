//! `agit pull` — fetch + fast-forward only.
//!
//! A non-fast-forward means the same session continued in two places — a **true divergence**.
//! Each branch reports how far ahead either side is and prints the two ways out verbatim
//! (reconcile with the merge agent / fork to keep both lines). Transcripts are never merged
//! automatically, and the runtime is never materialized (PRD, "Remote" section).
//!
//! # The two ways out must actually run
//!
//! In the reference syntax `a/b` is `owner/repo` (see [`crate::domain::refs`]), so
//! `agit merge origin/refund-fix --into refund-fix` looks `origin` up as a username and ends at
//! "this machine has no origin/refund-fix". A command printed inside an error that fails when
//! typed back is worse than printing nothing: it sends the reader into a second layer of
//! confusion.
//!
//! So what gets printed is **the commit id on the remote side**. A sha prefix is a form the
//! reference syntax already accepts (four hex digits or more), so `agit merge <sha> --into <b>`
//! and `agit fork <sha> -b <b>-remote` both resolve straight to the point `origin/<b>` is at.

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Target: `owner/repo@branch` (legacy repo plus `-b` is also accepted).
    #[arg(value_name = "owner/repo@branch")]
    pub repo: Option<String>,
    /// Branches to pull (repeatable). Default: the context branch, or all fast-forwardable branches.
    #[arg(short = 'b', long)]
    pub branch: Vec<String>,
    /// Pull all branches.
    #[arg(long, conflicts_with = "branch")]
    pub all: bool,
    /// Also delete tracking refs gone upstream (fetch --prune already happens at fetch time).
    #[arg(long)]
    pub prune: bool,
}

pub fn run(args: Args) -> CmdResult {
    let _ = args.prune; // fetch already carries --prune; this flag is reserved for selective prune.

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let (repo_arg, target_branch) = match args.repo.as_deref() {
        Some(raw) => match split_pull_target(raw) {
            Ok(v) => v,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        },
        None => (None, None),
    };
    if target_branch.is_some() && (!args.branch.is_empty() || args.all) {
        ui::error("a branch in `<owner>/<repo>@<branch>` cannot be combined with `-b` or `--all`.");
        return Ok(ExitCode::Usage);
    }
    let (owner, name, ctx_branch) = match resolve_targets(&repo_arg, &cwd) {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!("{owner}/{name} doesn’t exist locally."));
        ui::hint(&format!("fetch it first: `agit clone {owner}/{name}`"));
        return Ok(ExitCode::Precondition);
    };
    if repo.remote("origin").is_none() {
        ui::error("this repo has no origin remote (never pushed / purely local).");
        ui::hint("create the remote first with `agit push`");
        return Ok(ExitCode::Precondition);
    }

    // 1. fetch.
    let client = crate::hub::Client::from_env();
    if let Err(e) = crate::hub::identity::verify_slug(&repo, &client, &owner, &name) {
        ui::error(&format!("refusing to fetch: {e:#}"));
        return Ok(ExitCode::Precondition);
    }
    let out = crate::hub::git::run(&repo, &["fetch", "origin", "--prune", "--tags"])?;
    if !out.ok() {
        ui::error(&format!("fetch failed: {}", out.stderr.trim()));
        ui::hint("network or auth. Check credentials with `agit whoami --check`");
        return Ok(ExitCode::Network);
    }

    // 2. Fast-forward branch by branch.
    let want: Vec<String> = if let Some(b) = target_branch {
        vec![b]
    } else if args.all {
        repo.branches()
    } else if !args.branch.is_empty() {
        args.branch
    } else if let Some(b) = ctx_branch {
        vec![b]
    } else {
        repo.branches()
    };

    let mut diverged = false;
    for b in &want {
        if !repo.has_ref(&format!("refs/heads/{b}")) {
            ui::warning(&format!("no local branch `{b}` — skipped"));
            continue;
        }
        if !repo.has_ref(&format!("refs/remotes/origin/{b}")) {
            println!("{b} has no upstream counterpart — skipped (local line)");
            continue;
        }
        match repo.git_opt(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{b}...origin/{b}"),
        ]) {
            Some(counts) => {
                let mut it = counts.split_whitespace();
                let ahead: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let behind: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                match (ahead, behind) {
                    (0, 0) => println!("{b} is up to date"),
                    (0, behind) => match fast_forward(&repo, b) {
                        Ok(()) => println!("{b} fast-forwarded +{behind}"),
                        Err(e) => ui::warning(&format!("{b}: fast-forward failed: {e:#}")),
                    },
                    (ahead, 0) => println!("{b} is {ahead} turns ahead — publish with `agit push`"),
                    (ahead, behind) => {
                        diverged = true;
                        ui::error(&format!(
                            "{b} truly diverged: this session continued in two places (local +{ahead} / remote +{behind})."
                        ));
                        match remote_point(&repo, b) {
                            Some(sha) => {
                                eprintln!("  two ways out (origin/{b} is at {sha}):");
                                for line in ways_out(b, &sha) {
                                    eprintln!("    {line}");
                                }
                            }
                            // With no point to name, no command can be printed — printing one
                            // that does not resolve is worse than printing nothing.
                            None => ui::hint(&format!(
                                "couldn’t read where origin/{b} points; `agit fetch` again, then re-run"
                            )),
                        }
                        println!(
                            "{}",
                            ui::dim(
                                "  transcripts are never auto-merged — only an agent holding both contexts can spot intent collisions"
                            )
                        );
                    }
                }
            }
            None => ui::warning(&format!("{b} and origin/{b} can’t be compared")),
        }
    }

    if diverged {
        return Ok(ExitCode::Precondition);
    }
    Ok(ExitCode::Ok)
}

/// Normalize `owner/repo@branch` while retaining the old `owner/repo -b branch`
/// spelling for scripts.
fn split_pull_target(raw: &str) -> crate::Result<(Option<String>, Option<String>)> {
    let parsed = crate::commands::target::parse(raw)?;
    if parsed.tail != crate::domain::refs::Tail::None {
        anyhow::bail!("pull accepts a branch target, not a historic selector: `{raw}`");
    }
    let repo = parsed
        .repo
        .ok_or_else(|| anyhow::anyhow!("pull target must name a repository"))?;
    let repo = if repo.contains('/') {
        repo
    } else {
        let owner = crate::infra::credentials::current_user()
            .ok_or_else(|| anyhow::anyhow!("a bare repo target needs a signed-in owner"))?;
        format!("{owner}/{repo}")
    };
    let branch = match parsed.base.as_deref() {
        None => None,
        Some("@") => anyhow::bail!("pull target must name a branch explicitly"),
        Some(b) => Some(b.to_string()),
    };
    Ok((Some(repo), branch))
}

/// Where the remote side lands, abbreviated to a sha prefix the reference syntax accepts.
///
/// A sha and not `origin/<b>`: reference resolution reads the latter as `owner/repo`. git picks
/// the abbreviation length (`--short` guarantees uniqueness inside this repo); its shortest form
/// is seven hex digits, well past the four a sha prefix requires.
fn remote_point(repo: &Repo, branch: &str) -> Option<String> {
    let short = repo
        .git_opt(&[
            "rev-parse",
            "--short",
            &format!("refs/remotes/origin/{branch}"),
        ])
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= 4);
    short.or_else(|| {
        repo.git_opt(&["rev-parse", &format!("refs/remotes/origin/{branch}")])
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() >= 4)
    })
}

/// The two ways out of a divergence, each one a command that pastes straight into a terminal.
///
/// A separate function so a test can take the command text and resolve it — "the printed command
/// runs" is proven only by joining both ends.
fn ways_out(branch: &str, sha: &str) -> Vec<String> {
    vec![
        format!("reconcile: `agit merge {sha} --into {branch}` (the merge agent)"),
        format!("keep both: `agit fork {sha} -b {branch}-remote --resume`"),
    ]
}

/// Fast-forward a branch to origin/<branch>.
///
/// The branch currently checked out goes through `merge --ff-only` (the index and the worktree
/// move with it — the worktree holds this session's transcript files); every other branch only
/// needs its ref moved.
fn fast_forward(repo: &Repo, branch: &str) -> crate::Result<()> {
    fast_forward_to(repo, branch, &format!("refs/remotes/origin/{branch}"))
}

/// Advance a local branch to a resolved remote-tracking point with expected-head CAS.
///
/// The active branch uses the same transactional checkout refresher as storage migration: a v0
/// namespace collision is rejected before any ref or worktree byte moves, and a failed postflight
/// restores both the checkout and branch ref. Inactive branches still receive an expected-old CAS
/// instead of an unconditional update-ref.
pub(super) fn fast_forward_to(repo: &Repo, branch: &str, target: &str) -> crate::Result<()> {
    let local_ref = format!("refs/heads/{branch}");
    let old = repo
        .git(&["rev-parse", "--verify", &format!("{local_ref}^{{commit}}")])?
        .trim()
        .to_owned();
    let new = repo
        .git(&["rev-parse", "--verify", &format!("{target}^{{commit}}")])?
        .trim()
        .to_owned();
    if old == new {
        return Ok(());
    }
    repo.git(&["merge-base", "--is-ancestor", &old, &new])?;
    // The index and worktree refresh in whichever worktree has the branch checked out; a branch
    // nobody has checked out only moves its ref.
    let checkout = super::worktree::existing(repo, branch)?.unwrap_or_else(|| repo.clone());
    super::plumbing::update_branch_cas_and_refresh(&checkout, branch, &new, &old, true)
}

/// Decide the repo and the context branch.
fn resolve_targets(
    repo_arg: &Option<String>,
    cwd: &std::path::Path,
) -> Result<(String, String, Option<String>), ExitCode> {
    match repo_arg {
        Some(slug) => match super::parse_slug(slug) {
            Ok((o, n)) => Ok((o, n, None)),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                Err(ExitCode::Usage)
            }
        },
        None => match super::context::resolve(cwd) {
            Ok(c) => match c.owner_name() {
                Ok((o, n)) => Ok((o, n, Some(c.branch))),
                Err(_) => Err(ExitCode::Usage),
            },
            Err(e) => {
                ui::error(&format!("{e:#}"));
                ui::hint("or name the repo: `agit pull <owner/repo> --all`");
                Err(ExitCode::Ref)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::refs;

    /// This pins that both commands printed at a divergence actually resolve.
    ///
    /// End to end: the printed commands go back through [`refs`], and the ref they name must
    /// resolve to the commit on the remote side. A plausible wrong implementation escapes it by
    /// printing `origin/<b>`, which the reference syntax reads as `owner/repo` (`origin` as the
    /// owner) — both ways out then exit 3 when typed back.
    #[test]
    fn the_two_ways_out_are_commands_that_actually_resolve() {
        let dir = std::env::temp_dir().join(format!(
            "agit-pull-diverge-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = Repo::init(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "base").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "base"]).unwrap();
        repo.git(&["checkout", "--quiet", "-b", "refund-fix"])
            .unwrap();
        // The remote side: build it first, then take the local line one different step from the
        // same fork point.
        std::fs::write(dir.join("a.txt"), "remote side").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "remote turn"]).unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/refund-fix", "refund-fix"])
            .unwrap();
        let remote_sha = repo
            .git(&["rev-parse", "refs/remotes/origin/refund-fix"])
            .unwrap();
        repo.git(&["reset", "--hard", "--quiet", "HEAD~1"]).unwrap();
        std::fs::write(dir.join("a.txt"), "local side").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "local turn"]).unwrap();

        let sha = remote_point(&repo, "refund-fix").expect("the remote point must be readable");
        let lines = ways_out("refund-fix", &sha);
        assert!(
            lines[0].contains(&format!("agit merge {sha} --into refund-fix")),
            "{lines:?}"
        );
        assert!(
            lines[1].contains(&format!("agit fork {sha} -b refund-fix-remote")),
            "{lines:?}"
        );

        // The ref inside the command resolves, and it resolves to the commit on the remote side.
        let spec = refs::parse(&sha).unwrap();
        let got = refs::resolve(&repo, &spec).expect("a printed ref must resolve");
        assert_eq!(got.sha, remote_sha.trim());

        // The `origin/<b>` spelling parses as `owner/repo` — that is why it cannot run, and why
        // going back to it walks into the same failure.
        let old = refs::parse("origin/refund-fix").unwrap();
        assert_eq!(
            old.repo,
            refs::RepoSel::Slug("origin".into(), "refund-fix".into()),
            "`origin/<b>` is owner/repo, not a remote-tracking branch"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_v0_fast_forward_refuses_ignored_v1_namespace_collision() {
        use crate::domain::meta::{self, LayoutVersion, Meta};

        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut old_meta = Meta::new_file_line();
        old_meta.layout = LayoutVersion::V0;
        meta::write(repo.root(), &old_meta).unwrap();
        std::fs::write(repo.root().join(".gitignore"), "/LOG\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 main").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let born = Meta::new_session_line("codex".into(), "/work".into());
        let tree = crate::commands::new::fresh_session_tree(
            &repo,
            old.trim(),
            &meta::to_text(&born).unwrap(),
        )
        .unwrap();
        let remote =
            crate::commands::plumbing::commit_tree(&repo, &tree, &[old.trim()], "remote v1")
                .unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/main", &remote])
            .unwrap();

        let user_bytes = b"ignored user LOG\n";
        std::fs::write(repo.root().join(meta::LOG_FILE), user_bytes).unwrap();
        let error = fast_forward_to(&repo, "main", "refs/remotes/origin/main").unwrap_err();
        assert!(error.to_string().contains("user data"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).unwrap(), old);
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert_eq!(
            std::fs::read(repo.root().join(meta::LOG_FILE)).unwrap(),
            user_bytes
        );
    }
}
