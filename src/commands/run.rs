//! `agit run` — start any frozen ref.
//!
//! One command covers the whole path: fetch (the owner-qualified form = explicitly networked) →
//! locate → branch arbitration → materialize → launch. Arbitration has a single rule (the PRD's
//! `agit run` section):
//!
//! * the ref is a **branch head** of a session line you can **write**, with no live instance →
//!   equivalent to resume (continue). "Writable" is answered by the hub's write-permission gate:
//!   your own namespace, or an org repo you are granted;
//! * everything else — a tag, a historic commit, someone else's branch, the file line — **forks
//!   by necessity**; `-b` gives the new branch name, and on a tty it suggests one to confirm.
//!
//! Running someone else's ref leaves the work local by default and does not touch your hub
//! namespace; `--mine` moves "publish under your own name" up front (equivalent to `clone --mine`
//! first).

use super::CmdResult;
use crate::domain::refs::{self};
use crate::domain::repo::Repo;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// Any ref: `owner/repo@ref` (explicitly networked) or a local `<ref>`.
    pub source: String,
    /// Copy their repo into your namespace first, then fork (requires sign-in).
    #[arg(long)]
    pub mine: bool,
    /// Launch under another runtime.
    #[arg(long = "as", value_name = "runtime")]
    pub as_runtime: Option<String>,
    /// Launch in this directory.
    #[arg(long, value_name = "dir")]
    pub cwd: Option<PathBuf>,
    /// Name for the forked branch (required when forking; on a TTY you confirm the suggestion).
    #[arg(short = 'b', long, value_name = "branch")]
    pub branch: Option<String>,
    /// Prepare only (fork included) and print the command — don’t launch.
    #[arg(long)]
    pub no_launch: bool,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let spec = match refs::parse(&args.source) {
        Ok(s) => s,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    };
    let source = super::echo::Source::for_spec(&spec);
    let legacy_output = super::echo::legacy_output("run");
    let mut startup_notes = RunNotes {
        defer: !legacy_output,
        notes: Vec::new(),
    };

    // ── 1. Fetch. owner-qualified = explicitly networked ──
    let (slug, networked) = match &spec.repo {
        refs::RepoSel::Slug(o, n) => {
            let slug = format!("{o}/{n}");
            if args.mine {
                // Ensure it is under your own name first (equivalent to `clone --mine`;
                // `clone` is idempotent: an existing read-only checkout is promoted in place;
                // one already under your name is only updated).
                if let Err(e) = ensure_mine(&slug) {
                    super::fix::register_terminal_api_error(&e);
                    ui::error(&format!("promotion into your namespace failed: {e:#}"));
                    return Ok(super::terminal_error_code(&e, ExitCode::Network));
                }
                let me = crate::infra::credentials::current_user().unwrap_or_default();
                (format!("{me}/{n}"), true)
            } else if Repo::open(config::repo_dir(o, n).unwrap_or_default()).is_some() {
                if !fetch_quiet(o, n) {
                    ui::warning(
                        "fetch failed — continuing with local history (a network path was asked for but unavailable)",
                    );
                }
                (slug, true)
            } else {
                // Automatic read-only clone: **does not bind the current directory**.
                if let Err(e) = super::clone::readonly_clone(o, n) {
                    super::fix::register_terminal_api_error(&e);
                    ui::error(&format!("fetch failed: {e:#}"));
                    return Ok(super::clone::readonly_clone_error_code(&e));
                }
                (slug, true)
            }
        }
        refs::RepoSel::Local(name) => match super::clone::unique_checkout(name)? {
            Some(checkout) => (checkout.slug(), false),
            None => {
                ui::error(&format!("no local repo named `{name}` — write owner/repo."));
                return Ok(ExitCode::Ref);
            }
        },
        refs::RepoSel::Context => match super::context::resolve(&cwd) {
            Ok(c) => (c.repo, false),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                ui::hint("or fetch explicitly: agit run <owner/repo>@<ref>");
                return Ok(ExitCode::Ref);
            }
        },
    };
    let (owner, name) = match super::parse_slug(&slug) {
        Ok(v) => v,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    };
    let repo = match Repo::open(config::repo_dir(&owner, &name).unwrap_or_default()) {
        Some(r) => r,
        None => {
            ui::error(&format!("{slug} does not exist locally."));
            return Ok(ExitCode::Precondition);
        }
    };
    if networked {
        startup_notes.push(format!("  fetched the latest history of {slug}"));
    }

    // ── 2. Locate ──
    let base_name = match &spec.base {
        refs::Base::Name(b) | refs::Base::SessionBranch(b) => b.clone(),
        refs::Base::Default => {
            ui::error("run needs a ref to aim at (branch / tag / commit / #n).");
            ui::hint(&format!(
                "e.g. `agit run {slug}@main` or `agit run {slug}@v2`"
            ));
            return Ok(ExitCode::Usage);
        }
        refs::Base::At => {
            ui::error("run doesn’t take `@` (your own line belongs to `agit resume`).");
            return Ok(ExitCode::Usage);
        }
    };

    // An ordinary name must select one identity before branch continuation can take priority.
    // Tailed selectors already pass through fork resolution and cannot continue a branch head.
    // Web ids retain their declaration/version alias contract instead of resolving its branch
    // spelling again, which could introduce unrelated tag or object-prefix candidates.
    if matches!(spec.tail, refs::Tail::None)
        && !crate::domain::meta::is_bare_id(&base_name)
        && let Err(error) = refs::resolve(&repo, &spec)
    {
        ui::error(&format!(
            "failed to resolve `{slug}@{base_name}`: {error:#}"
        ));
        return Ok(super::terminal_error_code(&error, ExitCode::Ref));
    }

    // The `agit-...` id the web interface shows (session declaration / version) folds back to
    // a branch name first: both id forms and the branch-name entry point land in one arbitration,
    // instead of forking a live line as if it were a historic point.
    let web_id = base_name.clone();
    let mut folded_oid: Option<String> = None;
    let base_name = match refs::version_alias(&repo, &base_name) {
        Some(b) => {
            folded_oid = crate::domain::meta::sha_from_id(&base_name).map(str::to_string);
            startup_notes.push(format!("  {base_name} → branch `{b}`"));
            b
        }
        None => base_name,
    };
    let aligns_published_history = folded_oid.as_ref().is_some_and(|oid| {
        repo.git_opt(&["cat-file", "-e", &format!("{oid}^{{commit}}")])
            .is_some()
    });
    let history_update = aligns_published_history
        .then(|| super::migration::begin_startup_recovery(&repo, "run-history"))
        .transpose()?
        .flatten();
    // After the fold, the local branch is aligned to the OID the id names: the web interface's
    // "continue" means **the head of this line right now** — absent locally it is created there,
    // behind and fast-forwardable it is fast-forwarded. A session-declaration id carries no OID
    // (there is nothing to align to, and the `cat-file` guard blocks it), and local work that has
    // forked away is never overwritten — two lines that have each settled have no lossless
    // answer, and saying so beats picking a side for the user.
    if let Some(oid) = &folded_oid
        && aligns_published_history
    {
        let head_ref = format!("refs/heads/{base_name}");
        let cur = repo
            .git_opt(&["rev-parse", &head_ref])
            .map(|s| s.trim().to_string());
        match cur {
            None => {
                repo.git(&["branch", &base_name, oid])?;
                startup_notes.push(format!(
                    "  created local `{base_name}` at {}",
                    &oid[..9.min(oid.len())]
                ));
            }
            Some(cur) if cur != *oid => {
                if repo
                    .git_opt(&["merge-base", "--is-ancestor", &cur, oid])
                    .is_some()
                {
                    super::plumbing::update_branch_cas_and_refresh(
                        &repo, &base_name, oid, &cur, false,
                    )?;
                    startup_notes.push(format!(
                        "  fast-forwarded `{base_name}` to {}",
                        &oid[..9.min(oid.len())]
                    ));
                } else if repo
                    .git_opt(&["merge-base", "--is-ancestor", oid, &cur])
                    .is_some()
                {
                    // The local side is already ahead: the history the id names is contained in
                    // the local line, so it continues on the local head.
                    startup_notes.push(format!(
                        "  local `{base_name}` is ahead of the published tip — continuing on the local head"
                    ));
                } else {
                    ui::error(&format!(
                        "local `{base_name}` and the published tip have diverged — neither side can absorb the other"
                    ));
                    ui::hint(&format!(
                        "fork the exact published version instead: agit fork {slug}@{web_id} -b <new>"
                    ));
                    return Ok(ExitCode::Policy);
                }
            }
            _ => {}
        }
    }

    super::migration::finish_external_history_update(&repo, history_update)?;

    // ── 3. Arbitration: continue or fork ──
    let branch_exists = repo.has_ref(&format!("refs/heads/{base_name}"));
    // The shape comes from `meta.line`: the file line cannot be continued, and neither can a
    // point with no meta — that is not "the file line", it is "no declaration", and both can only
    // fork.
    let is_session_line = branch_exists
        && crate::domain::meta::line_at_ref(&repo, &format!("refs/heads/{base_name}"))
            == Some(crate::domain::meta::Line::Session);
    let is_branch_head = matches!(spec.tail, refs::Tail::None)
        && is_session_line
        && !super::branch::is_sealed(&repo, &base_name);

    // Write access decides where to go only on the branch-head path: every other shape forks
    // without asking, and asking would only add a network round trip to a `run <tag>` that could
    // have stayed offline.
    //
    // Your own namespace needs no network; someone else's namespace asks the hub's
    // write-permission gate — an org owner and a member granted this repo are both inside that
    // decision. Comparing only whether the owner is you forks an org branch you may write as if
    // it were someone else's read-only line. Network / auth errors propagate unchanged (as in
    // push / import): conflating a timeout with "read-only" opens an extra branch beside the
    // original line on the user's behalf.
    let access = if is_branch_head && !args.mine {
        match crate::infra::credentials::current_user() {
            Some(me) => Some(super::writability(&me, &owner, &name)?),
            // With no sign-in there is no identity a grant could name: only this machine's
            // `local/` namespace counts as your own.
            None if owner == "local" => Some(super::Writability::Mine),
            None => Some(super::Writability::ReadOnly),
        }
    } else {
        None
    };
    // Continuable = the shapes whose first push can land on the original line: your own, the
    // ones the hub allows, and the ones the hub does not have yet where you are the org owner
    // (push creates it under the org) — the same set of answers push / import accept.
    let can_continue = matches!(
        access,
        Some(
            super::Writability::Mine | super::Writability::Granted | super::Writability::Creatable
        )
    );

    if can_continue {
        super::echo::emit(
            "run",
            &[super::echo::Selection::new(
                format!("{slug}@{base_name}"),
                source,
            )],
        );
        startup_notes.flush();
        let head = match access {
            Some(super::Writability::Granted) => "a session branch head the hub lets you write",
            Some(super::Writability::Creatable) => {
                "a session branch head of a repo your org will create on first push"
            }
            _ => "a session branch head you can write",
        };
        println!(
            "{}",
            ui::dim(&format!("  arbitration: {head} → continue (resume)"))
        );
        return super::resume::run(super::resume::Args {
            target: Some(format!("{slug}@{base_name}")),
            as_runtime: args.as_runtime,
            cwd: args.cwd,
            no_launch: args.no_launch,
            force: false,
        });
    }

    let (full_ref, fork_base) = match resolve_fork(&spec, &slug, &base_name, &cwd)? {
        ForkResolution::Selected(full_ref, fork_base) => (full_ref, fork_base),
        ForkResolution::Refused(code) => return Ok(code),
    };
    if !legacy_output {
        let target = if matches!(spec.tail, refs::Tail::None) {
            fork_base
                .resolved
                .branch
                .as_ref()
                .unwrap_or(&fork_base.resolved.sha)
        } else {
            &fork_base.resolved.sha
        };
        super::echo::emit(
            "run",
            &[super::echo::Selection::new(
                format!("{slug}@{target}"),
                source,
            )],
        );
        startup_notes.flush();
    }

    // Everything else: it forks by necessity.
    let why = if !matches!(spec.tail, refs::Tail::None) {
        "a historic point (not a branch head)"
    } else if !repo.has_ref(&format!("refs/heads/{base_name}")) {
        // May be a tag or a sha
        "tag or historic commit"
    } else if crate::domain::meta::is_file_line_at(&repo, &format!("refs/heads/{base_name}")) {
        "the file line (no session)"
    } else if !is_session_line {
        "a point with no declared line (incomplete checkout?)"
    } else if super::branch::is_sealed(&repo, &base_name) {
        "a sealed branch"
    } else if args.mine {
        "your own copy (--mine)"
    } else if access == Some(super::Writability::ReadOnly) {
        "someone else’s branch (the hub grants you no write access)"
    } else {
        "someone else’s branch (the hub doesn’t list this repo for you)"
    };
    println!(
        "{}",
        ui::dim(&format!(
            "  arbitration: {why} → forking is mandatory (won’t continue on the original line)"
        ))
    );

    let new_name = match &args.branch {
        Some(b) => b.clone(),
        None => {
            // A `-run-1` left behind by an earlier run must not stop this one on a name
            // collision: take the first free ordinal.
            let suggestion = (1..)
                .map(|n| format!("{base_name}-run-{n}"))
                .find(|b| !repo.has_ref(&format!("refs/heads/{b}")))
                .expect("the sequence is unbounded");
            match ui::prompt::confirm(&format!("fork a new branch `{suggestion}`?"), true) {
                Ok(Some(true)) => suggestion,
                Ok(Some(false)) => {
                    println!(
                        "cancelled. to name it yourself: `agit run {} -b <name>`",
                        args.source
                    );
                    return Ok(ExitCode::Ok);
                }
                _ => {
                    ui::error("non-interactive runs must pass -b <new-branch>.");
                    ui::hint(&format!("e.g. `agit run {} -b {suggestion}`", args.source));
                    return Ok(ExitCode::Interactive);
                }
            }
        }
    };

    let Some(_) = super::fork::fork_branch(&fork_base, &full_ref, &new_name)? else {
        return Ok(ExitCode::Policy);
    };
    ui::success(&format!("forked out {slug} @ {new_name}"));

    // ── 4. Materialize + launch (= resume's loading) ──
    let rargs = super::resume::Args {
        target: Some(new_name.clone()),
        as_runtime: args.as_runtime,
        cwd: args.cwd,
        no_launch: args.no_launch,
        force: false,
    };
    match super::resume::resume_branch(&fork_base.repo, &slug, &new_name, &rargs)? {
        Some(res) => super::resume::finish_pub(res, args.no_launch),
        None => Ok(ExitCode::Precondition),
    }
}

struct RunNotes {
    defer: bool,
    notes: Vec<String>,
}

impl RunNotes {
    fn push(&mut self, note: String) {
        if self.defer {
            self.notes.push(note);
        } else {
            println!("{}", ui::dim(&note));
        }
    }

    fn flush(&mut self) {
        for note in self.notes.drain(..) {
            println!("{}", ui::dim(&note));
        }
    }
}

enum ForkResolution {
    Selected(String, super::fork::ForkBase),
    Refused(ExitCode),
}

/// Fork selection uses the actual checkout and folded branch with the requested selector.
fn resolve_fork(
    spec: &refs::RefSpec,
    slug: &str,
    base_name: &str,
    cwd: &Path,
) -> crate::Result<ForkResolution> {
    let tail = match &spec.tail {
        refs::Tail::None => String::new(),
        refs::Tail::Tilde(n) => format!("~{n}"),
        refs::Tail::Turn(n) => format!("#{}", turn_display(*n)),
        refs::Tail::Event { turn, index } => format!("#{}.{}", turn_display(*turn), index),
        refs::Tail::Range { .. } => {
            ui::error("run can’t start from a range.");
            return Ok(ForkResolution::Refused(ExitCode::Usage));
        }
        refs::Tail::Path(_) => {
            ui::error("run can’t start from an in-tree file.");
            return Ok(ForkResolution::Refused(ExitCode::Usage));
        }
    };
    let full_ref = format!("{slug}@{base_name}{tail}");
    let Some(fork_base) = super::fork::resolve_base(&full_ref, cwd)? else {
        return Ok(ForkResolution::Refused(ExitCode::Ref));
    };
    Ok(ForkResolution::Selected(full_ref, fork_base))
}

fn turn_display(n: u32) -> String {
    if n == refs::LAST_TURN {
        "-1".to_string()
    } else {
        n.to_string()
    }
}

/// Promote into your own namespace (idempotent). Reuses the `clone --mine` path.
fn ensure_mine(slug: &str) -> crate::Result<()> {
    let (o, n) = super::parse_slug(slug)?;
    let me = crate::infra::credentials::current_user()
        .ok_or_else(|| anyhow::anyhow!("--mine needs `agit login` first"))?;
    if o == me {
        return Ok(());
    }
    // Already a local checkout under your own name?
    if Repo::open(config::repo_dir(&me, &n)?).is_some() {
        return Ok(());
    }
    // Take the route: reuse the `clone` command (`--mine`). clone only fetches and does not
    // launch; run materializes the target branch itself afterwards. No directory binding: this is
    // a promotion internal to run, and binding is a declaration the user makes only by typing
    // `clone` directly.
    let code = super::clone::acquire_mine(slug)?;
    if code != ExitCode::Ok {
        anyhow::bail!("clone --mine didn’t succeed (exit {})", code.as_i32());
    }
    Ok(())
}

fn fetch_quiet(owner: &str, name: &str) -> bool {
    let Ok(dir) = config::repo_dir(owner, name) else {
        return false;
    };
    let Some(repo) = Repo::open(&dir) else {
        return false;
    };
    let client = crate::hub::Client::from_env();
    if crate::hub::identity::verify_slug(&repo, &client, owner, name).is_err() {
        return false;
    }
    match crate::hub::git::run(&repo, &["fetch", "origin", "--tags", "--prune"]) {
        Ok(o) => o.ok(),
        Err(_) => false,
    }
}
