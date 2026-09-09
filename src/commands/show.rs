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
    /// Session id, prefix, path or ref; omitted targets use the branch in AGIT_SESSION.
    #[arg(value_name = "owner/repo@ref | session")]
    pub target: Option<String>,

    /// Only sessions of one local agent
    #[arg(long, value_name = "owner/agent")]
    pub agent: Option<String>,

    /// Full-screen interactive browsing
    #[arg(long)]
    pub tui: bool,

    /// Render the complete LOG instead of the saved VIEW.
    #[arg(long)]
    pub log_only: bool,

    /// Emit native JSONL from the selected evidence without headers or truncation.
    #[arg(long, conflicts_with = "max_chars")]
    pub raw: bool,

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
    if args.raw && use_tui {
        ui::error("--raw cannot be combined with an active --tui request.");
        return Ok(ExitCode::Usage);
    }
    // Reference-syntax fast path: `ref#n` / `ref#n.k` / `ref:path` / a repo qualifier with `@`.
    // Everything that enters this path resolves by the PRD reference syntax (see domain::refs).
    if let Some(t) = &args.target
        && (t.contains('#') || t.contains(':') || t.contains('@'))
    {
        return Ok(show_ref(t, &args, use_tui).unwrap_or_else(|| {
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
    {
        match names_local_ref(t) {
            Ok(true) => return Ok(show_ref(t, &args, use_tui).unwrap_or(ExitCode::Ref)),
            Ok(false) => {}
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        }
    }
    let selected_context = if args.target.is_none() {
        let ctx = match super::context::resolve(&cwd) {
            Ok(ctx) => ctx,
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Ref);
            }
        };
        if args.agent.as_deref().is_some_and(|agent| agent != ctx.repo) {
            ui::error(
                "--agent and AGIT_SESSION name different repositories; supply a session target explicitly.",
            );
            return Ok(ExitCode::Ref);
        }
        Some(ctx)
    } else {
        None
    };
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
            // Native transcript discovery requires an explicit session selector.
            None => adopted_sessions()?,
        };
        let start = match args.target.as_deref() {
            Some(selector) => session_index(&sessions, selector)?,
            None => {
                let branch = &selected_context
                    .as_ref()
                    .expect("omitted target has explicit context")
                    .branch;
                sessions
                    .iter()
                    .position(|session| session.branch.as_ref() == Some(branch))
                    .ok_or_else(|| anyhow::anyhow!("branch `{branch}` has no settled session"))?
            }
        };
        if sessions.is_empty() {
            println!("no sessions.");
            return Ok(ExitCode::Ok);
        }
        // The two sources' transcripts have **different forms**: the one in the repo is an
        // envelope that retains its source identity; a store link points at the runtime's native
        // transcript, read directly. Pick the wrong side and every line renders as unreadable.
        return match &repo {
            Some(r) if args.log_only => {
                let selected = &sessions[start];
                let content = read_session(Some(r), selected, true, false)?;
                crate::tui::screens::transcript::browse_snapshot(
                    selected.branch.as_deref().unwrap_or(&selected.id),
                    content.text,
                    repository_source(true),
                )
            }
            Some(r) => crate::tui::screens::transcript::browse_repo(r, &sessions, start),
            None => crate::tui::screens::transcript::browse_native(&sessions, start),
        };
    }

    let target = match &repo {
        Some(r) => match &args.target {
            Some(t) => session::find(r, t)?,
            None => session::on_branch(
                r,
                &selected_context
                    .as_ref()
                    .expect("omitted target has explicit context")
                    .branch,
            )?,
        },
        None => {
            let Some(store) = Store::open()? else {
                if args.raw {
                    ui::error("the requested native session has no adopted local link.");
                    ui::hint("adopt it first with `agit import <session-id> --link-only`");
                    return Ok(ExitCode::Ref);
                }
                println!("no sessions adopted yet.");
                ui::hint(
                    "`agit import <session-id> --from <runtime> --into <owner/repo>@<branch>`",
                );
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

    let content = read_session(repo.as_ref(), &target, args.log_only, args.raw)?;
    if args.raw {
        print!("{}", content.text);
        return Ok(ExitCode::Ok);
    }
    let parsed = if content.from_repo {
        transcript::display::parse(&content.text)?
    } else {
        let rt = adapter::infer_runtime(&content.text).unwrap_or(target.runtime.as_str());
        adapter::get(rt)?.parse(&content.text)?
    };

    let selection = match (&selected_context, &args.agent, &target.branch) {
        (Some(context), _, Some(branch)) => super::echo::Selection::new(
            format!("{}@{branch}", context.repo),
            super::echo::Source::Environment,
        ),
        (_, Some(slug), Some(branch)) => {
            super::echo::Selection::new(format!("{slug}@{branch}"), super::echo::Source::Explicit)
        }
        (_, Some(slug), None) => super::echo::Selection::new(
            format!("{slug} session={}", target.id),
            super::echo::Source::Explicit,
        ),
        _ => super::echo::Selection::new(
            format!("{} session={}", target.runtime, target.id),
            super::echo::Source::Explicit,
        ),
    };
    super::echo::emit("show", &[selection]);

    // ─── Header ───
    let mut kv: Vec<(&str, String)> = vec![
        ("session", ui::bold(&target.id)),
        ("runtime", target.runtime.clone()),
        ("recorded", ui::ago(target.mtime)),
        (
            "source",
            if content.from_repo {
                repository_source(args.log_only)
            } else {
                "live transcript"
            }
            .into(),
        ),
    ];
    // When the content comes from a repo the session metadata sits in that branch tip's
    // `session/meta.json`; a store link (a live transcript in the runtime's directory) has no
    // meta — that one has not been fixed by a commit.
    match (&content.header, &link_info) {
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
        match (&target.branch, repo.as_ref().filter(|_| content.from_repo)) {
            (Some(branch), Some(_)) => format!("{branch}:{}", sequence_file(args.log_only)),
            (None, Some(repo)) => ui::tilde(&repo.root().join(sequence_file(args.log_only))),
            (_, None) => ui::tilde(&target.path),
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
    if let Some(index) = sessions
        .iter()
        .position(|session| session.branch.as_deref() == Some(selector))
    {
        return Ok(index);
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

#[derive(Debug)]
struct SessionRead {
    text: String,
    header: Option<(meta::Meta, Option<String>)>,
    from_repo: bool,
}

/// Repository sessions expose the explicitly selected sequence; native selectors expose the live file.
/// A branch's content and header share a frozen commit even if its tip advances during the read.
fn read_session(
    repo: Option<&Repo>,
    target: &session::Stored,
    log_only: bool,
    raw_output: bool,
) -> crate::Result<SessionRead> {
    let from_repo = repo.is_some_and(|r| {
        target.branch.is_some()
            || target.path == r.root().join(meta::LOG_FILE)
            || target.path == r.root().join(meta::LEGACY_LOG_FILE)
    });
    let (raw, header) = match (repo.filter(|_| from_repo), &target.branch) {
        (Some(repo), Some(branch)) => {
            let point = repo.git(&[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{branch}^{{commit}}"),
            ])?;
            let view = point_content(repo, &point, log_only)?;
            let header = meta::read_at_ref_result(repo, &point)?
                .map(|snapshot| (snapshot, Some(meta::id_from_sha(&point))));
            (view, header)
        }
        (Some(repo), None) => {
            let view =
                crate::domain::storage::materialize_worktree(repo.root(), sequence_file(log_only))?;
            (view, Some((meta::resolve(repo.root())?, None)))
        }
        (None, _) => (std::fs::read_to_string(&target.path)?, None),
    };
    Ok(SessionRead {
        text: if from_repo && raw_output {
            transcript::unwrap_strict(&raw)?
        } else {
            raw
        },
        header,
        from_repo,
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
/// locked to the branch supplied through AGIT_SESSION and never guesses from the machine-wide store.
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

/// Open the repository selected by the supplied session environment.
fn current_context_repo(cwd: &std::path::Path) -> crate::Result<Option<Repo>> {
    let slug = match super::context::repo_for(cwd) {
        Ok(repo) => super::context::qualify(&repo),
        Err(error) => {
            ui::error(&format!("{error:#}"));
            ui::hint("name a session explicitly or set AGIT_SESSION=<owner>/<repo>@<branch>");
            return Ok(None);
        }
    };
    let (owner, name) = super::parse_slug(&slug)?;
    match super::clone::local_store(&owner, &name)? {
        Some(repo) => Ok(Some(repo)),
        None => {
            ui::error(&format!("{slug} has no local repo."));
            ui::hint(&format!("fetch it first: `agit clone {slug}`"));
            Ok(None)
        }
    }
}

/// The local repo a reference lives in: `owner/repo@` names it, otherwise it is the context repo.
///
/// An unqualified reference uses the repository supplied through AGIT_SESSION; directory state
/// cannot choose which repository owns a branch name.
fn open_ref_repo(spec: &refs::RefSpec) -> crate::Result<(Repo, String)> {
    let (o, n) = match &spec.repo {
        refs::RepoSel::Slug(o, n) => (o.clone(), n.clone()),
        _ => {
            let cwd = std::env::current_dir()?;
            super::parse_slug(&super::context::repo_for(&cwd)?)?
        }
    };
    let dir = crate::infra::config::repo_dir(&o, &n)?;
    Repo::open(&dir)
        .map(|repo| (repo, format!("{o}/{n}")))
        .ok_or_else(|| anyhow::anyhow!("{o}/{n} doesn’t exist locally."))
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
    let Ok((repo, _)) = open_ref_repo(&spec) else {
        return Ok(false);
    };
    match refs::resolve(&repo, &spec) {
        Ok(_) => Ok(true),
        Err(e) if refs::is_not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Render by reference syntax: a branch or history point, `#n` (one turn), `#n.k` (one event),
/// `:path`.
/// `Some(exit code)` = handled (an already printed error included), `None` = fall back to the
/// legacy path.
fn show_ref(t: &str, args: &Args, use_tui: bool) -> Option<ExitCode> {
    let spec = refs::parse(t).ok()?;
    let source = super::echo::Source::for_spec(&spec);
    let spec = match super::context::substitute_at(spec) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Some(ExitCode::Ref);
        }
    };
    if matches!(spec.tail, refs::Tail::Range { .. }) {
        ui::error("show does not support turn ranges; select a complete turn with <ref>#<turn>.");
        return Some(ExitCode::Usage);
    }
    if args.log_only && matches!(spec.tail, refs::Tail::Event { .. } | refs::Tail::Path(_)) {
        ui::error(
            "--log-only selects conversation history; it cannot select an event or file path.",
        );
        return Some(ExitCode::Usage);
    }
    let (repo, slug) = match open_ref_repo(&spec) {
        Ok(repo) => repo,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Some(ExitCode::Ref);
        }
    };
    if use_tui && matches!(spec.tail, refs::Tail::Event { .. } | refs::Tail::Path(_)) {
        ui::error(
            "--tui supports a session point or a complete turn; use line output for events or files.",
        );
        return Some(ExitCode::Usage);
    }
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
            if args.raw {
                return Some(print_native(&events.concat()));
            }
            let text = events.concat();
            if use_tui {
                return Some(browse_ref_text(t, text, "turn LOG"));
            }
            let base = match &spec.base {
                refs::Base::Name(name) | refs::Base::SessionBranch(name) => name.as_str(),
                _ => "HEAD",
            };
            super::echo::emit(
                "show",
                &[super::echo::Selection::new(
                    format!("{slug}@{base}#{turn}"),
                    source,
                )],
            );
            println!("{}", ui::dim(&format!("  turn {turn}")));
            return Some(render_envelopes(&text, args.max_chars));
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

    if meta::read_at_ref(&repo, &resolved.sha).is_some_and(|snapshot| snapshot.is_file_line()) {
        if args.log_only || args.raw {
            ui::error("this is a file line; --log-only and --raw require a session line.");
            return Some(ExitCode::Usage);
        }
        if use_tui {
            ui::error("this is a file line; use line output to read its tree and history.");
            return Some(ExitCode::Usage);
        }
        return Some(
            match show_file_line(&repo, &resolved.sha, t, &slug, source) {
                Ok(()) => ExitCode::Ok,
                Err(error) => {
                    ui::error(&format!("cannot read this file line: {error:#}"));
                    ExitCode::Precondition
                }
            },
        );
    }

    // The selected sequence is a visibility boundary. Unreadable VIEW content must not
    // widen to LOG; complete history is available only through an explicit request.
    let env = match point_content(&repo, &resolved.sha, args.log_only) {
        Ok(view) => view,
        Err(error) => {
            ui::error(&format!(
                "cannot read this point's {}: {error:#}",
                sequence_file(args.log_only)
            ));
            return Some(ExitCode::Precondition);
        }
    };
    if args.raw {
        return Some(print_native(&env));
    }
    if use_tui {
        return Some(browse_ref_text(t, env, repository_source(args.log_only)));
    }
    let selected_ref = if spec.tail == refs::Tail::None {
        resolved.branch.as_deref().unwrap_or(&resolved.sha)
    } else {
        &resolved.sha
    };
    super::echo::emit(
        "show",
        &[super::echo::Selection::new(
            format!("{slug}@{selected_ref}"),
            source,
        )],
    );
    if args.log_only {
        println!("{}", repository_source(true));
    }
    Some(render_envelopes(&env, args.max_chars))
}

/// Raw output retains every selected native value in order, without rendering or wrapper fields.
fn print_native(envelopes: &str) -> ExitCode {
    match transcript::unwrap_strict(envelopes) {
        Ok(text) => {
            print!("{text}");
            ExitCode::Ok
        }
        Err(error) => {
            ui::error(&format!("cannot read native JSONL: {error:#}"));
            ExitCode::Precondition
        }
    }
}

fn browse_ref_text(target: &str, text: String, source: &str) -> ExitCode {
    match crate::tui::screens::transcript::browse_snapshot(target, text, source) {
        Ok(code) => code,
        Err(error) => {
            ui::error(&format!("cannot open this transcript: {error:#}"));
            ExitCode::Failure
        }
    }
}

/// A file line has no conversation VIEW; its selected tree and history describe the point.
fn show_file_line(
    repo: &Repo,
    sha: &str,
    target: &str,
    slug: &str,
    source: super::echo::Source,
) -> crate::Result<()> {
    let tree = repo.git(&["ls-tree", "--name-only", "--full-tree", sha])?;
    let history = repo.git(&[
        "log",
        "--first-parent",
        "--max-count=5",
        "--format=%h %s",
        sha,
    ])?;
    super::echo::emit(
        "show",
        &[super::echo::Selection::new(format!("{slug}@{sha}"), source)],
    );
    println!("file line {target} ({})", &sha[..9.min(sha.len())]);
    ui::section("tree");
    print!("{tree}");
    ui::section("recent commits");
    println!("{history}");
    Ok(())
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

fn sequence_file(log_only: bool) -> &'static str {
    if log_only {
        meta::LOG_FILE
    } else {
        meta::VIEW_FILE
    }
}

fn repository_source(log_only: bool) -> &'static str {
    if log_only {
        "repository LOG"
    } else {
        "repository VIEW"
    }
}

fn point_content(repo: &Repo, sha: &str, log_only: bool) -> crate::Result<String> {
    let file = sequence_file(log_only);
    repo.show_result(sha, file)?
        .ok_or_else(|| anyhow::anyhow!("this point has no {file}"))
}

/// Saved evidence retains its native source identity until parsing is complete.
fn render_envelopes(envelopes: &str, max_chars: usize) -> ExitCode {
    match transcript::display::parse(envelopes) {
        Ok(parsed) => {
            print!("{}", ui::transcript::render_transcript(&parsed, max_chars));
            ExitCode::Ok
        }
        Err(error) => {
            ui::error(&format!("cannot render saved transcript: {error:#}"));
            ExitCode::Precondition
        }
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
        crate::domain::storage::write_snapshot(r.root(), "", "").unwrap();
        r.add_all().unwrap();
        r.commit("session").unwrap();
        r.git(&["checkout", "--quiet", "main"]).unwrap();

        let target = crate::domain::session::find(&r, "s1").unwrap();
        let (header, version) = super::read_session(Some(&r), &target, false, false)
            .unwrap()
            .header
            .unwrap();
        assert!(header.is_session_line());
        assert_eq!(header.cwd, "/the/project");
        let tip = r.git(&["rev-parse", "refs/heads/s1"]).unwrap();
        assert_eq!(
            version.as_deref(),
            Some(meta::id_from_sha(tip.trim()).as_str())
        );
    }

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
        let mut sessions = [stored("abc-one"), stored("abc-two")];
        sessions[1].branch = Some("work".into());
        assert_eq!(super::session_index(&sessions, "work").unwrap(), 1);
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

    /// Repository evidence retains source identity while rendering hides envelope fields.
    #[test]
    fn an_enveloped_repo_transcript_renders_the_conversation() {
        let (_d, r) = checkout_with_enveloped_transcript();
        let target = session::latest(&r).unwrap();
        let text = super::read_session(Some(&r), &target, false, false)
            .unwrap()
            .text;
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.contains("_object_hash"),
            "saved evidence retains its envelope before parsing: {text}"
        );

        let parsed = transcript::display::parse(&text).unwrap();
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
        let error = super::read_session(Some(&r), &target, false, false)
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
        let error = super::point_content(&r, head.trim(), false).unwrap_err();
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
            // Turn rendering parses only the selected envelopes, preserving their native sources.
            let parsed = transcript::display::parse(&events.concat()).unwrap();
            let out = crate::ui::transcript::render_transcript(&parsed, 2000);
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
        let text = super::read_session(None, &target, false, false)
            .unwrap()
            .text;
        assert_eq!(
            text,
            format!("{USER}\n"),
            "a file outside the repo must not change a byte"
        );
    }
}
