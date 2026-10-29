//! Recovery state of a repository's registered checkouts, read once per repository.
//!
//! The read-only preflight needs two facts about every registered checkout: whether its private
//! Git directory holds checkout-transaction metadata, and whether its branch has a legacy layout
//! recovery ref. For the layout `git worktree add` creates, both follow from files Git keeps (a
//! linked checkout's `.git` link and its registration under `<common>/worktrees/<id>`), from the
//! branches `git worktree list` already reports, and from one listing of the refs. A branch with a
//! recovery ref is ruled out, where it can be, from one batch read of the repository's objects.
//! So no Git process runs per checkout unless that read leaves a branch to the exact legacy probe.
//! A checkout whose layout differs in any way is answered by the per-checkout probe instead: a
//! shortcut that guessed how Git resolves an unusual layout could look in the wrong directory and
//! report a checkout with pending recovery as settled.

use super::LEGACY_RECOVERY_REF_PREFIX;
use crate::domain::meta;
use crate::domain::repo::{Repo, Worktree};
use anyhow::Result;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Stdio};

/// `git rev-parse --verify <name>` resolves a recovery ref name through each of these prefixes,
/// so a listing that omits any of them could miss a ref the per-checkout probe would find.
const RECOVERY_REF_SPELLINGS: [&str; 5] = [
    LEGACY_RECOVERY_REF_PREFIX,
    "refs/refs/agit/layout-v0/",
    "refs/tags/refs/agit/layout-v0/",
    "refs/heads/refs/agit/layout-v0/",
    "refs/remotes/refs/agit/layout-v0/",
];
const MAX_GIT_LINK_BYTES: usize = 64 * 1024;
const MAX_REF_LISTING_BYTES: usize = 1024 * 1024;
/// The legacy probe reads HEAD's metadata through an output cap of this size, and fails above it.
const MAX_PROBE_METADATA_BYTES: usize = 1024 * 1024;
const MAX_BATCH_HEADER_BYTES: u64 = 256;
const MAX_BATCH_OBJECT_BYTES: usize = 1024 * 1024;

pub(super) struct Checkouts {
    /// Checkouts in the plain layout, keyed by canonical path.
    plain: HashMap<PathBuf, PlainCheckout>,
    /// The recovery refs, or None when the listing cannot stand in for per-branch lookups.
    recovery_refs: Option<RecoveryRefs>,
    /// Plain checkouts whose branch has a recovery ref and whose legacy probe would find nothing
    /// to recover.
    settled: HashSet<PathBuf>,
}

struct RecoveryRefs {
    /// Every recovery ref name in ASCII lowercase.
    folded: BTreeSet<String>,
    /// The object each recovery ref names, by its exact name.
    objects: HashMap<String, String>,
}

struct PlainCheckout {
    git_dir: PathBuf,
    /// The checked-out branch; None only for a detached HEAD that names a commit.
    branch: Option<String>,
    /// The commit HEAD resolves to, as `git worktree list` reports it.
    head: Option<String>,
}

impl Checkouts {
    /// `common` is the repository's canonical common Git directory when it is known exactly, and
    /// `registered` is the `git worktree list` inspection run from `repo`.
    pub(super) fn inspect(repo: &Repo, common: Option<&Path>, registered: &[Worktree]) -> Self {
        let mut plain = HashMap::new();
        if let Some(common) = common {
            let mut seen = HashSet::new();
            let mut repeated = HashSet::new();
            for worktree in registered {
                let Ok(path) = worktree.path.canonicalize() else {
                    continue;
                };
                // Two registrations of one path carry two HEADs, and only one of them is the
                // registration Git reaches from the checkout.
                if !seen.insert(path.clone()) {
                    repeated.insert(path);
                    continue;
                }
                if let Some(checkout) = plain_checkout(common, worktree) {
                    plain.insert(path, checkout);
                }
            }
            for path in repeated {
                plain.remove(&path);
            }
        }
        let recovery_refs = if plain.is_empty() {
            None
        } else {
            recovery_refs(repo)
        };
        let settled = match &recovery_refs {
            Some(refs) => settled_by_history(
                repo,
                plain
                    .iter()
                    .filter_map(|(path, checkout)| {
                        let branch = checkout.branch.as_ref()?;
                        let recovery = refs
                            .objects
                            .get(&format!("{LEGACY_RECOVERY_REF_PREFIX}{branch}"))?;
                        let head = checkout.head.as_ref()?;
                        Some((path.clone(), head.clone(), recovery.clone()))
                    })
                    .collect(),
            ),
            None => HashSet::new(),
        };
        Checkouts {
            plain,
            recovery_refs,
            settled,
        }
    }

    /// Whether the checkout at `root` has pending recovery. `metadata` and `legacy` are the
    /// per-checkout Git probes, run only for the facts these files cannot establish exactly.
    pub(super) fn pending(
        &self,
        root: &Path,
        metadata: impl FnOnce() -> Result<bool>,
        legacy: impl FnOnce() -> Result<bool>,
    ) -> Result<bool> {
        let path = root.canonicalize().ok();
        let Some((path, checkout)) = path
            .as_ref()
            .and_then(|path| Some((path, self.plain.get(path)?)))
        else {
            return Ok(metadata()? || legacy()?);
        };
        if super::super::plumbing::interrupted_checkout_metadata_in(&checkout.git_dir)? {
            return Ok(true);
        }
        match (&self.recovery_refs, &checkout.branch) {
            (Some(_), None) => Ok(false),
            (Some(refs), Some(branch)) if !may_have_recovery_ref(&refs.folded, branch) => Ok(false),
            (Some(_), Some(_)) if self.settled.contains(path) => Ok(false),
            _ => legacy(),
        }
    }
}

/// Loose refs are files, so on a case-insensitive filesystem Git finds a recovery ref under a
/// branch name that differs from it only in case. Names are therefore compared without ASCII
/// case, and a branch outside ASCII, whose folding belongs to the filesystem, is never ruled out.
fn may_have_recovery_ref(refs: &BTreeSet<String>, branch: &str) -> bool {
    !branch.is_ascii()
        || refs.contains(&format!("{LEGACY_RECOVERY_REF_PREFIX}{branch}").to_ascii_lowercase())
}

/// The private Git directory and branch of a registration whose layout is the plain one: the
/// main checkout's `.git` is the common directory itself, and a linked checkout's `.git` link
/// names a registration under `<common>/worktrees` whose `gitdir` names the checkout back and
/// whose `commondir` names this repository. Git reads the same files when it runs inside the
/// checkout, so the directory is the one `git rev-parse --git-path` would report there, and the
/// branch is the one `git symbolic-ref` would report.
fn plain_checkout(common: &Path, worktree: &Worktree) -> Option<PlainCheckout> {
    let branch = match (&worktree.branch, &worktree.head) {
        (Some(branch), _) => Some(branch.clone()),
        // `git worktree list` prints a zero object name and neither a branch nor `detached` when
        // HEAD cannot be resolved; `git symbolic-ref` then fails instead of reporting no branch.
        (None, Some(head)) if head.bytes().any(|byte| byte != b'0') => None,
        (None, _) => return None,
    };
    let link = worktree.path.join(".git");
    let metadata = std::fs::symlink_metadata(&link).ok()?;
    let git_dir = if metadata.is_dir() {
        (link.canonicalize().ok()? == common).then(|| common.to_path_buf())?
    } else if metadata.is_file() {
        let registration = linked_git_dir(&link)?.canonicalize().ok()?;
        let registrations = common.join("worktrees").canonicalize().ok()?;
        if registration.parent() != Some(registrations.as_path())
            || registered_link(&registration)? != link.canonicalize().ok()?
            || registered_common(&registration)? != common
        {
            return None;
        }
        registration
    } else {
        return None;
    };
    let head = worktree
        .head
        .clone()
        .filter(|head| head.bytes().any(|byte| byte != b'0'));
    Some(PlainCheckout {
        git_dir,
        branch,
        head,
    })
}

/// The directory a `.git` link names, parsed the way Git parses it: a `gitdir: ` line with any
/// trailing line terminators removed, relative to the directory holding the link.
fn linked_git_dir(link: &Path) -> Option<PathBuf> {
    let bytes = crate::domain::storage::read_bytes_capped(link, MAX_GIT_LINK_BYTES).ok()?;
    let target = native_path(trim_line_terminators(bytes.strip_prefix(b"gitdir: ")?))?;
    Some(link.parent()?.join(target))
}

/// The checkout link a registration's `gitdir` file names, canonicalized. Git trims trailing
/// whitespace and resolves a relative path against the registration directory.
fn registered_link(registration: &Path) -> Option<PathBuf> {
    let bytes =
        crate::domain::storage::read_bytes_capped(&registration.join("gitdir"), MAX_GIT_LINK_BYTES)
            .ok()?;
    let target = native_path(bytes.trim_ascii_end())?;
    registration.join(target).canonicalize().ok()
}

/// The common directory a registration's `commondir` file names, canonicalized. Git strips
/// trailing line terminators and resolves a relative path against the registration directory;
/// a registration without the file is not a usable Git directory.
fn registered_common(registration: &Path) -> Option<PathBuf> {
    let bytes = crate::domain::storage::read_bytes_capped(
        &registration.join("commondir"),
        MAX_GIT_LINK_BYTES,
    )
    .ok()?;
    let target = native_path(trim_line_terminators(&bytes))?;
    registration.join(target).canonicalize().ok()
}

fn trim_line_terminators(mut bytes: &[u8]) -> &[u8] {
    while let Some(rest) = bytes
        .strip_suffix(b"\n")
        .or_else(|| bytes.strip_suffix(b"\r"))
    {
        bytes = rest;
    }
    bytes
}

fn native_path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

/// Every ref `git rev-parse --verify` could resolve for a recovery ref name, from one listing of
/// all refs. A pattern would miss a loose ref whose directory differs in case from it on a
/// case-insensitive filesystem, where `rev-parse` still finds the file, so names are matched
/// without ASCII case here. Only ASCII names under the recovery namespace's own spelling are
/// answered from the listing; any other spelling, a name whose case folding belongs to the
/// filesystem, an incomplete listing, or a diagnostic leaves every branch to the per-checkout
/// lookup. Git leaves a dangling symbolic ref out of every listing; agit never writes one here.
fn recovery_refs(repo: &Repo) -> Option<RecoveryRefs> {
    let output = repo
        .inspection_output(
            &["for-each-ref", "--format=%(refname)%00%(objectname)"],
            MAX_REF_LISTING_BYTES,
        )
        .ok()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    let listing = std::str::from_utf8(&output.stdout).ok()?;
    let spellings = RECOVERY_REF_SPELLINGS.map(str::to_ascii_lowercase);
    let mut refs = RecoveryRefs {
        folded: BTreeSet::new(),
        objects: HashMap::new(),
    };
    for line in listing.lines() {
        let (name, object) = line.split_once('\0')?;
        let folded = name.to_ascii_lowercase();
        if !spellings
            .iter()
            .any(|spelling| folded.starts_with(spelling))
        {
            continue;
        }
        if !name.starts_with(LEGACY_RECOVERY_REF_PREFIX) || !name.is_ascii() {
            return None;
        }
        refs.folded.insert(folded);
        refs.objects.insert(name.to_owned(), object.to_owned());
    }
    Some(refs)
}

/// The checkouts among `candidates` (path, HEAD commit, recovery ref object) whose legacy probe
/// would find nothing to recover, decided from one `git cat-file --batch` for the repository.
///
/// The probe reads HEAD's `session/meta.json`. Its only candidate is a regular, readable v1
/// snapshot whose commit has the storage migration subject and whose first parent is the
/// recovery ref. A checkout is settled here only where every read answers as the probe's own
/// commands would and one of them rules the candidate out. Everything else is left to the probe:
/// a candidate, every read the probe fails on, and anything this read does not interpret the way
/// Git does. So the probe's errors and candidates stay exactly its own.
fn settled_by_history(repo: &Repo, candidates: Vec<(PathBuf, String, String)>) -> HashSet<PathBuf> {
    let mut settled = HashSet::new();
    if candidates.is_empty() {
        return settled;
    }
    let Ok(mut child) = repo
        .inspection_command(&["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return settled;
    };
    if let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) {
        let mut objects = Batch {
            input,
            output: BufReader::new(output),
        };
        for (path, head, recovery) in candidates {
            match rules_out_recovery(&mut objects, &head, &recovery) {
                Ok(true) => {
                    settled.insert(path);
                }
                Ok(false) => {}
                // The stream is out of step; what is left goes to the probe.
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    settled
}

/// Whether the legacy probe, given `head` and the recovery ref object `recovery`, would return no
/// candidate without failing. `Err` means the batch stream can no longer be read in step.
fn rules_out_recovery(objects: &mut Batch, head: &str, recovery: &str) -> std::io::Result<bool> {
    let Object::Found {
        kind,
        body: Some(body),
    } = objects.read(head, MAX_BATCH_OBJECT_BYTES)?
    else {
        return Ok(false);
    };
    let Some(commit) = (kind == "commit").then(|| Commit::parse(&body)).flatten() else {
        return Ok(false);
    };
    let width = head.len() / 2;
    let layout = match session_metadata(objects, &commit.tree, width)? {
        Metadata::Absent => return Ok(true),
        Metadata::Unreadable => return Ok(false),
        Metadata::Present(layout) => layout,
    };
    if layout != meta::LayoutVersion::V1 || commit.subject != meta::STORAGE_MIGRATION_MESSAGE {
        return Ok(true);
    }
    Ok(commit.first_parent.is_some_and(|parent| parent != recovery))
}

enum Metadata {
    /// `git ls-tree <commit> -- session/meta.json` lists nothing.
    Absent,
    /// The probe would fail reading it.
    Unreadable,
    /// It parses as metadata of this layout.
    Present(meta::LayoutVersion),
}

/// HEAD's `session/meta.json` as the probe reads it: listed only inside a `session` tree, required
/// to be a regular blob within the probe's output cap, and parsed as metadata.
fn session_metadata(objects: &mut Batch, tree: &str, width: usize) -> std::io::Result<Metadata> {
    let Some(root) = objects.tree(tree)? else {
        return Ok(Metadata::Unreadable);
    };
    let session = match tree_entry(&root, b"session", width) {
        Err(()) => return Ok(Metadata::Unreadable),
        Ok(Some((mode, oid))) if mode == b"40000" => oid,
        Ok(_) => return Ok(Metadata::Absent),
    };
    let Some(session) = objects.tree(&session)? else {
        return Ok(Metadata::Unreadable);
    };
    let file = match tree_entry(&session, b"meta.json", width) {
        Err(()) => return Ok(Metadata::Unreadable),
        Ok(None) => return Ok(Metadata::Absent),
        Ok(Some((mode, oid))) if mode == b"100644" || mode == b"100755" => oid,
        Ok(Some(_)) => return Ok(Metadata::Unreadable),
    };
    Ok(match objects.read(&file, MAX_PROBE_METADATA_BYTES)? {
        Object::Found {
            kind,
            body: Some(body),
        } if kind == "blob" => match serde_json::from_slice::<meta::Meta>(&body) {
            Ok(snapshot) => Metadata::Present(snapshot.layout),
            Err(_) => Metadata::Unreadable,
        },
        _ => Metadata::Unreadable,
    })
}

/// A tree entry's mode and object id.
type TreeEntry = (Vec<u8>, String);

/// The mode and object of the only entry called `name` in a tree object. Two entries with one
/// name, or a malformed tree, are an error: Git's answer for them is not this reader's to guess.
fn tree_entry(
    body: &[u8],
    name: &[u8],
    width: usize,
) -> std::result::Result<Option<TreeEntry>, ()> {
    let mut found = None;
    let mut rest = body;
    while !rest.is_empty() {
        let space = rest.iter().position(|byte| *byte == b' ').ok_or(())?;
        let nul = rest.iter().position(|byte| *byte == 0).ok_or(())?;
        if nul < space || rest.len() < nul + 1 + width {
            return Err(());
        }
        let (mode, entry) = (&rest[..space], &rest[space + 1..nul]);
        let oid = &rest[nul + 1..nul + 1 + width];
        if entry == name {
            if found.is_some() {
                return Err(());
            }
            found = Some((mode.to_vec(), hex::encode(oid)));
        }
        rest = &rest[nul + 1 + width..];
    }
    Ok(found)
}

/// The parts of a commit object the probe consults.
struct Commit {
    tree: String,
    first_parent: Option<String>,
    /// What `git show -s --format=%s` prints.
    subject: String,
}

impl Commit {
    /// None for anything whose subject Git might print differently: a commit that is not UTF-8,
    /// holds a NUL, or names an encoding to convert from.
    fn parse(body: &[u8]) -> Option<Self> {
        if body.contains(&0) {
            return None;
        }
        let text = std::str::from_utf8(body).ok()?;
        let (headers, message) = text.split_once("\n\n").unwrap_or((text, ""));
        let mut tree = None;
        let mut first_parent = None;
        for header in headers.split('\n') {
            if let Some(oid) = header.strip_prefix("tree ") {
                tree.get_or_insert_with(|| oid.to_owned());
            } else if let Some(oid) = header.strip_prefix("parent ") {
                first_parent.get_or_insert_with(|| oid.to_owned());
            } else if header.starts_with("encoding ") {
                return None;
            }
        }
        Some(Commit {
            tree: tree?,
            first_parent,
            subject: subject(message),
        })
    }
}

/// Git's `%s`: leading blank lines skipped, then the first paragraph's lines, each without its
/// trailing whitespace, joined by single spaces.
fn subject(message: &str) -> String {
    message
        .split('\n')
        .map(|line| line.trim_end_matches(|c: char| c.is_ascii_whitespace()))
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

enum Object {
    Missing,
    /// `body` is None when the object is larger than the cap the read gave.
    Found {
        kind: String,
        body: Option<Vec<u8>>,
    },
}

struct Batch {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Batch {
    /// One object by name. An object above `cap` is consumed without being kept, so the stream
    /// stays in step; `Err` means it could not be.
    fn read(&mut self, name: &str, cap: usize) -> std::io::Result<Object> {
        let malformed = || std::io::Error::other("cat-file --batch printed a malformed record");
        writeln!(self.input, "{name}")?;
        self.input.flush()?;
        let mut header = Vec::new();
        (&mut self.output)
            .take(MAX_BATCH_HEADER_BYTES)
            .read_until(b'\n', &mut header)?;
        let header = std::str::from_utf8(&header)
            .ok()
            .and_then(|header| header.strip_suffix('\n'))
            .ok_or_else(malformed)?;
        let fields: Vec<&str> = header.split(' ').collect();
        let (kind, size) = match fields.as_slice() {
            [_, "missing"] => return Ok(Object::Missing),
            [_, kind, size] => (*kind, size.parse::<u64>().map_err(|_| malformed())?),
            _ => return Err(malformed()),
        };
        let body = if size <= cap as u64 {
            let mut body = vec![0; size as usize];
            self.output.read_exact(&mut body)?;
            Some(body)
        } else {
            let skipped = std::io::copy(&mut (&mut self.output).take(size), &mut std::io::sink())?;
            if skipped != size {
                return Err(malformed());
            }
            None
        };
        let mut terminator = [0];
        self.output.read_exact(&mut terminator)?;
        if terminator != *b"\n" {
            return Err(malformed());
        }
        Ok(Object::Found {
            kind: kind.to_owned(),
            body,
        })
    }

    /// A tree object's body, or None when the name is not a readable tree within the cap.
    fn tree(&mut self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        Ok(match self.read(name, MAX_BATCH_OBJECT_BYTES)? {
            Object::Found {
                kind,
                body: Some(body),
            } if kind == "tree" => Some(body),
            _ => None,
        })
    }
}

/// The common Git directory Git uses for the main checkout at `root`, canonicalized, when it is
/// `root/.git` itself: a real directory without a `commondir` redirection.
pub(super) fn main_checkout_common_dir(root: &Path) -> Option<PathBuf> {
    let git_dir = root.join(".git");
    let metadata = std::fs::symlink_metadata(&git_dir).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    match std::fs::symlink_metadata(git_dir.join("commondir")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.canonicalize().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unexpected(probe: &'static str) -> impl FnOnce() -> Result<bool> {
        move || panic!("plain checkout reached the {probe} probe")
    }

    /// Plain checkouts are answered from Git's files: a journal in a linked checkout's private
    /// directory is pending recovery, and a checkout without a recovery ref reaches no
    /// per-checkout probe. Probing every checkout, or reading the journal from the common
    /// directory, fails here.
    #[test]
    fn plain_checkouts_reach_a_probe_only_for_recovered_branches() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("main")).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "base"])
            .unwrap();
        for branch in ["clean", "journal"] {
            repo.git(&["branch", branch]).unwrap();
            repo.add_worktree(&directory.path().join(branch), branch)
                .unwrap();
        }
        let detached = directory.path().join("detached");
        repo.git(&["worktree", "add", "--detach", detached.to_str().unwrap()])
            .unwrap();
        let journal = Repo::at(directory.path().join("journal"))
            .git_path("agit-checkout-transaction.json")
            .unwrap();
        std::fs::write(&journal, b"pending").unwrap();

        let inspected = repo.clone().local_objects_only();
        let registered = inspected.inspection_worktrees().unwrap();
        let common = main_checkout_common_dir(repo.root()).unwrap();
        let checkouts = Checkouts::inspect(&inspected, Some(&common), &registered);
        for root in [repo.root(), &directory.path().join("clean"), &detached] {
            assert!(
                !checkouts
                    .pending(root, unexpected("metadata"), unexpected("legacy"))
                    .unwrap()
            );
        }
        assert!(
            checkouts
                .pending(
                    &directory.path().join("journal"),
                    unexpected("metadata"),
                    unexpected("legacy"),
                )
                .unwrap()
        );

        // A recovery ref under another spelling Git resolves, and a checkout whose `.git` link
        // Git's own rules would not resolve to its registration, leave the answer to the probes.
        repo.git(&["update-ref", "refs/tags/refs/agit/layout-v0/clean", "HEAD"])
            .unwrap();
        let unusual = directory.path().join("unusual");
        repo.git(&["branch", "unusual"]).unwrap();
        repo.add_worktree(&unusual, "unusual").unwrap();
        let link = std::fs::read_to_string(unusual.join(".git")).unwrap();
        std::fs::write(unusual.join(".git"), format!("{} \n", link.trim_end())).unwrap();
        let registered = inspected.inspection_worktrees().unwrap();
        let checkouts = Checkouts::inspect(&inspected, Some(&common), &registered);
        for (root, plain) in [(directory.path().join("clean"), true), (unusual, false)] {
            let (mut metadata, mut legacy) = (false, false);
            let pending = checkouts.pending(
                &root,
                || {
                    metadata = true;
                    Ok(false)
                },
                || {
                    legacy = true;
                    Ok(false)
                },
            );
            assert!(!pending.unwrap());
            assert_eq!((metadata, legacy), (!plain, true), "{}", root.display());
        }
    }

    /// A recovered branch is settled from the repository's one batch read only where the legacy
    /// probe itself would find nothing to recover, and every other recovered branch still gets the
    /// probe: the same answer per branch, with no per-checkout Git process for the branches the
    /// batch settles. Settling the migration commit on top of its recovery ref would report pending
    /// recovery as settled, and so would reading the subject past a NUL where Git's `%s` stops;
    /// settling a snapshot whose metadata the probe cannot read would hide its failure.
    #[test]
    fn recovered_branches_are_settled_only_where_the_legacy_probe_finds_nothing() {
        use crate::domain::meta::{self, LayoutVersion, Meta};
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("main")).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "base"])
            .unwrap();
        let base = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let current = meta::to_text(&Meta::new_file_line()).unwrap();
        let mut legacy = Meta::new_file_line();
        legacy.layout = LayoutVersion::V0;
        let legacy = meta::to_text(&legacy).unwrap();
        let migration = meta::STORAGE_MIGRATION_MESSAGE;
        let truncated = format!("{migration}\0suffix");
        let elsewhere = {
            repo.git(&["commit", "--allow-empty", "-m", "unrelated"])
                .unwrap();
            repo.git(&["rev-parse", "HEAD"]).unwrap()
        };
        // (branch, metadata, subject, recovery ref target, settled by the batch)
        let cases = [
            ("plain", None, "work", base.as_str(), true),
            (
                "legacy",
                Some(legacy.as_str()),
                migration,
                base.as_str(),
                true,
            ),
            (
                "subject",
                Some(current.as_str()),
                "work",
                base.as_str(),
                true,
            ),
            (
                "elsewhere",
                Some(current.as_str()),
                migration,
                elsewhere.as_str(),
                true,
            ),
            (
                "candidate",
                Some(current.as_str()),
                migration,
                base.as_str(),
                false,
            ),
            ("linked", Some("link"), migration, base.as_str(), false),
            // Git's `%s` stops at the NUL, so the probe reads the migration subject.
            (
                "truncated",
                Some(current.as_str()),
                truncated.as_str(),
                base.as_str(),
                false,
            ),
            ("malformed", Some("{"), migration, base.as_str(), false),
        ];
        for (branch, metadata, subject, recovery, _) in cases {
            let path = directory.path().join(branch);
            repo.git(&["branch", branch, &base]).unwrap();
            let checkout = repo.add_worktree(&path, branch).unwrap();
            std::fs::write(path.join("work.txt"), branch).unwrap();
            checkout.add_all().unwrap();
            match metadata {
                Some("link") => {
                    let target = path.join("target.txt");
                    std::fs::write(&target, &current).unwrap();
                    let blob = checkout
                        .git(&["hash-object", "-w", target.to_str().unwrap()])
                        .unwrap();
                    std::fs::remove_file(&target).unwrap();
                    checkout
                        .git(&[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("120000,{blob},{}", meta::FILE),
                        ])
                        .unwrap();
                }
                Some(text) => {
                    std::fs::create_dir_all(path.join("session")).unwrap();
                    std::fs::write(path.join(meta::FILE), text).unwrap();
                    checkout.add_all().unwrap();
                }
                None => {}
            }
            if let Some((subject, rest)) = subject.split_once('\0') {
                // `-m` cannot carry a NUL, so the commit object is written directly.
                checkout.commit(subject).unwrap();
                let object = checkout.git(&["cat-file", "commit", "HEAD"]).unwrap();
                let (headers, _) = object.split_once("\n\n").unwrap();
                let raw = directory.path().join(format!("{branch}.commit"));
                std::fs::write(&raw, format!("{headers}\n\n{subject}\0{rest}\n")).unwrap();
                let oid = checkout
                    .git(&[
                        "hash-object",
                        "-t",
                        "commit",
                        "-w",
                        "--literally",
                        raw.to_str().unwrap(),
                    ])
                    .unwrap();
                checkout.git(&["update-ref", "HEAD", &oid]).unwrap();
            } else {
                checkout.commit(subject).unwrap();
            }
            repo.git(&[
                "update-ref",
                &format!("{LEGACY_RECOVERY_REF_PREFIX}{branch}"),
                recovery,
            ])
            .unwrap();
        }

        let inspected = repo.clone().local_objects_only();
        let registered = inspected.inspection_worktrees().unwrap();
        let common = main_checkout_common_dir(repo.root()).unwrap();
        let checkouts = Checkouts::inspect(&inspected, Some(&common), &registered);
        for (branch, _, _, _, settled) in cases {
            let path = directory.path().join(branch);
            let probe = super::super::interrupted_storage_checkout_with_policy(
                &Repo::at(&path).local_objects_only(),
                super::super::ReadPolicy::LocalOnly,
            );
            let mut probed = false;
            let pending = checkouts.pending(&path, unexpected("metadata"), || {
                probed = true;
                Ok(false)
            });
            assert!(!pending.unwrap(), "{branch}");
            assert_eq!(probed, !settled, "{branch}");
            assert_eq!(
                matches!(probe, Ok(None)),
                settled,
                "{branch}: {:?}",
                probe.map(|candidate| candidate.is_some())
            );
        }
    }
}
