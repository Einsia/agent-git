//! `agit tag` — name a milestone / release version.
//!
//! A tag is **a named pointer moved only by a person (or by an agent explicitly)**, cleanly
//! divided from the branch that advances on its own (PRD). Exactly one rule is hard: a pushed
//! tag can neither be deleted nor moved — it is how others enter with `agit run owner/repo@v2`,
//! and moving it swaps the floor out from under them.

use super::CmdResult;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Tag name. Omitted = list all.
    pub name: Option<String>,
    /// Which target ref to point at (`owner/repo@ref`, or a local ref). Default: current branch head.
    pub ref_: Option<String>,
    /// Annotation.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
    /// Retag the same name (only if never pushed).
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Delete (only if never pushed).
    #[arg(short = 'd', long)]
    pub delete: bool,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let parsed_ref = match args.ref_.as_deref() {
        Some(raw) => match crate::commands::target::parse(raw) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        },
        None => None,
    };
    let explicit = parsed_ref.as_ref().and_then(|t| t.repo.clone());
    let (slug, ctx_branch) = if let Some(slug) = explicit {
        (slug, None)
    } else {
        let ctx = match super::context::resolve(&cwd) {
            Ok(c) => c,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        };
        (ctx.repo, Some(ctx.branch))
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let Some(repo) = Repo::open(crate::infra::config::repo_dir(&owner, &name)?) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        ui::hint(&format!("fetch it first: `agit clone {slug}`"));
        return Ok(ExitCode::Precondition);
    };

    match args.name {
        None => list(&repo),
        Some(tag) if args.delete => delete(&repo, &tag, args.force),
        Some(tag) => create(
            &repo,
            &tag,
            args.ref_.as_deref(),
            args.message.as_deref(),
            args.force,
            ctx_branch.as_deref().unwrap_or("main"),
        ),
    }
}

fn list(repo: &Repo) -> CmdResult {
    let out = repo.git(&[
        "tag",
        "--list",
        "--format=%(refname:short)  %(objectname:short)  %(contents:subject)",
    ])?;
    if out.trim().is_empty() {
        println!("no tags yet. Mint one: `agit tag v1`.");
    } else {
        print!("{out}");
    }
    Ok(ExitCode::Ok)
}

/// Whether this tag already exists on the remote. None = it cannot be asked (offline, etc.).
fn on_remote(repo: &Repo, tag: &str) -> Option<bool> {
    if repo.remote("origin").is_none() {
        return Some(false);
    }
    repo.git_opt(&["ls-remote", "--tags", "origin", &format!("refs/tags/{tag}")])
        .map(|out| !out.trim().is_empty())
}

fn create(
    repo: &Repo,
    tag: &str,
    ref_: Option<&str>,
    message: Option<&str>,
    force: bool,
    ctx_branch: &str,
) -> CmdResult {
    if !domain_valid_name(tag) {
        ui::error(&format!(
            "`{tag}` is not a legal tag name (no spaces / ~ / # / : / @)."
        ));
        return Ok(ExitCode::Usage);
    }
    if repo.has_tag(tag) {
        match on_remote(repo, tag) {
            Some(true) => {
                ui::error(&format!(
                    "`{tag}` is already pushed — it can’t move; it’s how others enter with run."
                ));
                ui::hint("mint a higher one instead: `agit tag v2`");
                return Ok(ExitCode::Policy);
            }
            Some(false) if !force => {
                ui::error(&format!(
                    "`{tag}` exists (unpushed). Add `-f` if you really want to move it."
                ));
                return Ok(ExitCode::Precondition);
            }
            None if !force => {
                ui::error(&format!(
                    "`{tag}` exists, and the remote’s state can’t be asked right now."
                ));
                ui::hint("confirm it was never pushed before adding -f; pushed tags don’t move");
                return Ok(ExitCode::Policy);
            }
            _ => {}
        }
    }

    // Resolve the target: an explicit ref goes through the reference syntax; the default pins
    // the head of the current branch.
    let spec = ref_.unwrap_or(ctx_branch);
    let spec = refs::parse(spec)?;
    let target = refs::resolve(repo, &spec).map_err(|e| {
        ui::error(&format!("{e:#}"));
        e
    })?;

    let mut cmd = vec!["tag"];
    if force {
        cmd.push("-f");
    }
    if let Some(m) = message {
        cmd.push("-a");
        cmd.push("-m");
        cmd.push(m);
    }
    cmd.push(tag);
    cmd.push(&target.sha);
    repo.git(&cmd)?;
    ui::success(&format!(
        "{tag} → {}",
        &target.sha[..12.min(target.sha.len())]
    ));
    Ok(ExitCode::Ok)
}

fn delete(repo: &Repo, tag: &str, force: bool) -> CmdResult {
    if !repo.has_tag(tag) {
        ui::error(&format!("no tag `{tag}`."));
        return Ok(ExitCode::Ref);
    }
    match on_remote(repo, tag) {
        Some(true) => {
            ui::error(&format!(
                "`{tag}` is already pushed — it can’t be deleted; remote history is immutable."
            ));
            ui::hint("for a new-generation entry point, mint a new tag: `agit tag v2 <branch>`");
            return Ok(ExitCode::Policy);
        }
        None if !force => {
            ui::error(&format!(
                "the remote’s state can’t be asked, so `{tag}` can’t be proven unpushed."
            ));
            ui::hint("add -f once confirmed");
            return Ok(ExitCode::Policy);
        }
        _ => {}
    }
    repo.git(&["tag", "-d", tag])?;
    ui::success(&format!("deleted {tag} (local only)"));
    Ok(ExitCode::Ok)
}

fn domain_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '~' | '#' | ':' | '@'))
        && !name.starts_with('-')
}
