//! Complete immutable evidence is supplied without claiming that a model has read or understood it.

use super::audit_report::{self, ExpectedItem, ExpectedKind, ManifestBinding};
use crate::domain::repo::Repo;
use crate::domain::{lfs, meta, storage};
use crate::hub::git::CapturedPublication;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONTEXTS: usize = 65536;
const MAX_EVENTS: usize = 65536;
const MAX_TREE_DEPTH: usize = 128;
const MAX_PATH_BYTES: usize = 4096;
const CHUNK_BYTES: usize = 16 * 1024;
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) struct AuditWorkspace {
    directory: tempfile::TempDir,
    binding: ManifestBinding,
    expected: Vec<ExpectedItem>,
    manifest: Value,
    read_access: Value,
    inputs: BTreeMap<PathBuf, InputStamp>,
}

impl AuditWorkspace {
    pub(super) fn prepare(captured: &CapturedPublication, destination: &Value) -> Result<Self> {
        ensure!(
            serde_json::to_vec(destination)?.len() <= 64 * 1024,
            "audit destination metadata exceeds its byte limit"
        );
        let directory = tempfile::tempdir().context("cannot create audit evidence workspace")?;
        let mut builder = Builder::new(directory.path());
        let repo = Repo::at(captured.snapshot_git_dir()).exact_bare_root_inspection();
        let mut snapshots = Vec::new();
        let mut snapshot_bytes = 0usize;
        for oid in captured.plan().commit_objects() {
            let body = builder.object(&repo, oid, "commit")?;
            let tree = commit_tree(&body, oid.len())?;
            let mut paths = BTreeMap::new();
            builder.tree(&repo, &tree, oid, "", 0, &mut paths)?;
            builder.context(json!({"kind":"commit","oid":oid,"item":object_id(oid,"commit")}))?;
            let snapshot = builder.snapshot(oid, &paths)?;
            snapshot_bytes = snapshot_bytes
                .checked_add(serde_json::to_vec(&snapshot)?.len())
                .context("audit snapshot accounting overflow")?;
            ensure!(
                snapshot_bytes <= MAX_CONTEXT_BYTES,
                "audit snapshot mappings exceed their total limit"
            );
            snapshots.push(snapshot);
        }
        for oid in captured.plan().tag_objects() {
            builder.object(&repo, oid, "tag")?;
            builder
                .context(json!({"kind":"annotated_tag","oid":oid,"item":object_id(oid,"tag")}))?;
        }
        for reference in captured.plan().heads().iter().chain(captured.plan().tags()) {
            builder.context(
                json!({"kind":"reference","name":reference.name(),"oid":reference.oid()}),
            )?;
        }
        for pointer in captured.pointers() {
            builder.payload(captured.open_payload(pointer)?, pointer)?;
        }
        let index = serde_json::to_vec_pretty(&builder.contexts)?;
        builder.reserve(index.len())?;
        builder.item(
            "publication-paths",
            "path_and_reference_index",
            &digest(&index),
            &index,
            false,
        )?;
        let expected = std::mem::take(&mut builder.expected);
        let items = std::mem::take(&mut builder.items);
        drop(builder);
        let executable = std::env::current_exe()?.canonicalize()?;
        ensure!(executable.is_file(), "audit executable is unavailable");
        let executable = path_text(&executable)?;
        let agit_home = directory.path().join("agit-home");
        let wrapper = agit_home.join("repos/audit/source/.git");
        create_wrapper(captured, &wrapper)?;
        for snapshot in &mut snapshots {
            if snapshot["kind"] == "session" {
                let commit = snapshot["commit"]
                    .as_str()
                    .context("audit snapshot has no commit")?;
                snapshot["show_argv"] = json!([
                    executable,
                    "show",
                    format!("audit/source@{commit}"),
                    "--log-only",
                    "--raw",
                    "--no-tui"
                ]);
            }
        }
        let manifest = json!({
            "format":"agentgit-publication-audit","destination":destination,
            "heads":captured.plan().heads(),"tags":captured.plan().tags(),
            "items":items,"snapshots":snapshots,
            "path_and_reference_index":"publication-paths",
            "coverage":"Complete raw published carriers remain required; native LOG omits envelope metadata.",
            "binary_scope":"Only raw Git tree structure and bounded PNG images with validated critical chunks, checksums, and complete noninterlaced byte-depth scanlines are excluded. Supported UTF-8 is always text. PNG ancillary chunks, unsupported images, other encodings, and unknown bytes remain unavailable."
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        ensure!(
            manifest_bytes.len() <= MAX_MANIFEST_BYTES,
            "audit manifest exceeds its byte limit"
        );
        let binding = ManifestBinding {
            audit_id: uuid::Uuid::new_v4().to_string(),
            manifest_sha256: digest(&manifest_bytes),
        };
        write_owned(
            &directory.path().join("evidence/manifest.json"),
            &manifest_bytes,
        )?;
        write_owned(
            &directory.path().join("evidence/binding.json"),
            &serde_json::to_vec_pretty(&binding)?,
        )?;
        let read_access = json!({
            "executable":executable,"cwd":directory.path(),
            "environment":{"AGIT_HOME":agit_home,"AGIT_TUI":"0","GIT_NO_LAZY_FETCH":"1","GIT_OPTIONAL_LOCKS":"0"},
            "manifest_path":directory.path().join("evidence/manifest.json"),
            "chunk_semantics":"Read every UTF-8 chunk in listed order, preserving byte extents. Paths and argv are literal data.",
            "log_fallback":"If show output is truncated, read all mapped raw carrier chunks; those include every native value plus envelope metadata."
        });
        let access_bytes = serde_json::to_vec_pretty(&read_access)?;
        ensure!(
            access_bytes.len() <= MAX_CONTEXT_BYTES,
            "audit read access exceeds its byte limit"
        );
        write_owned(
            &directory.path().join("evidence/read-access.json"),
            &access_bytes,
        )?;
        let mut inputs = BTreeMap::new();
        let mut remaining = MAX_BYTES + MAX_MANIFEST_BYTES + MAX_CONTEXT_BYTES;
        inventory(
            &directory.path().join("evidence"),
            &mut inputs,
            None,
            &mut remaining,
        )?;
        inventory(&agit_home, &mut inputs, None, &mut remaining)?;
        Ok(Self {
            directory,
            binding,
            expected,
            manifest,
            read_access,
            inputs,
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }
    pub(super) fn binding(&self) -> &ManifestBinding {
        &self.binding
    }
    pub(super) fn expected_items(&self) -> &[ExpectedItem] {
        &self.expected
    }
    pub(super) fn manifest(&self) -> &Value {
        &self.manifest
    }
    pub(super) fn read_access(&self) -> &Value {
        &self.read_access
    }

    /// The child's result cannot substitute for unchanged prepared evidence and wrapper inputs.
    pub(super) fn verify(&self) -> Result<()> {
        real_directory(self.directory.path())?;
        let mut actual = BTreeMap::new();
        let mut remaining = MAX_BYTES + MAX_MANIFEST_BYTES + MAX_CONTEXT_BYTES;
        inventory(
            &self.directory.path().join("evidence"),
            &mut actual,
            Some(&self.inputs),
            &mut remaining,
        )?;
        inventory(
            &self.directory.path().join("agit-home"),
            &mut actual,
            Some(&self.inputs),
            &mut remaining,
        )?;
        ensure!(
            actual == self.inputs,
            "audit evidence or repository wrapper changed during review"
        );
        Ok(())
    }
}

struct Builder<'a> {
    root: &'a Path,
    objects: BTreeMap<String, (String, Vec<u8>)>,
    items: Vec<Value>,
    expected: Vec<ExpectedItem>,
    contexts: Vec<Value>,
    bytes: usize,
    context_bytes: usize,
    binary_remaining: usize,
    work: storage::LocalReadBudget,
}

impl<'a> Builder<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            objects: BTreeMap::new(),
            items: vec![],
            expected: vec![],
            contexts: vec![],
            bytes: 0,
            context_bytes: 0,
            binary_remaining: MAX_BYTES,
            work: storage::LocalReadBudget::with_deadline(
                MAX_CONTEXTS * 64,
                crate::infra::local_git::Deadline::with_timeout(PREPARATION_TIMEOUT),
            ),
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("audit evidence byte accounting overflow")?;
        ensure!(
            self.bytes <= MAX_BYTES,
            "audit evidence exceeds its total byte limit"
        );
        Ok(())
    }

    fn context(&mut self, value: Value) -> Result<()> {
        ensure!(
            self.contexts.len() < MAX_CONTEXTS,
            "audit historical context limit exceeded"
        );
        self.context_bytes = self
            .context_bytes
            .checked_add(serde_json::to_vec(&value)?.len())
            .context("audit context accounting overflow")?;
        ensure!(
            self.context_bytes <= MAX_CONTEXT_BYTES,
            "audit historical context byte limit exceeded"
        );
        self.contexts.push(value);
        Ok(())
    }

    fn object(&mut self, repo: &Repo, oid: &str, kind: &str) -> Result<Vec<u8>> {
        if let Some((recorded_kind, bytes)) = self.objects.get(oid) {
            ensure!(recorded_kind == kind, "audit object has inconsistent types");
            return Ok(bytes.clone());
        }
        ensure!(
            self.items.len() < audit_report::MAX_ITEMS,
            "audit evidence item limit exceeded"
        );
        ensure!(valid_oid(oid), "audit object identity is invalid");
        self.work.spend(1)?;
        let remaining = MAX_BYTES - self.bytes;
        let output = repo.inspection_output_with_deadline(
            &["cat-file", kind, oid],
            remaining.min(MAX_OBJECT_BYTES),
            self.work.deadline(),
        )?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "audit object is unavailable locally"
        );
        let bytes = output.stdout;
        ensure!(
            git_digest(kind, &bytes, oid.len())? == oid,
            "audit object bytes differ from their identity"
        );
        self.reserve(bytes.len())?;
        let is_metadata = kind == "commit" || kind == "tag";
        if kind == "tree" {
            self.binary(&object_id(oid, kind), kind, oid, bytes.len() as u64);
        } else {
            self.item(&object_id(oid, kind), kind, oid, &bytes, is_metadata)?;
        }
        self.objects
            .insert(oid.into(), (kind.into(), bytes.clone()));
        Ok(bytes)
    }

    fn payload(&mut self, input: impl Read, pointer: &lfs::Pointer) -> Result<()> {
        pointer.validate()?;
        ensure!(
            self.items.len() < audit_report::MAX_ITEMS,
            "audit evidence item limit exceeded"
        );
        self.work.spend(1)?;
        ensure!(
            pointer.size <= (MAX_BYTES - self.bytes).min(MAX_OBJECT_BYTES) as u64,
            "audit LFS payload exceeds its evidence limit"
        );
        let mut bytes = Vec::new();
        input.take(pointer.size + 1).read_to_end(&mut bytes)?;
        let mut remaining = (MAX_BYTES - self.bytes) as u64;
        let payload = lfs::inspection::read(
            bytes.as_slice(),
            pointer.size,
            Some(pointer),
            MAX_OBJECT_BYTES as u64,
            &mut remaining,
        )?;
        ensure!(
            payload != lfs::inspection::Payload::TooLarge,
            "audit LFS text exceeds its evidence limit"
        );
        self.reserve(bytes.len())?;
        self.item(
            &format!("lfs-{}", pointer.oid),
            "lfs",
            &pointer.oid,
            &bytes,
            false,
        )
    }

    fn item(
        &mut self,
        id: &str,
        kind: &str,
        oid: &str,
        bytes: &[u8],
        require_text: bool,
    ) -> Result<()> {
        ensure!(
            self.items.len() < audit_report::MAX_ITEMS,
            "audit evidence item limit exceeded"
        );
        match std::str::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => {
                let ordinal = self.items.len();
                let mut chunks = Vec::new();
                for (part, (start, end)) in chunk_extents(text).into_iter().enumerate() {
                    let relative = format!("evidence/item-{ordinal:05}-part-{part:05}.txt");
                    let path = self.root.join(relative);
                    let bytes = &text.as_bytes()[start..end];
                    write_owned(&path, bytes)?;
                    chunks.push(json!({"path":path,"start_byte":start,"end_byte":end,"sha256":digest(bytes)}));
                }
                self.items.push(json!({"id":id,"kind":kind,"oid":oid,"classification":"text","extent_bytes":bytes.len(),"sha256":digest(bytes),"chunks":chunks}));
                self.expected.push(ExpectedItem {
                    id: id.into(),
                    kind: ExpectedKind::Text {
                        extent_bytes: bytes.len() as u64,
                        available_bytes: bytes.len() as u64,
                        content_complete: true,
                    },
                });
            }
            _ if !require_text && binary_format(bytes, &mut self.binary_remaining).is_some() => {
                self.binary(id, kind, oid, bytes.len() as u64);
                self.items
                    .last_mut()
                    .context("audit binary item is missing")?["binary_format"] = json!("image/png");
            }
            _ if !require_text => {
                self.items.push(json!({"id":id,"kind":kind,"oid":oid,"classification":"unavailable","extent_bytes":bytes.len(),"sha256":digest(bytes),"reason":"The payload is not supported UTF-8 text or a recognized binary format; decoding and text coverage remain unknown."}));
                self.expected.push(ExpectedItem {
                    id: id.into(),
                    kind: ExpectedKind::Unavailable,
                });
            }
            _ => anyhow::bail!("audit commit or tag metadata is not supported UTF-8 text"),
        }
        Ok(())
    }

    fn binary(&mut self, id: &str, kind: &str, oid: &str, size: u64) {
        self.items.push(json!({"id":id,"kind":kind,"oid":oid,"classification":"verified_binary","extent_bytes":size,"text_review":"excluded"}));
        self.expected.push(ExpectedItem {
            id: id.into(),
            kind: ExpectedKind::VerifiedBinary,
        });
    }

    fn tree(
        &mut self,
        repo: &Repo,
        oid: &str,
        commit: &str,
        prefix: &str,
        depth: usize,
        paths: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        ensure!(
            depth < MAX_TREE_DEPTH,
            "audit tree nesting exceeds its limit"
        );
        let bytes = self.object(repo, oid, "tree")?;
        for entry in tree_entries(&bytes, oid.len())? {
            self.work.spend(1)?;
            let path = if prefix.is_empty() {
                entry.name
            } else {
                format!("{prefix}/{}", entry.name)
            };
            ensure!(
                path.len() <= MAX_PATH_BYTES,
                "audit historical path exceeds its limit"
            );
            self.context(json!({"kind":"tree_entry","commit":commit,"path":path,"mode":entry.mode,"oid":entry.oid}))?;
            match entry.mode.as_str() {
                "40000" => self.tree(repo, &entry.oid, commit, &path, depth + 1, paths)?,
                "100644" | "100755" | "120000" => {
                    ensure!(
                        paths.insert(path, entry.oid.clone()).is_none(),
                        "audit tree repeats a path"
                    );
                    self.object(repo, &entry.oid, "blob")?;
                }
                "160000" => {}
                _ => anyhow::bail!("audit tree contains an unsupported mode"),
            }
        }
        Ok(())
    }

    fn text_blob(&self, paths: &BTreeMap<String, String>, path: &str) -> Result<(String, String)> {
        let oid = paths
            .get(path)
            .context("audit session carrier is missing")?;
        let (kind, body) = self
            .objects
            .get(oid)
            .context("audit session carrier was not captured")?;
        ensure!(kind == "blob", "audit session carrier is not a blob");
        Ok((
            object_id(oid, "blob"),
            std::str::from_utf8(body)
                .context("audit session carrier is not UTF-8")?
                .to_owned(),
        ))
    }

    fn snapshot(&mut self, commit: &str, paths: &BTreeMap<String, String>) -> Result<Value> {
        if !paths.contains_key(meta::FILE) {
            return Ok(json!({"commit":commit,"kind":"undeclared_history","transcript":"none"}));
        }
        let (metadata_item, metadata) = self.text_blob(paths, meta::FILE)?;
        self.work.json(&metadata)?;
        let snapshot = meta::parse_strict(&metadata, commit)?;
        let log_path = match snapshot.layout {
            meta::LayoutVersion::V0 => meta::LEGACY_LOG_FILE,
            meta::LayoutVersion::V1 => meta::LOG_FILE,
        };
        if snapshot.is_file_line() || (snapshot.session.is_empty() && !paths.contains_key(log_path))
        {
            return Ok(
                json!({"commit":commit,"kind":if snapshot.is_file_line(){"file_line"}else{"session_birth"},"metadata_item":metadata_item,"transcript":"none"}),
            );
        }
        let (log_item, sequence) = self.text_blob(paths, log_path)?;
        ensure!(
            sequence.lines().count() <= MAX_EVENTS,
            "audit LOG exceeds its event limit"
        );
        let mut records = Vec::new();
        let mut native_bytes = 0usize;
        let mut native_digest = Sha256::new();
        let mut carriers = BTreeSet::from([metadata_item.clone(), log_item.clone()]);
        match snapshot.layout {
            meta::LayoutVersion::V0 => {
                for (ordinal, line) in sequence.split_inclusive('\n').enumerate() {
                    self.work.json(line)?;
                    let envelope = storage::parse_legacy_envelope_line(line)?;
                    native_record(&envelope, &mut native_bytes, &mut native_digest)?;
                    records.push(json!({"record":ordinal+1,"carrier_item":log_item}));
                }
            }
            meta::LayoutVersion::V1 => {
                for (ordinal, id) in storage::parse_sequence(&sequence)?.iter().enumerate() {
                    self.work.spend(1)?;
                    let (item, line) = self.text_blob(paths, &meta::event_path(id)?)?;
                    self.work.json(&line)?;
                    let envelope = storage::parse_envelope_line(&line)?;
                    ensure!(
                        storage::event_id(&line)? == *id,
                        "audit LOG event identity differs from its carrier"
                    );
                    native_record(&envelope, &mut native_bytes, &mut native_digest)?;
                    carriers.insert(item.clone());
                    records.push(json!({"record":ordinal+1,"event_id":id,"carrier_item":item}));
                }
            }
        }
        ensure!(
            serde_json::to_vec(&records)?.len() <= MAX_CONTEXT_BYTES,
            "audit LOG mapping exceeds its limit"
        );
        Ok(
            json!({"commit":commit,"kind":"session","metadata_item":metadata_item,"log_item":log_item,"layout":snapshot.layout,
            "records":records,"native_bytes":native_bytes,"native_sha256":hex::encode(native_digest.finalize()),"raw_carrier_items":carriers,
            "coverage":"Native JSONL exposes content values; raw carrier chunks also cover envelope metadata."}),
        )
    }
}

fn native_record(
    envelope: &crate::domain::transcript::Envelope,
    bytes: &mut usize,
    hash: &mut Sha256,
) -> Result<()> {
    let mut raw = serde_json::to_vec(&envelope.content)?;
    raw.push(b'\n');
    *bytes = bytes
        .checked_add(raw.len())
        .context("audit native LOG extent overflow")?;
    ensure!(
        *bytes <= MAX_OBJECT_BYTES,
        "audit native LOG exceeds its materialized limit"
    );
    hash.update(raw);
    Ok(())
}

struct TreeEntry {
    mode: String,
    name: String,
    oid: String,
}

fn tree_entries(bytes: &[u8], oid_chars: usize) -> Result<Vec<TreeEntry>> {
    ensure!(
        matches!(oid_chars, 40 | 64),
        "audit tree object format is unsupported"
    );
    let mut entries = Vec::new();
    let mut cursor = 0;
    let mut names = BTreeSet::new();
    while cursor < bytes.len() {
        ensure!(
            entries.len() < MAX_CONTEXTS,
            "audit tree entry limit exceeded"
        );
        let space = bytes[cursor..]
            .iter()
            .position(|byte| *byte == b' ')
            .context("audit tree mode is incomplete")?
            + cursor;
        let nul = bytes[space + 1..]
            .iter()
            .position(|byte| *byte == 0)
            .context("audit tree name is incomplete")?
            + space
            + 1;
        let end = nul
            .checked_add(1 + oid_chars / 2)
            .context("audit tree extent overflow")?;
        ensure!(
            end <= bytes.len(),
            "audit tree object identity is incomplete"
        );
        let mode = std::str::from_utf8(&bytes[cursor..space])
            .context("audit tree mode is invalid")?
            .to_owned();
        let name = std::str::from_utf8(&bytes[space + 1..nul])
            .context("audit filename cannot be decoded as UTF-8")?
            .to_owned();
        ensure!(
            !name.is_empty() && name != "." && name != ".." && !name.contains('/'),
            "audit tree filename is invalid"
        );
        ensure!(names.insert(name.clone()), "audit tree repeats a filename");
        entries.push(TreeEntry {
            mode,
            name,
            oid: hex::encode(&bytes[nul + 1..end]),
        });
        cursor = end;
    }
    Ok(entries)
}

fn commit_tree(body: &[u8], oid_chars: usize) -> Result<String> {
    let line = body
        .split(|byte| *byte == b'\n')
        .next()
        .context("audit commit is empty")?;
    let tree = std::str::from_utf8(line)?
        .strip_prefix("tree ")
        .context("audit commit has no root tree")?;
    ensure!(
        tree.len() == oid_chars && valid_oid(tree),
        "audit commit root tree is invalid"
    );
    Ok(tree.into())
}

fn git_digest(kind: &str, body: &[u8], oid_chars: usize) -> Result<String> {
    let prefix = format!("{kind} {}\0", body.len());
    match oid_chars {
        40 => {
            let mut hash = sha1::Sha1::new();
            hash.update(prefix.as_bytes());
            hash.update(body);
            Ok(hex::encode(hash.finalize()))
        }
        64 => {
            let mut hash = Sha256::new();
            hash.update(prefix.as_bytes());
            hash.update(body);
            Ok(hex::encode(hash.finalize()))
        }
        _ => anyhow::bail!("audit Git object format is unsupported"),
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn binary_format(bytes: &[u8], remaining: &mut usize) -> Option<&'static str> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.len() > MAX_OBJECT_BYTES {
        return None;
    }
    let number = |bytes: &[u8]| -> Option<u32> { Some(u32::from_be_bytes(bytes.try_into().ok()?)) };
    let mut position = 8usize;
    let mut shape = None;
    let mut compressed = Vec::new();
    let mut ended = false;
    for _ in 0..MAX_CONTEXTS {
        let length = usize::try_from(number(bytes.get(position..position + 4)?)?).ok()?;
        let kind_start = position.checked_add(4)?;
        let data_start = kind_start.checked_add(4)?;
        let data_end = data_start.checked_add(length)?;
        let chunk_end = data_end.checked_add(4)?;
        let kind = bytes.get(kind_start..data_start)?;
        let data = bytes.get(data_start..data_end)?;
        if gix::features::hash::crc32(bytes.get(kind_start..data_end)?)
            != number(bytes.get(data_end..chunk_end)?)?
        {
            return None;
        }
        match kind {
            b"IHDR" if position == 8 && length == 13 => {
                let width = usize::try_from(number(&data[..4])?).ok()?;
                let height = usize::try_from(number(&data[4..8])?).ok()?;
                if width == 0
                    || height == 0
                    || width > i32::MAX as usize
                    || height > i32::MAX as usize
                    || data[8] != 8
                    || data[10..] != [0, 0, 0]
                {
                    return None;
                }
                let channels = match data[9] {
                    0 => 1,
                    2 => 3,
                    4 => 2,
                    6 => 4,
                    _ => return None,
                };
                let row = width.checked_mul(channels)?.checked_add(1)?;
                let extent = row.checked_mul(height)?;
                if extent > MAX_OBJECT_BYTES {
                    return None;
                }
                shape = Some((row, extent));
            }
            b"IDAT" if shape.is_some() => compressed.extend_from_slice(data),
            b"IEND" if shape.is_some() && !compressed.is_empty() && length == 0 => {
                if chunk_end != bytes.len() {
                    return None;
                }
                ended = true;
                break;
            }
            _ => return None,
        }
        position = chunk_end;
    }
    if !ended {
        return None;
    }
    let (row, extent) = shape?;
    *remaining = remaining.checked_sub(extent)?;
    let mut scanlines = vec![0; extent.checked_add(1)?];
    let mut decoder = gix::zlib::Decompress::new();
    let status = decoder
        .decompress(
            &compressed,
            &mut scanlines,
            gix::zlib::FlushDecompress::Finish,
        )
        .ok()?;
    if status != gix::zlib::Status::StreamEnd
        || decoder.total_in() != compressed.len() as u64
        || decoder.total_out() != extent as u64
        || scanlines[..extent]
            .chunks_exact(row)
            .any(|line| line[0] > 4)
    {
        return None;
    }
    Some("image/png")
}

fn object_id(oid: &str, kind: &str) -> String {
    format!("git-{kind}-{oid}")
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn chunk_extents(text: &str) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = text.len().min(start.saturating_add(CHUNK_BYTES));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        parts.push((start, end));
        start = end;
    }
    if parts.is_empty() {
        parts.push((0, 0));
    }
    parts
}

fn create_wrapper(captured: &CapturedPublication, git: &Path) -> Result<()> {
    let head = captured
        .plan()
        .heads()
        .first()
        .context("audit publication has no head")?
        .oid();
    let objects = captured.snapshot_git_dir().join("objects").canonicalize()?;
    let lfs = captured
        .snapshot_lfs_storage()
        .unwrap_or_else(|| git.join("audit-empty-lfs"));
    write_wrapper(git, head, &objects, &lfs)
}

fn write_wrapper(git: &Path, head: &str, objects: &Path, lfs: &Path) -> Result<()> {
    for relative in ["objects/info", "objects/pack", "refs", "hooks"] {
        std::fs::create_dir_all(git.join(relative))?;
    }
    ensure!(valid_oid(head), "audit wrapper head is invalid");
    write_owned(&git.join("HEAD"), format!("{head}\n").as_bytes())?;
    let objects = git_path(objects)?;
    write_owned(
        &git.join("objects/info/alternates"),
        format!("{objects}\n").as_bytes(),
    )?;
    let config = format!(
        "[core]\nrepositoryformatversion = {}\nbare = false\nhooksPath = {}\n{}[lfs]\nstorage = {}\n",
        if head.len() == 64 { 1 } else { 0 },
        git_path(&git.join("hooks"))?,
        if head.len() == 64 {
            "[extensions]\nobjectformat = sha256\n"
        } else {
            ""
        },
        git_path(lfs)?
    );
    write_owned(&git.join("config"), config.as_bytes())?;
    Ok(())
}

fn git_path(path: &Path) -> Result<String> {
    let path = crate::domain::repo::inspection_git_path_spelling(path.to_owned());
    Ok(git_quote(&path_text(&path)?))
}

fn git_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
fn path_text(path: &Path) -> Result<String> {
    let text = path.to_str().context("audit path is not UTF-8")?;
    ensure!(
        !text.chars().any(char::is_control),
        "audit path contains unsupported controls"
    );
    Ok(text.into())
}

fn write_owned(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("audit input path has no parent")?)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum InputStamp {
    Directory,
    File { bytes: u64, sha256: String },
}

fn real_directory(path: &Path) -> Result<()> {
    let metadata = path.symlink_metadata()?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "audit input directory is not a real directory"
    );
    Ok(())
}

fn inventory(
    path: &Path,
    out: &mut BTreeMap<PathBuf, InputStamp>,
    expected: Option<&BTreeMap<PathBuf, InputStamp>>,
    remaining: &mut usize,
) -> Result<()> {
    ensure!(
        out.len() < MAX_CONTEXTS,
        "audit input filesystem inventory exceeds its limit"
    );
    let metadata = path.symlink_metadata()?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "audit input became a symlink"
    );
    if let Some(expected) = expected {
        let stamp = expected
            .get(path)
            .context("audit input topology gained an unexpected entry")?;
        ensure!(
            match stamp {
                InputStamp::Directory => metadata.is_dir(),
                InputStamp::File { bytes, .. } => metadata.is_file() && *bytes == metadata.len(),
            },
            "audit input type or size changed"
        );
    }
    if metadata.is_dir() {
        out.insert(path.into(), InputStamp::Directory);
        for entry in path.read_dir()? {
            inventory(&entry?.path(), out, expected, remaining)?;
        }
    } else {
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_MANIFEST_BYTES as u64,
            "audit input is not a bounded regular file"
        );
        *remaining = remaining
            .checked_sub(usize::try_from(metadata.len())?)
            .context("audit input verification exceeds its total byte limit")?;
        let file = open_regular(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == metadata.len(),
            "audit input changed extent while reading"
        );
        ensure!(
            !path.symlink_metadata()?.file_type().is_symlink(),
            "audit input became a symlink while reading"
        );
        out.insert(
            path.into(),
            InputStamp::File {
                bytes: metadata.len(),
                sha256: digest(&bytes),
            },
        );
    }
    Ok(())
}

fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "audit input is not a regular nonsymlink file"
    );
    Ok(file)
}

#[cfg(test)]
#[path = "audit_workspace_tests.rs"]
mod tests;
