//! The byte-level contract of the v0/v1 session storage formats.
//!
//! v0 stores the whole [`Envelope`] as JSONL; v1 puts every canonical envelope in
//! `events/a/b/c/d/<event-id>` and keeps only the event id sequence in `LOG` / `VIEW`.
//! `_object_hash` still addresses `content` alone; `event-id` covers the full envelope and its
//! trailing LF, so same-content events from different session/source pairs never share the wrong
//! bytes.

use crate::Result;
use crate::domain::meta::{self, LayoutVersion};
use crate::domain::transcript::{self, Envelope};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Read limit for one event object.
pub const MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;

/// Total byte limit for one materialization result (one LOG or one VIEW).
///
/// A paired read returns two independent results, so the process-level bound on result buffers is
/// twice this value; the deduplicated event union stays bounded by this value on its own, and each
/// event body is read once.
pub const MAX_MATERIALIZED_BYTES: usize = 512 * 1024 * 1024;

/// Maximum sequence length allowed in one LOG / VIEW.
pub const MAX_SEQUENCE_EVENTS: usize = 1_000_000;

/// The attributes block agit manages. It sits after the user's existing rules, which is what
/// protects the raw bytes of v1 content-addressed files.
const LEGACY_ATTRIBUTES_BEGIN: &str = "# agit:storage-v1 begin";
const LEGACY_ATTRIBUTES_END: &str = "# agit:storage-v1 end";
const DEFAULTS_BEGIN: &str = "# agit:storage-v1 defaults begin";
const DEFAULTS_END: &str = "# agit:storage-v1 defaults end";
const OBJECTS_BEGIN: &str = "# agit:storage-v1 objects begin";
const OBJECTS_END: &str = "# agit:storage-v1 objects end";

const DEFAULTS_CONTENT: &str = "# agit:storage-v1 defaults begin\n\
# Normalize ordinary text to LF.\n\
* text=auto eol=lf\n\
# agit:storage-v1 defaults end\n";

const OBJECTS_CONTENT: &str = "# agit:storage-v1 objects begin\n\
# Content-addressed data must remain byte-for-byte stable.\n\
LOG        -text -merge\n\
VIEW       -text -merge\n\
events/**  -text -merge -diff\n\
# agit:storage-v1 objects end\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SequenceKind {
    Log,
    View,
}

impl SequenceKind {
    fn parse(path: &str) -> Result<Self> {
        match path {
            meta::LOG_FILE | meta::LEGACY_LOG_FILE => Ok(Self::Log),
            meta::VIEW_FILE | meta::LEGACY_VIEW_FILE => Ok(Self::View),
            _ => anyhow::bail!(
                "sequence file must be `{}` or `{}`; got `{path}`",
                meta::LOG_FILE,
                meta::VIEW_FILE
            ),
        }
    }

    const fn path(self, layout: LayoutVersion) -> &'static str {
        match (layout, self) {
            (LayoutVersion::V0, Self::Log) => meta::LEGACY_LOG_FILE,
            (LayoutVersion::V0, Self::View) => meta::LEGACY_VIEW_FILE,
            (LayoutVersion::V1, Self::Log) => meta::LOG_FILE,
            (LayoutVersion::V1, Self::View) => meta::VIEW_FILE,
        }
    }
}

/// Serialize an envelope into its one wire form: single-line JSON plus exactly one LF.
pub fn envelope_line(envelope: &Envelope) -> String {
    let mut line = serde_json::to_string(envelope)
        .unwrap_or_else(|e| unreachable!("Envelope serialization cannot fail: {e}"));
    line.push('\n');
    line
}

/// Parse one envelope strictly.
///
/// Beyond the JSON shape this checks the canonical bytes, the trailing LF, the session id and
/// `_object_hash = hash(content)`, so the returned envelope is safe to compute an event id from.
pub fn parse_envelope_line(line: &str) -> Result<Envelope> {
    let envelope = parse_legacy_envelope_line(line)?;
    let canonical = envelope_line(&envelope);
    if canonical != line {
        anyhow::bail!("envelope is valid JSON but not in canonical wire form");
    }
    Ok(envelope)
}

/// Parse a historical v0 envelope while tolerating its old JSON object field order.
///
/// Some legacy synthetic marker/summary writers serialized through `serde_json::Value`, whose map
/// order differed from the declared [`Envelope`] wire order. Shape, provenance and content hash
/// remain strict; only reserialization order/insignificant JSON whitespace are normalized.
pub(crate) fn parse_legacy_envelope_line(line: &str) -> Result<Envelope> {
    let Some(json) = line.strip_suffix('\n') else {
        anyhow::bail!("envelope must end with exactly one LF");
    };
    if json.is_empty() {
        anyhow::bail!("envelope must not be empty");
    }
    if json.contains(['\n', '\r']) {
        anyhow::bail!("envelope must be one LF-terminated line (CRLF is not canonical)");
    }

    let envelope: Envelope = serde_json::from_str(json).context("invalid envelope JSON")?;
    if envelope.source.is_empty() {
        anyhow::bail!("envelope `_source` must not be empty");
    }
    if !meta::is_bare_id(&envelope.session_id) {
        anyhow::bail!("envelope `_session_id` must be `agit-` plus 40 lowercase hex characters");
    }
    if !meta::is_event_id(&envelope.object_hash) {
        anyhow::bail!("envelope `_object_hash` must be 40 lowercase hex characters");
    }
    let expected_object_hash = transcript::object_hash(&envelope.content);
    if envelope.object_hash != expected_object_hash {
        anyhow::bail!(
            "envelope `_object_hash` mismatch: expected {expected_object_hash}, got {}",
            envelope.object_hash
        );
    }
    Ok(envelope)
}

/// Parse envelope JSONL strictly. An empty file is valid; every line of a non-empty file is a
/// canonical envelope.
pub fn parse_envelopes(text: &str) -> Result<Vec<Envelope>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let Some(body) = text.strip_suffix('\n') else {
        anyhow::bail!("envelope JSONL must end with LF");
    };
    body.split('\n')
        .enumerate()
        .map(|(index, json)| {
            let mut line = json.to_owned();
            line.push('\n');
            parse_envelope_line(&line)
                .with_context(|| format!("invalid envelope at line {}", index + 1))
        })
        .collect()
}

/// event id = `SHA256(canonical full envelope line, trailing LF included)[..40]`.
pub fn event_id(envelope_line: &str) -> Result<String> {
    parse_envelope_line(envelope_line)?;
    Ok(hex::encode(Sha256::digest(envelope_line.as_bytes()))[..meta::EVENT_ID_HEX_LEN].to_owned())
}

/// Parse `LOG` / `VIEW` strictly. An empty sequence is valid; every line of a non-empty sequence
/// is an event id of 40 lowercase hex characters, and the file ends with LF.
pub fn parse_sequence(text: &str) -> Result<Vec<String>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let Some(body) = text.strip_suffix('\n') else {
        anyhow::bail!("event sequence must end with LF");
    };
    let mut ids = Vec::new();
    for (index, id) in body.split('\n').enumerate() {
        if ids.len() == MAX_SEQUENCE_EVENTS {
            anyhow::bail!("event sequence exceeds {MAX_SEQUENCE_EVENTS} entries");
        }
        if !meta::is_event_id(id) {
            anyhow::bail!(
                "event sequence line {} must be exactly 40 lowercase hex characters; got `{id}`",
                index + 1
            );
        }
        ids.push(id.to_owned());
    }
    Ok(ids)
}

/// Encode an event id sequence into canonical `LOG` / `VIEW` bytes.
pub fn sequence_text(ids: &[String]) -> Result<String> {
    let mut text = String::new();
    for (index, id) in ids.iter().enumerate() {
        if index == MAX_SEQUENCE_EVENTS {
            anyhow::bail!("event sequence exceeds {MAX_SEQUENCE_EVENTS} entries");
        }
        meta::event_path(id).with_context(|| format!("invalid event id at index {index}"))?;
        text.push_str(id);
        text.push('\n');
    }
    Ok(text)
}

/// Normalize the agit-managed v1 attributes block to the end of the file.
///
/// A managed block already present is replaced and unrelated user rules stay ahead of it. The
/// return value always ends with LF, and repeating the call on the same input is idempotent.
pub fn attributes_text(existing: Option<&str>) -> String {
    attributes_text_impl(existing, false).expect("lenient attributes normalization cannot fail")
}

/// Strict attributes normalization for every path that will write a tree or worktree.
///
/// An unmatched/nested managed marker is corruption, not permission to discard everything after
/// it. Callers that mutate storage must propagate this error and leave the original file intact.
pub fn attributes_text_strict(existing: Option<&str>) -> Result<String> {
    attributes_text_impl(existing, true)
}

fn attributes_text_impl(existing: Option<&str>, reject_malformed: bool) -> Result<String> {
    let original = existing.unwrap_or_default();
    let blocks = [
        (LEGACY_ATTRIBUTES_BEGIN, LEGACY_ATTRIBUTES_END),
        (DEFAULTS_BEGIN, DEFAULTS_END),
        (OBJECTS_BEGIN, OBJECTS_END),
    ];
    for (begin, end) in blocks {
        if let Err(error) = validate_attributes_blocks(original, begin, end) {
            if reject_malformed {
                return Err(error);
            }
            // The compatibility/preview API remains infallible, but it must never reproduce the
            // historical data-loss behavior. Preserve every original byte and append a clean
            // managed block; strict mutation callers will still refuse until the bad marker is
            // repaired by the user.
            return Ok(render_attributes_preserving(original));
        }
    }

    let mut unrelated = existing.unwrap_or_default().to_owned();
    for (begin, end) in blocks {
        remove_attributes_block(&mut unrelated, begin, end);
    }
    Ok(render_attributes(&unrelated))
}

fn render_attributes(unrelated: &str) -> String {
    let unrelated = unrelated.trim_matches(['\r', '\n']);
    if unrelated.is_empty() {
        format!("{DEFAULTS_CONTENT}\n{OBJECTS_CONTENT}")
    } else {
        // Git attributes are last-match-wins per attribute. Put the ordinary-text default first,
        // user rules second, and the content-addressed storage exceptions last. This preserves
        // e.g. a user's `*.bin binary` while still making events unconditionally byte-stable.
        format!("{DEFAULTS_CONTENT}\n{unrelated}\n\n{OBJECTS_CONTENT}")
    }
}

fn render_attributes_preserving(unrelated: &str) -> String {
    if unrelated.is_empty() {
        return format!("{DEFAULTS_CONTENT}\n{OBJECTS_CONTENT}");
    }
    let separator = if unrelated.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{DEFAULTS_CONTENT}\n{unrelated}{separator}{OBJECTS_CONTENT}")
}

fn validate_attributes_blocks(text: &str, begin: &str, end: &str) -> Result<()> {
    let mut cursor = 0usize;
    let mut open = false;
    loop {
        let next_begin = text[cursor..].find(begin).map(|offset| cursor + offset);
        let next_end = text[cursor..].find(end).map(|offset| cursor + offset);
        let next = match (next_begin, next_end) {
            (None, None) => break,
            (Some(position), None) => (position, true),
            (None, Some(position)) => (position, false),
            (Some(begin_position), Some(end_position)) if begin_position < end_position => {
                (begin_position, true)
            }
            (Some(_), Some(end_position)) => (end_position, false),
        };
        match (open, next.1) {
            (false, true) => open = true,
            (true, false) => open = false,
            (false, false) => anyhow::bail!("managed attributes end marker `{end}` has no begin"),
            (true, true) => anyhow::bail!("managed attributes begin marker `{begin}` is nested"),
        }
        cursor = next.0 + if next.1 { begin.len() } else { end.len() };
    }
    if open {
        anyhow::bail!("managed attributes begin marker `{begin}` has no end marker `{end}`");
    }
    Ok(())
}

fn remove_attributes_block(text: &mut String, begin: &str, end_marker: &str) {
    while let Some(start) = text.find(begin) {
        let search_from = start + begin.len();
        let end = text[search_from..]
            .find(end_marker)
            .map(|relative| search_from + relative + end_marker.len())
            .unwrap_or(text.len());
        let end = if text.as_bytes().get(end) == Some(&b'\r')
            && text.as_bytes().get(end + 1) == Some(&b'\n')
        {
            end + 2
        } else if text.as_bytes().get(end) == Some(&b'\n') {
            end + 1
        } else {
            end
        };
        text.replace_range(start..end, "");
    }
}

/// Write the agit-managed `.gitattributes` rules.
pub fn ensure_attributes(root: &Path) -> Result<PathBuf> {
    ensure_storage_root(root)?;
    let path = root.join(meta::ATTRS_FILE);
    let current = read_optional_regular_text(&path)?.unwrap_or_default();
    let next = attributes_text_strict(Some(&current))?;
    write_if_changed(&path, next.as_bytes())?;
    Ok(path)
}

/// Compatibility alias for [`ensure_attributes`].
pub fn ensure_gitattributes(root: &Path) -> Result<PathBuf> {
    ensure_attributes(root)
}

/// Pure-function v1 snapshot encoding.
///
/// Returns the LOG / VIEW sequence blobs and the deduplicated event files; meta and
/// `.gitattributes` are managed separately by the caller (the latter merges into the existing tree
/// content through [`attributes_text`]).
pub fn snapshot_files(
    log_envelopes: &str,
    view_envelopes: &str,
) -> Result<BTreeMap<String, Vec<u8>>> {
    validate_envelope_input_bounds("LOG", log_envelopes)?;
    validate_envelope_input_bounds("VIEW", view_envelopes)?;
    let log = parse_envelopes(log_envelopes).context("invalid LOG envelope JSONL")?;
    let view = parse_envelopes(view_envelopes).context("invalid VIEW envelope JSONL")?;

    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut log_ids = Vec::with_capacity(log.len());
    let mut unique_event_bytes = 0usize;
    for envelope in &log {
        let line = envelope_line(envelope);
        if line.len() > MAX_EVENT_BYTES {
            anyhow::bail!(
                "event is {} bytes, above the {MAX_EVENT_BYTES}-byte limit",
                line.len()
            );
        }
        let id = event_id(&line)?;
        let path = meta::event_path(&id)?;
        match files.entry(path) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                unique_event_bytes = unique_event_bytes
                    .checked_add(line.len())
                    .context("unique event size overflow")?;
                if unique_event_bytes > MAX_MATERIALIZED_BYTES {
                    anyhow::bail!(
                        "unique event bytes exceed the {MAX_MATERIALIZED_BYTES}-byte snapshot limit"
                    );
                }
                entry.insert(line.into_bytes());
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if entry.get().as_slice() != line.as_bytes() =>
            {
                anyhow::bail!("event id collision for {id}");
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        log_ids.push(id);
    }

    let reachable: HashSet<&str> = log_ids.iter().map(String::as_str).collect();
    let mut view_ids = Vec::with_capacity(view.len());
    for envelope in &view {
        let line = envelope_line(envelope);
        if line.len() > MAX_EVENT_BYTES {
            anyhow::bail!(
                "VIEW event is {} bytes, above the {MAX_EVENT_BYTES}-byte limit",
                line.len()
            );
        }
        let id = event_id(&line)?;
        if !reachable.contains(id.as_str()) {
            anyhow::bail!("VIEW references event {id} which is not reachable from LOG");
        }
        let path = meta::event_path(&id)?;
        match files.get(&path) {
            Some(log_line) if log_line == line.as_bytes() => {}
            Some(_) => anyhow::bail!("event id collision for {id}"),
            None => anyhow::bail!("VIEW references event {id} which has no LOG object"),
        }
        view_ids.push(id);
    }
    files.insert(
        meta::LOG_FILE.to_owned(),
        sequence_text(&log_ids)?.into_bytes(),
    );
    files.insert(
        meta::VIEW_FILE.to_owned(),
        sequence_text(&view_ids)?.into_bytes(),
    );
    Ok(files)
}

fn validate_envelope_input_bounds(label: &str, text: &str) -> Result<()> {
    validate_envelope_input_bounds_with_limits(
        label,
        text,
        MAX_EVENT_BYTES,
        MAX_MATERIALIZED_BYTES,
        MAX_SEQUENCE_EVENTS,
    )
}

/// Bound every raw line before serde sees it. In particular, the aggregate snapshot limit is not
/// a substitute for the per-event limit: otherwise a single nearly-512 MiB JSON value would be
/// parsed and allocated before [`snapshot_files`] eventually rejected its serialized envelope.
fn validate_envelope_input_bounds_with_limits(
    label: &str,
    text: &str,
    max_event_bytes: usize,
    max_materialized_bytes: usize,
    max_events: usize,
) -> Result<()> {
    if text.len() > max_materialized_bytes {
        anyhow::bail!("{label} exceeds the {max_materialized_bytes}-byte snapshot limit");
    }

    let mut line_bytes = 0usize;
    let mut events = 0usize;
    for byte in text.bytes() {
        line_bytes = line_bytes
            .checked_add(1)
            .context("envelope line size overflow")?;
        if line_bytes > max_event_bytes {
            anyhow::bail!("{label} event exceeds the {max_event_bytes}-byte limit");
        }
        if byte == b'\n' {
            events = events.checked_add(1).context("event count overflow")?;
            if events > max_events {
                anyhow::bail!("{label} exceeds {max_events} events");
            }
            line_bytes = 0;
        }
    }
    Ok(())
}

/// Append the envelopes a legacy layout holds only in VIEW to LOG, so it satisfies the v1
/// reachability constraint.
///
/// Under v0, merge/cherry-pick/revert may leave a marker, a summary or a selected source line
/// present only in VIEW. Appending follows VIEW order, and one full envelope needs to appear in LOG
/// only once; VIEW's own order and repeats are kept unchanged by the caller.
pub fn make_view_reachable(log: &str, view: &str) -> Result<String> {
    let mut out = log.to_owned();
    let mut reachable: HashSet<String> = parse_envelopes(log)?
        .iter()
        .map(envelope_line)
        .map(|line| event_id(&line))
        .collect::<Result<_>>()?;
    for envelope in parse_envelopes(view)? {
        let line = envelope_line(&envelope);
        if reachable.insert(event_id(&line)?) {
            out.push_str(&line);
        }
    }
    Ok(out)
}

/// Write two full envelope JSONL inputs as a v1 worktree.
///
/// Event files are add-only: one that already exists with identical bytes is skipped; differing
/// bytes under the same id fail immediately. Every event id VIEW references must be reachable from
/// LOG.
pub fn write_snapshot(root: &Path, log_env: &str, view_env: &str) -> Result<()> {
    let files = snapshot_files(log_env, view_env)?;
    ensure_storage_root(root)?;
    meta::ensure_write_safe(root)?;

    // Validate every mutable destination and the attributes source before publishing even one
    // immutable object. A symlinked `.gitattributes`/LOG/VIEW must not be followed, and malformed
    // managed markers must leave both user bytes and storage bytes untouched.
    let attributes_path = root.join(meta::ATTRS_FILE);
    let existing_attributes = read_optional_regular_text(&attributes_path)?.unwrap_or_default();
    let next_attributes = attributes_text_strict(Some(&existing_attributes))?;
    for sequence in [meta::LOG_FILE, meta::VIEW_FILE] {
        ensure_regular_file_or_missing(&root.join(sequence))?;
    }
    let legacy_paths = [meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE].map(|relative| {
        let path = root.join(relative);
        ensure_regular_file_or_missing(&path).map(|exists| (path, exists))
    });
    let legacy_paths = legacy_paths
        .into_iter()
        .collect::<Result<Vec<(PathBuf, bool)>>>()?;

    // Inspect every existing object before writing anything, including each ancestor via
    // symlink_metadata. Missing shard directories are created only later, one component at a
    // time, by write_event_once.
    for (relative, bytes) in files.iter().filter(|(path, _)| path.starts_with("events/")) {
        let id = relative
            .rsplit('/')
            .next()
            .expect("event path has filename");
        let path = event_destination(root, id, false)?;
        if ensure_regular_file_or_missing(&path)? {
            let existing = read_bytes_capped(&path, MAX_EVENT_BYTES)?;
            if existing != *bytes {
                anyhow::bail!("existing event {} has different bytes", path.display());
            }
        }
    }

    // Publish immutable objects before either sequence can name them. Individual files are also
    // atomically installed below, so an interrupted refresh can be retried without accepting a
    // partially-written object at its final content address.
    for (relative, bytes) in files.iter().filter(|(path, _)| path.starts_with("events/")) {
        let id = relative
            .rsplit('/')
            .next()
            .expect("event path has filename");
        write_event_once(root, id, bytes)?;
    }
    for (relative, bytes) in files
        .iter()
        .filter(|(path, _)| !path.starts_with("events/"))
    {
        write_if_changed(&root.join(relative), bytes)?;
    }
    for (path, existed) in legacy_paths {
        if !existed {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot remove legacy storage {}", path.display()));
            }
        }
    }
    write_if_changed(&attributes_path, next_attributes.as_bytes())?;
    Ok(())
}

/// Materialize LOG / VIEW back into full envelope JSONL, following the layout in the worktree
/// meta.
pub fn materialize_worktree(root: &Path, seq_file: &str) -> Result<String> {
    let kind = SequenceKind::parse(seq_file)?;
    let layout = meta::resolve(root)?.layout;
    match layout {
        LayoutVersion::V0 => {
            let path = root.join(kind.path(layout));
            ensure_regular_file_or_missing(&path)?;
            let text = read_text_capped(&path, MAX_MATERIALIZED_BYTES)?;
            canonical_v0(&text).with_context(|| format!("invalid v0 file {}", path.display()))
        }
        LayoutVersion::V1 => {
            let path = root.join(kind.path(layout));
            ensure_regular_file_or_missing(&path)?;
            let sequence = read_text_capped(&path, MAX_MATERIALIZED_BYTES)?;
            let ids = parse_sequence(&sequence)
                .with_context(|| format!("invalid v1 sequence {}", path.display()))?;
            if kind == SequenceKind::View {
                let log_path = root.join(meta::LOG_FILE);
                ensure_regular_file_or_missing(&log_path)?;
                let log_sequence = read_text_capped(&log_path, MAX_MATERIALIZED_BYTES)?;
                let log_ids = parse_sequence(&log_sequence)
                    .with_context(|| format!("invalid v1 sequence {}", log_path.display()))?;
                ensure_view_reachable(&ids, &log_ids)?;
            }
            materialize_worktree_ids(root, &ids)
        }
    }
}

/// Materialize LOG / VIEW back into full envelope JSONL, following the layout in the meta at a
/// Git ref.
///
/// v1 events are read in bulk through one `git cat-file --batch` process and a repeated id is read
/// once, while the output still preserves the order and the repeats of the sequence exactly.
pub fn materialize_at(repo_root: &Path, git_ref: &str, seq_file: &str) -> Result<String> {
    if git_ref.is_empty()
        || git_ref.len() > 1024
        || git_ref.starts_with('-')
        || git_ref.contains(['\n', '\r'])
    {
        anyhow::bail!("git ref must be a bounded non-option string without newlines");
    }
    // Freeze symbolic refs once. Otherwise a concurrently moving branch could supply meta/LOG from
    // one commit and event objects from another; using the short immutable OID also bounds every
    // batch input record independently of caller-controlled ref text.
    let commit = resolve_commit(repo_root, git_ref)?;
    let kind = SequenceKind::parse(seq_file)?;
    let meta_bytes = git_blob_at(repo_root, &commit, meta::FILE, MAX_EVENT_BYTES)?;
    let snapshot: meta::Meta = serde_json::from_slice(&meta_bytes)
        .context("invalid session/meta.json at requested ref")?;

    match snapshot.layout {
        LayoutVersion::V0 => {
            let path = kind.path(LayoutVersion::V0);
            let bytes = git_blob_at(repo_root, &commit, path, MAX_MATERIALIZED_BYTES)?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("{git_ref}:{path} is not UTF-8"))?;
            canonical_v0(&text).with_context(|| format!("invalid v0 file {git_ref}:{path}"))
        }
        LayoutVersion::V1 => {
            let path = kind.path(LayoutVersion::V1);
            let bytes = git_blob_at(repo_root, &commit, path, MAX_MATERIALIZED_BYTES)?;
            let sequence = String::from_utf8(bytes)
                .with_context(|| format!("{git_ref}:{path} is not UTF-8"))?;
            let ids = parse_sequence(&sequence)
                .with_context(|| format!("invalid v1 sequence {git_ref}:{path}"))?;
            if kind == SequenceKind::View {
                let log_bytes =
                    git_blob_at(repo_root, &commit, meta::LOG_FILE, MAX_MATERIALIZED_BYTES)?;
                let log_sequence = String::from_utf8(log_bytes)
                    .with_context(|| format!("{git_ref}:{} is not UTF-8", meta::LOG_FILE))?;
                let log_ids = parse_sequence(&log_sequence)
                    .with_context(|| format!("invalid v1 sequence {git_ref}:{}", meta::LOG_FILE))?;
                ensure_view_reachable(&ids, &log_ids)?;
            }
            materialize_ids_at(repo_root, &commit, &ids)
        }
    }
}

/// Materialize only the **first event** of the sequence (v1 reads only the hash list and the
/// first object; v0 streams to the first newline and canonicalizes only the first envelope).
/// Picking up the Codex bootstrap needs just this line, and materializing the whole LOG for it
/// swallows back the startup cost the compact VIEW saves.
pub fn materialize_head_at(
    repo_root: &Path,
    git_ref: &str,
    seq_file: &str,
) -> Result<Option<String>> {
    if git_ref.is_empty()
        || git_ref.len() > 1024
        || git_ref.starts_with('-')
        || git_ref.contains(['\n', '\r'])
    {
        anyhow::bail!("git ref must be a bounded non-option string without newlines");
    }
    let commit = resolve_commit(repo_root, git_ref)?;
    let kind = SequenceKind::parse(seq_file)?;
    let meta_bytes = git_blob_at(repo_root, &commit, meta::FILE, MAX_EVENT_BYTES)?;
    let snapshot: meta::Meta = serde_json::from_slice(&meta_bytes)
        .context("invalid session/meta.json at requested ref")?;
    match snapshot.layout {
        LayoutVersion::V0 => {
            // v0 is a single-file layout, but taking the first line still must not read the
            // whole blob into memory: stream up to the first newline, with the single-line budget
            // taken from the event limit.
            let path = kind.path(LayoutVersion::V0);
            let Some(first) = git_blob_first_line(repo_root, &commit, path, MAX_EVENT_BYTES)?
            else {
                return Ok(None);
            };
            let envelope = parse_legacy_envelope_line(&format!("{first}\n"))
                .with_context(|| format!("invalid v0 first line at {git_ref}:{path}"))?;
            Ok(Some(envelope_line(&envelope).trim_end().to_string()))
        }
        LayoutVersion::V1 => {
            let path = kind.path(LayoutVersion::V1);
            let bytes = git_blob_at(repo_root, &commit, path, MAX_MATERIALIZED_BYTES)?;
            let sequence = String::from_utf8(bytes)
                .with_context(|| format!("{git_ref}:{path} is not UTF-8"))?;
            let ids = parse_sequence(&sequence)
                .with_context(|| format!("invalid v1 sequence {git_ref}:{path}"))?;
            let Some(first) = ids.first() else {
                return Ok(None);
            };
            let one = materialize_ids_at(repo_root, &commit, std::slice::from_ref(first))?;
            Ok(one.lines().next().map(str::to_string))
        }
    }
}

/// Materialize LOG and VIEW from one frozen commit under independent result budgets.
///
/// v1 checks the union of referenced objects once and reads every unique body once. v0 performs a
/// streaming canonical-size pass before allocating either result, then streams the same immutable
/// blobs a second time into their exact final buffers. Each result is bounded by
/// [`MAX_MATERIALIZED_BYTES`], so a pair may retain at most twice that amount of result data; the
/// deduplicated v1 event union remains bounded by [`MAX_MATERIALIZED_BYTES`].
pub fn materialize_pair_at(repo_root: &Path, git_ref: &str) -> Result<(String, String)> {
    materialize_pair_at_with_limits(
        repo_root,
        git_ref,
        MAX_MATERIALIZED_BYTES,
        MAX_MATERIALIZED_BYTES,
    )
}

fn materialize_pair_at_with_limits(
    repo_root: &Path,
    git_ref: &str,
    max_sequence_bytes: usize,
    max_unique_event_bytes: usize,
) -> Result<(String, String)> {
    if git_ref.is_empty()
        || git_ref.len() > 1024
        || git_ref.starts_with('-')
        || git_ref.contains(['\n', '\r'])
    {
        anyhow::bail!("git ref must be a bounded non-option string without newlines");
    }
    let commit = resolve_commit(repo_root, git_ref)?;
    let meta_bytes = git_blob_at(repo_root, &commit, meta::FILE, MAX_EVENT_BYTES)?;
    let snapshot: meta::Meta = serde_json::from_slice(&meta_bytes)
        .context("invalid session/meta.json at requested ref")?;

    match snapshot.layout {
        LayoutVersion::V0 => materialize_v0_pair_at(repo_root, &commit, max_sequence_bytes)
            .with_context(|| format!("invalid v0 storage at {git_ref}")),
        LayoutVersion::V1 => {
            let log_bytes =
                git_blob_at(repo_root, &commit, meta::LOG_FILE, MAX_MATERIALIZED_BYTES)?;
            let log_sequence = String::from_utf8(log_bytes)
                .with_context(|| format!("{git_ref}:{} is not UTF-8", meta::LOG_FILE))?;
            let log_ids = parse_sequence(&log_sequence)
                .with_context(|| format!("invalid v1 sequence {git_ref}:{}", meta::LOG_FILE))?;
            drop(log_sequence);

            let view_bytes =
                git_blob_at(repo_root, &commit, meta::VIEW_FILE, MAX_MATERIALIZED_BYTES)?;
            let view_sequence = String::from_utf8(view_bytes)
                .with_context(|| format!("{git_ref}:{} is not UTF-8", meta::VIEW_FILE))?;
            let view_ids = parse_sequence(&view_sequence)
                .with_context(|| format!("invalid v1 sequence {git_ref}:{}", meta::VIEW_FILE))?;
            drop(view_sequence);

            materialize_pair_ids_with_limits(
                &log_ids,
                &view_ids,
                MAX_EVENT_BYTES,
                max_sequence_bytes,
                max_unique_event_bytes,
                |unique| inspect_git_event_sizes(repo_root, &commit, unique),
                |unique, sizes, first_offsets, output| {
                    read_git_events_into_output(
                        repo_root,
                        &commit,
                        unique,
                        sizes,
                        first_offsets,
                        output,
                    )
                },
            )
        }
    }
}

fn materialize_v0_pair_at(
    repo_root: &Path,
    commit: &str,
    max_sequence_bytes: usize,
) -> Result<(String, String)> {
    let mut sizes = [0usize; 2];
    visit_v0_pair(repo_root, commit, |sequence, canonical| {
        sizes[sequence] = sizes[sequence]
            .checked_add(canonical.len())
            .context("canonical v0 sequence size overflow")?;
        anyhow::ensure!(
            sizes[sequence] <= max_sequence_bytes,
            "materialized transcript exceeds the {max_sequence_bytes}-byte limit"
        );
        Ok(())
    })?;
    validate_pair_result_bound(sizes[0], sizes[1], max_sequence_bytes)?;

    let mut log = allocate_materialization_output(sizes[0])?;
    let mut view = allocate_materialization_output(sizes[1])?;
    let mut offsets = [0usize; 2];
    visit_v0_pair(repo_root, commit, |sequence, canonical| {
        let (output, offset) = match sequence {
            0 => (&mut log, &mut offsets[0]),
            1 => (&mut view, &mut offsets[1]),
            _ => unreachable!("v0 pair has exactly LOG and VIEW"),
        };
        let end = offset
            .checked_add(canonical.len())
            .context("canonical v0 output offset overflow")?;
        output
            .get_mut(*offset..end)
            .context("canonical v0 output exceeded its preflight size")?
            .copy_from_slice(canonical.as_bytes());
        *offset = end;
        Ok(())
    })?;
    anyhow::ensure!(
        offsets == sizes,
        "canonical v0 output size changed between immutable passes"
    );
    Ok((
        String::from_utf8(log).context("canonical v0 LOG is not UTF-8")?,
        String::from_utf8(view).context("canonical v0 VIEW is not UTF-8")?,
    ))
}

fn visit_v0_pair(
    repo_root: &Path,
    commit: &str,
    mut visit: impl FnMut(usize, &str) -> Result<()>,
) -> Result<()> {
    visit_v0_pair_at_with_limits(
        repo_root,
        commit,
        MAX_EVENT_BYTES,
        MAX_MATERIALIZED_BYTES,
        MAX_SEQUENCE_EVENTS,
        |kind, _, canonical| {
            visit(
                match kind {
                    SequenceKind::Log => 0,
                    SequenceKind::View => 1,
                },
                canonical,
            )
        },
    )
}

/// Stream both legacy transcript blobs from one immutable commit and one Git batch process.
///
/// Every raw line is bounded before JSON parsing and only its canonical v1 wire form is passed to
/// the visitor. Migration uses the limit-aware entry point to spool a complete v1 snapshot without
/// ever materializing either legacy sequence in memory.
pub(crate) fn visit_v0_pair_at_with_limits(
    repo_root: &Path,
    commit: &str,
    max_event_bytes: usize,
    max_blob_bytes: usize,
    max_events: usize,
    mut visit: impl FnMut(SequenceKind, usize, &str) -> Result<()>,
) -> Result<()> {
    anyhow::ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "legacy pair reader requires an immutable commit object id"
    );
    const PATHS: [(SequenceKind, &str); 2] = [
        (SequenceKind::Log, meta::LEGACY_LOG_FILE),
        (SequenceKind::View, meta::LEGACY_VIEW_FILE),
    ];
    with_legacy_pair_batch(repo_root, commit, |reader| {
        for (sequence, path) in PATHS {
            let spec = format!("{commit}:{path}");
            let header = read_batch_header(reader)?;
            let mut remaining = legacy_blob_size_from_header(&spec, &header, max_blob_bytes)?;
            let mut line_number = 0usize;
            while let Some(raw) = read_bounded_blob_line(reader, &mut remaining, max_event_bytes)? {
                if line_number == max_events {
                    anyhow::bail!("{path} exceeds {max_events} events");
                }
                line_number += 1;
                let line = std::str::from_utf8(&raw)
                    .with_context(|| format!("{spec} line {line_number} is not UTF-8"))?;
                let envelope = parse_legacy_envelope_line(line)
                    .with_context(|| format!("invalid {spec} envelope at line {line_number}"))?;
                let canonical = envelope_line(&envelope);
                anyhow::ensure!(
                    canonical.len() <= max_event_bytes,
                    "{spec} line {line_number} canonicalizes above the {max_event_bytes}-byte event limit"
                );
                visit(sequence, raw.len(), &canonical)?;
            }
            let mut separator = [0u8; 1];
            reader
                .read_exact(&mut separator)
                .with_context(|| format!("cannot read git batch separator after {spec}"))?;
            anyhow::ensure!(
                separator == *b"\n",
                "git cat-file --batch omitted the separator after {spec}"
            );
        }
        Ok(())
    })
}

fn with_legacy_pair_batch<T>(
    repo_root: &Path,
    commit: &str,
    consume: impl FnOnce(&mut BufReader<std::process::ChildStdout>) -> Result<T>,
) -> Result<T> {
    let specs = [
        format!("{commit}:{}", meta::LEGACY_LOG_FILE),
        format!("{commit}:{}", meta::LEGACY_VIEW_FILE),
    ];
    let mut child = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start git cat-file --batch for v0 pair")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || -> Result<()> {
        for spec in specs {
            stdin
                .write_all(spec.as_bytes())
                .context("cannot send v0 request to git cat-file")?;
            stdin
                .write_all(b"\n")
                .context("cannot terminate v0 request to git cat-file")?;
        }
        stdin
            .flush()
            .context("cannot flush v0 git cat-file requests")
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let parsed = consume(&mut reader);
    if parsed.is_err() {
        let _ = child.kill();
    }
    let writer_result = writer
        .join()
        .map_err(|_| anyhow::anyhow!("v0 git cat-file input writer panicked"))?;
    let output = child
        .wait_with_output()
        .context("cannot wait for v0 git cat-file --batch")?;
    let value = parsed?;
    writer_result?;
    if !output.status.success() {
        anyhow::bail!(
            "v0 git cat-file --batch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(value)
}

fn legacy_blob_size_from_header(spec: &str, header: &str, max_blob_bytes: usize) -> Result<usize> {
    if header.ends_with(" missing") {
        anyhow::bail!("{spec} is missing");
    }
    let mut fields = header.split_whitespace();
    let oid = fields.next().context("v0 batch header omitted object id")?;
    let object_type = fields
        .next()
        .context("v0 batch header omitted object type")?;
    let size: u64 = fields
        .next()
        .context("v0 batch header omitted object size")?
        .parse()
        .context("v0 batch header contained an invalid object size")?;
    anyhow::ensure!(
        fields.next().is_none()
            && matches!(oid.len(), 40 | 64)
            && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "unexpected v0 git cat-file batch header `{header}`"
    );
    anyhow::ensure!(
        object_type == "blob",
        "{spec} is a {object_type}, not a blob"
    );
    anyhow::ensure!(
        size <= max_blob_bytes as u64,
        "{spec} exceeds the {max_blob_bytes}-byte read cap"
    );
    usize::try_from(size).context("v0 blob size does not fit memory")
}

fn read_bounded_blob_line<R: BufRead>(
    reader: &mut R,
    remaining: &mut usize,
    max_line_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    if *remaining == 0 {
        return Ok(None);
    }
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        anyhow::ensure!(!available.is_empty(), "git cat-file ended inside a v0 blob");
        let available = &available[..available.len().min(*remaining)];
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        anyhow::ensure!(
            line.len().saturating_add(take) <= max_line_bytes,
            "v0 event exceeds the {max_line_bytes}-byte limit"
        );
        line.try_reserve(take)
            .context("cannot allocate bounded v0 event line")?;
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        *remaining -= take;
        if newline.is_some() || *remaining == 0 {
            return Ok(Some(line));
        }
    }
}

fn ensure_view_reachable(view: &[String], log: &[String]) -> Result<()> {
    let reachable: HashSet<&str> = log.iter().map(String::as_str).collect();
    if let Some(id) = view.iter().find(|id| !reachable.contains(id.as_str())) {
        anyhow::bail!("VIEW references event {id} which is not reachable from LOG");
    }
    Ok(())
}

fn resolve_commit(repo_root: &Path, git_ref: &str) -> Result<String> {
    let expression = format!("{git_ref}^{{commit}}");
    let output = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--verify", &expression])
        .output()
        .with_context(|| format!("cannot resolve {git_ref}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "cannot resolve {git_ref}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let commit = String::from_utf8(output.stdout)
        .context("git rev-parse returned non-UTF-8 output")?
        .trim()
        .to_owned();
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("git rev-parse returned an invalid commit object id");
    }
    Ok(commit)
}

fn canonical_v0(text: &str) -> Result<String> {
    if text.is_empty() {
        return Ok(String::new());
    }
    if !text.ends_with('\n') {
        anyhow::bail!("legacy envelope JSONL must end with LF");
    }
    text.split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| {
            parse_legacy_envelope_line(line)
                .map(|envelope| envelope_line(&envelope))
                .with_context(|| format!("invalid legacy envelope at line {}", index + 1))
        })
        .collect()
}

/// Size every unique event before allocating its expanded output, then read each unique body once
/// directly into that output. Duplicate occurrences are copied from the already-validated first
/// occurrence, so the materializer never keeps a second transcript-sized body cache alive.
fn materialize_ids_with_limits<'a>(
    ids: &'a [String],
    max_event_bytes: usize,
    max_total_bytes: usize,
    inspect: impl FnOnce(&[&'a str]) -> Result<Vec<usize>>,
    fill: impl FnOnce(&[&'a str], &[usize], &[usize], &mut [u8]) -> Result<()>,
) -> Result<String> {
    if ids.is_empty() {
        return Ok(String::new());
    }

    let (unique, indexes) = index_unique_ids(ids)?;
    let sizes = inspect(&unique)?;
    validate_event_sizes(&unique, &sizes, max_event_bytes)?;
    let (first_offsets, total) = sequence_layout(ids, &indexes, &sizes, max_total_bytes)?;
    let mut output = allocate_materialization_output(total)?;
    fill(&unique, &sizes, &first_offsets, &mut output)?;
    validate_unique_output(&unique, &sizes, &first_offsets, &output, max_event_bytes)?;
    expand_sequence(ids, &indexes, &sizes, &first_offsets, &mut output)?;
    String::from_utf8(output).context("validated events did not compose as UTF-8")
}

fn materialize_pair_ids_with_limits<'a>(
    log_ids: &'a [String],
    view_ids: &[String],
    max_event_bytes: usize,
    max_sequence_bytes: usize,
    max_unique_event_bytes: usize,
    inspect: impl FnOnce(&[&'a str]) -> Result<Vec<usize>>,
    fill_log: impl FnOnce(&[&'a str], &[usize], &[usize], &mut [u8]) -> Result<()>,
) -> Result<(String, String)> {
    let (unique, indexes) = index_unique_ids(log_ids)?;
    if let Some(id) = view_ids
        .iter()
        .find(|id| !indexes.contains_key(id.as_str()))
    {
        anyhow::bail!("VIEW references event {id} which is not reachable from LOG");
    }
    let sizes = if unique.is_empty() {
        Vec::new()
    } else {
        inspect(&unique)?
    };
    validate_event_sizes(&unique, &sizes, max_event_bytes)?;
    validate_unique_event_bytes(&sizes, max_unique_event_bytes)?;
    let (log_first_offsets, log_bytes) =
        sequence_layout(log_ids, &indexes, &sizes, max_sequence_bytes)?;
    let view_bytes = expanded_sequence_size(view_ids, &indexes, &sizes, max_sequence_bytes)?;
    validate_pair_result_bound(log_bytes, view_bytes, max_sequence_bytes)?;

    // Both allocations happen only after the independent sequence sizes and the explicit 2x
    // process bound are known. If either reservation fails, no event body has been requested yet
    // and the other allocation is dropped on return. Unique bodies are then read only into their
    // first LOG occurrence; VIEW copies from those validated ranges without a second body cache.
    let mut log = allocate_materialization_output(log_bytes)?;
    let mut view = allocate_materialization_output(view_bytes)?;
    if !unique.is_empty() {
        fill_log(&unique, &sizes, &log_first_offsets, &mut log)?;
        validate_unique_output(&unique, &sizes, &log_first_offsets, &log, max_event_bytes)?;
        expand_sequence(log_ids, &indexes, &sizes, &log_first_offsets, &mut log)?;
    }

    let mut offset = 0usize;
    for id in view_ids {
        let index = indexes[id.as_str()];
        let size = sizes[index];
        let source = log_first_offsets[index];
        let source_end = source
            .checked_add(size)
            .context("VIEW event source range overflow")?;
        let target_end = offset
            .checked_add(size)
            .context("VIEW event target range overflow")?;
        view.get_mut(offset..target_end)
            .context("VIEW event target range is out of bounds")?
            .copy_from_slice(
                log.get(source..source_end)
                    .context("VIEW event source range is out of bounds")?,
            );
        offset = target_end;
    }

    Ok((
        String::from_utf8(log).context("validated LOG events did not compose as UTF-8")?,
        String::from_utf8(view).context("validated VIEW events did not compose as UTF-8")?,
    ))
}

fn index_unique_ids(ids: &[String]) -> Result<(Vec<&str>, HashMap<&str, usize>)> {
    let mut unique = Vec::new();
    let mut indexes = HashMap::new();
    for id in ids {
        indexes
            .try_reserve(1)
            .context("cannot allocate materialization event index")?;
        if let std::collections::hash_map::Entry::Vacant(entry) = indexes.entry(id.as_str()) {
            unique
                .try_reserve(1)
                .context("cannot allocate unique event index")?;
            let index = unique.len();
            entry.insert(index);
            unique.push(id.as_str());
        }
    }
    Ok((unique, indexes))
}

fn validate_event_sizes(ids: &[&str], sizes: &[usize], max_event_bytes: usize) -> Result<()> {
    anyhow::ensure!(
        sizes.len() == ids.len(),
        "event size preflight returned {} results for {} unique events",
        sizes.len(),
        ids.len()
    );
    for (id, size) in ids.iter().zip(sizes) {
        if *size > max_event_bytes {
            anyhow::bail!("event {id} is {size} bytes, above the {max_event_bytes}-byte limit");
        }
    }
    Ok(())
}

fn validate_unique_event_bytes(sizes: &[usize], max_unique_event_bytes: usize) -> Result<()> {
    let mut total = 0usize;
    for size in sizes {
        total = total
            .checked_add(*size)
            .context("unique event byte count overflow")?;
        anyhow::ensure!(
            total <= max_unique_event_bytes,
            "unique event bytes exceed the {max_unique_event_bytes}-byte snapshot limit"
        );
    }
    Ok(())
}

fn validate_pair_result_bound(
    log_bytes: usize,
    view_bytes: usize,
    max_sequence_bytes: usize,
) -> Result<()> {
    let pair_bytes = log_bytes
        .checked_add(view_bytes)
        .context("LOG and VIEW materialized size overflow")?;
    let max_pair_bytes = max_sequence_bytes
        .checked_mul(2)
        .context("paired materialization byte limit overflow")?;
    anyhow::ensure!(
        pair_bytes <= max_pair_bytes,
        "LOG and VIEW require {pair_bytes} result bytes, above the explicit {max_pair_bytes}-byte process bound"
    );
    Ok(())
}

fn sequence_layout(
    ids: &[String],
    indexes: &HashMap<&str, usize>,
    sizes: &[usize],
    max_total_bytes: usize,
) -> Result<(Vec<usize>, usize)> {
    let mut first_offsets = Vec::new();
    first_offsets
        .try_reserve_exact(sizes.len())
        .context("cannot allocate first-occurrence index")?;
    first_offsets.resize(sizes.len(), usize::MAX);
    let mut total = 0usize;
    for id in ids {
        let index = *indexes
            .get(id.as_str())
            .with_context(|| format!("event {id} was not indexed"))?;
        if first_offsets[index] == usize::MAX {
            first_offsets[index] = total;
        }
        total = total
            .checked_add(sizes[index])
            .context("materialized size overflow")?;
        if total > max_total_bytes {
            anyhow::bail!("materialized transcript exceeds the {max_total_bytes}-byte limit");
        }
    }
    Ok((first_offsets, total))
}

fn expanded_sequence_size(
    ids: &[String],
    indexes: &HashMap<&str, usize>,
    sizes: &[usize],
    max_total_bytes: usize,
) -> Result<usize> {
    let mut total = 0usize;
    for id in ids {
        let index = *indexes.get(id.as_str()).with_context(|| {
            format!("VIEW references event {id} which is not reachable from LOG")
        })?;
        total = total
            .checked_add(sizes[index])
            .context("materialized size overflow")?;
        if total > max_total_bytes {
            anyhow::bail!("materialized transcript exceeds the {max_total_bytes}-byte limit");
        }
    }
    Ok(total)
}

fn allocate_materialization_output(total: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(total)
        .context("cannot allocate bounded materialization output")?;
    output.resize(total, 0);
    Ok(output)
}

fn validate_unique_output(
    ids: &[&str],
    sizes: &[usize],
    first_offsets: &[usize],
    output: &[u8],
    max_event_bytes: usize,
) -> Result<()> {
    for (index, id) in ids.iter().enumerate() {
        let start = first_offsets[index];
        let end = start
            .checked_add(sizes[index])
            .context("event output range overflow")?;
        let bytes = output
            .get(start..end)
            .with_context(|| format!("event {id} output range is out of bounds"))?;
        let line =
            std::str::from_utf8(bytes).with_context(|| format!("event {id} is not UTF-8"))?;
        validate_event_for_id(id, line, max_event_bytes)?;
    }
    Ok(())
}

/// Expand first occurrences in-place. Every duplicate follows its immutable source range, so no
/// copy can overwrite a source needed by a later occurrence.
fn expand_sequence(
    ids: &[String],
    indexes: &HashMap<&str, usize>,
    sizes: &[usize],
    first_offsets: &[usize],
    output: &mut [u8],
) -> Result<()> {
    let mut offset = 0usize;
    for id in ids {
        let index = indexes[id.as_str()];
        let size = sizes[index];
        let source = first_offsets[index];
        if source != offset {
            let end = source
                .checked_add(size)
                .context("event source range overflow")?;
            output.copy_within(source..end, offset);
        }
        offset = offset
            .checked_add(size)
            .context("materialized offset overflow")?;
    }
    Ok(())
}

fn validate_event_for_id(id: &str, line: &str, max_event_bytes: usize) -> Result<()> {
    if line.len() > max_event_bytes {
        anyhow::bail!(
            "event {id} is {} bytes, above the {max_event_bytes}-byte limit",
            line.len()
        );
    }
    let actual = event_id(line).with_context(|| format!("event {id} is not a valid envelope"))?;
    if actual != id {
        anyhow::bail!("event id mismatch: sequence names {id}, envelope hashes to {actual}");
    }
    Ok(())
}

fn ensure_storage_root(root: &Path) -> Result<()> {
    ensure_real_directory(root, false)
        .with_context(|| format!("unsafe repository root {}", root.display()))
}

/// Return whether `path` exists as a real regular file. Symlinks, directories and special files
/// are errors rather than alternate spellings of a writable destination.
fn ensure_regular_file_or_missing(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(true)
        }
        Ok(_) => anyhow::bail!(
            "refusing storage path {}: expected a regular file, not a symlink/directory/special file",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

fn read_optional_regular_text(path: &Path) -> Result<Option<String>> {
    if !ensure_regular_file_or_missing(path)? {
        return Ok(None);
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {} as UTF-8", path.display()))
        .map(Some)
}

fn ensure_real_directory(path: &Path, create_if_missing: bool) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => anyhow::bail!(
            "refusing storage directory {}: an ancestor is a symlink or non-directory",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_if_missing => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot create directory {}", path.display()));
                }
            }
            // Re-inspect after creation/AlreadyExists so a raced-in symlink is never accepted.
            ensure_real_directory(path, false)
        }
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// Resolve an event destination without following any path component below the repository root.
/// In preflight mode missing directories remain untouched; publish mode creates each missing shard
/// in its already-validated real parent and verifies it again afterwards.
fn event_destination(root: &Path, id: &str, create_parents: bool) -> Result<PathBuf> {
    ensure_storage_root(root)?;
    let relative = meta::event_path(id)?;
    let relative = Path::new(&relative);
    let mut current = root.to_path_buf();
    let parent = relative.parent().expect("event path has a parent");
    for component in parent.components() {
        let std::path::Component::Normal(component) = component else {
            anyhow::bail!("event path contains a non-repository component");
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => anyhow::bail!(
                "refusing event path {}: an ancestor is a symlink or non-directory",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_parents => {
                ensure_real_directory(&current, true)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect event path {}", current.display()));
            }
        }
    }
    Ok(root.join(relative))
}

fn write_event_once(root: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    let path = event_destination(root, id, true)?;
    let parent = path.parent().expect("event path always has a parent");

    if ensure_regular_file_or_missing(&path)? {
        let existing = read_bytes_capped(&path, MAX_EVENT_BYTES)?;
        anyhow::ensure!(
            existing == bytes,
            "existing event {} does not match its event id {id}",
            path.display()
        );
        return Ok(());
    }

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create temporary event in {}", parent.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("cannot write temporary event for {id}"))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("cannot sync temporary event for {id}"))?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_regular_file_or_missing(&path)?;
            let existing = read_bytes_capped(&path, MAX_EVENT_BYTES)?;
            anyhow::ensure!(
                existing == bytes,
                "existing event {} does not match its event id {id}",
                path.display()
            );
            Ok(())
        }
        Err(error) => {
            Err(error.error).with_context(|| format!("cannot publish event {}", path.display()))
        }
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if ensure_regular_file_or_missing(path)? {
        let file =
            std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let size = file
            .metadata()
            .with_context(|| format!("cannot stat open file {}", path.display()))?
            .len();
        if size == bytes.len() as u64 && read_bytes_capped(path, bytes.len())? == bytes {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        ensure_real_directory(parent, false)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("cannot create temporary file in {}", parent.display()))?;
        temporary
            .write_all(bytes)
            .with_context(|| format!("cannot write temporary file for {}", path.display()))?;
        temporary
            .as_file()
            .sync_all()
            .with_context(|| format!("cannot sync temporary file for {}", path.display()))?;
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("cannot publish {}", path.display()))?;
        return Ok(());
    }
    anyhow::bail!("{} has no parent directory", path.display())
}

fn read_text_capped(path: &Path, limit: usize) -> Result<String> {
    let bytes = read_bytes_capped(path, limit)?;
    String::from_utf8(bytes).with_context(|| format!("cannot read {} as UTF-8", path.display()))
}

fn read_bytes_capped(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot stat open file {}", path.display()))?;
    if metadata.len() > limit as u64 {
        anyhow::bail!("{} exceeds the {limit}-byte limit", path.display());
    }

    // Read from the same open handle that was inspected above. The extra byte closes the
    // metadata/read growth race without ever allocating an attacker-controlled file size.
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(limit));
    Read::by_ref(&mut file)
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() > limit {
        anyhow::bail!("{} exceeds the {limit}-byte limit", path.display());
    }
    Ok(bytes)
}

fn materialize_worktree_ids(root: &Path, ids: &[String]) -> Result<String> {
    materialize_ids_with_limits(
        ids,
        MAX_EVENT_BYTES,
        MAX_MATERIALIZED_BYTES,
        |unique| inspect_worktree_event_sizes(root, unique),
        |unique, sizes, first_offsets, output| {
            read_worktree_events_into_output(root, unique, sizes, first_offsets, output)
        },
    )
}

fn inspect_worktree_event_sizes(root: &Path, ids: &[&str]) -> Result<Vec<usize>> {
    let mut sizes = Vec::new();
    sizes
        .try_reserve_exact(ids.len())
        .context("cannot allocate worktree event sizes")?;
    for id in ids {
        let path = event_destination(root, id, false)?;
        anyhow::ensure!(
            ensure_regular_file_or_missing(&path)?,
            "event {id} is missing at {}",
            path.display()
        );
        let file = std::fs::File::open(&path)
            .with_context(|| format!("cannot open event {}", path.display()))?;
        let size = file
            .metadata()
            .with_context(|| format!("cannot stat open event {}", path.display()))?
            .len();
        let size = usize::try_from(size).context("event size does not fit memory")?;
        sizes.push(size);
    }
    Ok(sizes)
}

fn read_worktree_events_into_output(
    root: &Path,
    ids: &[&str],
    sizes: &[usize],
    first_offsets: &[usize],
    output: &mut [u8],
) -> Result<()> {
    for (index, id) in ids.iter().enumerate() {
        let path = event_destination(root, id, false)?;
        anyhow::ensure!(
            ensure_regular_file_or_missing(&path)?,
            "event {id} is missing at {}",
            path.display()
        );
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("cannot open event {}", path.display()))?;
        let actual = usize::try_from(
            file.metadata()
                .with_context(|| format!("cannot stat open event {}", path.display()))?
                .len(),
        )
        .context("event size does not fit memory")?;
        let expected = sizes[index];
        anyhow::ensure!(
            actual == expected,
            "event {id} changed size during materialization: expected {expected}, found {actual}"
        );
        let start = first_offsets[index];
        let end = start
            .checked_add(expected)
            .context("event output range overflow")?;
        let destination = output
            .get_mut(start..end)
            .with_context(|| format!("event {id} output range is out of bounds"))?;
        file.read_exact(destination)
            .with_context(|| format!("cannot read event {}", path.display()))?;
        let mut extra = [0u8; 1];
        anyhow::ensure!(
            file.read(&mut extra)
                .with_context(|| format!("cannot finish reading event {}", path.display()))?
                == 0,
            "event {id} grew during materialization"
        );
    }
    Ok(())
}

/// Stream the first line of a blob (up to the first newline or EOF); reaching `limit` with no
/// newline fails as over the limit. The child is killed as soon as the line is in hand: not one
/// byte of the remainder enters memory.
fn git_blob_first_line(
    repo_root: &Path,
    git_ref: &str,
    path: &str,
    limit: usize,
) -> Result<Option<String>> {
    use std::io::Read as _;
    let spec = format!("{git_ref}:{path}");
    let mut child = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", "blob", &spec])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("cannot read {spec}"))?;
    let mut out = child.stdout.take().context("no stdout from git cat-file")?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let line = loop {
        let n = out
            .read(&mut chunk)
            .with_context(|| format!("cannot read {spec}"))?;
        if n == 0 {
            break if buf.is_empty() { None } else { Some(buf) };
        }
        // The budget covers the trailing LF (the same accounting the event limit uses), and both
        // branches check **before** appending — if the newline branch skipped the check, a first
        // line over budget would go on into JSON parsing.
        if let Some(pos) = chunk[..n].iter().position(|b| *b == b'\n') {
            if buf.len() + pos + 1 > limit {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("first line of {spec} exceeds {limit} bytes");
            }
            buf.extend_from_slice(&chunk[..pos]);
            break Some(buf);
        }
        if buf.len() + n + 1 > limit {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("first line of {spec} exceeds {limit} bytes");
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let _ = child.kill();
    let _ = child.wait();
    match line {
        None => Ok(None),
        Some(bytes) => Ok(Some(
            String::from_utf8(bytes).with_context(|| format!("{spec} is not UTF-8"))?,
        )),
    }
}

fn git_blob_at(repo_root: &Path, git_ref: &str, path: &str, limit: usize) -> Result<Vec<u8>> {
    let spec = format!("{git_ref}:{path}");
    let size = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", "-s", &spec])
        .output()
        .with_context(|| format!("cannot inspect {spec}"))?;
    if !size.status.success() {
        anyhow::bail!(
            "cannot inspect {spec}: {}",
            String::from_utf8_lossy(&size.stderr).trim()
        );
    }
    let size: usize = String::from_utf8(size.stdout)
        .context("git cat-file -s returned non-UTF-8 output")?
        .trim()
        .parse()
        .with_context(|| format!("git returned an invalid size for {spec}"))?;
    if size > limit {
        anyhow::bail!("{spec} is {size} bytes, above the {limit}-byte limit");
    }

    let output = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", "blob", &spec])
        .output()
        .with_context(|| format!("cannot read {spec}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "cannot read {spec}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.len() != size {
        anyhow::bail!(
            "git returned {} bytes for {spec}, expected {size}",
            output.stdout.len()
        );
    }
    Ok(output.stdout)
}

fn materialize_ids_at(repo_root: &Path, git_ref: &str, ids: &[String]) -> Result<String> {
    materialize_ids_with_limits(
        ids,
        MAX_EVENT_BYTES,
        MAX_MATERIALIZED_BYTES,
        |unique| inspect_git_event_sizes(repo_root, git_ref, unique),
        |unique, sizes, first_offsets, output| {
            read_git_events_into_output(repo_root, git_ref, unique, sizes, first_offsets, output)
        },
    )
}

const MAX_BATCH_HEADER_BYTES: usize = 1024;

fn with_event_batch<T>(
    repo_root: &Path,
    git_ref: &str,
    ids: &[&str],
    mode: &str,
    consume: impl FnOnce(&mut BufReader<std::process::ChildStdout>) -> Result<T>,
) -> Result<T> {
    let mut child = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", mode])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start git cat-file {mode}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);

    std::thread::scope(|scope| -> Result<T> {
        // Borrow the already-indexed ids instead of cloning up to a million request strings into a
        // second heap buffer. The writer must run concurrently because Git may fill stdout before
        // the request pipe accepts the complete sequence.
        let writer = scope.spawn(move || -> Result<()> {
            for id in ids {
                let spec = format!("{git_ref}:{}", meta::event_path(id)?);
                stdin
                    .write_all(spec.as_bytes())
                    .context("cannot send event request to git cat-file")?;
                stdin
                    .write_all(b"\n")
                    .context("cannot terminate event request to git cat-file")?;
            }
            stdin.flush().context("cannot flush git cat-file requests")
        });

        let parsed = consume(&mut reader);
        if parsed.is_err() {
            let _ = child.kill();
        }
        let writer_result = writer
            .join()
            .map_err(|_| anyhow::anyhow!("git cat-file input writer panicked"))?;
        let output = child
            .wait_with_output()
            .with_context(|| format!("cannot wait for git cat-file {mode}"))?;
        let value = parsed?;
        writer_result?;
        if !output.status.success() {
            anyhow::bail!(
                "git cat-file {mode} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(value)
    })
}

fn inspect_git_event_sizes(repo_root: &Path, git_ref: &str, ids: &[&str]) -> Result<Vec<usize>> {
    with_event_batch(repo_root, git_ref, ids, "--batch-check", |reader| {
        let mut sizes = Vec::new();
        sizes
            .try_reserve_exact(ids.len())
            .context("cannot allocate git event sizes")?;
        for id in ids {
            let header = read_batch_header(reader)?;
            sizes.push(event_size_from_header(git_ref, id, &header)?);
        }
        Ok(sizes)
    })
}

fn read_git_events_into_output(
    repo_root: &Path,
    git_ref: &str,
    ids: &[&str],
    sizes: &[usize],
    first_offsets: &[usize],
    output: &mut [u8],
) -> Result<()> {
    with_event_batch(repo_root, git_ref, ids, "--batch", |reader| {
        for (index, id) in ids.iter().enumerate() {
            let header = read_batch_header(reader)?;
            let actual = event_size_from_header(git_ref, id, &header)?;
            let expected = sizes[index];
            anyhow::ensure!(
                actual == expected,
                "event {id} changed size between git batch passes: expected {expected}, found {actual}"
            );
            let start = first_offsets[index];
            let end = start
                .checked_add(expected)
                .context("event output range overflow")?;
            let destination = output
                .get_mut(start..end)
                .with_context(|| format!("event {id} output range is out of bounds"))?;
            reader
                .read_exact(destination)
                .with_context(|| format!("cannot read event {id} from git cat-file"))?;
            let mut separator = [0u8; 1];
            reader.read_exact(&mut separator)?;
            anyhow::ensure!(
                separator == *b"\n",
                "git cat-file --batch omitted the separator after event {id}"
            );
        }
        Ok(())
    })
}

fn event_size_from_header(git_ref: &str, id: &str, header: &str) -> Result<usize> {
    let spec = format!("{git_ref}:{}", meta::event_path(id)?);
    if header.ends_with(" missing") {
        anyhow::bail!("event {id} is missing at {spec}");
    }
    let mut fields = header.split_whitespace();
    let oid = fields.next().context("batch header omitted object id")?;
    let object_type = fields.next().context("batch header omitted object type")?;
    let size: usize = fields
        .next()
        .context("batch header omitted object size")?
        .parse()
        .context("batch header contained an invalid object size")?;
    anyhow::ensure!(
        fields.next().is_none()
            && matches!(oid.len(), 40 | 64)
            && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "unexpected git cat-file batch header `{header}`"
    );
    anyhow::ensure!(
        object_type == "blob",
        "event {id} at {spec} is a {object_type}, not a blob"
    );
    Ok(size)
}

fn read_batch_header<R: BufRead>(reader: &mut R) -> Result<String> {
    let mut header = Vec::with_capacity(96);
    loop {
        let available = reader.fill_buf()?;
        anyhow::ensure!(
            !available.is_empty(),
            "git cat-file ended before an object header"
        );
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        anyhow::ensure!(
            header.len().saturating_add(take) <= MAX_BATCH_HEADER_BYTES,
            "git cat-file object header exceeds {MAX_BATCH_HEADER_BYTES} bytes"
        );
        header.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            header.pop();
            return String::from_utf8(header).context("git cat-file returned a non-UTF-8 header");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID_A: &str = "agit-0123456789abcdef0123456789abcdef01234567";
    const SID_B: &str = "agit-fedcba9876543210fedcba9876543210fedcba98";

    fn envelope(session_id: &str, content: serde_json::Value) -> Envelope {
        Envelope {
            source: "codex".into(),
            session_id: session_id.into(),
            object_hash: transcript::object_hash(&content),
            content,
        }
    }

    fn line(session_id: &str, n: i64) -> String {
        envelope_line(&envelope(session_id, json!({"n": n})))
    }

    /// The first-line budget covers the trailing LF and both branches check before appending:
    /// this pins that a line whose newline falls just outside the budget is rejected while a line
    /// inside the boundary is allowed.
    #[test]
    fn blob_first_line_budget_counts_the_lf() {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(d.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(d.path().join("f"), "0123456\nrest\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "x"]);
        assert_eq!(
            git_blob_first_line(d.path(), "HEAD", "f", 8)
                .unwrap()
                .as_deref(),
            Some("0123456"),
            "a 7-byte body plus LF exactly fills the 8-byte budget"
        );
        let err = git_blob_first_line(d.path(), "HEAD", "f", 7)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn event_id_covers_the_full_canonical_envelope_and_final_lf() {
        let a = line(SID_A, 1);
        let b = line(SID_B, 1);
        assert_eq!(
            event_id(&a).unwrap(),
            hex::encode(Sha256::digest(a.as_bytes()))[..40]
        );
        assert_eq!(
            parse_envelope_line(&a).unwrap().object_hash,
            parse_envelope_line(&b).unwrap().object_hash,
            "content hash intentionally stays content-only"
        );
        assert_ne!(event_id(&a).unwrap(), event_id(&b).unwrap());
        assert!(event_id(a.trim_end_matches('\n')).is_err());
    }

    #[test]
    fn envelope_parser_rejects_noncanonical_or_tampered_bytes() {
        let valid = line(SID_A, 1);
        assert!(parse_envelope_line(&valid).is_ok());
        assert!(parse_envelope_line(&format!(" {valid}")).is_err());
        assert!(parse_envelope_line(&valid.replace("\n", "\r\n")).is_err());

        let mut value: serde_json::Value = serde_json::from_str(valid.trim_end()).unwrap();
        value["_object_hash"] = json!("0".repeat(40));
        let tampered = format!("{}\n", serde_json::to_string(&value).unwrap());
        assert!(parse_envelope_line(&tampered).is_err());
        assert!(parse_envelopes("\n").is_err());
    }

    #[test]
    fn envelope_parser_rejects_unknown_fields_in_current_and_legacy_bytes() {
        let canonical = line(SID_A, 1);
        let mut value: serde_json::Value = serde_json::from_str(canonical.trim_end()).unwrap();
        value["unexpected"] = json!(true);
        let with_extra = format!("{}\n", serde_json::to_string(&value).unwrap());

        assert!(parse_envelope_line(&with_extra).is_err());
        assert!(canonical_v0(&with_extra).is_err());
    }

    #[test]
    fn v0_reader_canonicalizes_the_legacy_value_field_order() {
        let content = json!({"type":"system","subtype":"agit:__merge_start__"});
        let old = format!(
            "{}\n",
            json!({
                "_source": "codex",
                "_session_id": SID_A,
                "_object_hash": transcript::object_hash(&content),
                "content": content,
            })
        );
        assert!(old.starts_with("{\"_object_hash\""), "{old}");
        assert!(parse_envelope_line(&old).is_err());

        let canonical = canonical_v0(&old).unwrap();
        assert!(canonical.starts_with("{\"_source\""), "{canonical}");
        assert!(parse_envelope_line(&canonical).is_ok());
    }

    #[test]
    fn sequence_parser_is_strict_and_preserves_duplicates() {
        let id = "a".repeat(40);
        let text = format!("{id}\n{id}\n");
        assert_eq!(parse_sequence(&text).unwrap(), vec![id.clone(), id]);
        assert!(parse_sequence(&"A".repeat(40)).is_err());
        assert!(parse_sequence(&format!("{}\n\n", "a".repeat(40))).is_err());
        assert!(parse_sequence(&format!("{}\r\n", "a".repeat(40))).is_err());
    }

    #[test]
    fn materializer_reads_each_unique_body_once_into_the_final_output() {
        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let third = line(SID_A, 3);
        let first_id = event_id(&first).unwrap();
        let second_id = event_id(&second).unwrap();
        let third_id = event_id(&third).unwrap();
        let ids = vec![
            first_id.clone(),
            second_id.clone(),
            first_id.clone(),
            third_id.clone(),
        ];
        let bodies = HashMap::from([
            (first_id.clone(), first.clone()),
            (second_id.clone(), second.clone()),
            (third_id.clone(), third.clone()),
        ]);
        let expanded = first.len() * 2 + second.len() + third.len();
        let inspections = std::cell::RefCell::new(HashMap::<String, usize>::new());
        let reads = std::cell::RefCell::new(HashMap::<String, usize>::new());

        let materialized = materialize_ids_with_limits(
            &ids,
            bodies.values().map(String::len).max().unwrap(),
            expanded,
            |unique| {
                Ok(unique
                    .iter()
                    .map(|id| {
                        *inspections
                            .borrow_mut()
                            .entry((*id).to_owned())
                            .or_default() += 1;
                        bodies[*id].len()
                    })
                    .collect())
            },
            |unique, sizes, first_offsets, output| {
                assert_eq!(
                    output.len(),
                    expanded,
                    "only the final body buffer is exposed"
                );
                for (index, id) in unique.iter().enumerate() {
                    *reads.borrow_mut().entry((*id).to_owned()).or_default() += 1;
                    let body = bodies[*id].as_bytes();
                    assert_eq!(body.len(), sizes[index]);
                    let start = first_offsets[index];
                    output[start..start + body.len()].copy_from_slice(body);
                }
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(materialized, format!("{first}{second}{first}{third}"));
        assert!(inspections.into_inner().values().all(|count| *count == 1));
        assert!(reads.into_inner().values().all(|count| *count == 1));
    }

    #[test]
    fn materializer_rejects_expansion_before_reading_any_body() {
        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let first_id = event_id(&first).unwrap();
        let second_id = event_id(&second).unwrap();
        let ids = vec![first_id.clone(), second_id.clone(), first_id.clone()];
        let sizes = HashMap::from([(first_id, first.len()), (second_id, second.len())]);
        let expanded = first.len() * 2 + second.len();
        let body_pass_started = std::cell::Cell::new(false);

        let error = materialize_ids_with_limits(
            &ids,
            first.len().max(second.len()),
            expanded - 1,
            |unique| Ok(unique.iter().map(|id| sizes[*id]).collect()),
            |_, _, _, _| {
                body_pass_started.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("materialized transcript"),
            "{error:#}"
        );
        assert!(
            !body_pass_started.get(),
            "expanded size must be rejected before any event body read"
        );
    }

    #[test]
    fn pair_materializer_reads_the_union_once_and_preserves_mixed_order() {
        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let third = line(SID_A, 3);
        let first_id = event_id(&first).unwrap();
        let second_id = event_id(&second).unwrap();
        let third_id = event_id(&third).unwrap();
        let log_ids = vec![
            first_id.clone(),
            second_id.clone(),
            first_id.clone(),
            third_id.clone(),
        ];
        let view_ids = vec![second_id.clone(), first_id.clone(), second_id.clone()];
        let bodies = HashMap::from([
            (first_id.clone(), first.clone()),
            (second_id.clone(), second.clone()),
            (third_id.clone(), third.clone()),
        ]);
        let expected_log = format!("{first}{second}{first}{third}");
        let expected_view = format!("{second}{first}{second}");
        let sequence_limit = expected_log.len().max(expected_view.len());
        let unique_limit = first.len() + second.len() + third.len();
        assert!(expected_log.len() + expected_view.len() > sequence_limit);
        let inspections = std::cell::RefCell::new(HashMap::<String, usize>::new());
        let reads = std::cell::RefCell::new(HashMap::<String, usize>::new());

        let (log, view) = materialize_pair_ids_with_limits(
            &log_ids,
            &view_ids,
            bodies.values().map(String::len).max().unwrap(),
            sequence_limit,
            unique_limit,
            |unique| {
                Ok(unique
                    .iter()
                    .map(|id| {
                        *inspections
                            .borrow_mut()
                            .entry((*id).to_owned())
                            .or_default() += 1;
                        bodies[*id].len()
                    })
                    .collect())
            },
            |unique, sizes, first_offsets, output| {
                for (index, id) in unique.iter().enumerate() {
                    *reads.borrow_mut().entry((*id).to_owned()).or_default() += 1;
                    let body = bodies[*id].as_bytes();
                    assert_eq!(body.len(), sizes[index]);
                    let start = first_offsets[index];
                    output[start..start + body.len()].copy_from_slice(body);
                }
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(log, expected_log);
        assert_eq!(view, expected_view);
        assert_eq!(inspections.borrow().len(), 3);
        assert!(inspections.into_inner().values().all(|count| *count == 1));
        assert_eq!(reads.borrow().len(), 3);
        assert!(reads.into_inner().values().all(|count| *count == 1));
    }

    #[test]
    fn pair_materializer_checks_sequence_union_and_reachability_before_body_reads() {
        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let first_id = event_id(&first).unwrap();
        let second_id = event_id(&second).unwrap();
        let bodies = HashMap::from([
            (first_id.clone(), first.clone()),
            (second_id.clone(), second),
        ]);
        let body_passes = std::cell::Cell::new(0usize);

        let oversized_log = vec![first_id.clone(), first_id.clone()];
        let sequence_error = materialize_pair_ids_with_limits(
            &oversized_log,
            std::slice::from_ref(&first_id),
            first.len(),
            first.len(),
            first.len(),
            |unique| Ok(unique.iter().map(|id| bodies[*id].len()).collect()),
            |_, _, _, _| {
                body_passes.set(body_passes.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            sequence_error
                .to_string()
                .contains("materialized transcript"),
            "{sequence_error:#}"
        );
        assert_eq!(body_passes.get(), 0);

        let union_ids = vec![first_id.clone(), second_id.clone()];
        let unique_bytes = bodies[&first_id].len() + bodies[&second_id].len();
        let union_error = materialize_pair_ids_with_limits(
            &union_ids,
            std::slice::from_ref(&first_id),
            first.len().max(bodies[&second_id].len()),
            unique_bytes,
            unique_bytes - 1,
            |unique| Ok(unique.iter().map(|id| bodies[*id].len()).collect()),
            |_, _, _, _| {
                body_passes.set(body_passes.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            union_error.to_string().contains("unique event bytes"),
            "{union_error:#}"
        );
        assert_eq!(body_passes.get(), 0);

        let reachability_error = materialize_pair_ids_with_limits(
            std::slice::from_ref(&first_id),
            std::slice::from_ref(&second_id),
            first.len(),
            first.len() * 2,
            first.len() * 2,
            |_| {
                panic!("unreachable VIEW must fail before object inspection");
            },
            |_, _, _, _| {
                panic!("unreachable VIEW must fail before body reads");
            },
        )
        .unwrap_err();
        assert!(
            reachability_error.to_string().contains("not reachable"),
            "{reachability_error:#}"
        );
    }

    #[test]
    fn raw_event_line_limit_is_checked_before_json_parsing() {
        validate_envelope_input_bounds_with_limits("LOG", "abc\n", 4, 32, 2).unwrap();
        let oversized =
            validate_envelope_input_bounds_with_limits("LOG", "abcd\n", 4, 32, 2).unwrap_err();
        assert!(oversized.to_string().contains("4-byte"), "{oversized:#}");
        assert!(
            validate_envelope_input_bounds_with_limits("LOG", "a\nb\n", 8, 32, 1)
                .unwrap_err()
                .to_string()
                .contains("1 events")
        );
        assert!(
            validate_envelope_input_bounds_with_limits("LOG", "unterminated", 32, 4, 2)
                .unwrap_err()
                .to_string()
                .contains("snapshot limit")
        );
    }

    #[test]
    fn attributes_replace_the_managed_block_and_preserve_user_rules() {
        let first = attributes_text(Some("*.bin binary\n"));
        let defaults = first.find(DEFAULTS_BEGIN).unwrap();
        let user = first.find("*.bin binary").unwrap();
        let objects = first.find(OBJECTS_BEGIN).unwrap();
        assert!(defaults < user && user < objects, "{first}");
        assert_eq!(first.matches(DEFAULTS_BEGIN).count(), 1);
        assert_eq!(first.matches(OBJECTS_BEGIN).count(), 1);
        assert_eq!(attributes_text(Some(&first)), first, "must be idempotent");

        let changed = first.replace("LOG        -text -merge", "LOG merge=union");
        let repaired = attributes_text(Some(&changed));
        assert!(repaired.contains("*.bin binary"));
        assert!(repaired.contains("LOG        -text -merge"));
        assert!(!repaired.contains("LOG merge=union"));
    }

    #[test]
    fn malformed_attributes_marker_never_discards_the_user_tail() {
        let existing = format!("*.bin binary\n{OBJECTS_BEGIN}\nkeep-this-tail -text\n");
        let preview = attributes_text(Some(&existing));
        assert!(preview.contains("keep-this-tail -text"), "{preview}");
        let error = attributes_text_strict(Some(&existing)).unwrap_err();
        assert!(error.to_string().contains("no end marker"), "{error:#}");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(meta::ATTRS_FILE), &existing).unwrap();
        let event = line(SID_A, 1);
        let error = write_snapshot(dir.path(), &event, &event).unwrap_err();
        assert!(error.to_string().contains("no end marker"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(meta::ATTRS_FILE)).unwrap(),
            existing
        );
        assert!(!dir.path().join(meta::LOG_FILE).exists());
        assert!(!dir.path().join(meta::EVENTS_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn attributes_symlink_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("attributes");
        std::fs::write(&target, "outside bytes\n").unwrap();
        symlink(&target, dir.path().join(meta::ATTRS_FILE)).unwrap();

        let error = ensure_attributes(dir.path()).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error:#}");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "outside bytes\n");
    }

    #[test]
    fn attributes_preserve_user_binary_rules_but_force_storage_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(meta::ATTRS_FILE),
            attributes_text(Some("*.bin binary\nevents/** text merge=union diff\n")).as_bytes(),
        )
        .unwrap();
        let event = "events/a/b/c/d/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let attrs = repo
            .git(&[
                "check-attr",
                "text",
                "binary",
                "merge",
                "diff",
                "--",
                "asset.bin",
                event,
            ])
            .unwrap();
        assert!(attrs.contains("asset.bin: binary: set"), "{attrs}");
        assert!(attrs.contains("asset.bin: text: unset"), "{attrs}");
        assert!(attrs.contains(&format!("{event}: text: unset")), "{attrs}");
        assert!(attrs.contains(&format!("{event}: merge: unset")), "{attrs}");
        assert!(attrs.contains(&format!("{event}: diff: unset")), "{attrs}");
    }

    #[test]
    fn autocrlf_clone_keeps_content_addressed_bytes_exact() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = crate::domain::repo::Repo::init(source_dir.path()).unwrap();
        source.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let event = line(SID_A, 1);
        write_snapshot(source.root(), &event, &event).unwrap();
        let snapshot = meta::Meta::new(SID_A.into(), "codex".into(), "/repo".into());
        meta::write(source.root(), &snapshot).unwrap();
        source.add_all().unwrap();
        source.commit("v1").unwrap();

        let checkout_parent = tempfile::tempdir().unwrap();
        let checkout = checkout_parent.path().join("clone");
        let output = std::process::Command::new("git")
            .args(["-c", "core.autocrlf=true", "clone", "--no-local"])
            .arg(source.root())
            .arg(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let id = event_id(&event).unwrap();
        assert_eq!(
            std::fs::read(checkout.join(meta::event_path(&id).unwrap())).unwrap(),
            event.as_bytes()
        );
        assert_eq!(
            materialize_worktree(&checkout, meta::LOG_FILE).unwrap(),
            event
        );
    }

    #[test]
    fn manual_merge_conflicts_without_writing_markers_into_log() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let base = line(SID_A, 1);
        write_snapshot(repo.root(), &base, &base).unwrap();
        meta::write(
            repo.root(),
            &meta::Meta::new(SID_A.into(), "codex".into(), "/repo".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("base").unwrap();

        repo.git(&["checkout", "-q", "-b", "side"]).unwrap();
        let side = format!("{base}{}", line(SID_A, 2));
        write_snapshot(repo.root(), &side, &side).unwrap();
        repo.add_all().unwrap();
        repo.commit("side").unwrap();

        repo.git(&["checkout", "-q", "main"]).unwrap();
        let ours = format!("{base}{}", line(SID_A, 3));
        write_snapshot(repo.root(), &ours, &ours).unwrap();
        repo.add_all().unwrap();
        repo.commit("ours").unwrap();
        let ours_log = std::fs::read(repo.root().join(meta::LOG_FILE)).unwrap();

        assert!(repo.git(&["merge", "--no-commit", "side"]).is_err());
        let conflicted = std::fs::read(repo.root().join(meta::LOG_FILE)).unwrap();
        assert_eq!(conflicted, ours_log);
        assert!(
            !conflicted
                .windows(b"<<<<<<<".len())
                .any(|w| w == b"<<<<<<<")
        );
        repo.git(&["merge", "--abort"]).unwrap();
    }

    #[test]
    fn snapshot_files_is_pure_and_contains_sequences_and_event_union() {
        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let files = snapshot_files(
            &format!("{first}{second}{first}"),
            &format!("{second}{first}"),
        )
        .unwrap();
        assert_eq!(files.len(), 4, "two sequences plus two unique events");
        assert!(!files.contains_key(meta::ATTRS_FILE));
        assert_eq!(
            std::str::from_utf8(&files[meta::LOG_FILE])
                .unwrap()
                .lines()
                .count(),
            3
        );
        assert_eq!(
            std::str::from_utf8(&files[meta::VIEW_FILE])
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn legacy_view_only_events_become_reachable_once_without_changing_view() {
        let first = line(SID_A, 1);
        let view_only = line(SID_A, 2);
        let view = format!("{first}{view_only}{view_only}");
        let upgraded = make_view_reachable(&first, &view).unwrap();

        assert_eq!(upgraded, format!("{first}{view_only}"));
        let files = snapshot_files(&upgraded, &view).unwrap();
        assert_eq!(
            parse_sequence(std::str::from_utf8(&files[meta::VIEW_FILE]).unwrap())
                .unwrap()
                .len(),
            3,
            "VIEW multiplicity is preserved"
        );
    }

    #[test]
    fn snapshot_roundtrips_and_keeps_user_attributes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(meta::ATTRS_FILE), "*.bin binary\n").unwrap();
        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let log = format!("{first}{second}{first}");
        let view = format!("{second}{first}");

        write_snapshot(dir.path(), &log, &view).unwrap();
        write_snapshot(dir.path(), &log, &view).unwrap();
        let mut snapshot_meta = meta::Meta::new(SID_A.into(), "codex".into(), "/r".into());
        snapshot_meta.layout = LayoutVersion::V1;
        meta::write(dir.path(), &snapshot_meta).unwrap();

        assert_eq!(
            materialize_worktree(dir.path(), meta::LOG_FILE).unwrap(),
            log
        );
        assert_eq!(
            materialize_worktree(dir.path(), meta::VIEW_FILE).unwrap(),
            view
        );
        let attrs = std::fs::read_to_string(dir.path().join(meta::ATTRS_FILE)).unwrap();
        assert!(attrs.contains("*.bin binary\n"));
        assert_eq!(attrs.matches(DEFAULTS_BEGIN).count(), 1);
        assert_eq!(attrs.matches(OBJECTS_BEGIN).count(), 1);

        let first_id = event_id(&first).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(meta::event_path(&first_id).unwrap())).unwrap(),
            first
        );
    }

    #[test]
    fn existing_event_size_is_rejected_without_unbounded_read_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let event = line(SID_A, 1);
        let id = event_id(&event).unwrap();
        let path = dir.path().join(meta::event_path(&id).unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_EVENT_BYTES + 1) as u64).unwrap();

        let error = write_snapshot(dir.path(), &event, &event).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error:#}");
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            (MAX_EVENT_BYTES + 1) as u64
        );
        assert!(!dir.path().join(meta::LOG_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_event_root_without_writing_outside_repo() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join(meta::EVENTS_DIR)).unwrap();
        let event = line(SID_A, 1);

        let error = write_snapshot(dir.path(), &event, &event).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(!dir.path().join(meta::LOG_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_legacy_parent_before_any_write_or_delete() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_log = outside.path().join("log.jsonl");
        std::fs::write(&outside_log, "outside legacy bytes\n").unwrap();
        symlink(outside.path(), dir.path().join("session")).unwrap();
        let event = line(SID_A, 1);

        let error = write_snapshot(dir.path(), &event, &event).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(outside_log).unwrap(),
            "outside legacy bytes\n"
        );
        assert!(!dir.path().join(meta::LOG_FILE).exists());
        assert!(!dir.path().join(meta::EVENTS_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_shard_or_final_event() {
        use std::os::unix::fs::symlink;

        let event = line(SID_A, 1);
        let id = event_id(&event).unwrap();
        let relative = meta::event_path(&id).unwrap();

        let shard_case = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(shard_case.path().join(meta::EVENTS_DIR)).unwrap();
        symlink(
            outside_dir.path(),
            shard_case.path().join(meta::EVENTS_DIR).join(&id[..1]),
        )
        .unwrap();
        assert!(write_snapshot(shard_case.path(), &event, &event).is_err());
        assert_eq!(std::fs::read_dir(outside_dir.path()).unwrap().count(), 0);

        let final_case = tempfile::tempdir().unwrap();
        let final_path = final_case.path().join(relative);
        std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        let outside_file = final_case.path().join("outside-event-target");
        std::fs::write(&outside_file, b"outside bytes\n").unwrap();
        symlink(&outside_file, &final_path).unwrap();
        let error = write_snapshot(final_case.path(), &event, &event).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error:#}");
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"outside bytes\n");
        assert!(!final_case.path().join(meta::LOG_FILE).exists());
    }

    #[test]
    fn snapshot_rejects_view_events_outside_log() {
        let dir = tempfile::tempdir().unwrap();
        let error = write_snapshot(dir.path(), &line(SID_A, 1), &line(SID_A, 2)).unwrap_err();
        assert!(error.to_string().contains("not reachable"));
        assert!(!dir.path().join(meta::LOG_FILE).exists());
    }

    #[test]
    fn v1_readers_reject_view_objects_that_are_not_reachable_from_log() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let log_line = line(SID_A, 1);
        let foreign = line(SID_A, 2);
        write_snapshot(dir.path(), &log_line, &log_line).unwrap();
        let foreign_id = event_id(&foreign).unwrap();
        let foreign_path = dir.path().join(meta::event_path(&foreign_id).unwrap());
        std::fs::create_dir_all(foreign_path.parent().unwrap()).unwrap();
        std::fs::write(&foreign_path, foreign).unwrap();
        std::fs::write(
            dir.path().join(meta::VIEW_FILE),
            sequence_text(std::slice::from_ref(&foreign_id)).unwrap(),
        )
        .unwrap();
        meta::write(
            dir.path(),
            &meta::Meta::new(SID_A.into(), "codex".into(), "/r".into()),
        )
        .unwrap();

        assert!(
            materialize_worktree(dir.path(), meta::VIEW_FILE)
                .unwrap_err()
                .to_string()
                .contains("not reachable")
        );
        repo.add_all().unwrap();
        repo.commit("unreachable view").unwrap();
        assert!(
            materialize_at(dir.path(), "HEAD", meta::VIEW_FILE)
                .unwrap_err()
                .to_string()
                .contains("not reachable")
        );
    }

    #[test]
    fn worktree_materializer_dual_reads_v0() {
        let dir = tempfile::tempdir().unwrap();
        let mut snapshot_meta = meta::Meta::new(SID_A.into(), "codex".into(), "/r".into());
        snapshot_meta.layout = LayoutVersion::V0;
        meta::write(dir.path(), &snapshot_meta).unwrap();
        let log = line(SID_A, 1);
        std::fs::write(dir.path().join(meta::LEGACY_LOG_FILE), &log).unwrap();
        assert_eq!(
            materialize_worktree(dir.path(), meta::LOG_FILE).unwrap(),
            log
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_materializer_refuses_symlinked_event_objects() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let event = line(SID_A, 1);
        write_snapshot(dir.path(), &event, &event).unwrap();
        meta::write(
            dir.path(),
            &meta::Meta::new(SID_A.into(), "codex".into(), "/r".into()),
        )
        .unwrap();
        let id = event_id(&event).unwrap();
        let path = dir.path().join(meta::event_path(&id).unwrap());
        let outside = dir.path().join("outside-event");
        std::fs::write(&outside, &event).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();

        let error = materialize_worktree(dir.path(), meta::LOG_FILE).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error:#}");
    }

    #[test]
    fn materialize_at_batch_reads_v1_and_dual_reads_v0_history() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

        let old_line = line(SID_A, 0);
        let mut old_meta = meta::Meta::new(SID_A.into(), "codex".into(), "/r".into());
        old_meta.layout = LayoutVersion::V0;
        meta::write(dir.path(), &old_meta).unwrap();
        std::fs::write(dir.path().join(meta::LEGACY_LOG_FILE), &old_line).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let v0 = repo.git(&["rev-parse", "HEAD"]).unwrap();

        assert_eq!(
            materialize_at(dir.path(), &v0, meta::LOG_FILE).unwrap(),
            old_line
        );

        let new_line = line(SID_A, 1);
        write_snapshot(dir.path(), &new_line, &new_line).unwrap();
        let new_meta = meta::Meta::new(SID_A.into(), "codex".into(), "/r".into());
        meta::write(dir.path(), &new_meta).unwrap();
        repo.add_all().unwrap();
        repo.commit("v1").unwrap();

        assert_eq!(
            materialize_at(dir.path(), "HEAD", meta::LOG_FILE).unwrap(),
            new_line
        );
    }

    #[test]
    fn materialize_pair_at_preserves_v1_order_and_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let third = line(SID_A, 3);
        let log = format!("{first}{second}{first}{third}");
        let view = format!("{second}{first}{second}");
        write_snapshot(dir.path(), &log, &view).unwrap();
        meta::write(
            dir.path(),
            &meta::Meta::new(SID_A.into(), "codex".into(), "/r".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("v1 pair").unwrap();

        let sequence_limit = log.len().max(view.len());
        let unique_limit = first.len() + second.len() + third.len();
        assert!(log.len() + view.len() > sequence_limit);
        assert_eq!(
            materialize_pair_at_with_limits(dir.path(), "HEAD", sequence_limit, unique_limit,)
                .unwrap(),
            (log, view)
        );
    }

    #[test]
    fn materialize_v0_pair_canonicalizes_streams_under_independent_limits() {
        fn legacy_line(canonical: &str) -> String {
            let envelope = parse_envelope_line(canonical).unwrap();
            format!(
                "{{\"content\":{},\"_object_hash\":{},\"_session_id\":{},\"_source\":{}}}\n",
                serde_json::to_string(&envelope.content).unwrap(),
                serde_json::to_string(&envelope.object_hash).unwrap(),
                serde_json::to_string(&envelope.session_id).unwrap(),
                serde_json::to_string(&envelope.source).unwrap(),
            )
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

        let first = line(SID_A, 1);
        let second = line(SID_A, 2);
        let log = format!("{first}{second}{first}");
        let view = format!("{second}{first}{second}");
        let raw_log = format!(
            "{}{}{}",
            legacy_line(&first),
            legacy_line(&second),
            legacy_line(&first)
        );
        let raw_view = format!(
            "{}{}{}",
            legacy_line(&second),
            legacy_line(&first),
            legacy_line(&second)
        );
        let mut snapshot_meta = meta::Meta::new(SID_A.into(), "codex".into(), "/r".into());
        snapshot_meta.layout = LayoutVersion::V0;
        meta::write(dir.path(), &snapshot_meta).unwrap();
        std::fs::write(dir.path().join(meta::LEGACY_LOG_FILE), raw_log).unwrap();
        std::fs::write(dir.path().join(meta::LEGACY_VIEW_FILE), raw_view).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 pair").unwrap();

        let sequence_limit = log.len().max(view.len());
        assert!(log.len() + view.len() > sequence_limit);
        assert_eq!(
            materialize_pair_at_with_limits(dir.path(), "HEAD", sequence_limit, sequence_limit,)
                .unwrap(),
            (log.clone(), view.clone())
        );
        let error =
            materialize_pair_at_with_limits(dir.path(), "HEAD", sequence_limit - 1, sequence_limit)
                .unwrap_err();
        assert!(
            format!("{error:#}").contains("materialized transcript"),
            "{error:#}"
        );
    }

    #[test]
    fn materialize_at_ignores_local_replace_objects() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let original = line(SID_A, 1);
        write_snapshot(dir.path(), &original, &original).unwrap();
        meta::write(
            dir.path(),
            &meta::Meta::new(SID_A.into(), "codex".into(), "/r".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("v1").unwrap();

        let id = event_id(&original).unwrap();
        let real = repo
            .git(&[
                "rev-parse",
                &format!("HEAD:{}", meta::event_path(&id).unwrap()),
            ])
            .unwrap();
        std::fs::write(dir.path().join("replacement-event"), line(SID_B, 9)).unwrap();
        let replacement = repo
            .git(&["hash-object", "-w", "replacement-event"])
            .unwrap();
        repo.git(&["replace", real.trim(), replacement.trim()])
            .unwrap();

        assert_eq!(
            materialize_at(dir.path(), "HEAD", meta::LOG_FILE).unwrap(),
            original,
            "local replace refs are not part of the graph that push publishes"
        );
    }
}
