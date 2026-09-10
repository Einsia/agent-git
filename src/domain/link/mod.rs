//! Links: what stands for a session in the store.
//!
//! # The store keeps no copy
//!
//! The session file already lives in the runtime's own directory, and it does not disappear (a
//! spot check of `rollout_path` across the 18779 rows of the `threads` table — the newest 400 and
//! the oldest 200 — found 0 missing). Keeping a copy costs something real: 18858 sessions on this
//! machine come to 11.2 GB, and a copying implementation reads and writes the whole file every
//! time, 92 ms for a 36 MB session — paid once per turn of conversation.
//!
//! So a session in the store is one small JSON:
//!
//! ```text
//! ~/.agit/store/claude-code/db57fdab-....json
//! ```
//!
//! ```json
//! ```
//!
//! # Runtime and id are in the path
//!
//! Recording one thing in two places becomes inconsistent one day, and the path is the copy you
//! hold first when looking a session up — so the body does not repeat `runtime` and
//! `session_id`.
//!
//! `cwd` is the **source of truth** for "which project this belongs to": `agit log --here`
//! filters by repo, and the snapshot's `code` field and the naming suggestion both rely on it.
//! Partition slugs collide; cwd does not.
//!
//! `agent` is "which agent this session belongs to". `agit import -n <agent>` writes it in as it
//! records the session's initial version. The recorded namespace and branch complete the claim;
//! ordinary commands still require an explicit target or AGIT_SESSION.
//!
//! The only steps that write a link are the ones that bind content to a runtime:
//!
//! * `agit import` writes down the existence of an existing session as it adopts it (`agit hooks
//!   ingest` likewise, only without guessing the ownership).
//! * `agit resume` / `run` / `fork` write the ownership and the baseline at the moment they
//!   materialize into the runtime — lineage not recorded at install time is lost forever.
//! * `agit commit` fills in cwd, ownership and branch as it settles, so committing the same
//!   session again and again needs no session id.
//!
//! A branch can retain historical runtime instances, but superseded and merge archive links
//! are excluded from ordinary active claims. Materialization records the exact branch tip in `materialized_from`; a repeated
//! prepare can therefore reuse the same instance, while a newer tip can replace an instance only
//! after its byte baseline proves that no runtime content was appended.
//!
//! `agit clone` does **not** write one: it only fetches, it does not run (see
//! [`crate::commands::clone`]); materializing is `agit run`'s job.
//!
//! # One file per session
//!
//! Not a single `links.json`: writes can fire concurrently (two sessions opened at once), one
//! file per session drops the read-modify-write race, and file names cannot collide.

use crate::Result;
use crate::adapter;
use crate::domain::merge_archive::{MergeArchiveRole, RuntimeLinkKey};
use crate::domain::store::Store;
use crate::domain::turn;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One session link in the store.
///
/// `source` / `session_id` come from the file path and are never persisted (see the module
/// documentation).
#[derive(Debug, Clone)]
pub struct Link {
    /// The runtime: `codex` / `claude-code`.
    pub source: String,
    pub session_id: String,
    /// The working directory the session runs in.
    pub cwd: Option<String>,
    /// The agent name it belongs to. Absent before the first commit.
    pub agent: Option<String>,
    /// The namespace the agent sits in (your own name, or an organization). An absent namespace
    /// is an incomplete legacy claim and cannot authorize hook settlement or environment propagation.
    pub owner: Option<String>,
    /// The branch this session holds. Absent before the first commit.
    ///
    /// Local evidence for the "one branch, one session" invariant: settle uses it to find the
    /// branch to advance.
    pub branch: Option<String>,
    /// The materialization baseline: the byte count of the live transcript generated at the
    /// moment resume/run/fork installs the VIEW into the runtime. Settlement reads only the bytes
    /// appended **after** the baseline; the baseline content is a materialized copy of history
    /// already in the repo (its ids have been reminted), so comparing it byte for byte against
    /// committed content is neither right nor possible.
    pub baseline_bytes: Option<u64>,
    /// SHA-256 of the baseline region: doctor verifies that "the live transcript has had no
    /// non-append write inside the baseline".
    pub baseline_hash: Option<String>,
    /// The exact session-branch tip represented by this runtime instance's recorded baseline.
    ///
    /// A runtime-local id is not durable lineage. This commit id lets resume distinguish an
    /// idempotent repeat from a branch that advanced and needs a fresh materialization. A
    /// successful settlement advances this tip together with the byte baseline.
    pub materialized_from: Option<String>,
    /// The runtime instance that replaced this claim, in `<runtime>/<session-id>` form.
    ///
    /// The transcript remains in the runtime for recovery, but a superseded link is no longer a
    /// candidate for implicit context or branch settlement.
    pub superseded_by: Option<String>,
    /// A static local role binds exploration capture to its exact merge archive journal.
    /// It cannot authorize ordinary settlement, context selection, or branch reclamation.
    pub merge_archive: Option<MergeArchiveRole>,
    /// The user dismissed this unclaimed session from the naming inbox.
    pub naming_ignored: bool,
}

/// The on-disk form. Empty optional fields are omitted rather than written as `null`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Body {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    materialized_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merge_archive: Option<MergeArchiveRole>,
    #[serde(default, skip_serializing_if = "is_false")]
    naming_ignored: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Link {
    pub fn new(source: &str, session_id: &str, cwd: Option<&Path>) -> Link {
        Link {
            source: source.to_string(),
            session_id: session_id.to_string(),
            cwd: cwd.map(|p| p.to_string_lossy().to_string()),
            agent: None,
            owner: None,
            branch: None,
            baseline_bytes: None,
            baseline_hash: None,
            materialized_from: None,
            superseded_by: None,
            merge_archive: None,
            naming_ignored: false,
        }
    }

    /// Explicit inspection retains known claim fields without treating malformed data as absence.
    #[cfg(feature = "cli")]
    pub(crate) fn from_json(source: &str, session_id: &str, bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{'),
            "the session link must be a JSON object"
        );
        let body: Body = serde_json::from_slice(bytes).context("invalid session link fields")?;
        Ok(from_body(source.to_owned(), session_id.to_owned(), body))
    }

    fn body(&self) -> Body {
        Body {
            cwd: self.cwd.clone(),
            agent: self.agent.clone(),
            owner: self.owner.clone(),
            branch: self.branch.clone(),
            baseline_bytes: self.baseline_bytes,
            baseline_hash: self.baseline_hash.clone(),
            materialized_from: self.materialized_from.clone(),
            superseded_by: self.superseded_by.clone(),
            merge_archive: self.merge_archive.clone(),
            naming_ignored: self.naming_ignored,
        }
    }

    /// `<runtime>/<session-id>`, the machine-local identity used in supersession records.
    pub fn instance(&self) -> String {
        format!("{}/{}", self.source, self.session_id)
    }

    /// Only active links may resolve implicit context or advance their claimed branch.
    pub fn is_active(&self) -> bool {
        self.superseded_by.is_none() && self.merge_archive.is_none()
    }

    /// Archive authority requires the exact local runtime, journal role, and complete route.
    /// A role alone cannot authorize a missing namespace or a superseded runtime instance.
    pub fn is_archive_for(&self, role: &MergeArchiveRole, source: &str, session_id: &str) -> bool {
        let Some((owner, agent)) = role.slug.split_once('/') else {
            return false;
        };
        role.validate(role.origin_head.len()).is_ok()
            && RuntimeLinkKey {
                runtime: source.to_owned(),
                session_id: session_id.to_owned(),
            }
            .validate()
            .is_ok()
            && self.source == source
            && self.session_id == session_id
            && self.merge_archive.as_ref() == Some(role)
            && self.superseded_by.is_none()
            && self.owner.as_deref() == Some(owner)
            && self.agent.as_deref() == Some(agent)
            && self.branch.as_deref() == Some(role.branch.as_str())
    }

    /// The on-disk JSON. Shared by the tests and `write`, so what you see is what is written.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.body())?)
    }

    /// Look up the real transcript file.
    pub fn resolve(&self) -> Option<PathBuf> {
        let ad = adapter::get(&self.source).ok()?;
        ad.resolve(&self.session_id, self.cwd.as_ref().map(Path::new))
            // Claude Desktop deliberately has no listing/lookup surface of its own: the Code
            // tab writes Claude Code jsonl and the Claude Code adapter owns the read side. Keep
            // that de-duplication for session discovery, but let an already-claimed link read
            // the file it actually points at. Without this fallback, an ExportOnly materialized
            // link can never prove that its baseline is untouched and every repeated prepare
            // becomes unverifiable.
            .or_else(|| {
                (self.source == "claude-desktop")
                    .then(|| {
                        adapter::get("claude-code")
                            .ok()?
                            .resolve(&self.session_id, self.cwd.as_ref().map(Path::new))
                    })
                    .flatten()
            })
    }

    /// Read the transcript (raw bytes).
    ///
    /// Bytes rather than `String`: the transcript's raw bytes go into the git blob unchanged (the
    /// root snapshot's session identity is computed from them too), and any encoding round trip
    /// can alter them.
    pub fn read_bytes(&self) -> Result<Vec<u8>> {
        let p = self.resolve().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot find the transcript of session {} ({}).\n  \
                 The runtime may have deleted it, or this session comes from another machine.",
                short(&self.session_id),
                self.source
            )
        })?;
        std::fs::read(&p).with_context(|| format!("cannot read {}", p.display()))
    }

    /// Read the transcript.
    pub fn read(&self) -> Result<String> {
        let bytes = self.read_bytes()?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Parse into the intermediate representation (IR).
    pub fn parse(&self) -> Result<adapter::Session> {
        let text = self.read()?;
        // Identify the source runtime from the content rather than trusting the path — the file
        // may have been moved or renamed.
        let rt = adapter::infer_runtime(&text).unwrap_or(self.source.as_str());
        adapter::get(rt)?.parse(&text)
    }

    /// Compute the turn chain.
    pub fn chain(&self) -> Result<turn::Chain> {
        Ok(turn::chain_of(&self.parse()?))
    }
}

/// `<runtime>/<id>.json`
pub fn link_path(store: &Store, source: &str, session_id: &str) -> PathBuf {
    store.root().join(source).join(format!("{session_id}.json"))
}

/// Write one link.
/// Cross-process mutual exclusion for one link.
///
/// A writer's read-modify-write critical sections (import's claim snapshot and its restore on
/// failure, settlement advancing the watermark) all run under this lock, which is released along
/// with the returned handle. The lock file is the link's name plus a `.lock` suffix and its
/// contents mean nothing; [`list`] accepts only `.json`, so it never takes it for a link.
pub fn lock(store: &Store, source: &str, session_id: &str) -> Result<std::fs::File> {
    use fs2::FileExt as _;
    let dir = store.root().join(source);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let lp = dir.join(format!("{session_id}.json.lock"));
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lp)
        .with_context(|| format!("cannot open {}", lp.display()))?;
    f.lock_exclusive()
        .with_context(|| format!("cannot lock {}", lp.display()))?;
    Ok(f)
}

pub fn write(store: &Store, link: &Link) -> Result<PathBuf> {
    let dir = store.root().join(&link.source);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let lp = link_path(store, &link.source, &link.session_id);
    std::fs::write(&lp, format!("{}\n", link.to_json()?))
        .with_context(|| format!("cannot write {}", lp.display()))?;
    Ok(lp)
}

/// An exact local Link image retained by a merge archive transition journal.
/// Unknown JSON fields remain in `json` so a transition can preserve unrelated metadata.
#[derive(Debug, Clone)]
pub struct ArchiveLinkSnapshot {
    pub link: Link,
    pub json: String,
}

const MAX_ARCHIVE_LINK_BYTES: u64 = 64 * 1024;

fn archive_key(source: &str, session_id: &str) -> Result<()> {
    RuntimeLinkKey {
        runtime: source.to_owned(),
        session_id: session_id.to_owned(),
    }
    .validate()
}

fn is_redirect(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn archive_link_directory_exists(path: &Path) -> Result<bool> {
    crate::domain::merge_archive::authority_directory_exists(path)
}

/// Read a regular, bounded Link image without treating unreadable metadata as absence.
/// The caller holds the participating Link lock through transition publication.
pub fn read_archive_link_snapshot(
    store: &Store,
    source: &str,
    session_id: &str,
) -> Result<Option<ArchiveLinkSnapshot>> {
    use std::io::Read as _;

    archive_key(source, session_id)?;
    if !archive_link_directory_exists(store.root())?
        || !archive_link_directory_exists(&store.root().join(source))?
    {
        return Ok(None);
    }
    let path = link_path(store, source, session_id);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !is_redirect(&metadata),
            "archive Link {} is a redirect or non-regular file",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("cannot open archive Link {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && !is_redirect(&metadata),
        "archive Link {} is a redirect or non-regular file",
        path.display()
    );
    let mut json = String::new();
    file.take(MAX_ARCHIVE_LINK_BYTES + 1)
        .read_to_string(&mut json)
        .with_context(|| format!("cannot read archive Link {}", path.display()))?;
    anyhow::ensure!(
        json.len() as u64 <= MAX_ARCHIVE_LINK_BYTES,
        "archive Link image is too large"
    );
    Ok(Some(parse_archive_link_image(source, session_id, json)?))
}

pub(crate) fn parse_archive_link_image(
    source: &str,
    session_id: &str,
    json: String,
) -> Result<ArchiveLinkSnapshot> {
    archive_key(source, session_id)?;
    crate::domain::merge_archive::checked_link_image(&json)?;
    crate::domain::metadata_facts::JsonFacts::parse(&json)?;
    let body: Body = serde_json::from_str(&json).context("archive Link image is unreadable")?;
    Ok(ArchiveLinkSnapshot {
        link: from_body(source.to_owned(), session_id.to_owned(), body),
        json,
    })
}

/// Publish an exact Link image after the command journals its expected and planned states.
/// The caller holds the Link lock; this function does not acquire or release that lock.
/// A publication error can follow a successful rename, so recovery rereads the journal and Link
/// instead of assuming the old image remains installed or attempting an unconditional rollback.
pub fn publish_archive_transition_locked(
    store: &Store,
    source: &str,
    session_id: &str,
    expected_json: Option<&str>,
    planned_json: &str,
) -> Result<PathBuf> {
    archive_key(source, session_id)?;
    anyhow::ensure!(
        planned_json.len() as u64 <= MAX_ARCHIVE_LINK_BYTES,
        "planned archive Link image is too large"
    );
    crate::domain::merge_archive::checked_link_image(planned_json)?;
    let planned: Body =
        serde_json::from_str(planned_json).context("planned archive Link image is unreadable")?;
    if let Some(role) = &planned.merge_archive {
        role.validate(role.origin_head.len())?;
        let (owner, agent) = role
            .slug
            .split_once('/')
            .context("invalid archive repository")?;
        anyhow::ensure!(
            planned.owner.as_deref() == Some(owner)
                && planned.agent.as_deref() == Some(agent)
                && planned.branch.as_deref() == Some(role.branch.as_str()),
            "planned archive Link route does not match its role"
        );
    }
    let current = read_archive_link_snapshot(store, source, session_id)?;
    anyhow::ensure!(
        current.as_ref().map(|snapshot| snapshot.json.as_str()) == expected_json,
        "archive Link changed before transition publication"
    );
    anyhow::ensure!(
        archive_link_directory_exists(store.root())?
            && archive_link_directory_exists(&store.root().join(source))?,
        "archive Link parent is missing; acquire its Link lock before publication"
    );
    let path = link_path(store, source, session_id);
    crate::domain::merge_archive::durable_publish_transition_bytes(
        &path,
        planned_json.as_bytes(),
        current.is_some(),
    )?;
    Ok(path)
}

fn from_body(source: String, session_id: String, body: Body) -> Link {
    Link {
        source,
        session_id,
        cwd: body.cwd,
        agent: body.agent,
        owner: body.owner,
        branch: body.branch,
        baseline_bytes: body.baseline_bytes,
        baseline_hash: body.baseline_hash,
        materialized_from: body.materialized_from,
        superseded_by: body.superseded_by,
        merge_archive: body.merge_archive,
        naming_ignored: body.naming_ignored,
    }
}

/// Read one link.
///
/// `runtime` comes from the parent directory name and `session_id` from the file name, so the
/// path is where those two fields come from.
pub fn read(path: &Path) -> Option<Link> {
    let body: Body = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    link_from_body(path, body)
}

fn link_from_body(path: &Path, body: Body) -> Option<Link> {
    let source = path.parent()?.file_name()?.to_str()?.to_string();
    let session_id = path.file_stem()?.to_str()?.to_string();
    Some(from_body(source, session_id, body))
}

/// List every link in the store.
///
/// Accepts only `<registered runtime>/<id>.json`. Anything lying directly in the store root (the
/// allowlist file, an editor's temporary file) is not taken for a link.
pub fn list(store: &Store) -> Vec<Link> {
    let root = store.root();
    if !root.exists() {
        return vec![];
    }
    let mut out = vec![];
    for e in walkdir::WalkDir::new(root)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let in_runtime_dir = p
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|x| x.to_str())
            .is_some_and(|rt| adapter::RUNTIMES.contains(&rt));
        if !in_runtime_dir {
            continue;
        }
        if let Some(l) = read(p) {
            out.push(l);
        }
    }
    out.sort_by(|a, b| (&a.source, &a.session_id).cmp(&(&b.source, &b.session_id)));
    out
}

/// Archive preparation cannot treat unreadable Link metadata as an absent branch claim.
/// Every registered runtime directory is inspected before selecting the requested route.
pub(crate) fn archive_claims_for_branch(
    store: &Store,
    owner: &str,
    agent: &str,
    branch: &str,
) -> Result<Vec<ArchiveLinkSnapshot>> {
    let mut selected = Vec::new();
    let mut entries = 0usize;
    let mut bytes = 0usize;
    if !archive_link_directory_exists(store.root())? {
        return Ok(selected);
    }
    for runtime in adapter::RUNTIMES {
        let directory = store.root().join(runtime);
        if !archive_link_directory_exists(&directory)? {
            continue;
        }
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            entries += 1;
            anyhow::ensure!(
                entries <= 8192,
                "archive claim inventory exceeds its entry limit"
            );
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("archive claim filename is not a valid native ID")
                })?;
            let snapshot = read_archive_link_snapshot(store, runtime, id)?
                .ok_or_else(|| anyhow::anyhow!("archive claim disappeared during inventory"))?;
            bytes = bytes
                .checked_add(snapshot.json.len())
                .ok_or_else(|| anyhow::anyhow!("archive claim inventory byte count overflowed"))?;
            anyhow::ensure!(
                bytes <= 8 * 1024 * 1024,
                "archive claim inventory exceeds its byte limit"
            );
            if claims_branch(&snapshot.link, owner, agent, branch) {
                anyhow::ensure!(
                    snapshot.link.owner.as_deref() == Some(owner),
                    "archive preparation requires an explicit owner on every prior claim"
                );
                selected.push(snapshot);
                anyhow::ensure!(selected.len() <= 128, "too many archive previous claims");
            }
        }
    }
    selected.sort_by(|a, b| {
        (&a.link.source, &a.link.session_id).cmp(&(&b.link.source, &b.link.session_id))
    });
    Ok(selected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkIssueKind {
    UnreadableDirectory,
    UnreadableFile,
    InspectionLimit,
    InvalidData,
    InvalidPath,
}

impl LinkIssueKind {
    pub fn description(self) -> &'static str {
        match self {
            Self::UnreadableDirectory => "unreadable link directory",
            Self::UnreadableFile => "unreadable link file",
            Self::InspectionLimit => "link file exceeds the inspection budget",
            Self::InvalidData => "invalid link data",
            Self::InvalidPath => "invalid link path",
        }
    }
}

#[derive(Debug)]
pub struct LinkIssue {
    pub path: PathBuf,
    pub kind: LinkIssueKind,
    pub repository: Option<(String, String)>,
}

/// Diagnostic enumeration retains unreadable adoption evidence without changing lenient readers.
pub fn list_checked(store: &Store) -> (Vec<Link>, Vec<LinkIssue>) {
    let root = store.root();
    let root_issue = match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => None,
        Ok(_) => Some(LinkIssueKind::InvalidPath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), Vec::new());
        }
        Err(_) => Some(LinkIssueKind::UnreadableDirectory),
    };
    if let Some(kind) = root_issue {
        return (
            Vec::new(),
            vec![LinkIssue {
                path: root.to_owned(),
                kind,
                repository: None,
            }],
        );
    }
    let mut links = Vec::new();
    let mut issues = Vec::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).max_depth(2) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = error.path().unwrap_or(root);
                let runtime_directory = path.parent() == Some(root) && registered_runtime(path);
                if path == root || runtime_directory || is_link_path(root, path) {
                    issues.push(LinkIssue {
                        path: path.to_owned(),
                        kind: if path == root || runtime_directory {
                            LinkIssueKind::UnreadableDirectory
                        } else {
                            LinkIssueKind::UnreadableFile
                        },
                        repository: None,
                    });
                }
                continue;
            }
        };
        if entry.depth() == 1 {
            if registered_runtime(entry.path()) && !entry.file_type().is_dir() {
                issues.push(LinkIssue {
                    path: entry.path().to_owned(),
                    kind: LinkIssueKind::InvalidPath,
                    repository: None,
                });
            }
            continue;
        }
        if !is_link_path(root, entry.path()) {
            continue;
        }
        let path = entry.path();
        if !entry.file_type().is_file() {
            issues.push(LinkIssue {
                path: path.to_owned(),
                kind: LinkIssueKind::InvalidPath,
                repository: None,
            });
            continue;
        }
        let bytes = match read_checked_record(path) {
            Ok(bytes) => bytes,
            Err(kind) => {
                issues.push(LinkIssue {
                    path: path.to_owned(),
                    kind,
                    repository: None,
                });
                continue;
            }
        };
        match serde_json::from_slice::<Body>(&bytes) {
            Ok(body) => match link_from_body(path, body) {
                Some(link) => links.push(link),
                None => issues.push(LinkIssue {
                    path: path.to_owned(),
                    kind: LinkIssueKind::InvalidPath,
                    repository: issue_repository(&bytes),
                }),
            },
            Err(_) => issues.push(LinkIssue {
                path: path.to_owned(),
                kind: LinkIssueKind::InvalidData,
                repository: issue_repository(&bytes),
            }),
        }
    }
    links.sort_by(|a, b| (&a.source, &a.session_id).cmp(&(&b.source, &b.session_id)));
    issues.sort_by(|a, b| a.path.cmp(&b.path));
    (links, issues)
}

const MAX_CHECKED_LINK_BYTES: u64 = 1024 * 1024;

/// Enumeration metadata cannot authorize a read after a record is replaced.
fn read_checked_record(path: &Path) -> std::result::Result<Vec<u8>, LinkIssueKind> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return LinkIssueKind::InvalidPath;
        }
        let _ = error;
        LinkIssueKind::UnreadableFile
    })?;
    let metadata = file.metadata().map_err(|_| LinkIssueKind::UnreadableFile)?;
    if !metadata.is_file() || metadata.is_symlink() {
        return Err(LinkIssueKind::InvalidPath);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(LinkIssueKind::InvalidPath);
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_CHECKED_LINK_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LinkIssueKind::UnreadableFile)?;
    if bytes.len() as u64 > MAX_CHECKED_LINK_BYTES {
        return Err(LinkIssueKind::InspectionLimit);
    }
    Ok(bytes)
}

fn registered_runtime(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|runtime| adapter::RUNTIMES.contains(&runtime))
}

fn is_link_path(root: &Path, path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("json")
        && path
            .parent()
            .is_some_and(|parent| parent.parent() == Some(root) && registered_runtime(parent))
}

fn issue_repository(bytes: &[u8]) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct Identity {
        owner: String,
        agent: String,
    }

    // Scope needs a complete, unambiguous identity even when another field has an invalid type.
    let identity: Identity = serde_json::from_slice(bytes).ok()?;
    for component in [&identity.owner, &identity.agent] {
        if component.trim() != component || crate::domain::repo::valid_name(component).is_err() {
            return None;
        }
    }
    Some((identity.owner, identity.agent))
}

/// When a link was last updated.
///
/// Takes the link file's time and not the transcript's: the transcript's needs the file looked up
/// first (one glob each). And the link's mtime answers exactly the question the user is asking —
/// "which one did I adopt or commit most recently".
pub fn touched_at(store: &Store, link: &Link) -> std::time::SystemTime {
    std::fs::metadata(link_path(store, &link.source, &link.session_id))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// Cross-process exclusion for changing the active runtime claim of one session branch.
///
/// Per-session locks cannot protect a one-branch invariant: two materializations mint different
/// ids and therefore take different locks. The stable digest gives every process claiming the
/// same branch one shared lock without putting owner, repo, or branch names into a filesystem
/// path.
pub struct BranchLock {
    _branch: std::fs::File,
    _repository: std::fs::File,
    route: (PathBuf, String, String),
}

impl BranchLock {
    pub(crate) fn require_route(&self, store: &Store, slug: &str, branch: &str) -> Result<()> {
        anyhow::ensure!(
            self.route.0 == store.root() && self.route.1 == slug && self.route.2 == branch,
            "branch guard belongs to a different preparation route"
        );
        Ok(())
    }
}

pub fn lock_branch(store: &Store, slug: &str, branch: &str) -> Result<BranchLock> {
    use fs2::FileExt as _;
    use sha2::Digest as _;

    let repository = lock_repository(store, slug, false)?;
    let mut digest = sha2::Sha256::new();
    digest.update(slug.as_bytes());
    digest.update([0]);
    digest.update(branch.as_bytes());
    let dir = store.root().join(".locks").join("branches");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{}.lock", hex::encode(digest.finalize())));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("cannot lock {}", path.display()))?;
    Ok(BranchLock {
        _branch: file,
        _repository: repository,
        route: (store.root().to_owned(), slug.to_owned(), branch.to_owned()),
    })
}

/// Repository moves exclude every branch writer, including branches created during promotion.
/// Shared repository guards let ordinary writes to independent branches proceed concurrently.
fn lock_repository(store: &Store, slug: &str, exclusive: bool) -> Result<std::fs::File> {
    use fs2::FileExt as _;
    use sha2::Digest as _;

    let dir = store.root().join(".locks").join("repositories");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{}.lock", hex::encode(sha2::Sha256::digest(slug))));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    if exclusive {
        file.lock_exclusive()
    } else {
        fs2::FileExt::lock_shared(&file)
    }
    .with_context(|| format!("cannot lock {}", path.display()))?;
    Ok(file)
}

/// Namespace guards precede link locks and use a stable order across overlapping promotions.
pub fn lock_repositories_exclusive(store: &Store, slugs: &[&str]) -> Result<Vec<std::fs::File>> {
    let mut slugs = slugs.to_vec();
    slugs.sort_unstable();
    slugs.dedup();
    slugs
        .into_iter()
        .map(|slug| lock_repository(store, slug, true))
        .collect()
}

/// Whether the runtime transcript still consists exactly of its recorded materialization.
///
/// Replacement is allowed only for `Untouched`. Missing hashes from legacy links and unreadable
/// transcripts fail closed: inability to prove that no work exists must never discard a writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationActivity {
    Untouched,
    Appended,
    Rewritten,
    Unverifiable,
}

pub fn materialization_activity(link: &Link) -> MaterializationActivity {
    let Ok(bytes) = link.read_bytes() else {
        return MaterializationActivity::Unverifiable;
    };
    materialization_activity_with_bytes(link, &bytes)
}

pub(crate) fn materialization_activity_with_bytes(
    link: &Link,
    bytes: &[u8],
) -> MaterializationActivity {
    use sha2::Digest as _;

    let (Some(baseline), Some(expected)) = (link.baseline_bytes, &link.baseline_hash) else {
        return MaterializationActivity::Unverifiable;
    };
    let Ok(baseline) = usize::try_from(baseline) else {
        return MaterializationActivity::Unverifiable;
    };
    if bytes.len() < baseline {
        return MaterializationActivity::Rewritten;
    }
    let actual = hex::encode(sha2::Sha256::digest(&bytes[..baseline]));
    if actual != *expected {
        return MaterializationActivity::Rewritten;
    }
    if bytes.len() == baseline {
        MaterializationActivity::Untouched
    } else {
        MaterializationActivity::Appended
    }
}

/// Whether a link still claims the exact branch destination named by a caller.
///
/// A legacy link without an owner belongs to the signed-in namespace, so it remains compatible
/// with the historical representation. A recorded owner, however, is part of the destination and
/// must match too; checking only the branch name can settle a rerouted runtime into another repo.
pub fn claims_branch(link: &Link, owner: &str, agent: &str, branch: &str) -> bool {
    link.is_active()
        && link.agent.as_deref() == Some(agent)
        && link.branch.as_deref() == Some(branch)
        && link
            .owner
            .as_deref()
            .is_none_or(|candidate| candidate == owner)
}

/// Active runtime claims for one session branch. A missing owner is the legacy personal-repo
/// form and keeps the same compatibility rule as commit and resume.
pub fn active_for_branch(store: &Store, owner: &str, agent: &str, branch: &str) -> Vec<Link> {
    list(store)
        .into_iter()
        .filter(|link| {
            link.is_active()
                && link.agent.as_deref() == Some(agent)
                && link.branch.as_deref() == Some(branch)
                && link
                    .owner
                    .as_deref()
                    .is_none_or(|candidate| candidate == owner)
        })
        .collect()
}

/// The link touched most recently.
///
/// Used by the commands that still take the global-latest strategy. It must pick by time and not
/// take the first of the list: the list is sorted by (runtime, id), so taking the first picks by
/// lexicographic uuid order, which has nothing to do with "most recent".
pub fn latest(store: &Store) -> Option<Link> {
    list(store)
        .into_iter()
        .filter(Link::is_active)
        .max_by_key(|l| touched_at(store, l))
}

/// Read one specific link from the store (None when it does not exist).
///
/// Used when re-importing an already adopted session: fill in the **existing** one instead of
/// making a new one. This is the one that cannot be filled in after the fact.
pub fn get(store: &Store, source: &str, session_id: &str) -> Option<Link> {
    read(&link_path(store, source, session_id))
}

/// Keep an unclaimed session out of the naming inbox.
///
/// The read-modify-write is locked because a stale screen action can race with a runtime hook or
/// an import. A session that became managed while the screen was open wins that race and is left
/// unchanged; ownership must never acquire presentation state from an obsolete row.
pub fn dismiss_naming(
    store: &Store,
    source: &str,
    session_id: &str,
    cwd: Option<&Path>,
) -> Result<Link> {
    let _guard = lock(store, source, session_id)?;
    let mut link =
        get(store, source, session_id).unwrap_or_else(|| Link::new(source, session_id, cwd));
    if is_managed(&link) {
        return Ok(link);
    }
    if link.cwd.is_none() {
        link.cwd = cwd.map(|p| p.to_string_lossy().to_string());
    }
    link.naming_ignored = true;
    write(store, &link)?;
    Ok(link)
}

/// Whether there is enough ownership evidence to treat the session as managed by AgentGit.
///
/// A link that carries only a hook pre-registration or `--link-only` still has no agent, no
/// branch and no baseline; it has no recoverable version identity yet, so the current
/// conversation stays under `new`'s protection.
pub fn is_managed(link: &Link) -> bool {
    link.merge_archive.is_some()
        || link.agent.is_some()
        || link.branch.is_some()
        || link.baseline_bytes.is_some()
}

/// One session recorded under an agent name.
///
/// The same memory can be materialized into more than one runtime (`agit resume --as` carries on
/// in another harness), so "one agent to many sessions" is a normal state, not an anomaly.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub link: Link,
    /// The transcript file is newer than the link file — this session has had new content since
    /// the last import / commit.
    ///
    /// The test uses two `stat` calls and **never opens the transcript**. It holds because the
    /// write order on both sides is fixed: import and commit both write the link only after
    /// reading the transcript, so "the link is newer" means "the content as of that moment is
    /// already recorded"; whatever the transcript grows after that carries its mtime past the
    /// link's.
    pub touched: bool,
}

/// List the sessions recorded under an agent name.
///
/// Reads the link files and the transcripts' mtimes only; parses no content.
pub fn for_agent(store: &Store, agent: &str) -> Vec<Candidate> {
    list(store)
        .into_iter()
        .filter(|l| l.is_active() && l.agent.as_deref() == Some(agent))
        .map(|l| {
            let link_at = touched_at(store, &l);
            let touched = l
                .resolve()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
                .is_some_and(|t| t > link_at);
            Candidate { link: l, touched }
        })
        .collect()
}

/// The unique candidate, None when there is none (the caller shows the candidate list to the
/// user).
///
/// Two tests, each demanding a unique answer:
///
/// 1. There is only one candidate — nothing is left to choose.
/// 2. Only one of several candidates is "touched". Once the same memory is materialized into two
///    runtimes you work in one of them only, and the other's transcript stops at the moment of
///    install; "which one has new content" is then a fact, not a guess.
///
/// Neither holds and it returns None: **never** pick one among sessions that were all touched (or
/// none touched) — picking wrong records a stretch of work into another lineage, and is not
/// noticed right away.
pub fn only_one(cands: &[Candidate]) -> Option<&Candidate> {
    let mut active = cands.iter().filter(|candidate| candidate.link.is_active());
    match (active.next(), active.next()) {
        (Some(only), None) => return Some(only),
        (None, _) => return None,
        _ => {}
    }
    let mut touched = cands
        .iter()
        .filter(|candidate| candidate.link.is_active() && candidate.touched);
    match (touched.next(), touched.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Find a link by id or prefix.
///
/// An ambiguous prefix **must error** instead of taking the first — taking the wrong session
/// leaves the user carrying on in a completely unrelated context, and is not noticed right away.
pub fn find(store: &Store, selector: &str) -> Result<Link> {
    let sel = selector.trim();
    if sel.is_empty() {
        bail!("session selector must not be empty");
    }
    let matches: Vec<Link> = list(store)
        .into_iter()
        .filter(|l| l.session_id.starts_with(sel))
        .collect();

    match matches.len() {
        0 => bail!(
            "no session `{sel}` in the store.\n  \
             `agit import <full-session-id> --from <runtime> --into <owner/repo>@<branch>` lets you choose its lineage, or `agit log` lists what you have."
        ),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let ids: Vec<String> = matches
                .iter()
                .take(6)
                .map(|l| short(&l.session_id))
                .collect();
            bail!(
                "`{sel}` matches {n} sessions; give a longer prefix: {}",
                ids.join(", ")
            )
        }
    }
}

/// The short form of an id (for display).
pub fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let s = Store::at(&root);
        (d, s)
    }

    /// Diagnostics retain malformed records while ordinary discovery keeps its accepted links.
    #[test]
    fn checked_listing_preserves_links_and_reports_only_registered_records() {
        let (_directory, store) = store();
        write(&store, &Link::new("codex", "healthy", None)).unwrap();
        let mut superseded = Link::new("claude-code", "historical", None);
        superseded.superseded_by = Some("codex/healthy".into());
        write(&store, &superseded).unwrap();
        for path in [
            "codex/broken.json",
            "codex/typed.json",
            "unknown/ignored.json",
        ] {
            let path = store.root().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{malformed").unwrap();
        }
        std::fs::write(store.root().join("stray.json"), "{malformed").unwrap();
        std::fs::create_dir_all(store.root().join("codex/nested")).unwrap();
        std::fs::write(store.root().join("codex/nested/ignored.json"), "{malformed").unwrap();
        std::fs::create_dir_all(store.root().join("codex/directory.json")).unwrap();
        std::fs::write(
            store.root().join("codex/typed.json"),
            r#"{"owner":"alice","agent":"fixture","baseline_bytes":"PRIVATE-SENTINEL"}"#,
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("broken.json", store.root().join("codex/ignored-link.json"))
            .unwrap();

        let legacy = list(&store);
        let (checked, issues) = list_checked(&store);
        let identities = |links: &[Link]| {
            links
                .iter()
                .map(|link| {
                    (
                        link.source.clone(),
                        link.session_id.clone(),
                        link.to_json().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(identities(&checked), identities(&legacy));
        assert_eq!(checked.len(), 2);
        assert_eq!(issues.len(), 3 + usize::from(cfg!(unix)));
        assert_eq!(issues[0].path.file_name().unwrap(), "broken.json");
        assert_eq!(issues[0].kind, LinkIssueKind::InvalidData);
        assert_eq!(issues[0].repository, None);
        for issue in &issues[1..issues.len() - 1] {
            assert_eq!(issue.kind, LinkIssueKind::InvalidPath);
            assert_eq!(issue.repository, None);
        }
        let typed = issues.last().unwrap();
        assert_eq!(typed.path.file_name().unwrap(), "typed.json");
        assert_eq!(typed.kind, LinkIssueKind::InvalidData);
        assert_eq!(typed.repository, Some(("alice".into(), "fixture".into())));
        assert!(!format!("{issues:?}").contains("PRIVATE-SENTINEL"));
    }

    /// A partial or conflicting identity cannot attribute malformed evidence to a repository.
    #[test]
    fn issue_scope_requires_complete_valid_unambiguous_identity() {
        for text in [
            r#"{"owner":"alice","agent":"fixture""#,
            r#"{"owner":"alice"}"#,
            r#"{"owner":"alice","owner":"bob","agent":"fixture"}"#,
            r#"{"owner":"../alice","agent":"fixture"}"#,
            r#"{"owner":"alice ","agent":"fixture"}"#,
            r#"{"owner":null,"agent":"fixture"}"#,
        ] {
            assert_eq!(issue_repository(text.as_bytes()), None);
        }
        assert_eq!(
            issue_repository(br#"{"owner":"alice","agent":"fixture","baseline_bytes":false}"#),
            Some(("alice".into(), "fixture".into()))
        );
    }

    /// Oversized evidence stays unattributed instead of being parsed from a truncated prefix.
    #[test]
    fn checked_record_budget_preserves_lenient_readers() {
        let (_directory, store) = store();
        let runtime = store.root().join("codex");
        std::fs::create_dir_all(&runtime).unwrap();
        let mut bytes = br#"{"owner":"alice","agent":"fixture","cwd":"PRIVATE-SENTINEL"}"#.to_vec();
        bytes.resize(MAX_CHECKED_LINK_BYTES as usize, b' ');
        let accepted = runtime.join("accepted.json");
        std::fs::write(&accepted, &bytes).unwrap();
        assert_eq!(read_checked_record(&accepted).unwrap(), bytes);
        bytes.push(b' ');
        let oversized = runtime.join("oversized.json");
        std::fs::write(&oversized, &bytes).unwrap();
        assert!(read(&oversized).is_some());
        assert_eq!(list(&store).len(), 2);
        let (links, issues) = list_checked(&store);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].session_id, "accepted");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, oversized);
        assert_eq!(issues[0].kind, LinkIssueKind::InspectionLimit);
        assert_eq!(issues[0].repository, None);
        assert!(!format!("{issues:?}").contains("PRIVATE-SENTINEL"));
        assert_eq!(std::fs::read(oversized).unwrap(), bytes);
    }

    /// A cached regular-file entry must not let a replacement symlink supply claim identity.
    #[cfg(unix)]
    #[test]
    fn checked_record_rejects_symlink_replacement_after_enumeration() {
        let (directory, store) = store();
        let path = write(&store, &Link::new("codex", "candidate", None)).unwrap();
        let entry = walkdir::WalkDir::new(store.root())
            .min_depth(2)
            .max_depth(2)
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert!(entry.file_type().is_file());
        let foreign = directory.path().join("foreign.json");
        let private = br#"{"owner":"foreign","agent":"PRIVATE-SENTINEL"}"#;
        std::fs::write(&foreign, private).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&foreign, &path).unwrap();
        assert_eq!(
            read_checked_record(entry.path()),
            Err(LinkIssueKind::InvalidPath)
        );
        assert_eq!(std::fs::read(&foreign).unwrap(), private);
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
    }

    /// A replacement FIFO cannot make diagnostic reads wait for a writer.
    #[cfg(unix)]
    #[test]
    fn checked_record_rejects_fifo_replacement_without_blocking() {
        use std::os::unix::ffi::OsStrExt as _;

        let (_directory, store) = store();
        let path = write(&store, &Link::new("codex", "candidate", None)).unwrap();
        let entry = walkdir::WalkDir::new(store.root())
            .min_depth(2)
            .max_depth(2)
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert!(entry.file_type().is_file());
        std::fs::remove_file(&path).unwrap();
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (sent, received) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = sent.send(read_checked_record(entry.path()));
        });
        assert_eq!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("checked record inspection blocked on a FIFO"),
            Err(LinkIssueKind::InvalidPath)
        );
        reader.join().unwrap();
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_file());
    }

    /// Runtime and session identity belong to the path, not the JSON body.
    ///
    /// `runtime` / `session_id` are in the path; writing them down again adds one more place that
    /// can disagree.
    #[test]
    fn body_keeps_runtime_identity_in_the_path() {
        let (_d, s) = store();
        let mut l = Link::new("codex", "AB", Some(Path::new("/repo/one")));
        l.agent = Some("photo".into());
        let p = write(&s, &l).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();

        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        // `Value` is backed by a BTreeMap, so this compares the set, not the order.
        assert_eq!(keys, vec!["agent", "cwd"]);
        for absent in ["runtime", "source", "session_id"] {
            assert!(
                v.get(absent).is_none(),
                "{absent} belongs in the path, not in the body"
            );
        }
    }

    #[test]
    fn optional_fields_are_omitted_not_null() {
        // A newly adopted session has no ownership and no lineage yet. Serializing must not
        // emit `"agent": null`.
        let l = Link::new("claude-code", "CD", None);
        let j = l.to_json().unwrap();
        assert_eq!(j, "{}", "a link that knows nothing is an empty object: {j}");
        assert!(!j.contains("null"));
    }

    #[test]
    fn naming_dismissal_is_backward_compatible_and_persistent() {
        let (_d, s) = store();
        let path = link_path(&s, "codex", "AB");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();

        let old = read(&path).unwrap();
        assert!(!old.naming_ignored, "an absent field means not dismissed");

        let dismissed = dismiss_naming(&s, "codex", "AB", Some(Path::new("/repo"))).unwrap();
        assert!(dismissed.naming_ignored);
        assert_eq!(dismissed.cwd.as_deref(), Some("/repo"));
        let saved = read(&path).unwrap();
        assert!(saved.naming_ignored);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("\"naming_ignored\": true")
        );
    }

    #[test]
    fn only_a_claimed_link_is_managed() {
        let mut link = Link::new("codex", "s", None);
        assert!(!is_managed(&link));
        link.naming_ignored = true;
        assert!(!is_managed(&link), "a dismissal is not an adoption");

        let mut claimed = link;
        claimed.agent = Some("paper".into());
        assert!(is_managed(&claimed));
    }

    #[test]
    fn a_stale_dismissal_does_not_mark_a_managed_session() {
        let (_d, s) = store();
        let mut claimed = Link::new("codex", "AB", Some(Path::new("/repo")));
        claimed.agent = Some("photo".into());
        claimed.branch = Some("work".into());
        write(&s, &claimed).unwrap();

        let result = dismiss_naming(&s, "codex", "AB", Some(Path::new("/other"))).unwrap();
        assert!(!result.naming_ignored);
        assert_eq!(result.cwd.as_deref(), Some("/repo"));
        assert!(!get(&s, "codex", "AB").unwrap().naming_ignored);
    }

    #[test]
    fn runtime_and_id_come_back_from_the_path() {
        let (_d, s) = store();
        let l = Link::new("claude-code", "db57fdab-1234", Some(Path::new("/r")));
        write(&s, &l).unwrap();

        let back = read(&link_path(&s, "claude-code", "db57fdab-1234")).unwrap();
        assert_eq!(back.source, "claude-code");
        assert_eq!(back.session_id, "db57fdab-1234");
        assert_eq!(back.cwd.as_deref(), Some("/r"));
        assert!(back.agent.is_none());
    }

    #[test]
    fn links_from_both_runtimes_coexist() {
        let (_d, s) = store();
        write(&s, &Link::new("codex", "AB", None)).unwrap();
        write(&s, &Link::new("claude-code", "CD", None)).unwrap();
        assert_eq!(list(&s).len(), 2);
        // Links with the same id under different runtimes do not overwrite each other (the path
        // carries the runtime).
        write(&s, &Link::new("codex", "SAME", None)).unwrap();
        write(&s, &Link::new("claude-code", "SAME", None)).unwrap();
        assert_eq!(list(&s).len(), 4);
    }

    #[test]
    fn superseded_links_are_not_active_branch_claims() {
        let (_d, s) = store();
        let mut old = Link::new("codex", "OLD", None);
        old.owner = Some("alice".into());
        old.agent = Some("photo".into());
        old.branch = Some("work".into());
        old.materialized_from = Some("a".repeat(40));
        old.superseded_by = Some("codex/NEW".into());
        write(&s, &old).unwrap();

        let mut current = Link::new("codex", "NEW", None);
        current.owner = Some("alice".into());
        current.agent = Some("photo".into());
        current.branch = Some("work".into());
        current.materialized_from = Some("b".repeat(40));
        write(&s, &current).unwrap();

        let active = active_for_branch(&s, "alice", "photo", "work");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "NEW");
        let round_trip = get(&s, "codex", "OLD").unwrap();
        assert_eq!(round_trip.materialized_from, Some("a".repeat(40)));
        assert_eq!(round_trip.superseded_by.as_deref(), Some("codex/NEW"));
    }

    fn archive_role() -> MergeArchiveRole {
        MergeArchiveRole {
            generation: "019e8308-072a-739c-a75b-604d0848ba6f".to_owned(),
            slug: "alice/photo".to_owned(),
            branch: "work".to_owned(),
            origin_head: "a".repeat(40),
            logical_session: format!("agit-{}", "b".repeat(40)),
        }
    }

    fn archive_link(source: &str, session_id: &str) -> Link {
        let mut link = Link::new(source, session_id, None);
        link.owner = Some("alice".to_owned());
        link.agent = Some("photo".to_owned());
        link.branch = Some("work".to_owned());
        link.merge_archive = Some(archive_role());
        link
    }

    #[test]
    fn archive_roles_round_trip_without_repeating_native_identity() {
        let (_directory, store) = store();
        let legacy = Link::new("codex", "LEGACY", None);
        let legacy_path = write(&store, &legacy).unwrap();
        assert!(read(&legacy_path).unwrap().merge_archive.is_none());
        assert!(legacy.is_active());

        let archived = archive_link("codex", "ARCHIVE");
        let path = write(&store, &archived).unwrap();
        let saved = read(&path).unwrap();
        assert_eq!(saved.merge_archive, archived.merge_archive);
        assert!(saved.is_archive_for(&archive_role(), "codex", "ARCHIVE"));
        assert!(!saved.is_active());
        let body: serde_json::Value = serde_json::from_str(&saved.to_json().unwrap()).unwrap();
        for forbidden in ["source", "session_id", "runtime"] {
            assert!(body.get(forbidden).is_none());
            assert!(body["merge_archive"].get(forbidden).is_none());
        }
    }

    #[test]
    fn archive_authority_requires_exact_role_native_key_and_route() {
        let link = archive_link("codex", "ARCHIVE");
        let role = archive_role();
        assert!(link.is_archive_for(&role, "codex", "ARCHIVE"));
        assert!(!link.is_archive_for(&role, "claude-code", "ARCHIVE"));
        assert!(!link.is_archive_for(&role, "codex", "OTHER"));
        let mut other_role = role.clone();
        other_role.generation = "019e8308-072b-7748-a4d0-a7c3114a7f88".to_owned();
        assert!(!link.is_archive_for(&other_role, "codex", "ARCHIVE"));
        other_role = role.clone();
        other_role.origin_head = "c".repeat(40);
        assert!(!link.is_archive_for(&other_role, "codex", "ARCHIVE"));
        let mut superseded = link.clone();
        superseded.superseded_by = Some("codex/NEXT".to_owned());
        assert!(!superseded.is_archive_for(&role, "codex", "ARCHIVE"));
        assert!(!superseded.is_active());
        let mut ownerless = link.clone();
        ownerless.owner = None;
        assert!(!ownerless.is_archive_for(&role, "codex", "ARCHIVE"));
        assert!(!claims_branch(&ownerless, "alice", "photo", "work"));
        let mut rerouted = link.clone();
        rerouted.branch = Some("other".to_owned());
        assert!(!rerouted.is_archive_for(&role, "codex", "ARCHIVE"));
        let mut invalid = link;
        invalid.merge_archive.as_mut().unwrap().generation = "invalid".to_owned();
        assert!(!invalid.is_active());
        assert!(!invalid.is_archive_for(
            invalid.merge_archive.as_ref().unwrap(),
            "codex",
            "ARCHIVE"
        ));
    }

    #[test]
    fn archive_roles_never_become_ordinary_candidates() {
        let (_directory, store) = store();
        let archive = archive_link("codex", "ARCHIVE");
        write(&store, &archive).unwrap();
        assert!(active_for_branch(&store, "alice", "photo", "work").is_empty());
        assert!(latest(&store).is_none());
        assert!(for_agent(&store, "photo").is_empty());
        assert!(!claims_branch(&archive, "alice", "photo", "work"));
        assert!(is_managed(&archive));
        let archived_candidate = Candidate {
            link: archive,
            touched: true,
        };
        assert!(only_one(std::slice::from_ref(&archived_candidate)).is_none());

        let mut ordinary = Link::new("claude-code", "ORDINARY", None);
        ordinary.owner = Some("alice".to_owned());
        ordinary.agent = Some("photo".to_owned());
        ordinary.branch = Some("work".to_owned());
        write(&store, &ordinary).unwrap();
        assert_eq!(latest(&store).unwrap().session_id, "ORDINARY");
        assert_eq!(active_for_branch(&store, "alice", "photo", "work").len(), 1);
        assert_eq!(for_agent(&store, "photo").len(), 1);
        let candidates = [
            archived_candidate,
            Candidate {
                link: ordinary,
                touched: false,
            },
        ];
        assert_eq!(only_one(&candidates).unwrap().link.session_id, "ORDINARY");
        assert_eq!(
            get(&store, "codex", "ARCHIVE").unwrap().session_id,
            "ARCHIVE"
        );
    }

    #[test]
    fn archive_transition_compares_exact_bytes_and_preserves_unknown_fields() {
        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ARCHIVE").unwrap();
        let mut image: serde_json::Value =
            serde_json::from_str(&archive_link("codex", "ARCHIVE").to_json().unwrap()).unwrap();
        image["extension"] = serde_json::json!({"retained": true});
        let original = format!("{}\n", serde_json::to_string(&image).unwrap());
        let path =
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", None, &original).unwrap();
        let snapshot = read_archive_link_snapshot(&store, "codex", "ARCHIVE")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.json, original);
        assert!(
            snapshot
                .link
                .is_archive_for(&archive_role(), "codex", "ARCHIVE")
        );
        image["naming_ignored"] = serde_json::json!(true);
        let planned = serde_json::to_string(&image).unwrap();
        publish_archive_transition_locked(
            &store,
            "codex",
            "ARCHIVE",
            Some(&snapshot.json),
            &planned,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), planned);
        assert!(
            publish_archive_transition_locked(
                &store,
                "codex",
                "ARCHIVE",
                Some(&snapshot.json),
                &original
            )
            .is_err()
        );
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", None, &original).is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), planned);

        let reformatted = serde_json::to_string_pretty(&image).unwrap();
        std::fs::write(&path, &reformatted).unwrap();
        assert!(
            publish_archive_transition_locked(
                &store,
                "codex",
                "ARCHIVE",
                Some(&planned),
                &original
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), reformatted);
    }

    #[test]
    fn archive_transitions_allow_journaled_ordinary_retirement_and_restore() {
        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ORDINARY").unwrap();
        let original = "{\"owner\":\"alice\",\"agent\":\"photo\",\"branch\":\"work\"}\n";
        let retired = "{\"owner\":\"alice\",\"agent\":\"photo\",\"branch\":\"work\",\"superseded_by\":\"codex/ARCHIVE\"}\n";
        publish_archive_transition_locked(&store, "codex", "ORDINARY", None, original).unwrap();
        publish_archive_transition_locked(&store, "codex", "ORDINARY", Some(original), retired)
            .unwrap();
        assert!(!get(&store, "codex", "ORDINARY").unwrap().is_active());
        publish_archive_transition_locked(&store, "codex", "ORDINARY", Some(retired), original)
            .unwrap();
        assert!(get(&store, "codex", "ORDINARY").unwrap().is_active());
    }

    /// Ordinary Link permissions do not prevent a byte-checked transition to private authority.
    #[cfg(unix)]
    #[test]
    fn archive_transition_accepts_owner_controlled_ordinary_links() {
        use std::os::unix::fs::PermissionsExt;

        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ORDINARY").unwrap();
        let mut ordinary = archive_link("codex", "ORDINARY");
        ordinary.merge_archive = None;
        let path = write(&store, &ordinary).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let planned = archive_link("codex", "ORDINARY").to_json().unwrap();
        publish_archive_transition_locked(&store, "codex", "ORDINARY", Some(&original), &planned)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), planned);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        publish_archive_transition_locked(&store, "codex", "ORDINARY", Some(&planned), &original)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(get(&store, "codex", "ORDINARY").unwrap().is_active());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(
            publish_archive_transition_locked(
                &store,
                "codex",
                "ORDINARY",
                Some(&original),
                &planned
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o664
        );
    }

    /// File privacy cannot protect authority when another user can replace a containing directory.
    #[cfg(unix)]
    #[test]
    fn archive_transition_rejects_writable_authority_ancestry() {
        use std::os::unix::fs::PermissionsExt;

        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ORDINARY").unwrap();
        let mut ordinary = archive_link("codex", "ORDINARY");
        ordinary.merge_archive = None;
        let path = write(&store, &ordinary).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let planned = archive_link("codex", "ORDINARY").to_json().unwrap();
        for directory in [
            store.root().join("codex"),
            store.root().to_path_buf(),
            store.root().parent().unwrap().to_path_buf(),
        ] {
            let permissions = std::fs::metadata(&directory).unwrap().permissions();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(
                publish_archive_transition_locked(
                    &store,
                    "codex",
                    "ORDINARY",
                    Some(&original),
                    &planned
                )
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            std::fs::set_permissions(&directory, permissions).unwrap();
        }
        publish_archive_transition_locked(&store, "codex", "ORDINARY", Some(&original), &planned)
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), planned);
    }

    /// A readable ordinary Link becomes private even when its directory grants inherited reads.
    #[cfg(windows)]
    #[test]
    fn archive_transition_replaces_inherited_public_reads_with_private_acl() {
        use crate::infra::windows_security;

        let (_directory, store) = store();
        let runtime = store.root().join("codex");
        windows_security::private_directory(&runtime).unwrap();
        let acl = std::process::Command::new("icacls")
            .arg(&runtime)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .output()
            .unwrap();
        assert!(acl.status.success());
        let _guard = lock(&store, "codex", "ORDINARY").unwrap();
        let mut ordinary = archive_link("codex", "ORDINARY");
        ordinary.merge_archive = None;
        let path = write(&store, &ordinary).unwrap();
        windows_security::validate_path(&path, false, false).unwrap();
        assert!(windows_security::validate_path(&path, false, true).is_err());
        let original = std::fs::read_to_string(&path).unwrap();
        let planned = archive_link("codex", "ORDINARY").to_json().unwrap();
        publish_archive_transition_locked(&store, "codex", "ORDINARY", Some(&original), &planned)
            .unwrap();
        windows_security::validate_path(&path, false, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), planned);
    }

    #[test]
    fn archive_transition_refuses_missing_corrupt_and_rerouted_expected_links() {
        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ARCHIVE").unwrap();
        let planned = archive_link("codex", "ARCHIVE").to_json().unwrap();
        let path = link_path(&store, "codex", "ARCHIVE");
        assert!(
            read_archive_link_snapshot(&store, "codex", "ARCHIVE")
                .unwrap()
                .is_none()
        );
        assert!(get(&store, "codex", "ARCHIVE").is_none());
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", Some(&planned), &planned)
                .is_err()
        );
        assert!(!path.exists());
        for corrupt in ["{", "[]", "{\"owner\":false}", "{\"merge_archive\":{}}"] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(read_archive_link_snapshot(&store, "codex", "ARCHIVE").is_err());
            assert!(
                publish_archive_transition_locked(
                    &store,
                    "codex",
                    "ARCHIVE",
                    Some(corrupt),
                    &planned
                )
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        }
        let rerouted = "{\"owner\":\"mallory\",\"agent\":\"other\",\"branch\":\"elsewhere\"}";
        std::fs::write(&path, rerouted).unwrap();
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", Some(&planned), &planned)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rerouted);
        for invalid in ["{", "[]"] {
            assert!(
                publish_archive_transition_locked(
                    &store,
                    "codex",
                    "ARCHIVE",
                    Some(rerouted),
                    invalid
                )
                .is_err()
            );
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), rerouted);
    }

    #[test]
    fn archive_transition_refuses_invalid_roles_keys_and_nonregular_paths() {
        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ARCHIVE").unwrap();
        let mut link = archive_link("codex", "ARCHIVE");
        link.owner = None;
        assert!(
            publish_archive_transition_locked(
                &store,
                "codex",
                "ARCHIVE",
                None,
                &link.to_json().unwrap()
            )
            .is_err()
        );
        let planned = archive_link("codex", "ARCHIVE").to_json().unwrap();
        for (source, session_id) in [
            ("../codex", "ARCHIVE"),
            ("codex", "../outside"),
            ("unknown", "ARCHIVE"),
        ] {
            assert!(
                publish_archive_transition_locked(&store, source, session_id, None, &planned)
                    .is_err()
            );
        }
        let path = link_path(&store, "codex", "ARCHIVE");
        std::fs::create_dir(&path).unwrap();
        assert!(read_archive_link_snapshot(&store, "codex", "ARCHIVE").is_err());
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", None, &planned).is_err()
        );
        assert!(path.is_dir());
    }

    #[test]
    fn archive_transition_bounds_link_images_before_publication() {
        let (_directory, store) = store();
        let _guard = lock(&store, "codex", "ARCHIVE").unwrap();
        let oversized = format!(
            "{{\"extension\":\"{}\"}}",
            "x".repeat(MAX_ARCHIVE_LINK_BYTES as usize)
        );
        let path = link_path(&store, "codex", "ARCHIVE");
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", None, &oversized)
                .is_err()
        );
        assert!(!path.exists());
        std::fs::write(&path, &oversized).unwrap();
        assert!(read_archive_link_snapshot(&store, "codex", "ARCHIVE").is_err());
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", Some(&oversized), "{}")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), oversized);
    }

    #[test]
    fn archive_transition_uses_runtime_path_even_for_duplicate_native_ids() {
        let (_directory, store) = store();
        let _codex = lock(&store, "codex", "SAME").unwrap();
        let _claude = lock(&store, "claude-code", "SAME").unwrap();
        let planned = archive_link("codex", "SAME").to_json().unwrap();
        publish_archive_transition_locked(&store, "codex", "SAME", None, &planned).unwrap();
        publish_archive_transition_locked(&store, "claude-code", "SAME", None, "{}").unwrap();
        assert!(get(&store, "codex", "SAME").unwrap().is_archive_for(
            &archive_role(),
            "codex",
            "SAME"
        ));
        assert!(get(&store, "claude-code", "SAME").unwrap().is_active());
        assert!(find(&store, "SAME").is_err());
        assert_eq!(
            read_archive_link_snapshot(&store, "claude-code", "SAME")
                .unwrap()
                .unwrap()
                .json,
            "{}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_transition_refuses_redirected_file_and_runtime_directory() {
        use std::os::unix::fs::symlink;

        let (directory, store) = store();
        let _guard = lock(&store, "codex", "ARCHIVE").unwrap();
        let outside = directory.path().join("outside.json");
        std::fs::write(&outside, "{}").unwrap();
        let path = link_path(&store, "codex", "ARCHIVE");
        symlink(&outside, &path).unwrap();
        assert!(read_archive_link_snapshot(&store, "codex", "ARCHIVE").is_err());
        assert!(
            publish_archive_transition_locked(&store, "codex", "ARCHIVE", Some("{}"), "{}")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();
        let outside_directory = directory.path().join("outside");
        std::fs::create_dir(&outside_directory).unwrap();
        symlink(&outside_directory, store.root().join("claude-code")).unwrap();
        assert!(read_archive_link_snapshot(&store, "claude-code", "ARCHIVE").is_err());
        assert!(
            publish_archive_transition_locked(&store, "claude-code", "ARCHIVE", None, "{}")
                .is_err()
        );
        assert!(!outside_directory.join("ARCHIVE.json").exists());
    }

    #[test]
    fn stray_files_are_not_links() {
        // Other things can sit in the store root (the allowlist, an editor's temporary file) and
        // must not be taken for links.
        let (_d, s) = store();
        write(&s, &Link::new("codex", "AB", None)).unwrap();
        std::fs::write(s.root().join("credentials.json"), "{}").unwrap();
        std::fs::create_dir_all(s.root().join("not-a-runtime")).unwrap();
        std::fs::write(s.root().join("not-a-runtime").join("X.json"), "{}").unwrap();
        let all = list(&s);
        assert_eq!(all.len(), 1, "only <runtime>/<id>.json counts");
        assert_eq!(all[0].session_id, "AB");
    }

    /// "Use the most recent one when the session argument is omitted" must really pick by time.
    ///
    /// The list is sorted by (runtime, id), so taking the first picks by lexicographic uuid order
    /// — which has nothing to do with "most recent", and "most recent" is what `agit show` /
    /// `agit resume` say in their help.
    #[test]
    fn latest_is_by_time_not_by_id_order() {
        let (_d, s) = store();
        // `zzz` is written first and `aaa` second: lexicographic order is the reverse of time.
        write(&s, &Link::new("codex", "zzz-old", None)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&s, &Link::new("codex", "aaa-new", None)).unwrap();

        assert_eq!(
            list(&s)[0].session_id,
            "aaa-new",
            "the list is sorted by id"
        );
        assert_eq!(
            latest(&s).unwrap().session_id,
            "aaa-new",
            "the two agree here; the test below is the real one"
        );

        // Touch the old one again (the equivalent of committing to it).
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut old = read(&link_path(&s, "codex", "zzz-old")).unwrap();
        old.agent = Some("photo".into());
        write(&s, &old).unwrap();
        assert_eq!(
            list(&s)[0].session_id,
            "aaa-new",
            "the list order is unaffected"
        );
        assert_eq!(
            latest(&s).unwrap().session_id,
            "zzz-old",
            "the latest is the one just touched"
        );
    }

    #[test]
    fn ambiguous_prefix_errors_instead_of_guessing() {
        let (_d, s) = store();
        write(&s, &Link::new("codex", "abc111", None)).unwrap();
        write(&s, &Link::new("codex", "abc222", None)).unwrap();
        let e = find(&s, "abc").unwrap_err().to_string();
        assert!(
            e.contains("matches 2 sessions"),
            "an ambiguous prefix must be reported: {e}"
        );
        assert_eq!(find(&s, "abc1").unwrap().session_id, "abc111");
    }

    #[test]
    fn missing_session_suggests_import() {
        let (_d, s) = store();
        let e = find(&s, "nope").unwrap_err().to_string();
        assert!(
            e.contains("agit import"),
            "the error must give the next step: {e}"
        );
        assert!(
            e.contains(
                "agit import <full-session-id> --from <runtime> --into <owner/repo>@<branch>"
            ),
            "the next step requires complete source and destination identity: {e}"
        );
        assert!(
            !e.contains("agit import nope"),
            "a missing selector is not a resolved native ID: {e}"
        );
    }

    fn cand(id: &str, touched: bool) -> Candidate {
        let mut l = Link::new("codex", id, None);
        l.agent = Some("photo".into());
        Candidate { link: l, touched }
    }

    /// The reverse lookup behind `agit commit <agent>`: agent name → session.
    #[test]
    fn for_agent_is_the_reverse_index() {
        let (_d, s) = store();
        let mut mine = Link::new("codex", "AB", None);
        mine.agent = Some("photo".into());
        write(&s, &mine).unwrap();
        let mut other = Link::new("codex", "CD", None);
        other.agent = Some("recon".into());
        write(&s, &other).unwrap();
        // A session with no version recorded yet belongs to no agent.
        write(&s, &Link::new("claude-code", "EF", None)).unwrap();

        let got = for_agent(&s, "photo");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].link.session_id, "AB");
        assert!(for_agent(&s, "nobody").is_empty());
        // A missing transcript (the test environment has no real runtime directory) counts as
        // "not touched".
        assert!(!got[0].touched);
    }

    /// A single candidate goes straight through — asking a question with one possible answer
    /// wastes the user's time.
    #[test]
    fn a_single_candidate_needs_no_disambiguation() {
        let one = [cand("AB", false)];
        assert_eq!(only_one(&one).unwrap().link.session_id, "AB");
        assert!(only_one(&[]).is_none());
    }

    /// Once one memory is installed into two runtimes, "which one has new content" is a fact,
    /// not a guess.
    ///
    /// You work in one of them only; the other's transcript stops at the moment of install.
    #[test]
    fn two_runtimes_from_one_install_resolve_to_the_one_worked_in() {
        let both = [cand("codex-side", false), cand("cc-side", true)];
        assert_eq!(only_one(&both).unwrap().link.session_id, "cc-side");
    }

    /// With both touched (or neither), the user has to say which.
    #[test]
    fn genuinely_ambiguous_candidates_are_refused() {
        assert!(
            only_one(&[cand("AB", true), cand("CD", true)]).is_none(),
            "picking one while both have new content records the work into another lineage"
        );
        assert!(
            only_one(&[cand("AB", false), cand("CD", false)]).is_none(),
            "with neither touched there is equally nothing to go on"
        );
    }

    /// Re-importing an already adopted session must not erase the lineage.
    ///
    /// Regression test for a real bug: building a brand-new `Link` on every `import` drops
    /// whatever the existing one holds.
    #[test]
    fn re_reading_an_existing_link_preserves_lineage() {
        let (_d, s) = store();
        let mut l = Link::new("codex", "AB", Some(Path::new("/r")));
        l.agent = Some("photo".into());
        write(&s, &l).unwrap();

        let back = get(&s, "codex", "AB").expect("an adopted session must read back");
        assert_eq!(back.agent.as_deref(), Some("photo"));
        assert!(get(&s, "codex", "NOPE").is_none());
    }

    #[test]
    fn rewriting_a_link_is_idempotent() {
        // Importing the same session twice is the same path with the same content, a no-op.
        let (_d, s) = store();
        let l = Link::new("codex", "AB", Some(Path::new("/r")));
        let p1 = write(&s, &l).unwrap();
        let c1 = std::fs::read_to_string(&p1).unwrap();
        let p2 = write(&s, &l).unwrap();
        assert_eq!(p1, p2);
        assert_eq!(c1, std::fs::read_to_string(&p2).unwrap());
    }
}
