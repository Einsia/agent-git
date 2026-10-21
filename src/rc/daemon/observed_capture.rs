//! Observation retains collection authority without acquiring native input control.

use super::*;
use anyhow::{Context, ensure};

#[derive(Default)]
pub(super) struct Collector {
    cursor: usize,
    recorded: HashMap<String, Recorded>,
    reported: HashMap<String, String>,
}

/// The completed boundary last recorded for a conversation and the carrier it was read from.
#[derive(Clone, Debug)]
struct Recorded {
    digest: String,
    carrier: Carrier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Carrier {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    file: (u64, u64),
}

impl Carrier {
    fn of(path: &std::path::Path, metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            path: normalized(path),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            file: (metadata.dev(), metadata.ino()),
        }
    }

    /// A carrier that is still the conversation's current one and is unchanged holds no new
    /// turn, so the whole transcript need not be read and hashed again. Codex restores a
    /// compressed rollout before it appends, so a carrier that went cold since it was read is
    /// unchanged too, and checking it never restores it.
    fn current_and_unchanged(&self, entry: &roster::Entry) -> bool {
        if current_carrier(entry).is_none_or(|current| normalized(&current) != self.path) {
            return false;
        }
        match std::fs::metadata(&self.path) {
            Ok(metadata) => Self::of(&self.path, &metadata) == *self,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entry.runtime == "codex" && crate::adapter::codex_rollout::is_cold(&self.path)
            }
            Err(_) => false,
        }
    }
}

fn normalized(path: &std::path::Path) -> PathBuf {
    match (
        path.parent().map(|parent| parent.canonicalize()),
        path.file_name(),
    ) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_owned(),
    }
}

/// The transcript the runtime currently writes, located without restoring cold storage.
fn current_carrier(entry: &roster::Entry) -> Option<PathBuf> {
    if entry.runtime != "codex" {
        return crate::adapter::get(&entry.runtime)
            .ok()?
            .resolve_present(&entry.thread_id, Some(std::path::Path::new(&entry.cwd)));
    }
    let thread = match &entry.native_source {
        Some(source) => {
            let registry = crate::rc::runtime_sources::Registry::open().ok()?;
            let context =
                crate::rc::runtime_context::RuntimeContext::resolve(&registry, &source.source_id)
                    .ok()?;
            crate::adapter::codex_index::thread_by_id_in(&context.source.home, &entry.thread_id)?
        }
        None => crate::adapter::codex_index::thread_by_id(&entry.thread_id)?,
    };
    Some(crate::adapter::codex_rollout::plain_path(
        &thread.rollout_path,
    ))
}

impl Collector {
    pub(super) async fn reconcile(
        &mut self,
        daemon: &Arc<Mutex<Daemon>>,
        admission: &crate::rc::admission::Admission,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) {
        let mut sessions: Vec<_> = daemon
            .lock()
            .await
            .roster
            .observed
            .iter()
            .cloned()
            .collect();
        if sessions.is_empty() {
            return;
        }
        self.recorded
            .retain(|logical, _| sessions.contains(logical));
        self.reported
            .retain(|logical, _| sessions.contains(logical));
        let offset = self.cursor % sessions.len();
        sessions.rotate_left(offset);
        self.cursor = self.cursor.wrapping_add(16);
        for logical in sessions.into_iter().take(16) {
            let Some(_work) = admission.enter() else {
                return;
            };
            if let Some(recorded) = self.recorded.get(&logical).cloned() {
                let entry = daemon.lock().await.roster.get(&logical).cloned();
                if let Some(entry) = entry
                    && tokio::task::spawn_blocking(move || {
                        recorded.carrier.current_and_unchanged(&entry)
                    })
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
            }
            let request = {
                let mut state = daemon.lock().await;
                state.reserve_observed_capture(&logical)
            };
            let Some(request) = request else { continue };
            let result = tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => None,
                result = tokio::time::timeout(std::time::Duration::from_secs(30),
                    collect(daemon, request, self.recorded.get(&logical).map(|recorded| recorded.digest.clone()))) => Some(result),
            };
            daemon.lock().await.opening_sessions.remove(&logical);
            let Some(result) = result else { return };
            match result {
                Ok(Ok(recorded)) => {
                    self.reported.remove(&logical);
                    if let Some(recorded) = recorded {
                        self.recorded.insert(logical, recorded);
                    }
                }
                result => {
                    // The same failure recurs every cycle until its cause changes; one line each.
                    let failure = format!("{result:?}");
                    if self.reported.get(&logical) != Some(&failure) {
                        eprintln!(
                            "agitd: observed conversation archive remains pending ({logical}): {failure}"
                        );
                        self.reported.insert(logical, failure);
                    }
                }
            }
        }
    }
}

async fn collect(
    daemon: &Arc<Mutex<Daemon>>,
    request: super::capture_binding::Request,
    recorded: Option<String>,
) -> crate::Result<Option<Recorded>> {
    let logical = request.logical.clone();
    let mut entry = request
        .prior_entry
        .clone()
        .context("Observed session binding is missing")?;
    let project = request
        .project
        .clone()
        .context("Observed project is missing")?;
    let lineage = request
        .resolve()
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?
        .context("Observed capture is unavailable")?;
    {
        let mut state = daemon.lock().await;
        ensure!(
            state.opts.local_owner
                && state.settlement.borrow().local_owner
                && state
                    .mirror
                    .project_path(&entry.workspace_id, &project.0)
                    .as_ref()
                    == Some(&project.1),
            "Observed project binding changed during capture"
        );
        ensure!(
            state.roster.get(&logical).is_some_and(|current| {
                current.thread_id == entry.thread_id
                    && crate::protocol::NativeSourceRef::same_store(
                        current.native_source.as_ref(),
                        entry.native_source.as_ref(),
                    )
                    && current.runtime == entry.runtime
                    && current.cwd == entry.cwd
            }),
            "Observed native binding changed during capture"
        );
        entry.agit_session = Some(lineage.to_string());
        entry.expected_agent_id = Some(lineage.agent_id().into());
        let changed = state.roster.get(&logical).is_none_or(|current| {
            current.agit_session != entry.agit_session
                || current.expected_agent_id != entry.expected_agent_id
        }) || state.roster.captures.get(&logical) != lineage.capture.as_ref();
        if changed {
            let mut roster = state.roster.clone();
            if let Some(kind) = &lineage.capture {
                roster.captures.insert(logical.clone(), kind.clone());
            }
            roster.record(&logical, entry.clone())?;
            roster.save()?;
            state.roster = roster;
        }
    }
    tokio::task::spawn_blocking(move || {
        capture_boundary(&logical, &entry, &lineage, recorded.as_deref())
    })
    .await?
}

fn capture_boundary(
    logical: &str,
    entry: &roster::Entry,
    lineage: &crate::rc::lineage::AgitSession,
    recorded: Option<&str>,
) -> crate::Result<Option<Recorded>> {
    use crate::domain::{link, store::Store};
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let key = entry
        .native_source
        .as_ref()
        .map(|source| source.session_ref(&entry.thread_id))
        .unwrap_or_else(|| entry.thread_id.clone());
    let store = Store::open()?.context("Observed capture store is missing")?;
    let claim = link::get_checked(&store, &entry.runtime, &key)?
        .context("Observed capture claim is missing")?;
    #[cfg(unix)]
    if entry.runtime == "claude-code"
        && crate::rc::native_claude::Client::unwritten_transcript(
            &entry.thread_id,
            std::path::Path::new(&entry.cwd),
        )
        .is_some()
    {
        return Ok(None);
    }
    let path = claim
        .resolve()
        .context("Observed native transcript is missing")?;
    let file = std::fs::File::open(&path)?;
    let metadata = file.metadata()?;
    let carrier = Carrier::of(&path, &metadata);
    let size = metadata.len();
    ensure!(
        size <= crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64,
        "Observed native transcript exceeds its capture budget"
    );
    let mut bytes = vec![];
    file.take(size).read_to_end(&mut bytes)?;
    let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes[..=end])?;
    let boundary = crate::commands::commit::completed_native_boundary(&entry.runtime, text)?;
    #[cfg(unix)]
    let boundary = if entry.runtime == "claude-code" {
        let completed = crate::rc::native_claude::completion::boundaries(
            &entry.thread_id,
            &path,
            text.as_bytes(),
        )?;
        boundary.max(completed.last().map(|end| *end as u64))
    } else {
        boundary
    };
    let Some(boundary) = boundary else {
        return Ok(None);
    };
    let digest = format!("{:x}", Sha256::digest(&bytes[..boundary as usize]));
    if recorded == Some(digest.as_str()) {
        return Ok(Some(Recorded { digest, carrier }));
    }
    let job = crate::rc::archive_jobs::Job::capture(
        logical,
        &entry.thread_id,
        &entry.runtime,
        entry.native_source.clone(),
        std::path::Path::new(&entry.cwd),
        lineage,
        &format!("observed-{digest}"),
        &path,
        boundary,
        None,
    )?;
    job.record()?;
    Ok(Some(Recorded { digest, carrier }))
}

impl Daemon {
    fn reserve_observed_capture(
        &mut self,
        logical: &str,
    ) -> Option<super::capture_binding::Request> {
        if !self.opts.local_owner || !self.settlement.borrow().local_owner {
            return None;
        }
        let entry = self.roster.get(logical)?.clone();
        let project_id = entry.project_id.as_ref()?;
        let project = self.mirror.project_path(&entry.workspace_id, project_id)?;
        let cwd = PathBuf::from(&entry.cwd);
        if !cwd.starts_with(&project) {
            return None;
        }
        let conflicts = |runtime: &str,
                         source: Option<&crate::protocol::NativeSourceRef>,
                         native: Option<&str>| {
            runtime == entry.runtime
                && crate::protocol::NativeSourceRef::same_store(
                    source,
                    entry.native_source.as_ref(),
                )
                && native == Some(entry.thread_id.as_str())
        };
        if self.sessions.contains_key(logical)
            || self.opening_sessions.contains_key(logical)
            || self.archive_recovering.contains_key(logical)
            || self.sessions.values().any(|live| {
                conflicts(
                    &live.info.runtime,
                    live.info.native_source.as_ref(),
                    live.runtime_thread_id.as_deref(),
                )
            })
            || self.opening_sessions.values().any(|opening| {
                conflicts(
                    &opening.runtime,
                    opening.native_source.as_ref(),
                    opening.native_id.as_deref(),
                )
            })
            || self
                .archive_recovering
                .values()
                .any(|job| conflicts(&job.runtime, job.native_source.as_ref(), Some(&job.native)))
        {
            return None;
        }
        self.opening_sessions.insert(
            logical.into(),
            LaunchReservation {
                generation: 0,
                runtime: entry.runtime.clone(),
                native_source: entry.native_source.clone(),
                native_id: Some(entry.thread_id.clone()),
            },
        );
        Some(super::capture_binding::Request {
            logical: logical.into(),
            runtime: entry.runtime.clone(),
            native: entry.thread_id.clone(),
            native_source: entry.native_source.clone(),
            cwd,
            project: Some((project_id.clone(), project)),
            prior_entry: Some(entry),
            prior_capture: self.roster.captures.get(logical).cloned(),
            wire: None,
            authority: crate::rc::authority::Guard::default(),
            settlement: self.settlement.subscribe(),
        })
    }

    pub(super) fn retain_observed_capture(
        &mut self,
        info: &SessionInfo,
        cwd: &std::path::Path,
    ) -> crate::Result<Option<SessionInfo>> {
        if !self.opts.local_owner || info.workspace_id != crate::rc::endpoint::WORKSPACE {
            return Ok(None);
        }
        let Some(project) = &info.project_id else {
            return Ok(None);
        };
        let root = self
            .mirror
            .project_path(&info.workspace_id, project)
            .context("Observed project is no longer bound")?;
        ensure!(
            cwd.starts_with(root),
            "Observed session is outside its project"
        );
        let native = info
            .runtime_session_id
            .as_deref()
            .context("Observed native identity is missing")?;
        let logical = self
            .roster
            .logical_for_thread_in(
                &info.runtime,
                info.native_source
                    .as_ref()
                    .map(|source| source.source_id.as_str()),
                native,
                &info.workspace_id,
            )
            .unwrap_or_else(crate::domain::meta::mint_session_id);
        let mut roster = self.roster.clone();
        let mut entry = match roster.get(&logical) {
            Some(entry) => {
                ensure!(
                    entry.thread_id == native
                        && crate::protocol::NativeSourceRef::same_store(
                            entry.native_source.as_ref(),
                            info.native_source.as_ref(),
                        )
                        && entry.cwd == cwd.to_string_lossy(),
                    "Observed session binding changed"
                );
                entry.clone()
            }
            None => roster::Entry {
                native_source: info.native_source.clone(),
                runtime: info.runtime.clone(),
                thread_id: native.into(),
                cwd: cwd.to_string_lossy().into(),
                workspace_id: info.workspace_id.clone(),
                project_id: Some(project.clone()),
                agit_session: None,
                expected_agent_id: None,
                permission_mode: info.permission_mode,
                guard_attempts: Default::default(),
                prior_threads: vec![],
                ever_dangerous: info.dangerous,
            },
        };
        let changed_danger = info.dangerous && !entry.ever_dangerous;
        entry.ever_dangerous |= info.dangerous;
        let mut archive = info.clone();
        archive.session_id = logical.clone();
        if let Some((session, identity)) = entry
            .agit_session
            .as_deref()
            .zip(entry.expected_agent_id.as_deref())
        {
            let lineage = crate::rc::lineage::AgitSession::parse(session, identity)?;
            archive.agent = Some(lineage.slug());
            archive.branch = Some(lineage.branch().into());
        }
        roster.record(&logical, entry)?;
        if roster.observed.insert(logical) || changed_danger {
            roster.save()?;
            self.roster = roster;
        }
        Ok(Some(archive))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collector skips a conversation only while the carrier it read is still the thread's
    /// current carrier and unchanged. Comparing only the old file would miss a Codex segment
    /// rollover, and treating a missing file as changed would restore every cold rollout.
    #[test]
    fn unchanged_carrier_skips_reads_until_it_grows_rolls_over_or_is_restored() {
        let _environment = crate::infra::config::env_lock();
        let previous = std::env::var_os("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        // SAFETY: this test holds the process-wide configuration lock.
        unsafe { std::env::set_var("CODEX_HOME", home.path()) };
        let restore_home = scopeguard(move || match previous {
            // SAFETY: the configuration lock is still held.
            Some(previous) => unsafe { std::env::set_var("CODEX_HOME", previous) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        });
        let thread = "70707070-7070-4070-8070-707070707070";
        let directory = home.path().join("sessions/2026/10/01");
        std::fs::create_dir_all(&directory).unwrap();
        let root = directory.join(format!("rollout-2026-10-01T00-00-00-{thread}.jsonl"));
        std::fs::write(&root, "{}\n").unwrap();
        let database = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        database
            .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER)")
            .unwrap();
        database
            .execute(
                "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
                [thread, root.to_str().unwrap()],
            )
            .unwrap();
        let entry: roster::Entry = serde_json::from_value(serde_json::json!({
            "runtime": "codex", "thread_id": thread, "cwd": home.path(), "workspace_id": "local",
        }))
        .unwrap();
        let read = |path: &std::path::Path| Carrier::of(path, &std::fs::metadata(path).unwrap());

        let carrier = read(&root);
        assert!(carrier.current_and_unchanged(&entry));
        std::fs::write(&root, "{}\n{}\n").unwrap();
        assert!(
            !carrier.current_and_unchanged(&entry),
            "an append is new content"
        );

        let carrier = read(&root);
        std::fs::write(
            root.with_extension("jsonl.zst"),
            ruzstd::encoding::compress_to_vec(
                &b"{}\n{}\n"[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        std::fs::remove_file(&root).unwrap();
        assert!(
            carrier.current_and_unchanged(&entry),
            "a cold carrier is unchanged"
        );
        assert!(
            !root.exists(),
            "checking a cold carrier must not restore it"
        );

        let segment = directory.join(format!(
            "rollout-2026-10-01T00-00-00-{thread}_80808080-8080-4080-8080-808080808080.jsonl"
        ));
        std::fs::write(&segment, "{}\n").unwrap();
        database
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                [segment.to_str().unwrap(), thread],
            )
            .unwrap();
        assert!(
            !carrier.current_and_unchanged(&entry),
            "a rollover moves the current carrier"
        );
        drop(restore_home);
    }

    fn scopeguard(run: impl FnOnce()) -> impl Drop {
        struct Guard<F: FnOnce()>(Option<F>);
        impl<F: FnOnce()> Drop for Guard<F> {
            fn drop(&mut self) {
                if let Some(run) = self.0.take() {
                    run();
                }
            }
        }
        Guard(Some(run))
    }

    #[test]
    fn observed_identity_survives_viewers_and_respects_native_writer_and_project_fences() {
        if crate::rc::in_isolated_test(
            "rc::daemon::observed_capture::tests::observed_identity_survives_viewers_and_respects_native_writer_and_project_fences",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let project = root.path().canonicalize().unwrap();
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.opts.local_owner = true;
            state
                .settlement
                .send_modify(|authority| authority.local_owner = true);
            state
                .mirror
                .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
                .unwrap();
            let (tx, _rx) = mpsc::channel(1);
            let mut native = super::super::tests::rpc_test_live(
                "watch",
                1,
                tx,
                crate::protocol::PermissionMode::Default,
            );
            native.info.workspace_id = crate::rc::endpoint::WORKSPACE.into();
            native.info.project_id = Some("project".into());
            native.info.runtime_session_id = Some("native".into());
            native.runtime_thread_id = Some("native".into());
            let first = state
                .retain_observed_capture(&native.info, &project)
                .unwrap()
                .unwrap();
            assert_ne!(first.session_id, native.info.session_id);
            assert!(state.sessions.is_empty());
            assert!(state.watches.is_empty());
            native.info.dangerous = true;
            let repeated = state
                .retain_observed_capture(&native.info, &project)
                .unwrap()
                .unwrap();
            assert_eq!(first.session_id, repeated.session_id);
            let saved = Roster::try_load().unwrap();
            assert!(saved.observed.contains(&first.session_id));
            assert!(saved.get(&first.session_id).unwrap().ever_dangerous);
            state.roster = saved;
            state.sessions.insert("other-alias".into(), native);
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
            state.sessions.clear();
            assert!(state.reserve_observed_capture(&first.session_id).is_some());
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
            let failure = state
                .require_launch_slot(
                    &first,
                    &LaunchSpec {
                        cwd: project.clone(),
                        resume_from: Some("native".into()),
                        agit_session: None,
                        model: None,
                        dangerous: false,
                        permission_mode: None,
                    },
                )
                .err()
                .unwrap();
            assert_eq!(failure.error.data.as_ref().unwrap()["retryable"], true);
            assert_eq!(failure.error.data.as_ref().unwrap()["outcome"], "not_sent");
            state.opening_sessions.clear();
            state
                .mirror
                .unbind(crate::rc::endpoint::WORKSPACE, "project");
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
        });
    }
}
