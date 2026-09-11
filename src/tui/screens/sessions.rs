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
//! # Discovery and presentation
//!
//! [`assemble`] is a pure function over already collected rows; it performs no filesystem I/O.
//! Discovery uses runtime indexes and local claims, with bounded empty-shell and opening-window
//! probes for unmanaged candidates. The index and exact recorded paths remain discovery data,
//! never authority to adopt or resume a session.

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
    Elsewhere,
    Unnamed,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::Here => "here",
            Badge::SameRepo => "same-repo",
            Badge::Elsewhere => "elsewhere",
            Badge::Unnamed => "unnamed",
        }
    }
}

/// One row in the list.
#[derive(Debug, Clone)]
pub struct Row {
    pub badge: Badge,
    /// `owner/name` when the namespace is known. An ownerless legacy name stays unqualified
    /// and cannot suppress a qualified candidate; an unnamed session has no repository.
    pub slug: Option<String>,
    pub branch: Option<String>,
    pub runtime: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub here: bool,
    /// An indexed opening prompt or an advisory prompt from a bounded opening window.
    pub gist: Option<String>,
    pub last_active: SystemTime,
    /// The transcript is still growing — a second writer must not take it over.
    pub live: bool,
    pub ambiguous: bool,
}

impl Row {
    /// The text the filter matches against.
    pub fn haystack(&self) -> String {
        [
            self.slug.as_deref().unwrap_or_default(),
            self.branch.as_deref().unwrap_or_default(),
            &self.runtime,
            self.cwd.as_deref().unwrap_or_default(),
            self.gist.as_deref().unwrap_or_default(),
        ]
        .join(" ")
    }
}

/// A runtime-index session enriched by the bounded advisory probe policy.
#[derive(Debug, Clone)]
pub struct Seen {
    pub cwd: Option<String>,
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
            cwd: sr.cwd.clone(),
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
    pub runtime: String,
    pub cwd: Option<String>,
    pub slug: String,
    pub branch: String,
    pub last_active: SystemTime,
    /// When this branch's native session was last written to; `None` = this machine has no
    /// session for it at all.
    ///
    /// The single-writer gate rests on this. Sessions from this source run in **another
    /// directory**, so a current-project index can omit them. On resume, `resume` may reuse
    /// that same native session — two writers appending to one
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
    pub all_projects: bool,
    /// The current account name, used to qualify a link's bare agent name into `owner/name`.
    ///
    /// The caller supplies the identity so [`assemble`] remains a pure function over collected
    /// facts. An ownerless link stays unqualified when no account is available.
    pub owner: Option<String>,
    pub links: Vec<Adopted>,
    pub seen: Vec<Seen>,
    pub same_repo: Vec<SameRepo>,
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

    // Managed links retain their recorded project; all-project scope also includes other directories.
    for a in &input.links {
        let l = &a.link;
        if !l.is_active() || (!input.all_projects && l.cwd.as_deref() != Some(input.cwd.as_str())) {
            continue;
        }
        let (Some(agent), Some(branch)) = (&l.agent, &l.branch) else {
            continue;
        };
        let s = seen_by_identity(&l.source, &l.session_id);
        rows.push(Row {
            ambiguous: false,
            badge: if l.cwd.as_deref() == Some(input.cwd.as_str()) {
                Badge::Here
            } else {
                Badge::Elsewhere
            },
            cwd: l.cwd.clone(),
            here: l.cwd.as_deref() == Some(input.cwd.as_str()),
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
    let mut occurrences = std::collections::HashMap::new();
    for seen in &input.seen {
        *occurrences
            .entry((seen.runtime.as_str(), seen.id.as_str()))
            .or_insert(0usize) += 1;
    }
    for s in &input.seen {
        let identity = (s.runtime.as_str(), s.id.as_str());
        if adopted.contains(&identity)
            || ignored.contains(&identity)
            || !s.worth_naming
            || (!input.all_projects && s.cwd.as_deref().is_none_or(|cwd| cwd != input.cwd))
        {
            continue;
        }
        rows.push(Row {
            ambiguous: occurrences[&identity] > 1,
            badge: Badge::Unnamed,
            cwd: s.cwd.clone(),
            here: s.cwd.as_deref() == Some(input.cwd.as_str()),
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
        let existing = rows.iter_mut().find(|r| {
            r.slug.as_deref() == Some(sr.slug.as_str()) && r.branch.as_deref() == Some(&sr.branch)
        });
        if let Some(row) = existing {
            if row.badge == Badge::Elsewhere {
                row.badge = Badge::SameRepo;
                row.here = true;
                row.live |= sr.last_seen.is_some_and(|seen| is_live(seen, now));
            }
            continue;
        }
        rows.push(Row {
            ambiguous: false,
            badge: Badge::SameRepo,
            slug: Some(sr.slug.clone()),
            branch: Some(sr.branch.clone()),
            runtime: sr.runtime.clone(),
            session_id: None,
            cwd: sr.cwd.clone(),
            here: true,
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
    use std::io::Read;
    let Some(file) = super::selector::opening_file(path) else {
        return true;
    };
    let Ok(meta) = file.metadata() else {
        return true;
    };
    if meta.len() > NAMING_PROBE_BYTES {
        return true; // an incomplete read yields no verdict: a file this large is no empty shell
    }
    let mut bytes = Vec::new();
    if file
        .take(NAMING_PROBE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > NAMING_PROBE_BYTES
    {
        return true;
    }
    contains_bytes(&bytes, br#""type":"user""#)
}

/// Substring search. The transcript is UTF-8, but turning it into a `String` first copies the
/// whole file a second time.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ── Fetching data (the only part that touches the filesystem) ──────────

/// Gather the raw material from the store and the runtime index once, and assemble the list.
///
/// Native reads are restricted to bounded advisory opening windows on unadopted candidates.
pub fn collect(cwd: &Path) -> Vec<Row> {
    let now = SystemTime::now();
    assemble(&gather(cwd, now, false), now)
}

/// Gather the raw material. Split out so [`assemble`] stays a pure function.
fn collect_all(cwd: &Path) -> Vec<Row> {
    let now = SystemTime::now();
    assemble(&gather(cwd, now, true), now)
}

fn gather(cwd: &Path, now: SystemTime, all_projects: bool) -> Input {
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
    let seen = probe_sessions_for_scope(cwd, &link_refs, all_projects)
        .iter()
        .map(|item| Seen::from_ref(&item.session, item.worth_naming))
        .collect();

    let same_repo = same_repo_branches(&links, now);
    Input {
        cwd: cwd_s,
        all_projects,
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
pub(super) fn probe_sessions_for_scope(
    cwd: &Path,
    links: &[&Link],
    all_projects: bool,
) -> Vec<ProbedSession> {
    let mut refs: Vec<SessionRef> = Vec::new();
    for rt in crate::adapter::RUNTIMES {
        let Ok(ad) = crate::adapter::get(rt) else {
            continue;
        };
        let here = ad.sessions_for(cwd).unwrap_or_default();
        let mut known: std::collections::HashSet<_> = here
            .iter()
            .map(|row| (row.id.clone(), row.path.clone()))
            .collect();
        refs.extend(here);
        if all_projects {
            refs.extend(
                ad.all_sessions()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|row| known.insert((row.id.clone(), row.path.clone()))),
            );
        }
    }
    let mut rows = apply_naming_probe(refs, links, |session| {
        worth_naming(session.runtime, &session.path, session.gist.as_deref())
    });
    for item in rows
        .iter_mut()
        .filter(|item| item.worth_naming)
        .take(NAMING_PROBE_LIMIT)
    {
        let session = &mut item.session;
        if session.runtime != "cursor" && (session.gist.is_none() || session.cwd.is_none()) {
            let preview = super::selector::preview(session.runtime, &session.path);
            if session.gist.is_none() {
                session.gist = preview.gist;
            }
            if let Some(cwd) = preview.cwd {
                session.cwd = Some(cwd);
            }
        }
    }
    rows
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
    let Some((owner, agent)) = slug.split_once('/') else {
        return Some(now);
    };
    // **Every** matching link counts, and the most recent one wins.
    //
    // One branch can carry more than one link: every session switch inside the runtime has the
    // hook register the new one against the same branch. Looking only at the first link after
    // sorting declares the branch takeable whenever "the first one stopped long ago, some later
    // one is still being written" — and those are exactly the two writers this gate stops.
    //
    // Recorded owners cannot lend activity to another namespace. The ordinary claim predicate
    // retains the conservative legacy ownerless claim gate without merging those display rows.
    let mut latest: Option<SystemTime> = None;
    for link in links
        .iter()
        .map(|a| &a.link)
        .filter(|link| crate::domain::link::claims_branch(link, owner, agent, branch))
    {
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
                runtime: snap.runtime.clone(),
                cwd: (!snap.cwd.is_empty()).then(|| snap.cwd.clone()),
                slug,
                branch: b.clone(),
                // Missing or unrepresentable commit activity sinks the row without hiding it.
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
pub(crate) fn committed_at(
    repo: &crate::domain::repo::Repo,
) -> std::collections::HashMap<String, SystemTime> {
    let Some(out) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname)%09%(committerdate:unix)",
        "refs/heads/",
    ]) else {
        return Default::default();
    };
    out.lines()
        .filter_map(|line| {
            let (name, secs) = line.split_once('\t')?;
            let secs: u64 = secs.trim().parse().ok()?;
            Some((
                name.strip_prefix("refs/heads/")?.to_string(),
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs))?,
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
    let rows = collect_all(cwd);
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
    let runtimes = super::selector::runtimes(rows.iter().map(|row| row.runtime.as_str()));
    let out = match super::selector::preselect(&runtimes, "agit resume")? {
        Some(scope) => resident(&mut guard, cwd, rows, scope),
        None => Ok(crate::ExitCode::Ok),
    };
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
    mut scope: super::selector::Scope,
) -> crate::CmdResultAlias {
    let mut deferred = std::collections::HashSet::new();
    let mut naming_focus: Option<super::naming::Identity> = None;
    loop {
        // The inbox is the first stop whenever a new unclaimed session appears, including after
        // the runtime hands the terminal back. A skip lives in `deferred` only for this resident
        // visit; selecting that unnamed row from the Sessions screen removes it and opens the
        // inbox again.
        let scoped_rows: Vec<_> = rows
            .iter()
            .filter(|row| scope.includes(&row.runtime, row.here))
            .cloned()
            .collect();
        if super::naming::has_pending(&scoped_rows, &deferred) {
            match super::naming::run(&scoped_rows, cwd, &mut deferred, naming_focus.as_ref())? {
                super::naming::Outcome::Quit => return Ok(crate::ExitCode::Ok),
                super::naming::Outcome::Done => naming_focus = None,
                super::naming::Outcome::Projects => {
                    scope.all_projects = !scope.all_projects;
                    naming_focus = None;
                    continue;
                }
                super::naming::Outcome::Runtimes => {
                    let runtimes =
                        super::selector::runtimes(rows.iter().map(|row| row.runtime.as_str()));
                    let Some(selected) = super::selector::preselect(&runtimes, "agit resume")?
                    else {
                        return Ok(crate::ExitCode::Ok);
                    };
                    scope.runtime = selected.runtime;
                    naming_focus = None;
                    continue;
                }
                super::naming::Outcome::Adopt(choice) => {
                    naming_focus = None;
                    let _ = super::naming::execute_import(guard, &choice)?;
                    rows = collect_all(cwd);
                    if rows.is_empty() {
                        return Ok(crate::ExitCode::Ok);
                    }
                    continue;
                }
            }
        }
        match run_loop(&rows, &mut scope)? {
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
    // The rescan applies the same bounded discovery probes as initial entry.
    let rows = collect_all(cwd);
    widgets::refresh_rc_status();
    guard.resume()?;
    Ok(rows)
}

fn run_loop(rows: &[Row], scope: &mut super::selector::Scope) -> crate::Result<Outcome> {
    let runtimes = super::selector::runtimes(rows.iter().map(|row| row.runtime.as_str()));
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    term.clear()?;
    let mut state = ListState::default();
    state.select(Some(0));
    let mut filter = Filter::default();
    let mut notice: Option<String> = None;

    loop {
        let view: Vec<&Row> = rows
            .iter()
            .filter(|r| scope.includes(&r.runtime, r.here) && filter.matches(&r.haystack()))
            .collect();
        if state.selected().unwrap_or(0) >= view.len() {
            state.select(if view.is_empty() {
                None
            } else {
                Some(view.len() - 1)
            });
        }
        let mut page_area = Rect::default();
        term.draw(|f| {
            page_area = draw(f, &view, &mut state, &filter, notice.as_deref(), scope);
        })?;

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
            KeyCode::Char('a') => {
                scope.all_projects = !scope.all_projects;
                state.select(Some(0));
            }
            KeyCode::Tab => {
                scope.cycle_runtime(&runtimes);
                state.select(Some(0));
            }
            KeyCode::PageDown | KeyCode::PageUp => {
                let heights = view
                    .iter()
                    .map(|row| row_lines(row, page_area).len())
                    .collect::<Vec<_>>();
                state.select(widgets::page_selection(
                    state.selected(),
                    &heights,
                    page_area.height.saturating_sub(2) as usize,
                    key.code == KeyCode::PageDown,
                ));
            }
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
    scope: &super::selector::Scope,
) -> Rect {
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

    let items: Vec<ListItem> = view
        .iter()
        .map(|row| ListItem::new(row_lines(row, list_area)))
        .collect();
    let title = match filter.hint() {
        Some(q) => format!("sessions · {}  {q}", scope.label()),
        None => format!("sessions ({}) · {}", view.len(), scope.label()),
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
            "enter continue   a projects   tab runtime   pgup/pgdn page   / filter   q quit"
        },
    );
    list_area
}

fn row_lines(row: &Row, area: Rect) -> Vec<Line<'static>> {
    let width = area.width.saturating_sub(4) as usize;
    let mut lines = vec![row_line(row, width), project_line(row, width)];
    // A list item must fit the inner viewport to keep its selected identity visible.
    lines.truncate(area.height.saturating_sub(2) as usize);
    lines
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

fn project_line(row: &Row, width: usize) -> Line<'static> {
    widgets::clamp_line(
        Line::from(Span::styled(
            format!(
                "  {} · {} · {}",
                row.runtime,
                super::selector::project_label(row.cwd.as_deref()),
                crate::ui::truncate(row.gist.as_deref().unwrap_or("preview unavailable"), 60)
            ),
            theme::muted(),
        )),
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
    out.push_str(&format!(
        "project  {}\n",
        super::selector::project_label(r.cwd.as_deref())
    ));
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
            cwd: Some("/w".into()),
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
            all_projects: false,
            cwd: "/w".into(),
            owner: Some("nana".into()),
            links: vec![
                link("A", "/w", Some("payments"), Some("refund-fix")),
                link("B", "/w", None, None), // link-only: not named yet
            ],
            seen: vec![seen("A", 500), seen("B", 100)],
            same_repo: vec![SameRepo {
                runtime: "claude-code".into(),
                cwd: Some("/other".into()),
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

    #[test]
    fn superseded_transcripts_do_not_duplicate_the_active_branch_or_reenter_naming() {
        let mut historical = link("old", "/w", Some("photo"), Some("work"));
        historical.link.superseded_by = Some("claude-code/current".into());
        let input = Input {
            all_projects: false,
            cwd: "/w".into(),
            owner: Some("alice".into()),
            links: vec![
                historical,
                link("current", "/w", Some("photo"), Some("work")),
            ],
            seen: vec![seen("old", 1000), seen("current", 0)],
            same_repo: vec![SameRepo {
                runtime: "claude-code".into(),
                cwd: Some("/other".into()),
                slug: "alice/photo".into(),
                branch: "work".into(),
                last_active: t(0),
                last_seen: None,
            }],
        };
        let rows = assemble(&input, t(1000));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id.as_deref(), Some("current"));
        assert_eq!(rows[0].badge, Badge::Here);
        assert!(!rows[0].live);
    }

    /// On a tie the adopted session comes before the unnamed one; the owner recorded on the
    /// link wins over the login name.
    #[test]
    fn ties_prefer_adopted_and_the_links_owner_wins() {
        let mut l = link("A", "/w", Some("agent-git"), Some("run-1"));
        l.link.owner = Some("acme".into());
        let input = Input {
            all_projects: false,
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
            all_projects: false,
            cwd: "/w".into(),
            links: vec![link("A", "/elsewhere", Some("x"), Some("b"))],
            ..Default::default()
        };
        assert!(assemble(&input, t(1)).is_empty());
    }

    #[test]
    fn project_scope_keeps_recorded_paths_and_runtime_filters_intersect_all_projects() {
        let input = Input {
            cwd: "/w".into(),
            all_projects: true,
            links: vec![
                link("local", "/w", Some("qa"), Some("local")),
                link("remote", "/another", Some("qa"), Some("remote")),
            ],
            seen: vec![
                seen("local", 100),
                Seen {
                    cwd: Some("/another".into()),
                    ..seen("remote", 200)
                },
                Seen {
                    cwd: None,
                    runtime: "codex".into(),
                    ..seen("unknown", 300)
                },
            ],
            ..Default::default()
        };
        let rows = assemble(&input, t(1000));
        let mut scope = super::super::selector::Scope::default();
        let selected = |scope: &super::super::selector::Scope| {
            rows.iter()
                .filter(|row| scope.includes(&row.runtime, row.here))
                .map(|row| row.session_id.as_deref().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(selected(&scope), ["local"]);
        scope.all_projects = true;
        assert_eq!(selected(&scope), ["unknown", "remote", "local"]);
        scope.runtime = Some("claude-code".into());
        assert_eq!(selected(&scope), ["remote", "local"]);
        assert_eq!(rows[1].badge, Badge::Elsewhere);
        assert_eq!(rows[1].cwd.as_deref(), Some("/another"));
        assert!(rows[0].cwd.is_none());
        assert!(detail_text(Some(&rows[0]), None).contains("project  unknown"));
    }

    #[test]
    fn all_project_inventory_does_not_hide_an_existing_same_repo_link_in_current_scope() {
        let mut adopted = link("other", "/other", Some("qa"), Some("work"));
        adopted.link.owner = Some("nana".into());
        let input = Input {
            cwd: "/w".into(),
            all_projects: true,
            owner: Some("different-account".into()),
            links: vec![adopted],
            seen: vec![Seen {
                cwd: Some("/other".into()),
                ..seen("other", 100)
            }],
            same_repo: vec![SameRepo {
                slug: "nana/qa".into(),
                branch: "work".into(),
                runtime: "claude-code".into(),
                cwd: Some("/other".into()),
                last_active: t(200),
                last_seen: Some(t(950)),
            }],
        };
        let rows = assemble(&input, t(1000));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].badge, Badge::SameRepo);
        assert!(rows[0].here);
        assert!(rows[0].live);
        assert_eq!(rows[0].cwd.as_deref(), Some("/other"));
        assert_eq!(rows[0].session_id.as_deref(), Some("other"));
    }

    #[test]
    fn another_owners_same_named_branch_stays_separate_from_current_project_candidates() {
        for account in [None, Some("other")] {
            let mut adopted = link("external", "/other", Some("qa"), Some("work"));
            adopted.link.owner = Some("other".into());
            let input = Input {
                cwd: "/w".into(),
                all_projects: true,
                owner: account.map(str::to_owned),
                links: vec![adopted],
                seen: vec![Seen {
                    cwd: Some("/other".into()),
                    ..seen("external", 100)
                }],
                same_repo: vec![SameRepo {
                    slug: "mine/qa".into(),
                    branch: "work".into(),
                    runtime: "claude-code".into(),
                    cwd: Some("/current-repo-checkout".into()),
                    last_active: t(200),
                    last_seen: None,
                }],
            };
            let rows = assemble(&input, t(1000));
            assert_eq!(rows.len(), 2);
            let mut scope = super::super::selector::Scope::default();
            let current: Vec<_> = rows
                .iter()
                .filter(|row| scope.includes(&row.runtime, row.here))
                .collect();
            assert_eq!(current.len(), 1);
            assert_eq!(current[0].badge, Badge::SameRepo);
            assert_eq!(current[0].cwd.as_deref(), Some("/current-repo-checkout"));
            assert_eq!(
                choose(current[0]).unwrap(),
                Outcome::Resume {
                    slug: "mine/qa".into(),
                    branch: "work".into(),
                }
            );
            scope.all_projects = true;
            let all: Vec<_> = rows
                .iter()
                .filter(|row| scope.includes(&row.runtime, row.here))
                .collect();
            assert_eq!(all.len(), 2);
            let external = all
                .iter()
                .find(|row| row.session_id.as_deref() == Some("external"))
                .unwrap();
            assert_eq!(external.badge, Badge::Elsewhere);
            assert!(!external.here);
            assert_eq!(external.cwd.as_deref(), Some("/other"));
            assert_eq!(
                choose(external).unwrap(),
                Outcome::Resume {
                    slug: "other/qa".into(),
                    branch: "work".into(),
                }
            );
        }
    }

    /// An adopted session does not show up a second time as "waiting to be named".
    #[test]
    fn an_adopted_session_is_not_also_offered_for_naming() {
        let input = Input {
            all_projects: false,
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
            all_projects: false,
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
            all_projects: false,
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
            all_projects: false,
            cwd: "/w".into(),
            owner: Some("nana".into()),
            links: vec![link("A", "/w", Some("payments"), Some("refund-fix"))],
            seen: vec![seen("A", 10)],
            same_repo: vec![SameRepo {
                runtime: "claude-code".into(),
                cwd: Some("/other".into()),
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

    /// An ownerless display row cannot hide a qualified branch discovered through its code anchor.
    #[test]
    fn an_unknown_owner_does_not_suppress_a_qualified_same_repo_candidate() {
        let input = Input {
            all_projects: false,
            cwd: "/w".into(),
            owner: None,
            links: vec![link("A", "/w", Some("payments"), Some("refund-fix"))],
            seen: vec![seen("A", 10)],
            same_repo: vec![SameRepo {
                runtime: "claude-code".into(),
                cwd: Some("/other".into()),
                slug: "nana/payments".into(),
                branch: "refund-fix".into(),
                last_active: t(10),
                last_seen: None,
            }],
        };
        let rows = assemble(&input, t(1000));
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| { row.badge == Badge::Here && row.slug.as_deref() == Some("payments") })
        );
        let qualified = rows
            .iter()
            .find(|row| row.badge == Badge::SameRepo)
            .unwrap();
        assert_eq!(
            choose(qualified).unwrap(),
            Outcome::Resume {
                slug: "nana/payments".into(),
                branch: "refund-fix".into(),
            }
        );
    }

    #[test]
    fn branch_activity_uses_recorded_owners_and_keeps_unresolved_claims_conservative() {
        let now = t(1000);
        for (owner, agent, branch, superseded, expected) in [
            (Some("other"), "qa", "work", false, None),
            (Some("mine"), "qa", "work", false, Some(now)),
            (None, "qa", "work", false, Some(now)),
            (Some("mine"), "different", "work", false, None),
            (Some("mine"), "qa", "different", false, None),
            (Some("mine"), "qa", "work", true, None),
        ] {
            let mut adopted = link("unresolved", "/other", Some(agent), Some(branch));
            adopted.link.source = "unavailable-fixture-runtime".into();
            adopted.link.owner = owner.map(str::to_owned);
            adopted.link.superseded_by = superseded.then(|| "replacement".into());
            assert_eq!(
                branch_last_seen(&[adopted], "mine/qa", "work", now),
                expected,
                "owner={owner:?}, agent={agent}, branch={branch}, superseded={superseded}"
            );
        }
        assert_eq!(branch_last_seen(&[], "unqualified", "work", now), Some(now));
    }

    /// A session missing from the index must not sink, and must not be declared takeable.
    ///
    /// Both failure directions match `is_live` — calling it "live" wrongly only blocks one
    /// takeover, calling it "dead" wrongly interleaves two writers' appends into one transcript,
    /// which is data corruption.
    #[test]
    fn a_session_missing_from_the_index_is_neither_sunk_nor_declared_dead() {
        let input = Input {
            all_projects: false,
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
            all_projects: false,
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
                runtime: "claude-code".into(),
                cwd: Some("/other".into()),
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

    #[cfg(unix)]
    #[test]
    fn empty_shell_probe_keeps_special_and_unselected_sources_without_reading_them() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("empty.jsonl");
        std::fs::write(&source, "{}\n").unwrap();
        assert!(!worth_naming("claude-code", &source, None));
        let alias = directory.path().join("alias.jsonl");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        assert!(worth_naming("claude-code", &alias, None));
        let fifo = directory.path().join("fifo.jsonl");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(worth_naming("claude-code", &fifo, None));
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "{}\n");
        assert!(std::fs::symlink_metadata(&alias).unwrap().is_symlink());
    }

    #[test]
    fn committed_activity_keeps_exact_branch_names_with_tags_and_packed_refs() {
        let directory = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(directory.path()).unwrap();
        repo.git(&["config", "user.name", "Candidate fixture"])
            .unwrap();
        repo.git(&["config", "user.email", "candidate@example.test"])
            .unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "candidate fixture"])
            .unwrap();
        repo.git(&["branch", "collision"]).unwrap();
        repo.git(&["tag", "collision"]).unwrap();
        repo.git(&["pack-refs", "--all"]).unwrap();
        let times = committed_at(&repo);
        assert!(times.contains_key("collision"));
        assert!(!times.contains_key("heads/collision"));
        let seconds = repo
            .git(&["show", "-s", "--format=%ct", "refs/heads/collision"])
            .unwrap();
        assert_eq!(
            times["collision"],
            SystemTime::UNIX_EPOCH + Duration::from_secs(seconds.trim().parse().unwrap())
        );
    }

    #[test]
    fn committed_activity_keeps_branches_with_unrepresentable_git_dates() {
        use crate::commands::plumbing::raw_git;

        let directory = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(directory.path()).unwrap();
        let tree = raw_git(&repo, &["mktree"], Some("")).unwrap();
        let ordinary_seconds = 1_700_000_000;
        let extreme_seconds = i64::MAX as u64;
        for (branch, seconds) in [("ordinary", ordinary_seconds), ("extreme", extreme_seconds)] {
            let commit = format!(
                "tree {}\nauthor Candidate fixture <candidate@example.test> {seconds} +0000\ncommitter Candidate fixture <candidate@example.test> {seconds} +0000\n\nSynthetic activity\n",
                tree.trim()
            );
            let oid = raw_git(
                &repo,
                &["hash-object", "-t", "commit", "-w", "--stdin"],
                Some(&commit),
            )
            .unwrap();
            repo.git(&["update-ref", &format!("refs/heads/{branch}"), oid.trim()])
                .unwrap();
        }
        for packed in [false, true] {
            if packed {
                repo.git(&["pack-refs", "--all", "--prune"]).unwrap();
                assert!(!repo.git_path("refs/heads/extreme").unwrap().exists());
            }
            let records = repo
                .git(&[
                    "for-each-ref",
                    "--format=%(refname)%09%(committerdate:unix)",
                    "refs/heads/",
                ])
                .unwrap();
            assert!(
                records
                    .lines()
                    .any(|line| line == "refs/heads/ordinary\t1700000000")
            );
            assert!(
                records
                    .lines()
                    .any(|line| line == "refs/heads/extreme\t9223372036854775807")
            );
            let times = committed_at(&repo);
            assert_eq!(times["ordinary"], t(ordinary_seconds));
            let representable =
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(extreme_seconds));
            match representable {
                Some(expected) => assert_eq!(times.get("extreme"), Some(&expected)),
                None => assert!(!times.contains_key("extreme")),
            }
            let branches = repo.branches();
            assert!(branches.iter().any(|branch| branch == "ordinary"));
            assert!(branches.iter().any(|branch| branch == "extreme"));
        }
    }

    #[test]
    fn a_row_matches_on_repo_branch_runtime_and_gist() {
        let input = Input {
            all_projects: false,
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

    #[test]
    fn short_session_panes_keep_the_selected_identity_and_page_by_visible_rows() {
        use ratatui::backend::TestBackend;

        let input = Input {
            cwd: "/work".into(),
            owner: Some("nana".into()),
            links: ["FIRST", "MIDDLE", "LAST"]
                .into_iter()
                .map(|name| link(name, "/work", Some("payments"), Some(name)))
                .collect(),
            seen: ["FIRST", "MIDDLE", "LAST"]
                .into_iter()
                .map(|name| Seen {
                    cwd: Some("/work".into()),
                    ..seen(name, 0)
                })
                .collect(),
            ..Default::default()
        };
        let rows = assemble(&input, t(1000));
        let view = rows.iter().collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(2));
        for (width, height, notice, inner_height, previous_page) in [
            (79, 5, None, 1, 1),
            (79, 6, Some("choose another session"), 1, 1),
            (200, 5, Some("choose another session"), 1, 1),
            (79, 6, None, 2, 1),
            (79, 8, None, 4, 0),
            (79, 5, None, 1, 1),
        ] {
            let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut area = Rect::default();
            term.draw(|frame| {
                area = draw(
                    frame,
                    &view,
                    &mut state,
                    &Filter::default(),
                    notice,
                    &super::super::selector::Scope::default(),
                );
            })
            .unwrap();
            let inner = widgets::pane("").inner(area);
            assert_eq!(inner.height, inner_height);
            let buffer = term.backend().buffer();
            let text = (inner.y..inner.bottom())
                .map(|y| {
                    (inner.x..inner.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            for required in ["nana/payments @ LAST", "▸ "] {
                assert!(text.contains(required), "missing {required:?}: {text}");
            }
            assert_eq!(state.selected(), Some(2));
            assert_eq!(
                choose(view[2]),
                Ok(Outcome::Resume {
                    slug: "nana/payments".into(),
                    branch: "LAST".into(),
                })
            );
            let heights = view
                .iter()
                .map(|row| row_lines(row, area).len())
                .collect::<Vec<_>>();
            assert!(
                heights
                    .iter()
                    .all(|height| *height <= inner.height as usize)
            );
            assert_eq!(
                widgets::page_selection(Some(2), &heights, inner.height as usize, false),
                Some(previous_page)
            );
            if inner.height >= 2 {
                assert!(text.contains("/work"), "{text}");
            }
        }
    }
}
