//! Low-confidence values in what a push publishes.
//!
//! Entropy alone also matches runtime ids, hashes and encrypted carriers, and a session's raw
//! records hold many of them in records publication leaves out. So the low-confidence listing
//! reads only what push publishes: each session's LOG records as
//! [`publication_records::published_sequence`] keeps them, the files as every branch tip holds
//! them, the files staged for the next commit and the working-tree files that differ from the
//! index, and commit and tag messages. A scan of one push's branches reads those branches only,
//! their messages the destination doesn't hold, and the checkout only when it holds one of them. Event objects are read only through the LOG that names
//! them, so a record publication leaves out is never read, and a working-tree link is never
//! followed, since Git stores and publishes the link itself. The high-confidence scan keeps its
//! own, wider surface ([`scan_agent_repo`]).

use super::*;
use crate::domain::repo::{ObjectBody, Repo};
use crate::domain::{meta, publication_records, storage};
use serde_json::Value;

/// The low-confidence values found in what a push publishes.
pub struct PublishedLowConfidence {
    /// The first values found, each labelled with where it is.
    pub listed: Vec<Hit>,
    /// How many distinct values of each shape were counted, in [`Shape::ALL`] order.
    pub counts: [usize; Shape::ALL.len()],
    /// Every part was read and every value on it counted, so the counts are exact.
    pub counted_all: bool,
    /// The parts that were not read.
    pub unscanned: Unscanned,
}

impl PublishedLowConfidence {
    /// How many distinct values were counted.
    pub fn count(&self) -> usize {
        self.counts.iter().sum()
    }

    /// How many distinct values of this shape were counted.
    pub fn count_of(&self, shape: Shape) -> usize {
        self.counts[shape.index()]
    }
}

/// Look for low-confidence values in what a push of this repo publishes, within the surface
/// `plan` names: every branch, or the branches of one push ([`ScanPlan::of_branches`]), with their
/// history the destination doesn't hold, and the working tree only where the plan scans it.
/// Values the user allowed, in the privacy dictionary or the local allowlist, are skipped as the
/// full scan skips them.
pub fn published_low_confidence(
    repo: &Repo,
    allowed: &HashSet<String>,
    plan: &ScanPlan,
) -> crate::Result<PublishedLowConfidence> {
    published_low_confidence_within(repo, allowed, plan, &ScanLimits::DEFAULT)
}

fn published_low_confidence_within(
    repo: &Repo,
    allowed: &HashSet<String>,
    plan: &ScanPlan,
    limits: &ScanLimits,
) -> crate::Result<PublishedLowConfidence> {
    let home = crate::infra::config::agit_home().context(ScanPreparationFailure::LocalState)?;
    let mut allowlist = load_allowlist(&home);
    allowlist.extend(allowed.iter().cloned());
    let registered = registered_matcher_for_repo(repo)?;
    let worktree = plan.scans_worktree(repo);
    let repo = repo.clone().local_objects_only();
    let tips: Vec<(String, String)> = branch_tips(&repo)?
        .into_iter()
        .filter(|(branch, _)| {
            plan.branches
                .as_ref()
                .is_none_or(|branches| branches.contains(branch))
        })
        .collect();
    let mut pass = Pass::new(&repo, &allowlist, limits);
    for (branch, tip) in &tips {
        let session = meta::read_at_ref(&repo, tip)
            .filter(|metadata| !metadata.is_file_line())
            .map(|metadata| metadata.layout);
        if let Some(layout) = session {
            pass.records(branch, tip, layout);
        }
        pass.tip_files(branch, tip, session)?;
    }
    let checkout = meta::read(repo.root())
        .filter(|metadata| !metadata.is_file_line())
        .map(|metadata| metadata.layout);
    if worktree && inside_work_tree(&repo) {
        pass.staged(checkout)?;
        pass.working_tree(checkout)?;
    }
    let Pass {
        mut out,
        mut unscanned,
        ..
    } = pass;
    let branches: Vec<String> = tips.into_iter().map(|(branch, _)| branch).collect();
    // One push's messages are those of its commits the destination doesn't hold; the whole
    // repository's are every branch's.
    let revisions = match &plan.branches {
        Some(_) => plan.revs(),
        None => vec!["--branches".to_owned()],
    };
    let revisions: Vec<&str> = revisions.iter().map(String::as_str).collect();
    scan_commit_messages(
        &repo,
        HistorySelection::Revisions(&revisions),
        limits,
        &allowlist,
        &registered,
        &mut out,
        &mut unscanned,
    )?;
    scan_tag_objects(
        &repo,
        limits,
        &allowlist,
        &registered,
        TagSelection::Branches(&branches),
        &mut out,
        &mut unscanned,
    )?;
    Ok(PublishedLowConfidence {
        counted_all: !out.incomplete && unscanned.is_empty(),
        counts: out.low_confidence_shapes,
        listed: out.low_confidence,
        unscanned,
    })
}

fn inside_work_tree(repo: &Repo) -> bool {
    repo.git_opt(&["rev-parse", "--is-inside-work-tree"])
        .as_deref()
        .map(str::trim)
        == Some("true")
}

/// Every local branch with the commit at its tip.
fn branch_tips(repo: &Repo) -> crate::Result<Vec<(String, String)>> {
    let Some(out) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/heads",
    ]) else {
        anyhow::bail!("git cannot list the local branches, so what push publishes cannot be read");
    };
    Ok(out
        .lines()
        .filter_map(|line| {
            let (tip, name) = line.trim().split_once(' ')?;
            Some((name.strip_prefix("refs/heads/")?.to_owned(), tip.to_owned()))
        })
        .collect())
}

/// Whether a path holds session data that publication does not carry as it is: the metadata,
/// which no secret scan reads (see [`in_publish_surface`]), and a session line's LOG, VIEW and
/// event objects, whose records reach publication only as [`publication_records`] keeps them. On
/// a file line these names are ordinary files.
fn is_session_payload(path: &str, session: Option<meta::LayoutVersion>) -> bool {
    path == meta::FILE
        || match session {
            Some(meta::LayoutVersion::V1) => {
                matches!(path, meta::LOG_FILE | meta::VIEW_FILE)
                    || path
                        .strip_prefix(meta::EVENTS_DIR)
                        .is_some_and(|rest| rest.starts_with('/'))
            }
            Some(meta::LayoutVersion::V0) => {
                matches!(path, meta::LEGACY_LOG_FILE | meta::LEGACY_VIEW_FILE)
            }
            None => false,
        }
}

/// Leaves out the ids a Claude-shaped record is chained and attributed by, each only when it has
/// that field's id shape: a UUID in the top-level `uuid`, `parentUuid` and `sessionId`, the
/// runtime's request id in `requestId` and its message id in `message.id`. A value of any other
/// shape in those fields is listed, and so is the same value anywhere else in the record.
fn without_chaining_ids(record: &mut Value) {
    let Some(fields) = record.as_object_mut() else {
        return;
    };
    for field in ["uuid", "parentUuid", "sessionId"] {
        if fields
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(is_uuid)
        {
            fields.remove(field);
        }
    }
    if fields
        .get("requestId")
        .and_then(Value::as_str)
        .is_some_and(|id| is_runtime_id(id, "req_"))
    {
        fields.remove("requestId");
    }
    if let Some(message) = fields.get_mut("message").and_then(Value::as_object_mut)
        && message
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| is_runtime_id(id, "msg_"))
    {
        message.remove("id");
    }
}

/// An API request or message id: its prefix, `bdrk_` or `vrtx_` when a cloud provider served it,
/// then at least 16 letters and digits.
fn is_runtime_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|rest| {
        let rest = ["bdrk_", "vrtx_"]
            .iter()
            .find_map(|provider| rest.strip_prefix(provider))
            .unwrap_or(rest);
        rest.len() >= 16 && rest.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

/// One low-confidence pass. Its read budget is the full scan's, counted the same way: a part is
/// sized before it is read, and once the budget is spent nothing more is read.
struct Pass<'a> {
    repo: &'a Repo,
    allowlist: &'a HashSet<String>,
    limits: &'a ScanLimits,
    spent: u64,
    /// File blobs already read: a file shared by several branches is listed once.
    blobs: HashSet<String>,
    out: HitCollector,
    unscanned: Unscanned,
}

impl<'a> Pass<'a> {
    fn new(repo: &'a Repo, allowlist: &'a HashSet<String>, limits: &'a ScanLimits) -> Self {
        Pass {
            repo,
            allowlist,
            limits,
            spent: 0,
            blobs: HashSet::new(),
            out: HitCollector::new(),
            unscanned: Unscanned::default(),
        }
    }

    /// Count `bytes` against the budget before reading them; `false` once the budget is spent.
    fn reserve(&mut self, bytes: u64) -> bool {
        if self.unscanned.over_budget.is_some() {
            return false;
        }
        if bytes > self.limits.budget_bytes.saturating_sub(self.spent) {
            self.unscanned.over_budget =
                Some((self.spent.saturating_add(bytes), self.limits.budget_bytes));
            return false;
        }
        self.spent += bytes;
        true
    }

    /// Scan one carrier and keep its low-confidence hits, placed by `place`.
    fn scan(&mut self, text: &str, place: impl Fn(&mut Hit)) {
        let scanned = scan_text_capped(text, self.allowlist, Policy::CLIENT, self.out.remaining());
        if scanned.truncated {
            self.out.mark_truncated();
        }
        self.out.extend(
            scanned
                .hits
                .into_iter()
                .filter(|hit| !hit.is_high_confidence())
                .map(|mut hit| {
                    place(&mut hit);
                    hit
                }),
        );
    }

    /// A session's records as publication carries them, each scanned on its own and located by
    /// its LOG line.
    ///
    /// The LOG is expanded only within what the budget has left and the size one carrier may
    /// have, both checked against the stored sizes before anything is read: a session past the
    /// carrier size is reported unread, and once the budget is spent no later session is
    /// expanded at all.
    fn records(&mut self, branch: &str, tip: &str, layout: meta::LayoutVersion) {
        let label = format!("{branch}:{}", meta::LOG_FILE);
        if self.unscanned.over_budget.is_some() || self.out.is_full() {
            return;
        }
        let left = self.limits.budget_bytes.saturating_sub(self.spent);
        let bound = left.min(self.limits.max_object_bytes);
        let mut work = storage::LocalReadBudget::with_deadline(
            usize::MAX,
            crate::infra::local_git::Deadline::new(),
        );
        let log = match storage::materialize_log_local(
            self.repo.root(),
            tip,
            layout,
            usize::try_from(bound).unwrap_or(usize::MAX),
            storage::MAX_SEQUENCE_EVENTS,
            &mut work,
        ) {
            Ok(log) => log,
            Err(error)
                if left < self.limits.max_object_bytes
                    && error.downcast_ref::<storage::ReadLimitExceeded>().is_some() =>
            {
                self.unscanned.over_budget = Some((
                    self.spent.saturating_add(left).saturating_add(1),
                    self.limits.budget_bytes,
                ));
                return;
            }
            Err(_) => {
                self.unscanned.record_unsupported(label);
                return;
            }
        };
        self.spent = self.spent.saturating_add(log.len() as u64);
        let Ok(envelopes) = log
            .split_inclusive('\n')
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut line = line.to_owned();
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                storage::parse_legacy_envelope_line(&line)
            })
            .collect::<crate::Result<Vec<_>>>()
        else {
            self.unscanned.record_unsupported(label);
            return;
        };
        let published = publication_records::published_sequence(
            envelopes
                .iter()
                .map(|envelope| (envelope.source.as_str(), &envelope.content)),
        );
        for (index, record) in published.into_iter().enumerate() {
            let Some(mut record) = record else {
                continue;
            };
            if self.out.is_full() {
                return;
            }
            without_chaining_ids(&mut record);
            self.scan(&record.to_string(), |hit| {
                hit.file = Some(label.clone());
                hit.line = index + 1;
                hit.source = Source::Record;
            });
        }
    }

    /// The files at a branch tip, apart from session data.
    fn tip_files(
        &mut self,
        branch: &str,
        tip: &str,
        session: Option<meta::LayoutVersion>,
    ) -> crate::Result<()> {
        let listing = self
            .repo
            .git_bytes_result(&["ls-tree", "-r", "-z", "--full-tree", tip])?;
        let mut paths = HashMap::new();
        for entry in listing.split(|byte| *byte == 0) {
            let Some((header, path)) = std::str::from_utf8(entry)
                .ok()
                .and_then(|entry| entry.split_once('\t'))
            else {
                continue;
            };
            let mut fields = header.split(' ');
            let (Some(_), Some("blob"), Some(oid)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if !is_session_payload(path, session) && self.blobs.insert(oid.to_owned()) {
                paths.insert(oid.to_owned(), format!("{branch}:{path}"));
            }
        }
        self.read_blobs(paths, Source::BlobObject)
    }

    /// The files staged for the next commit, as the index holds them, apart from session data.
    /// A file staged and then changed again is read here and in the working tree too.
    fn staged(&mut self, session: Option<meta::LayoutVersion>) -> crate::Result<()> {
        let listing = self.repo.git_bytes_result(&[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-renames",
            "--no-abbrev",
            "--diff-filter=d",
        ])?;
        let mut paths = HashMap::new();
        let mut fields = listing.split(|byte| *byte == 0);
        // Each change is `:<old mode> <new mode> <old oid> <new oid> <status>`, then its path.
        while let (Some(header), Some(path)) = (fields.next(), fields.next()) {
            let (Ok(header), Ok(path)) = (std::str::from_utf8(header), std::str::from_utf8(path))
            else {
                continue;
            };
            let mut parts = header.trim_start_matches(':').split(' ');
            let (Some(_), Some(mode), Some(_), Some(oid)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            // A gitlink names a commit of another repository, not content of this one.
            if mode == "160000" || !in_publish_surface(path) || is_session_payload(path, session) {
                continue;
            }
            if self.blobs.insert(oid.to_owned()) {
                paths.insert(oid.to_owned(), path.to_owned());
            }
        }
        self.read_blobs(paths, Source::File)
    }

    /// Read and scan the blobs `paths` labels, each sized before it is read.
    fn read_blobs(&mut self, paths: HashMap<String, String>, source: Source) -> crate::Result<()> {
        let mut sized = Vec::with_capacity(paths.len());
        self.repo
            .git_cat_file_batch_check(paths.keys().cloned().collect(), |oid, _, size| {
                sized.push((oid.to_owned(), size));
                Ok(())
            })?;
        let mut wanted = Vec::with_capacity(sized.len());
        for (oid, size) in sized {
            if size > self.limits.max_object_bytes {
                self.unscanned.oversized.push((paths[&oid].clone(), size));
            } else if self.reserve(size) {
                wanted.push(oid);
            }
        }
        let max = usize::try_from(self.limits.max_object_bytes).unwrap_or(usize::MAX);
        let repo = self.repo;
        repo.git_cat_file_batch(wanted, max, |oid, _, body| {
            let label = paths[oid].clone();
            match body {
                ObjectBody::TooLarge(size) => self.unscanned.oversized.push((label, size as u64)),
                ObjectBody::Read(bytes) => {
                    if let Some(text) = readable_text(bytes) {
                        self.scan(text, |hit| {
                            hit.file = Some(label.clone());
                            hit.source = source;
                        });
                    }
                }
            }
            Ok(())
        })
    }

    /// The working-tree files that differ from the index, apart from session data; the rest was
    /// read at its branch tip or in the index. Only regular files reached through real
    /// directories are read: Git stores a link as its text, so what a link points at is never
    /// published.
    fn working_tree(&mut self, session: Option<meta::LayoutVersion>) -> crate::Result<()> {
        let listing = self.repo.git_bytes_result(&[
            "ls-files",
            "-z",
            "--modified",
            "--others",
            "--exclude-standard",
        ])?;
        let mut seen = HashSet::new();
        for path in listing.split(|byte| *byte == 0) {
            let Ok(path) = std::str::from_utf8(path) else {
                continue;
            };
            if path.is_empty()
                || !seen.insert(path)
                || !in_publish_surface(path)
                || is_session_payload(path, session)
            {
                continue;
            }
            let Some(metadata) = regular_file_within(self.repo.root(), path) else {
                continue;
            };
            let file = self.repo.root().join(path);
            let size = metadata.len();
            if size > self.limits.max_object_bytes {
                self.unscanned.oversized_files.push((path.to_owned(), size));
                continue;
            }
            if is_lfs_worktree_file(self.repo, path)? || !self.reserve(size) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else {
                self.unscanned
                    .record_unsupported(format!("working-tree file {path}"));
                continue;
            };
            if let Some(text) = readable_text(&bytes) {
                self.scan(text, |hit| {
                    hit.file = Some(path.to_owned());
                    hit.source = Source::File;
                });
            }
        }
        Ok(())
    }
}

/// The metadata of `path` when it names a regular file under `root` reached through real
/// directories only. A link anywhere on the way, the file itself or a directory above it, can
/// lead outside the repository, so it yields `None`.
fn regular_file_within(root: &Path, path: &str) -> Option<std::fs::Metadata> {
    let mut at = root.to_path_buf();
    let mut components = Path::new(path).components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        at.push(part);
        let metadata = std::fs::symlink_metadata(&at).ok()?;
        if components.peek().is_none() {
            return metadata.file_type().is_file().then_some(metadata);
        }
        if !metadata.file_type().is_dir() {
            return None;
        }
    }
    None
}

/// A carrier's text, or `None` for binary content and an LFS pointer, which the full scan does
/// not read as text either.
fn readable_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    crate::domain::lfs::Pointer::parse(bytes)
        .ok()
        .flatten()
        .is_none()
        .then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chaining_id_is_left_out_only_where_and_as_the_runtime_writes_it() {
        const ID: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let mut record = serde_json::json!({
            "uuid": ID,
            "parentUuid": ID,
            "sessionId": "not-a-session-id-but-a-pasted-value",
            "requestId": "req_011CUnJ3pQn5xyZ8AbCdEfGh",
            "message": {"id": "msg_bdrk_01XFDUDYJgAACzvnptvVoYEL", "content": ID},
            "toolUseResult": {"id": ID, "requestId": "req_011CUnJ3pQn5xyZ8AbCdEfGh"},
        });
        without_chaining_ids(&mut record);
        assert_eq!(
            record,
            serde_json::json!({
                "sessionId": "not-a-session-id-but-a-pasted-value",
                "message": {"content": ID},
                "toolUseResult": {"id": ID, "requestId": "req_011CUnJ3pQn5xyZ8AbCdEfGh"},
            })
        );
        let mut other =
            serde_json::json!({"uuid": "u1", "requestId": "req_short", "message": {"id": ID}});
        let before = other.clone();
        without_chaining_ids(&mut other);
        assert_eq!(other, before);
    }

    /// A session past the size one carrier may have is reported unread, and once the budget is
    /// spent no later session is expanded: none of their values is listed and the count is not
    /// exact. A pass that expands the LOG first and checks its size after would list them.
    #[test]
    fn a_session_past_a_read_bound_is_never_expanded() {
        const FIRST: &str = "3f2a9c1e-5b7d-4e8f-a6c2-9d1b0e7f4a35";
        const SECOND: &str = "8e1d4b6a-2c9f-4a7e-b3d5-6f0c1a9e2b48";
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        for (key, value) in [
            ("commit.gpgsign", "false"),
            ("user.name", "agit"),
            ("user.email", "agit@localhost"),
        ] {
            repo.git(&["config", key, value]).unwrap();
        }
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        std::fs::write(repo.root().join("README.md"), "fixture").unwrap();
        repo.add_all().unwrap();
        repo.commit("File line").unwrap();
        for (branch, id) in [("a", FIRST), ("b", SECOND)] {
            let session = meta::Meta::new(
                format!("agit-{}", branch.repeat(40)),
                "claude-code".into(),
                String::new(),
            );
            let record = serde_json::json!({"type": "user", "message": {"role": "user", "content": format!("job {id}")}});
            let log = crate::domain::transcript::wrap_lines(
                &format!("{record}\n"),
                "claude-code",
                &session.session,
            );
            repo.git(&["checkout", "-q", "-B", branch, "main"]).unwrap();
            storage::write_snapshot(repo.root(), &log, &log).unwrap();
            meta::write(repo.root(), &session).unwrap();
            repo.add_all().unwrap();
            repo.commit(branch).unwrap();
        }
        repo.git(&["checkout", "-q", "main"]).unwrap();
        let scan = |limits: ScanLimits| {
            let report =
                published_low_confidence_within(&repo, &HashSet::new(), &ScanPlan::full(), &limits)
                    .unwrap();
            let listed: Vec<String> = report
                .listed
                .iter()
                .map(|hit| hit.redacted.clone())
                .collect();
            (report, listed)
        };

        let (whole, listed) = scan(ScanLimits::DEFAULT);
        assert_eq!(listed, [redact(FIRST), redact(SECOND)]);
        assert!(whole.counted_all);

        let (report, listed) = scan(ScanLimits {
            max_object_bytes: 64,
            ..ScanLimits::DEFAULT
        });
        assert!(listed.is_empty(), "{listed:?}");
        assert_eq!(report.unscanned.unsupported, ["a:LOG", "b:LOG"]);
        assert!(!report.counted_all);

        let (report, listed) = scan(ScanLimits {
            budget_bytes: 64,
            ..ScanLimits::DEFAULT
        });
        assert!(listed.is_empty(), "{listed:?}");
        assert!(report.unscanned.over_budget.is_some());
        assert!(
            report.unscanned.unsupported.is_empty(),
            "{:?}",
            report.unscanned
        );
        assert!(!report.counted_all);
    }

    #[test]
    fn session_data_is_set_aside_only_on_a_session_line() {
        use meta::LayoutVersion::{V0, V1};
        for (path, session, payload) in [
            ("LOG", Some(V1), true),
            ("VIEW", Some(V1), true),
            ("events/0/0/0/0/abc", Some(V1), true),
            ("eventsfile", Some(V1), false),
            ("session/log.jsonl", Some(V0), true),
            ("LOG", Some(V0), false),
            ("LOG", None, false),
            ("events/notes.md", None, false),
            ("session/meta.json", None, true),
            ("memory/notes.md", Some(V1), false),
        ] {
            assert_eq!(
                is_session_payload(path, session),
                payload,
                "{path} {session:?}"
            );
        }
    }
}
