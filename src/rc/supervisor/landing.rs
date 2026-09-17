//! Repository preparation runs independently of native session input.
use super::*;

pub(super) struct LandingTask(pub tokio::task::JoinHandle<Landing>);

impl Drop for LandingTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) struct Landing {
    thread_id: String,
    lease: SettlementState,
    settlement: tokio::sync::watch::Receiver<SettlementState>,
    agit_session: Option<crate::rc::lineage::AgitSession>,
    archive_handoff: Option<crate::commands::commit::archive::RcHandoff>,
    landed_thread: Option<String>,
    runtime: String,
    session_id: String,
    cwd: PathBuf,
    exe: Option<PathBuf>,
}

impl Landing {
    fn capture(session: &Session, thread_id: &str, lease: SettlementState) -> Self {
        Self {
            thread_id: thread_id.into(),
            lease,
            settlement: session.settlement.clone(),
            agit_session: session.agit_session.clone(),
            archive_handoff: session.archive_handoff.clone(),
            landed_thread: None,
            runtime: session.info.runtime.clone(),
            session_id: session.info.session_id.clone(),
            cwd: session.cwd.clone(),
            exe: session.settlement_exe(),
        }
    }

    async fn run(&mut self) {
        let thread_id = self.thread_id.clone();
        let thread_id = thread_id.as_str();
        let lease = self.lease;
        let started = std::time::Instant::now();
        if !settlement_lease_is_current(&self.settlement, lease) {
            return;
        }
        // A previous success is not permanent authority: the slug can be
        // deleted and reused while the session stays alive. Clear the cached
        // proof before every network revalidation so failure cannot fall
        // through to commit/push.
        self.landed_thread = None;
        let expected_archive = self.archive_handoff.clone();
        let Some(agit_session) = self.agit_session.clone() else {
            // Unmanaged (no project bound) means there is nowhere to land, which is not a
            // failure.
            if expected_archive.is_none() {
                self.landed_thread = Some(thread_id.to_string());
            }
            return;
        };
        // `commands::rc::land_argv` builds the argv — it lives next to the clap definition, so
        // renaming a flag has exactly one site to change, and a test really parses this argv.
        // Built by hand here, one rename becomes a subprocess call that always fails and only
        // mutters about it in the log.
        let mut args = crate::commands::rc::land_argv(
            &agit_session.slug(),
            agit_session.agent_id(),
            agit_session.branch(),
            &self.runtime,
            thread_id,
            &self.cwd.to_string_lossy(),
        );
        if lease.local_owner {
            args.push("--local-owner".into());
        }
        let out = if let Some(exe) = self.exe.clone() {
            let mut command = tokio::process::Command::new(exe);
            command.args(&args).env(
                crate::hub::identity::EXPECTED_AGENT_ID_ENV,
                agit_session.agent_id(),
            );
            if lease.local_owner {
                command.env_remove(crate::hub::identity::EXPECTED_AGENT_ID_ENV);
            }
            command
                .env_remove(crate::commands::commit::archive::NATIVE_ENV)
                .env_remove(crate::commands::commit::archive::ROLE_ENV);
            if let Some(expected) = &expected_archive {
                let (Ok(native), Ok(role)) = (
                    serde_json::to_string(&expected.native),
                    serde_json::to_string(&expected.role),
                ) else {
                    return;
                };
                command
                    .env(crate::commands::commit::archive::NATIVE_ENV, native)
                    .env(crate::commands::commit::archive::ROLE_ENV, role);
            }
            guarded_output(&mut self.settlement, lease, command).await
        } else {
            None
        };
        match out {
            Some(o) if o.status.success() => {
                let handoffs: Vec<_> = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix(crate::commands::commit::archive::RC_PREFIX)
                    })
                    .map(serde_json::from_str::<crate::commands::commit::archive::RcHandoff>)
                    .collect();
                if handoffs.is_empty() {
                    if expected_archive.is_none() {
                        self.landed_thread = Some(thread_id.to_string());
                    } else {
                        tracing_note("RC landing lost its retained Archive classification");
                    }
                } else if handoffs.len() == 1 {
                    match handoffs.into_iter().next().unwrap() {
                        Ok(handoff)
                            if handoff.native.session_id == thread_id
                                && crate::adapter::normalize(&self.runtime).ok()
                                    == Some(handoff.native.runtime.as_str())
                                && handoff.role.slug == agit_session.slug()
                                && handoff.role.branch == agit_session.branch()
                                && expected_archive
                                    .as_ref()
                                    .is_none_or(|expected| expected == &handoff)
                                && handoff
                                    .role
                                    .validate(handoff.role.origin_head.len())
                                    .is_ok() =>
                        {
                            self.archive_handoff = Some(handoff);
                            self.landed_thread = Some(thread_id.to_string());
                        }
                        _ => tracing_note(
                            "archive landing returned a different native identity or generation",
                        ),
                    }
                } else {
                    tracing_note("archive landing returned multiple authority handoffs");
                }
            }
            Some(o) => tracing_note(&format!(
                "lineage landing failed for {agit_session}: {} (will retry next turn)",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            None => tracing_note(&format!(
                "lineage landing could not run for {agit_session} (will retry next turn)"
            )),
        }
        trace_phase(&self.session_id, "lineage.land", started);
    }
}

impl Session {
    /// Binding publishes native ownership immediately. Repository preparation has its own
    /// worker so Git and Hub latency cannot hold the harness command loop.
    pub(super) async fn bind_if_known(&mut self) {
        self.announce_binding().await;
        if self.publication.is_some() || self.landing.is_some() {
            return;
        }
        let Some(thread_id) = self.driver.runtime_thread_id() else {
            return;
        };
        if self.landed_thread.as_deref() == Some(thread_id.as_str()) {
            return;
        }
        let Some(lease) = settlement_lease(&self.settlement) else {
            return;
        };
        let mut landing = Landing::capture(self, &thread_id, lease);
        self.landing = Some(LandingTask(tokio::spawn(async move {
            landing.run().await;
            landing
        })));
    }

    pub(super) async fn finish_landing(&mut self) {
        let Some(mut task) = self.landing.take() else {
            return;
        };
        match (&mut task.0).await {
            Ok(landing) => {
                if self.driver.runtime_thread_id().as_deref() == Some(landing.thread_id.as_str())
                    && settlement_lease_is_current(&self.settlement, landing.lease)
                {
                    self.landed_thread = landing.landed_thread;
                    self.archive_handoff = landing.archive_handoff;
                }
            }
            Err(error) => tracing_note(&format!("repository preparation worker failed: {error}")),
        }
    }

    /// Settlement serializes with startup preparation and refreshes repository authority
    /// before committing. A finished background task alone never authorizes publication.
    pub(super) async fn land(&mut self, thread_id: &str, lease: SettlementState) {
        self.finish_landing().await;
        let mut landing = Landing::capture(self, thread_id, lease);
        landing.run().await;
        self.landed_thread = landing.landed_thread;
        self.archive_handoff = landing.archive_handoff;
    }
}
