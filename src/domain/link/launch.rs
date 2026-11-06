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

/// Whether rewriting a link as `after` could end what makes its branch, or the branch of a link
/// beside it, held: the record has to be there before that is gone from the store.
pub(super) fn unsettles(before: &Link, after: &Link) -> bool {
    bare(before) != bare(after)
        || before.owner != after.owner
        || before.agent != after.agent
        || before.branch != after.branch
}

/// A branch held back from every upload because an unconfirmed link names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    pub slug: String,
    pub branch: String,
    /// The sessions whose links named it, `<runtime>/<id>`.
    pub sessions: Vec<String>,
    /// The person let it upload again, as it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
}

fn holds(store: &Store) -> PathBuf {
    beside(store).join("upload-holds")
}

fn hold_path(store: &Store, slug: &str, branch: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(format!("{slug}@{branch}").as_bytes()));
    holds(store).join(format!("{}.json", &digest[..32]))
}

/// Finds the store's unconfirmed links and records a hold on each branch they name. A hold, once
/// recorded, stays, whatever becomes of the link.
pub fn record_holds(store: &Store) -> Result<()> {
    let all = list(store);
    for link in all.iter().filter(|link| unconfirmed(link, &all)) {
        let (Some(agent), Some(branch)) = (&link.agent, &link.branch) else {
            continue;
        };
        let slug = match &link.owner {
            Some(owner) => format!("{owner}/{agent}"),
            None => agent.clone(),
        };
        let session = format!("{}/{}", link.source, link.session_id);
        let path = hold_path(store, &slug, branch);
        let mut hold = held_at(&path).unwrap_or(Hold {
            slug: slug.clone(),
            branch: branch.clone(),
            sessions: Vec::new(),
            released: false,
        });
        if hold.released || hold.sessions.contains(&session) {
            continue;
        }
        hold.sessions.push(session);
        crate::infra::config::create_state_dir(&holds(store))?;
        let mut temporary = tempfile::NamedTempFile::new_in(holds(store))?;
        use std::io::Write as _;
        serde_json::to_writer(&mut temporary, &hold)?;
        temporary.flush()?;
        temporary.into_temp_path().persist(&path)?;
    }
    Ok(())
}

fn held_at(path: &Path) -> Option<Hold> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The hold on `slug`@`branch`, after recording any the store's links call for. A hold that can't
/// be read counts as one, and so does one the links call for that couldn't be recorded. Only the
/// person's release lifts it.
pub fn held(store: &Store, slug: &str, branch: &str) -> Option<Hold> {
    let _ = record_holds(store);
    let path = hold_path(store, slug, branch);
    let unreadable = || Hold {
        slug: slug.into(),
        branch: branch.into(),
        sessions: Vec::new(),
        released: false,
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Hold>(&bytes).map_or_else(
            |_| Some(unreadable()),
            |hold| (!hold.released).then_some(hold),
        ),
        // Not recorded, whether or not recording failed: what the links call for still holds.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => all_holds(store)
            .into_iter()
            .find(|hold| hold.slug == slug && hold.branch == branch),
        Err(_) => Some(unreadable()),
    }
}

/// Lifts the hold on `slug`@`branch`, for good: the links that called for it don't hold it again.
pub fn release(store: &Store, slug: &str, branch: &str) -> Result<()> {
    let path = hold_path(store, slug, branch);
    let mut hold = held_at(&path).unwrap_or(Hold {
        slug: slug.into(),
        branch: branch.into(),
        sessions: Vec::new(),
        released: false,
    });
    hold.released = true;
    crate::infra::config::create_state_dir(&holds(store))?;
    let mut temporary = tempfile::NamedTempFile::new_in(holds(store))?;
    use std::io::Write as _;
    serde_json::to_writer(&mut temporary, &hold)?;
    temporary.flush()?;
    temporary.into_temp_path().persist(&path)?;
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
    let all = list(store);
    for link in all.iter().filter(|link| unconfirmed(link, &all)) {
        let (Some(agent), Some(branch)) = (&link.agent, &link.branch) else {
            continue;
        };
        let slug = match &link.owner {
            Some(owner) => format!("{owner}/{agent}"),
            None => agent.clone(),
        };
        if !found
            .iter()
            .any(|hold| hold.slug == slug && &hold.branch == branch)
        {
            found.push(Hold {
                slug,
                branch: branch.clone(),
                sessions: vec![format!("{}/{}", link.source, link.session_id)],
                released: false,
            });
        }
    }
    found.retain(|hold| !hold.released);
    found.sort_by(|a, b| (&a.slug, &a.branch).cmp(&(&b.slug, &b.branch)));
    found
}

/// The fixed phrase every refusal of a held branch carries.
pub const REFUSAL: &str = "may hold another conversation's turns";

/// Why a held branch doesn't upload, and the way on, for the person. Never offered to an agent:
/// lifting the hold uploads the branch's turns as they are.
pub fn explain(hold: &Hold) -> String {
    format!(
        "{slug}@{branch} {REFUSAL}, so it stays on this computer. To go on, fork it at your own \
         last turn: `agit fork {slug}@{branch}#<n> -b <new-name>`. Once the other turns are moved \
         out, `agit branch unhold {branch} --repo {slug}` lets it upload again, with its turns as they are.",
        slug = hold.slug,
        branch = hold.branch,
    )
}
