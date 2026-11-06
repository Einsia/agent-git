//! A fresh Codex session holds the branch an agit launch made for it, and nothing else does.
//!
//! An agit launch that starts a fresh Codex session writes a claim for it before launching: the
//! branch, the folder and the time, under a single-use id the launch passes along as
//! `AGIT_LAUNCH`. The new session's start takes that claim, once, if it starts in the same folder
//! soon after. A session that sees agit's launch variables without a claim it can take was not
//! started by that launch, so agit records nothing for it and tells the person once.
//!
//! A link made before this, from the launch variables alone, is unconfirmed when another session
//! holds the same branch: such a link may belong to another conversation, so the branch it names
//! is held back from every upload, and `agit status` says so.

use super::{Link, list};
use crate::Result;
use crate::domain::store::Store;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The environment variable an agit launch passes its claim id in.
pub const ENV: &str = "AGIT_LAUNCH";
/// How long after a launch its session may take the claim.
pub const WINDOW_MS: u64 = 2 * 60 * 1000;
/// What a link that took a launch's claim records as `claimed_by`.
pub const CLAIMED_BY: &str = "launch";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    slug: String,
    branch: String,
    cwd: String,
    created_ms: u64,
    /// How the session the launch starts begins, as its start hook says: `startup` for a fresh
    /// one, `resume` for one the launch resumes. A session that begins otherwise can't take it.
    start: String,
    /// For a resume, the conversation the launch resumes. Codex may continue it as a new
    /// conversation that names it as its origin, and report that one's start either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Kept beside the store, not in it: the store's folders are runtimes, and their files links.
fn beside(store: &Store) -> PathBuf {
    store
        .root()
        .parent()
        .map_or_else(|| store.root().join(".."), Path::to_path_buf)
}

fn launches(store: &Store, runtime: &str) -> PathBuf {
    beside(store).join("launch-claims").join(runtime)
}

fn same_folder(a: &str, b: &Path) -> bool {
    let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    canonical(Path::new(a)) == canonical(b)
}

/// How the session a launch starts begins.
pub const STARTUP: &str = "startup";
pub const RESUME: &str = "resume";

/// Writes the claim a launch of `runtime` in `cwd` makes for `slug`@`branch`, for the session it
/// starts the way `start` says; its id, for `AGIT_LAUNCH`.
pub fn mint(
    store: &Store,
    runtime: &str,
    slug: &str,
    branch: &str,
    cwd: &Path,
    start: &str,
    thread: Option<&str>,
) -> Result<String> {
    let dir = launches(store, runtime);
    crate::infra::config::create_state_dir(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let id = uuid::Uuid::new_v4().to_string();
    let claim = Claim {
        slug: slug.into(),
        branch: branch.into(),
        cwd: cwd.to_string_lossy().into_owned(),
        created_ms: now_ms(),
        start: start.into(),
        thread: thread.map(str::to_owned),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
    use std::io::Write as _;
    serde_json::to_writer(&mut temporary, &claim)?;
    temporary.flush()?;
    temporary
        .into_temp_path()
        .persist(dir.join(format!("{id}.json")))?;
    Ok(id)
}

fn open_claim(store: &Store, runtime: &str, id: &str) -> Option<(PathBuf, Claim)> {
    uuid::Uuid::parse_str(id).ok()?;
    let path = launches(store, runtime).join(format!("{id}.json"));
    let claim = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    Some((path, claim))
}

/// The session that would take a launch's claim: the branch it would hold, where it began and how.
#[derive(Clone, Copy)]
pub struct Taker<'a> {
    pub slug: &'a str,
    pub branch: &'a str,
    pub origin: &'a Path,
    pub start: &'a str,
    /// The session itself, and the conversation it continues, if it names one.
    pub session_id: &'a str,
    pub continues: Option<&'a str>,
}

fn fits(claim: &Claim, taker: Taker<'_>) -> bool {
    // A claim that names the conversation it resumes is for that one, or one continuing it,
    // however its start is told; any other names how its session begins.
    let theirs = match claim.thread.as_deref() {
        Some(thread) => thread == taker.session_id || taker.continues == Some(thread),
        None => claim.start == taker.start,
    };
    theirs
        && claim.slug == taker.slug
        && claim.branch == taker.branch
        && same_folder(&claim.cwd, taker.origin)
        && now_ms().saturating_sub(claim.created_ms) <= WINDOW_MS
}

/// Whether launch `id` made a claim `taker` may still take.
pub fn open(store: &Store, runtime: &str, id: &str, taker: Taker<'_>) -> bool {
    open_claim(store, runtime, id).is_some_and(|(_, claim)| fits(&claim, taker))
}

/// Takes launch `id`'s claim for session `session_id`, once: only the first session to take it
/// gets it.
pub fn take(store: &Store, runtime: &str, id: &str, taker: Taker<'_>) -> bool {
    let Some((path, claim)) = open_claim(store, runtime, id) else {
        return false;
    };
    if !fits(&claim, taker) {
        return false;
    }
    let taken = path.with_file_name(format!("{id}.taken-{}", taker.session_id));
    std::fs::rename(&path, &taken).is_ok()
}

/// Retires launch `id`'s claim once the session that launch started is linked to `slug`@`branch`
/// by any path, so nothing else can take it for the rest of its window.
pub fn retire(store: &Store, runtime: &str, id: &str, slug: &str, branch: &str, session_id: &str) {
    if let Some((path, claim)) = open_claim(store, runtime, id)
        && claim.slug == slug
        && claim.branch == branch
    {
        let _ = std::fs::rename(
            &path,
            path.with_file_name(format!("{id}.taken-{session_id}")),
        );
    }
}

/// What a person is told, when the conversation starts, about a Codex conversation agit isn't
/// saving.
pub const NOTICE: &str = "agit isn't saving this Codex conversation: it started while Codex \
    was still carrying an earlier agit launch. Restarting Codex fixes it for new conversations; \
    `agit import` can link this one.";

/// Whether `link` holds its branch only on the word of launch variables, from before launches
/// made their claims: nothing else confirms it, and another session holds the same branch.
pub fn unconfirmed(link: &Link, all: &[Link]) -> bool {
    let (Some(agent), Some(branch)) = (&link.agent, &link.branch) else {
        return false;
    };
    bare(link)
        && all.iter().any(|other| {
            (other.source != link.source || other.session_id != link.session_id)
                && other.agent.as_ref() == Some(agent)
                && other.owner == link.owner
                && other.branch.as_ref() == Some(branch)
        })
}

/// Whether a link carries nothing but the launch variables' word for its branch.
fn bare(link: &Link) -> bool {
    link.source == "codex"
        && link.claimed_by.is_none()
        && link.materialized_from.is_none()
        && link.baseline_bytes.is_none()
        && link.native_checkpoint.is_none()
        && link.session_privacy.is_none()
        && link.is_active()
}

/// How a person confirms a link an older agit made: it then holds its branch on their word.
pub const CONFIRMED: &str = "confirmed";

/// Whether `link` is a Codex link an agit before launches made their claims bound to its branch
/// from the launch variables alone, at the session's start, whatever turns it took since.
///
/// Every other way a Codex link comes to name a branch leaves its own mark, and that claim alone
/// leaves none: a launch's claim since then (`claimed_by`), agit's own resume, which writes the
/// link before launching (`materialized_from`; resuming a thread agit already holds writes no
/// link), an import, including a project's capture through `agit on` and `agit save`, which
/// imports (`session_privacy`), a desktop session (`native_binding`) and a merge's archive role
/// (`merge_archive`). What a turn's settlement adds to any link doesn't tell them apart.
pub fn older(link: &Link) -> bool {
    link.source == "codex"
        && link.agent.is_some()
        && link.branch.is_some()
        && link.claimed_by.is_none()
        && link.materialized_from.is_none()
        && link.session_privacy.is_none()
        && link.native_binding.is_none()
        && link.merge_archive.is_none()
        && link.is_active()
}

/// Whether rewriting a link as `after` could end what makes its branch, or the branch of a link
/// beside it, held: the record has to be there before that is gone from the store.
pub(super) fn unsettles(before: &Link, after: &Link) -> bool {
    bare(before) != bare(after)
        || older(before) != older(after)
        || before.owner != after.owner
        || before.agent != after.agent
        || before.branch != after.branch
}

/// A branch held back from every upload because a link that may be another conversation's names
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    pub slug: String,
    pub branch: String,
    /// The sessions whose unconfirmed links named it, `<runtime>/<id>`.
    pub sessions: Vec<String>,
    /// The sessions an older agit linked to it that the person hasn't confirmed yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub older: Vec<String>,
    /// The person let it upload again, as it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
}

impl Hold {
    fn new(slug: &str, branch: &str) -> Self {
        Self {
            slug: slug.into(),
            branch: branch.into(),
            sessions: Vec::new(),
            older: Vec::new(),
            released: false,
        }
    }

    /// Whether it still keeps its branch from uploading.
    fn holds(&self) -> bool {
        !self.released && (!self.sessions.is_empty() || !self.older.is_empty())
    }
}

fn holds(store: &Store) -> PathBuf {
    beside(store).join("upload-holds")
}

fn hold_path(store: &Store, slug: &str, branch: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(format!("{slug}@{branch}").as_bytes()));
    holds(store).join(format!("{}.json", &digest[..32]))
}

/// Holds the branch's hold record for one read, change and write: two of them at once, a
/// confirmation in each of two terminals, would each write back what the other removed.
fn lock_hold(store: &Store, slug: &str, branch: &str) -> Result<std::fs::File> {
    crate::infra::config::create_state_dir(&holds(store))?;
    let path = hold_path(store, slug, branch).with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    crate::infra::local_state::lock_exclusive(&file, &path, "a branch's upload hold")?;
    Ok(file)
}

#[cfg(test)]
thread_local! {
    /// How many links recording holds read again under a branch's lock.
    static REREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn write_hold(store: &Store, hold: &Hold) -> Result<()> {
    crate::infra::config::create_state_dir(&holds(store))?;
    let mut temporary = tempfile::NamedTempFile::new_in(holds(store))?;
    use std::io::Write as _;
    serde_json::to_writer(&mut temporary, hold)?;
    temporary.flush()?;
    temporary
        .into_temp_path()
        .persist(hold_path(store, &hold.slug, &hold.branch))?;
    Ok(())
}

fn slug_of(link: &Link) -> Option<(String, &String)> {
    let (Some(agent), Some(branch)) = (&link.agent, &link.branch) else {
        return None;
    };
    let slug = match &link.owner {
        Some(owner) => format!("{owner}/{agent}"),
        None => agent.clone(),
    };
    Some((slug, branch))
}

/// The holds the store's links call for, by branch: unconfirmed links, and links an older agit
/// made.
fn called_for(store: &Store) -> Vec<Hold> {
    called_by(&list(store))
}

/// [`called_for`] from the links `all`.
fn called_by(all: &[Link]) -> Vec<Hold> {
    let mut found: Vec<Hold> = Vec::new();
    for link in all {
        let unconfirmed = unconfirmed(link, all);
        if !unconfirmed && !older(link) {
            continue;
        }
        let Some((slug, branch)) = slug_of(link) else {
            continue;
        };
        let session = format!("{}/{}", link.source, link.session_id);
        let index = match found
            .iter()
            .position(|hold| hold.slug == slug && &hold.branch == branch)
        {
            Some(index) => index,
            None => {
                found.push(Hold::new(&slug, branch));
                found.len() - 1
            }
        };
        let hold = &mut found[index];
        if unconfirmed {
            hold.sessions.push(session);
        } else {
            hold.older.push(session);
        }
    }
    found
}

/// Finds the store's links that may be another conversation's and records a hold on each branch
/// they name. A hold, once recorded, stays, whatever becomes of the link.
pub fn record_holds(store: &Store) -> Result<()> {
    let all = list(store);
    for listed in called_by(&all) {
        let _lock = lock_hold(store, &listed.slug, &listed.branch)?;
        // The links naming this branch are read again under the lock: a confirmation that took
        // it first may have answered for one of them since. Whether a link is held depends on the
        // links of its own branch alone.
        let named = all
            .iter()
            .filter(|link| {
                slug_of(link)
                    .is_some_and(|(slug, branch)| slug == listed.slug && *branch == listed.branch)
            })
            .filter_map(|link| {
                #[cfg(test)]
                REREAD.with(|count| count.set(count.get() + 1));
                super::get(store, &link.source, &link.session_id)
            })
            .collect::<Vec<_>>();
        let Some(called) = called_by(&named)
            .into_iter()
            .find(|hold| hold.slug == listed.slug && hold.branch == listed.branch)
        else {
            continue;
        };
        let path = hold_path(store, &called.slug, &called.branch);
        let mut hold = held_at(&path).unwrap_or_else(|| Hold::new(&called.slug, &called.branch));
        if hold.released {
            continue;
        }
        let before = hold.clone();
        for session in called.sessions {
            if !hold.sessions.contains(&session) {
                hold.sessions.push(session);
            }
        }
        for session in called.older {
            if !hold.older.contains(&session) {
                hold.older.push(session);
            }
        }
        if hold != before {
            write_hold(store, &hold)?;
        }
    }
    Ok(())
}

fn held_at(path: &Path) -> Option<Hold> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The hold on `slug`@`branch`, after recording any the store's links call for. A hold that can't
/// be read counts as one, and so does one the links call for that couldn't be recorded. Only the
/// person lifts it.
pub fn held(store: &Store, slug: &str, branch: &str) -> Option<Hold> {
    let _ = record_holds(store);
    let path = hold_path(store, slug, branch);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Hold>(&bytes).map_or_else(
            |_| Some(Hold::new(slug, branch)),
            |hold| hold.holds().then_some(hold),
        ),
        // Not recorded, whether or not recording failed: what the links call for still holds.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => all_holds(store)
            .into_iter()
            .find(|hold| hold.slug == slug && hold.branch == branch),
        Err(_) => Some(Hold::new(slug, branch)),
    }
}

/// Lifts the hold on `slug`@`branch`, for good: the links that called for it don't hold it again.
pub fn release(store: &Store, slug: &str, branch: &str) -> Result<()> {
    let _lock = lock_hold(store, slug, branch)?;
    let path = hold_path(store, slug, branch);
    let mut hold = held_at(&path).unwrap_or_else(|| Hold::new(slug, branch));
    hold.released = true;
    write_hold(store, &hold)
}

/// The links an older agit made to `slug`@`branch` that the person hasn't confirmed yet.
pub fn unconfirmed_older(store: &Store, slug: &str, branch: &str) -> Vec<Link> {
    list(store)
        .into_iter()
        .filter(|link| {
            older(link)
                && slug_of(link).is_some_and(|(owner, named)| owner == slug && named == branch)
        })
        .collect()
}

/// Confirms that the older agit's links of `sessions` belong to `slug`@`branch`: each then holds
/// it on the person's word, and the hold on that branch no longer counts them. Nothing else
/// changes, on this branch or any other.
pub fn confirm(store: &Store, slug: &str, branch: &str, sessions: &[String]) -> Result<()> {
    record_holds(store)?;
    let listed = unconfirmed_older(store, slug, branch)
        .into_iter()
        .filter(|link| sessions.contains(&link.session_id))
        .collect::<Vec<_>>();
    confirm_listed(store, slug, branch, &listed)
}

/// [`confirm`] for the links `listed` as they were read before their locks were taken.
fn confirm_listed(store: &Store, slug: &str, branch: &str, listed: &[Link]) -> Result<()> {
    for listed in listed {
        let _guard = super::lock(store, &listed.source, &listed.session_id)?;
        // Read again under the lock: an import or a settlement may have changed the link since it
        // was listed, and only a link that still is the older agit's, on this branch, is confirmed.
        let Some(link) = super::get(store, &listed.source, &listed.session_id).filter(|link| {
            older(link)
                && slug_of(link).is_some_and(|(owner, named)| owner == slug && named == branch)
        }) else {
            continue;
        };
        let mut confirmed = link.clone();
        confirmed.claimed_by = Some(CONFIRMED.into());
        super::write(store, &confirmed)?;
        // Before it settled a turn the link may also be on the list of unconfirmed links beside
        // another; confirmed, it is on neither.
        let _lock = lock_hold(store, slug, branch)?;
        let path = hold_path(store, slug, branch);
        if let Some(mut hold) = held_at(&path) {
            let session = format!("{}/{}", link.source, link.session_id);
            hold.older.retain(|named| *named != session);
            hold.sessions.retain(|named| *named != session);
            write_hold(store, &hold)?;
        }
    }
    Ok(())
}

/// Every hold on this store's branches: those recorded, and those its links call for that aren't
/// recorded yet. Writes nothing, so `agit status` can show them.
pub fn all_holds(store: &Store) -> Vec<Hold> {
    let mut found: Vec<Hold> = std::fs::read_dir(holds(store))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| held_at(&entry.path()))
        .collect();
    for called in called_for(store) {
        if !found
            .iter()
            .any(|hold| hold.slug == called.slug && hold.branch == called.branch)
        {
            found.push(called);
        }
    }
    found.retain(Hold::holds);
    found.sort_by(|a, b| (&a.slug, &a.branch).cmp(&(&b.slug, &b.branch)));
    found
}

/// The fixed phrase every refusal of a held branch carries.
pub const REFUSAL: &str = "may hold another conversation's turns";

/// Why a held branch doesn't upload, and the way on, for the person. Never offered to an agent:
/// lifting the hold uploads the branch's turns as they are.
pub fn explain(hold: &Hold) -> String {
    if hold.sessions.is_empty() && !hold.older.is_empty() {
        return explain_older(hold);
    }
    format!(
        "{slug}@{branch} {REFUSAL}, so it stays on this computer. To go on, fork it at your own \
         last turn: `agit fork {slug}@{branch}#<n> -b <new-name>`. Once the other turns are moved \
         out, `agit branch unhold {branch} --repo {slug}` lets it upload again, with its turns as they are.",
        slug = hold.slug,
        branch = hold.branch,
    )
}

/// Why a branch an older agit linked a Codex conversation to doesn't upload, and the two ways on:
/// confirming the conversation belongs there, which an agent acting for the person may run, or
/// forking.
fn explain_older(hold: &Hold) -> String {
    let confirm = if hold.older.len() > 1 {
        format!(
            "confirm each that belongs there: `agit branch confirm {branch} --repo {slug} --session <id>`",
            slug = hold.slug,
            branch = hold.branch,
        )
    } else {
        format!(
            "confirm it belongs there: `agit branch confirm {branch} --repo {slug}`",
            slug = hold.slug,
            branch = hold.branch,
        )
    };
    format!(
        "{slug}@{branch} was linked to a Codex conversation by an older agit and {REFUSAL}, so it \
         stays on this computer. To go on, {confirm}, or fork it at your own last turn: \
         `agit fork {slug}@{branch}#<n> -b <new-name>`.",
        slug = hold.slug,
        branch = hold.branch,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two conversations on one branch confirmed at once, from two terminals, both come off its
    /// hold: each confirmation changes the hold record only while it holds the branch's lock, so
    /// neither writes back what the other removed.
    #[test]
    fn confirmations_of_two_conversations_at_once_both_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = Store::at(&root);
        std::fs::create_dir_all(store.root()).unwrap();
        for id in ["a", "b"] {
            let mut link = Link::new("codex", id, None);
            link.owner = Some("me".into());
            link.agent = Some("qa".into());
            link.branch = Some("work".into());
            super::super::write(&store, &link).unwrap();
        }
        record_holds(&store).unwrap();
        assert!(held(&store, "me/qa", "work").is_some());
        // While another change holds the record, a confirmation waits for it.
        let lock = lock_hold(&store, "me/qa", "work").unwrap();
        let confirming = |id: &'static str| {
            let root = root.clone();
            std::thread::spawn(move || confirm(&Store::at(root), "me/qa", "work", &[id.into()]))
        };
        let (a, b) = (confirming("a"), confirming("b"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!a.is_finished() && !b.is_finished());
        drop(lock);
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
        assert_eq!(held(&store, "me/qa", "work"), None);
    }

    /// Recording holds reads again, under each held branch's lock, only the links that name that
    /// branch: a store of many links across many branches isn't read once per branch.
    #[test]
    fn recording_holds_reads_again_only_the_held_branchs_links() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        std::fs::create_dir_all(store.root()).unwrap();
        let mut write = |id: &str, branch: &str, claimed: bool| {
            let mut link = Link::new("codex", id, None);
            link.owner = Some("me".into());
            link.agent = Some("qa".into());
            link.branch = Some(branch.into());
            link.claimed_by = claimed.then(|| CLAIMED_BY.into());
            super::super::write(&store, &link).unwrap();
        };
        write("held-1", "held", false);
        write("held-2", "held", false);
        for index in 0..20 {
            write(&format!("other-{index}"), &format!("other-{index}"), true);
        }
        REREAD.with(|count| count.set(0));
        record_holds(&store).unwrap();
        assert_eq!(REREAD.with(std::cell::Cell::get), 2);
        assert!(held(&store, "me/qa", "held").is_some());
    }

    /// A link an import moves after a confirmation listed it, and before the confirmation takes
    /// its lock, stays as the import left it: it is no longer the older agit's link to that branch.
    #[test]
    fn a_confirmation_writes_the_link_as_it_is_under_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        std::fs::create_dir_all(store.root()).unwrap();
        let mut link = Link::new("codex", "old", None);
        link.owner = Some("me".into());
        link.agent = Some("qa".into());
        link.branch = Some("work".into());
        super::super::write(&store, &link).unwrap();
        let listed = unconfirmed_older(&store, "me/qa", "work");
        assert_eq!(listed.len(), 1);
        link.branch = Some("elsewhere".into());
        super::super::write(&store, &link).unwrap();
        confirm_listed(&store, "me/qa", "work", &listed).unwrap();
        let now = super::super::get(&store, "codex", "old").unwrap();
        assert_eq!(now.branch.as_deref(), Some("elsewhere"));
        assert_eq!(now.claimed_by, None);
    }
}
