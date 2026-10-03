//! Observation retains collection authority without acquiring native input control.

use super::*;
use anyhow::{Context, ensure};

#[derive(Default)]
pub(super) struct Collector {
    cursor: usize,
    recorded: HashMap<String, String>,
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
        let offset = self.cursor % sessions.len();
        sessions.rotate_left(offset);
        self.cursor = self.cursor.wrapping_add(16);
        for logical in sessions.into_iter().take(16) {
            let Some(_work) = admission.enter() else {
                return;
            };
            let request = {
                let mut state = daemon.lock().await;
                state.reserve_observed_capture(&logical)
            };
            let Some(request) = request else { continue };
            let result = tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => None,
                result = tokio::time::timeout(std::time::Duration::from_secs(30),
                    collect(daemon, request, self.recorded.get(&logical).cloned())) => Some(result),
            };
            daemon.lock().await.opening_sessions.remove(&logical);
            let Some(result) = result else { return };
            match result {
                Ok(Ok(Some(digest))) => {
                    self.recorded.insert(logical, digest);
                }
                Ok(Ok(None)) => {}
                result => eprintln!(
                    "agitd: observed conversation archive remains pending ({logical}): {result:?}"
                ),
            }
        }
    }
}

async fn collect(
    daemon: &Arc<Mutex<Daemon>>,
    request: super::capture_binding::Request,
    recorded: Option<String>,
) -> crate::Result<Option<String>> {
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
                    && current.native_source == entry.native_source
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
) -> crate::Result<Option<String>> {
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
    let path = claim
        .resolve()
        .context("Observed native transcript is missing")?;
    let file = std::fs::File::open(&path)?;
    let size = file.metadata()?.len();
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
    let Some(boundary) = crate::commands::commit::completed_native_boundary(&entry.runtime, text)?
    else {
        return Ok(None);
    };
    let digest = format!("{:x}", Sha256::digest(&bytes[..boundary as usize]));
    if recorded == Some(digest.as_str()) {
        return Ok(None);
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
    Ok(Some(digest))
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
                && source == entry.native_source.as_ref()
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
                        && entry.native_source == info.native_source
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
            state.opening_sessions.clear();
            state
                .mirror
                .unbind(crate::rc::endpoint::WORKSPACE, "project");
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
        });
    }
}
