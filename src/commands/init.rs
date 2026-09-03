//! `agit init` — create a repo: the main file line + a binding for the current directory.
//!
//! main is the **file line** (PRD "the two branch forms"): a branch the scaffolding produces
//! that never claims a session, accepts only file commits and cannot be resumed — the trunk of
//! team memory. `agit new` starts a session off it and inherits memory/skills; merging a session
//! branch back into it is distillation.
//!
//! Usable while signed out (owner is recorded as `local` for now and put right at the first
//! push). `--seed` folds the project's existing assets in, **listed and confirmed item by item
//! before anything enters the repo** — personal memory can carry private content and is never
//! collected silently.

use super::CmdResult;
use crate::domain::meta::{self, Meta};
use crate::domain::repo::{self, Repo};
use crate::domain::storage;
use crate::domain::workspace;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// Repo name. Omitted: the directory name is only a suggestion — typing it yourself is what makes it real.
    pub name: Option<String>,
    /// Fold the project’s existing assets (AGENTS.md, CLAUDE.md, .claude/skills/, ...)
    /// into main as its first file commit (confirmed item by item).
    #[arg(long)]
    pub seed: bool,
    /// Record that the first `agit push` publishes private (overridable with `--public` then).
    /// Visibility is settled at first publish only; this just records the intent in the repo.
    #[arg(long)]
    pub private: bool,
    /// Don’t bind the current directory.
    #[arg(long)]
    pub no_bind: bool,
    /// Bind this directory even if it is already bound to another repo.
    #[arg(long, conflicts_with = "no_bind")]
    pub rebind: bool,

    /// Asset choices made by the TUI. `None` means use the ordinary `--seed` policy; `Some`
    /// carries the exact item-by-item confirmation and must not ask a second time.
    #[arg(skip)]
    pub(crate) seed_assets: Option<Vec<(PathBuf, PathBuf)>>,
}

pub fn run(args: Args) -> CmdResult {
    let mut args = args;
    let cwd = std::env::current_dir()?;

    if wants_tui(&args) {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                let Some(picked) = crate::tui::screens::initialize::pick(&cwd)? else {
                    return Ok(ExitCode::Ok);
                };
                args.name = Some(picked.name);
                args.no_bind = !picked.bind;
                args.seed = picked.seed_assets.is_some();
                args.seed_assets = picked.seed_assets;
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }

    // Name: an explicit argument > under a tty, the directory name as a suggestion that is
    // retyped to confirm > an error without a tty.
    let name = match args.name {
        Some(n) => match repo::valid_name(&n) {
            Ok(()) => n,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        },
        None => {
            let suggestion = cwd
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            match ui::prompt::input(&format!("repo name (suggestion: {suggestion})"), None) {
                Ok(Some(n)) if !n.trim().is_empty() => n.trim().to_string(),
                _ => {
                    ui::error("a name is required without a TTY.");
                    ui::hint(&format!("e.g. `agit init {suggestion}`"));
                    return Ok(ExitCode::Interactive);
                }
            }
        }
    };

    let owner = crate::infra::credentials::current_user().unwrap_or_else(|| "local".to_string());
    // A binding conflict is refused before anything touches disk: refusing after the repo is
    // created leaves a complete new repo behind, and the next retry runs into "already exists".
    if !args.no_bind
        && !args.rebind
        && let Some(ws) = workspace::read(&cwd)
        && ws.repo != format!("{owner}/{name}")
    {
        ui::error(&format!(
            "this directory is already bound to {}; keep working there, or rebind it explicitly with `--rebind`",
            ws.repo
        ));
        return Ok(ExitCode::Precondition);
    }
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    let repo = match Repo::open(&dir).map(|r| (checkout_state(&r), r)) {
        Some((CheckoutState::Empty, existing)) => {
            // The shape `agit repo create` + `agit clone` leaves behind: the remote identity
            // is already recorded in `.git/config`, only the main file line is missing. Lay the
            // line down in this checkout — refusing leaves a repo that can neither be init'd
            // ("already exists") nor start a session (no main).
            if existing.current_branch().as_deref() != Some("main") {
                existing.git(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
            }
            existing.ensure_committer()?;
            println!(
                "{}",
                ui::dim(&format!(
                    "  {owner}/{name} is an empty checkout — laying down its main file line in place"
                ))
            );
            existing
        }
        Some((CheckoutState::Unborn, _)) => {
            // No history yet, but the worktree/index already holds something: the scaffolding
            // overwrites files of the same name. Those bytes are the user's, and init has no
            // standing to decide for them whether to lose them.
            ui::error(&format!(
                "{owner}/{name} has no commits yet but its checkout is not empty ({}).",
                dir.display()
            ));
            ui::hint(
                "move the files out of the way (or commit them yourself) before `agit init` lays down the main file line",
            );
            return Ok(ExitCode::Precondition);
        }
        Some((CheckoutState::HasHistory, _)) => {
            ui::error(&format!(
                "{owner}/{name} already exists ({}).",
                dir.display()
            ));
            ui::hint("init creates repos; an existing one is used as-is");
            return Ok(ExitCode::Precondition);
        }
        None => Repo::init(&dir)?,
    };
    scaffold(repo.root())?;

    let seeded = if let Some(picked) = args.seed_assets.as_deref() {
        copy_seed_assets(repo.root(), picked)?
    } else if args.seed {
        seed_into(repo.root(), &cwd)?
    } else {
        0
    };
    if args.private {
        repo.set_visibility_preference("private")?;
        println!(
            "{}",
            ui::dim(
                "  preference recorded: the first `agit push` publishes private (`--public` overrides)"
            )
        );
    }

    repo.add_all()?;
    let msg = if seeded > 0 {
        "agit: init (main file line + adopted project assets)"
    } else {
        "agit: init (main file line)"
    };
    repo.commit(msg)?;

    if !args.no_bind {
        workspace::bind(&cwd, &format!("{owner}/{name}"), args.rebind)?;
        ui::success(&format!(
            "repo created: {owner}/{name} (main is the file line; scaffolding in {})",
            ui::tilde(repo.root())
        ));
        println!("{}", ui::dim("  bound to this directory. Next:"));
        println!("    agit import          adopt a running session (settles on import)");
        println!("    agit new -b <name>   start a fresh session (inherits memory/skills)");
    } else {
        ui::success(&format!(
            "repo created: {owner}/{name} (no directory bound)"
        ));
    }
    Ok(ExitCode::Ok)
}

/// The wizard represents only the zero-argument command. Flags keep their existing command-line
/// meaning and never disappear into a form that does not expose them.
fn wants_tui(args: &Args) -> bool {
    args.name.is_none()
        && !args.seed
        && !args.private
        && !args.no_bind
        && !args.rebind
        && args.seed_assets.is_none()
}

/// What an existing checkout is, as far as `init` is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckoutState {
    /// No commits and no local branches, nothing in the worktree but `.git`, and an empty index
    /// — the shell that `agit repo create` + `agit clone` leaves behind, where `init` can lay the
    /// line down in place.
    Empty,
    /// No history, but the worktree or the index already holds content. The scaffolding
    /// overwrites files of the same name, so this is refused.
    Unborn,
    /// Commits exist. Even one other branch is history of its own, and `init` does not decide
    /// for the user.
    HasHistory,
}

/// The test for taking a checkout over looks at **content**, not just at refs: "no commits" does
/// not prove the worktree is empty — a checkout that was cloned and then hand-written with an
/// AGENTS.md has no commits either.
fn checkout_state(repo: &Repo) -> CheckoutState {
    if repo.commit_count() > 0 || !repo.local_branches().is_empty() {
        return CheckoutState::HasHistory;
    }
    // An unreadable directory, or an unreadable entry inside it, counts as content: refusing
    // when in doubt is cheaper than overwriting when in doubt — the entry that got skipped may
    // be the user's only file.
    let worktree_has_content = match std::fs::read_dir(repo.root()) {
        Ok(entries) => entries.into_iter().any(|entry| match entry {
            Ok(entry) => entry.file_name().to_string_lossy() != ".git",
            Err(_) => true,
        }),
        Err(_) => true,
    };
    let index_has_content = repo
        .git_opt(&["ls-files"])
        .is_none_or(|listing| !listing.trim().is_empty());
    if worktree_has_content || index_has_content {
        CheckoutState::Unborn
    } else {
        CheckoutState::Empty
    }
}

/// Collect the project's existing memory / skills assets into the repo root (confirmed item by
/// item); returns how many were taken.
///
/// `agit import` comes through here when it creates a repo of its own — both answers to "what
/// belongs on the main file line" must be one implementation, or a repo from import is not the
/// same kind of thing as a repo from init.
pub(super) fn seed_into(repo_root: &Path, project: &Path) -> crate::Result<usize> {
    let found = find_seed_assets(project);
    if found.is_empty() {
        println!("  no adoptable assets found (AGENTS.md / CLAUDE.md / .claude/skills/ …)");
        return Ok(0);
    }
    let picked = pick_assets(&found);
    copy_seed_assets(repo_root, &picked)
}

fn copy_seed_assets(repo_root: &Path, picked: &[(PathBuf, PathBuf)]) -> crate::Result<usize> {
    for (dst, src) in picked {
        let target = repo_root.join(dst);
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, &target)?;
    }
    if !picked.is_empty() {
        ui::success(&format!("adopted {} assets", picked.len()));
    }
    Ok(picked.len())
}

/// The scaffolding for the main file line.
///
/// `session/meta.json` is part of it: **every** commit carries a meta, the file line's included
/// (the form is written there, not inferred from "are there session files"). The file line
/// carries no `session/log.jsonl` and no `session/VIEW` — it never claims a session, so it has
/// no VIEW.
pub(super) fn scaffold(root: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(root.join("memory"))?;
    std::fs::create_dir_all(root.join("skills"))?;
    std::fs::write(root.join("memory/.gitkeep"), "")?;
    std::fs::write(root.join("skills/.gitkeep"), "")?;
    meta::write(root, &Meta::new_file_line())
        .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    storage::ensure_attributes(root).map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    std::fs::write(
        root.join("AGENTS.md"),
        "# Team memory\n\nShared instructions for this repo. Every session started with `agit new` carries it.\n\n\
         - memory/  distilled facts and decisions (merging back to main = distillation)\n\
         - skills/  reusable skills\n",
    )?;
    Ok(())
}

/// Known asset locations → target paths.
pub(crate) fn find_seed_assets(cwd: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for (rel, dst) in [("AGENTS.md", "AGENTS.md"), ("CLAUDE.md", "CLAUDE.md")] {
        let src = cwd.join(rel);
        if src.is_file() {
            out.push((PathBuf::from(dst), src));
        }
    }
    // .claude/skills/<name>/SKILL.md → skills/<name>/SKILL.md
    let sk = cwd.join(".claude/skills");
    if let Ok(entries) = std::fs::read_dir(&sk) {
        for e in entries.flatten() {
            let skill_md = e.path().join("SKILL.md");
            if skill_md.is_file() {
                out.push((
                    PathBuf::from(format!(
                        "skills/{}/SKILL.md",
                        e.file_name().to_string_lossy()
                    )),
                    skill_md,
                ));
            }
        }
    }
    out
}

/// The ways assets enter the repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seed {
    /// The user has explicitly said "take all of it" (`-y`).
    All,
    /// A tty is there: ask item by item.
    AskEach,
    /// Nothing to ask on and no explicit authorization: take nothing.
    Nothing,
}

/// A non-interactive run **takes nothing by default**.
///
/// This is not fastidiousness: personal memory (CLAUDE.md, `.claude/skills/`) can hold private
/// content, and `--seed` says only "willing to take assets in", not "this one too" — the design
/// requires item-by-item confirmation before anything enters the repo, and with no way to ask,
/// the only safe answer is to take nothing. `-y` is the explicit "take all of it" action; with it
/// there is nothing left to ask (under a tty too — that is exactly what `-y` means).
fn seed_policy(tty: bool, yes: bool) -> Seed {
    match (yes, tty) {
        (true, _) => Seed::All,
        (false, true) => Seed::AskEach,
        (false, false) => Seed::Nothing,
    }
}

fn pick_assets(found: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, PathBuf)> {
    let policy = seed_policy(ui::is_tty(), std::env::var_os("AGIT_YES").is_some());
    if policy != Seed::AskEach {
        let take = policy == Seed::All;
        for (dst, src) in found {
            println!(
                "  {} {} (from {})",
                if take { "adopt" } else { "found" },
                dst.display(),
                src.display()
            );
        }
        if !take {
            ui::hint(
                "nothing adopted: personal memory can carry private content and there’s no TTY to confirm item by item — re-run with `-y` to take all of it",
            );
            return vec![];
        }
        return found.to_vec();
    }
    use crate::ui::prompt;
    let labels: Vec<String> = found
        .iter()
        .map(|(dst, src)| format!("{} ← {}", dst.display(), src.display()))
        .collect();
    // Confirm item by item: nothing is picked by default, and only an Enter counts.
    let mut picked = Vec::new();
    for (item, label) in found.iter().zip(labels.iter()) {
        match prompt::confirm(&format!("adopt {label}?"), true) {
            Ok(Some(true)) => picked.push(item.clone()),
            Ok(Some(false)) => println!("  skip {label}"),
            _ => break, // the tty is gone: stop asking, take nothing on the user's behalf
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        args: super::Args,
    }

    /// Only the bare form is represented by the wizard; every flag stays on the established CLI
    /// path so its semantics cannot be lost behind a checkbox that does not exist.
    #[test]
    fn only_the_zero_argument_form_enters_the_init_wizard() {
        assert!(wants_tui(&W::try_parse_from(["x"]).unwrap().args));
        for argv in [
            vec!["x", "repo"],
            vec!["x", "--seed"],
            vec!["x", "--private"],
            vec!["x", "--no-bind"],
            vec!["x", "--rebind"],
        ] {
            assert!(!wants_tui(&W::try_parse_from(argv).unwrap().args));
        }
    }

    /// "Personal memory can carry private content and is never collected silently" — a
    /// non-interactive `--seed` takes nothing.
    ///
    /// This pins the answer for a run with no tty. An implementation that takes everything there
    /// instead lets CI, pipelines and every agent-driven call (none of them a tty) fold
    /// CLAUDE.md and the whole skills directory into a repo that gets pushed, with nobody
    /// confirming.
    #[test]
    fn seeding_without_a_tty_takes_nothing_unless_explicitly_confirmed() {
        assert_eq!(seed_policy(false, false), Seed::Nothing);
        assert_eq!(
            seed_policy(false, true),
            Seed::All,
            "`-y` is the explicit action"
        );
        assert_eq!(
            seed_policy(true, false),
            Seed::AskEach,
            "a tty means asking item by item"
        );
        assert_eq!(
            seed_policy(true, true),
            Seed::All,
            "`-y` means not asking again"
        );
    }

    /// Asset discovery recognizes the places the design names.
    #[test]
    fn seed_assets_are_found_where_the_design_says_they_live() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(p.join("AGENTS.md"), "a").unwrap();
        std::fs::write(p.join("CLAUDE.md"), "c").unwrap();
        std::fs::create_dir_all(p.join(".claude/skills/refund")).unwrap();
        std::fs::write(p.join(".claude/skills/refund/SKILL.md"), "s").unwrap();
        // A directory with no SKILL.md is not a skill and is not taken.
        std::fs::create_dir_all(p.join(".claude/skills/empty")).unwrap();

        let found = find_seed_assets(p);
        let dsts: Vec<String> = found.iter().map(|(d, _)| d.display().to_string()).collect();
        assert!(dsts.contains(&"AGENTS.md".to_string()), "{dsts:?}");
        assert!(dsts.contains(&"CLAUDE.md".to_string()), "{dsts:?}");
        assert!(
            dsts.contains(&"skills/refund/SKILL.md".to_string()),
            "{dsts:?}"
        );
        assert_eq!(found.len(), 3, "{dsts:?}");
    }

    /// Only a checkout with "no commits, and an empty worktree and index" can be taken over by
    /// `init`: that is what `agit clone` after `agit repo create` produces. With an untracked
    /// AGENTS.md sitting in the worktree the scaffolding overwrites it, so that checkout must be
    /// judged not takeable; a repo that has landed a commit has history of its own and is no
    /// longer an empty shell either. A test that only counts commits and branches silently loses
    /// bytes in the first case.
    #[test]
    fn only_a_checkout_without_any_content_counts_as_empty() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("agents/alice/photo")).unwrap();
        assert_eq!(checkout_state(&r), CheckoutState::Empty);

        std::fs::write(
            r.root().join("AGENTS.md"),
            "# mine
",
        )
        .unwrap();
        assert_eq!(
            checkout_state(&r),
            CheckoutState::Unborn,
            "an untracked file is user content"
        );
        std::fs::remove_file(r.root().join("AGENTS.md")).unwrap();
        assert_eq!(checkout_state(&r), CheckoutState::Empty);

        // Content that is only in the index, with the worktree copy deleted, counts too.
        std::fs::write(
            r.root().join("memory.md"),
            "x
",
        )
        .unwrap();
        r.add_all().unwrap();
        std::fs::remove_file(r.root().join("memory.md")).unwrap();
        assert_eq!(
            checkout_state(&r),
            CheckoutState::Unborn,
            "a staged file is user content even after the worktree copy is gone"
        );
        r.git(&["rm", "-q", "--cached", "memory.md"]).unwrap();
        assert_eq!(checkout_state(&r), CheckoutState::Empty);

        scaffold(r.root()).unwrap();
        r.add_all().unwrap();
        r.commit("agit: init (main file line)").unwrap();
        assert_eq!(checkout_state(&r), CheckoutState::HasHistory);
    }

    /// The main the scaffolding lays down is a **file line**, with the form written into the meta.
    #[test]
    fn the_scaffold_declares_a_file_line() {
        let d = tempfile::tempdir().unwrap();
        scaffold(d.path()).unwrap();
        let m = meta::read(d.path()).expect("every commit carries a meta, the file line included");
        assert!(m.is_file_line());
        assert!(d.path().join("memory").is_dir() && d.path().join("skills").is_dir());
        assert!(d.path().join("AGENTS.md").is_file());
        assert!(d.path().join(meta::ATTRS_FILE).is_file());
        // The file line never claims a session, so it has no transcript and no VIEW.
        assert!(!d.path().join(meta::LOG_FILE).exists());
        assert!(!d.path().join(meta::VIEW_FILE).exists());
    }
}
