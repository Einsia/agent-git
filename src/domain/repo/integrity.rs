//! Loose objects a hard stop leaves without content, and the local branches that reach them.
//!
//! Git writes a loose object to a temporary file and renames it into place. When the machine stops
//! before the data reaches the disk, the final name can be left with no bytes, and Git then
//! refuses the object as corrupt wherever history reaches it. Such a store is found by listing the
//! object directories, without reading any object, so the check is cheap enough to run before
//! every write.
//!
//! A repair records the damaged objects before it removes their empty files. Once the files are
//! gone the objects are only missing, so without the record a repair that stops halfway (no
//! network, no confirmation, a branch checked out) would leave damage no later check finds.

use super::Repo;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::io::Write as _;

/// The damaged objects a repair recorded, under the repository's common Git directory.
const RECORD: &str = "agit/damaged-objects";
/// Where a repair keeps a branch whose history stays unreadable. These refs reach damage on
/// purpose, so walks that judge the repository's health leave them out.
pub const SET_ASIDE: &str = "refs/agit/damaged/";

/// The damaged objects of one repository and the local branches whose history reaches them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Damage {
    /// Object ids the store cannot provide, sorted: loose files left empty, and recorded objects
    /// that are still unreadable.
    pub objects: Vec<String>,
    /// Local branches whose history reaches one of `objects`, or cannot be walked at all.
    pub branches: Vec<String>,
}

/// Loose object files with no content, sorted by object id.
pub fn empty_loose_objects(repo: &Repo) -> Result<Vec<String>> {
    let directory = repo.git_path("objects")?;
    let mut found = Vec::new();
    let fanout = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot list {}", directory.display()));
        }
    };
    for entry in fanout {
        let entry = entry.with_context(|| format!("cannot list {}", directory.display()))?;
        let prefix = entry.file_name().to_string_lossy().into_owned();
        if prefix.len() != 2 || !is_hex(&prefix) {
            continue;
        }
        let path = entry.path();
        let objects =
            std::fs::read_dir(&path).with_context(|| format!("cannot list {}", path.display()))?;
        for object in objects {
            let object = object.with_context(|| format!("cannot list {}", path.display()))?;
            let rest = object.file_name().to_string_lossy().into_owned();
            // SHA-1 and SHA-256 object names; temporary files and anything else are not objects.
            if !matches!(rest.len(), 38 | 62) || !is_hex(&rest) {
                continue;
            }
            let metadata = object
                .metadata()
                .with_context(|| format!("cannot inspect {}", object.path().display()))?;
            if metadata.is_file() && metadata.len() == 0 {
                found.push(format!("{prefix}{rest}"));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The damage in `repo`, or none while no loose object is empty and no ref outside [`SET_ASIDE`]
/// reaches a recorded object that is still unreadable. A record nothing needs any more is
/// forgotten.
pub fn assess(repo: &Repo) -> Result<Option<Damage>> {
    let empty = empty_loose_objects(repo)?;
    let objects = damaged_objects(repo, &empty)?;
    if objects.is_empty() {
        return Ok(None);
    }
    let branches: Vec<String> = repo
        .local_branches()
        .into_iter()
        .filter(|branch| reaches(repo, &format!("refs/heads/{branch}"), &objects))
        .collect();
    // A remote-tracking ref counts too: once the record is gone, a plain fetch never sends the
    // objects of commits the store claims, and a later pull would build on them.
    if empty.is_empty() && branches.is_empty() && !reached_by_any_ref(repo, &objects) {
        clear_record(repo)?;
        return Ok(None);
    }
    Ok(Some(Damage { objects, branches }))
}

/// `empty` together with every recorded object the store still cannot read, sorted. A record
/// whose objects all read again is forgotten.
fn damaged_objects(repo: &Repo, empty: &[String]) -> Result<Vec<String>> {
    let mut objects: BTreeSet<String> = empty.iter().cloned().collect();
    let recorded = recorded(repo)?;
    let mut unreadable = false;
    for object in recorded.iter() {
        if !objects.contains(object) && !readable(repo, object) {
            objects.insert(object.clone());
            unreadable = true;
        }
    }
    if !recorded.is_empty() && !unreadable && empty.is_empty() {
        clear_record(repo)?;
    }
    Ok(objects.into_iter().collect())
}

/// Whether the local store holds `object` and can read its header. A missing object fails, and so
/// does a loose file left empty or cut short, which an existence check alone would accept.
pub fn readable(repo: &Repo, object: &str) -> bool {
    matches!(
        repo.git_status_local(&["cat-file", "-t", object]),
        Ok((Some(0), _, _))
    )
}

/// Add `objects` to the record before their empty files are removed. The record and its directory
/// entry reach the disk first: it guards against the same power loss that empties objects.
pub fn record(repo: &Repo, objects: &[String]) -> Result<()> {
    let path = record_path(repo)?;
    let parent = path
        .parent()
        .context("damaged-object record has no directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let mut all: BTreeSet<String> = recorded(repo)?.into_iter().collect();
    all.extend(objects.iter().cloned());
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create a record in {}", parent.display()))?;
    for object in &all {
        writeln!(temporary, "{object}")?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("cannot sync {}", parent.display()))?;
    Ok(())
}

/// Forget the record: every object it names reads again, or no ref outside [`SET_ASIDE`] reaches
/// one.
pub fn clear_record(repo: &Repo) -> Result<()> {
    let path = record_path(repo)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

fn recorded(repo: &Repo) -> Result<Vec<String>> {
    let path = record_path(repo)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text
            .lines()
            .filter(|line| matches!(line.len(), 40 | 64) && is_hex(line))
            .map(str::to_owned)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn record_path(repo: &Repo) -> Result<std::path::PathBuf> {
    Ok(repo.common_dir()?.join(RECORD))
}

/// Whether the history of `revision` reaches one of `objects` (sorted), or cannot be walked
/// because a commit or tree on the way is unreadable. Listing a blob does not read it, so the
/// listed ids are compared with the damaged ones instead of trusting the walk to fail.
pub fn reaches(repo: &Repo, revision: &str, objects: &[String]) -> bool {
    reaches_from(repo, &[revision], objects)
}

/// Remote-tracking refs and tags, other than version tags, that reach one of `objects` (sorted),
/// each walked on its own. Version tags are left out: a checkout holds one per published turn,
/// and walking each is too slow to name the few that matter.
pub fn other_refs_reaching(repo: &Repo, objects: &[String]) -> Vec<String> {
    let listed = repo
        .git_status_local(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/remotes",
            "refs/tags",
        ])
        .ok()
        .and_then(|(status, out, _)| (status == Some(0)).then_some(out))
        .unwrap_or_default();
    listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(oid, name)| name.strip_prefix("refs/tags/agit-") != Some(*oid))
        .map(|(_, name)| name)
        .filter(|name| reaches(repo, name, objects))
        .map(str::to_owned)
        .collect()
}

/// Whether any ref outside [`SET_ASIDE`] reaches one of `objects` (sorted).
fn reached_by_any_ref(repo: &Repo, objects: &[String]) -> bool {
    let exclude = format!("--exclude={SET_ASIDE}*");
    reaches_from(repo, &[exclude.as_str(), "--all"], objects)
}

fn reaches_from(repo: &Repo, revisions: &[&str], objects: &[String]) -> bool {
    let args: Vec<&str> = ["rev-list", "--objects"]
        .into_iter()
        .chain(revisions.iter().copied())
        .chain(["--"])
        .collect();
    match repo.git_status_local(&args) {
        Ok((Some(0), listed, _)) => listed.lines().any(|line| {
            let oid = line.split(' ').next().unwrap_or_default();
            objects
                .binary_search_by(|object| object.as_str().cmp(oid))
                .is_ok()
        }),
        _ => true,
    }
}

/// Objects the history of `revision` needs that this store cannot provide, found without
/// fetching. A history that cannot be walked at all is reported as `revision` itself.
pub fn missing_objects(repo: &Repo, revision: &str) -> Vec<String> {
    match repo.git_status_local(&["rev-list", "--objects", "--missing=print", revision, "--"]) {
        Ok((Some(0), listed, _)) => listed
            .lines()
            .filter_map(|line| line.strip_prefix('?'))
            .map(str::to_owned)
            .collect(),
        _ => vec![revision.to_owned()],
    }
}

/// Refuse to write a commit on top of `parents` when their history reaches a damaged object: the
/// new commit would extend history that cannot be read or published.
///
/// A store found intact is not listed again in the same process: an empty object under its final
/// name means the system lost data it had accepted, which only a crash or power loss does, and
/// that stops this process too. An import writes many commits in a row, so a record that no ref
/// needs any more is forgotten here rather than walked again for each of them.
pub fn refuse_damaged_parents(repo: &Repo, parents: &[&str]) -> Result<()> {
    static INTACT: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    let intact = INTACT.get_or_init(Default::default);
    let store = repo.git_path("objects")?;
    if intact.lock().is_ok_and(|seen| seen.contains(&store)) {
        return Ok(());
    }
    let empty = empty_loose_objects(repo)?;
    let objects = damaged_objects(repo, &empty)?;
    for parent in parents {
        anyhow::ensure!(
            objects.is_empty() || !reaches(repo, parent, &objects),
            "{}",
            damaged_store_message(repo)
        );
    }
    if !objects.is_empty() {
        if !empty.is_empty() || reached_by_any_ref(repo, &objects) {
            return Ok(());
        }
        clear_record(repo)?;
    }
    if let Ok(mut seen) = intact.lock() {
        seen.insert(store);
    }
    Ok(())
}

/// "1 damaged Git object", "2 damaged Git objects".
pub fn damaged_objects_phrase(count: usize) -> String {
    if count == 1 {
        "1 damaged Git object".into()
    } else {
        format!("{count} damaged Git objects")
    }
}

/// Whether Git's diagnostics describe an unreadable object in the local store rather than a
/// network or permission failure.
pub fn reports_damage(diagnostics: &str) -> bool {
    let text = diagnostics.to_ascii_lowercase();
    (text.contains("object file") && text.contains("is empty"))
        || (text.contains("loose object") && text.contains("is corrupt"))
        || text.contains("object corrupt or missing")
        || text.contains("bad object")
}

/// What a person reads when a command stops at damaged objects: what happened and the one command
/// that repairs it.
pub fn damaged_store_message(repo: &Repo) -> String {
    let target = repository_slug(repo)
        .map(|slug| format!(" {slug}"))
        .unwrap_or_else(|| " OWNER/REPO".into());
    format!(
        "this repository's local Git objects are damaged (a hard stop or crash can leave loose \
         objects empty); run `agit doctor --repair-objects{target}` to restore what the Hub \
         has and to see what only this machine had"
    )
}

/// `owner/name` of a repository under agit's home, from its location.
fn repository_slug(repo: &Repo) -> Option<String> {
    let name = repo.root().file_name()?.to_str()?;
    let owner = repo.root().parent()?.file_name()?.to_str()?;
    Some(format!("{owner}/{name}"))
}

fn is_hex(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object a repair recorded and then removed still stops a commit on top of history that
    /// reaches it, until the store reads it again; then the record is forgotten. A guard that only
    /// lists empty files lets that commit through once the files are gone.
    #[test]
    fn a_recorded_object_refuses_commits_until_it_reads_again() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        std::fs::write(directory.path().join("page.md"), "content\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("Page").unwrap();
        let tip = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let blob = repo.git(&["rev-parse", "HEAD:page.md"]).unwrap();
        let loose = repo
            .git_path("objects")
            .unwrap()
            .join(&blob[..2])
            .join(&blob[2..]);
        let content = std::fs::read(&loose).unwrap();
        record(&repo, std::slice::from_ref(&blob)).unwrap();
        std::fs::remove_file(&loose).unwrap();
        assert!(refuse_damaged_parents(&repo, &[&tip]).is_err());
        std::fs::write(&loose, content).unwrap();
        refuse_damaged_parents(&repo, &[&tip]).unwrap();
        assert!(recorded(&repo).unwrap().is_empty());
    }
}
