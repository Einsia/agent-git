//! The repos screen (bare `agit new`): pick an agent, then name the new session.
//!
//! # Why the alt screen is left as soon as a repo is picked
//!
//! The branch name is asked on the **normal screen** (`docs/07_tui.md` §2): once the user closes
//! the interface, the terminal has to show what just happened, the same as after an ordinary
//! command. Anything written inside the alt screen is gone the moment it exits, and that stretch
//! turns blank.
//!
//! # "how many sessions" counts session lines
//!
//! The `main` that `agit init` creates is a **file line** and never claims a session
//! (`03_branch_model.md` §1). Taking the branch total as the session count makes a repo that was
//! just initialized, with no session at all, show `1 session` — that trades "cannot be counted"
//! for "counted wrong", and the second is harder to spot.
//!
//! # The cost of the first frame
//!
//! [`assemble`] is a pure function, and [`gather`] makes a fixed three git calls per repo (list
//! branches plus two batched `cat-file` runs, see [`crate::domain::meta::lines_at_refs`]); only
//! a checkout that is not yours costs one more question about `upstream`. The shared-file
//! overview is **not read here** — it appears only in the detail pane, read once on demand and
//! cached (following §4.1 "the list does not resolve, the detail resolves on demand").

// ratatui has a `Line` of its own, and this file uses both.
use crate::domain::meta::Line as BranchLine;
use crate::domain::repo::Repo;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// The inheritance point when `--from` is omitted. **Refers to the one in `new`** instead of
/// spelling `"main"` a second time here.
///
/// [`choose`] decides whether an unreadable line declaration is blocked by asking whether this
/// `--from` is the default. Let the two literals move apart and what should be blocked is not —
/// a failure that shows no error, only one missing gate.
///
/// This constant is **not** "the inheritance point is hardwired to main": `agit new --from v0.3`
/// goes through this screen too, and then the whole screen reckons in `v0.3` (see
/// [`Input::from`]).
use crate::commands::new::DEFAULT_FROM;

/// The raw material scanned out of one local checkout.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub owner: String,
    pub name: String,
    pub path: PathBuf,
    /// Every branch name.
    pub branches: Vec<String>,
    /// The line of each branch, same order and length as [`Scan::branches`]. `None` = no line
    /// declaration.
    pub lines: Vec<Option<BranchLine>>,
    /// The line of the inheritance point. It need not be a branch (`--from` also takes a tag or
    /// a sha), so it is kept on its own.
    pub from_line: Option<BranchLine>,
    /// Whether an `upstream` points at someone else's copy (asked only for a checkout that is
    /// not yours).
    pub has_upstream: bool,
}

/// Everything assembling the rows needs.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// The current account name. `None` when not signed in — read-only cannot be decided then,
    /// see [`assemble`].
    pub me: Option<String>,
    /// This run's inheritance point (`--from`, default [`DEFAULT_FROM`]).
    pub from: String,
    pub scans: Vec<Scan>,
}

/// Where a repository is available before the user selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Local,
    Hub,
}

/// One row of the list.
#[derive(Debug, Clone)]
pub struct Row {
    pub source: Source,
    pub owner: String,
    pub name: String,
    pub path: PathBuf,
    /// How many session lines. File lines do not count (see the module comment).
    pub sessions: usize,
    /// This run's inheritance point, carried unchanged — what the screen says and what
    /// downstream actually uses must be the same string.
    pub from_ref: String,
    /// The line of the inheritance point. `None` = the declaration cannot be read; see
    /// [`choose`] for its two readings.
    pub from_line: Option<BranchLine>,
    /// Branch names that already exist — used to block a duplicate on the spot, rather than
    /// letting the user finish typing and then take an error.
    pub branches: Vec<String>,
    /// Read-only checkout: `new` on someone else's checkout is legal, but pushing has to go the
    /// `--mine` route.
    pub read_only: bool,
}

impl Row {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The text the filter matches against.
    pub fn haystack(&self) -> String {
        self.slug()
    }
}

/// Assemble the raw material into a sorted list. **Pure function, no filesystem.**
///
/// Sorted lexicographically by slug, not by recent activity: this picker gets opened over and
/// over, and only a stable order builds muscle memory; "recent" would also cost one more git
/// question per repo.
pub fn assemble(input: &Input) -> Vec<Row> {
    let mut rows: Vec<Row> = input
        .scans
        .iter()
        .map(|s| Row {
            source: Source::Local,
            owner: s.owner.clone(),
            name: s.name.clone(),
            path: s.path.clone(),
            sessions: s
                .lines
                .iter()
                .filter(|l| **l == Some(BranchLine::Session))
                .count(),
            from_ref: input.from.clone(),
            from_line: s.from_line,
            branches: s.branches.clone(),
            // The test is shared with `agit push` (`push::is_read_only`): both halves must hold.
            // When not signed in the first half cannot be decided — then **do not mark it**; better
            // to say one thing less than to tell you your own repo cannot be pushed to. The status
            // bar's "not signed in" already says it.
            read_only: input
                .me
                .as_deref()
                .is_some_and(|me| crate::commands::push::is_read_only(me, &s.owner, has(s))),
        })
        .collect();
    rows.sort_by_key(|r| r.slug());
    rows
}

/// The third argument of `is_read_only` only cares whether there is one; the value itself is
/// unused.
fn has(s: &Scan) -> Option<&str> {
    s.has_upstream.then_some("upstream")
}

/// What the user chose on this screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// This repo is picked; the branch name is asked next, on the normal screen.
    Pick(String),
    Quit,
}

/// What happens when a row is selected. A pure function — "when `new` cannot start from here"
/// is a test, not rendering.
///
/// `new` is defined as **memory only, no context**, so the inheritance point must be a file
/// line. A session-line inheritance point does not silently change the meaning: it says on the
/// spot that this would be a `fork` (`docs/07_tui.md` §3.2).
pub fn choose(r: &Row) -> Result<Outcome, String> {
    let (slug, from) = (r.slug(), &r.from_ref);
    // Hub metadata cannot prove a git ref's line. The explicit `new` path clones and validates
    // the selected repository before it creates a session.
    if r.source == Source::Hub {
        return Ok(Outcome::Pick(slug));
    }
    match r.from_line {
        Some(BranchLine::File) => Ok(Outcome::Pick(slug)),
        Some(BranchLine::Session) => Err(format!(
            "`{from}` in {slug} is a session line. `new` carries memory only — inheriting from a \
             session line would be a fork, which carries the conversation too.\n\n\
             if that is what you want: agit fork {slug}@{from} -b <name> --resume"
        )),
        // An unreadable declaration means two different things; they must not be conflated.
        //
        // The default `main`: a repo made by `agit init` always has a file line, so failing to
        // read one means this checkout is broken — blocking is right.
        //
        // A `--from` the user wrote: its syntax is wider than this screen knows (`~n`, `#turn`,
        // `:path` are all legal), so an unreadable declaration usually means **we** cannot parse
        // it, not that the point does not exist. The real resolution is downstream in
        // `refs::resolve`, which knows every form. Blocking here would kill a command that would
        // have run, so let it through and leave it to the layer that can tell.
        None if from == DEFAULT_FROM => Err(format!(
            "{slug} has no `{from}` file line, so there is nothing for a new session to inherit.\n\n\
             a repo made by `agit init` always has one — re-fetch this checkout, or start over \
             with `agit init`."
        )),
        None => Ok(Outcome::Pick(slug)),
    }
}

/// The shared-file overview. Used by the detail pane, read on demand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shared {
    pub memory: usize,
    pub skills: usize,
    pub agents_md: bool,
}

/// Count the shared files at one ref. The test matches the three paths `new` materializes.
pub fn shared_of(paths: &[String]) -> Shared {
    Shared {
        memory: paths.iter().filter(|p| p.starts_with("memory/")).count(),
        skills: paths.iter().filter(|p| p.starts_with("skills/")).count(),
        agents_md: paths.iter().any(|p| p == "AGENTS.md"),
    }
}

// ── Fetching data (the only part that touches the filesystem) ──────────

pub fn collect(from: &str) -> Vec<Row> {
    assemble(&gather(from, false))
}

/// Import discovery reads only local metadata and never repairs a partial clone.
pub(super) fn collect_local(from: &str) -> Vec<Row> {
    assemble(&gather(from, true))
}

/// The two refs a name can land on: the local one, and one that exists only on the remote.
///
/// Asking only `refs/heads/` mirrors the failure where a branch that was never pushed cannot be
/// counted — after `agit fetch`, a branch a collaborator just pushed makes it into the list yet
/// contributes 0 to the session count. Both go into the same batch, so the process count does not
/// change ([`crate::domain::meta::lines_at_refs`] already tolerates objects it cannot read).
fn both_places(name: &str) -> [String; 2] {
    [
        format!("refs/heads/{name}"),
        format!("refs/remotes/origin/{name}"),
    ]
}

/// Collapse a batch of "two candidates per name" into "one answer per name".
///
/// The local checkout wins; one that exists only on the remote is the fallback. The trailing
/// pair is the inheritance point's own. This is a pure function of its own so the pairing rule
/// can be asserted directly — off by one slot and every session count lands on the wrong
/// branch.
fn collapse(
    read: &[Option<BranchLine>],
    branches: usize,
) -> (Vec<Option<BranchLine>>, Option<BranchLine>) {
    let first = |i: usize| {
        read.get(i)
            .copied()
            .flatten()
            .or(read.get(i + 1).copied().flatten())
    };
    (
        (0..branches).map(|i| first(i * 2)).collect(),
        first(branches * 2),
    )
}

fn gather(from: &str, local_only: bool) -> Input {
    let me = crate::infra::credentials::current_user();
    let mut scans = Vec::new();
    for (owner, name, path) in crate::commands::clone::list_local().unwrap_or_default() {
        let Some(repo) = Repo::open(&path) else {
            continue;
        };
        let branches = if local_only {
            let Ok(branches) = repo.branches_local_bounded(512) else {
                continue;
            };
            branches
        } else {
            repo.branches()
        };
        // Two candidates per branch, plus a trailing pair for the inheritance point itself —
        // `--from` can be a tag or a sha and need not appear in the branch list. All of it goes
        // into the same batch, still two `cat-file` runs.
        let mut refs: Vec<String> = branches.iter().flat_map(|b| both_places(b)).collect();
        refs.push(from.to_string());
        refs.push(format!("origin/{from}"));
        let read = if local_only {
            crate::domain::meta::at_refs_with_policy(
                &repo,
                &refs,
                crate::domain::repo::ReadPolicy::LocalOnly,
                64 * 1024,
            )
            .into_iter()
            .map(|metadata| metadata.map(|metadata| metadata.line))
            .collect()
        } else {
            crate::domain::meta::lines_at_refs(&repo, &refs)
        };
        let (lines, from_line) = collapse(&read, branches.len());
        // The second half of the read-only test costs a git question, so it is asked only for a
        // checkout that is **not yours**: once the first half does not hold the answer is decided,
        // and asking anyway burns one process per repo for nothing.
        let foreign = me.as_deref().is_some_and(|me| me != owner);
        let has_upstream = foreign
            && if local_only {
                repo.git_status_local(&["remote", "get-url", "upstream"])
                    .is_ok_and(|(status, _, _)| status == Some(0))
            } else {
                repo.upstream_url().is_some()
            };
        scans.push(Scan {
            owner,
            name,
            path,
            branches,
            lines,
            from_line,
            has_upstream,
        });
    }
    Input {
        me,
        from: from.to_string(),
        scans,
    }
}

#[derive(Debug, Clone, Copy)]
enum HubStatus {
    SignedOut,
    Loading,
    Ready,
    Unavailable,
    SignInRequired,
}

impl HubStatus {
    fn label(self) -> &'static str {
        match self {
            Self::SignedOut => "Hub: sign in with agit login",
            Self::Loading => "Hub: loading repositories",
            Self::Ready => "Hub: repositories loaded",
            Self::Unavailable => "Hub: unavailable · r retry",
            Self::SignInRequired => "Hub: sign in with agit login · r retry",
        }
    }

    fn empty_text(self) -> &'static str {
        match self {
            Self::Loading => "Looking for your Hub repositories…\nYou can cancel with q.",
            Self::Unavailable => {
                "No local repositories.\nHub repositories could not be loaded.\nPress r to retry, or q to cancel.\nCreate a local repo: agit init <name>"
            }
            Self::SignInRequired | Self::SignedOut => {
                "No local repositories.\nSign in to find your Hub repos: agit login\nCreate a repo: agit init <name>\nPress q to return to the shell."
            }
            Self::Ready => {
                "No repositories found.\nCreate one: agit init <name>\nOr clone one: agit clone <owner>/<repo>\nPress q to return to the shell."
            }
        }
    }
}

struct HubDiscovery {
    started: bool,
    status: HubStatus,
    pending: Option<Receiver<Result<Vec<crate::hub::RemoteAgent>, HubStatus>>>,
}

impl HubDiscovery {
    fn new() -> Self {
        Self {
            started: false,
            status: if crate::infra::credentials::current_user().is_some() {
                HubStatus::Loading
            } else {
                HubStatus::SignedOut
            },
            pending: None,
        }
    }

    fn start(&mut self) {
        if self.pending.is_some() {
            return;
        }
        self.started = true;
        let Some(me) = crate::infra::credentials::current_user() else {
            self.status = HubStatus::SignedOut;
            return;
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        self.status = HubStatus::Loading;
        // Only one request may be outstanding. Dropping the picker drops its receiver, so a
        // late response cannot update another screen or keep cancellation waiting on the Hub.
        let spawned = std::thread::Builder::new()
            .name("agit-repo-picker".into())
            .spawn(move || {
                let result = crate::hub::Client::from_env_without_refresh(Duration::from_secs(4))
                    .agents_owned_by(&me)
                    .map(|agents| owned_agents(agents, &me))
                    .map_err(|error| {
                        if error
                            .downcast_ref::<crate::hub::client::ApiError>()
                            .is_some_and(|error| matches!(error.status, 401 | 403))
                        {
                            HubStatus::SignInRequired
                        } else {
                            HubStatus::Unavailable
                        }
                    });
                let _ = sender.send(result);
            });
        if spawned.is_ok() {
            self.pending = Some(receiver);
        } else {
            self.status = HubStatus::Unavailable;
        }
    }

    fn poll(&mut self, rows: &mut Vec<Row>, from: &str) -> bool {
        let Some(receiver) = &self.pending else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err(HubStatus::Unavailable),
        };
        self.pending = None;
        match result {
            Ok(agents) => {
                merge_hub_rows(rows, agents, from);
                self.status = HubStatus::Ready;
                true
            }
            Err(status) => {
                self.status = status;
                false
            }
        }
    }
}

fn owned_agents(agents: Vec<crate::hub::RemoteAgent>, me: &str) -> Vec<crate::hub::RemoteAgent> {
    agents
        .into_iter()
        .filter(|agent| {
            agent.owner == me
                && [&agent.owner, &agent.name].into_iter().all(|part| {
                    part.trim() == part.as_str() && crate::domain::repo::valid_name(part).is_ok()
                })
        })
        .collect()
}

fn merge_hub_rows(rows: &mut Vec<Row>, agents: Vec<crate::hub::RemoteAgent>, from: &str) {
    rows.retain(|row| row.source == Source::Local);
    let mut slugs: std::collections::HashSet<String> = rows.iter().map(Row::slug).collect();
    let mut remote: Vec<Row> = agents
        .into_iter()
        .filter_map(|agent| {
            if !slugs.insert(agent.slug()) {
                return None;
            }
            Some(Row {
                source: Source::Hub,
                owner: agent.owner,
                name: agent.name,
                // A Hub row has no checkout. Filesystem reads are gated on Source::Local.
                path: PathBuf::new(),
                sessions: agent.session_count,
                from_ref: from.to_string(),
                from_line: None,
                branches: Vec::new(),
                read_only: false,
            })
        })
        .collect();
    remote.sort_by_key(Row::slug);
    rows.extend(remote);
}

// ── Screen: rendering and keys ────────────────────────────────────────

use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

/// The picked repo and the typed branch name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub slug: String,
    pub branch: String,
}

/// One pass of "pick a repo → name it". `None` = the user gave up, or there is nothing to pick.
pub fn pick(from: &str) -> crate::Result<Option<Picked>> {
    let mut rows = collect(from);
    let mut hub = HubDiscovery::new();
    widgets::refresh_rc_status();
    let picked = {
        let mut guard = crate::tui::term::Guard::enter()?;
        let out = run_loop(&mut rows, from, &mut hub);
        // Give the terminal back first: the questions and summary below belong in the scrollback,
        // not in the alt screen.
        guard.suspend()?;
        out?
    };
    drop(hub);
    let Outcome::Pick(slug) = picked else {
        return Ok(None);
    };
    let row = rows.iter().find(|r| r.slug() == slug);
    Ok(name_it(&slug, row).map(|branch| Picked { slug, branch }))
}

/// Ask for the branch name on the normal screen.
///
/// **There is no "press enter for the default".** `agit init` already set this rule: the
/// directory name is only a suggestion, and only typing it yourself counts. The name is this
/// session's identity in version control, and it is worth one keystroke.
fn name_it(slug: &str, row: Option<&Row>) -> Option<String> {
    println!(
        "\n  {}  {}",
        crate::ui::dim("repo    "),
        crate::ui::bold(slug)
    );
    if let Some(r) = row {
        println!(
            "  {}  {} {}",
            crate::ui::dim("from    "),
            r.from_ref,
            crate::ui::dim(&format!(
                "({})",
                if r.source == Source::Hub {
                    "verified after clone"
                } else {
                    line_label(r.from_line)
                }
            ))
        );
        if r.source == Source::Hub {
            println!("  source   Hub — cloned after you enter a branch name");
        }
        if r.read_only {
            println!(
                "  {}  {} is a read-only checkout — publishing needs `agit push --mine`",
                crate::ui::dim("note    "),
                r.slug()
            );
        }
    }
    println!();
    // A duplicate name is blocked on the spot. Letting the user finish typing and then take a
    // "branch already exists" holds back until last something we already know.
    //
    // There is no retry limit: no test supports any particular number of tries, and exiting
    // silently once it runs out reads as the screen hanging. "I am done here" already has an
    // exit — Esc / Ctrl-D makes `input` return `None`.
    loop {
        let typed = crate::ui::prompt::input("branch name for the new session", None).ok()??;
        let branch = typed.trim().to_string();
        if branch.is_empty() {
            crate::ui::error("a name is required.");
            continue;
        }
        if let Err(error) = crate::domain::repo::valid_branch_name(&branch) {
            crate::ui::error(&format!("{error:#}"));
            continue;
        }
        if let Err(error) = crate::commands::target::branch_only(&format!("{slug}@{branch}")) {
            crate::ui::error(&crate::commands::terminal_error_message(&error));
            continue;
        }
        match std::process::Command::new("git")
            .args(["check-ref-format", "--branch", &branch])
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(_) => {
                crate::ui::error(&format!("invalid branch name `{branch}`; choose another."));
                continue;
            }
            Err(error) => {
                crate::ui::error(&format!("cannot validate the branch name: {error:#}"));
                continue;
            }
        }
        if row.is_some_and(|r| r.branches.contains(&branch)) {
            crate::ui::error(&format!(
                "`{branch}` already exists in {slug} — pick another."
            ));
            continue;
        }
        return Some(branch);
    }
}

/// The line in plain words. When it cannot be read, say so; do not guess.
fn line_label(line: Option<BranchLine>) -> &'static str {
    match line {
        Some(BranchLine::File) => "file line",
        Some(BranchLine::Session) => "session line",
        None => "no declaration here",
    }
}

fn run_loop(rows: &mut Vec<Row>, from: &str, hub: &mut HubDiscovery) -> crate::Result<Outcome> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default();
    state.select(Some(0));
    let mut filter = Filter::default();
    let mut notice: Option<String> = None;
    // The detail pane's shared-file overview is read once on demand and kept: a trip up and down
    // the list must not start git over and over.
    let mut shared: std::collections::HashMap<String, Shared> = std::collections::HashMap::new();

    loop {
        let selected = rows
            .iter()
            .filter(|r| filter.matches(&r.haystack()))
            .nth(state.selected().unwrap_or(0))
            .map(Row::slug);
        if hub.poll(rows, from) {
            let index = selected.as_ref().and_then(|slug| {
                rows.iter()
                    .filter(|r| filter.matches(&r.haystack()))
                    .position(|r| r.slug() == *slug)
            });
            state.select(index.or(Some(0)));
        }
        let view: Vec<&Row> = rows
            .iter()
            .filter(|r| filter.matches(&r.haystack()))
            .collect();
        if state.selected().is_none() && !view.is_empty() {
            state.select(Some(0));
        }
        if state.selected().unwrap_or(0) >= view.len() {
            state.select(if view.is_empty() {
                None
            } else {
                Some(view.len() - 1)
            });
        }
        if let Some(r) = state.selected().and_then(|i| view.get(i))
            && r.source == Source::Local
        {
            let key = r.slug();
            shared.entry(key).or_insert_with(|| read_shared(r));
        }
        term.draw(|f| {
            draw(
                f,
                &view,
                &mut state,
                &filter,
                &shared,
                notice.as_deref(),
                &hub.status,
            )
        })?;
        // Render local choices before starting network work. A slow Hub must not hold the
        // first frame or prevent the user from leaving the screen.
        if !hub.started {
            hub.start();
        }
        if hub.pending.is_some() && !crossterm::event::poll(Duration::from_millis(80))? {
            continue;
        }
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(Outcome::Quit);
        }
        if filter.is_active() {
            match key.code {
                KeyCode::Esc => filter.close(),
                KeyCode::Enter => filter.blur(),
                KeyCode::Backspace => filter.pop(),
                KeyCode::Char(c) => filter.push(c),
                _ => {}
            }
            continue;
        }
        notice = None;
        let n = view.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Outcome::Quit),
            KeyCode::Char('/') => filter.open(),
            KeyCode::Char('r') => hub.start(),
            KeyCode::Down | KeyCode::Char('j') => {
                let i = state.selected().unwrap_or(0);
                state.select(Some((i + 1).min(n.saturating_sub(1))));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = state.selected().unwrap_or(0);
                state.select(Some(i.saturating_sub(1)));
            }
            KeyCode::Char('g') | KeyCode::Home => state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => state.select(Some(n.saturating_sub(1))),
            KeyCode::Enter => {
                let Some(r) = state.selected().and_then(|i| view.get(i)) else {
                    continue;
                };
                match choose(r) {
                    Ok(out) => return Ok(out),
                    // A blocked pick does not leave the screen: put the reason in front of
                    // the user so they can pick another one on the spot.
                    Err(why) => notice = Some(why),
                }
            }
            _ => {}
        }
    }
}

fn read_shared(r: &Row) -> Shared {
    let Some(repo) = Repo::open(&r.path) else {
        return Shared::default();
    };
    // Same reason as the session count: the inheritance point may exist only on the remote
    // (fetched, never checked out).
    for rev in [r.from_ref.clone(), format!("origin/{}", r.from_ref)] {
        let paths = repo.ls_tree(&rev);
        if !paths.is_empty() {
            return shared_of(&paths);
        }
    }
    Shared::default()
}

fn draw(
    f: &mut Frame,
    view: &[&Row],
    state: &mut ListState,
    filter: &Filter,
    shared: &std::collections::HashMap<String, Shared>,
    notice: Option<&str>,
    hub_status: &HubStatus,
) {
    let panes = widgets::layout(f.area());
    widgets::render_status(
        f,
        panes.status,
        &widgets::Status {
            title: "agit new".into(),
            identity: crate::infra::credentials::current_user()
                .map(|u| format!("{u} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: widgets::Counters::default(),
        },
    );
    let list_area = widgets::list_area_with_notice(f, panes, notice);
    let list_panes = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(list_area);
    f.render_widget(
        Paragraph::new(hub_status.label()).style(Style::default().fg(theme::MUTED)),
        list_panes[0],
    );
    let list_area = list_panes[1];

    let items: Vec<ListItem> = view.iter().map(|r| ListItem::new(row_lines(r))).collect();
    let title = match filter.hint() {
        Some(q) => format!("pick a repo  {q}"),
        None => format!("pick a repo ({})", view.len()),
    };
    if view.is_empty() {
        let text = if filter.hint().is_some() {
            "Nothing matches this filter."
        } else {
            hub_status.empty_text()
        };
        f.render_widget(
            Paragraph::new(text)
                .block(widgets::pane(&title))
                .wrap(Wrap { trim: false }),
            list_area,
        );
    } else {
        f.render_stateful_widget(
            List::new(items)
                .block(widgets::pane(&title))
                .highlight_style(theme::selected())
                .highlight_symbol("▸ "),
            list_area,
            state,
        );
    }

    if let Some(area) = panes.detail {
        let sel = state.selected().and_then(|i| view.get(i)).copied();
        let text = if sel.is_none() && filter.hint().is_none() {
            hub_status.empty_text().to_string()
        } else {
            detail_text(sel, sel.and_then(|r| shared.get(&r.slug())), notice)
        };
        f.render_widget(
            Paragraph::new(text)
                .block(widgets::pane("inherits"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
    widgets::render_footer(
        f,
        panes.footer,
        if filter.is_active() {
            "type to filter   enter apply   esc cancel"
        } else {
            "↑↓ move   enter pick   / filter   r retry   q cancel"
        },
    );
}

/// One row takes two lines.
///
/// One line does not fit: the list pane takes well under half the body (see
/// [`widgets::layout`]), and on a narrow terminal `owner/repo` plus the session count plus the
/// read-only mark overflows in the worst case. The cost of cramming them onto one line is not
/// that it looks bad — what gets truncated is always the **tail**, which is the `read-only`
/// warning, and that is exactly the part of the row that must not be lost. So the name gets one
/// line and the status another, and neither depends on width to survive.
fn row_lines(r: &Row) -> Vec<Line<'static>> {
    let mut status = vec![Span::styled(
        match r.source {
            Source::Local => format!("  {}", plural(r.sessions, "session")),
            Source::Hub => "  Hub · clone on selection".to_string(),
        },
        Style::default().fg(theme::MUTED),
    )];
    if r.read_only {
        status.push(Span::styled(
            " · read-only",
            Style::default().fg(theme::WARN),
        ));
    }
    vec![Line::from(r.slug()), Line::from(status)]
}

/// `1 sessions` is the screen saying nobody ever reads it.
fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

fn detail_text(r: Option<&Row>, shared: Option<&Shared>, notice: Option<&str>) -> String {
    let Some(r) = r else {
        return "nothing matches this filter.".into();
    };
    let mut out = String::new();
    if let Some(n) = notice {
        out.push_str(n);
        out.push_str("\n\n");
    }
    out.push_str(&format!("repo      {}\n", r.slug()));
    if r.source == Source::Hub {
        out.push_str(&format!(
            "source    Hub (not cloned)\nfrom      {} (verified after clone)\n",
            r.from_ref
        ));
        out.push_str("\nAfter you select this repo and name the session, agit clones it locally and validates the inheritance point.\n\nNo repository or branch is created on the Hub.");
        return out;
    }
    out.push_str(&format!(
        "from      {} ({})\n",
        r.from_ref,
        match r.from_line {
            Some(BranchLine::Session) => "session line — not a valid start for `new`",
            other => line_label(other),
        }
    ));
    out.push_str(&format!("sessions  {}\n", r.sessions));
    if let Some(s) = shared {
        out.push_str(&format!("\nmemory/   {}\n", plural(s.memory, "file")));
        out.push_str(&format!("skills/   {}\n", plural(s.skills, "skill")));
        out.push_str(&format!(
            "AGENTS.md {}\n",
            if s.agents_md { "yes" } else { "no" }
        ));
    }
    if r.read_only {
        out.push_str(
            "\nread-only checkout: starting a session here works, but publishing it needs your \
             own copy (`agit push --mine`).",
        );
    }
    out.push_str("\nthe new session inherits the shared files above, and no conversation.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lines[0]` is the line of the inheritance-point branch; the rest are other branches.
    fn scan(owner: &str, name: &str, lines: &[Option<BranchLine>]) -> Scan {
        Scan {
            owner: owner.into(),
            name: name.into(),
            path: PathBuf::from("/x"),
            branches: (0..lines.len())
                .map(|i| {
                    if i == 0 {
                        DEFAULT_FROM.into()
                    } else {
                        format!("b{i}")
                    }
                })
                .collect(),
            lines: lines.to_vec(),
            from_line: lines.first().copied().flatten(),
            has_upstream: false,
        }
    }

    fn me(scans: Vec<Scan>) -> Input {
        from_me(DEFAULT_FROM, scans)
    }

    fn from_me(from: &str, scans: Vec<Scan>) -> Input {
        Input {
            me: Some("nana".into()),
            from: from.into(),
            scans,
        }
    }

    fn remote(owner: &str, name: &str) -> crate::hub::RemoteAgent {
        crate::hub::RemoteAgent {
            agent_id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
            owner: owner.into(),
            name: name.into(),
            clone_url: "https://example.test/unused.git".into(),
            visibility: "private".into(),
            session_count: 8,
            updated_at: None,
            last_gist: None,
        }
    }

    #[test]
    fn hub_choices_are_owned_repositories_with_safe_exact_slugs() {
        let agents = owned_agents(
            vec![
                remote("nana", "mine"),
                remote("other", "visible"),
                remote("nana", "../escape"),
                remote("nana", "bad\u{1b}[2J"),
                remote("nana", " padded "),
            ],
            "nana",
        );
        assert_eq!(
            agents.iter().map(|agent| agent.slug()).collect::<Vec<_>>(),
            ["nana/mine"]
        );
    }

    #[test]
    fn remote_discovery_preserves_local_validation_and_deduplicates_exact_slugs() {
        let mut rows = assemble(&me(vec![
            scan("nana", "broken", &[None]),
            scan("other", "shared", &[Some(BranchLine::File)]),
        ]));
        merge_hub_rows(
            &mut rows,
            vec![
                remote("nana", "shared"),
                remote("nana", "broken"),
                remote("nana", "shared"),
            ],
            "release",
        );
        assert_eq!(
            rows.iter().map(Row::slug).collect::<Vec<_>>(),
            ["nana/broken", "other/shared", "nana/shared"]
        );
        assert_eq!(rows[0].source, Source::Local);
        assert!(
            choose(&rows[0]).is_err(),
            "Hub metadata cannot hide a broken local file line"
        );
        let hub = &rows[2];
        assert_eq!(hub.source, Source::Hub);
        assert_eq!(hub.from_ref, "release");
        assert_eq!(choose(hub), Ok(Outcome::Pick("nana/shared".into())));
        let text = detail_text(Some(hub), None, None);
        assert!(text.contains("release (verified after clone)"), "{text}");
        assert!(
            !text.contains("no declaration"),
            "unknown remote refs are not corrupt local refs"
        );
    }

    #[test]
    fn discovery_failure_keeps_rows_and_allows_retry_without_an_outstanding_worker() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut discovery = HubDiscovery {
            started: true,
            status: HubStatus::Loading,
            pending: Some(receiver),
        };
        let mut rows = assemble(&me(vec![scan("nana", "mine", &[Some(BranchLine::File)])]));
        assert!(!discovery.poll(&mut rows, "main"));
        assert!(discovery.pending.is_some());
        sender.send(Err(HubStatus::Unavailable)).unwrap();
        assert!(!discovery.poll(&mut rows, "main"));
        assert!(discovery.pending.is_none());
        assert_eq!(rows[0].slug(), "nana/mine");
        assert!(discovery.status.label().contains("r retry"));
    }

    /// The session count counts session lines only.
    ///
    /// A repo fresh out of `agit init`, holding nothing but the file line `main`, must show
    /// **0**. Taking the branch total as the session count makes it show 1 — that trades
    /// "cannot be counted" for "counted wrong", and the second is harder to spot.
    #[test]
    fn a_fresh_repo_has_no_sessions_even_though_it_has_a_branch() {
        let rows = assemble(&me(vec![
            scan("nana", "fresh", &[Some(BranchLine::File)]),
            scan(
                "nana",
                "busy",
                &[
                    Some(BranchLine::File),
                    Some(BranchLine::Session),
                    Some(BranchLine::Session),
                ],
            ),
        ]));
        assert_eq!(rows[1].sessions, 0, "a repo with only main has no sessions");
        assert_eq!(rows[0].sessions, 2);
        // A branch with no line declaration is not a session — guessing one is worse than admitting
        // we cannot tell.
        let unknown = assemble(&me(vec![scan(
            "nana",
            "partial",
            &[Some(BranchLine::File), None],
        )]));
        assert_eq!(unknown[0].sessions, 0);
    }

    /// Sorted lexicographically by slug, independent of the input order.
    #[test]
    fn the_list_is_in_a_stable_alphabetical_order() {
        let rows = assemble(&me(vec![
            scan("nana", "zeta", &[]),
            scan("alice", "beta", &[]),
            scan("nana", "alpha", &[]),
        ]));
        let slugs: Vec<String> = rows.iter().map(|r| r.slug()).collect();
        assert_eq!(slugs, ["alice/beta", "nana/alpha", "nana/zeta"]);
    }

    /// A read-only checkout is marked, and both halves of the test must hold.
    ///
    /// The second half cannot be dropped: a promoted copy sits under someone else's namespace
    /// with its `upstream` pointing at the original, and it **can** be pushed to. The test is
    /// shared with `agit push`.
    #[test]
    fn a_read_only_checkout_is_marked_and_a_promoted_one_is_not() {
        let foreign = scan("alice", "infra", &[]);
        let promoted = Scan {
            has_upstream: true,
            ..scan("alice", "infra", &[])
        };
        assert!(assemble(&me(vec![foreign.clone()]))[0].read_only);
        assert!(!assemble(&me(vec![promoted]))[0].read_only);
        assert!(
            !assemble(&me(vec![scan("nana", "mine", &[])]))[0].read_only,
            "your own checkout is not read-only"
        );
        // Not signed in means "is this mine" cannot be decided. Then it is not marked — telling you
        // your own repo cannot be pushed to is far worse than saying one thing less, and the status
        // bar's not signed in already says it.
        let anon = Input {
            me: None,
            ..from_me(DEFAULT_FROM, vec![foreign])
        };
        assert!(!assemble(&anon)[0].read_only);
    }

    /// A session-line inheritance point does not silently change the meaning; it says on the spot
    /// that this would be a `fork`.
    #[test]
    fn picking_a_session_line_explains_that_it_would_be_a_fork() {
        let rows = assemble(&me(vec![scan(
            "nana",
            "payments",
            &[Some(BranchLine::Session)],
        )]));
        let err = choose(&rows[0]).unwrap_err();
        assert!(
            err.contains("session line"),
            "the message must name the test: {err}"
        );
        assert!(
            err.contains("agit fork nana/payments@main"),
            "the message must give the command form: {err}"
        );
        assert!(
            err.contains("-b <name>"),
            "the message must be typeable as printed: {err}"
        );
    }

    /// `--from` is carried all the way to the screen and never hardwired to `main`.
    ///
    /// `agit new --from v0.3` leaves the key argument empty just the same, so the interface
    /// still opens; if the whole screen reckoned in `main`, it would say it inherits from `main`
    /// while what downstream inherits is `v0.3` — the opposite of "explicit is better than
    /// implicit".
    #[test]
    fn an_explicit_inheritance_point_is_what_the_screen_shows_and_judges() {
        let rows = assemble(&from_me(
            "v0.3",
            vec![scan("nana", "payments", &[Some(BranchLine::Session)])],
        ));
        assert_eq!(rows[0].from_ref, "v0.3");
        let err = choose(&rows[0]).unwrap_err();
        assert!(err.contains("`v0.3` in nana/payments"), "{err}");
        assert!(err.contains("agit fork nana/payments@v0.3"), "{err}");
    }

    /// With an unreadable declaration, the default `main` is blocked and a `--from` the user wrote
    /// is let through.
    ///
    /// The syntax of `--from` is wider than this screen knows (`~n` / `#turn` / `:path` are all
    /// legal), so an unreadable declaration usually means **we** cannot parse it. Blocking here
    /// would kill a command that would have run, and downstream `refs::resolve` knows every
    /// form — leave it to the layer that can tell.
    #[test]
    fn an_unreadable_declaration_only_blocks_the_default_inheritance_point() {
        let unreadable = vec![scan("nana", "payments", &[None])];
        assert!(choose(&assemble(&me(unreadable.clone()))[0]).is_err());
        assert_eq!(
            choose(&assemble(&from_me("v0.3~2", unreadable))[0]),
            Ok(Outcome::Pick("nana/payments".into())),
            "an unrecognized ref syntax goes through to the layer downstream, not ruled out here"
        );
    }

    /// A branch that exists only on the remote still counts.
    ///
    /// Asking only `refs/heads/` mirrors the failure where a branch that was never pushed cannot be
    /// counted — after `agit fetch`, a branch a collaborator just pushed makes it into the list yet
    /// contributes 0 to the session count.
    #[test]
    fn a_branch_that_exists_only_on_the_remote_still_counts() {
        // Two branches plus a trailing pair for the inheritance point, two slots per name:
        // local first, remote second.
        let read = vec![
            Some(BranchLine::File),    // refs/heads/main
            None,                      //   origin/main (local already answered, unused)
            None,                      // refs/heads/theirs — never checked out
            Some(BranchLine::Session), //   origin/theirs
            Some(BranchLine::File),    // the inheritance point itself
            None,
        ];
        let (lines, from_line) = collapse(&read, 2);
        assert_eq!(
            lines,
            vec![Some(BranchLine::File), Some(BranchLine::Session)],
            "local wins; one that exists only on the remote is the fallback"
        );
        assert_eq!(from_line, Some(BranchLine::File));
        // A pair with no answer at all is genuinely unreadable; it must not borrow the next
        // slot.
        assert_eq!(collapse(&[None, None], 1), (vec![None], None));
    }

    /// With no `main` at all the message says something else; it must not be conflated with the one
    /// above.
    #[test]
    fn a_repo_without_a_file_line_says_so_in_its_own_words() {
        let rows = assemble(&me(vec![scan("nana", "broken", &[None])]));
        let err = choose(&rows[0]).unwrap_err();
        assert!(err.contains("no `main` file line"), "{err}");
        assert!(
            !err.contains("fork"),
            "a fork does not solve this one: {err}"
        );
    }

    #[test]
    fn picking_a_file_line_hands_back_the_slug() {
        let rows = assemble(&me(vec![scan(
            "nana",
            "payments",
            &[Some(BranchLine::File)],
        )]));
        assert_eq!(choose(&rows[0]), Ok(Outcome::Pick("nana/payments".into())));
    }

    /// The shared-file overview counts the three paths `new` actually materializes.
    #[test]
    fn the_shared_overview_counts_what_new_will_materialize() {
        let paths: Vec<String> = [
            "AGENTS.md",
            "memory/a.md",
            "memory/b/c.md",
            "skills/x/SKILL.md",
            "session/meta.json",
            "README.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            shared_of(&paths),
            Shared {
                memory: 2,
                skills: 1,
                agents_md: true,
            }
        );
        assert_eq!(shared_of(&[]), Shared::default());
    }

    /// A real render: the session count and the read-only mark both land on the list row.
    #[test]
    fn the_frame_shows_the_session_count_and_the_read_only_mark() {
        use ratatui::backend::TestBackend;
        let rows = assemble(&me(vec![
            scan("alice", "infra", &[Some(BranchLine::File)]),
            scan(
                "nana",
                "payments",
                &[Some(BranchLine::File), Some(BranchLine::Session)],
            ),
        ]));
        let view: Vec<&Row> = rows.iter().collect();
        let mut state = ListState::default();
        state.select(Some(1));
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &view,
                &mut state,
                &Filter::default(),
                &std::collections::HashMap::new(),
                None,
                &HubStatus::SignedOut,
            )
        })
        .unwrap();
        let text: String = {
            let buf = term.backend().buffer();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            text.contains("agit new"),
            "the status bar must name the screen: {text}"
        );
        assert!(text.contains("alice/infra"), "{text}");
        assert!(
            text.contains("read-only"),
            "a read-only checkout must be marked: {text}"
        );
        assert!(text.contains("1 session"), "{text}");
        assert!(
            !text.contains("1 sessions"),
            "the singular must not be written as 1 sessions: {text}"
        );
        assert!(text.contains("enter pick"), "{text}");
    }
}
