//! `agit new` — start a brand new session (empty VIEW).
//!
//! The split with fork (PRD): fork opens a new line carrying context; new **carries memory only**
//! — it inherits the shared files (memory/ skills/ AGENTS.md) from `--from` (the main file line by
//! default), materializes them where the harness expects them, and injects `AGIT_SESSION` at
//! launch.
//!
//! Enterprise memory injection needs no new mechanism: the organization repo's main keeps
//! evolving, and every member's `agit new` picks up the latest.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

/// The inheritance point when `--from` is omitted: the main file line.
///
/// It is defined here because the default belongs to `new`. The picker (`tui::screens::repos`)
/// refers to this same constant instead of writing its own `"main"`: it decides whether to block
/// when the line declaration cannot be read by asking whether this run's `--from` is the default
/// (see `repos::choose`). Once the two literals are edited apart, what should be blocked is let
/// through — a failure that shows no error, only one gate fewer.
pub const DEFAULT_FROM: &str = "main";

#[derive(ClapArgs)]
pub struct Args {
    /// Target `owner/repo@from-ref` (or legacy repo plus `--from`; default: context).
    #[arg(value_name = "owner/repo@from-ref")]
    pub repo: Option<String>,
    /// New branch name.
    #[arg(short = 'b', long, value_name = "name")]
    pub branch: Option<String>,
    /// Inheritance point (default: the main file line).
    #[arg(long, value_name = "ref", default_value = DEFAULT_FROM)]
    pub from: String,
    /// Target runtime (default: config runtime.default → claude-code).
    #[arg(long = "as", value_name = "runtime")]
    pub as_runtime: Option<String>,
    /// Launch directory (default: current directory).
    #[arg(long, value_name = "dir")]
    pub cwd: Option<PathBuf>,
    /// Create the branch, materialize shared files, print the launch command — don’t start the harness.
    #[arg(long)]
    pub no_launch: bool,
    /// Intentionally discard the current unmanaged runtime session and start empty.
    #[arg(long)]
    pub fresh: bool,
}

pub fn run(args: Args) -> CmdResult {
    let mut args = args;
    // With no arguments, and when the test holds, the picker opens first (`docs/07_tui.md`
    // §3.2). It asks for both the repo and the branch name before handing control back, and the
    // whole path below does not change by a byte — the TUI only **fills in arguments**, it does
    // not reimplement new.
    //
    // `--from` is handed to it unchanged: `agit new --from v0.3` leaves the key arguments empty
    // just the same, so the interface still opens, but the whole screen must be computed against
    // `v0.3`. A screen that says "inherits from main" while inheriting from somewhere else is
    // far worse than never opening one.
    //
    // **It does not open inside an unadopted runtime session.** The two guards below reject this
    // `new` (unless `--fresh`), and they deliberately sit ahead of the branch-name prompt, so
    // that no question is asked that will not be used. Opening a full-screen picker, letting the
    // user choose a repo and type a name, and only then refusing, is exactly what that rule
    // blocks — only heavier.
    if args.repo.is_none()
        && args.branch.is_none()
        && (args.fresh || crate::infra::runtime_session::unmanaged().is_none())
    {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => match crate::tui::screens::repos::pick(&args.from)? {
                Some(picked) => {
                    args.repo = Some(picked.slug);
                    args.branch = Some(picked.branch);
                }
                // The user gave up, or this machine has no repo at all (the next step was
                // already given there).
                None => return Ok(ExitCode::Ok),
            },
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }
    let cwd_now = std::env::current_dir()?;
    let work_dir = args.cwd.clone().unwrap_or(cwd_now.clone());

    // Unified target form: `owner/repo@<inheritance-ref>`.  Keep `--from`
    // as a compatibility spelling for scripts that already use it.
    let unified = args
        .repo
        .as_deref()
        .map(crate::commands::target::parse)
        .transpose()?;
    if unified
        .as_ref()
        .is_some_and(|t| t.repo.is_some() && t.base.is_some())
        && args.from != DEFAULT_FROM
    {
        ui::error("use either `<owner>/<repo>@<from-ref>` or `--from`, not both.");
        return Ok(ExitCode::Usage);
    }
    let from_ref = unified
        .as_ref()
        .filter(|t| t.repo.is_some())
        .and_then(crate::commands::target::ref_text)
        .unwrap_or_else(|| args.from.clone());

    // ── repo resolution ──
    let slug = match &args.repo {
        Some(r) => {
            let repo_name = unified
                .as_ref()
                .and_then(|t| t.repo.clone())
                .unwrap_or_else(|| r.to_string());
            match super::parse_slug(&repo_name) {
                Ok((o, n)) => format!("{o}/{n}"),
                Err(_) => {
                    // A bare name: the one local repo carrying it.
                    let me =
                        crate::infra::credentials::current_user().unwrap_or_else(|| "local".into());
                    match super::clone::checkouts_named(&me, r)?.as_slice() {
                        [only] => only.slug(),
                        _ => {
                            ui::error(&format!(
                                "`{r}` is ambiguous or missing — write owner/repo."
                            ));
                            return Ok(ExitCode::Ref);
                        }
                    }
                }
            }
        }
        // The branch name comes from `-b`; only "which repo" is missing, so the directory
        // binding is enough. A full `resolve` demands that a branch resolve as well, which
        // leaves a directory fresh out of `agit init` unable to open its first session.
        None => match super::context::repo_for(&cwd_now) {
            Ok(r) => super::context::qualify(&r),
            Err(e) => {
                // Inside an unadopted runtime session, the difference between `new` and
                // `import` is explained first even with no explicit target; otherwise the user
                // sees an unrelated "cannot resolve repo" and misuses it again. With the target
                // unknown, replaceable placeholders stand in.
                if !args.fresh
                    && let Some(current) = crate::infra::runtime_session::unmanaged()
                {
                    ui::session::warn_new(
                        &current,
                        "<owner/repo>",
                        args.branch.as_deref().unwrap_or("<branch>"),
                    );
                    return Ok(ExitCode::Precondition);
                }
                ui::error(&format!("{e:#}"));
                ui::hint("be explicit: agit new <owner/repo> -b <branch>");
                return Ok(ExitCode::Ref);
            }
        },
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let dir = config::repo_dir(&owner, &name)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        ui::hint(&format!("fetch it first: `agit clone {slug}` (read-only)"));
        return Ok(ExitCode::Precondition);
    };

    // ── --from: main by default, must be a file line (memory only, no context) ──
    // The repo is already chosen above; the ref here is a ref **inside that repo**, and a branch
    // name may contain `/`. The shared parser reads a bare `topic/foo` as the owner/repo form,
    // so the local-ref entry point is required.
    let from_spec = crate::commands::target::parse_local(&from_ref)?;
    let from_head = match crate::domain::refs::resolve(&repo, &from_spec) {
        Ok(resolved) => resolved.sha,
        Err(e) => {
            ui::error(&format!("{slug} has no ref `{from_ref}`: {e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    match meta::line_at_ref(&repo, &from_head) {
        Some(meta::Line::File) => {}
        Some(meta::Line::Session) => {
            ui::error(&format!(
                "`{from_ref}` is a session line. `new` carries memory only (the inheritance target must point at a file line)."
            ));
            ui::hint(&format!(
                "for context, fork: `agit fork {from_ref} -b <new-name> --resume`"
            ));
            return Ok(ExitCode::Precondition);
        }
        None => {
            ui::error(&format!(
                "`{}` carries no {} — its line was never declared.",
                from_ref,
                meta::FILE
            ));
            ui::hint(
                "a repo made by `agit init` always has one; re-fetch this checkout, or start over with `agit init`",
            );
            return Ok(ExitCode::Precondition);
        }
    }

    // `new` is an empty session; running it inside a runtime session that has not been adopted
    // leaves the current conversation in the runtime while taking the user onto another line
    // with no context. Blocked by default; only an explicit `--fresh` allows dropping the
    // current session. This check sits ahead of the branch-name prompt, so the user never
    // answers an interactive question that will not be used.
    if !args.fresh
        && let Some(current) = crate::infra::runtime_session::unmanaged()
    {
        ui::session::warn_new(
            &current,
            &slug,
            args.branch.as_deref().unwrap_or("<branch>"),
        );
        return Ok(ExitCode::Precondition);
    }

    // ── new branch name ──
    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => match ui::prompt::input("branch name for the new session", Some("new-session"))? {
            Some(b) if !b.trim().is_empty() => b.trim().to_string(),
            _ => {
                ui::error("non-interactive use requires -b <name>.");
                return Ok(ExitCode::Interactive);
            }
        },
    };
    if let Err(e) = crate::domain::repo::valid_branch_name(&branch) {
        ui::error(&format!("{e:#}"));
        return Ok(ExitCode::Usage);
    }
    if repo.has_ref(&format!("refs/heads/{branch}")) {
        ui::error(&format!("branch `{branch}` already exists."));
        return Ok(ExitCode::Policy);
    }

    // ── create the branch: inherit the tree at from, mint an empty session ──
    let claim = super::fork::mint_claim(&from_head, &branch);
    let mut snap = meta::Meta::new(
        claim,
        runtime_of(&args),
        work_dir.to_string_lossy().to_string(),
    );
    snap.turn = None; // no turn yet; the first turn commit is #1
    // The birth commit holds no conversation; marking it as a turn makes `agit log` lay out a
    // settlement for a turn that never happened.
    snap.kind = meta::Kind::File;
    snap.milestone = Some(format!("new session (from {from_ref})"));
    let snap_text = meta::to_text(&snap)?;
    // The new session's tree = the shared files at from, with a `line: session` meta swapped in,
    // and empty LOG / VIEW written once every old session carrier is cleared: an empty VIEW is
    // the definition of new (not one byte of content is inherited — that is fork's job). The
    // line form is fixed at this moment.
    let tree = fresh_session_tree(&repo, &from_head, &snap_text)?;
    let commit = super::plumbing::commit_tree(
        &repo,
        &tree,
        &[&from_head],
        &format!("agit: new session {branch}"),
    )?;
    super::plumbing::update_ref_cas(&repo, &format!("refs/heads/{branch}"), &commit, None)?;

    ui::success(&format!(
        "new session {slug} @ {branch} (memory inherited from {from_ref})"
    ));

    // ── materialize the shared files ──
    //
    // AGENTS.md lands at the project root (Codex / Cursor read it natively); memory goes through
    // the mirror model into the runtime's own memory directory (see `commands::memory`). agit no
    // longer materializes skills — they travel with the code repo's `.claude/skills` /
    // `.agents/skills`.
    materialize_shared(&repo, &from_head, &work_dir);
    match super::memory::materialize(&repo, &branch, &slug, &runtime_of(&args), &work_dir) {
        Ok(Some(report)) => super::memory::report_materialize(&report),
        Ok(None) => {}
        Err(error) => ui::warning(&format!("memory was not materialized: {error:#}")),
    }

    // ── launch ──
    let cmd = format!(
        "(export AGIT_SESSION={}; cd {} && {})",
        shell_q(&super::context::encode_session_env(&slug, &branch)),
        shell_q(&work_dir.to_string_lossy()),
        runtime_cli_of(&args)
    );
    if args.no_launch {
        println!("\n  {}", ui::accent(&cmd));
        println!(
            "{}",
            ui::dim(
                "  note: a fresh session gets its id minted by the harness; once `agit setup --hooks` is installed, the first settle claims this branch"
            )
        );
        return Ok(ExitCode::Ok);
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()?;
    Ok(match status.code() {
        Some(0) | None => ExitCode::Ok,
        Some(_) => ExitCode::Precondition,
    })
}

/// Mint an empty v1 session tree from any file-line tree.
///
/// An aging repo's file line may carry v0 session files by accident, or a whole `events/` tree
/// dragged in by a merge somewhere in its history. A new session inherits the shared files only,
/// never context, so every storage carrier is enumerated by path and cleared before empty LOG /
/// VIEW and v1 attributes are written. A directory cannot be removed with a single
/// `update-index --force-remove events`; what exists in the Git index is each file underneath it.
pub(super) fn fresh_session_tree(
    repo: &Repo,
    base: &str,
    meta_text: &str,
) -> crate::Result<String> {
    super::plumbing::ensure_v1_upgrade_preflight(repo, base)?;
    let layout = super::plumbing::storage_layout_at(repo, base)?;
    let existing_attributes = super::plumbing::regular_blob_text_at(repo, base, meta::ATTRS_FILE)?;
    let mut edits: std::collections::BTreeMap<String, Option<Vec<u8>>> = repo
        .ls_tree_result(base)?
        .into_iter()
        .filter(|path| meta::is_storage_path_for(layout, path))
        .map(|path| (path, None))
        .collect();
    for (path, bytes) in storage::snapshot_files("", "")? {
        edits.insert(path, Some(bytes));
    }
    edits.insert(meta::FILE.to_string(), Some(meta_text.as_bytes().to_vec()));
    edits.insert(
        meta::ATTRS_FILE.to_string(),
        Some(storage::attributes_text_strict(existing_attributes.as_deref())?.into_bytes()),
    );
    super::plumbing::tree_apply_owned(repo, base, edits.into_iter().collect())
}

fn runtime_of(args: &Args) -> String {
    args.as_runtime
        .clone()
        .or_else(|| super::config::get("runtime.default"))
        .unwrap_or_else(|| "claude-code".into())
}

fn runtime_cli_of(args: &Args) -> String {
    match runtime_of(args).as_str() {
        "codex" => "codex".into(),
        "opencode" => "opencode".into(),
        _ => "claude".into(),
    }
}

/// Materialize the shared instructions: AGENTS.md → cwd/AGENTS.md (an existing file is never
/// overwritten — the project's own copy wins).
fn materialize_shared(repo: &Repo, from_head: &str, cwd: &Path) {
    let entries = repo.ls_tree(from_head);
    for p in entries {
        if p == "AGENTS.md" {
            let Some(text) = repo.show(from_head, &p) else {
                continue;
            };
            let dst = cwd.join(&p);
            if dst.exists() {
                println!(
                    "{}",
                    ui::dim(&format!("  skipped (exists): {}", dst.display()))
                );
                continue;
            }
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&dst, &text).is_ok() {
                println!(
                    "  {} materialized {}",
                    ui::ok(ui::theme::symbols().check),
                    p
                );
            }
        }
    }
}

fn shell_q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

    #[test]
    fn fresh_is_explicit_opt_in() {
        assert!(!W::parse_from(["x"]).args.fresh);
        assert!(W::parse_from(["x", "--fresh"]).args.fresh);
    }

    #[test]
    fn slash_inheritance_refs_are_parsed_inside_the_selected_repo() {
        let spec = crate::commands::target::parse_local("topic/foo").unwrap();
        assert_eq!(
            spec.base,
            crate::domain::refs::Base::Name("topic/foo".into())
        );
        assert_eq!(spec.repo, crate::domain::refs::RepoSel::Context);
    }

    /// This pins that the default for `--from` really is [`DEFAULT_FROM`].
    ///
    /// The failure it guards against **shows no error**: `tui::screens::repos::choose` decides
    /// whether to block when the line declaration cannot be read by asking whether this run's
    /// `--from` is the default. Once the default here parts ways with that constant, the picker
    /// takes the new default for "a ref the user wrote themselves" and lets it through, so what
    /// should be blocked is not — nothing on the screen changes, there is simply one gate fewer.
    #[test]
    fn the_default_inheritance_point_is_the_one_the_picker_checks_against() {
        #[derive(clap::Parser)]
        struct Only {
            #[command(flatten)]
            args: Args,
        }
        let parsed = <Only as clap::Parser>::parse_from(["agit-new"]);
        assert_eq!(parsed.args.from, DEFAULT_FROM);
    }

    #[test]
    fn fresh_session_tree_removes_every_old_storage_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::create_dir_all(repo.root().join("session")).unwrap();
        std::fs::create_dir_all(repo.root().join("events/a/a/a/a")).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), "legacy\n").unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), "legacy\n").unwrap();
        std::fs::write(
            repo.root()
                .join("events/a/a/a/a/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "stale\n",
        )
        .unwrap();
        std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.bin binary\n").unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("base").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let born = meta::Meta::new_session_line("codex".into(), "/work".into());
        let tree = fresh_session_tree(&repo, head.trim(), &meta::to_text(&born).unwrap()).unwrap();
        let commit =
            super::super::plumbing::commit_tree(&repo, &tree, &[head.trim()], "fresh session")
                .unwrap();

        assert_eq!(repo.show_raw(&commit, meta::LOG_FILE).as_deref(), Some(""));
        assert_eq!(repo.show_raw(&commit, meta::VIEW_FILE).as_deref(), Some(""));
        assert!(repo.show_raw(&commit, meta::LEGACY_LOG_FILE).is_none());
        assert!(repo.show_raw(&commit, meta::LEGACY_VIEW_FILE).is_none());
        assert!(
            repo.ls_tree(&commit)
                .iter()
                .all(|path| !path.starts_with("events/"))
        );
        let attributes = repo.show_raw(&commit, meta::ATTRS_FILE).unwrap();
        assert!(attributes.contains("*.bin binary"));
        assert!(attributes.contains("agit:storage-v1"));
    }

    #[test]
    fn fresh_session_refuses_v0_user_file_in_v1_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        let mut old = meta::Meta::new_file_line();
        old.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &old).unwrap();
        std::fs::write(repo.root().join(meta::LOG_FILE), "user-owned root LOG\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 file line").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let born = meta::Meta::new_session_line("codex".into(), "/work".into());

        let error = fresh_session_tree(&repo, &head, &meta::to_text(&born).unwrap()).unwrap_err();
        assert!(error.to_string().contains("user-owned"), "{error:#}");
        assert_eq!(
            repo.show_raw_result(&head, meta::LOG_FILE)
                .unwrap()
                .as_deref(),
            Some("user-owned root LOG\n")
        );
    }
}
