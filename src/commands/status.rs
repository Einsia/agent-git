//! `agit status` — the state of this machine at a glance.
//!
//! Answers four things: which sessions are adopted; which agent each is managed by and up to
//! which version; whether anything is committed but not pushed; which sessions in this repo are
//! still unadopted.
//!
//! Displayed session rows use bounded, read-only native snapshots. Incomplete evidence remains
//! unavailable instead of being reported as no unsettled work.

/// Internal memory-only observation runs before ordinary command startup.
#[doc(hidden)]
pub fn native_observation_worker(args: &[std::ffi::OsString]) -> Option<i32> {
    super::diff::pending::status_native_worker(args)
}

use super::CmdResult;
use super::auto_push::Off;
use crate::domain::link;
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

mod branches;
mod merges;
mod project;
mod sessions;
mod shared;

/// How many branch rows the repositories of one status page show in all.
const BRANCH_ROWS: usize = 128;

/// How many branch rows a repository may take when `used` rows are taken and `later`
/// repositories come after it: every one of those keeps a row of its own, so one repository with
/// many branches can't push the others out of the table. At least one.
fn row_limit(used: usize, later: usize) -> usize {
    BRANCH_ROWS
        .saturating_sub(used)
        .saturating_sub(later)
        .max(1)
}

/// What the table left out, by repository (`omitted`, branches) and in all (`repositories`), as
/// a warning and the command that lists the rest. `None` when nothing was.
fn left_out_of_table(omitted: &[(String, usize)], repositories: usize) -> Option<(String, String)> {
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    if omitted.is_empty() && repositories == 0 {
        return None;
    }
    let mut parts: Vec<String> = omitted
        .iter()
        .take(3)
        .map(|(slug, count)| {
            format!(
                "{count} more branch{} of {slug}",
                if *count == 1 { "" } else { "es" }
            )
        })
        .collect();
    if omitted.len() > 3 {
        parts.push(format!("more of {} other repositories", omitted.len() - 3));
    }
    if repositories > 0 {
        parts.push(format!(
            "{repositories} more repositor{}",
            if repositories == 1 { "y" } else { "ies" }
        ));
    }
    Some((
        format!(
            "status is incomplete: {} don't fit the display budget",
            parts.join(", ")
        ),
        format!(
            "`agit branch --repo {} --all` lists every branch of a repository",
            omitted
                .first()
                .map_or_else(|| "<owner/repo>".to_owned(), |(slug, _)| quote_arg(slug))
        ),
    ))
}

/// Threads a status page keeps busy at once. Its reads are short Git queries whose time is
/// mostly process start-up, so a few in flight hide that latency without loading the machine.
const PAGE_WORKERS: usize = 8;

/// Runs `work` for each of `items` on up to [`PAGE_WORKERS`] threads and hands every result to
/// `take` in item order. An item starts only while it is fewer than that many places ahead of the
/// next one `take` is due, so once `take` answers false at most that many items past it have run.
/// Without the bound, threads that finish quick items race through the rest of the list while one
/// slow item holds `take` back, and a page that stops early has inspected everything anyway.
fn in_order<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
    mut take: impl FnMut(usize, R) -> bool,
) {
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(PAGE_WORKERS)
        .min(items.len());
    if workers <= 1 {
        for (index, item) in items.iter().enumerate() {
            if !take(index, work(item)) {
                return;
            }
        }
        return;
    }
    // How many items `take` has received, and whether it stopped.
    let gate = Mutex::new((0usize, false));
    let moved = Condvar::new();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (sender, results) = std::sync::mpsc::channel();
        for _ in 0..workers {
            let (sender, next, gate, moved, work) = (sender.clone(), &next, &gate, &moved, &work);
            scope.spawn(move || {
                // A thread whose item panics stops the others, so none waits for a turn that the
                // missing result keeps from coming, and the panic reaches the caller.
                struct Stopping<'a>(&'a Mutex<(usize, bool)>, &'a Condvar);
                impl Drop for Stopping<'_> {
                    fn drop(&mut self) {
                        if std::thread::panicking() {
                            self.0.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
                            self.1.notify_all();
                        }
                    }
                }
                let _stopping = Stopping(gate, moved);
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let mut state = gate.lock().unwrap_or_else(PoisonError::into_inner);
                    while !state.1 && index >= state.0 + workers {
                        state = moved.wait(state).unwrap_or_else(PoisonError::into_inner);
                    }
                    if state.1 {
                        break;
                    }
                    drop(state);
                    if sender.send((index, work(item))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        let mut waiting = BTreeMap::new();
        let mut due = 0;
        for (index, result) in results {
            waiting.insert(index, result);
            while let Some(result) = waiting.remove(&due) {
                let more = take(due, result);
                due += 1;
                let mut state = gate.lock().unwrap_or_else(PoisonError::into_inner);
                *state = (due, !more);
                drop(state);
                moved.notify_all();
                if !more {
                    return;
                }
            }
        }
    });
}

#[derive(ClapArgs)]
pub struct Args {
    /// Also inspect runtime indexes for unadopted sessions (slower; SQLite may maintain sidecars)
    #[arg(long)]
    pub check_missing: bool,
    /// Session rows per page (text: 8; JSON: 100).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub limit: Option<u16>,
    /// Skip this many session rows.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
}

pub fn run(args: Args) -> CmdResult {
    if super::json::requested() {
        return structured(&args);
    }
    let s = ui::theme::symbols();
    let cwd = std::env::current_dir()?;
    let ClaimInventory {
        store,
        mut links,
        issues: link_issues,
    } = claim_inventory(config::store_root()?);
    let in_folder = order(&mut links, &cwd);
    // The display budget never cuts the current folder's sessions; it applies to the rest.
    let limit = (args.limit.unwrap_or(8) as usize).max(in_folder.saturating_sub(args.offset));
    let page = observe_page(
        store.as_ref(),
        &links,
        link_issues.is_empty(),
        args.offset,
        limit,
    );
    let inventory_complete = page.inventory_complete;

    // ── Who am I (PRD status, first block: the context resolution result and its route) ──
    ui::section("who am I");
    let runtime_sessions = super::context::runtime_sessions();
    match super::context::from_env_with_claims(&links, inventory_complete) {
        Ok(Some(c)) => {
            println!("  {} @ {}", c.repo, c.branch);
            println!("  {}", ui::dim(&format!("via: {}", c.via)));
            if let Some(note) = super::context::not_this_terminal(
                &format!("{}@{}", c.repo, c.branch),
                &runtime_sessions,
            ) {
                println!("  {note}");
            }
        }
        Ok(None) => {
            println!(
                "  {}",
                ui::dim("no session target supplied through AGIT_SESSION")
            );
        }
        Err(error) => println!("  session target unavailable: {error}"),
    }
    print_runtime_sessions(&runtime_sessions, !args.check_missing);
    if let Some(ws) = crate::domain::workspace::read(&cwd) {
        println!("  {}", ui::dim(&format!("bound repo: {}", ws.repo)));
    }
    if let Some(notice) = super::project::worktree_notice(&cwd) {
        println!("  {notice}");
    }
    // A folder that uploads by itself says what it doesn't upload.
    if let Some(said) = super::project::opencode_not_captured(&cwd) {
        println!("  {said}");
    }
    // Told once: branches already on the Hub that the folder they ran in keeps back.
    if let Ok(uploads) = super::project::kept_back_uploads_to_tell() {
        for upload in uploads {
            println!("  {}", upload.describe());
        }
    }
    if let Some(count) = worktree_left_out(&cwd).filter(|count| *count > 0) {
        println!(
            "  {}",
            super::front::earlier_note(count, "here", super::front::Reader::of_this_process())
        );
    }
    print_upload_waits();
    print_uploads_not_through();
    print_automatic_upload_off();
    print_held_branches();

    // ── Local store ──
    ui::section("local");
    if store.is_none() {
        println!("  no sessions adopted yet.");
        ui::hint(
            "`agit import` opens the session picker; use `agit import <id> --into <owner/repo>@<branch>` for an explicit target",
        );
    }

    let committed = links.iter().filter(|l| l.agent.is_some()).count();

    print!(
        "{}",
        ui::table::key_values(&[
            ("store", ui::tilde(&config::store_root()?)),
            (
                "adopted sessions",
                if inventory_complete {
                    format!("{} ({committed} with a recorded repository)", links.len())
                } else {
                    format!(
                        "{} observed ({committed} with a recorded repository; incomplete inventory)",
                        links.len()
                    )
                }
            ),
        ])
    );

    // ── Adopted sessions ──
    if !links.is_empty() {
        println!(
            "{}",
            ui::table::render(
                &[
                    "session",
                    "runtime",
                    "repo",
                    "branch",
                    "last commit",
                    "pending activity",
                    "local instance",
                    "project",
                    "upload"
                ],
                &page.rows
            )
        );
        // A conversation nothing saves says so, with the command that saves it. Turning saving
        // on for its folder is the person's own choice, not one this line makes for them.
        for (link, row) in links.iter().skip(args.offset).zip(&page.rows) {
            if row[5] == sessions::NOT_SAVED {
                ui::hint(&format!(
                    "{} isn't saved: nothing asked agit to save this folder's conversations. `agit import {}` saves it.",
                    link::short(&link.session_id),
                    link.session_id
                ));
            }
        }
        let leading = in_folder.saturating_sub(args.offset).min(page.rows.len());
        if leading > 0 && in_folder < links.len() {
            println!(
                "{}",
                ui::dim(&if leading == 1 {
                    "this folder's session is listed first".to_owned()
                } else {
                    format!("this folder's {leading} sessions are listed first")
                })
            );
        }
        let next = args.offset.saturating_add(limit);
        let remaining = links.len().saturating_sub(next);
        if remaining > 0 {
            let page_size = args
                .limit
                .map(|limit| format!(" --limit {limit}"))
                .unwrap_or_default();
            println!(
                "{}",
                ui::dim(&format!(
                    "… {remaining} more; next page: `agit status --offset {next}{page_size}`"
                ))
            );
        }
        print_publication_waits(&links);
    }
    // Also with no links yet: a conversation waiting for its first landing has none.
    print_privacy_waits(&links);

    if !inventory_complete {
        ui::warning("status is incomplete: some local session claims cannot be inspected");
    }

    // ── Agent repos on this machine ──
    //
    // The test for "to publish" is git's ahead / behind, not whether some staging directory
    // exists — the local repo is the authoritative copy, and whether a push succeeded shows up
    // in the refs.
    let agents = super::clone::list_local()?;
    if !agents.is_empty() {
        ui::section("agent repos");
        let mut rows = Vec::new();
        // Branches left out, by repository, and repositories left out entirely.
        let mut omitted: Vec<(String, usize)> = Vec::new();
        let mut repositories_omitted = 0;
        let mut aside_example = None;
        let mut aside_unreadable = false;
        in_order(&agents, ahead, |index, (inspected, aside)| {
            if rows.len() >= BRANCH_ROWS {
                repositories_omitted = agents.len() - index;
                return false;
            }
            let (owner, name, path) = &agents[index];
            let slug = format!("{owner}/{name}");
            let aside = aside.unwrap_or_else(|_| {
                aside_unreadable = true;
                BTreeSet::new()
            });
            let limit = row_limit(rows.len(), agents.len() - index - 1);
            match branches::settle(&Repo::at(path), inspected, BRANCH_ROWS, limit) {
                Ok(page) if page.branches.is_empty() => rows.push(vec![
                    slug,
                    "—".into(),
                    "—".into(),
                    "—".into(),
                    "no branch refs".into(),
                ]),
                Ok(page) => {
                    if page.omitted > 0 {
                        omitted.push((slug.clone(), page.omitted));
                    }
                    for branch in page.branches {
                        let state = if aside.contains(&branch.name) {
                            aside_example.get_or_insert_with(|| format!("{slug}@{}", branch.name));
                            format!("{}; withdrawn on the Hub, set aside", branch.state)
                        } else {
                            branch.state
                        };
                        rows.push(vec![
                            slug.clone(),
                            branch.name,
                            meta::short(&meta::id_from_sha(&branch.head)),
                            if branch.tracking.is_empty() {
                                "—".into()
                            } else {
                                branch.tracking
                            },
                            state,
                        ]);
                    }
                }
                Err(error) => rows.push(vec![
                    slug,
                    "—".into(),
                    "—".into(),
                    "—".into(),
                    format!("unavailable: {error:#}"),
                ]),
            }
            true
        });
        println!(
            "{}",
            ui::table::render(
                &["repo", "branch", "last commit", "tracking ref", "state"],
                &rows
            )
        );
        if let Some((warning, hint)) = left_out_of_table(&omitted, repositories_omitted) {
            ui::warning(&warning);
            ui::hint(&hint);
        }
        if let Some(example) = aside_example {
            #[cfg(windows)]
            use crate::ui::quote_powershell_argument as quote_arg;
            #[cfg(not(windows))]
            use crate::ui::session::shell_arg as quote_arg;
            ui::hint(&format!(
                "pushes leave out a branch set aside as withdrawn on the Hub; once the withdrawal is restored, `agit push {}` asks the Hub again",
                quote_arg(&example)
            ));
        }
        if aside_unreadable {
            ui::warning(
                "the record of branches withdrawn on the Hub cannot be read; set-aside branches are not marked",
            );
        }
        let shared = shared::inspect(&agents);
        if !shared.items.is_empty() {
            ui::section("shared-file changes");
            println!(
                "{}",
                ui::table::render(&["target", "path", "staged", "local bytes"], &shared.rows())
            );
        }
        ui::hint(
            "local bytes are compared without Git clean filters or line-ending conversion; content is not displayed",
        );
        if shared.incomplete {
            ui::warning(
                "shared-file inspection is incomplete; unavailable evidence does not mean unchanged",
            );
        }
        let merges = merges::page(&agents);
        if !merges.items.is_empty() {
            ui::section("merge transactions");
            println!(
                "{}",
                ui::table::render(&["target", "source", "progress"], &merges.rows())
            );
        }
        if merges.incomplete {
            ui::warning(
                "merge transaction inspection is incomplete; unavailable evidence does not mean no transaction",
            );
        }
    }

    // ── Current repo ──
    if let Some(repo) = config::repo_root() {
        let want = repo.to_string_lossy().to_string();
        let here = links
            .iter()
            .filter(|l| l.cwd.as_deref() == Some(want.as_str()))
            .count();
        ui::section("this repo");
        print!(
            "{}",
            ui::table::key_values(&[
                ("path", ui::tilde(&repo)),
                (
                    "adopted from this repo",
                    if inventory_complete {
                        here.to_string()
                    } else {
                        format!("{here} observed (incomplete inventory)")
                    },
                ),
            ])
        );
    }

    // ── Unadopted sessions (expensive, explicitly triggered) ──
    if args.check_missing {
        ui::section("unadopted sessions");
        let sp = ui::spinner("checking runtime indexes…");
        let discovery = uncaptured(&links, inventory_complete);
        let missing = &discovery.sessions;
        sp.finish_and_clear();
        if let Some(note) = super::context::runtime_session_note(&runtime_sessions) {
            println!("  {note}");
        }
        if missing.is_empty() && discovery.errors.is_empty() {
            println!(
                "  {} no unadopted sessions found in the checked indexes",
                ui::ok(s.check)
            );
        } else if !missing.is_empty() {
            println!(
                "  {} {} sessions not adopted yet",
                ui::dim(s.idle),
                missing.len()
            );
            for (rt, id) in missing.iter().take(8) {
                println!(
                    "    {} {}  {}",
                    ui::dim(s.idle),
                    link::short(id),
                    ui::dim(rt)
                );
            }
            if missing.len() > 8 {
                println!("    {}", ui::dim(&format!("… {} more", missing.len() - 8)));
            }
            ui::hint(
                "`agit import <session-id> --from <runtime> --into <owner/repo>@<branch>` lets you choose its lineage",
            );
        }
        for error in discovery.errors {
            ui::warning(&format!(
                "{} index could not be checked: {}",
                error.runtime, error.message
            ));
        }
    } else {
        ui::hint("--check-missing lists this repo’s unadopted sessions");
    }

    Ok(ExitCode::Ok)
}

/// Puts the sessions that ran in the folder of `cwd` first, then active claims ahead of recovery
/// history within each part, without using filesystem timestamps, and returns how many lead.
///
/// The folder follows the front door's rule; a home directory or filesystem root has none, so
/// nothing leads there.
fn order(links: &mut Vec<link::Link>, cwd: &std::path::Path) -> usize {
    let folder = super::front::folder(cwd).ok();
    let (mut here, mut rest): (Vec<_>, Vec<_>) = std::mem::take(links)
        .into_iter()
        .partition(|link| folder.as_deref().is_some_and(|folder| ran_in(link, folder)));
    here.sort_by_key(|link| !link.is_active());
    rest.sort_by_key(|link| !link.is_active());
    let leading = here.len();
    here.append(&mut rest);
    *links = here;
    leading
}

/// Whether a claim's recorded working directory is `folder` or below it, as written or once
/// symlinks resolve: `folder` is canonical, and the runtime may record either form.
fn ran_in(link: &link::Link, folder: &std::path::Path) -> bool {
    link.cwd
        .as_deref()
        .map(std::path::Path::new)
        .is_some_and(|cwd| {
            cwd.starts_with(folder) || cwd.canonicalize().is_ok_and(|cwd| cwd.starts_with(folder))
        })
}

/// In a git worktree its main checkout's policy covers, how many sessions from before that
/// coverage wait for `agit save --all`; `None` anywhere else.
fn worktree_left_out(cwd: &std::path::Path) -> Option<usize> {
    let folder = super::front::folder(cwd).ok()?;
    super::project::worktree_of(&folder)?;
    let sessions = super::front::sessions_in(&folder).ok()?;
    Some(super::front::left_out_in(&folder, &sessions))
}

/// The "who am I" lines for the conversation the runtime environment names.
///
/// It is labeled as display only: the explicit-target rule above still decides what commands
/// act on, and this line must not read as a second way to select a session.
fn print_runtime_sessions(sessions: &[super::context::RuntimeSession], with_note: bool) {
    match sessions {
        [] => {}
        [one] => {
            println!(
                "  runtime session: {} {} ({}; display only, not a command target)",
                one.runtime, one.session_id, one.variable
            );
            match &one.target {
                Some(target) => println!("  {}", ui::dim(&format!("saved to: {target}"))),
                None if one.managed => {
                    println!("  {}", ui::dim("saved to: no active complete claim"))
                }
                None => match &one.project {
                    Some(save) => println!(
                        "  {}",
                        ui::dim(&format!(
                            "saved to: {} at the end of each turn, by this folder's project{}",
                            save.repository,
                            match (save.sign_in, save.upload_off) {
                                (super::project::SignIn::Absent, _) =>
                                    " (once agit is signed in)".to_owned(),
                                (_, true) => format!(
                                    " (automatic upload is off: `agit push {}` publishes it)",
                                    save.repository
                                ),
                                (super::project::SignIn::Current, false) => String::new(),
                                (super::project::SignIn::Expired, false) =>
                                    " (uploads wait for `agit login`)".to_owned(),
                            }
                        ))
                    ),
                    None => println!("  {}", ui::dim("saved to: not saved yet")),
                },
            }
        }
        many => {
            println!(
                "  runtime session: ambiguous ({}; display only, not a command target)",
                many.iter()
                    .map(|session| format!(
                        "{}={} ({})",
                        session.variable, session.session_id, session.runtime
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    if with_note && let Some(note) = super::context::runtime_session_note(sessions) {
        ui::hint(&note);
    }
}

/// Typed display-only form of [`print_runtime_sessions`]. `state` is `none`, `detected` (exactly
/// one conversation, also in `session`) or `ambiguous` (`session` is null and `candidates` lists
/// every conversation the variables name).
fn runtime_session_json(sessions: &[super::context::RuntimeSession]) -> serde_json::Value {
    serde_json::json!({
        "state": match sessions.len() {
            0 => "none",
            1 => "detected",
            _ => "ambiguous",
        },
        "session": match sessions {
            [one] => one.to_json(),
            _ => serde_json::Value::Null,
        },
        "candidates": sessions.iter().map(super::context::RuntimeSession::to_json).collect::<Vec<_>>(),
    })
}

fn structured(args: &Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let earlier = worktree_left_out(&cwd);
    let ClaimInventory {
        store,
        mut links,
        issues: link_issues,
    } = claim_inventory(config::store_root()?);

    let in_folder = order(&mut links, &cwd);
    let limit = args.limit.unwrap_or(100) as usize;
    let page = observe_page(
        store.as_ref(),
        &links,
        link_issues.is_empty(),
        args.offset,
        limit,
    );
    let inventory_complete = page.inventory_complete;
    let selection = match super::context::from_env_with_claims(&links, inventory_complete) {
        Ok(Some(context)) => serde_json::json!({
            "repo": context.repo, "branch": context.branch, "source": context.via,
        }),
        Ok(None) => serde_json::json!({"repo":null, "branch":null,
            "reason":"no session target supplied through AGIT_SESSION"}),
        Err(error) => serde_json::json!({"repo":null, "branch":null, "reason":error.to_string()}),
    };
    let publication_waits = crate::rc::supervisor::archive_recovery::publication_waits();
    let items: Vec<_> = links
        .iter()
        .enumerate()
        .skip(args.offset)
        .take(limit)
        .zip(page.rows)
        .map(|((index, link), detail)| {
            let target = match (&link.owner, &link.agent, &link.branch) {
                (Some(owner), Some(repo), Some(branch)) => Some(format!("{owner}/{repo}@{branch}")),
                _ => None,
            };
            let publication_wait = match &publication_waits {
                Ok(waits) => waits
                    .get(&(link.source.clone(), link.session_id.clone()))
                    .map(|wait| {
                        serde_json::json!({
                            "reason": "push_disabled", "repository": wait.repository,
                            "off": wait.off.name(),
                            "capture_repository": wait.capture_repository,
                            "command": wait.command,
                        })
                    }),
                Err(error) => Some(serde_json::json!({
                    "reason": "unknown", "error": format!("{error:#}"),
                })),
            };
            serde_json::json!({
                "runtime": link.source, "session_id": link.session_id,
                "owner": link.owner, "repository_name": link.agent, "branch": link.branch,
                "target": target, "cwd": link.cwd, "in_current_folder": index < in_folder,
                "active": link.is_active(), "superseded_by": link.superseded_by,
                "last_commit": (detail[4] != "—").then_some(&detail[4]),
                "pending_activity": detail[5], "local_instance": detail[6],
                "project": detail[7], "upload": sessions::upload_json(&detail[8]),
                "publication_wait": publication_wait,
            })
        })
        .collect();
    let next = args.offset.saturating_add(items.len());
    let agents = super::clone::list_local()?;
    let mut repositories = Vec::new();
    let mut remaining = BRANCH_ROWS;
    let mut repositories_omitted = 0;
    in_order(&agents, ahead, |index, (inspected, set_aside)| {
        if remaining == 0 {
            repositories_omitted = agents.len() - index;
            return false;
        }
        let (owner, name, path) = &agents[index];
        let limit = row_limit(BRANCH_ROWS - remaining, agents.len() - index - 1);
        let branch_sync = match branches::settle(&Repo::at(path), inspected, BRANCH_ROWS, limit) {
            Ok(page) => {
                remaining = remaining.saturating_sub(page.branches.len().max(1));
                serde_json::json!({
                    "items": page.branches, "omitted": page.omitted, "error": null,
                })
            }
            Err(error) => {
                remaining = remaining.saturating_sub(1);
                serde_json::json!({"items": null, "omitted": null, "error": format!("{error:#}")})
            }
        };
        let set_aside = match set_aside {
            Ok(branches) => serde_json::json!({"branches": branches, "error": null}),
            Err(error) => serde_json::json!({"branches": null, "error": format!("{error:#}")}),
        };
        repositories.push(serde_json::json!({
            "repo": format!("{owner}/{name}"), "path": path, "branches": branch_sync,
            "set_aside": set_aside,
        }));
        true
    });
    let shared_files = shared::inspect(&agents);
    let merge_transactions = merges::page(&agents);
    let runtime_sessions = super::context::runtime_sessions();
    let missing = if args.check_missing {
        if let Some(note) = super::context::runtime_session_note(&runtime_sessions) {
            ui::hint(&note);
        }
        let discovery = uncaptured(&links, inventory_complete);
        Some((discovery.sessions.into_iter().map(|(runtime, session_id)| {
            serde_json::json!({"runtime": runtime, "session_id": session_id})
        }).collect::<Vec<_>>(), discovery.errors))
    } else {
        None
    };
    let code_repo = config::repo_root();
    let adopted_here = code_repo
        .as_ref()
        .filter(|_| inventory_complete)
        .map(|root| {
            links
                .iter()
                .filter(|link| link.cwd.as_deref() == root.to_str())
                .count()
        });
    let result = serde_json::json!({
        "schema_version": 1, "cwd": cwd, "selection": selection,
        "runtime_session": runtime_session_json(&runtime_sessions),
        "bound_repo": crate::domain::workspace::read(&cwd).map(|workspace| workspace.repo),
        "worktree": super::project::worktree_notice(&cwd),
        // Uploading the sessions from before is the person's choice: the agent asks.
        "earlier_sessions_not_uploaded": earlier.map(|count| serde_json::json!({
            "count": count,
            "ask_user": super::front::earlier_note(count, "here", super::front::Reader::Agent),
        })),
        "upload_waits": upload_waits_json(),
        "uploads_not_through": uploads_not_through_json(),
        "automatic_upload_off": automatic_upload_off(),
        "store_path": store.as_ref().map(|store| store.root()),
        "sessions": {"items": items, "total": links.len(), "offset": args.offset,
            "limit": limit, "next_offset": (next < links.len()).then_some(next),
             "incomplete": !inventory_complete},
        "repositories": repositories, "repositories_omitted": repositories_omitted,
        "shared_files": shared_files, "merge_transactions": merge_transactions,
        "code_repository": {"path": code_repo, "adopted_sessions": adopted_here},
        "unadopted": {"checked": args.check_missing,
            "sessions": missing.as_ref().map(|(sessions, _)| sessions),
            "incomplete": missing.as_ref().map(|(_, errors)| !errors.is_empty()),
            "errors": missing.as_ref().map(|(_, errors)| errors)},
    });
    println!("{}", serde_json::to_string(&result)?);
    Ok(ExitCode::Ok)
}

/// A repository's branches and set-aside record, read before its turn on the page comes. The
/// branches are read with the most rows any repository may show; [`branches::settle`] judges
/// whether that reading stands for the rows left when its turn comes.
fn ahead(
    (_, _, path): &(String, String, std::path::PathBuf),
) -> (
    crate::Result<branches::Page>,
    crate::Result<BTreeSet<String>>,
) {
    let repo = Repo::at(path);
    (branches::inspect(&repo, BRANCH_ROWS), set_aside(&repo))
}

/// Branches the checkout's Hub refused as withdrawn, which pushes leave out until the person names
/// one again. An unreadable record is an error, so status never presents it as none.
fn set_aside(repo: &Repo) -> crate::Result<BTreeSet<String>> {
    let Some(identity) = crate::hub::identity::read(repo)? else {
        return Ok(BTreeSet::new());
    };
    Ok(crate::domain::privacy_git::withheld_refs(repo, &identity)?
        .iter()
        .filter_map(|name| name.strip_prefix("refs/heads/"))
        .map(str::to_owned)
        .collect())
}

fn observe_page(
    store: Option<&Store>,
    links: &[link::Link],
    complete: bool,
    offset: usize,
    limit: usize,
) -> sessions::Page {
    store
        .map(|store| sessions::rows(store, links, complete, offset, limit))
        .unwrap_or_else(|| sessions::Page {
            rows: Vec::new(),
            inventory_complete: complete && links.is_empty(),
        })
}

/// The repositories status and the overview read in the current folder: the one the folder is
/// bound to, and those of the conversations recorded as run in it. A record of conversations that
/// can't be read in full reads as every repository, so a repository whose state is unknown is
/// never left out of what this folder needs.
pub fn reads_here() -> super::migration::Reads {
    use super::migration::Reads;
    let Ok(cwd) = std::env::current_dir() else {
        return Reads::Every;
    };
    let Ok(store) = config::store_root() else {
        return Reads::Every;
    };
    let ClaimInventory { links, issues, .. } = claim_inventory(store);
    if !issues.is_empty() {
        return Reads::Every;
    }
    let folder = super::front::folder(&cwd).ok();
    let mut read: BTreeSet<String> = links
        .iter()
        .filter(|link| folder.as_deref().is_some_and(|folder| ran_in(link, folder)))
        .filter_map(|link| Some(format!("{}/{}", link.owner.as_ref()?, link.agent.as_ref()?)))
        .collect();
    read.extend(folder.as_deref().and_then(super::front::bound));
    read.extend(crate::domain::workspace::read(&cwd).map(|binding| binding.repo));
    Reads::Only(read)
}

struct ClaimInventory {
    store: Option<Store>,
    links: Vec<link::Link>,
    issues: Vec<link::LinkIssue>,
}

/// Only a missing carrier proves an empty inventory; inspection failures retain unknown claims.
fn claim_inventory(root: std::path::PathBuf) -> ClaimInventory {
    let store = Store::at(root);
    let (links, mut issues) =
        link::list_checked_with_limits(&store, sessions::MAX_LINKS, sessions::MAX_LINK_BYTES);
    let carrier_issue = match std::fs::symlink_metadata(store.root()) {
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && links.is_empty()
                && issues.is_empty() =>
        {
            return ClaimInventory {
                store: None,
                links,
                issues,
            };
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => None,
        Ok(_) => Some(link::LinkIssueKind::InvalidPath),
        Err(_) => Some(link::LinkIssueKind::UnreadableDirectory),
    };
    if let Some(kind) = carrier_issue {
        issues.push(link::LinkIssue {
            path: store.root().to_owned(),
            kind,
            repository: None,
        });
    }
    ClaimInventory {
        store: Some(store),
        links,
        issues,
    }
}

#[derive(Default)]
struct Discovery {
    sessions: Vec<(&'static str, String)>,
    errors: Vec<IndexError>,
}

#[derive(serde::Serialize)]
struct IndexError {
    runtime: &'static str,
    message: String,
}

/// Runtime indexes define discovery; an unreadable index is not evidence of an empty one.
fn uncaptured(links: &[link::Link], complete: bool) -> Discovery {
    if !complete {
        return Discovery {
            sessions: Vec::new(),
            errors: vec![IndexError {
                runtime: "claims",
                message: "unavailable: claim inventory incomplete; adoption cannot be determined"
                    .into(),
            }],
        };
    }
    let Some(repo) = config::repo_root().or_else(|| std::env::current_dir().ok()) else {
        return Discovery::default();
    };
    let mut known: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for link in links {
        known
            .entry(&link.source)
            .or_default()
            .insert(&link.session_id);
    }

    let mut out = Discovery::default();
    for rt in crate::adapter::RUNTIMES {
        let Ok(ad) = crate::adapter::get(rt) else {
            continue;
        };
        // Unadopted sessions are offered for naming; runtime bookkeeping is not.
        let sessions = match ad.session_choices_for(&repo) {
            Ok(sessions) => sessions,
            Err(error) => {
                out.errors.push(IndexError {
                    runtime: ad.id(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        for sr in sessions {
            if !known
                .get(ad.id())
                .is_some_and(|ids| ids.contains(sr.id.as_str()))
            {
                out.sessions.push((ad.id(), sr.id));
            }
        }
    }
    out
}

/// Sessions whose completed turns wait to upload because automatic publication is off for them.
/// The daemon parks such a turn and says so only in its own log, so this is where the wait
/// becomes visible. Turning it off is the person's choice, so the line says what is off and where
/// as a fact, and offers a command only when one setting as narrow as the destination ends it.
fn print_publication_waits(links: &[crate::domain::link::Link]) {
    let waits = match crate::rc::supervisor::archive_recovery::publication_waits() {
        Ok(waits) => waits,
        Err(error) => {
            ui::warning(&format!(
                "turns waiting to upload cannot be listed: {error:#}"
            ));
            return;
        }
    };
    for link in links {
        let Some(wait) = waits.get(&(link.source.clone(), link.session_id.clone())) else {
            continue;
        };
        let session = crate::domain::link::short(&link.session_id);
        let off = match wait.off {
            Off::Destination => format!("for {} on this computer", wait.repository),
            Off::CaptureRepository => format!(
                "for {}, the repository that records this conversation",
                wait.capture_repository
            ),
            Off::UserDefault => format!(
                "on this computer for every repository without its own setting, {} included",
                wait.repository
            ),
        };
        ui::info(format_args!(
            "{session}: automatic publication is off {off}, so its completed turns stay on this computer until you choose to upload them"
        ));
        if let Some(command) = &wait.command {
            ui::hint(&format!(
                "if you want them uploaded to {} automatically: `{command}`",
                wait.repository
            ));
        }
    }
}

/// Sessions whose settlement waits for the privacy check. A hook settles quietly, so this is
/// where such a wait becomes visible. A wait that does not end by itself names the step that ends
/// it, once for every wait it ends.
fn print_privacy_waits(links: &[crate::domain::link::Link]) {
    let waiting = crate::domain::privacy::pending::load();
    let mut steps = Vec::new();
    for link in links {
        let Some(entry) = waiting.get(&link.session_id) else {
            continue;
        };
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.since);
        if let Some(bytes) = entry.refused_bytes {
            ui::warning(&format!(
                "{}: privacy check refused a record of {} since {}; {} turn{} wait, and settling again does not change that",
                link::short(&link.session_id),
                super::mib(bytes),
                ui::ago(since),
                entry.turns,
                if entry.turns == 1 { "" } else { "s" },
            ));
            ui::hint(&super::commit::refused_record_hint(
                super::commit::drop_target(link).as_deref(),
            ));
            continue;
        }
        if crate::domain::privacy::pending::worker_unavailable(&entry.reason) {
            ui::warning(&format!(
                "{}: the privacy check couldn't run since {}; {} turn{} wait and are kept on this computer, and the next attempt tries again",
                link::short(&link.session_id),
                ui::ago(since),
                entry.turns,
                if entry.turns == 1 { "" } else { "s" },
            ));
            let step = crate::domain::privacy::pending::WORKER_UNAVAILABLE_STEP.to_owned();
            if !steps.contains(&step) {
                steps.push(step);
            }
            continue;
        }
        let ends = crate::domain::privacy::pending::steps(&entry.reason);
        ui::warning(&format!(
            "{}: privacy check pending ({}) since {}; {} turn{} wait{}",
            link::short(&link.session_id),
            entry.reason,
            ui::ago(since),
            entry.turns,
            if entry.turns == 1 { "" } else { "s" },
            if ends.is_none() {
                ", and the next settlement tries again"
            } else {
                ""
            },
        ));
        for step in ends.into_iter().flatten() {
            if !steps.contains(&step) {
                steps.push(step);
            }
        }
    }
    // A conversation the daemon observes has no link until its first landing, and a landing
    // that waited for the privacy check left none: its wait shows here, so it never stalls
    // unseen.
    for (session, entry) in &waiting {
        if links.iter().any(|link| &link.session_id == session) {
            continue;
        }
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.since);
        let ends = crate::domain::privacy::pending::steps(&entry.reason);
        ui::warning(&format!(
            "a conversation not saved yet ({}): privacy check pending ({}) since {}{}",
            crate::domain::link::short(session),
            entry.reason,
            ui::ago(since),
            if ends.is_none() {
                "; the next attempt tries again"
            } else {
                ""
            },
        ));
        for step in ends.into_iter().flatten() {
            if !steps.contains(&step) {
                steps.push(step);
            }
        }
    }
    for step in steps {
        ui::hint(&step);
    }
}

/// Branches that stay on this computer because a Codex link that may be another conversation's
/// names them.
fn print_held_branches() {
    let Some(store) = crate::domain::store::Store::open().ok().flatten() else {
        return;
    };
    for hold in crate::domain::link::launch::all_holds(&store) {
        ui::info(format_args!(
            "{}",
            crate::domain::link::launch::explain(&hold)
        ));
        // Status reads no conversation's content; `agit branch confirm` shows each one's opening
        // words.
        for link in crate::domain::link::launch::unconfirmed_older(&store, &hold.slug, &hold.branch)
        {
            ui::info(format_args!(
                "  {}",
                super::branch::conversation_line(&store, &link, false)
            ));
        }
    }
}

/// Uploads the privacy check holds back, for every target on this computer: an automatic upload
/// has nobody watching it, and its wait is not tied to the folder status runs in.
fn print_upload_waits() {
    let mut steps = Vec::new();
    for (target, entry) in crate::domain::privacy::pending::upload::load() {
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.since);
        let ends = crate::domain::privacy::pending::steps(&entry.reason);
        ui::warning(&format!(
            "{target}: its upload waits for the privacy check ({}) since {}; nothing was sent{}",
            entry.reason,
            ui::ago(since),
            if ends.is_none() {
                ", and the next upload tries again"
            } else {
                ""
            },
        ));
        for step in ends.into_iter().flatten() {
            if !steps.contains(&step) {
                steps.push(step);
            }
        }
    }
    for step in steps {
        ui::hint(&step);
    }
}

/// Every branch of every checkout here whose newest automatic upload didn't go through, and what
/// kept some of them from being read. It is read apart from the session page and the branch row
/// budget: a refused upload is refused again on every turn, and a page that left its branch out
/// would let it go unseen. Each names only the branch and the class of what happened.
#[derive(Default)]
struct UploadsNotThrough {
    items: Vec<(String, super::upload_outcome::Undone)>,
    /// What can't be read, a repository or a branch, and why.
    unreadable: Vec<(String, String)>,
}

fn uploads_not_through() -> UploadsNotThrough {
    let mut found = UploadsNotThrough::default();
    let checkouts = match super::clone::list_local() {
        Ok(checkouts) => checkouts,
        Err(error) => {
            found
                .unreadable
                .push(("the repositories here".into(), format!("{error:#}")));
            return found;
        }
    };
    for (owner, name, path) in checkouts {
        let repository = format!("{owner}/{name}");
        let branches = || -> crate::Result<Vec<String>> {
            Ok(Repo::at(&path)
                .git(&["for-each-ref", "--format=%(refname)", "refs/heads/"])?
                .lines()
                .filter_map(|line| line.trim().strip_prefix("refs/heads/"))
                .map(str::to_owned)
                .collect())
        };
        match super::upload_outcome::undone_in(&repository, branches) {
            Ok(branches) => {
                for (branch, undone) in branches {
                    let target = format!("{repository}@{branch}");
                    match undone {
                        Ok(undone) => found.items.push((target, undone)),
                        Err(error) => found
                            .unreadable
                            .push((target, ui::home_shortened(&format!("{error:#}")))),
                    }
                }
            }
            Err(error) => found
                .unreadable
                .push((repository, ui::home_shortened(&format!("{error:#}")))),
        }
    }
    found
}

/// The command that shows what kept an upload of `target` from going through and what to do.
fn inspect_command(target: &str) -> String {
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    format!("agit push {}", quote_arg(target))
}

/// Repositories a folder here would upload to by itself, while the repository's own setting keeps
/// automatic upload off. The setting wins, and this says so: the folder's setting alone would read
/// as though its turns were on their way.
fn automatic_upload_off() -> Vec<String> {
    super::clone::list_local()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(owner, name, path)| {
            let repository = format!("{owner}/{name}");
            let repo = Repo::open(&path)?;
            (super::project::binds_with_automatic_upload(&repository)
                && !super::auto_push::publication_allowed(&repo, &repository).ok()?)
            .then_some(repository)
        })
        .collect()
}

fn print_automatic_upload_off() {
    for repository in automatic_upload_off() {
        println!(
            "  {repository}: automatic upload is off: turns are saved here; `agit push {repository}` publishes them"
        );
    }
}

/// The first line the push reported for `target` (`<owner/repo>@<branch>`), which `agit status`
/// shows beside the class: only here, since every other line that tells a class goes into the
/// conversation's transcript.
fn said_at(target: &str) -> Option<String> {
    let (repository, branch) = target.split_once('@')?;
    super::upload_outcome::said(repository, branch)
}

/// [`uploads_not_through`] as `agit status` shows it.
fn print_uploads_not_through() {
    let found = uploads_not_through();
    if found.items.is_empty() && found.unreadable.is_empty() {
        return;
    }
    ui::section("uploads that didn't go through");
    let reader = super::project::Reader::of_this_process();
    for (target, undone) in &found.items {
        println!(
            "  {target}: {}",
            undone.describe_for(target, reader, said_at(target).as_deref())
        );
        println!(
            "    {}",
            ui::dim(&format!("→ inspect: `{}`", inspect_command(target)))
        );
    }
    for (what, error) in &found.unreadable {
        println!(
            "  {what}: the records of its uploads can't be read ({error}), so an upload that didn't go through may be missing here"
        );
    }
}

/// [`print_uploads_not_through`] for `status --json`, and for `project list --json`, which the menu
/// bar reads on every poll: both answer from the same records, in the same shape.
pub(crate) fn uploads_not_through_json() -> serde_json::Value {
    let found = uploads_not_through();
    let reader = super::project::Reader::of_this_process();
    serde_json::json!({
        "items": found.items.iter().map(|(target, undone)| {
            let said = said_at(target);
            serde_json::json!({
                "target": target,
                "state": undone.state(),
                "reason": undone.reason(),
                "description": undone.describe_for(target, reader, said.as_deref()),
                "said": said,
                "command": inspect_command(target),
            })
        }).collect::<Vec<_>>(),
        "unreadable": found.unreadable.iter().map(|(what, error)| serde_json::json!({
            "of": what,
            "error": error,
        })).collect::<Vec<_>>(),
    })
}

/// [`print_upload_waits`] for `status --json`.
fn upload_waits_json() -> Vec<serde_json::Value> {
    crate::domain::privacy::pending::upload::load()
        .into_iter()
        .map(|(target, entry)| {
            serde_json::json!({
                "target": target,
                "reason": entry.reason,
                "since": entry.since,
                "steps": crate::domain::privacy::pending::steps(&entry.reason),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    /// A repository with more branches than the table holds leaves a row for every repository
    /// after it, so none of them drops out of `agit status` unseen. The warning names where
    /// branches were left out and how many, and the command that lists them; a warning that said
    /// only "branches or repositories" left the person no repository to name.
    #[test]
    fn every_repository_keeps_a_row_in_the_status_table() {
        use super::BRANCH_ROWS;
        assert_eq!(super::row_limit(0, 0), BRANCH_ROWS);
        assert_eq!(super::row_limit(0, 5), BRANCH_ROWS - 5);
        assert_eq!(super::row_limit(BRANCH_ROWS - 1, 3), 1);
        let (warning, hint) =
            super::left_out_of_table(&[("sim-hao/parley".into(), 212)], 0).unwrap();
        assert_eq!(
            warning,
            "status is incomplete: 212 more branches of sim-hao/parley don't fit the display budget"
        );
        assert_eq!(
            hint,
            "`agit branch --repo sim-hao/parley --all` lists every branch of a repository"
        );
        assert!(super::left_out_of_table(&[], 0).is_none());
    }

    #[test]
    fn a_missing_store_only_proves_a_complete_empty_observation() {
        let empty = super::observe_page(None, &[], true, 0, 8);
        assert!(empty.inventory_complete);
        assert!(empty.rows.is_empty());
        assert!(!super::observe_page(None, &[], false, 0, 8).inventory_complete);
        let claim = crate::domain::link::Link::new("codex", "unverified", None);
        assert!(!super::observe_page(None, &[claim], true, 0, 8).inventory_complete);
    }

    #[test]
    fn expensive_scan_is_opt_in() {
        // CC has to read a directory and Codex has to query a database; neither belongs in the
        // default path.
        assert!(
            !super::Args {
                check_missing: false,
                limit: None,
                offset: 0,
            }
            .check_missing
        );
    }

    /// A page reads its repositories on several threads but must take them in order and stop
    /// where its budget runs out. Results arrive in item order whatever order they finish in,
    /// and after a stop only a bounded number of items beyond it have run: a pool that let quick
    /// items race ahead of a slow one would read every repository of a page that shows a few.
    #[test]
    fn a_page_takes_results_in_order_and_runs_little_past_where_it_stops() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let items: Vec<usize> = (0..200).collect();
        let mut seen = Vec::new();
        super::in_order(
            &items,
            |&item| {
                if item % 7 == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                item * 2
            },
            |index, doubled| {
                seen.push((index, doubled));
                true
            },
        );
        assert_eq!(
            seen,
            (0..200).map(|item| (item, item * 2)).collect::<Vec<_>>()
        );

        let started = AtomicUsize::new(0);
        super::in_order(
            &items,
            |&item| {
                started.fetch_add(1, Ordering::SeqCst);
                if item == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            },
            |_, ()| false,
        );
        assert!(
            started.load(Ordering::SeqCst) <= 1 + super::PAGE_WORKERS,
            "{} items ran for a page that stopped at the first",
            started.load(Ordering::SeqCst)
        );
    }
}
