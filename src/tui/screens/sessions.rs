//! Data layer for the Sessions screen (bare `agit` / `agit resume`).
//!
//! It owns only "which rows the list has, in what order"; rendering and keys live elsewhere.
//!
//! # Three sources (`docs/07_tui.md` §3.1)
//!
//! | Badge | Meaning | Source |
//! |---|---|---|
//! | `here` | a session adopted in this directory | the store link's `cwd` matches |
//! | `same-repo` | a branch in the same code repo | each branch's `session/meta.json` code anchor |
//! | `unnamed` | a session with no name yet | in the runtime index, unmanaged in the store |
//!
//! The test for the first two sources and the `agit resume` picker **are the same one**
//! (`resume::gather_candidates`); the third is unique to this screen — it is the UI entry point
//! for "waiting to be named".
//!
//! # No transcript parsing
//!
//! Everything this layer works from comes from the runtime index and the store links (the
//! performance discipline in `docs/07_tui.md` §4.1). [`assemble`] is outright a **pure function**:
//! it has no filesystem, so "the first frame parses no transcript" is not a discipline a test has
//! to watch; it is something the types make impossible.
//!
//! The only thing that touches a file is [`worth_naming`], and it runs **only on unadopted
//! candidates**, with a bounded read.

use crate::adapter::SessionRef;
use crate::domain::link::Link;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// How long a transcript file has to go without growing before its session counts as "not
/// running".
///
/// A transcript that is still growing is most likely open in someone else's terminal, and
/// `--resume` puts a second writer on the same file; once the two streams of appends interleave,
/// both histories are destroyed (see `docs/04_workspaces.md` §4). That is data corruption, not an
/// experience problem, so the UI blocks it too.
pub const LIVE_WINDOW: Duration = Duration::from_secs(90);

/// How many bytes at most are read to decide whether an unadopted session is worth listing.
///
/// The window has to hold two things at once: an abandoned empty session in full (the kind
/// `/resume` or `/clear` leaves behind), and the head of a real session up to its first
/// `type:"user"`. So **a file smaller than the window gets an exact answer, and a file larger
/// than it is by definition not that kind of empty shell** — neither side has to guess.
const NAMING_PROBE_BYTES: u64 = 32 * 1024;

/// How many unadopted sessions that probe runs on at most, in one pass.
///
/// One probe costs a bounded amount, but the number of probes scales with "how many unadopted
/// sessions this directory has" — exactly the shape the discipline in `docs/07_tui.md` §4.1
/// watches. So the candidates are sorted by recent activity and only the leading ones are
/// judged; everything past that is kept (fail open). Same precedent as `agit import` reading the
/// opening prompt once, only for the candidates it is about to show.
const NAMING_PROBE_LIMIT: usize = 20;

/// One runtime-index row after the shared naming probe policy has been applied.
pub(super) struct ProbedSession {
    pub session: SessionRef,
    pub worth_naming: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Here,
    SameRepo,
    Unnamed,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::Here => "here",
            Badge::SameRepo => "same-repo",
            Badge::Unnamed => "unnamed",
        }
    }
}

/// One row in the list.
#[derive(Debug, Clone)]
pub struct Row {
    pub badge: Badge,
    /// `owner/name`. An unnamed session is unmanaged, so this is `None`.
    ///
    /// **Always the qualified form.** A store link holds the **bare** agent name, while the
    /// `same-repo` source carries a full slug by construction; with both forms in one field,
    /// dedup compares them against each other and one branch shows up as two rows, once as
    /// `here` and once as `same-repo`. Qualifying happens in [`assemble`] (the caller takes the
    /// owner from the credentials, see [`Input::owner`]).
    pub slug: Option<String>,
    pub branch: Option<String>,
    pub runtime: String,
    pub session_id: Option<String>,
    /// Gist of the opening prompt. Present only when the runtime index hands it over for free
    /// (codex does, claude does not).
    pub gist: Option<String>,
    pub last_active: SystemTime,
    /// The transcript is still growing — a second writer must not take it over.
    pub live: bool,
}

impl Row {
    /// The text the filter matches against.
    pub fn haystack(&self) -> String {
        [
            self.slug.as_deref().unwrap_or_default(),
            self.branch.as_deref().unwrap_or_default(),
            &self.runtime,
            self.gist.as_deref().unwrap_or_default(),
        ]
        .join(" ")
    }
}

/// A session already seen (from the runtime index, **with no transcript ever opened**).
#[derive(Debug, Clone)]
pub struct Seen {
    pub id: String,
    pub runtime: String,
    pub mtime: SystemTime,
    pub gist: Option<String>,
    /// When unadopted: whether this session is worth asking the user to name
    /// ([`worth_naming`]'s verdict).
    pub worth_naming: bool,
}

impl Seen {
    pub fn from_ref(sr: &SessionRef, worth_naming: bool) -> Seen {
        Seen {
            id: sr.id.clone(),
            runtime: sr.runtime.to_string(),
            mtime: sr.mtime,
            gist: sr.gist.clone(),
            worth_naming,
        }
    }
}

/// One branch in the same code repo.
#[derive(Debug, Clone)]
pub struct SameRepo {
    pub slug: String,
    pub branch: String,
    pub last_active: SystemTime,
    /// When this branch's native session was last written to; `None` = this machine has no
    /// session for it at all.
    ///
    /// The single-writer gate rests on this. Sessions from this source run in **another
    /// directory**, so they never show up in [`Input::seen`] (which scans only the current cwd),
    /// and on resume `resume` may reuse that same native session — two writers appending to one
    /// transcript, both histories destroyed (`docs/07_tui.md` §3.1: this is data corruption, not
    /// an experience problem).
    pub last_seen: Option<SystemTime>,
}

/// One adopted link, plus its own timestamp.
#[derive(Debug, Clone)]
pub struct Adopted {
    pub link: Link,
    /// The mtime of the store link file. It is the backstop when the runtime index has no
    /// entry for this session — falling back to `UNIX_EPOCH` sinks a perfectly normal adopted
    /// session to the bottom of the list.
    pub touched: SystemTime,
}

/// Everything the rows are assembled from.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// The canonical path of the current directory.
    pub cwd: String,
    /// The current account name, used to qualify a link's bare agent name into `owner/name`.
    ///
    /// The caller takes it from the credentials rather than [`assemble`] reading it — that would
    /// stop [`assemble`] being a pure function, and the pure function is what makes "the first
    /// frame parses no transcript" a guarantee in the types. `None` when not signed in; both
    /// sources then degrade to bare names and dedup still holds.
    pub owner: Option<String>,
    pub links: Vec<Adopted>,
    pub seen: Vec<Seen>,
    pub same_repo: Vec<SameRepo>,
}

/// The bare name of a slug (its last segment).
///
/// **Dedup always compares bare names, never full slugs.** When not signed in `owner` is `None`,
/// so the `here` source cannot qualify while `same-repo` carries the owner by construction;
/// comparing full slugs walks straight back into the same bug — the same trade-off `same_target`
/// makes in [`crate::commands::context`].
fn bare(slug: &str) -> &str {
    slug.rsplit('/').next().unwrap_or(slug)
}

/// Qualify a bare agent name into `owner/name`; with no owner it comes back unchanged.
fn qualify(owner: Option<&str>, agent: &str) -> String {
    match owner {
        Some(o) if !agent.contains('/') => format!("{o}/{agent}"),
        _ => agent.to_string(),
    }
}

/// Assemble the three sources into one sorted list. **A pure function, with no filesystem.**
pub fn assemble(input: &Input, now: SystemTime) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let seen_by_identity = |runtime: &str, id: &str| {
        input
            .seen
            .iter()
            .find(|s| s.runtime == runtime && s.id == id)
    };

    // ① here: a link whose cwd matches this directory and that is already managed.
    for a in &input.links {
        let l = &a.link;
        if l.cwd.as_deref() != Some(input.cwd.as_str()) {
            continue;
        }
        let (Some(agent), Some(branch)) = (&l.agent, &l.branch) else {
            continue;
        };
        let s = seen_by_identity(&l.source, &l.session_id);
        rows.push(Row {
            badge: Badge::Here,
            // The owner recorded on the link wins: for a session in an org repo (einsia/...)
            // or a read-only checkout (acme/...), qualifying with the login name points
            // at a repo that does not exist, and enter reports "no branch" outright.
            slug: Some(qualify(
                l.owner.as_deref().or(input.owner.as_deref()),
                agent,
            )),
            branch: Some(branch.clone()),
            runtime: l.source.clone(),
            session_id: Some(l.session_id.clone()),
            gist: s.and_then(|s| s.gist.clone()),
            // With no index entry (the transcript was deleted or moved), the link's own time
            // keeps the row off the bottom.
            last_active: s.map(|s| s.mtime).unwrap_or(a.touched),
            // An unknown session is treated as still running. The failure direction matches
            // `is_live`: calling it "live" wrongly only blocks one takeover, calling it "dead"
            // wrongly interleaves two writers' appends into one transcript.
            live: s.map(|s| is_live(s.mtime, now)).unwrap_or(true),
        });
    }

    // ② unnamed: sessions the index can see but the store does not manage.
    //
    // "Unmanaged" covers both no link at all and a link-only record that merely registers
    // existence (the kind `agit hooks ingest` writes) — to the user those are one and the same
    // thing: not named yet.
    let adopted: std::collections::HashSet<(&str, &str)> = input
        .links
        .iter()
        .map(|a| &a.link)
        .filter(|l| l.agent.is_some() && l.branch.is_some())
        .map(|l| (l.source.as_str(), l.session_id.as_str()))
        .collect();
    let ignored: std::collections::HashSet<(&str, &str)> = input
        .links
        .iter()
        .map(|a| &a.link)
        .filter(|l| l.naming_ignored)
        .map(|l| (l.source.as_str(), l.session_id.as_str()))
        .collect();
    for s in &input.seen {
        let identity = (s.runtime.as_str(), s.id.as_str());
        if adopted.contains(&identity) || ignored.contains(&identity) || !s.worth_naming {
            continue;
        }
        rows.push(Row {
            badge: Badge::Unnamed,
            slug: None,
            branch: None,
            runtime: s.runtime.clone(),
            session_id: Some(s.id.clone()),
            gist: s.gist.clone(),
            last_active: s.mtime,
            live: is_live(s.mtime, now),
        });
    }

    // ③ same-repo: branches in the same code repo, minus the ones already listed as here.
    for sr in &input.same_repo {
        let dup = rows.iter().any(|r| {
            r.slug.as_deref().map(bare) == Some(bare(&sr.slug))
                && r.branch.as_deref() == Some(&sr.branch)
        });
        if dup {
            continue;
        }
        rows.push(Row {
            badge: Badge::SameRepo,
            slug: Some(sr.slug.clone()),
            branch: Some(sr.branch.clone()),
            runtime: String::new(),
            session_id: None,
            gist: None,
            last_active: sr.last_active,
            // With no session for it on this machine there is no transcript to collide with;
            // with one, the same window applies, and an unreadable time always counts as live —
            // the same failure direction as the `here` source.
            live: match sr.last_seen {
                Some(seen) => is_live(seen, now),
                None => false,
            },
        });
    }

    rank(&mut rows);
    rows
}

/// Sort: one timeline, most recently active first; on a tie the adopted session comes before
/// the unnamed one. How many unnamed sessions there are is reported by the status bar counter,
/// and takes no part in the order.
pub fn rank(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        b.last_active.cmp(&a.last_active).then_with(|| {
            u8::from(a.badge == Badge::Unnamed).cmp(&u8::from(b.badge == Badge::Unnamed))
        })
    });
}

/// The transcript grew within [`LIVE_WINDOW`].
pub fn is_live(mtime: SystemTime, now: SystemTime) -> bool {
    now.duration_since(mtime)
        .map(|d| d < LIVE_WINDOW)
        .unwrap_or(true) // a clock step back or a cross-machine mtime: better live (no takeover)
}

/// Whether this **unadopted** session is worth asking the user to name.
///
/// `/resume` and `/clear` abandon the startup session in place, leaving an empty transcript with
/// no user turn on disk. Asking for a name again on every switch is pure noise.
///
/// The test looks only at **whether the user ever spoke**, with a bounded read
/// ([`NAMING_PROBE_BYTES`]):
///
/// * codex: the index hands over `first_user_message` for free, not one byte is read;
/// * claude: read the head window looking for `"type":"user"`. A file smaller than the window
///   gives an **exact** answer; a larger one is by definition not that kind of empty shell and is
///   **always kept**.
///
/// When in doubt, keep it (fail open): one noisy row costs far less than a real session that
/// never gets a naming prompt.
pub fn worth_naming(runtime: &str, path: &Path, gist: Option<&str>) -> bool {
    if let Some(g) = gist {
        return !g.trim().is_empty();
    }
    if runtime != "claude-code" {
        return true; // an unrecognized runtime yields no verdict
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    if meta.len() > NAMING_PROBE_BYTES {
        return true; // an incomplete read yields no verdict: a file this large is no empty shell
    }
    match std::fs::read(path) {
        Ok(bytes) => contains_bytes(&bytes, br#""type":"user""#),
        Err(_) => true,
    }
}

/// Substring search. The transcript is UTF-8, but turning it into a `String` first copies the
/// whole file a second time.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ── Fetching data (the only part that touches the filesystem) ──────────

/// Gather the raw material from the store and the runtime index once, and assemble the list.
///
/// **No transcript is opened**, the one exception being [`worth_naming`]'s bounded probe on
/// unadopted candidates.
pub fn collect(cwd: &Path) -> Vec<Row> {
    let now = SystemTime::now();
    assemble(&gather(cwd, now), now)
}

/// Gather the raw material. Split out so [`assemble`] stays a pure function.
fn gather(cwd: &Path, now: SystemTime) -> Input {
    let cwd_s = cwd.to_string_lossy().to_string();
    let store = crate::domain::store::Store::open_or_init().ok();
    let links: Vec<Adopted> = store
        .as_ref()
        .map(|st| {
            crate::domain::link::list(st)
                .into_iter()
                .map(|l| Adopted {
                    touched: crate::domain::link::touched_at(st, &l),
                    link: l,
                })
                .collect()
        })
        .unwrap_or_default();

    let link_refs = links.iter().map(|item| &item.link).collect::<Vec<_>>();
    let seen = probe_sessions_for_naming(cwd, &link_refs)
        .iter()
        .map(|item| Seen::from_ref(&item.session, item.worth_naming))
        .collect();

    let same_repo = same_repo_branches(&links, now);
    Input {
        cwd: cwd_s,
        owner: crate::infra::credentials::current_user(),
        links,
        seen,
        same_repo,
    }
}

/// Gather runtime-index rows and apply the naming probe budget on one recency axis.
///
/// Every TUI that offers unmanaged sessions uses this path so opening a screen cannot multiply
/// transcript reads by the number of candidates in the directory.
pub(super) fn probe_sessions_for_naming(cwd: &Path, links: &[&Link]) -> Vec<ProbedSession> {
    let mut refs: Vec<SessionRef> = Vec::new();
    for rt in crate::adapter::RUNTIMES {
        let Ok(ad) = crate::adapter::get(rt) else {
            continue;
        };
        refs.extend(ad.sessions_for(cwd).unwrap_or_default());
    }
    apply_naming_probe(refs, links, |session| {
        worth_naming(session.runtime, &session.path, session.gist.as_deref())
    })
}

fn apply_naming_probe(
    mut refs: Vec<SessionRef>,
    links: &[&Link],
    mut probe: impl FnMut(&SessionRef) -> bool,
) -> Vec<ProbedSession> {
    // A managed or ignored session never enters the naming queue, so it spends no probe budget.
    let adopted: std::collections::HashSet<(&str, &str)> = links
        .iter()
        .filter(|link| link.agent.is_some() && link.branch.is_some())
        .map(|link| (link.source.as_str(), link.session_id.as_str()))
        .collect();
    let ignored: std::collections::HashSet<(&str, &str)> = links
        .iter()
        .filter(|link| link.naming_ignored)
        .map(|link| (link.source.as_str(), link.session_id.as_str()))
        .collect();

    // The probe budget is spent in order of recent activity: the rows the user is most likely
    // to see are judged first.
    refs.sort_by_key(|r| std::cmp::Reverse(r.mtime));

    let mut budget = NAMING_PROBE_LIMIT;
    refs.into_iter()
        .map(|session| {
            let sr = &session;
            let identity = (sr.runtime, sr.id.as_str());
            let needs_probe = !adopted.contains(&identity) && !ignored.contains(&identity);
            let worth = if !needs_probe {
                false // adopted or ignored: not in the naming queue, so this I/O is not spent
            } else if budget == 0 {
                true // budget spent, so no verdict — keep it rather than hide it
            } else {
                budget -= 1;
                probe(sr)
            };
            ProbedSession {
                session,
                worth_naming: worth,
            }
        })
        .collect()
}

/// When this branch's native session was last written to.
///
/// `None` = the store has no link managing this branch, so this machine has no session for it at
/// all — there is no transcript to collide with, and taking it over creates no second writer.
///
/// A link whose file cannot be read returns **now**, that is, "live". The failure direction
/// matches [`is_live`]: calling it "live" wrongly only blocks one takeover, calling it "dead"
/// wrongly interleaves two streams of appends into one transcript and destroys both histories.
fn branch_last_seen(
    links: &[Adopted],
    slug: &str,
    branch: &str,
    now: SystemTime,
) -> Option<SystemTime> {
    // **Every** matching link counts, and the most recent one wins.
    //
    // One branch can carry more than one link: every session switch inside the runtime has the
    // hook register the new one against the same branch. Looking only at the first link after
    // sorting declares the branch takeable whenever "the first one stopped long ago, some later
    // one is still being written" — and those are exactly the two writers this gate stops.
    //
    // Compare bare names, for the reason in [`bare`]: a link holds the bare agent name, while
    // this source carries a full slug by construction.
    let mut latest: Option<SystemTime> = None;
    for link in links.iter().map(|a| &a.link).filter(|l| {
        l.branch.as_deref() == Some(branch) && l.agent.as_deref().map(bare) == Some(bare(slug))
    }) {
        // An unreadable file is treated as being written right now: calling it "live" wrongly
        // only blocks one takeover, calling it "dead" wrongly interleaves two streams of appends
        // into one transcript.
        let seen = crate::adapter::get(&link.source)
            .ok()
            .and_then(|ad| ad.resolve(&link.session_id, None))
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
            .unwrap_or(now);
        latest = Some(latest.map_or(seen, |cur: SystemTime| cur.max(seen)));
    }
    latest
}

/// Branches in the same code repo. The test comes from
/// [`crate::commands::resume::same_repo_as`] — one shared copy.
fn same_repo_branches(links: &[Adopted], now: SystemTime) -> Vec<SameRepo> {
    let Some(origin) = crate::infra::config::repo_origin() else {
        return Vec::new();
    };
    let Ok(all) = crate::commands::clone::list_local() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (owner, name, path) in all {
        let Some(repo) = crate::domain::repo::Repo::open(&path) else {
            continue;
        };
        let branches = repo.branches();
        let refs: Vec<String> = branches.iter().map(|b| format!("refs/heads/{b}")).collect();
        // Two batches per repo: one for meta (`cat-file`), one for commit times
        // (`for-each-ref`).
        //
        // Asking branch by branch costs linearly in the branch count, and this is on the path
        // bare `agit` must take to draw its first frame, once for **every local repo** — exactly
        // the shape `docs/07_tui.md` §4.1 watches.
        let snaps = crate::domain::meta::at_refs(&repo, &refs);
        let committed = committed_at(&repo);
        for (b, snap) in branches.iter().zip(snaps) {
            let Some(snap) = snap else { continue };
            let Some(code) = &snap.code else { continue };
            if !crate::commands::resume::same_repo_as(code, &origin) {
                continue;
            }
            let slug = format!("{owner}/{name}");
            let last_seen = branch_last_seen(links, &slug, b, now);
            out.push(SameRepo {
                slug,
                branch: b.clone(),
                // A branch's "last active" = the time of its head commit. Missing means the
                // row sinks, and that says the ref disappeared between the two git calls.
                last_active: committed
                    .get(b.as_str())
                    .copied()
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                last_seen,
            });
        }
    }
    out
}

/// The head commit time of every branch in one repo, asked in a single `for-each-ref`.
///
/// Stat-ing `.git/refs/heads/<b>` does not work: in a repo produced by `agit clone` the refs live
/// in `packed-refs` (that is how git clone writes them), so that path does not exist at all and
/// the whole batch of branches degrades to `UNIX_EPOCH` and sinks to the bottom of the list.
/// Asking git holds for packed-refs too, and does not reach around `domain::repo` to touch the
/// layout of .git.
fn committed_at(repo: &crate::domain::repo::Repo) -> std::collections::HashMap<String, SystemTime> {
    let Some(out) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname:short)%09%(committerdate:unix)",
        "refs/heads/",
    ]) else {
        return Default::default();
    };
    out.lines()
        .filter_map(|line| {
            let (name, secs) = line.split_once('\t')?;
            let secs: u64 = secs.trim().parse().ok()?;
            Some((
                name.to_string(),
                SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            ))
        })
        .collect()
}

// ── Screen: rendering and keys ────────────────────────────────────────

use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

/// What the user chose on this screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Continue this session line.
    Resume { slug: String, branch: String },
    /// Open the naming inbox on this session. The destination remains an explicit user choice.
    Adopt { runtime: String, session_id: String },
    /// No candidate at all. **Do not enter an empty TUI**: making the user press q to leave an
    /// empty list wastes an interaction (§4.1).
    Nothing,
    /// The user quit.
    Quit,
}

/// The resident Sessions screen.
///
/// It stays alive for the whole agent session: pick one → **hand the terminal to the runtime** →
/// the runtime exits → take the terminal back → rescan → back to the list (`docs/07_tui.md` §2).
/// So this function does not return until the user presses q.
pub fn run(cwd: &Path) -> crate::CmdResultAlias {
    let rows = collect(cwd);
    if rows.is_empty() {
        // No candidate means no empty shell: making the user press q at an empty list wastes
        // an interaction.
        println!("no session to continue in this directory.");
        crate::ui::hint(
            "adopt one with `agit import`, or start a fresh one with `agit new -b <name>`",
        );
        return Ok(crate::ExitCode::Ok);
    }
    widgets::refresh_rc_status();
    let mut guard = crate::tui::term::Guard::enter()?;
    let out = resident(&mut guard, cwd, rows);
    // Give the terminal back before letting the result (an error above all) propagate: those
    // words belong on the normal screen, not in the alt screen — whatever is written in the alt
    // screen goes with it the moment it exits.
    drop(guard);
    out
}

/// List → handoff → back → list. q quits.
fn resident(
    guard: &mut crate::tui::term::Guard,
    cwd: &Path,
    mut rows: Vec<Row>,
) -> crate::CmdResultAlias {
    let mut deferred = std::collections::HashSet::new();
    let mut naming_focus: Option<super::naming::Identity> = None;
    loop {
        // The inbox is the first stop whenever a new unclaimed session appears, including after
        // the runtime hands the terminal back. A skip lives in `deferred` only for this resident
        // visit; selecting that unnamed row from the Sessions screen removes it and opens the
        // inbox again.
        if super::naming::has_pending(&rows, &deferred) {
            match super::naming::run(&rows, cwd, &mut deferred, naming_focus.as_ref())? {
                super::naming::Outcome::Quit => return Ok(crate::ExitCode::Ok),
                super::naming::Outcome::Done => naming_focus = None,
                super::naming::Outcome::Adopt(choice) => {
                    naming_focus = None;
                    let _ = super::naming::execute_import(guard, &choice)?;
                    rows = collect(cwd);
                    if rows.is_empty() {
                        return Ok(crate::ExitCode::Ok);
                    }
                    continue;
                }
            }
        }
        match run_loop(&rows)? {
            Outcome::Quit | Outcome::Nothing => return Ok(crate::ExitCode::Ok),
            Outcome::Adopt {
                runtime,
                session_id,
            } => {
                let identity = super::naming::Identity {
                    runtime,
                    session_id,
                };
                deferred.remove(&identity);
                naming_focus = Some(identity);
            }
            Outcome::Resume { slug, branch } => {
                rows = handoff(guard, cwd, &slug, &branch)?;
                if rows.is_empty() {
                    return Ok(crate::ExitCode::Ok);
                }
            }
        }
    }
}

/// Hand the terminal to the runtime, wait for it to finish, take it back and rescan.
///
/// # Why the summary prints outside the alt screen
///
/// Once the user closes the interface, the terminal still shows what happened, exactly as it does
/// after an ordinary command (`docs/07_tui.md` §2). Whatever is written in the alt screen is gone
/// the moment it exits, leaving that stretch blank.
fn handoff(
    guard: &mut crate::tui::term::Guard,
    cwd: &Path,
    slug: &str,
    branch: &str,
) -> crate::Result<Vec<Row>> {
    guard.suspend()?;
    println!(
        "\n{} {slug} @ {branch}",
        crate::ui::accent(crate::ui::theme::symbols().arrow)
    );
    // The rules for loading and launching exist once, in `commands::resume`.
    //
    // **Errors propagate**: swallowing a failure like a corrupt repo or a runtime command that
    // cannot be assembled leaves the user watching the interface rescan and come back to the
    // list, with no failure exit code and no error to inspect — the same path reports an error
    // from the command line and stays silent from the interface, which is two behaviors. The
    // guard sits one level up, so the terminal is restored either way.
    //
    // The exit **code** does not propagate: the runtime exiting non-zero on its own (the user
    // hit an error inside it, or pressed Ctrl-C) is a normal end to a session and must not close
    // this screen too.
    crate::commands::resume::launch_branch(slug, branch)?;
    // Rescan on the way back: new sessions and changes in management are seen at this step.
    // All of it comes from the runtime index and the store; no transcript is opened (§6.3).
    let rows = collect(cwd);
    widgets::refresh_rc_status();
    guard.resume()?;
    Ok(rows)
}

fn run_loop(rows: &[Row]) -> crate::Result<Outcome> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default();
    state.select(Some(0));
    let mut filter = Filter::default();
    let mut notice: Option<String> = None;

    loop {
        let view: Vec<&Row> = rows
            .iter()
            .filter(|r| filter.matches(&r.haystack()))
            .collect();
        if state.selected().unwrap_or(0) >= view.len() {
            state.select(if view.is_empty() {
                None
            } else {
                Some(view.len() - 1)
            });
        }
        term.draw(|f| draw(f, &view, &mut state, &filter, notice.as_deref()))?;

        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        // Filter input mode: keys go to it first.
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
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Outcome::Quit);
            }
            KeyCode::Char('/') => filter.open(),
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
                    // A blocked case (the session is still running, or it has no name yet)
                    // does not leave the screen; the reason goes in front of the user so they
                    // can pick another row on the spot.
                    Err(why) => notice = Some(why),
                }
            }
            _ => {}
        }
    }
}

/// What happens when a row is selected. A pure function — "when continuing is not allowed" is
/// a test, not rendering.
fn choose(r: &Row) -> Result<Outcome, String> {
    if r.live {
        return Err(format!(
            "{} looks like it is still running (its transcript grew within the last {}s). \
             resuming it now would put two writers on one transcript and destroy both \
             histories — quit it in its own terminal first.",
            r.slug.as_deref().unwrap_or("this session"),
            LIVE_WINDOW.as_secs()
        ));
    }
    match (&r.slug, &r.branch) {
        (Some(slug), Some(branch)) => Ok(Outcome::Resume {
            slug: slug.clone(),
            branch: branch.clone(),
        }),
        _ => match &r.session_id {
            Some(id) => Ok(Outcome::Adopt {
                runtime: r.runtime.clone(),
                session_id: id.clone(),
            }),
            None => Err("this row has no session to continue.".into()),
        },
    }
}

fn draw(
    f: &mut Frame,
    view: &[&Row],
    state: &mut ListState,
    filter: &Filter,
    notice: Option<&str>,
) {
    let panes = widgets::layout(f.area());
    let unnamed = view.iter().filter(|r| r.badge == Badge::Unnamed).count();
    widgets::render_status(
        f,
        panes.status,
        &widgets::Status {
            title: "agit".into(),
            identity: crate::infra::credentials::current_user()
                .map(|u| format!("{u} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: widgets::Counters { unnamed },
        },
    );
    let list_area = widgets::list_area_with_notice(f, panes, notice);

    let width = list_area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = view
        .iter()
        .map(|r| ListItem::new(row_line(r, width)))
        .collect();
    let title = match filter.hint() {
        Some(q) => format!("sessions  {q}"),
        None => format!("sessions ({})", view.len()),
    };
    f.render_stateful_widget(
        List::new(items)
            .block(widgets::pane(&title))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        list_area,
        state,
    );

    if let Some(area) = panes.detail {
        let sel = state.selected().and_then(|i| view.get(i));
        f.render_widget(
            Paragraph::new(detail_text(sel.copied(), notice))
                .block(widgets::pane("details"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
    // The key hints follow the mode: while filter input is active `q` types into the query; it
    // does not quit — a footer that still reads `q quit` is the screen telling a lie.
    widgets::render_footer(
        f,
        panes.footer,
        if filter.is_active() {
            "type to filter   enter apply   esc cancel"
        } else {
            "↑↓ move   enter continue   / filter   q quit"
        },
    );
}

fn row_line(r: &Row, width: usize) -> Line<'static> {
    let s = theme::symbols();
    let mark = if r.live { s.active } else { s.idle };
    let (badge_color, name) = match r.badge {
        Badge::Unnamed => (
            theme::WARN,
            r.session_id
                .as_deref()
                .map(crate::domain::link::short)
                .unwrap_or_default(),
        ),
        _ => (
            theme::MUTED,
            format!(
                "{} @ {}",
                r.slug.as_deref().unwrap_or_default(),
                r.branch.as_deref().unwrap_or_default()
            ),
        ),
    };
    let active = crate::ui::ago(r.last_active);
    let name_width = width.saturating_sub(14 + widgets::cols(&active));
    let name = widgets::truncate_cols(&name, name_width);
    widgets::clamp_line(
        Line::from(vec![
            Span::raw(format!("{mark} ")),
            Span::styled(
                format!("{:<9} ", r.badge.label()),
                Style::default().fg(badge_color),
            ),
            Span::raw(name),
            Span::styled(format!("  {active}"), theme::muted()),
        ]),
        width,
    )
}

fn detail_text(r: Option<&Row>, notice: Option<&str>) -> String {
    let Some(r) = r else {
        return "nothing matches this filter.".into();
    };
    let mut out = String::new();
    if let Some(n) = notice {
        out.push_str(n);
        out.push_str("\n\n");
    }
    if let Some(slug) = &r.slug {
        out.push_str(&format!("repo     {slug}\n"));
    }
    if let Some(b) = &r.branch {
        out.push_str(&format!("branch   {b}\n"));
    }
    if !r.runtime.is_empty() {
        out.push_str(&format!("runtime  {}\n", r.runtime));
    }
    if let Some(id) = &r.session_id {
        out.push_str(&format!("session  {}\n", crate::domain::link::short(id)));
    }
    out.push_str(&format!("active   {}\n", crate::ui::ago(r.last_active)));
    if let Some(g) = &r.gist {
        out.push_str(&format!("\n{g}\n"));
    }
    if r.badge == Badge::Unnamed {
        out.push_str(
            "\nthis session is not under version control yet.\nenter shows how to adopt it.",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }
    fn seen(id: &str, at: u64) -> Seen {
        Seen {
            id: id.into(),
            runtime: "claude-code".into(),
            mtime: t(at),
            gist: None,
            worth_naming: true,
        }
    }
    fn session_ref(id: &str, at: u64) -> SessionRef {
        SessionRef {
            id: id.into(),
            path: Path::new("/nonexistent").join(id),
            runtime: "claude-code",
            cwd: Some("/w".into()),
            mtime: t(at),
            gist: None,
        }
    }
    fn link(id: &str, cwd: &str, agent: Option<&str>, branch: Option<&str>) -> Adopted {
        let mut l = Link::new("claude-code", id, Some(Path::new(cwd)));
        l.agent = agent.map(Into::into);
        l.branch = branch.map(Into::into);
        Adopted {
            link: l,
            touched: t(0),
        }
    }

    /// Each of the three sources produces one row, on a single recency axis.
    #[test]
    fn the_three_sources_land_on_one_recency_axis() {
        let input = Input {
            cwd: "/w".into(),
            owner: Some("nana".into()),
            links: vec![
                link("A", "/w", Some("payments"), Some("refund-fix")),
                link("B", "/w", None, None), // link-only: not named yet
            ],
            seen: vec![seen("A", 500), seen("B", 100)],
            same_repo: vec![SameRepo {
                slug: "nana/infra".into(),
                branch: "deploy".into(),
                last_active: t(900),
                last_seen: None,
            }],
        };
        let rows = assemble(&input, t(1000));
        let badges: Vec<_> = rows.iter().map(|r| r.badge).collect();
        assert_eq!(
            badges,
            vec![Badge::SameRepo, Badge::Here, Badge::Unnamed],
            "one timeline: 900 > 500 > 100, not grouped by category"
        );
        assert_eq!(rows[1].slug.as_deref(), Some("nana/payments"));
        assert_eq!(rows[1].branch.as_deref(), Some("refund-fix"));
    }

    /// On a tie the adopted session comes before the unnamed one; the owner recorded on the
    /// link wins over the login name.
    #[test]
    fn ties_prefer_adopted_and_the_links_owner_wins() {
        let mut l = link("A", "/w", Some("agent-git"), Some("run-1"));
        l.link.owner = Some("acme".into());
        let input = Input {
            cwd: "/w".into(),
            owner: Some("hachi".into()),
            links: vec![l],
            seen: vec![seen("A", 500), seen("Z", 500)],
            ..Default::default()
        };
        let rows = assemble(&input, t(1000));
        assert_eq!(
            rows[0].badge,
            Badge::Here,
            "on a tie, the adopted session comes first"
        );
        assert_eq!(
            rows[0].slug.as_deref(),
            Some("acme/agent-git"),
            "the login name must not impersonate an org or read-only checkout owner"
        );
        assert_eq!(rows[1].badge, Badge::Unnamed);
    }

    /// A session from another directory does not enter this list.
    #[test]
    fn another_directorys_session_is_not_listed() {
        let input = Input {
            cwd: "/w".into(),
            links: vec![link("A", "/elsewhere", Some("x"), Some("b"))],
            ..Default::default()
        };
        assert!(assemble(&input, t(1)).is_empty());
    }

    /// An adopted session does not show up a second time as "waiting to be named".
    #[test]
    fn an_adopted_session_is_not_also_offered_for_naming() {
        let input = Input {
            cwd: "/w".into(),
            links: vec![link("A", "/w", Some("payments"), Some("refund-fix"))],
            seen: vec![seen("A", 10)],
            ..Default::default()
        };
        let rows = assemble(&input, t(20));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].badge, Badge::Here);
    }

    /// A dismissed session stays out of the naming queue without becoming managed. Runtime is
    /// part of the identity, so dismissing a Codex id must not hide a Claude session whose id
    /// happens to match it.
    #[test]
    fn a_dismissed_session_is_hidden_only_in_its_runtime() {
        let mut dismissed = link("A", "/w", None, None);
        dismissed.link.source = "codex".into();
        dismissed.link.naming_ignored = true;
        let input = Input {
            cwd: "/w".into(),
            links: vec![dismissed],
            seen: vec![
                Seen {
                    runtime: "codex".into(),
                    ..seen("A", 20)
                },
                seen("A", 10),
            ],
            ..Default::default()
        };

        let rows = assemble(&input, t(100));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].runtime, "claude-code");
        assert_eq!(rows[0].badge, Badge::Unnamed);
    }

    /// An adopted row takes activity and summary data from the runtime recorded on its link.
    /// Session identifiers are runtime-local, so matching only the identifier can borrow another
    /// runtime's liveness and incorrectly block or permit takeover.
    #[test]
    fn an_adopted_session_matches_runtime_and_id() {
        let mut adopted = link("A", "/w", Some("payments"), Some("refund-fix"));
        adopted.link.source = "codex".into();
        let input = Input {
            cwd: "/w".into(),
            links: vec![adopted],
            seen: vec![
                Seen {
                    gist: Some("claude summary".into()),
                    worth_naming: false,
                    ..seen("A", 95)
                },
                Seen {
                    runtime: "codex".into(),
                    gist: Some("codex summary".into()),
                    ..seen("A", 10)
                },
            ],
            ..Default::default()
        };

        let rows = assemble(&input, t(100));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].runtime, "codex");
        assert_eq!(rows[0].gist.as_deref(), Some("codex summary"));
        assert_eq!(rows[0].last_active, t(10));
        assert!(!rows[0].live);
    }

    /// same-repo does not duplicate a branch `here` already lists.
    ///
    /// **The two sources spell a slug differently by construction**: a store link holds the bare
    /// agent name (`payments`), the same-repo source a full slug (`nana/payments`). Writing
    /// `payments` on both sides hides the bug where dedup compares the two forms against each
    /// other and never finds them equal on real data — one branch then shows up as two rows,
    /// once as here and once as same-repo.
    #[test]
    fn same_repo_does_not_duplicate_a_row_already_here() {
        let input = Input {
            cwd: "/w".into(),
            owner: Some("nana".into()),
            links: vec![link("A", "/w", Some("payments"), Some("refund-fix"))],
            seen: vec![seen("A", 10)],
            same_repo: vec![SameRepo {
                slug: "nana/payments".into(), // a full slug — a different form from the link's
                branch: "refund-fix".into(),
                last_active: t(10),
                last_seen: None,
            }],
        };
        let rows = assemble(&input, t(20));
        assert_eq!(
            rows.len(),
            1,
            "one branch must not produce two rows: {rows:?}"
        );
        assert_eq!(
            rows[0].slug.as_deref(),
            Some("nana/payments"),
            "what is displayed is always the qualified form"
        );
    }

    /// With no sign-in there is no owner to qualify with; both sources degrade to bare names,
    /// and dedup still holds.
    #[test]
    fn dedup_still_holds_when_the_owner_is_unknown() {
        let input = Input {
            cwd: "/w".into(),
            owner: None,
            links: vec![link("A", "/w", Some("payments"), Some("refund-fix"))],
            seen: vec![seen("A", 10)],
            same_repo: vec![SameRepo {
                slug: "nana/payments".into(),
                branch: "refund-fix".into(),
                last_active: t(10),
                last_seen: None,
            }],
        };
        assert_eq!(assemble(&input, t(20)).len(), 1);
    }

    /// A session missing from the index must not sink, and must not be declared takeable.
    ///
    /// Both failure directions match `is_live` — calling it "live" wrongly only blocks one
    /// takeover, calling it "dead" wrongly interleaves two writers' appends into one transcript,
    /// which is data corruption.
    #[test]
    fn a_session_missing_from_the_index_is_neither_sunk_nor_declared_dead() {
        let input = Input {
            cwd: "/w".into(),
            owner: Some("nana".into()),
            links: vec![Adopted {
                touched: t(900),
                ..link("A", "/w", Some("payments"), Some("refund-fix"))
            }],
            seen: vec![], // the transcript was deleted or moved: not in the index
            ..Default::default()
        };
        let rows = assemble(&input, t(1000));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].last_active,
            t(900),
            "the link's own time is the backstop; the row must not sink to 1970"
        );
        assert!(
            rows[0].live,
            "an unknown session is treated as still running"
        );
    }

    /// An empty transcript does not enter the naming queue — asking for a name again on every
    /// `/resume` is pure noise.
    #[test]
    fn an_abandoned_session_is_not_offered_for_naming() {
        let mut s = seen("B", 10);
        s.worth_naming = false;
        let input = Input {
            cwd: "/w".into(),
            seen: vec![s],
            ..Default::default()
        };
        assert!(assemble(&input, t(20)).is_empty());
    }

    /// A branch in the same code repo passes the single-writer gate too.
    ///
    /// Sessions from this source run in **another directory**, so they never show up in `seen`
    /// (which scans only the current cwd). Marking them inactive unconditionally lets a branch
    /// being written elsewhere through for takeover, and `resume` may reuse that same native
    /// session — two streams of appends interleaving into one transcript, both histories
    /// destroyed. That is data corruption, not an experience problem, so this source goes
    /// through the same window as `here`.
    #[test]
    fn a_same_repo_branch_running_elsewhere_is_not_offered_for_takeover() {
        let running = |seen: Option<u64>| Input {
            cwd: "/w".into(),
            owner: Some("nana".into()),
            same_repo: vec![SameRepo {
                slug: "nana/infra".into(),
                branch: "deploy".into(),
                last_active: t(900),
                last_seen: seen.map(t),
            }],
            ..Default::default()
        };
        // just written elsewhere: blocked.
        let rows = assemble(&running(Some(1000)), t(1010));
        assert!(
            rows[0].live,
            "a branch still running in another directory must not be taken over"
        );
        assert!(choose(&rows[0]).is_err());

        // long since stopped: continuing is allowed.
        let rows = assemble(&running(Some(1000)), t(9000));
        assert!(!rows[0].live);
        assert!(choose(&rows[0]).is_ok());

        // no session for it on this machine at all: no transcript to collide with, allowed.
        let rows = assemble(&running(None), t(1010));
        assert!(!rows[0].live, "with no session there is no second writer");
    }

    /// Growth within [`LIVE_WINDOW`] means "running"; on a clock step back, better live.
    #[test]
    fn liveness_errs_on_the_side_of_not_taking_over() {
        assert!(is_live(t(1000), t(1030)));
        assert!(!is_live(t(1000), t(1200)));
        // an mtime in the future (clock stepped back, or another machine): no takeover.
        assert!(is_live(t(2000), t(1000)));
    }

    /// The naming test: a small file gets an exact answer, a large file is kept, and a gist
    /// the index hands over for free wins.
    #[test]
    fn the_naming_probe_is_bounded_and_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.jsonl");
        std::fs::write(
            &real,
            "{\"type\":\"mode\"}\n{\"type\":\"user\",\"message\":{}}\n",
        )
        .unwrap();
        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, "{\"type\":\"mode\"}\n{\"type\":\"attachment\"}\n").unwrap();
        let big = dir.path().join("big.jsonl");
        std::fs::write(&big, vec![b'x'; (super::NAMING_PROBE_BYTES + 1) as usize]).unwrap();

        assert!(worth_naming("claude-code", &real, None));
        assert!(
            !worth_naming("claude-code", &empty, None),
            "the whole file is read and no user ever spoke — an exact answer, not a guess"
        );
        assert!(
            worth_naming("claude-code", &big, None),
            "an incomplete read yields no verdict (fail open)"
        );
        // codex: a gist from the index means no file is read.
        assert!(worth_naming(
            "codex",
            Path::new("/nonexistent"),
            Some("refactor settlement")
        ));
        assert!(!worth_naming(
            "codex",
            Path::new("/nonexistent"),
            Some("  ")
        ));
        // a missing file is kept too; one failed read does not hide a session.
        assert!(worth_naming("claude-code", Path::new("/nonexistent"), None));
    }

    /// Naming screens spend their shared probe budget on the most recent eligible sessions.
    /// Managed and ignored rows spend none, and eligible rows beyond the budget remain visible.
    #[test]
    fn the_naming_probe_budget_is_shared_and_fails_open() {
        let mut managed = Link::new("claude-code", "managed", Some(Path::new("/w")));
        managed.agent = Some("payments".into());
        managed.branch = Some("work".into());
        let mut ignored = Link::new("claude-code", "ignored", Some(Path::new("/w")));
        ignored.naming_ignored = true;
        let links = [&managed, &ignored];

        let eligible = NAMING_PROBE_LIMIT + 2;
        let mut refs = (0..eligible)
            .map(|index| session_ref(&format!("candidate-{index}"), index as u64))
            .collect::<Vec<_>>();
        refs.push(session_ref("managed", 10_000));
        refs.push(session_ref("ignored", 9_999));

        let mut probed = Vec::new();
        let rows = apply_naming_probe(refs, &links, |session| {
            probed.push(session.id.clone());
            false
        });
        assert_eq!(probed.len(), NAMING_PROBE_LIMIT);
        assert_eq!(probed[0], format!("candidate-{}", eligible - 1));
        assert!(!probed.iter().any(|id| id == "managed" || id == "ignored"));

        let eligible_rows = rows
            .iter()
            .filter(|row| row.session.id.starts_with("candidate-"))
            .collect::<Vec<_>>();
        assert!(
            eligible_rows
                .iter()
                .take(NAMING_PROBE_LIMIT)
                .all(|row| !row.worth_naming)
        );
        assert!(
            eligible_rows
                .iter()
                .skip(NAMING_PROBE_LIMIT)
                .all(|row| row.worth_naming)
        );
    }

    #[test]
    fn a_row_matches_on_repo_branch_runtime_and_gist() {
        let input = Input {
            cwd: "/w".into(),
            links: vec![link("A", "/w", Some("payments"), Some("refund-fix"))],
            seen: vec![Seen {
                // CJK fixture (AGENTS.md exception iii): the filter matches a CJK gist as a
                // substring, with no tokenizer in between.
                gist: Some("修掉退款重试".into()),
                ..seen("A", 10)
            }],
            ..Default::default()
        };
        let h = assemble(&input, t(20))[0].haystack();
        for needle in ["payments", "refund-fix", "claude-code", "修掉退款重试"] {
            assert!(
                h.contains(needle),
                "{needle} is not in the filterable text: {h}"
            );
        }
    }
}
