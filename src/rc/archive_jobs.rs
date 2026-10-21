//! Completed native prefixes remain pending until protected publication has a durable receipt.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use crate::{
    domain::{meta, repo::Repo},
    protocol::NativeSourceRef,
    rc::{capture::RepositoryKind, lineage::AgitSession},
};

const MAX_JOBS: usize = 4096;
const MAX_JOB_BYTES: u64 = 32 * 1024;
const MAX_PREFIX_BYTES: u64 = crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64;

/// The job's evidence can never settle as recorded: retrying cannot change the outcome until
/// the claim or the transcript does.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Unrecoverable(pub &'static str);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    version: u32,
    pub logical: String,
    pub native: String,
    pub runtime: String,
    pub native_source: Option<NativeSourceRef>,
    pub cwd: PathBuf,
    pub lineage: String,
    pub repository_id: String,
    pub capture: Option<RepositoryKind>,
    pub turn_id: String,
    pub transcript: PathBuf,
    pub prefix_bytes: u64,
    pub prefix_hash: String,
    pub required_bytes: u64,
    pub archive_handoff: Option<crate::commands::commit::archive::RcHandoff>,
}

impl Job {
    /// Completed prefixes of one capture settle and publish through the same canonical branch,
    /// whichever segment of the conversation's lineage each was queued against.
    pub(crate) fn same_capture(&self, other: &Self) -> bool {
        self.logical == other.logical
            && self.native == other.native
            && self.runtime == other.runtime
            && NativeSourceRef::same_store(
                self.native_source.as_ref(),
                other.native_source.as_ref(),
            )
            && self.cwd == other.cwd
            && self.lineage == other.lineage
            && self.repository_id == other.repository_id
            && self.capture == other.capture
            && self.archive_handoff == other.archive_handoff
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        logical: &str,
        native: &str,
        runtime: &str,
        native_source: Option<NativeSourceRef>,
        cwd: &Path,
        lineage: &AgitSession,
        turn_id: &str,
        transcript: &Path,
        prefix_bytes: u64,
        archive_handoff: Option<crate::commands::commit::archive::RcHandoff>,
    ) -> crate::Result<Self> {
        let prefix_hash = prefix_digest(transcript, prefix_bytes)?;
        let job = Self {
            version: 1,
            logical: logical.into(),
            native: native.into(),
            runtime: runtime.into(),
            native_source,
            cwd: cwd.canonicalize()?,
            lineage: lineage.to_string(),
            repository_id: lineage.agent_id().into(),
            capture: lineage.capture.clone(),
            turn_id: turn_id.into(),
            transcript: transcript.canonicalize()?,
            prefix_bytes,
            prefix_hash,
            required_bytes: prefix_bytes,
            archive_handoff,
        };
        job.validate()?;
        Ok(job)
    }

    fn validate(&self) -> crate::Result<()> {
        ensure!(self.version == 1, "unsupported pending archive version");
        for value in [&self.logical, &self.native, &self.runtime, &self.turn_id] {
            ensure!(
                !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
                "invalid pending archive identity"
            );
        }
        ensure!(
            crate::adapter::normalize(&self.runtime)? == self.runtime,
            "noncanonical archive runtime"
        );
        ensure!(
            self.cwd.is_absolute() && self.transcript.is_absolute(),
            "archive paths must be absolute"
        );
        ensure!(
            self.required_bytes > 0
                && self.required_bytes <= self.prefix_bytes
                && self.prefix_bytes <= MAX_PREFIX_BYTES,
            "invalid pending archive boundary"
        );
        ensure!(
            self.prefix_hash.len() == 64 && self.prefix_hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid archive prefix hash"
        );
        self.session()?;
        Ok(())
    }

    pub(crate) fn session(&self) -> crate::Result<AgitSession> {
        let mut lineage = AgitSession::parse(&self.lineage, &self.repository_id)?;
        lineage.capture = self.capture.clone();
        Ok(lineage)
    }

    /// The carrier still holds the completed prefix this job hashed.
    pub(crate) fn verify_prefix(&self) -> crate::Result<()> {
        self.validate()?;
        // Codex may compress a completed carrier before its archive publishes.
        if self.runtime == "codex" {
            crate::adapter::codex_rollout::restore(&self.transcript)?;
        }
        let digest = prefix_digest(&self.transcript, self.prefix_bytes).context(Unrecoverable(
            "completed archive prefix is no longer readable",
        ))?;
        if digest != self.prefix_hash {
            return Err(Unrecoverable("completed archive prefix changed").into());
        }
        Ok(())
    }

    /// Where this job's completed prefix ends in its conversation's capture stream.
    ///
    /// Codex continues a thread in new segments, so the carrier a job hashed can be an ancestor
    /// of the current leaf; it proves the prefix while it remains part of the conversation's
    /// capture. A carrier outside the lineage can never be settled by this job.
    pub(crate) fn stream_end(&self, claim: &crate::domain::link::Link) -> crate::Result<u64> {
        if claim.native_binding.is_none() {
            let current = claim
                .resolve()
                .context("archive transcript is missing")?
                .canonicalize()?;
            if current != self.transcript {
                return Err(Unrecoverable("archive transcript carrier changed").into());
            }
            return Ok(self.prefix_bytes);
        }
        let spans = claim.capture_spans()?;
        crate::adapter::codex::capture_offset(&spans, &self.transcript, self.prefix_bytes)
            .ok_or_else(|| {
                Unrecoverable("archive transcript is no longer part of its conversation").into()
            })
    }

    /// Identifies one queued completion; it names the job's file.
    pub(crate) fn filename(&self) -> String {
        let coordinates = serde_json::to_vec(&(
            &self.logical,
            &self.runtime,
            &self.native_source,
            &self.native,
            &self.lineage,
            &self.repository_id,
            &self.turn_id,
        ))
        .expect("archive coordinates serialize");
        format!("{:x}.json", Sha256::digest(coordinates))
    }

    pub(crate) fn record(&self) -> crate::Result<()> {
        self.validate()?;
        let directory = directory()?;
        let _lock = lock(&directory)?;
        let path = directory.join(self.filename());
        if let Some(saved) = read(&path)? {
            ensure!(
                saved.cwd == self.cwd
                    && saved.capture == self.capture
                    && saved.transcript == self.transcript
                    && saved.archive_handoff == self.archive_handoff,
                "replayed archive completion changed its capture"
            );
            saved.verify_prefix()?;
            return Ok(());
        }
        // Jobs leave the queue only when a confirmed publication covers them, or when a newer
        // completion of the same capture and carrier contains their prefix byte for byte: that
        // job's publication covers them too, so one job per carrier keeps the queue bounded
        // without dropping unpublished content.
        let queued = scan(&directory)?;
        let mut subsumed = Vec::new();
        for older in queued.iter().filter(|older| {
            older.same_capture(self)
                && older.transcript == self.transcript
                && older.prefix_bytes <= self.prefix_bytes
        }) {
            let contained = if older.prefix_bytes == self.prefix_bytes {
                older.prefix_hash == self.prefix_hash
            } else {
                prefix_digest(&self.transcript, older.prefix_bytes)
                    .is_ok_and(|digest| digest == older.prefix_hash)
            };
            if !contained {
                continue;
            }
            if older.prefix_bytes == self.prefix_bytes {
                return Ok(());
            }
            subsumed.push(older);
        }
        ensure!(
            queued.len() - subsumed.len() < MAX_JOBS,
            "pending archive capacity exceeded"
        );
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() as u64 <= MAX_JOB_BYTES,
            "archive job exceeds its record limit"
        );
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        sync_directory(&directory)?;
        for older in subsumed {
            fs::remove_file(directory.join(older.filename()))?;
        }
        sync_directory(&directory)
    }

    /// The claim this job settles through.
    pub(crate) fn claim(&self) -> crate::Result<crate::domain::link::Link> {
        let store = crate::domain::store::Store::open()?
            .context("archive native claim store is missing")?;
        crate::domain::link::get_checked(&store, &self.runtime, &self.claim_key())?
            .context(Unrecoverable("archive native claim is missing"))
    }

    /// Identifies the capture whose jobs share one recovery attempt (see [`Self::same_capture`]).
    pub(crate) fn capture_key(&self) -> String {
        serde_json::to_string(&(
            &self.logical,
            &self.native,
            &self.runtime,
            self.native_source.as_ref().map(|source| &source.source_id),
            &self.cwd,
            &self.lineage,
            &self.repository_id,
            &self.capture,
            &self.archive_handoff,
        ))
        .expect("archive coordinates serialize")
    }

    /// A digest of the claim file this job settles through, or `None` when there is none. A
    /// failure that only the claim can resolve is retried when this changes.
    pub(crate) fn claim_digest(&self) -> Option<String> {
        let store = crate::domain::store::Store::open().ok()??;
        let path = crate::domain::link::link_path(&store, &self.runtime, &self.claim_key());
        Some(format!("{:x}", Sha256::digest(fs::read(path).ok()?)))
    }

    pub(crate) fn claim_key(&self) -> String {
        self.native_source
            .as_ref()
            .map(|source| source.session_ref(&self.native))
            .unwrap_or_else(|| self.native.clone())
    }

    /// `end` is this job's boundary in capture-stream coordinates, from [`Self::stream_end`].
    pub(crate) fn covered_by(&self, repo: &Repo, source: &str, end: u64) -> crate::Result<bool> {
        let lineage = self.session()?;
        crate::rc::capture::require(&lineage)?;
        ensure!(
            repo.root().canonicalize()? == lineage.repo_dir()?.canonicalize()?,
            "archive repository changed"
        );
        let Some(text) = repo.show_result(source, meta::FILE)? else {
            return Ok(false);
        };
        let meta: meta::Meta = serde_json::from_str(&text)?;
        Ok(meta.baseline_bytes.is_some_and(|bytes| bytes >= end)
            && meta.runtime == self.runtime
            && meta.line == meta::Line::Session)
    }

    pub(crate) fn remove(&self) -> crate::Result<()> {
        let directory = directory()?;
        let _lock = lock(&directory)?;
        let path = directory.join(self.filename());
        if let Some(saved) = read(&path)? {
            ensure!(
                saved == *self,
                "pending archive changed before receipt acceptance"
            );
            fs::remove_file(path)?;
        }
        sync_directory(&directory)
    }
}

pub(crate) fn pending() -> crate::Result<Vec<Job>> {
    let directory = directory()?;
    let _lock = lock(&directory)?;
    scan(&directory)
}

pub(crate) fn publication_confirmed(
    repo: &Repo,
    request: &crate::domain::privacy_receipt::SupervisorPushRequest,
) -> crate::Result<()> {
    let saved = crate::domain::privacy_receipt::outbox::Entry::load(repo, request)?
        .context("archive publication intent is missing")?;
    ensure!(
        saved.publication.is_some(),
        "archive publication has no durable confirmation"
    );
    // Jobs of one capture usually share a claim and often a prefix; resolve and hash each once.
    let mut claims = std::collections::HashMap::new();
    let mut verified = std::collections::HashSet::new();
    for job in pending()? {
        if job.logical != saved.capture.session_id
            || job.native != saved.capture.native_session_id
            || job.runtime != saved.capture.runtime
            || job.session()?.repo_dir()? != repo.root()
        {
            continue;
        }
        let covered = (|| -> crate::Result<bool> {
            let claim = match claims.entry((job.runtime.clone(), job.claim_key())) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => entry.insert(job.claim()?),
            };
            let end = job.stream_end(claim)?;
            if !job.covered_by(repo, &request.source, end)? {
                return Ok(false);
            }
            let proof = (
                job.transcript.clone(),
                job.prefix_bytes,
                job.prefix_hash.clone(),
            );
            if !verified.contains(&proof) {
                job.verify_prefix()?;
                verified.insert(proof);
            }
            Ok(true)
        })();
        // An unverifiable job stays queued for its own attempt to classify; it must neither
        // keep covered siblings queued nor fail the publication that covered them.
        if covered.unwrap_or(false) {
            job.remove()?;
        }
    }
    Ok(())
}

/// The SHA-256 of a carrier's first `bytes`, which must end on a record boundary.
fn prefix_digest(path: &Path, bytes: u64) -> crate::Result<String> {
    use std::io::{Seek, SeekFrom};
    ensure!(
        bytes > 0 && bytes <= MAX_PREFIX_BYTES,
        "invalid archive prefix extent"
    );
    let mut file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() >= bytes,
        "completed archive prefix is incomplete"
    );
    file.seek(SeekFrom::Start(bytes - 1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    ensure!(last == *b"\n", "completed archive prefix is incomplete");
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let copied = std::io::copy(&mut file.take(bytes), &mut hash)?;
    ensure!(copied == bytes, "completed archive prefix is incomplete");
    Ok(format!("{:x}", hash.finalize()))
}

fn directory() -> crate::Result<PathBuf> {
    let path = super::rc_dir()?.join("pending-archives");
    crate::infra::config::create_state_dir(&path)?;
    Ok(path)
}

fn lock(directory: &Path) -> crate::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("jobs.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn read(path: &Path) -> crate::Result<Option<Job>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.len() <= MAX_JOB_BYTES,
        "archive job exceeds its record limit"
    );
    let job: Job = serde_json::from_reader(file.take(MAX_JOB_BYTES + 1))?;
    job.validate()?;
    Ok(Some(job))
}

fn scan(directory: &Path) -> crate::Result<Vec<Job>> {
    let mut jobs = vec![];
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        ensure!(jobs.len() < MAX_JOBS, "pending archive capacity exceeded");
        if let Some(job) = read(&path)? {
            ensure!(
                path == directory.join(job.filename()),
                "pending archive is stored under another identity"
            );
            jobs.push(job);
        }
    }
    jobs.sort_by_cached_key(|job| job.filename());
    Ok(jobs)
}

fn sync_directory(directory: &Path) -> crate::Result<()> {
    #[cfg(not(unix))]
    let _ = directory;
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn transcript() -> String {
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"native-one\",\"cwd\":\"/fixture\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"inspect\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"call-one\",\"arguments\":\"{}\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-one\",\"output\":\"ok\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-one\"}}\n"
        ).into()
    }

    pub(crate) fn fixture(root: &Path) -> Job {
        let path = root.join("native.jsonl");
        let text = transcript();
        fs::write(&path, &text).unwrap();
        let lineage = AgitSession::parse(
            "alice/project@conversation",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        Job::capture(
            "logical-one",
            "native-one",
            "codex",
            None,
            root,
            &lineage,
            "turn-one",
            &path,
            text.len() as u64,
            None,
        )
        .unwrap()
    }

    /// Restart and replay retain the original completed prefix even when a later turn arrives.
    #[test]
    fn archive_jobs_survive_restart_and_replay_without_recapturing_new_input() {
        let root = tempfile::tempdir().unwrap();
        super::super::with_agit_home(root.path(), || {
            let job = fixture(root.path());
            job.record().unwrap();
            let mut text = fs::read_to_string(&job.transcript).unwrap();
            text.push_str("{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"another turn\"}]}}\n");
            fs::write(&job.transcript, &text).unwrap();
            let mut replay = job.clone();
            replay.prefix_bytes = text.len() as u64;
            replay.prefix_hash = format!("{:x}", Sha256::digest(text.as_bytes()));
            replay.record().unwrap();
            let recovered = pending().unwrap();
            assert_eq!(recovered, vec![job.clone()]);
            job.verify_prefix().unwrap();
            assert_eq!(
                crate::commands::commit::completed_native_boundary("codex", &text).unwrap(),
                Some(job.required_bytes)
            );
            let mut replaced = text.into_bytes();
            replaced[job.prefix_bytes as usize - 2] = b' ';
            fs::write(&job.transcript, replaced).unwrap();
            assert!(job.verify_prefix().is_err());
            assert!(replay.record().is_err());
            assert_eq!(
                pending().unwrap(),
                recovered,
                "invalid replay must retain the original pending work"
            );
        });
    }

    /// A completion queued against a thread's first carrier stays settleable after Codex
    /// continues the thread in a new segment that starts before that carrier's end: the carrier
    /// is still part of the capture, at the same stream offset. A newer completion of the same
    /// carrier replaces the queued ones it contains. A carrier outside the conversation is
    /// reported as unrecoverable rather than as a failure worth retrying.
    #[test]
    fn a_completion_queued_before_a_rollover_maps_into_the_current_lineage() {
        if crate::rc::in_isolated_test(
            "rc::archive_jobs::tests::a_completion_queued_before_a_rollover_maps_into_the_current_lineage",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let cwd = root.path().join("project");
        fs::create_dir(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let home = root.path().join("codex");
        let sessions = home.join("sessions/2026/10/08");
        fs::create_dir_all(&sessions).unwrap();
        let thread = uuid::Uuid::now_v7().to_string();
        let segment = uuid::Uuid::now_v7().to_string();
        let line = |value: serde_json::Value| format!("{value}\n");
        let header = |ordinal: u64, base: Option<usize>| {
            let mut payload = serde_json::json!({"id": thread, "cwd": cwd});
            if let Some(offset) = base {
                payload["history_mode"] = "paginated".into();
                payload["history_base"] = serde_json::json!({
                    "thread_id": thread, "end_ordinal_exclusive": ordinal, "end_byte_offset": offset,
                });
            }
            line(serde_json::json!({"type":"session_meta","ordinal":ordinal,"payload":payload}))
        };
        let user = |ordinal: u64, text: &str| {
            line(
                serde_json::json!({"type":"response_item","ordinal":ordinal,"payload":{
                "type":"message","role":"user","content":[{"type":"input_text","text":text}]}}),
            )
        };
        let settled = format!("{}{}", header(0, None), user(1, "first"));
        let interrupted = format!("{settled}{}", user(2, "interrupted"));
        let root_path = sessions.join(format!("rollout-2026-10-08T00-00-00-{thread}.jsonl"));
        fs::write(&root_path, &interrupted).unwrap();
        let segment_path = sessions.join(format!(
            "rollout-2026-10-12T00-00-00-{thread}_{segment}.jsonl"
        ));
        fs::write(
            &segment_path,
            format!(
                "{}{}",
                header(2, Some(settled.len())),
                user(3, "replacement")
            ),
        )
        .unwrap();
        let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, archived INTEGER, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1,?2,?3,0,'question','cli',1)",
            rusqlite::params![thread, segment_path.to_str(), cwd.to_str()],
        )
        .unwrap();
        let source = crate::rc::runtime_sources::Registry::open()
            .unwrap()
            .register(&home, None, None, None)
            .unwrap();
        let reference = NativeSourceRef {
            source_id: source.source_id.clone(),
            generation: source.generation,
        };
        let store = crate::domain::store::Store::open_or_init().unwrap();
        let claim = crate::domain::link::Link::from_native(
            crate::domain::link::NativeBinding {
                source: reference.clone(),
                thread_id: thread.clone(),
            },
            &cwd,
        )
        .unwrap();
        crate::domain::link::write(&store, &claim).unwrap();
        let lineage = AgitSession::parse(
            "alice/project@conversation",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        let queue = |turn: &str, transcript: &Path, bytes: usize| {
            Job::capture(
                "logical-one",
                &thread,
                "codex",
                Some(reference.clone()),
                &cwd,
                &lineage,
                turn,
                transcript,
                bytes as u64,
                None,
            )
            .unwrap()
        };

        let earlier = queue("first-turn", &root_path, settled.len());
        earlier.record().unwrap();
        let job = queue("interrupted-turn", &root_path, interrupted.len());
        job.record().unwrap();
        queue("replayed-turn", &root_path, interrupted.len())
            .record()
            .unwrap();
        assert_eq!(pending().unwrap(), vec![job.clone()]);

        job.verify_prefix().unwrap();
        assert_eq!(
            job.stream_end(&job.claim().unwrap()).unwrap(),
            interrupted.len() as u64
        );
        let captured = String::from_utf8(claim.read_bytes().unwrap()).unwrap();
        assert!(captured.starts_with(&interrupted) && captured.ends_with(&user(3, "replacement")));

        let stray_path = sessions.join(format!(
            "rollout-2026-10-08T00-00-00-{}.jsonl",
            uuid::Uuid::now_v7()
        ));
        fs::write(&stray_path, &interrupted).unwrap();
        let stray = queue("stray-turn", &stray_path, interrupted.len());
        let error = stray.stream_end(&stray.claim().unwrap()).unwrap_err();
        assert!(error.is::<Unrecoverable>(), "{error:#}");
    }

    #[test]
    fn a_native_completion_cannot_inherit_an_older_turns_publication_boundary() {
        let root = tempfile::tempdir().unwrap();
        let earlier = fixture(root.path());
        let text = concat!(
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"Interrupted before any model output\"}}\n",
        );
        fs::write(&earlier.transcript, text).unwrap();
        let job = Job::capture(
            &earlier.logical,
            &earlier.native,
            "claude-code",
            None,
            root.path(),
            &earlier.session().unwrap(),
            "interrupted-turn",
            &earlier.transcript,
            text.len() as u64,
            None,
        )
        .unwrap();
        assert!(
            crate::commands::commit::completed_native_boundary("claude-code", text)
                .unwrap()
                .unwrap()
                < job.required_bytes
        );
        assert_eq!(
            job.required_bytes,
            text.len() as u64,
            "publishing an earlier reply cannot acknowledge the interrupted prompt"
        );
    }

    /// Corrupt evidence cannot be converted into an empty archive queue or overwritten by replay.
    #[test]
    fn unreadable_archive_jobs_remain_a_recovery_error() {
        let root = tempfile::tempdir().unwrap();
        super::super::with_agit_home(root.path(), || {
            let job = fixture(root.path());
            job.record().unwrap();
            let path = directory().unwrap().join(job.filename());
            fs::write(&path, b"{not-json").unwrap();
            assert!(pending().is_err());
            assert!(job.record().is_err());
            assert_eq!(fs::read(&path).unwrap(), b"{not-json");
        });
    }
}
