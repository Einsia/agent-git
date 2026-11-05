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
                ui::error(&super::terminal_error_message(&e));
                return Ok(ExitCode::Usage);
            }
        },
        None => None,
    };
    let explicit = parsed_ref.as_ref().and_then(|t| t.repo.clone());
    let repo_source = if explicit.is_some() {
        super::echo::Source::Explicit
    } else {
        super::echo::Source::Environment
    };
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
    let (owner, name) = if repo_source == super::echo::Source::Explicit {
        crate::input_argument(super::parse_slug(&slug))?
    } else {
        super::parse_slug(&slug)?
    };
    let Some(repo) = Repo::open(crate::infra::config::repo_dir(&owner, &name)?) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        ui::hint(&format!("fetch it first: `agit clone {slug}`"));
        return Ok(ExitCode::Precondition);
    };

    match args.name {
        None => list(&repo, &slug, repo_source),
        Some(tag) if args.delete => delete(
            &repo,
            &slug,
            &tag,
            args.force,
            if repo_source == super::echo::Source::Environment {
                super::echo::Source::Mixed
            } else {
                repo_source
            },
        ),
        Some(tag) => create(
            &repo,
            &slug,
            &tag,
            args.ref_.as_deref(),
            args.message.as_deref(),
            args.force,
            ctx_branch.as_deref().unwrap_or("main"),
        ),
    }
}

fn list(repo: &Repo, slug: &str, source: super::echo::Source) -> CmdResult {
    let out = repo.git(&[
        "tag",
        "--list",
        "--format=%(refname:short)  %(objectname:short)  %(contents:subject)",
    ])?;
    super::echo::emit("tag", &[super::echo::Selection::new(slug, source)]);
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
    slug: &str,
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
    let spec = match ref_ {
        Some(explicit) => crate::input_argument(refs::parse(explicit))?,
        None => refs::parse(ctx_branch)?,
    };
    let source = if ref_.is_some() {
        super::echo::Source::for_spec(&spec)
    } else {
        super::echo::Source::Environment
    };
    let target = refs::resolve(repo, &spec).map_err(|e| {
        ui::error(&format!("{e:#}"));
        e
    })?;
    // Publication rewrites session history: a lightweight tag is published on the public commit
    // its turn became, while an annotated tag object names the local commit and carries a message
    // nothing protects, so it could never be published as it is.
    let on_session_line = crate::domain::meta::read_at_ref_result(repo, &target.sha)?
        .is_some_and(|metadata| metadata.is_session_line());
    if on_session_line {
        if message.is_some() {
            ui::error("a tag on session history can't carry a message (`-m`).");
            ui::hint(&format!(
                "leave out -m: `agit tag {tag} {}` is published on the turn's public commit",
                ref_.unwrap_or(ctx_branch)
            ));
            return Ok(ExitCode::Usage);
        }
        if let Some(code) = refuse_unpublishable_name(repo, tag)? {
            return Ok(code);
        }
    }
    super::echo::emit(
        "tag",
        &[super::echo::Selection::new(
            format!("{slug}@{}", target.branch.as_deref().unwrap_or(&target.sha)),
            source,
        )],
    );

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

fn delete(
    repo: &Repo,
    slug: &str,
    tag: &str,
    force: bool,
    source: super::echo::Source,
) -> CmdResult {
    if !repo.has_tag(tag) {
        ui::error(&format!("no tag `{tag}`."));
        return Ok(ExitCode::Ref);
    }
    super::echo::emit(
        "tag",
        &[super::echo::Selection::new(format!("{slug}@{tag}"), source)],
    );
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

/// Refuses a tag `agit commit --tag` would put on a session turn before anything lands: a name
/// git or agit cannot take, one already in use, or one publication would have to change.
pub(super) fn refuse_session_tag(repo: &Repo, tag: &str) -> crate::Result<Option<ExitCode>> {
    if !domain_valid_name(tag)
        || repo
            .git_opt(&["check-ref-format", &format!("refs/tags/{tag}")])
            .is_none()
    {
        ui::error(&format!(
            "`{tag}` is not a legal tag name (no spaces / ~ / # / : / @)."
        ));
        return Ok(Some(ExitCode::Usage));
    }
    if repo.has_tag(tag) {
        ui::error(&format!("`{tag}` already exists; pick another tag name."));
        return Ok(Some(ExitCode::Precondition));
    }
    refuse_unpublishable_name(repo, tag)
}

/// A tag name is published as it is, so it is held to the check a session branch name meets:
/// nothing the detector, persona masking or a value this device registered would replace.
fn refuse_unpublishable_name(repo: &Repo, tag: &str) -> crate::Result<Option<ExitCode>> {
    let redactor =
        crate::domain::redact::Redactor::try_this_machine()?.with_repository(repo.root())?;
    if redactor.publishes_unchanged(tag)? {
        return Ok(None);
    }
    ui::error(
        "this tag name holds text the privacy check would change, so it can't be published as it is.",
    );
    ui::hint("pick a plain name that says what the turn did, such as `ms-login-flow`");
    Ok(Some(ExitCode::Usage))
}

fn domain_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '~' | '#' | ':' | '@'))
        && !name.starts_with('-')
}
