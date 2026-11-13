//! The objects a publication run reads for each snapshot, and the trees it writes, through one
//! long-lived Git process per repository and kind of request instead of one process per request.
//!
//! Each process starts as [`read`] and [`input`] start theirs: in the same repository, with the
//! local-only inspection setup (no lazy fetch, no transport, replace refs and grafts off). So a
//! request answers what a process of its own would. An object missing here is missing, never
//! fetched. A body past the request's bound, or of another kind than asked, refuses the request.
//! A lone read refuses when Git says anything on stderr, such as an ambiguous name; Git says it
//! while resolving the name, before it answers, so a request refuses when its process has written
//! to stderr by the time the answer starts. After a refused request the process is dropped and the
//! next request starts another, since its place in the protocol is no longer known.
//!
//! Where only an object's ID is wanted, a `cat-file --batch-check` process answers without its
//! body.
//!
//! The processes live as long as the [`Objects`] that started them: in memory, for one run.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Stdio};

const UNAVAILABLE: &str = "privacy source object is unavailable";
/// An answer's header is an object ID, a kind and a decimal size, or a request and `missing`.
const MAX_HEADER: u64 = 64 * 1024;
/// The most a tree may take when it is read whole to list its entries.
const MAX_TREE: usize = crate::domain::storage::MAX_MATERIALIZED_BYTES;

/// One run's long-lived readers and tree writers, by repository.
#[derive(Default)]
pub(super) struct Objects {
    readers: RefCell<HashMap<PathBuf, Process>>,
    checks: RefCell<HashMap<PathBuf, Process>>,
    trees: RefCell<HashMap<PathBuf, Process>>,
    blobs: RefCell<HashMap<PathBuf, crate::commands::plumbing::BlobWriter>>,
}

/// One entry of a tree, its mode canonical as Git's tree walk reports it.
pub(super) struct Entry {
    pub(super) mode: u32,
    pub(super) name: Vec<u8>,
    pub(super) id: String,
}

impl Entry {
    /// The kind of object the mode says the entry names, as Git names it.
    pub(super) fn kind(&self) -> &'static str {
        match self.mode & 0o170000 {
            0o040000 => "tree",
            0o160000 => "commit",
            _ => "blob",
        }
    }

    /// The entry as `git ls-tree -z` lists it, which `git mktree -z` reads back.
    pub(super) fn listing(&self) -> Vec<u8> {
        let mut line = format!("{:06o} {} {}\t", self.mode, self.kind(), self.id).into_bytes();
        line.extend_from_slice(&self.name);
        line.push(0);
        line
    }
}

impl Objects {
    /// The body of the object `spec` names, when it is a `kind` within `limit` bytes; `None` when
    /// `spec` names nothing here.
    pub(super) fn read(
        &self,
        repo: &Repo,
        spec: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .answer(repo, spec, Some((kind, limit)))?
            .map(|(_, body)| body))
    }

    /// The ID of the blob `spec` names, read without its body; `None` when `spec` names nothing
    /// here or names another kind of object.
    pub(super) fn blob_id(&self, repo: &Repo, spec: &str) -> Result<Option<String>> {
        ensure!(!spec.contains('\n'), UNAVAILABLE);
        let mut checks = self.checks.borrow_mut();
        let root = repo.root().to_path_buf();
        let process = match checks.entry(root.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Process::start(repo, &["cat-file", "--batch-check"])?)
            }
        };
        let answered = process
            .send(format!("{spec}\n").into_bytes())
            .and_then(|()| {
                let header = process.header();
                process.quiet()?;
                Ok(header?
                    .filter(|(_, kind, _)| kind == "blob")
                    .map(|(id, _, _)| id))
            });
        if answered.is_err() {
            checks.remove(&root);
        }
        answered
    }

    /// Whether `spec` names an object here, of any kind.
    pub(super) fn exists(&self, repo: &Repo, spec: &str) -> Result<bool> {
        Ok(self.answer(repo, spec, None)?.is_some())
    }

    /// The commit `oid` names, as `git cat-file commit` prints it.
    pub(super) fn commit(&self, repo: &Repo, oid: &str) -> Result<Vec<u8>> {
        self.read(repo, &format!("{oid}^{{commit}}"), "commit", MAX_COMMIT)?
            .context(UNAVAILABLE)
    }

    /// The entries of the tree `spec` names, as `git ls-tree` lists them; `None` when `spec` names
    /// nothing here. An entry Git's tree walk refuses refuses the listing.
    pub(super) fn tree(&self, repo: &Repo, spec: &str) -> Result<Option<Vec<Entry>>> {
        let Some((id, body)) = self.answer(repo, spec, Some(("tree", MAX_TREE)))? else {
            return Ok(None);
        };
        entries(&body, id.len() / 2).map(Some)
    }

    /// The tree `git mktree -z --missing` writes in `repo` for `listing`, entries as
    /// [`Entry::listing`] gives them. Git checks each entry and sorts them as it does for a lone
    /// `mktree`; an entry it refuses refuses the tree.
    pub(super) fn mktree(&self, repo: &Repo, mut listing: Vec<u8>) -> Result<String> {
        let mut trees = self.trees.borrow_mut();
        let root = repo.root().to_path_buf();
        let process = match trees.entry(root.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Process::start(
                repo,
                &["mktree", "-z", "--missing", "--batch"],
            )?),
        };
        // An empty record ends one tree of the batch.
        listing.push(0);
        let written = process.send(listing).and_then(|()| {
            let mut line = Vec::new();
            (&mut process.stdout)
                .take(MAX_HEADER)
                .read_until(b'\n', &mut line)?;
            let oid = std::str::from_utf8(line.strip_suffix(b"\n").unwrap_or_default())?;
            ensure!(valid_oid(oid), "git mktree failed: {}", process.said());
            Ok(oid.to_owned())
        });
        if written.is_err() {
            trees.remove(&root);
        }
        written
    }

    /// The id of `bytes` as a blob in `repo`, written unless it is there already, as
    /// `git hash-object -t blob -w --stdin` writes it.
    pub(super) fn blob(&self, repo: &Repo, bytes: &[u8]) -> Result<String> {
        let mut blobs = self.blobs.borrow_mut();
        let writer = match blobs.entry(repo.root().to_path_buf()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(crate::commands::plumbing::BlobWriter::open(repo)?)
            }
        };
        Ok(writer.write(bytes)?.to_string())
    }

    /// The ID and, when `body` gives a kind and a bound, the body of the object `spec` names.
    fn answer(
        &self,
        repo: &Repo,
        spec: &str,
        body: Option<(&str, usize)>,
    ) -> Result<Option<(String, Vec<u8>)>> {
        ensure!(!spec.contains('\n'), UNAVAILABLE);
        let mut readers = self.readers.borrow_mut();
        let root = repo.root().to_path_buf();
        let process = match readers.entry(root.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Process::start(repo, &["cat-file", "--batch"])?)
            }
        };
        let answered = process
            .send(format!("{spec}\n").into_bytes())
            .and_then(|()| {
                let header = process.header();
                process.quiet()?;
                let Some((id, kind, size)) = header? else {
                    return Ok(None);
                };
                let wanted = body.is_none_or(|(wanted, limit)| {
                    kind == wanted && usize::try_from(size).is_ok_and(|size| size <= limit)
                });
                if !wanted {
                    anyhow::bail!(UNAVAILABLE);
                }
                // An answer whose body isn't asked for is passed over without being held.
                let mut bytes = Vec::new();
                let read = match body {
                    Some(_) => (&mut process.stdout).take(size).read_to_end(&mut bytes)? as u64,
                    None => {
                        std::io::copy(&mut (&mut process.stdout).take(size), &mut std::io::sink())?
                    }
                };
                let mut end = [0u8];
                process.stdout.read_exact(&mut end)?;
                ensure!(
                    read == size && end == *b"\n",
                    "local Git inspection stopped"
                );
                Ok(Some((id, bytes)))
            });
        if answered.is_err() {
            readers.remove(&root);
        }
        answered
    }
}

/// The entries of a raw tree object whose object IDs take `hash_len` bytes, read as Git's tree
/// walk reads them: an octal mode up to a space, a name up to a NUL, then the ID. Modes come out
/// canonical, as the walk reports them; a malformed entry refuses the whole tree.
fn entries(tree: &[u8], hash_len: usize) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut rest = tree;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|byte| *byte == b' ')
            .context("malformed mode in tree entry")?;
        let mut mode = 0u32;
        for digit in &rest[..space] {
            ensure!(
                (b'0'..=b'7').contains(digit),
                "malformed mode in tree entry"
            );
            mode = mode
                .checked_mul(8)
                .and_then(|mode| mode.checked_add(u32::from(digit - b'0')))
                .context("malformed mode in tree entry")?;
        }
        rest = &rest[space + 1..];
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .context("too-short tree object")?;
        ensure!(end > 0, "empty filename in tree entry");
        let name = rest[..end].to_vec();
        rest = &rest[end + 1..];
        ensure!(rest.len() >= hash_len, "too-short tree file");
        let id = rest[..hash_len]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        rest = &rest[hash_len..];
        entries.push(Entry {
            mode: canonical_mode(mode),
            name,
            id,
        });
    }
    Ok(entries)
}

/// The mode Git's tree walk reports for a stored `mode`: a regular file is executable or not by
/// its owner's execute bit, and anything not a file, a link or a directory is a submodule.
fn canonical_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o100000 if mode & 0o100 != 0 => 0o100755,
        0o100000 => 0o100644,
        0o120000 => 0o120000,
        0o040000 => 0o040000,
        _ => 0o160000,
    }
}

/// A long-lived Git process taking requests on stdin and answering on stdout.
struct Process {
    child: Child,
    input: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    stdout: BufReader<ChildStdout>,
    stderr: std::fs::File,
}

impl Process {
    fn start(repo: &Repo, args: &[&str]) -> Result<Self> {
        let stderr = tempfile::tempfile()?;
        let mut child = repo
            .inspection_command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr.try_clone()?)
            .spawn()
            .context("local Git inspection could not start")?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("local Git inspection has no pipes");
        };
        // Requests are written from a thread of their own: a process that stopped reading then
        // fails the write instead of signalling the CLI, and a large request never waits on the
        // answer to itself.
        let (input, requests) = std::sync::mpsc::channel::<Vec<u8>>();
        let writer = std::thread::spawn(move || {
            let mut stdin = stdin;
            if crate::domain::repo::suppress_pipe_signal(&stdin).is_err() {
                return;
            }
            for request in requests {
                if stdin.write_all(&request).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            input: Some(input),
            writer: Some(writer),
            stdout: BufReader::with_capacity(256 * 1024, stdout),
            stderr,
        })
    }

    fn send(&mut self, request: Vec<u8>) -> Result<()> {
        self.input
            .as_ref()
            .context("local Git inspection stopped")?
            .send(request)
            .map_err(|_| anyhow::anyhow!("local Git inspection stopped"))
    }

    /// The next answer's header: the object's ID, kind and size, or `None` when the request names
    /// nothing here.
    fn header(&mut self) -> Result<Option<(String, String, u64)>> {
        let mut line = Vec::new();
        (&mut self.stdout)
            .take(MAX_HEADER)
            .read_until(b'\n', &mut line)?;
        let line = line
            .strip_suffix(b"\n")
            .context("local Git inspection stopped")?;
        let line = std::str::from_utf8(line).context(UNAVAILABLE)?;
        if line.ends_with(" missing") {
            return Ok(None);
        }
        match line.split(' ').collect::<Vec<_>>().as_slice() {
            [id, kind, size] if valid_oid(id) => {
                Ok(Some(((*id).to_owned(), (*kind).to_owned(), size.parse()?)))
            }
            _ => anyhow::bail!(UNAVAILABLE),
        }
    }

    /// Fails once the process has said anything on stderr, as a lone process would have.
    fn quiet(&self) -> Result<()> {
        ensure!(self.stderr.metadata()?.len() == 0, UNAVAILABLE);
        Ok(())
    }

    /// What the process said on stderr, for an error.
    fn said(&self) -> String {
        let mut said = String::new();
        let mut stderr = &self.stderr;
        let _ = std::io::Seek::rewind(&mut stderr);
        let _ = stderr.take(4096).read_to_string(&mut said);
        said.trim().to_owned()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        self.input.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
#[path = "session_objects_tests.rs"]
mod tests;
