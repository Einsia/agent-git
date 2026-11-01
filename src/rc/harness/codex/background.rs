//! Private control expires by human intent; native work and its process remain alive.

use super::*;
use anyhow::{Context, ensure};
use std::time::Duration;
use tokio::time::Instant;

pub const FALLBACK_SILENCE: Duration = Duration::from_secs(15 * 60);
pub(super) const UNLOAD_DELAY_SECONDS: u64 = 1;

#[derive(Default)]
pub(super) enum Control {
    #[default]
    Unacquired,
    Shared,
    Foreground {
        last_instruction: Instant,
    },
    Background,
    Uncertain,
    Unsupported,
}

impl CodexDriver {
    pub(super) fn control_acquired(&mut self) {
        self.control = if self.proc.shared() {
            Control::Shared
        } else {
            Control::Foreground {
                last_instruction: Instant::now(),
            }
        };
    }

    /// Only accepted human instructions call this; reads and native events do not renew control.
    pub fn note_user_instruction(&mut self) {
        if self.proc.shared() {
            self.control = Control::Shared;
        } else if let Control::Foreground { last_instruction } = &mut self.control {
            *last_instruction = Instant::now();
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_test_fallback_control(&mut self) {
        self.control = Control::Foreground {
            last_instruction: Instant::now() - FALLBACK_SILENCE,
        };
    }

    pub fn take_control_event(&mut self) -> Option<HarnessEvent> {
        self.control_events.pop_front()
    }

    pub fn background_control(&self) -> bool {
        !self.proc.shared() && matches!(self.control, Control::Background | Control::Uncertain)
    }

    pub fn control_snapshot(&self) -> Value {
        let mode = if self.proc.shared() {
            "shared"
        } else {
            match self.control {
                Control::Unacquired => "opening",
                Control::Shared => "unavailable",
                Control::Foreground { .. } => "fallback",
                Control::Background => "background",
                Control::Uncertain => "unknown",
                Control::Unsupported => "fallback_release_unavailable",
            }
        };
        json!({"mode":mode,"silence_seconds":FALLBACK_SILENCE.as_secs()})
    }

    pub async fn background_after_silence(
        &mut self,
        observation_ready: bool,
    ) -> Option<HarnessEvent> {
        if self.proc.shared() {
            self.control = Control::Shared;
            return None;
        }
        let Control::Foreground { last_instruction } = self.control else {
            return None;
        };
        if !observation_ready
            || last_instruction.elapsed() < FALLBACK_SILENCE
            || self.pending_turn_start.is_some()
            || self.handshake_request.is_some()
        {
            return None;
        }
        let thread = self.thread_id.clone()?;
        // Configuration readback proves this private executor understands bounded idle unloading.
        let config = self
            .command_request("config/read", json!({"includeLayers":false}))
            .await;
        if !config.as_ref().is_ok_and(|value| {
            value["config"]["thread_unload_delay_secs"].as_u64() == Some(UNLOAD_DELAY_SECONDS)
        }) {
            self.control = Control::Unsupported;
            return Some(HarnessEvent::Progress { text: "This Codex version cannot confirm bounded background release. The task remains running; use a shared Codex server or upgrade the native CLI.".into() });
        }
        self.control = Control::Uncertain;
        let response = self
            .command_request("thread/unsubscribe", json!({"threadId":thread}))
            .await;
        match response {
            Ok(value) if matches!(value["status"].as_str(), Some("unsubscribed" | "notSubscribed" | "notLoaded")) => {
                self.control = Control::Background;
                self.pending_mode = None;
                self.pending_model = None;
                Some(HarnessEvent::Progress { text: "Remote control moved to the background after 15 minutes without instructions. The task continues and its saved output remains visible. Your next instruction will reconnect control.".into() })
            }
            Err(error) if error.is::<commands::NativeCommandRefusal>() => {
                self.control = Control::Unsupported;
                Some(HarnessEvent::Progress { text: "Codex refused background release. The task remains running and remote control is retained.".into() })
            }
            _ => Some(HarnessEvent::Progress { text: "Codex did not confirm background release. The task has not been stopped. Reading continues; the next instruction will verify control before sending input.".into() }),
        }
    }

    /// Reading an unloaded thread does not load it or reclaim its native writer.
    pub async fn background_observation(&mut self) -> crate::Result<Value> {
        ensure!(
            self.background_control(),
            "this conversation is not in fallback background mode"
        );
        let thread = self
            .thread_id
            .clone()
            .context("native thread identity is unavailable")?;
        let result = self
            .command_request(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            )
            .await?;
        self.validate_background_thread(&result)?;
        let page = self.command_request("thread/turns/list", json!({"threadId":thread,"limit":64,"itemsView":"notLoaded","sortDirection":"desc"})).await?;
        self.reconcile_background_turns(&page).await?;
        self.observe_background_settings();
        Ok(result["thread"]["status"].clone())
    }

    pub(super) fn observe_background_settings(&mut self) {
        if !self.background_control() {
            return;
        }
        let settings = self
            .transcript_path()
            .zip(self.thread_id.as_deref())
            .map(|(path, thread)| match &self.source {
                Some(source) => {
                    crate::rc::native_settings::read_codex_in(source.home(), &path, thread)
                }
                None => crate::rc::native_settings::read_codex(&path, thread),
            })
            .unwrap_or_default();
        let model = settings.model();
        let observed_model = model["model"].as_str().map(str::to_owned);
        let effort = model["effort"].as_str().map(str::to_owned);
        let previous_mode = (!self.native_mode_unknown).then_some(self.mode);
        let changed = previous_mode != settings.permission_mode
            || self.model != observed_model
            || self.effort != effort
            || self.native_model_unknown != observed_model.is_none();
        self.native_mode_unknown = settings.permission_mode.is_none();
        if let Some(mode) = settings.permission_mode {
            self.mode = mode;
        }
        self.native_model_unknown = observed_model.is_none();
        self.model = observed_model;
        self.effort = effort;
        self.pending_mode = None;
        self.pending_model = None;
        if changed {
            self.control_events
                .push_back(HarnessEvent::SettingsUpdated {
                    mode: settings.permission_mode,
                    applied: PermissionApply::Immediate,
                });
        }
    }

    fn validate_background_thread(&self, result: &Value) -> crate::Result<()> {
        ensure!(
            result["thread"]["id"].as_str() == self.thread_id.as_deref(),
            "native background read returned another conversation"
        );
        let cwd = result["thread"]["cwd"]
            .as_str()
            .context("native background read omitted its directory")?;
        ensure!(
            std::path::Path::new(cwd) == self.cwd,
            "native conversation directory changed while backgrounded"
        );
        Ok(())
    }

    async fn reconcile_background_turns(&mut self, page: &Value) -> crate::Result<()> {
        let rows = page["data"]
            .as_array()
            .context("native background turn metadata is unavailable")?;
        ensure!(
            self.control_events.len() <= 62,
            "background observation is awaiting delivery"
        );
        if let Some(current) = self.current_turn.as_deref()
            && let Some(turn) = rows.iter().find(|row| row["id"].as_str() == Some(current))
            && matches!(turn["status"].as_str(), Some("completed" | "interrupted" | "failed"))
            && let Some(event) = self.classify(json!({"method":"turn/completed","params":{"threadId":self.thread_id,"turn":turn}})).await {
                self.control_events.push_back(event);
            }
        if self.current_turn.is_none()
            && let Some(turn) = rows.first().filter(|turn| turn["status"] == "inProgress")
            && let Some(event) = self.classify(json!({"method":"turn/started","params":{"threadId":self.thread_id,"turn":turn}})).await {
                self.control_events.push_back(event);
            }
        Ok(())
    }

    /// Reattach to the same executor and thread before a human instruction can write.
    pub async fn resume_background(&mut self) -> crate::Result<()> {
        if self.proc.shared() {
            self.control = Control::Shared;
            return Ok(());
        }
        if !self.background_control() {
            self.note_user_instruction();
            return Ok(());
        }
        let thread = self
            .thread_id
            .clone()
            .context("native thread identity is unavailable")?;
        self.background_observation().await?;
        self.control = Control::Uncertain;
        let response = self.command_request("thread/resume", json!({"threadId":thread,"excludeTurns":true,"initialTurnsPage":{"limit":64,"itemsView":"notLoaded","sortDirection":"desc"}})).await?;
        self.validate_background_thread(&response)?;
        let active = opening::shared_active_turn(&response)?;
        if let Some(page) = response.get("initialTurnsPage") {
            self.reconcile_background_turns(page).await?;
        }
        ensure!(
            self.current_turn == active,
            "native activity changed while reconnecting; wait for its lifecycle before sending input"
        );
        let settings = crate::rc::native_settings::native_reply(&response);
        self.native_mode_unknown = settings.permission_mode.is_none();
        if let Some(mode) = settings.permission_mode {
            self.mode = mode;
        }
        let model = settings.model();
        self.model = model["model"].as_str().map(str::to_owned);
        self.effort = model["effort"].as_str().map(str::to_owned);
        self.native_model_unknown = self.model.is_none();
        self.pending_mode = None;
        self.pending_model = None;
        self.control_events
            .push_back(HarnessEvent::SettingsUpdated {
                mode: settings.permission_mode,
                applied: PermissionApply::Immediate,
            });
        self.control_acquired();
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn armed(responses: &[Value]) -> CodexDriver {
        let mut driver = CodexDriver::test_responder(Some("thread"), responses);
        driver.control_acquired();
        driver
    }

    #[tokio::test]
    async fn background_settings_follow_the_selected_home_without_reacquiring_control() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("rollout.jsonl");
        std::fs::write(&path, "").unwrap();
        let db = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, model TEXT, reasoning_effort TEXT, approval_mode TEXT, sandbox_policy TEXT)").unwrap();
        db.execute("INSERT INTO threads VALUES ('thread',?1,'/',NULL,NULL,0,0,'external-model','high','never','{\"type\":\"workspace-write\"}')", [path.canonicalize().unwrap().to_str().unwrap()]).unwrap();
        let mut driver = armed(&[]);
        driver.source = Some(
            crate::rc::native_codex::Source::new(
                home.path(),
                std::path::Path::new("/bin/cat"),
                None,
            )
            .unwrap(),
        );
        driver.control = Control::Background;
        driver.observe_background_settings();
        assert_eq!(driver.model.as_deref(), Some("external-model"));
        assert_eq!(driver.mode, PermissionMode::Auto);
        assert!(matches!(
            driver.take_control_event(),
            Some(HarnessEvent::SettingsUpdated {
                mode: Some(PermissionMode::Auto),
                ..
            })
        ));
        driver.observe_background_settings();
        assert!(driver.take_control_event().is_none());
        db.execute("DELETE FROM threads", []).unwrap();
        driver.observe_background_settings();
        assert!(driver.native_mode_unknown && driver.native_model_unknown);
        assert!(driver.model.is_none());
        assert!(matches!(
            driver.take_control_event(),
            Some(HarnessEvent::SettingsUpdated { mode: None, .. })
        ));
        assert_eq!(driver.control_snapshot()["mode"], "background");
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn only_acquired_private_intent_expires_and_release_preserves_native_work() {
        let mut driver = armed(&[
            json!({"id":1,"result":{"config":{"thread_unload_delay_secs":1}}}),
            json!({"method":"item/started","params":{"threadId":"thread","item":{"id":"ongoing","type":"agentMessage"}}}),
            json!({"id":2,"result":{"status":"unsubscribed"}}),
        ]);
        driver.current_turn = Some("active".into());
        driver.control = Control::Foreground {
            last_instruction: Instant::now() - FALLBACK_SILENCE,
        };
        driver.note_user_instruction();
        assert!(driver.background_after_silence(true).await.is_none());
        driver.control = Control::Foreground {
            last_instruction: Instant::now() - FALLBACK_SILENCE,
        };
        assert!(driver.background_after_silence(false).await.is_none());
        assert!(matches!(
            driver.background_after_silence(true).await,
            Some(HarnessEvent::Progress { .. })
        ));
        assert_eq!(driver.control_snapshot()["mode"], "background");
        assert_eq!(driver.current_turn.as_deref(), Some("active"));
        assert!(
            matches!(driver.next_event().await, Some(HarnessEvent::ItemStarted { item_id, .. }) if item_id == "ongoing")
        );
        assert!(driver.background_after_silence(true).await.is_none());
        driver.control = Control::Shared;
        assert!(driver.background_after_silence(true).await.is_none());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_unload_or_uncertain_unsubscribe_never_reports_released() {
        for (responses, expected) in [
            (
                vec![json!({"id":1,"result":{"config":{}}})],
                "fallback_release_unavailable",
            ),
            (
                vec![
                    json!({"id":1,"result":{"config":{"thread_unload_delay_secs":1}}}),
                    json!({"id":2,"result":{"status":"invalid"}}),
                ],
                "unknown",
            ),
        ] {
            let mut driver = armed(&responses);
            driver.control = Control::Foreground {
                last_instruction: Instant::now() - FALLBACK_SILENCE,
            };
            assert!(driver.background_after_silence(true).await.is_some());
            assert_eq!(driver.control_snapshot()["mode"], expected);
            assert!(driver.background_after_silence(true).await.is_none());
            driver.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn background_reads_complete_the_exact_turn_and_resume_uses_current_settings() {
        let mut driver = armed(&[
            json!({"id":1,"result":{"thread":{"id":"thread","cwd":"/","status":{"type":"idle"}}}}),
            json!({"id":2,"result":{"data":[{"id":"active","status":"completed"}]}}),
            json!({"id":3,"result":{"thread":{"id":"thread","cwd":"/","status":{"type":"idle"}},"model":"external-model","reasoningEffort":"high","approvalPolicy":"never","sandbox":{"type":"readOnly"},"initialTurnsPage":{"data":[{"id":"active","status":"completed"}]}}}),
        ]);
        driver.current_turn = Some("active".into());
        driver.pending_model = Some(("stale-model".into(), None));
        driver.control = Control::Background;
        driver.resume_background().await.unwrap();
        assert_eq!(driver.control_snapshot()["mode"], "fallback");
        assert!(
            matches!(driver.take_control_event(), Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "active")
        );
        assert!(matches!(
            driver.take_control_event(),
            Some(HarnessEvent::SettingsUpdated { .. })
        ));
        assert_eq!(driver.model.as_deref(), Some("external-model"));
        assert!(driver.pending_model.is_none());
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_reacquisition_keeps_observed_completion_and_blocks_new_input() {
        let mut driver = armed(&[
            json!({"id":1,"result":{"thread":{"id":"thread","cwd":"/","status":{"type":"idle"}}}}),
            json!({"id":2,"result":{"data":[{"id":"active","status":"completed"}]}}),
            json!({"id":3,"error":{"message":"thread already has an active writer"}}),
        ]);
        driver.current_turn = Some("active".into());
        driver.control = Control::Background;
        assert!(driver.resume_background().await.is_err());
        assert!(driver.background_control());
        assert!(
            matches!(driver.take_control_event(), Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "active")
        );
        driver.shutdown().await.unwrap();
    }
    #[tokio::test]
    #[ignore = "requires an isolated Codex home, executable and paused localhost provider"]
    async fn real_private_fallback_releases_observes_and_reacquires_without_stopping_work() {
        let home = PathBuf::from(std::env::var("AGIT_TEST_BACKGROUND_HOME").unwrap());
        let executable = PathBuf::from(std::env::var("AGIT_TEST_BACKGROUND_EXECUTABLE").unwrap());
        let project = home.join("project");
        let source = crate::rc::native_codex::Source::new(&home, &executable, None).unwrap();
        let spec = LaunchSpec {
            cwd: project.clone(),
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: Some(PermissionMode::Default),
            attach_only: false,
        };
        let mut driver = CodexDriver::launch_shared(spec.clone(), &source)
            .await
            .unwrap();
        assert!(!driver.proc.shared());
        driver.confirm_opening().await.unwrap();
        let native = driver.runtime_thread_id().unwrap().to_owned();
        assert!(matches!(
            driver
                .start_turn("Return the isolated fixture answer.", true, None)
                .await,
            TurnStartDispatch::Awaiting
        ));
        loop {
            match tokio::time::timeout(Duration::from_secs(15), driver.next_event())
                .await
                .unwrap()
                .unwrap()
            {
                HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted { .. }) => break,
                HarnessEvent::TurnStartResolved(other) => {
                    panic!("native turn was not accepted: {other:?}")
                }
                _ => {}
            }
        }
        driver.expire_test_fallback_control();
        assert!(driver.background_after_silence(true).await.is_some());
        assert_eq!(driver.control_snapshot()["mode"], "background");
        assert!(driver.has_active_turn());
        let resume = LaunchSpec {
            resume_from: Some(native),
            ..spec
        };
        let mut competing = CodexDriver::launch_private(resume.clone(), Some(&source))
            .await
            .unwrap();
        assert!(competing.confirm_opening().await.is_err());
        competing.shutdown().await.unwrap();
        std::fs::write(home.join("continue-fixture"), "continue").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut completed = false;
        loop {
            driver.background_observation().await.unwrap();
            while let Some(event) = driver.take_control_event() {
                completed |= matches!(event, HarnessEvent::TurnCompleted { .. });
            }
            if !driver.has_active_turn() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "background task did not complete"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(completed, "background completion must reach the supervisor");
        let transcript = driver.transcript_path().unwrap();
        assert!(
            std::fs::read_to_string(transcript)
                .unwrap()
                .contains("isolated fixture answer")
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let mut other = CodexDriver::launch_private(resume, Some(&source))
            .await
            .unwrap();
        other.confirm_opening().await.unwrap();
        assert!(driver.resume_background().await.is_err());
        assert!(driver.background_control());
        other.expire_test_fallback_control();
        assert!(other.background_after_silence(true).await.is_some());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        driver.resume_background().await.unwrap();
        assert_eq!(driver.control_snapshot()["mode"], "fallback");
        assert!(!driver.has_active_turn());
        other.shutdown().await.unwrap();
        driver.shutdown().await.unwrap();
    }
}
