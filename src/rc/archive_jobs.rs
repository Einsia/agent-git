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
const MAX_PREFIX_BYTES: u64 = 512 * 1024 * 1024;

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
        let text = prefix(transcript, prefix_bytes)?;
        let required_bytes = crate::commands::commit::completed_native_boundary(runtime, &text)?
            .context("completed native turn has no archive boundary")?;
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
            prefix_hash: format!("{:x}", Sha256::digest(text.as_bytes())),
            required_bytes,
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

    pub(crate) fn verify_prefix(&self, path: &Path) -> crate::Result<()> {
        self.validate()?;
        ensure!(
            path.canonicalize()? == self.transcript,
            "archive transcript carrier changed"
        );
        let text = prefix(path, self.prefix_bytes)?;
        ensure!(
            format!("{:x}", Sha256::digest(text.as_bytes())) == self.prefix_hash,
            "completed archive prefix changed"
        );
        Ok(())
    }

    fn filename(&self) -> String {
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
            saved.verify_prefix(&self.transcript)?;
            return Ok(());
        }
        ensure!(
            scan(&directory)?.len() < MAX_JOBS,
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
        sync_directory(&directory)
    }

    pub(crate) fn covered_by(&self, repo: &Repo, source: &str) -> crate::Result<bool> {
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
        Ok(meta
            .baseline_bytes
            .is_some_and(|bytes| bytes >= self.required_bytes)
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
    for job in pending()? {
        if job.logical == saved.capture.session_id
            && job.native == saved.capture.native_session_id
            && job.runtime == saved.capture.runtime
            && job.session()?.repo_dir()? == repo.root()
            && job.covered_by(repo, &request.source)?
        {
            job.verify_prefix(&job.transcript)?;
            job.remove()?;
        }
    }
    Ok(())
}

fn prefix(path: &Path, bytes: u64) -> crate::Result<String> {
    ensure!(
        bytes > 0 && bytes <= MAX_PREFIX_BYTES,
        "invalid archive prefix extent"
    );
    let mut text = String::new();
    fs::File::open(path)?
        .take(bytes)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 == bytes && text.ends_with('\n'),
        "completed archive prefix is incomplete"
    );
    Ok(text)
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
            job.verify_prefix(&job.transcript).unwrap();
            assert_eq!(
                crate::commands::commit::completed_native_boundary("codex", &text).unwrap(),
                Some(job.required_bytes)
            );
            let mut replaced = text.into_bytes();
            replaced[job.prefix_bytes as usize - 2] = b' ';
            fs::write(&job.transcript, replaced).unwrap();
            assert!(job.verify_prefix(&job.transcript).is_err());
            assert!(replay.record().is_err());
            assert_eq!(
                pending().unwrap(),
                recovered,
                "invalid replay must retain the original pending work"
            );
        });
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
