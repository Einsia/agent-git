//! Read-only integrity inspection of immutable, locally reachable session history.

use crate::domain::{meta, storage};
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const MAX_ROOTS: usize = 4_096;
const MAX_COMMITS: usize = 10_000;
const MAX_OBJECT_READS: usize = 200_000;
const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_SEQUENCE_EVENTS: usize = 100_000;
const MAX_FINDINGS: usize = 100;
const MAX_ROOT_OUTPUT: usize = 1024 * 1024;
const MAX_TREE_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TAG_DEPTH: usize = 32;
const MAX_HEADER_BYTES: usize = 256;

#[derive(Debug, Default)]
pub(super) struct HistoryReport {
    pub roots: usize,
    pub commits: usize,
    pub views: usize,
    pub file_lines: usize,
    pub undeclared: usize,
    pub incomplete: bool,
    pub findings: Vec<HistoryFinding>,
}

#[derive(Debug)]
pub(super) struct HistoryFinding {
    pub commit: Option<String>,
    pub reference: Option<String>,
    pub message: String,
}

impl HistoryReport {
    fn record(&mut self, commit: Option<&str>, error: Failure) {
        self.incomplete |= error.incomplete;
        if self.findings.len() == MAX_FINDINGS {
            self.incomplete = true;
            return;
        }
        self.findings.push(HistoryFinding {
            commit: commit.map(str::to_owned),
            reference: None,
            message: error.message.into(),
        });
    }
}

#[derive(Clone, Copy, Debug)]
struct Failure {
    message: &'static str,
    incomplete: bool,
}

impl Failure {
    const fn invalid(message: &'static str) -> Self {
        Self {
            message,
            incomplete: false,
        }
    }

    const fn unavailable(message: &'static str) -> Self {
        Self {
            message,
            incomplete: true,
        }
    }
}

type Checked<T> = Result<T, Failure>;

#[derive(Clone, Copy)]
struct Limits {
    commits: usize,
    reads: usize,
    object_bytes: usize,
    total_bytes: usize,
    sequence_events: usize,
    tree_cache_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            commits: MAX_COMMITS,
            reads: MAX_OBJECT_READS,
            object_bytes: MAX_OBJECT_BYTES,
            total_bytes: MAX_TOTAL_BYTES,
            sequence_events: MAX_SEQUENCE_EVENTS,
            tree_cache_bytes: MAX_TREE_CACHE_BYTES,
        }
    }
}

fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(root)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    command
}

fn bounded_output(root: &Path, args: &[&str], cap: usize) -> Checked<(bool, Vec<u8>)> {
    let mut child = command(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| Failure::unavailable("local Git inspection could not start"))?;
    let stderr = child.stderr.take().expect("piped Git diagnostic output");
    let diagnostics = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(MAX_HEADER_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| !bytes.is_empty())
            .unwrap_or(true)
    });
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .ok_or_else(|| Failure::unavailable("local Git output is unavailable"))
        .and_then(|stdout| {
            stdout
                .take(cap as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Failure::unavailable("local Git output could not be read"))?;
            if bytes.len() > cap {
                return Err(Failure::unavailable(
                    "history root enumeration exceeds its byte limit",
                ));
            }
            Ok(())
        });
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let diagnostic = diagnostics.join().unwrap_or(true);
    result?;
    let status = status.map_err(|_| Failure::unavailable("local Git inspection did not finish"))?;
    Ok((status.success() && !diagnostic, bytes))
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug)]
struct FrozenRef {
    name: Vec<u8>,
    oid: String,
}

fn enumerate_refs(
    root: &Path,
    prefixes: &[&str],
    count_limit: usize,
    byte_limit: usize,
) -> Checked<Vec<FrozenRef>> {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend_from_slice(prefixes);
    let (success, bytes) = bounded_output(root, &args, byte_limit)?;
    if !success {
        return Err(Failure::unavailable(
            "history refs could not be enumerated locally",
        ));
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(Failure::unavailable(
            "history ref enumeration is incomplete",
        ));
    }
    let mut refs = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Some(space) = line.iter().position(|byte| *byte == b' ') else {
            return Err(Failure::unavailable("history ref enumeration is malformed"));
        };
        let oid = std::str::from_utf8(&line[..space])
            .map_err(|_| Failure::unavailable("history ref object id is malformed"))?;
        if !valid_oid(oid) {
            return Err(Failure::unavailable("history ref object id is malformed"));
        }
        let name = &line[space + 1..];
        if !prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_bytes()) && name.len() > prefix.len())
        {
            return Err(Failure::unavailable(
                "history ref enumeration returned an unexpected namespace",
            ));
        }
        if refs.len() == count_limit {
            return Err(Failure::unavailable("history root count exceeds its limit"));
        }
        refs.push(FrozenRef {
            name: name.to_vec(),
            oid: oid.to_owned(),
        });
    }
    Ok(refs)
}

/// Branch names remain literal; each accompanying object id is frozen during enumeration.
pub(super) fn local_branches(root: &Path) -> Result<Vec<(String, String)>, String> {
    enumerate_refs(root, &["refs/heads/"], MAX_ROOTS, MAX_ROOT_OUTPUT)
        .and_then(|refs| {
            refs.into_iter()
                .map(|reference| {
                    let name = reference.name.strip_prefix(b"refs/heads/").ok_or_else(|| {
                        Failure::unavailable(
                            "local branch enumeration returned an unexpected namespace",
                        )
                    })?;
                    let name = std::str::from_utf8(name).map_err(|_| {
                        Failure::unavailable("a local branch name is not valid UTF-8")
                    })?;
                    Ok((name.to_owned(), reference.oid))
                })
                .collect()
        })
        .map_err(|error| error.message.to_owned())
}

fn roots(root: &Path) -> Checked<Vec<(String, bool, String)>> {
    let mut roots = enumerate_refs(
        root,
        &["refs/heads/", "refs/remotes/", "refs/tags/"],
        MAX_ROOTS,
        MAX_ROOT_OUTPUT,
    )?
    .into_iter()
    .map(|reference| {
        (
            reference.oid,
            reference.name.starts_with(b"refs/tags/"),
            String::from_utf8_lossy(&reference.name)
                .escape_debug()
                .to_string(),
        )
    })
    .collect::<Vec<_>>();
    let (success, head) = bounded_output(
        root,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        MAX_HEADER_BYTES,
    )?;
    if success {
        let head = std::str::from_utf8(&head)
            .map_err(|_| Failure::unavailable("HEAD object id is malformed"))?
            .trim_end_matches('\n');
        if !valid_oid(head) {
            return Err(Failure::unavailable("HEAD object id is malformed"));
        }
        roots.push((head.into(), false, "HEAD".into()));
    } else {
        let (symbolic, target) =
            bounded_output(root, &["symbolic-ref", "--quiet", "HEAD"], MAX_ROOT_OUTPUT)?;
        if !symbolic || !target.starts_with(b"refs/heads/") {
            return Err(Failure::unavailable(
                "HEAD cannot be read as a commit or an unborn local branch",
            ));
        }
    }
    if roots.len() > MAX_ROOTS {
        return Err(Failure::unavailable("history root count exceeds its limit"));
    }
    Ok(roots)
}

struct ObjectPipe {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl ObjectPipe {
    fn open(root: &Path, mode: &str) -> Checked<Self> {
        let mut child = command(root)
            .args(["cat-file", mode])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| Failure::unavailable("local object reader could not start"))?;
        let input = child.stdin.take().expect("piped object reader input");
        let output = BufReader::new(child.stdout.take().expect("piped object reader output"));
        Ok(Self {
            child,
            input,
            output,
        })
    }

    fn header(&mut self, oid: &str) -> Checked<(String, usize)> {
        writeln!(self.input, "{oid}")
            .and_then(|()| self.input.flush())
            .map_err(|_| {
                Failure::unavailable("local object reader stopped before completing history")
            })?;
        let mut header = Vec::new();
        (&mut self.output)
            .take(MAX_HEADER_BYTES as u64 + 1)
            .read_until(b'\n', &mut header)
            .map_err(|_| Failure::unavailable("local object header could not be read"))?;
        if header.len() > MAX_HEADER_BYTES || !header.ends_with(b"\n") {
            return Err(Failure::unavailable(
                "local object header is incomplete or oversized",
            ));
        }
        let header = std::str::from_utf8(&header)
            .map_err(|_| Failure::unavailable("local object header is malformed"))?;
        let fields: Vec<_> = header.trim_end_matches('\n').split(' ').collect();
        if fields.len() == 2 && fields[1] == "missing" {
            return Err(Failure::unavailable(
                "reachable history objects are unavailable locally",
            ));
        }
        if fields.len() != 3 || fields[0] != oid {
            return Err(Failure::unavailable(
                "local object reader returned an unexpected object",
            ));
        }
        let size = fields[2]
            .parse::<usize>()
            .map_err(|_| Failure::unavailable("local object length is malformed"))?;
        Ok((fields[1].into(), size))
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ObjectPipe {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Objects {
    information: ObjectPipe,
    contents: ObjectPipe,
    limits: Limits,
    reads: usize,
    bytes: usize,
    trees: HashMap<String, CachedTree>,
    tree_bytes: usize,
    parsed_tree_bytes: usize,
    stopped: bool,
}

impl Objects {
    fn open(root: &Path, limits: Limits) -> Checked<Self> {
        Ok(Self {
            information: ObjectPipe::open(root, "--batch-check")?,
            contents: ObjectPipe::open(root, "--batch")?,
            limits,
            reads: 0,
            bytes: 0,
            trees: HashMap::new(),
            tree_bytes: 0,
            parsed_tree_bytes: 0,
            stopped: false,
        })
    }

    fn read(&mut self, oid: &str) -> Checked<(String, Vec<u8>)> {
        if self.stopped {
            return Err(Failure::unavailable(
                "local object inspection stopped before completion",
            ));
        }
        let result = self.read_object(oid);
        if let Err(error) = result
            && error.message != "reachable history objects are unavailable locally"
        {
            self.stopped = true;
            self.information.stop();
            self.contents.stop();
        }
        result
    }

    fn read_object(&mut self, oid: &str) -> Checked<(String, Vec<u8>)> {
        if !valid_oid(oid) {
            return Err(Failure::invalid("history contains a malformed object id"));
        }
        if self.reads == self.limits.reads {
            return Err(Failure::unavailable(
                "history object read count exceeds its limit",
            ));
        }
        self.reads += 1;
        let (kind, size) = self.information.header(oid)?;
        let total = self
            .bytes
            .checked_add(size)
            .ok_or_else(|| Failure::unavailable("history byte budget is exhausted"))?;
        if size > self.limits.object_bytes || total > self.limits.total_bytes {
            return Err(Failure::unavailable(
                "history object bytes exceed the inspection limit",
            ));
        }
        let (body_kind, body_size) = self.contents.header(oid)?;
        if body_kind != kind || body_size != size {
            return Err(Failure::unavailable(
                "local object changed during immutable inspection",
            ));
        }
        self.bytes = total;
        let mut body = vec![0; size];
        self.contents
            .output
            .read_exact(&mut body)
            .map_err(|_| Failure::unavailable("local object body is incomplete"))?;
        let mut delimiter = [0];
        self.contents
            .output
            .read_exact(&mut delimiter)
            .map_err(|_| Failure::unavailable("local object body framing is incomplete"))?;
        if delimiter != *b"\n" {
            return Err(Failure::unavailable(
                "local object body framing is malformed",
            ));
        }
        let actual = match oid.len() {
            40 => object_digest::<sha1::Sha1>(&kind, &body),
            64 => object_digest::<sha2::Sha256>(&kind, &body),
            _ => unreachable!("object id is validated before reading"),
        };
        if actual != oid {
            return Err(Failure::invalid(
                "local object content does not match its object id",
            ));
        }
        Ok((kind, body))
    }

    fn tree_entry(&mut self, tree: &str, name: &[u8]) -> Checked<Option<(String, String)>> {
        if !self.trees.contains_key(tree) {
            let (kind, body) = self.read(tree)?;
            if kind != "tree" {
                return Err(Failure::invalid(
                    "a historical storage directory is not a tree",
                ));
            }
            self.charge_tree_parse(body.len())?;
            let Some(index) = index_tree(&body, tree.len() / 2, self.limits.tree_cache_bytes)?
            else {
                self.charge_tree_parse(body.len())?;
                return parse_tree_entry(&body, tree.len() / 2, name);
            };
            if self.tree_bytes + index.bytes > self.limits.tree_cache_bytes {
                self.trees.clear();
                self.tree_bytes = 0;
            }
            self.tree_bytes += index.bytes;
            self.trees.insert(tree.into(), index);
        }
        match self.trees[tree].entries.get(name) {
            Some(Some((mode, oid))) => Ok(Some((
                std::str::from_utf8(mode)
                    .map_err(|_| Failure::invalid("a historical storage mode is malformed"))?
                    .to_owned(),
                oid.clone(),
            ))),
            Some(None) => Err(Failure::invalid(
                "a historical storage path has duplicate tree entries",
            )),
            None => Ok(None),
        }
    }

    fn charge_tree_parse(&mut self, bytes: usize) -> Checked<()> {
        self.parsed_tree_bytes = self
            .parsed_tree_bytes
            .checked_add(bytes)
            .ok_or_else(|| Failure::unavailable("history tree parsing budget is exhausted"))?;
        if self.parsed_tree_bytes > self.limits.total_bytes {
            return Err(Failure::unavailable(
                "history tree parsing exceeds the inspection limit",
            ));
        }
        Ok(())
    }

    fn blob(&mut self, tree: &str, path: &str) -> Checked<Option<Vec<u8>>> {
        let mut tree = tree.to_owned();
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            let Some((mode, oid)) = self.tree_entry(&tree, component.as_bytes())? else {
                return Ok(None);
            };
            if components.peek().is_some() {
                if mode != "40000" {
                    return Err(Failure::invalid(
                        "a historical storage directory has an invalid mode",
                    ));
                }
                tree = oid;
            } else {
                if !matches!(mode.as_str(), "100644" | "100755") {
                    return Err(Failure::invalid(
                        "a historical storage file is not a regular blob",
                    ));
                }
                let (kind, body) = self.read(&oid)?;
                if kind != "blob" {
                    return Err(Failure::invalid("a historical storage file is not a blob"));
                }
                return Ok(Some(body));
            }
        }
        Err(Failure::invalid("a historical storage path is invalid"))
    }

    fn required_blob(&mut self, tree: &str, path: &str) -> Checked<Vec<u8>> {
        self.blob(tree, path)?.ok_or_else(|| {
            Failure::invalid("a declared session is missing a LOG, VIEW, or referenced event")
        })
    }

    fn metadata(&mut self, tree: &str) -> Checked<Option<meta::Meta>> {
        if self
            .tree_entry(tree, b"session")?
            .is_some_and(|(mode, _)| matches!(mode.as_str(), "100644" | "100755"))
        {
            return Ok(None);
        }
        self.blob(tree, meta::FILE)?
            .map(|bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| Failure::invalid("historical session metadata is not UTF-8"))?;
                meta::parse_strict(text, "historical commit").map_err(|_| {
                    Failure::invalid(
                        "historical session metadata is malformed or violates its invariants",
                    )
                })
            })
            .transpose()
    }
}

fn object_digest<D: Digest>(kind: &str, body: &[u8]) -> String {
    let mut digest = D::new();
    digest.update(format!("{kind} {}\0", body.len()).as_bytes());
    digest.update(body);
    hex::encode(digest.finalize())
}

type CachedEntry = Option<(Vec<u8>, String)>;

struct CachedTree {
    entries: BTreeMap<Vec<u8>, CachedEntry>,
    bytes: usize,
}

fn tree_record<'a>(
    rest: &mut &'a [u8],
    oid_bytes: usize,
) -> Checked<(&'a [u8], &'a [u8], &'a [u8])> {
    let bytes = *rest;
    let space = bytes
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| Failure::invalid("a historical tree entry is malformed"))?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| Failure::invalid("a historical tree entry is malformed"))?;
    if space >= end || bytes.len() < end + 1 + oid_bytes {
        return Err(Failure::invalid("a historical tree entry is truncated"));
    }
    let record = (
        &bytes[..space],
        &bytes[space + 1..end],
        &bytes[end + 1..end + 1 + oid_bytes],
    );
    *rest = &bytes[end + 1 + oid_bytes..];
    Ok(record)
}

fn index_tree(body: &[u8], oid_bytes: usize, cap: usize) -> Checked<Option<CachedTree>> {
    let mut rest = body;
    let mut index = CachedTree {
        entries: BTreeMap::new(),
        bytes: std::mem::size_of::<(String, CachedTree)>()
            + 8 * std::mem::size_of::<usize>()
            + oid_bytes * 2,
    };
    if index.bytes > cap {
        return Ok(None);
    }
    while !rest.is_empty() {
        let (mode, name, oid) = tree_record(&mut rest, oid_bytes)?;
        let entry_bytes = std::mem::size_of::<(Vec<u8>, CachedEntry)>()
            + 8 * std::mem::size_of::<usize>()
            + mode.len()
            + name.len()
            + oid.len() * 2;
        let Some(total) = index
            .bytes
            .checked_add(entry_bytes)
            .filter(|total| *total <= cap)
        else {
            return Ok(None);
        };
        index.bytes = total;
        index
            .entries
            .entry(name.to_vec())
            .and_modify(|entry| *entry = None)
            .or_insert_with(|| Some((mode.to_vec(), hex::encode(oid))));
    }
    Ok(Some(index))
}

fn parse_tree_entry(
    body: &[u8],
    oid_bytes: usize,
    name: &[u8],
) -> Checked<Option<(String, String)>> {
    let mut rest = body;
    let mut found = None;
    while !rest.is_empty() {
        let (mode, entry_name, oid) = tree_record(&mut rest, oid_bytes)?;
        if entry_name == name {
            if found.is_some() {
                return Err(Failure::invalid(
                    "a historical storage path has duplicate tree entries",
                ));
            }
            let mode = std::str::from_utf8(mode)
                .map_err(|_| Failure::invalid("a historical storage mode is malformed"))?;
            found = Some((mode.into(), hex::encode(oid)));
        }
    }
    Ok(found)
}

fn commit_header(body: &[u8], width: usize) -> Checked<(String, Vec<String>)> {
    let mut tree = None;
    let mut parents = Vec::new();
    for line in body
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty())
    {
        let (value, is_tree) = if let Some(value) = line.strip_prefix(b"tree ") {
            (value, true)
        } else if let Some(value) = line.strip_prefix(b"parent ") {
            (value, false)
        } else {
            continue;
        };
        let value = std::str::from_utf8(value)
            .map_err(|_| Failure::unavailable("a historical commit header is malformed"))?;
        if value.len() != width || !valid_oid(value) {
            return Err(Failure::unavailable(
                "a historical commit header has an invalid object id",
            ));
        }
        if is_tree {
            if tree.replace(value.into()).is_some() {
                return Err(Failure::unavailable(
                    "a historical commit has duplicate tree headers",
                ));
            }
        } else {
            if parents.len() == MAX_COMMITS {
                return Err(Failure::unavailable(
                    "a historical commit exceeds the parent limit",
                ));
            }
            parents.push(value.into());
        }
    }
    Ok((
        tree.ok_or_else(|| Failure::unavailable("a historical commit has no tree"))?,
        parents,
    ))
}

fn peel(objects: &mut Objects, oid: &str, tag_allowed: bool) -> Checked<Option<String>> {
    let mut oid = oid.to_owned();
    let mut expected_kind = None;
    for _ in 0..MAX_TAG_DEPTH {
        let (kind, body) = objects.read(&oid)?;
        if expected_kind
            .as_ref()
            .is_some_and(|expected| expected != &kind)
        {
            return Err(Failure::unavailable(
                "a historical tag target type does not match its object",
            ));
        }
        match kind.as_str() {
            "commit" => return Ok(Some(oid)),
            "tag" if tag_allowed => {
                let mut target = None;
                let mut target_kind = None;
                for line in body
                    .split(|byte| *byte == b'\n')
                    .take_while(|line| !line.is_empty())
                {
                    if let Some(value) = line.strip_prefix(b"object ") {
                        let value = std::str::from_utf8(value)
                            .ok()
                            .filter(|value| valid_oid(value) && value.len() == oid.len())
                            .ok_or_else(|| {
                                Failure::unavailable("a historical tag target is malformed")
                            })?;
                        if target.replace(value.to_owned()).is_some() {
                            return Err(Failure::unavailable(
                                "a historical tag has duplicate target headers",
                            ));
                        }
                    } else if let Some(value) = line.strip_prefix(b"type ") {
                        let value = std::str::from_utf8(value)
                            .ok()
                            .filter(|value| matches!(*value, "commit" | "tree" | "blob" | "tag"))
                            .ok_or_else(|| {
                                Failure::unavailable("a historical tag target type is malformed")
                            })?;
                        if target_kind.replace(value.to_owned()).is_some() {
                            return Err(Failure::unavailable(
                                "a historical tag has duplicate type headers",
                            ));
                        }
                    }
                }
                oid =
                    target.ok_or_else(|| Failure::unavailable("a historical tag has no target"))?;
                expected_kind =
                    Some(target_kind.ok_or_else(|| {
                        Failure::unavailable("a historical tag has no target type")
                    })?);
            }
            "tree" | "blob" if tag_allowed => return Ok(None),
            _ => {
                return Err(Failure::unavailable(
                    "a history branch or HEAD does not point to a commit",
                ));
            }
        }
    }
    Err(Failure::unavailable(
        "historical tag peeling exceeds its limit",
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Declaration {
    Absent,
    Session,
    File,
    Unknown,
}

struct CommitState {
    first_parent: Option<String>,
    declaration: Declaration,
}

pub(super) fn check(root: &Path) -> HistoryReport {
    check_with_limits(root, Limits::default())
}

/// Metadata lookup does not require conversation storage or older ancestors to be readable.
pub(super) fn read_metadata(root: &Path, oid: &str) -> Result<Option<meta::Meta>, String> {
    MetadataInspection::open(root)?.metadata(oid)
}

/// Declaration lookup follows the frozen primary lineage and never infers identity from paths.
#[cfg(test)]
pub(super) fn prior_declaration(root: &Path, oid: &str) -> Result<Option<meta::Line>, String> {
    MetadataInspection::open(root)?.prior_declaration(oid)
}

#[cfg(test)]
fn prior_declaration_with_limits(
    root: &Path,
    oid: &str,
    limits: Limits,
) -> Checked<Option<meta::Line>> {
    MetadataInspection::with_limits(root, limits)?.prior_declaration_checked(oid)
}

/// A repository's metadata queries share object, byte, parsing, and traversal budgets.
pub(super) struct MetadataInspection {
    objects: Objects,
    lookups: usize,
}

impl MetadataInspection {
    pub fn open(root: &Path) -> Result<Self, String> {
        Self::with_limits(root, Limits::default()).map_err(|error| error.message.to_owned())
    }

    fn with_limits(root: &Path, limits: Limits) -> Checked<Self> {
        Ok(Self {
            objects: Objects::open(root, limits)?,
            lookups: 0,
        })
    }

    pub fn metadata(&mut self, oid: &str) -> Result<Option<meta::Meta>, String> {
        self.lookup(oid)
            .map(|(snapshot, _)| snapshot)
            .map_err(|error| error.message.to_owned())
    }

    pub fn prior_declaration(&mut self, oid: &str) -> Result<Option<meta::Line>, String> {
        self.prior_declaration_checked(oid)
            .map_err(|error| error.message.to_owned())
    }

    fn lookup(&mut self, oid: &str) -> Checked<(Option<meta::Meta>, Option<String>)> {
        if self.lookups >= self.objects.limits.commits {
            return Err(Failure::unavailable(
                "metadata commit lookups exceed the repository inspection limit",
            ));
        }
        self.lookups += 1;
        commit_metadata(&mut self.objects, oid)
    }

    fn prior_declaration_checked(&mut self, oid: &str) -> Checked<Option<meta::Line>> {
        if !valid_oid(oid) {
            return self
                .lookup(oid)
                .map(|(snapshot, _)| snapshot.map(|snapshot| snapshot.line));
        }
        let mut cursor = Some(oid.to_owned());
        let mut seen = HashSet::new();
        while let Some(oid) = cursor.take() {
            if !seen.insert(oid.clone()) {
                return Err(Failure::unavailable(
                    "primary metadata history contains a cycle",
                ));
            }
            let (snapshot, parent) = self.lookup(&oid)?;
            if let Some(snapshot) = snapshot {
                return Ok(Some(snapshot.line));
            }
            cursor = parent;
        }
        Ok(None)
    }
}

fn commit_metadata(
    objects: &mut Objects,
    oid: &str,
) -> Checked<(Option<meta::Meta>, Option<String>)> {
    let (kind, body) = objects.read(oid)?;
    if kind != "commit" {
        return Err(Failure::unavailable(
            "metadata snapshot object is not a commit",
        ));
    }
    let (tree, parents) = commit_header(&body, oid.len())?;
    Ok((objects.metadata(&tree)?, parents.into_iter().next()))
}

fn check_with_limits(root: &Path, limits: Limits) -> HistoryReport {
    let mut report = HistoryReport::default();
    let roots = match roots(root) {
        Ok(roots) => roots,
        Err(error) => {
            report.record(None, error);
            return report;
        }
    };
    report.roots = roots.len();
    let mut objects = match Objects::open(root, limits) {
        Ok(objects) => objects,
        Err(error) => {
            report.record(None, error);
            return report;
        }
    };
    let mut queue = VecDeque::new();
    let mut queued = HashSet::new();
    let mut references = HashMap::new();
    for (oid, tag_allowed, reference) in roots {
        references
            .entry(oid.clone())
            .or_insert_with(|| reference.clone());
        match peel(&mut objects, &oid, tag_allowed) {
            Ok(Some(commit)) if queued.insert(commit.clone()) => {
                references.entry(commit.clone()).or_insert(reference);
                queue.push_back(commit);
            }
            Ok(_) => {}
            Err(error) => report.record(Some(&oid), error),
        }
    }
    let mut states = BTreeMap::new();
    while let Some(oid) = queue.pop_front() {
        if report.commits == limits.commits {
            report.record(
                None,
                Failure::unavailable("reachable commit count exceeds the inspection limit"),
            );
            break;
        }
        report.commits += 1;
        let header = objects.read(&oid).and_then(|(kind, body)| {
            if kind != "commit" {
                return Err(Failure::unavailable("a historical parent is not a commit"));
            }
            commit_header(&body, oid.len())
        });
        let (tree, parents) = match header {
            Ok(header) => header,
            Err(error) => {
                report.record(Some(&oid), error);
                continue;
            }
        };
        let first_parent = parents.first().cloned();
        for parent in parents {
            if queued.len() >= limits.commits && !queued.contains(&parent) {
                report.record(
                    Some(&oid),
                    Failure::unavailable("reachable commit count exceeds the inspection limit"),
                );
            } else if queued.insert(parent.clone()) {
                if let Some(reference) = references.get(&oid).cloned() {
                    references.insert(parent.clone(), reference);
                }
                queue.push_back(parent);
            }
        }
        let mut declaration = Declaration::Unknown;
        match inspect_snapshot(&mut objects, &tree, &mut report, &mut declaration) {
            Ok(()) => {}
            Err(error) => {
                report.record(Some(&oid), error);
            }
        }
        states.insert(
            oid,
            CommitState {
                first_parent,
                declaration,
            },
        );
        if objects.stopped {
            break;
        }
    }
    report.incomplete |= objects.stopped;
    let mut declared = HashMap::new();
    for (oid, state) in &states {
        if state.declaration != Declaration::Absent {
            continue;
        }
        let mut path = Vec::new();
        let mut cursor = oid;
        let declaration = loop {
            if path.len() > states.len() {
                break Declaration::Unknown;
            }
            if let Some(declaration) = declared.get(cursor) {
                break *declaration;
            }
            let Some(state) = states.get(cursor) else {
                break Declaration::Unknown;
            };
            if state.declaration != Declaration::Absent {
                break state.declaration;
            }
            path.push(cursor.clone());
            let Some(parent) = &state.first_parent else {
                break Declaration::Absent;
            };
            cursor = parent;
        };
        for entry in path {
            declared.insert(entry, declaration);
        }
        if declaration == Declaration::Session {
            report.record(
                Some(oid),
                Failure::invalid(
                    "session/meta.json is missing after the primary lineage declares a session",
                ),
            );
        } else if declaration == Declaration::File {
            report.record(
                Some(oid),
                Failure::invalid(
                    "session/meta.json is missing after the primary lineage declares a file line",
                ),
            );
        } else if declaration == Declaration::Unknown {
            report.record(
                Some(oid),
                Failure::unavailable(
                    "missing metadata cannot be classified because primary history is unavailable",
                ),
            );
        } else {
            report.undeclared += 1;
        }
    }
    for finding in &mut report.findings {
        finding.reference = finding
            .commit
            .as_ref()
            .and_then(|oid| references.get(oid))
            .cloned();
    }
    report
}

fn inspect_snapshot(
    objects: &mut Objects,
    tree: &str,
    report: &mut HistoryReport,
    declaration: &mut Declaration,
) -> Checked<()> {
    let Some(snapshot) = objects.metadata(tree)? else {
        *declaration = Declaration::Absent;
        return Ok(());
    };
    if snapshot.is_file_line() {
        report.file_lines += 1;
        *declaration = Declaration::File;
        return Ok(());
    }
    *declaration = Declaration::Session;
    match snapshot.layout {
        meta::LayoutVersion::V0 => check_legacy(objects, tree, false)?,
        meta::LayoutVersion::V1 => check_current(objects, tree, false)?,
    };
    report.views += 1;
    Ok(())
}

fn text(bytes: &[u8]) -> Checked<&str> {
    std::str::from_utf8(bytes)
        .map_err(|_| Failure::invalid("historical session storage is not UTF-8"))
}

struct Content {
    log: String,
    view: String,
}

fn check_legacy(objects: &mut Objects, tree: &str, include_log: bool) -> Checked<Content> {
    let log = objects.required_blob(tree, meta::LEGACY_LOG_FILE)?;
    let view = objects.required_blob(tree, meta::LEGACY_VIEW_FILE)?;
    let mut reachable = HashSet::new();
    let mut canonical_log = String::new();
    for (index, line) in text(&log)?.split_inclusive('\n').enumerate() {
        if index == objects.limits.sequence_events {
            return Err(Failure::unavailable(
                "historical sequence length exceeds the inspection limit",
            ));
        }
        let envelope = storage::parse_legacy_envelope_line(line)
            .map_err(|_| Failure::invalid("historical LOG contains an invalid envelope"))?;
        let canonical = storage::envelope_line(&envelope);
        reachable.insert(
            storage::event_id(&canonical)
                .map_err(|_| Failure::invalid("historical LOG contains an invalid event"))?,
        );
        if include_log {
            append_view(&mut canonical_log, &canonical, objects.limits.object_bytes)?;
        }
    }
    let mut canonical_view = String::new();
    for (index, line) in text(&view)?.split_inclusive('\n').enumerate() {
        if index == objects.limits.sequence_events {
            return Err(Failure::unavailable(
                "historical sequence length exceeds the inspection limit",
            ));
        }
        let envelope = storage::parse_legacy_envelope_line(line)
            .map_err(|_| Failure::invalid("historical VIEW contains an invalid envelope"))?;
        let canonical = storage::envelope_line(&envelope);
        let id = storage::event_id(&canonical)
            .map_err(|_| Failure::invalid("historical VIEW contains an invalid event"))?;
        if !reachable.contains(&id) && !legacy_view_only_record(&envelope.content) {
            return Err(Failure::invalid(
                "historical VIEW references an event absent from its LOG",
            ));
        }
        append_view(&mut canonical_view, &canonical, objects.limits.object_bytes)?;
    }
    check_markers(&canonical_view)?;
    Ok(Content {
        log: canonical_log,
        view: canonical_view,
    })
}

pub(super) fn legacy_view_only_record(content: &serde_json::Value) -> bool {
    let Some(content) = content.as_object().filter(|content| content.len() == 3) else {
        return false;
    };
    match content.get("type").and_then(serde_json::Value::as_str) {
        Some("system") => {
            matches!(
                content.get("subtype").and_then(serde_json::Value::as_str),
                Some(
                    "agit:__merge_start__"
                        | "agit:__merge_end__"
                        | "agit:__cherry_pick_start__"
                        | "agit:__cherry_pick_end__"
                )
            ) && content
                .get("source")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|source| !source.is_empty())
        }
        Some("user") => {
            content.get("agit").and_then(serde_json::Value::as_str) == Some("merge_summary")
                && content
                    .get("message")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|message| {
                        message.len() == 2
                            && message.get("role").and_then(serde_json::Value::as_str)
                                == Some("user")
                            && message
                                .get("content")
                                .and_then(serde_json::Value::as_str)
                                .is_some()
                    })
        }
        _ => false,
    }
}

fn sequence(bytes: &[u8], limit: usize) -> Checked<Vec<String>> {
    if bytes.iter().filter(|byte| **byte == b'\n').count() > limit {
        return Err(Failure::unavailable(
            "historical sequence length exceeds the inspection limit",
        ));
    }
    storage::parse_sequence(text(bytes)?)
        .map_err(|_| Failure::invalid("historical LOG or VIEW has an invalid event sequence"))
}

fn check_current(objects: &mut Objects, tree: &str, include_log: bool) -> Checked<Content> {
    let log = objects.required_blob(tree, meta::LOG_FILE)?;
    let log = sequence(&log, objects.limits.sequence_events)?;
    let view = objects.required_blob(tree, meta::VIEW_FILE)?;
    let view = sequence(&view, objects.limits.sequence_events)?;
    let reachable: HashSet<_> = log.iter().collect();
    if view.iter().any(|id| !reachable.contains(id)) {
        return Err(Failure::invalid(
            "historical VIEW references an event absent from its LOG",
        ));
    }
    let mut verified = HashSet::new();
    let mut canonical_log = String::new();
    for id in &log {
        if verified.insert(id) || include_log {
            let body = event(objects, tree, id)?;
            if include_log {
                append_view(
                    &mut canonical_log,
                    text(&body)?,
                    objects.limits.object_bytes,
                )?;
            }
        }
    }
    let mut canonical_view = String::new();
    for id in &view {
        let body = event(objects, tree, id)?;
        append_view(
            &mut canonical_view,
            text(&body)?,
            objects.limits.object_bytes,
        )?;
    }
    check_markers(&canonical_view)?;
    Ok(Content {
        log: canonical_log,
        view: canonical_view,
    })
}

pub(super) struct StoredSnapshot {
    pub meta: meta::Meta,
    pub log: String,
    pub view: String,
}

/// Snapshot reads use frozen object ids and never hydrate missing promisor objects.
pub(super) fn read_snapshot(root: &Path, oid: &str) -> Result<StoredSnapshot, String> {
    read_snapshot_checked(root, oid).map_err(|error| error.message.to_owned())
}

fn read_snapshot_checked(root: &Path, oid: &str) -> Checked<StoredSnapshot> {
    let mut objects = Objects::open(root, Limits::default())?;
    let (kind, bytes) = objects.read(oid)?;
    if kind != "commit" {
        return Err(Failure::unavailable("snapshot object is not a commit"));
    }
    let (tree, _) = commit_header(&bytes, oid.len())?;
    let snapshot = objects
        .metadata(&tree)?
        .ok_or_else(|| Failure::invalid("snapshot has no session metadata"))?;
    let content = if snapshot.is_file_line() {
        Content {
            log: String::new(),
            view: String::new(),
        }
    } else {
        match snapshot.layout {
            meta::LayoutVersion::V0 => check_legacy(&mut objects, &tree, true)?,
            meta::LayoutVersion::V1 => check_current(&mut objects, &tree, true)?,
        }
    };
    Ok(StoredSnapshot {
        meta: snapshot,
        log: content.log,
        view: content.view,
    })
}

fn event(objects: &mut Objects, tree: &str, id: &str) -> Checked<Vec<u8>> {
    let path = meta::event_path(id)
        .map_err(|_| Failure::invalid("historical storage references an invalid event id"))?;
    let body = objects.required_blob(tree, &path)?;
    let actual = storage::event_id(text(&body)?)
        .map_err(|_| Failure::invalid("historical storage contains an invalid event envelope"))?;
    if actual != id {
        return Err(Failure::invalid(
            "historical event bytes do not match their content address",
        ));
    }
    Ok(body)
}

fn append_view(view: &mut String, line: &str, cap: usize) -> Checked<()> {
    if view
        .len()
        .checked_add(line.len())
        .is_none_or(|size| size > cap)
    {
        return Err(Failure::unavailable(
            "historical VIEW expansion exceeds the inspection limit",
        ));
    }
    view.push_str(line);
    Ok(())
}

fn check_markers(view: &str) -> Checked<()> {
    match storage::unbalanced_view_markers(view) {
        Ok(0) => Ok(()),
        Ok(_) => Err(Failure::invalid(
            "historical VIEW has unbalanced merge or cherry-pick markers",
        )),
        Err(_) => Err(Failure::invalid("historical VIEW marker validation failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repo::Repo;
    use crate::domain::transcript;
    use std::fs;
    use std::path::PathBuf;

    struct Lab {
        _directory: tempfile::TempDir,
        repo: Repo,
    }

    impl Lab {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repo")).unwrap();
            Self {
                _directory: directory,
                repo,
            }
        }

        fn record(&self) -> String {
            self.repo.add_all().unwrap();
            self.repo.commit("synthetic history").unwrap();
            self.repo.git(&["rev-parse", "HEAD"]).unwrap()
        }

        fn file_line(&self) -> String {
            meta::write(self.repo.root(), &meta::Meta::new_file_line()).unwrap();
            self.record()
        }

        fn session(&self, raw: &str) -> String {
            let metadata = meta::Meta::new(
                format!("agit-{}", "a".repeat(40)),
                "claude-code".into(),
                "/synthetic".into(),
            );
            meta::write(self.repo.root(), &metadata).unwrap();
            let log = transcript::wrap_lines(raw, &metadata.runtime, &metadata.session);
            storage::write_snapshot(self.repo.root(), &log, &log).unwrap();
            self.record()
        }

        fn broken_view(&self) -> String {
            fs::write(self.repo.root().join(meta::VIEW_FILE), "invalid sequence\n").unwrap();
            self.record()
        }

        fn legacy_session(&self, log: &str, view: &str) -> String {
            let mut metadata = meta::Meta::new(
                format!("agit-{}", "a".repeat(40)),
                "claude-code".into(),
                "/synthetic".into(),
            );
            metadata.layout = meta::LayoutVersion::V0;
            meta::write(self.repo.root(), &metadata).unwrap();
            fs::write(self.repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap();
            fs::write(self.repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
            self.record()
        }

        fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
            let mut files: Vec<_> = walkdir::WalkDir::new(self.repo.root())
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| (entry.path().to_path_buf(), fs::read(entry.path()).unwrap()))
                .collect();
            files.sort();
            files
        }
    }

    #[test]
    fn healthy_tip_does_not_hide_a_broken_ancestor() {
        let lab = Lab::new();
        lab.file_line();
        lab.session("{\"message\":\"hello\"}\n");
        let broken = lab.broken_view();
        lab.session("{\"message\":\"restored\"}\n");
        let before = lab.files();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.commits, 4);
        assert_eq!(report.views, 2);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].commit.as_deref(), Some(broken.as_str()));
        assert!(
            report.findings[0]
                .message
                .contains("invalid event sequence")
        );
        assert_eq!(before, lab.files());
    }

    #[test]
    fn remote_tags_detached_head_and_merge_parents_are_frozen_roots() {
        let lab = Lab::new();
        let file = lab.file_line();
        let session = lab.session("{\"message\":\"hello\"}\n");
        let broken = lab.broken_view();
        lab.repo
            .git(&["update-ref", "refs/remotes/origin/review", &broken])
            .unwrap();
        lab.repo
            .git(&["tag", "-a", "historical", "-m", "synthetic tag", &broken])
            .unwrap();
        lab.repo.git(&["checkout", "--detach", &session]).unwrap();
        let tree = lab
            .repo
            .git(&["rev-parse", &format!("{file}^{{tree}}")])
            .unwrap();
        let merge = lab
            .repo
            .git(&[
                "commit-tree",
                &tree,
                "-p",
                &file,
                "-p",
                &broken,
                "-m",
                "synthetic merge",
            ])
            .unwrap();
        lab.repo
            .git(&["update-ref", "refs/heads/main", &merge])
            .unwrap();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.commits, 4);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].commit.as_deref(), Some(broken.as_str()));
        assert_eq!(report.file_lines, 2);
    }

    #[test]
    fn removed_metadata_retains_the_primary_declaration_even_after_a_bad_view() {
        let lab = Lab::new();
        fs::write(lab.repo.root().join("shared"), "undeclared\n").unwrap();
        lab.record();
        lab.session("{\"message\":\"hello\"}\n");
        lab.broken_view();
        fs::remove_file(lab.repo.root().join(meta::FILE)).unwrap();
        let missing = lab.record();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.findings.len(), 2);
        assert!(
            report
                .findings
                .iter()
                .any(
                    |finding| finding.commit.as_deref() == Some(missing.as_str())
                        && finding.message.contains("missing after")
                )
        );
    }

    #[test]
    fn undeclared_paths_and_file_first_parent_do_not_invent_a_session() {
        let lab = Lab::new();
        fs::create_dir_all(lab.repo.root().join("session")).unwrap();
        fs::write(
            lab.repo.root().join(meta::LEGACY_LOG_FILE),
            "ordinary user file",
        )
        .unwrap();
        let undeclared = lab.record();
        let session = lab.session("{\"message\":\"hello\"}\n");
        let tree = lab
            .repo
            .git(&["rev-parse", &format!("{undeclared}^{{tree}}")])
            .unwrap();
        let merge = lab
            .repo
            .git(&[
                "commit-tree",
                &tree,
                "-p",
                &undeclared,
                "-p",
                &session,
                "-m",
                "undeclared primary lineage",
            ])
            .unwrap();
        lab.repo
            .git(&["update-ref", "refs/heads/main", &merge])
            .unwrap();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.undeclared, 2);
        assert_eq!(report.views, 1);
    }

    #[test]
    fn file_line_metadata_loss_is_distinct_from_predeclaration_history() {
        let lab = Lab::new();
        fs::write(lab.repo.root().join("shared"), "undeclared\n").unwrap();
        lab.record();
        lab.file_line();
        fs::remove_file(lab.repo.root().join(meta::FILE)).unwrap();
        let missing = lab.record();
        lab.file_line();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.undeclared, 1);
        assert_eq!(report.file_lines, 2);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].commit.as_deref(), Some(missing.as_str()));
        assert!(report.findings[0].message.contains("declares a file line"));
    }

    #[test]
    fn legacy_field_order_and_mixed_session_envelopes_are_valid() {
        let lab = Lab::new();
        let mut metadata = meta::Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "claude-code".into(),
            "/synthetic".into(),
        );
        metadata.layout = meta::LayoutVersion::V0;
        meta::write(lab.repo.root(), &metadata).unwrap();
        let envelope = transcript::wrap_lines(
            "{\"message\":\"hello\"}\n",
            "codex",
            &format!("agit-{}", "b".repeat(40)),
        );
        let reordered = format!(
            "{}\n",
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&envelope).unwrap())
                .unwrap()
        );
        fs::write(lab.repo.root().join(meta::LEGACY_LOG_FILE), &reordered).unwrap();
        fs::write(lab.repo.root().join(meta::LEGACY_VIEW_FILE), &reordered).unwrap();
        let head = lab.record();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.views, 1);
        let snapshot = read_snapshot(lab.repo.root(), &head).unwrap();
        assert_eq!(snapshot.log, envelope);
        assert_eq!(snapshot.view, envelope);
        assert_eq!(snapshot.meta.layout, meta::LayoutVersion::V0);
    }

    #[test]
    fn legacy_view_only_synthetics_remain_valid_beneath_a_migrated_tip() {
        use crate::commands::merge::{marker_envelope, summary_envelope};
        for kind in ["merge", "cherry_pick"] {
            let lab = Lab::new();
            let session = format!("agit-{}", "a".repeat(40));
            let log = transcript::wrap_lines("{\"message\":\"hello\"}\n", "claude-code", &session);
            let start = marker_envelope(
                &format!("__{kind}_start__"),
                "claude-code",
                &session,
                "source#1",
            );
            let end = marker_envelope(
                &format!("__{kind}_end__"),
                "claude-code",
                &session,
                "source#1",
            );
            let summary = summary_envelope("Synthetic summary", "claude-code", &session);
            let view = format!("{start}{log}{summary}{end}");
            let old = lab.legacy_session(&log, &view);
            let before = lab.files();
            let stored = read_snapshot(lab.repo.root(), &old).unwrap();
            assert_eq!(stored.log, log);
            assert_eq!(stored.view, view);
            assert_eq!(
                super::super::check_view(lab.repo.root()).unwrap(),
                Some(super::super::ViewNote::Ok)
            );
            let by_ref = super::super::SessionRoot::Ref {
                repo: lab.repo.root().to_path_buf(),
                branch: "main".into(),
            };
            assert_eq!(
                super::super::check_view_root(&by_ref).unwrap(),
                Some(super::super::ViewNote::Ok)
            );
            assert!(check(lab.repo.root()).findings.is_empty());
            assert_eq!(before, lab.files());
            assert_eq!(
                crate::commands::migration::migrate_repo(&lab.repo).unwrap(),
                1
            );
            assert_eq!(
                meta::resolve(lab.repo.root()).unwrap().layout,
                meta::LayoutVersion::V1
            );
            let before = lab.files();
            let report = check(lab.repo.root());
            assert!(!report.incomplete, "{report:?}");
            assert!(report.findings.is_empty(), "{report:?}");
            assert_eq!(report.views, 2);
            assert_eq!(read_snapshot(lab.repo.root(), &old).unwrap().view, view);
            assert_eq!(before, lab.files());
        }
    }

    #[test]
    fn legacy_view_exemptions_require_closed_shapes_and_balanced_markers() {
        use crate::commands::merge::{marker_envelope, summary_envelope};
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines("{\"message\":\"hello\"}\n", "claude-code", &session);
        let start = marker_envelope("__merge_start__", "claude-code", &session, "source#1");
        let end = marker_envelope("__merge_end__", "claude-code", &session, "source#1");
        let wrong_end = marker_envelope("__merge_end__", "claude-code", &session, "other#1");
        let summary = summary_envelope("Synthetic summary", "claude-code", &session);
        let extra = |line: &str| {
            let mut envelope = storage::parse_envelope_line(line).unwrap();
            envelope.content["extra"] = serde_json::json!("unreachable real content");
            envelope.object_hash = transcript::object_hash(&envelope.content);
            storage::envelope_line(&envelope)
        };
        let real =
            transcript::wrap_lines("{\"message\":\"not in LOG\"}\n", "claude-code", &session);
        for (view, message) in [
            (format!("{log}{real}"), "absent from its LOG"),
            (
                format!("{}{log}{end}", extra(&start)),
                "absent from its LOG",
            ),
            (
                format!("{start}{log}{}{end}", extra(&summary)),
                "absent from its LOG",
            ),
            (format!("{start}{log}"), "unbalanced"),
            (format!("{start}{log}{wrong_end}"), "unbalanced"),
            (format!("{end}{log}{start}"), "unbalanced"),
        ] {
            let lab = Lab::new();
            let head = lab.legacy_session(&log, &view);
            let before = lab.files();
            let report = check(lab.repo.root());
            assert!(!report.incomplete, "{report:?}");
            assert_eq!(report.findings.len(), 1, "{report:?}");
            assert_eq!(report.findings[0].commit.as_deref(), Some(head.as_str()));
            assert!(report.findings[0].message.contains(message), "{report:?}");
            assert!(read_snapshot(lab.repo.root(), &head).is_err());
            assert_ne!(
                super::super::check_view(lab.repo.root()).unwrap(),
                Some(super::super::ViewNote::Ok)
            );
            assert_eq!(before, lab.files());
        }
    }

    #[test]
    fn current_layout_still_requires_synthetic_records_in_log() {
        use crate::commands::merge::summary_envelope;
        let lab = Lab::new();
        let session = format!("agit-{}", "a".repeat(40));
        let real = transcript::wrap_lines("{\"message\":\"hello\"}\n", "claude-code", &session);
        let synthetic = summary_envelope("Synthetic summary", "claude-code", &session);
        lab.session("{\"message\":\"hello\"}\n");
        let all = format!("{real}{synthetic}");
        storage::write_snapshot(lab.repo.root(), &all, &all).unwrap();
        fs::write(
            lab.repo.root().join(meta::LOG_FILE),
            format!("{}\n", storage::event_id(&real).unwrap()),
        )
        .unwrap();
        let head = lab.record();
        let before = lab.files();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.commit.as_deref() == Some(head.as_str())
                    && finding.message.contains("absent from its LOG")),
            "{report:?}"
        );
        assert!(read_snapshot(lab.repo.root(), &head).is_err());
        assert_eq!(before, lab.files());
    }

    #[test]
    fn content_addresses_reachability_and_marker_nesting_are_validated() {
        for case in ["hash", "unreachable", "marker"] {
            let lab = Lab::new();
            lab.session("{\"message\":\"hello\"}\n");
            match case {
                "hash" => {
                    let id = fs::read_to_string(lab.repo.root().join(meta::LOG_FILE)).unwrap();
                    fs::write(
                        lab.repo.root().join(meta::event_path(id.trim()).unwrap()),
                        "{}\n",
                    )
                    .unwrap();
                }
                "unreachable" => {
                    fs::write(
                        lab.repo.root().join(meta::VIEW_FILE),
                        format!("{}\n", "b".repeat(40)),
                    )
                    .unwrap();
                }
                _ => {
                    let log = transcript::wrap_lines(
                        "{\"subtype\":\"agit:__merge_start__\",\"source\":\"alice/source\"}\n",
                        "claude-code",
                        &format!("agit-{}", "a".repeat(40)),
                    );
                    storage::write_snapshot(lab.repo.root(), &log, &log).unwrap();
                }
            }
            let head = lab.record();
            let report = check(lab.repo.root());
            assert!(!report.incomplete, "{case}: {report:?}");
            assert_eq!(report.findings.len(), 1, "{case}: {report:?}");
            assert_eq!(report.findings[0].commit.as_deref(), Some(head.as_str()));
        }
    }

    #[test]
    fn local_replacement_refs_cannot_hide_the_bad_snapshot() {
        let lab = Lab::new();
        let healthy = lab.session("{\"message\":\"hello\"}\n");
        let broken = lab.broken_view();
        lab.repo
            .git(&["update-ref", &format!("refs/replace/{broken}"), &healthy])
            .unwrap();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].commit.as_deref(), Some(broken.as_str()));
    }

    #[test]
    fn limits_and_missing_history_are_explicitly_incomplete() {
        let lab = Lab::new();
        let first = lab.file_line();
        lab.session("{\"message\":\"hello\"}\n");
        for limits in [
            Limits {
                commits: 1,
                ..Limits::default()
            },
            Limits {
                reads: 1,
                ..Limits::default()
            },
            Limits {
                object_bytes: 1,
                ..Limits::default()
            },
            Limits {
                total_bytes: 1,
                ..Limits::default()
            },
            Limits {
                sequence_events: 0,
                ..Limits::default()
            },
        ] {
            let report = check_with_limits(lab.repo.root(), limits);
            assert!(report.incomplete, "{report:?}");
            assert!(!report.findings.is_empty(), "{report:?}");
        }
        fs::remove_file(
            lab.repo
                .root()
                .join(".git/objects")
                .join(&first[..2])
                .join(&first[2..]),
        )
        .unwrap();
        let report = check(lab.repo.root());
        assert!(report.incomplete, "{report:?}");
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.message.contains("unavailable locally"))
        );
    }

    #[test]
    fn non_commit_tags_are_skipped_but_storage_symlinks_are_invalid() {
        let lab = Lab::new();
        lab.session("{\"message\":\"hello\"}\n");
        let blob = lab.repo.git(&["rev-parse", "HEAD:VIEW"]).unwrap();
        lab.repo
            .git(&["update-ref", "refs/tags/shared-blob", &blob])
            .unwrap();
        assert!(check(lab.repo.root()).findings.is_empty());
        lab.repo
            .git(&[
                "update-index",
                "--cacheinfo",
                &format!("120000,{blob},VIEW"),
            ])
            .unwrap();
        lab.repo.commit("nonregular historical view").unwrap();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].message.contains("not a regular blob"));
    }

    #[test]
    fn empty_repository_is_a_complete_empty_history() {
        let lab = Lab::new();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.commits, 0);
    }

    #[test]
    fn ref_enumeration_warnings_cannot_hide_an_unreadable_root() {
        let lab = Lab::new();
        lab.file_line();
        fs::write(
            lab.repo.root().join(".git/refs/heads/broken"),
            "not an object id\n",
        )
        .unwrap();
        let report = check(lab.repo.root());
        assert!(report.incomplete, "{report:?}");
        assert!(!report.findings.is_empty(), "{report:?}");
    }

    #[test]
    fn declared_newborn_sessions_validate_their_committed_storage() {
        let lab = Lab::new();
        meta::write(
            lab.repo.root(),
            &meta::Meta::new_session_line("codex".into(), "/synthetic".into()),
        )
        .unwrap();
        storage::write_snapshot(lab.repo.root(), "", "").unwrap();
        lab.record();
        let healthy = check(lab.repo.root());
        assert!(!healthy.incomplete, "{healthy:?}");
        assert!(healthy.findings.is_empty(), "{healthy:?}");
        assert_eq!(healthy.views, 1);
        let broken = lab.broken_view();
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].commit.as_deref(), Some(broken.as_str()));
        assert!(read_snapshot(lab.repo.root(), &broken).is_err());
    }

    #[test]
    fn duplicate_tree_paths_are_rejected_for_either_object_width() {
        for width in [20, 32] {
            let mut body = b"100644 VIEW\0".to_vec();
            body.extend(vec![1; width]);
            let duplicate = body.clone();
            body.extend(duplicate);
            assert!(parse_tree_entry(&body, width, b"VIEW").is_err());
        }
    }

    #[test]
    fn partial_clone_missing_blobs_are_not_fetched_during_inspection() {
        let source = Lab::new();
        source.session("{\"message\":\"hello\"}\n");
        source
            .repo
            .git(&["config", "uploadpack.allowFilter", "true"])
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let clone = directory.path().join("partial");
        let output = Command::new("git")
            .args(["clone", "--no-local", "--filter=blob:none", "--no-checkout"])
            .arg(source.repo.root())
            .arg(&clone)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let files = || {
            let mut files: Vec<_> = walkdir::WalkDir::new(clone.join(".git/objects"))
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| (entry.path().to_path_buf(), fs::read(entry.path()).unwrap()))
                .collect();
            files.sort();
            files
        };
        let before = files();
        let report = check(&clone);
        assert!(report.incomplete, "{report:?}");
        assert_eq!(before, files());
        // A blocked promisor fetch can end cat-file before it emits a missing-object header.
        assert!(
            report.findings.iter().any(|finding| matches!(
                finding.message.as_str(),
                "reachable history objects are unavailable locally"
                    | "local object header is incomplete or oversized"
            )),
            "{report:?}"
        );
        let output = Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(["show", "HEAD:session/meta.json"])
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_NO_LAZY_FETCH", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_ne!(
            before,
            files(),
            "the positive control must actually fetch the missing blob"
        );
    }

    #[test]
    fn sha256_repository_history_uses_its_native_tree_object_width() {
        let lab = object_lab("sha256");
        lab.file_line();
        let head = lab.session("{\"message\":\"hello\"}\n");
        assert_eq!(head.len(), 64);
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.commits, 2);
        assert_eq!(report.views, 1);
    }

    #[test]
    fn metadata_queries_use_frozen_tips_and_do_not_read_conversation_or_older_history() {
        let lab = Lab::new();
        let file = lab.file_line();
        let session = lab.session("{\"message\":\"hello\"}\n");
        assert_eq!(
            read_metadata(lab.repo.root(), &file).unwrap().unwrap().line,
            meta::Line::File
        );
        assert_eq!(
            prior_declaration(lab.repo.root(), &session).unwrap(),
            Some(meta::Line::Session)
        );
        let log_blob = lab.repo.git(&["rev-parse", "HEAD:LOG"]).unwrap();
        for oid in [&file, &log_blob] {
            fs::remove_file(
                lab.repo
                    .root()
                    .join(".git/objects")
                    .join(&oid[..2])
                    .join(&oid[2..]),
            )
            .unwrap();
        }
        assert_eq!(
            prior_declaration(lab.repo.root(), &session).unwrap(),
            Some(meta::Line::Session)
        );
        assert!(read_snapshot(lab.repo.root(), &session).is_err());
        assert!(read_metadata(lab.repo.root(), "HEAD").is_err());
    }

    #[test]
    fn metadata_lookup_follows_the_primary_parent_and_stops_at_nearest_declaration() {
        let lab = Lab::new();
        fs::write(lab.repo.root().join("shared"), "undeclared\n").unwrap();
        let undeclared = lab.record();
        let session = lab.session("{\"message\":\"hello\"}\n");
        let tree = lab
            .repo
            .git(&["rev-parse", &format!("{undeclared}^{{tree}}")])
            .unwrap();
        let merge = lab
            .repo
            .git(&[
                "commit-tree",
                &tree,
                "-p",
                &undeclared,
                "-p",
                &session,
                "-m",
                "primary undeclared history",
            ])
            .unwrap();
        assert_eq!(prior_declaration(lab.repo.root(), &merge).unwrap(), None);
        lab.file_line();
        fs::remove_file(lab.repo.root().join(meta::FILE)).unwrap();
        let missing = lab.record();
        assert_eq!(
            prior_declaration(lab.repo.root(), &missing).unwrap(),
            Some(meta::Line::File)
        );
        fs::write(
            lab.repo.root().join(meta::FILE),
            "malformed private sentinel",
        )
        .unwrap();
        let malformed = lab.record();
        let error = prior_declaration(lab.repo.root(), &malformed).unwrap_err();
        assert!(error.contains("malformed"));
        assert!(!error.contains("private sentinel"));
    }

    #[test]
    fn ordinary_session_files_are_absent_metadata_until_a_line_is_declared() {
        let lab = Lab::new();
        fs::write(lab.repo.root().join("session"), "ordinary user file\n").unwrap();
        let undeclared = lab.record();
        assert_eq!(
            prior_declaration(lab.repo.root(), &undeclared).unwrap(),
            None
        );
        assert!(
            read_metadata(lab.repo.root(), &undeclared)
                .unwrap()
                .is_none()
        );
        let report = check(lab.repo.root());
        assert!(!report.incomplete, "{report:?}");
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.undeclared, 1);
        fs::remove_file(lab.repo.root().join("session")).unwrap();
        lab.session("{\"message\":\"hello\"}\n");
        fs::remove_dir_all(lab.repo.root().join("session")).unwrap();
        fs::write(lab.repo.root().join("session"), "ordinary user file\n").unwrap();
        let missing = lab.record();
        assert_eq!(
            prior_declaration(lab.repo.root(), &missing).unwrap(),
            Some(meta::Line::Session)
        );
        let report = check(lab.repo.root());
        assert!(
            report
                .findings
                .iter()
                .any(
                    |finding| finding.commit.as_deref() == Some(missing.as_str())
                        && finding.message.contains("missing after")
                )
        );
    }

    #[test]
    fn metadata_history_limits_and_missing_objects_cannot_become_absence() {
        let lab = Lab::new();
        let file = lab.file_line();
        fs::remove_file(lab.repo.root().join(meta::FILE)).unwrap();
        let missing = lab.record();
        for limits in [
            Limits {
                commits: 1,
                ..Limits::default()
            },
            Limits {
                reads: 0,
                ..Limits::default()
            },
            Limits {
                object_bytes: 1,
                ..Limits::default()
            },
            Limits {
                total_bytes: 1,
                ..Limits::default()
            },
        ] {
            let error =
                prior_declaration_with_limits(lab.repo.root(), &missing, limits).unwrap_err();
            assert!(error.incomplete, "{error:?}");
        }
        let metadata_blob = lab
            .repo
            .git(&["rev-parse", &format!("{file}:{}", meta::FILE)])
            .unwrap();
        fs::remove_file(
            lab.repo
                .root()
                .join(".git/objects")
                .join(&metadata_blob[..2])
                .join(&metadata_blob[2..]),
        )
        .unwrap();
        let error = prior_declaration(lab.repo.root(), &missing).unwrap_err();
        assert!(error.contains("unavailable locally"), "{error}");
    }

    #[test]
    fn oversized_valid_metadata_is_rejected_before_body_reading() {
        let lab = Lab::new();
        let mut metadata = meta::Meta::new_file_line();
        metadata.cwd = "private metadata sentinel".repeat(512);
        meta::write(lab.repo.root(), &metadata).unwrap();
        let head = lab.record();
        let error = prior_declaration_with_limits(
            lab.repo.root(),
            &head,
            Limits {
                object_bytes: 1024,
                ..Limits::default()
            },
        )
        .unwrap_err();
        assert!(error.incomplete);
        assert!(error.message.contains("object bytes"));
        assert!(!error.message.contains("private metadata sentinel"));
        assert_eq!(
            prior_declaration(lab.repo.root(), &head).unwrap(),
            Some(meta::Line::File)
        );
    }

    fn store_raw_tree(lab: &Lab, body: &[u8]) -> String {
        store_raw_object(lab, "tree", body)
    }

    fn store_raw_object(lab: &Lab, kind: &str, body: &[u8]) -> String {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(lab.repo.root())
            .args(["hash-object", "--literally", "-t", kind, "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(body).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn object_lab(format: &str) -> Lab {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo");
        let output = Command::new("git")
            .args(["init", "--initial-branch=main", "--object-format", format])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let lab = Lab {
            _directory: directory,
            repo: Repo::at(&path),
        };
        lab.repo
            .git(&["config", "user.name", "Synthetic fixture"])
            .unwrap();
        lab.repo
            .git(&["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        lab
    }

    fn raw_object(lab: &Lab, oid: &str) -> (String, Vec<u8>) {
        Objects::open(lab.repo.root(), Limits::default())
            .unwrap()
            .read(oid)
            .unwrap()
    }

    fn loose_object(lab: &Lab, oid: &str) -> PathBuf {
        lab.repo
            .root()
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..])
    }

    #[test]
    fn altered_object_bodies_are_rejected_before_parsing_or_caching() {
        for format in ["sha1", "sha256"] {
            for kind in ["blob", "tree", "commit", "tag"] {
                let lab = object_lab(format);
                lab.file_line();
                let head = lab.session("{\"message\":\"hello\"}\n");
                lab.repo
                    .git(&["tag", "-a", "archive", "-m", "synthetic tag"])
                    .unwrap();
                let oid = match kind {
                    "blob" => lab
                        .repo
                        .git(&["rev-parse", "HEAD:session/meta.json"])
                        .unwrap(),
                    "tree" => lab.repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap(),
                    "commit" => head.clone(),
                    "tag" => lab.repo.git(&["rev-parse", "refs/tags/archive"]).unwrap(),
                    _ => unreachable!(),
                };
                assert_eq!(oid.len(), if format == "sha1" { 40 } else { 64 });
                let healthy = check(lab.repo.root());
                assert!(healthy.findings.is_empty(), "{format} {kind}: {healthy:?}");
                assert!(!healthy.incomplete, "{healthy:?}");
                assert_eq!(healthy.views, 1);
                assert_eq!(healthy.commits, 2);
                let (read_kind, mut body) = raw_object(&lab, &oid);
                assert_eq!(read_kind, kind);
                let needle: &[u8] = match kind {
                    "blob" => b"/synthetic",
                    "tree" => b"session",
                    "commit" => b"synthetic history",
                    "tag" => b"synthetic tag",
                    _ => unreachable!(),
                };
                let offset = body
                    .windows(needle.len())
                    .position(|window| window == needle)
                    .unwrap();
                body[offset + 1] = body[offset + 1].to_ascii_uppercase();
                let replacement = store_raw_object(&lab, kind, &body);
                assert_ne!(oid, replacement);
                fs::remove_file(loose_object(&lab, &oid)).unwrap();
                fs::copy(loose_object(&lab, &replacement), loose_object(&lab, &oid)).unwrap();
                let before = lab.files();
                let mut objects = Objects::open(lab.repo.root(), Limits::default()).unwrap();
                let error = if kind == "tree" {
                    objects.tree_entry(&oid, b"session").unwrap_err()
                } else {
                    objects.read(&oid).unwrap_err()
                };
                assert_eq!(
                    error.message,
                    "local object content does not match its object id"
                );
                assert!(!error.incomplete);
                assert!(objects.stopped);
                assert!(objects.trees.is_empty());
                assert_eq!(objects.tree_bytes, 0);
                assert!(objects.read(&replacement).unwrap_err().incomplete);
                if kind == "blob" {
                    assert!(
                        read_metadata(lab.repo.root(), &head)
                            .unwrap_err()
                            .contains("does not match")
                    );
                }
                let report = check(lab.repo.root());
                assert!(report.incomplete, "{format} {kind}: {report:?}");
                assert!(
                    report
                        .findings
                        .iter()
                        .any(|finding| finding.message.contains("does not match")),
                    "{format} {kind}: {report:?}"
                );
                assert_eq!(before, lab.files());
            }
        }
    }

    #[test]
    fn object_digest_checks_preserve_empty_and_binary_bodies() {
        for format in ["sha1", "sha256"] {
            let lab = object_lab(format);
            for body in [b"".as_slice(), b"\0binary\xff\n\r\0".as_slice()] {
                let oid = store_raw_object(&lab, "blob", body);
                let before = lab.files();
                assert_eq!(raw_object(&lab, &oid), ("blob".into(), body.to_vec()));
                assert_eq!(before, lab.files());
            }
        }
    }

    fn tree_body(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (mode, name) in entries {
            body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            body.extend([1; 20]);
        }
        body
    }

    #[test]
    fn parsed_tree_cache_prevents_repeated_queries_from_rescanning_wide_trees() {
        let lab = Lab::new();
        let names: Vec<_> = (0..256).map(|index| format!("item-{index:04}")).collect();
        let entries: Vec<_> = names.iter().map(|name| ("100644", name.as_str())).collect();
        let body = tree_body(&entries);
        let oid = store_raw_tree(&lab, &body);
        let mut objects = Objects::open(lab.repo.root(), Limits::default()).unwrap();
        for _ in 0..1024 {
            assert!(objects.tree_entry(&oid, b"item-0128").unwrap().is_some());
            assert!(objects.tree_entry(&oid, b"absent").unwrap().is_none());
        }
        assert_eq!(objects.reads, 1);
        assert_eq!(objects.parsed_tree_bytes, body.len());
        assert!(objects.tree_bytes <= objects.limits.tree_cache_bytes);

        let limits = Limits {
            tree_cache_bytes: 1,
            total_bytes: body.len() * 4,
            ..Limits::default()
        };
        let mut objects = Objects::open(lab.repo.root(), limits).unwrap();
        for _ in 0..2 {
            assert!(objects.tree_entry(&oid, b"item-0128").unwrap().is_some());
            assert_eq!(objects.tree_bytes, 0);
        }
        let error = objects.tree_entry(&oid, b"absent").unwrap_err();
        assert!(error.incomplete);
        assert!(error.message.contains("tree parsing"));
        assert_eq!(objects.reads, 3);
    }

    #[test]
    fn cached_and_uncached_tree_queries_preserve_duplicate_and_mode_semantics() {
        let lab = Lab::new();
        let mut malformed = tree_body(&[("100644", "VIEW")]);
        malformed.extend_from_slice(b"truncated entry");
        for (body, query, expected_error) in [
            (
                tree_body(&[("100644", "VIEW"), ("100644", "VIEW")]),
                "VIEW",
                true,
            ),
            (
                tree_body(&[("100644", "other"), ("100644", "other"), ("120000", "VIEW")]),
                "VIEW",
                false,
            ),
            (
                tree_body(&[("100644", "other"), ("100644", "other")]),
                "absent",
                false,
            ),
            (malformed, "VIEW", true),
        ] {
            let oid = store_raw_tree(&lab, &body);
            let mut verdicts = Vec::new();
            for tree_cache_bytes in [1, MAX_TREE_CACHE_BYTES] {
                let mut objects = Objects::open(
                    lab.repo.root(),
                    Limits {
                        tree_cache_bytes,
                        ..Limits::default()
                    },
                )
                .unwrap();
                let result = objects.tree_entry(&oid, query.as_bytes());
                assert_eq!(result.is_err(), expected_error);
                verdicts.push(result.map_err(|error| (error.message, error.incomplete)));
            }
            assert_eq!(verdicts[0], verdicts[1]);
        }
    }

    #[test]
    fn local_branch_enumeration_preserves_unicode_whitespace_and_frozen_heads() {
        let lab = Lab::new();
        let first = lab.file_line();
        let literal = "\u{2003}review\u{2003}";
        lab.repo
            .git(&["update-ref", &format!("refs/heads/{literal}"), &first])
            .unwrap();
        lab.repo
            .git(&["update-ref", "refs/remotes/origin/excluded", &first])
            .unwrap();
        lab.repo
            .git(&["update-ref", "refs/tags/excluded", &first])
            .unwrap();
        let frozen = local_branches(lab.repo.root()).unwrap();
        assert_eq!(frozen.len(), 2);
        assert!(frozen.contains(&(literal.into(), first.clone())));
        assert!(frozen.contains(&("main".into(), first.clone())));
        let moved = lab.session("{\"message\":\"changed head\"}\n");
        assert!(frozen.iter().all(|(_, oid)| oid == &first));
        let current = local_branches(lab.repo.root()).unwrap();
        assert!(current.contains(&("main".into(), moved)));
        assert!(current.contains(&(literal.into(), first)));
    }

    #[test]
    fn local_branch_enumeration_limits_are_explicit_errors() {
        let lab = Lab::new();
        assert!(local_branches(lab.repo.root()).unwrap().is_empty());
        let head = lab.file_line();
        lab.repo
            .git(&["update-ref", "refs/heads/other", &head])
            .unwrap();
        let exact = enumerate_refs(lab.repo.root(), &["refs/heads/"], 2, MAX_ROOT_OUTPUT).unwrap();
        assert_eq!(exact.len(), 2);
        let count =
            enumerate_refs(lab.repo.root(), &["refs/heads/"], 1, MAX_ROOT_OUTPUT).unwrap_err();
        assert!(count.incomplete);
        assert!(count.message.contains("count exceeds"));
        let bytes = enumerate_refs(lab.repo.root(), &["refs/heads/"], MAX_ROOTS, 8).unwrap_err();
        assert!(bytes.incomplete);
        assert!(bytes.message.contains("byte limit"));
        fs::write(
            lab.repo.root().join(".git/refs/heads/broken"),
            "invalid object id\n",
        )
        .unwrap();
        assert!(local_branches(lab.repo.root()).is_err());
    }

    #[test]
    fn repository_metadata_queries_share_their_byte_budget() {
        let lab = Lab::new();
        let mut metadata = meta::Meta::new_file_line();
        metadata.cwd = format!("private metadata sentinel {}", "a".repeat(4096));
        meta::write(lab.repo.root(), &metadata).unwrap();
        let first = lab.record();
        metadata.cwd = format!("private metadata sentinel {}", "b".repeat(4096));
        meta::write(lab.repo.root(), &metadata).unwrap();
        let second = lab.record();
        let limits = Limits {
            total_bytes: 6144,
            ..Limits::default()
        };
        for oid in [&first, &second] {
            let mut fresh = MetadataInspection::with_limits(lab.repo.root(), limits).unwrap();
            assert_eq!(fresh.metadata(oid).unwrap().unwrap().line, meta::Line::File);
        }
        let mut shared = MetadataInspection::with_limits(lab.repo.root(), limits).unwrap();
        assert_eq!(
            shared.metadata(&first).unwrap().unwrap().line,
            meta::Line::File
        );
        let error = shared.metadata(&second).unwrap_err();
        assert!(error.contains("object bytes"), "{error}");
        assert!(!error.contains("private metadata sentinel"));
        assert!(shared.metadata(&first).is_err());
    }

    #[test]
    fn repository_metadata_and_lineage_queries_share_attempted_lookup_budget() {
        let lab = Lab::new();
        let file = lab.file_line();
        fs::remove_file(lab.repo.root().join(meta::FILE)).unwrap();
        let missing = lab.record();
        let limits = Limits {
            commits: 2,
            ..Limits::default()
        };
        let mut fresh = MetadataInspection::with_limits(lab.repo.root(), limits).unwrap();
        assert_eq!(
            fresh.prior_declaration(&missing).unwrap(),
            Some(meta::Line::File)
        );

        let mut shared = MetadataInspection::with_limits(lab.repo.root(), limits).unwrap();
        assert_eq!(
            shared.metadata(&file).unwrap().unwrap().line,
            meta::Line::File
        );
        let error = shared.prior_declaration(&missing).unwrap_err();
        assert!(error.contains("commit lookups"), "{error}");
        assert!(shared.metadata(&file).is_err());
        assert_eq!(shared.lookups, 2);

        let limits = Limits {
            commits: 1,
            ..Limits::default()
        };
        let mut shared = MetadataInspection::with_limits(lab.repo.root(), limits).unwrap();
        assert!(shared.metadata(&file).unwrap().is_some());
        assert!(
            shared
                .prior_declaration(&file)
                .unwrap_err()
                .contains("commit lookups")
        );
        let mut shared = MetadataInspection::with_limits(lab.repo.root(), limits).unwrap();
        assert!(shared.prior_declaration("HEAD").is_err());
        assert!(
            shared
                .metadata(&file)
                .unwrap_err()
                .contains("commit lookups")
        );
    }
}
