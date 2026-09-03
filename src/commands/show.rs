//! `agit show` — read a session, rendered in the terminal as a conversation.
//!
//! Two views: line output by default (pipeable), `--tui` for full-screen interaction.
//!
//! The full-screen one lives in `tui::screens::transcript`, shared with Timeline's Enter.
//! Terminal state goes through `tui::term::Guard` (RAII) — after raw mode is entered, a panic or
//! an early return leaves the terminal unusable (no echo, no cursor).

use super::CmdResult;
use crate::domain::link::{self, Link};
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::session;
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::{ExitCode, adapter, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Session id / prefix / transcript path (default: the current directory's repo)
    #[arg(value_name = "owner/repo@ref | session")]
    pub target: Option<String>,

    /// Only sessions of one local agent
    #[arg(long, value_name = "owner/agent")]
    pub agent: Option<String>,

    /// Full-screen interactive browsing
    #[arg(long)]
    pub tui: bool,

    /// Max chars per message
    #[arg(long, default_value = "2000", value_name = "chars")]
    pub max_chars: usize,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    // `show` has its own `--tui` flag because, unlike the other entry points, it never opens the
    // interface implicitly. Once it is requested, the common arbitration still owns every other
    // rule: an explicit off switch wins, an agent-session guard is overridden, and a missing
    // terminal is an error instead of a silent fallback.
    let use_tui = match tui_verdict(args.tui, crate::tui::Signals::from_process()) {
        None | Some(crate::tui::Verdict::Skip) => false,
        Some(crate::tui::Verdict::Enter) => true,
        Some(crate::tui::Verdict::Explain(note)) => {
            crate::tui::warn_skipped(&note);
            false
        }
        Some(crate::tui::Verdict::NoTerminal) => {
            ui::error("--tui needs an interactive terminal.");
            ui::hint("in pipes or CI, use the default line output");
            return Ok(ExitCode::Interactive);
        }
    };
    // Reference-syntax fast path: `ref#n` / `ref#n.k` / `ref:path` / a repo qualifier with `@`.
    // Everything that enters this path resolves by the PRD reference syntax (see domain::refs).
    if let Some(t) = &args.target
        && (t.contains('#') || t.contains(':') || t.contains('@'))
    {
        return Ok(show_ref(t, &args).unwrap_or_else(|| {
            ui::error(&format!(
                "could not resolve `{t}` as a local repository reference."
            ));
            ExitCode::Ref
        }));
    }
    // A bare branch name / tag / sha prefix resolves against the context repo first —
    // `agit show refund-fix` must show that branch head's VIEW, not a session link in the store
    // that happens to carry the same name. Only when it does not resolve does this fall back to
    // the store (whose target is a session id).
    if let Some(t) = &args.target
        && args.agent.is_none()
        && !use_tui
    {
        match names_local_ref(t) {
            Ok(true) => return Ok(show_ref(t, &args).unwrap_or(ExitCode::Ref)),
            Ok(false) => {}
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        }
    }
    // Two sources: a local repo (the content is inside it), or a link in the local store (which
    // resolves back to the original in the runtime's directory).
    let repo = match (&args.agent, args.target.is_none()) {
        (Some(slug), _) => {
            let (o, n) = super::parse_slug(slug)?;
            match super::clone::local_store(&o, &n)? {
                Some(r) => Some(r),
                None => {
                    ui::error(&format!("nothing local named {o}/{n}."));
                    ui::hint(&format!("fetch it first: `agit clone {o}/{n}`"));
                    return Ok(ExitCode::Failure);
                }
            }
        }
        (None, false) => None,
        (None, true) => match current_context_repo(&cwd)? {
            Some(r) => Some(r),
            None => return Ok(ExitCode::Ref),
        },
    };

    // For a store link the header shows the little the link knows (the agent, the working
    // directory) — that transcript has not been fixed into a snapshot by a commit.
    let mut link_info: Option<Link> = None;

    // `--tui` **holds on both sources**, so this check comes before the source split.
    //
    // Scoped to the `--agent` arm instead, `agit show --tui` without `--agent` silently degrades
    // to line output: an explicitly given flag does nothing, and in a pipe it does not even
    // return `Interactive` — a script concludes the interface was opened.
    if use_tui {
        let sessions = match &repo {
            Some(r) => session::list(r),
            // With no target the agit repo bound to the current working directory is already
            // locked in; only an explicit session id goes through the machine-wide store to
            // reach a runtime transcript.
            None => adopted_sessions()?,
        };
        let start = match args.target.as_deref() {
            Some(selector) => session_index(&sessions, selector)?,
            None => 0,
        };
        if sessions.is_empty() {
            println!("no sessions.");
            return Ok(ExitCode::Ok);
        }
        // The two sources' transcripts have **different forms**: the one in the repo is an
        // envelope, materialized and then unwrapped; a store link points at the runtime's native
        // transcript, read directly. Pick the wrong side and every line renders as unreadable.
        return match &repo {
            Some(r) => crate::tui::screens::transcript::browse_repo(r, &sessions, start),
            None => crate::tui::screens::transcript::browse_native(&sessions, start),
        };
    }

    let target = match &repo {
        Some(r) => match &args.target {
            Some(t) => session::find(r, t)?,
            None => match session::latest(r) {
                Some(s) => s,
                None => {
                    println!("this agent has no sessions.");
                    return Ok(ExitCode::Ok);
                }
            },
        },
        None => {
            let Some(store) = Store::open()? else {
                println!("no sessions adopted yet.");
                ui::hint("`agit import <session-id> -n <agent-name>`");
                return Ok(ExitCode::Ok);
            };
            // A target is guaranteed here: zero-argument show resolves a local repo above.
            let t = args
                .target
                .as_deref()
                .expect("store-backed show always has an explicit target");
            let lk = link::find(&store, t)?;
            let path = lk.resolve().ok_or_else(|| {
                anyhow::anyhow!(
                    "transcript file for {} not found",
                    link::short(&lk.session_id)
                )
            })?;
            let stored = session::Stored {
                id: lk.session_id.clone(),
                path,
                runtime: lk.source.clone(),
                mtime: std::time::SystemTime::now(),
                branch: None,
            };
            link_info = Some(lk);
            stored
        }
    };

    // `session/log.jsonl` in the repo is an envelope (see [`crate::domain::transcript`]) —
    // unwrap it back to raw lines before the parse/render pipeline. Unwrapping is lossy: reading
    // history tolerates faults line by line, and one corrupt line must not sink the whole read.
    let text = session_text(repo.as_ref(), &target)?;
    let rt = adapter::infer_runtime(&text).unwrap_or(target.runtime.as_str());
    let parsed = adapter::get(rt)?.parse(&text)?;

    // ─── Header ───
    let mut kv: Vec<(&str, String)> = vec![
        ("session", ui::bold(&target.id)),
        ("runtime", target.runtime.clone()),
        ("recorded", ui::ago(target.mtime)),
    ];
    // When the content comes from a repo the session metadata sits in that branch tip's
    // `session/meta.json`; a store link (a live transcript in the runtime's directory) has no
    // meta — that one has not been fixed by a commit.
    let header = repo.as_ref().and_then(|r| header_meta(r, &target));
    match (&header, &link_info) {
        (Some((s, version)), _) => {
            kv.push(("code repo", ui::tilde(std::path::Path::new(&s.cwd))));
            if let Some(c) = &s.code {
                kv.push(("code", c.clone()));
            }
            if let Some(version) = version {
                kv.push(("version", version.clone()));
            }
        }
        (None, Some(lk)) => {
            if let Some(c) = &lk.cwd {
                kv.push(("code repo", ui::tilde(std::path::Path::new(c))));
            }
            match &lk.agent {
                Some(a) => kv.push(("AGENT", a.clone())),
                None => kv.push(("AGENT", ui::dim("never versioned").to_string())),
            }
        }
        (None, None) => {}
    }
    kv.push((
        "file",
        match &target.branch {
            // A branch's content is read by ref; with a worktree the file is right there.
            Some(_) if target.path.is_file() => ui::tilde(&target.path),
            Some(branch) => format!("{branch}:{}", meta::LOG_FILE),
            None => ui::tilde(&target.path),
        },
    ));
    print!("{}", ui::table::key_values(&kv));

    // Lossy notice: how many events the IR dropped.
    let c = parsed.counts();
    if c.dropped > 0 {
        println!(
            "{}",
            ui::dim(&format!(
                "  ({} vendor-proprietary events aren’t rendered here — the raw transcript still has them whole)",
                c.dropped
            ))
        );
    }

    ui::section("conversation");
    print!(
        "{}",
        ui::transcript::render_transcript(&parsed, args.max_chars)
    );

    // Web link: a published session can be read in a browser.
    if let Some(url) = repo.as_ref().and_then(|r| web_url(r, &target.id)) {
        println!("\n{}", ui::dim(&format!("web: {url}")));
    }
    Ok(ExitCode::Ok)
}

/// Decide whether `show` enters its explicitly requested interface.
///
/// `Signals::forced` covers the global flag before the subcommand and an exported `AGIT_TUI=1`;
/// `explicit` covers `show --tui`, whose subcommand-local flag does not write that environment
/// variable. Both requests have the same precedence once combined.
fn tui_verdict(explicit: bool, mut signals: crate::tui::Signals) -> Option<crate::tui::Verdict> {
    if !explicit && !signals.forced {
        return None;
    }
    signals.forced = true;
    Some(crate::tui::verdict(&signals))
}

/// Normalize a prefix into the full session identity in the list, then return its position.
fn session_index(sessions: &[session::Stored], selector: &str) -> crate::Result<usize> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("session selector must not be empty");
    }
    let matches: Vec<&str> = sessions
        .iter()
        .filter(|session| session.id.starts_with(selector))
        .map(|session| session.id.as_str())
        .collect();
    let id = match matches.as_slice() {
        [] => anyhow::bail!("no session matches `{selector}`.\n  `agit log` lists what you have."),
        [id] => *id,
        many => anyhow::bail!(
            "`{selector}` matches {} sessions; give a longer prefix",
            many.len()
        ),
    };
    sessions
        .iter()
        .position(|session| session.id == id)
        .ok_or_else(|| anyhow::anyhow!("selected session disappeared from the list"))
}

/// Read one session's content, returning raw line text.
///
/// A session from a repo really is the `session/log.jsonl` envelope file — unwrap it back to raw
/// lines, skipping bad ones ([`transcript::unwrap_lossy`]). A store link points at a live
/// transcript in the runtime's directory, and a selector given directly names some file the user
/// holds — both are read verbatim; the envelope discipline governs only files inside the repo.
fn session_text(repo: Option<&Repo>, target: &session::Stored) -> crate::Result<String> {
    let from_repo = repo.is_some_and(|r| {
        target.branch.is_some()
            || target.path == r.root().join(meta::LOG_FILE)
            || target.path == r.root().join(meta::LEGACY_LOG_FILE)
    });
    let raw = match (repo.filter(|_| from_repo), &target.branch) {
        // A branch's content is read by its own ref — not by whichever branch is checked out.
        (Some(repo), Some(branch)) => crate::domain::storage::materialize_at(
            repo.root(),
            &format!("refs/heads/{branch}"),
            meta::LOG_FILE,
        )?,
        (Some(repo), None) => {
            crate::domain::storage::materialize_worktree(repo.root(), meta::LOG_FILE)?
        }
        (None, _) => std::fs::read_to_string(&target.path)?,
    };
    Ok(if from_repo {
        transcript::unwrap_lossy(&raw).0
    } else {
        raw
    })
}

/// The web link of a published session.
fn web_url(repo: &Repo, session_id: &str) -> Option<String> {
    let remote = repo.remote_url()?;
    let trimmed = remote.trim_end_matches(".git");
    let mut parts = trimmed.rsplit('/');
    let name = parts.next()?;
    let owner = parts.next()?;
    Some(format!(
        "{}/{owner}/{name}/sessions/{session_id}",
        crate::infra::config::hub_url()
    ))
}

/// Sessions adopted in the local store, ordered by most recent activity.
///
/// Only for an explicit session selector together with `--tui`; a zero-argument `show` is already
/// locked to the repo bound to the current working directory and never guesses a session out of
/// the machine-wide store.
fn adopted_sessions() -> crate::Result<Vec<session::Stored>> {
    let Some(store) = Store::open()? else {
        return Ok(Vec::new());
    };
    let mut out: Vec<session::Stored> = link::list(&store)
        .into_iter()
        .filter_map(|lk| {
            let path = lk.resolve()?;
            let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some(session::Stored {
                id: lk.session_id,
                path,
                runtime: lk.source,
                mtime,
                branch: None,
            })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    Ok(out)
}

/// Resolve the local AgentGit repo bound to the current working directory.
///
/// A zero-argument `show` must stay inside the current workspace; falling back to the
/// newest link in the machine-wide store can display an unrelated project's transcript.
fn current_context_repo(cwd: &std::path::Path) -> crate::Result<Option<Repo>> {
    let slug = match super::context::repo_for(cwd) {
        Ok(repo) => super::context::qualify(&repo),
        Err(error) => {
            ui::error(&format!("{error:#}"));
            ui::hint(
                "name a session explicitly, or bind this directory with `agit init` / `agit clone`",
            );
            return Ok(None);
        }
    };
    let (owner, name) = super::parse_slug(&slug)?;
    match super::clone::local_store(&owner, &name)? {
        Some(repo) => Ok(Some(repo)),
        None => {
            ui::error(&format!("{slug} is bound here but has no local repo."));
            ui::hint(&format!("fetch it first: `agit clone {slug}`"));
            Ok(None)
        }
    }
}

/// The local repo a reference lives in: `owner/repo@` names it, otherwise it is the context repo.
///
/// Context only has to resolve a **repo** ([`super::context::repo_for`]): the branch is already
/// written in the reference, and a workspace bound to a directory with no pinned branch must
/// still be able to `agit show <branch>`.
fn open_ref_repo(spec: &refs::RefSpec) -> crate::Result<Repo> {
    let (o, n) = match &spec.repo {
        refs::RepoSel::Slug(o, n) => (o.clone(), n.clone()),
        _ => {
            let cwd = std::env::current_dir()?;
            super::parse_slug(&super::context::repo_for(&cwd)?)?
        }
    };
    let dir = crate::infra::config::repo_dir(&o, &n)?;
    Repo::open(&dir).ok_or_else(|| anyhow::anyhow!("{o}/{n} doesn’t exist locally."))
}

/// Whether a bare name is a reference the context repo can resolve (branch / tag / sha prefix).
///
/// Three answers: `Ok(true)` resolves; `Ok(false)` is **a plain miss** (the name is no branch, no
/// tag, no sha prefix, or there is no context repo at all) — the caller then looks it up in the
/// store by session id; `Err` is the resolution itself failing (a branch and a tag with the same
/// name, corrupt history), which must stop and tell the user rather than silently taking another
/// path — the end of that path is the line "no sessions adopted yet".
fn names_local_ref(t: &str) -> crate::Result<bool> {
    let Ok(spec) = refs::parse(t) else {
        return Ok(false);
    };
    let spec = super::context::substitute_at(spec)?;
    let Ok(repo) = open_ref_repo(&spec) else {
        return Ok(false);
    };
    match refs::resolve(&repo, &spec) {
        Ok(_) => Ok(true),
        Err(e) if refs::is_not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The metadata and version ID the header wants: the header reads the tip of whichever branch
/// the body was read from — the main checkout sits on main, and its `session/meta.json` speaks
/// for the file line, not for this session.
fn header_meta(repo: &Repo, target: &session::Stored) -> Option<(meta::Meta, Option<String>)> {
    match &target.branch {
        Some(branch) => {
            let refname = format!("refs/heads/{branch}");
            let snap = meta::read_at_ref(repo, &refname)?;
            let version = repo
                .git_opt(&["rev-parse", &refname])
                .map(|sha| meta::id_from_sha(sha.trim()));
            Some((snap, version))
        }
        None => {
            let snap = meta::resolve(repo.root()).ok()?;
            let version = repo
                .git_opt(&["rev-parse", "HEAD"])
                .map(|sha| meta::id_from_sha(sha.trim()));
            Some((snap, version))
        }
    }
}

/// Render by reference syntax: a branch or history point, `#n` (one turn), `#n.k` (one event),
/// `:path`.
/// `Some(exit code)` = handled (an already printed error included), `None` = fall back to the
/// legacy path.
fn show_ref(t: &str, args: &Args) -> Option<ExitCode> {
    let spec = match super::context::substitute_at(refs::parse(t).ok()?) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Some(ExitCode::Ref);
        }
    };
    let repo = match open_ref_repo(&spec) {
        Ok(repo) => repo,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Some(ExitCode::Ref);
        }
    };
    // Turn-level references (`#n` / `#n.k`) are handled first: they do **not** go through refs'
    // resolution by position, see [`turn_events`].
    match &spec.tail {
        refs::Tail::Turn(n) => {
            let (turn, events) = match turn_events(&repo, &spec, *n) {
                Ok(v) => v,
                Err(error) => {
                    ui::error(&format!(
                        "cannot read turn {}'s LOG: {error:#}",
                        turn_label(*n)
                    ));
                    return Some(ExitCode::Precondition);
                }
            };
            println!("{}", ui::dim(&format!("  turn {turn}")));
            render_text(&turn_text(&events), args.max_chars);
            return Some(ExitCode::Ok);
        }
        refs::Tail::Event { turn: n, index } => {
            let (turn, events) = match turn_events(&repo, &spec, *n) {
                Ok(v) => v,
                Err(error) => {
                    ui::error(&format!(
                        "cannot read turn {}'s LOG: {error:#}",
                        turn_label(*n)
                    ));
                    return Some(ExitCode::Precondition);
                }
            };
            let Some(l) = events.get((*index as usize).saturating_sub(1)) else {
                ui::error(&format!("turn {turn} has no event #{index}."));
                return Some(ExitCode::Ref);
            };
            let Ok(env) = serde_json::from_str::<transcript::Envelope>(l) else {
                ui::error("that line is not a valid envelope.");
                return Some(ExitCode::Failure);
            };
            println!("{}", env.content);
            return Some(ExitCode::Ok);
        }
        _ => {}
    }

    let resolved = match refs::resolve(&repo, &spec) {
        Ok(r) => r,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Some(ExitCode::Ref);
        }
    };

    // `:path`: the file in the tree **verbatim**, exactly the semantics of `git show <sha>:<path>`.
    //
    // This takes the raw reader rather than [`Repo::show_result`]: that one resolves the names
    // `LOG` / `VIEW` into this line's logical event sequence (v0 lands in `session/log.jsonl`),
    // and the tree of a v0 session line may perfectly well hold a separate root `LOG` file the
    // author committed. Whoever wants that transcript types `agit show <ref>` / `agit export`;
    // whoever types `:LOG` wants the blob in the tree.
    if let Some(p) = &resolved.path {
        match repo.show_raw_result(&resolved.sha, p) {
            Ok(Some(text)) => {
                print!("{text}");
                return Some(ExitCode::Ok);
            }
            Ok(None) => {
                ui::error(&format!("`{p}` is not in the tree at this point."));
                ui::hint(
                    "see `agit repo path` and browse inside for a `git ls-tree`-style listing",
                );
                return Some(ExitCode::Ref);
            }
            Err(error) => {
                ui::error(&format!("cannot read `{p}` at this point: {error:#}"));
                return Some(ExitCode::Precondition);
            }
        }
    }

    // The point as a whole: rendered as its VIEW (the world `resume` sees).
    // VIEW is a deliberate visibility boundary.  Missing objects, bad hashes, limits,
    // malformed sequences, or events unreachable from LOG must fail closed rather than
    // widening the display to the complete LOG.
    let env = match point_view(&repo, &resolved.sha) {
        Ok(view) => view,
        Err(error) => {
            // A session line fresh out of `import` / `new` with no turn settled yet: having no
            // VIEW is its normal state, not corruption.
            if meta::read_at_ref(&repo, &resolved.sha)
                .is_some_and(|m| !m.is_file_line() && m.turn.is_none())
            {
                println!("{}", ui::dim(&format!("  {t}: no turns settled yet")));
                return Some(ExitCode::Ok);
            }
            ui::error(&format!("cannot read this point's VIEW: {error:#}"));
            return Some(ExitCode::Precondition);
        }
    };
    let (text, _) = transcript::unwrap_lossy(&env);
    render_text(&text, args.max_chars);
    Some(ExitCode::Ok)
}

/// The turn ordinal for display: `#-1` is [`refs::LAST_TURN`] (`u32::MAX`) inside, which printed
/// straight out reads 4294967295.
fn turn_label(n: u32) -> String {
    if n == refs::LAST_TURN {
        "-1".into()
    } else {
        n.to_string()
    }
}

/// Where `<ref>#n` / `<ref>#n.k` takes its material: returns (the real turn ordinal, that turn's
/// envelope lines).
///
/// `n` is the turn ordinal `agit log` prints — the `turn` field in `session/meta.json`, **not**
/// the nth commit on the first-parent chain. A branch also carries a birth commit, `-m` file
/// commits and merge commits, none of which take a turn ordinal, so the two numberings must come
/// apart on any history that is not pure turns: after `agit init` + `agit import -b s`, `s#1` by
/// position points at the init commit, which has no LOG at all, and **no n at all** points at
/// turn 2.
///
/// So this does not take the commit [`refs::resolve`] picks by position; it hands the **branch
/// head** together with the turn ordinal the user typed to
/// [`crate::commands::merge::turn_lines`] (`cherry-pick` / `revert` take theirs the same way),
/// which finds that turn by the `turn` field.
fn turn_events(repo: &Repo, spec: &refs::RefSpec, n: u32) -> crate::Result<(u32, Vec<String>)> {
    let head = refs::resolve(
        repo,
        &refs::RefSpec {
            tail: refs::Tail::None,
            ..spec.clone()
        },
    )?
    .sha;
    let turn = refs::real_turn(repo, &head, n)?;
    Ok((turn, turn_envelopes(repo, &head, turn)?))
}

/// One turn's LOG events (enveloped JSONL lines).
///
/// The transcript screen uses this too — "Enter to see this turn" and `agit show <ref>#n` must be
/// the same content; two implementations of it drift apart sooner or later over "which events
/// this turn actually contains". **The `turn` here is the real turn ordinal** (`meta.turn`), not
/// the printed position; [`turn_events`] owns that mapping, and the TUI side already holds
/// `meta.turn`.
pub(crate) fn turn_envelopes(repo: &Repo, head: &str, turn: u32) -> crate::Result<Vec<String>> {
    let log = crate::domain::storage::materialize_at(repo.root(), head, meta::LOG_FILE)?;
    let lines: Vec<&str> = log.split_inclusive('\n').collect();
    crate::commands::merge::turn_lines(repo, head, turn)?
        .into_iter()
        .map(|index| {
            lines
                .get(index)
                .map(|line| (*line).to_owned())
                .ok_or_else(|| anyhow::anyhow!("turn {turn} names missing LOG event {index}"))
        })
        .collect()
}

/// Unwrap a turn's envelope lines back to raw transcript lines — the render pipeline recognizes
/// the runtime's own line format, and envelope JSON infers no runtime, so feeding it in directly
/// prints a blank stretch.
fn turn_text(events: &[String]) -> String {
    let (text, _) = transcript::unwrap_lossy(&events.concat());
    text
}

fn point_view(repo: &Repo, sha: &str) -> crate::Result<String> {
    repo.show_result(sha, meta::VIEW_FILE)?
        .ok_or_else(|| anyhow::anyhow!("this point has no VIEW"))
}

/// Line rendering (pipeable): raw transcript lines through the parse/render pipeline.
fn render_text(text: &str, max_chars: usize) {
    print!("{}", rendered(text, max_chars));
}

/// Text that cannot be parsed comes back unchanged — better that than printing nothing.
fn rendered(text: &str, max_chars: usize) -> String {
    let rt = adapter::infer_runtime(text).unwrap_or("codex");
    match adapter::get(rt).and_then(|a| a.parse(text)) {
        Ok(parsed) => ui::transcript::render_transcript(&parsed, max_chars),
        Err(_) => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    /// The header follows the body: with the main checkout sitting on main (the file line),
    /// `show` of a session branch takes the meta and version ID from that branch's tip, not from
    /// the main checkout's.
    #[test]
    fn the_header_reads_the_branch_the_body_came_from() {
        use crate::domain::meta::{self, Meta};
        use crate::domain::repo::Repo;
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(r.root(), &Meta::new_file_line()).unwrap();
        r.add_all().unwrap();
        r.commit("init").unwrap();
        r.git(&["checkout", "--quiet", "-b", "s1", "main"]).unwrap();
        let mut snap = Meta::new_session_line("codex".into(), "/the/project".into());
        snap.session = format!("{}{}", meta::ID_PREFIX, "d".repeat(meta::ID_HEX_LEN));
        meta::write(r.root(), &snap).unwrap();
        r.add_all().unwrap();
        r.commit("session").unwrap();
        r.git(&["checkout", "--quiet", "main"]).unwrap();

        let target = crate::domain::session::find(&r, "s1").unwrap();
        let (header, version) = super::header_meta(&r, &target).unwrap();
        assert!(header.is_session_line());
        assert_eq!(header.cwd, "/the/project");
        let tip = r.git(&["rev-parse", "refs/heads/s1"]).unwrap();
        assert_eq!(
            version.as_deref(),
            Some(meta::id_from_sha(tip.trim()).as_str())
        );
    }

    use crate::adapter;
    use crate::domain::meta::{self, Meta};
    use crate::domain::repo::Repo;
    use crate::domain::session;
    use crate::domain::transcript;

    fn tui_signals() -> crate::tui::Signals {
        crate::tui::Signals {
            interactive: true,
            forced: false,
            off: None,
            agent_session: None,
        }
    }

    #[test]
    fn an_explicit_show_request_keeps_the_common_tui_precedence() {
        assert_eq!(super::tui_verdict(false, tui_signals()), None);
        assert_eq!(
            super::tui_verdict(true, tui_signals()),
            Some(crate::tui::Verdict::Enter)
        );

        let mut inside_agent = tui_signals();
        inside_agent.agent_session = Some(("AGIT_SESSION", "nana/payments@work".into()));
        assert_eq!(
            super::tui_verdict(true, inside_agent),
            Some(crate::tui::Verdict::Enter),
            "the explicit request overrides only the agent-session guard"
        );

        let mut off = tui_signals();
        off.off = Some("--no-tui");
        assert_eq!(
            super::tui_verdict(true, off),
            Some(crate::tui::Verdict::Skip),
            "an explicit off switch still wins"
        );

        let mut pipe = tui_signals();
        pipe.interactive = false;
        assert_eq!(
            super::tui_verdict(true, pipe),
            Some(crate::tui::Verdict::NoTerminal),
            "a requested interface cannot silently degrade in a pipe"
        );

        let mut global = tui_signals();
        global.forced = true;
        assert_eq!(
            super::tui_verdict(false, global),
            Some(crate::tui::Verdict::Enter),
            "the global flag before `show` is a request too"
        );
    }

    #[test]
    fn max_chars_bounded_by_default() {
        // A full transcript can run to megabytes; dumped whole to the terminal it is unreadable.
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            a: super::Args,
        }
        let w = W::parse_from(["x"]);
        assert!(w.a.max_chars > 0 && w.a.max_chars <= 10000);
    }

    fn claim() -> String {
        format!("{}{}", meta::ID_PREFIX, "b".repeat(meta::ID_HEX_LEN))
    }

    fn stored(id: &str) -> session::Stored {
        session::Stored {
            id: id.into(),
            path: std::path::PathBuf::from(id),
            runtime: "codex".into(),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            branch: None,
        }
    }

    #[test]
    fn tui_session_selector_requires_one_match() {
        let sessions = [stored("abc-one"), stored("abc-two")];
        assert!(super::session_index(&sessions, "missing").is_err());
        assert!(super::session_index(&sessions, "abc").is_err());
        assert_eq!(super::session_index(&sessions, "abc-t").unwrap(), 1);
    }

    const USER: &str = "{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{\"role\":\"user\",\"content\":\"PROMPT-TEXT\"}}";
    const ASST: &str = "{\"type\":\"assistant\",\"sessionId\":\"s1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"REPLY-TEXT\"}]}}";

    /// A minimal checkout: `session/meta.json` plus `session/log.jsonl` in envelope form.
    fn checkout_with_enveloped_transcript() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        let env = transcript::wrap_lines(&format!("{USER}\n{ASST}\n"), "claude-code", &claim());
        meta::ensure_session_dir(r.root()).unwrap();
        crate::domain::storage::write_snapshot(r.root(), &env, &env).unwrap();
        meta::write(
            r.root(),
            &Meta::new(claim(), "claude-code".into(), "/r".into()),
        )
        .unwrap();
        (d, r)
    }

    /// An enveloped transcript in the repo still renders as that conversation: unwrapped back to
    /// raw lines, then through the parse/render pipeline, with not one envelope key leaking into
    /// the conversation stream.
    #[test]
    fn an_enveloped_repo_transcript_renders_the_conversation() {
        let (_d, r) = checkout_with_enveloped_transcript();
        let target = session::latest(&r).unwrap();
        let text = super::session_text(Some(&r), &target).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            !text.contains("_object_hash"),
            "unwrapping leaves no envelope key: {text}"
        );

        let parsed = adapter::get("claude-code").unwrap().parse(&text).unwrap();
        let out = crate::ui::transcript::render_transcript(&parsed, 2000);
        assert!(
            out.contains("PROMPT-TEXT"),
            "the user's words must render: {out}"
        );
        assert!(
            out.contains("REPLY-TEXT"),
            "the agent's words must render: {out}"
        );
    }

    /// A v1 object name promises the complete envelope bytes; a corrupt object must be rejected,
    /// not silently skipped so the display can carry on.
    #[test]
    fn a_corrupt_v1_event_is_rejected() {
        let (_d, r) = checkout_with_enveloped_transcript();
        let env = transcript::wrap_lines(&format!("{USER}\n{ASST}\n"), "claude-code", &claim());
        let first = env.split_inclusive('\n').next().unwrap();
        let id = crate::domain::storage::event_id(first).unwrap();
        let event = r.root().join(meta::event_path(&id).unwrap());
        std::fs::write(event, b"{\"corrupt\":true}\n").unwrap();
        let target = session::latest(&r).unwrap();
        let error = super::session_text(Some(&r), &target)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&id) || error.contains("event"), "{error}");
    }

    #[test]
    fn a_broken_point_view_never_widens_to_the_log() {
        let (_d, r) = checkout_with_enveloped_transcript();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        r.add_all().unwrap();
        r.commit("valid snapshot").unwrap();
        std::fs::write(
            r.root().join(meta::VIEW_FILE),
            format!("{}\n", "0".repeat(40)),
        )
        .unwrap();
        r.add_all().unwrap();
        r.commit("broken VIEW").unwrap();
        let head = r.git(&["rev-parse", "HEAD"]).unwrap();

        assert!(
            r.show_result(head.trim(), meta::LOG_FILE)
                .unwrap()
                .is_some()
        );
        let error = super::point_view(&r, head.trim()).unwrap_err();
        assert!(error.to_string().contains("not reachable"));
    }

    fn user_line(turn: u32) -> String {
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"PROMPT-{turn}\"}}}}"
        )
    }

    fn assistant_line(turn: u32) -> String {
        format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"REPLY-{turn}\"}}]}}}}"
        )
    }

    /// The minimal history of `agit init` + `agit import -b s` + two settled turns.
    ///
    /// The first-parent chain is [init, claim, turn 1, turn 2] — **four commits, two turn
    /// ordinals**. That is the shape of any real branch, and exactly where "counting by position"
    /// and "counting by the `turn` field" come apart.
    fn init_claim_and_two_turns() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::ensure_session_dir(r.root()).unwrap();

        // 1) main from `agit init`: the file line, with no LOG/VIEW in the tree.
        meta::write(r.root(), &Meta::new_file_line()).unwrap();
        std::fs::write(r.root().join("AGENTS.md"), "hi\n").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: init").unwrap());

        // 2) The claim commit of `agit import -b s`: a session line whose identity is not
        //    claimed yet, still with no LOG.
        r.git(&["checkout", "-q", "-b", "s"]).unwrap();
        meta::write(
            r.root(),
            &Meta::new_session_line("claude-code".into(), "/r".into()),
        )
        .unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: claim session line").unwrap());

        // 3)/4) Two settled turns. The log is append-only, so the tree at the second turn
        //       holds all four lines.
        let mut raw = String::new();
        for turn in 1..=2u32 {
            raw.push_str(&format!("{}\n{}\n", user_line(turn), assistant_line(turn)));
            let env = transcript::wrap_lines(&raw, "claude-code", &claim());
            crate::domain::storage::write_snapshot(r.root(), &env, &env).unwrap();
            let mut m = Meta::new(claim(), "claude-code".into(), "/r".into());
            m.turn = Some(turn);
            meta::write(r.root(), &m).unwrap();
            r.add_all().unwrap();
            assert!(r.commit(&format!("agit: turn {turn}")).unwrap());
        }
        (d, r)
    }

    /// The n in `<ref>#n` is the turn ordinal `agit log` prints, not the nth commit on the
    /// first-parent chain.
    ///
    /// # What this pins
    ///
    /// The branch also carries the birth commit of `agit init` and `agit: claim session line`,
    /// and neither takes a turn ordinal. Resolved by position, `s#1` points at the init commit,
    /// which has no LOG at all (`cannot inspect <sha>:LOG` on the spot), `s#2` points at the
    /// claim commit (printing `turn 2` and a blank stretch), and `s#3` reports "no turn 3" —
    /// **no n at all** reads turn 2. So this asserts turn by turn that what comes back is that
    /// turn's own text, with no other turn's content mixed in.
    #[test]
    fn a_turn_ref_names_the_turn_number_not_the_nth_commit() {
        let (_d, r) = init_claim_and_two_turns();
        assert_eq!(
            r.git(&["rev-list", "--first-parent", "--count", "s"])
                .unwrap()
                .trim(),
            "4",
            "precondition: four commits, two turn ordinals — the two numberings come apart"
        );

        for turn in 1..=2u32 {
            let spec = crate::domain::refs::parse(&format!("s#{turn}")).unwrap();
            let (n, events) = super::turn_events(&r, &spec, turn)
                .unwrap_or_else(|e| panic!("`s#{turn}` must resolve to turn {turn}: {e:#}"));
            assert_eq!(n, turn);
            assert_eq!(
                events.len(),
                2,
                "one turn is two events: {}",
                events.concat()
            );
            // What is asserted is what `agit show <ref>#n` actually prints: envelope unwrapped,
            // then through the render pipeline. Rendering envelope JSON directly infers no
            // runtime and prints a blank stretch.
            let out = super::rendered(&super::turn_text(&events), 2000);
            assert!(out.contains(&format!("PROMPT-{turn}")), "{out}");
            assert!(out.contains(&format!("REPLY-{turn}")), "{out}");
            assert!(
                !out.contains(&format!("PROMPT-{}", 3 - turn)),
                "no other turn may mix in: {out}"
            );
        }

        // Only two turns exist: turn 3 must say "no turn 3" rather than read the third commit
        // on the chain.
        let spec = crate::domain::refs::parse("s#3").unwrap();
        let e = super::turn_events(&r, &spec, 3).unwrap_err().to_string();
        assert!(e.contains("no turn 3"), "{e}");

        // `#-1` lands on the last turn.
        let spec = crate::domain::refs::parse("s#-1").unwrap();
        let (n, events) = super::turn_events(&r, &spec, crate::domain::refs::LAST_TURN).unwrap();
        assert_eq!(n, 2);
        let (raw, _) = transcript::unwrap_lossy(&events.concat());
        assert!(raw.contains("REPLY-2"), "{raw}");
    }

    /// A live transcript reached by a store link or a direct path is read verbatim, never
    /// through envelope unwrapping.
    #[test]
    fn a_live_transcript_file_is_read_verbatim() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("live.jsonl");
        std::fs::write(&f, format!("{USER}\n")).unwrap();
        let target = session::Stored {
            id: "s1".into(),
            path: f,
            runtime: "claude-code".into(),
            mtime: std::time::SystemTime::now(),
            branch: None,
        };
        let text = super::session_text(None, &target).unwrap();
        assert_eq!(
            text,
            format!("{USER}\n"),
            "a file outside the repo must not change a byte"
        );
    }
}
