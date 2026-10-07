//! A shared controller attaches to the native writer without starting or terminating it.

use super::{
    ApprovalOutcome, HarnessEvent, InterruptDispatch, InterruptOutcome, LaunchSpec,
    PermissionModeChangeError, PermissionModeChangeResult, SteerDispatch, TurnGuardAttempt,
    TurnOutcome, TurnStartDispatch, TurnStartOutcome, models, proc::LaunchError,
};
use crate::{
    protocol::{
        ApprovalKind, ApprovalRequest, ApprovalResponse, ApprovalScope, Delivery, ItemKind,
        PermissionMode,
    },
    rc::native_claude::{Client, Snapshot, Update},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
};

struct PendingPrompt {
    id: String,
    steering: bool,
}

pub struct ClaudeNativeDriver {
    client: Option<Client>,
    snapshot: Snapshot,
    transcript: PathBuf,
    events: VecDeque<HarnessEvent>,
    approvals: HashSet<String>,
    retired_approvals: HashSet<String>,
    pending_prompt: Option<PendingPrompt>,
    pending_model: Option<String>,
    pending_interrupt: Option<(String, String)>,
    attaching_compaction: Option<String>,
}

impl ClaudeNativeDriver {
    pub fn attach(spec: LaunchSpec) -> Result<Self, LaunchError> {
        (|| -> crate::Result<Self> {
            let session = spec
                .resume_from
                .context("native attachment requires a session")?;
            let client = Client::attach(&session, &spec.cwd)?;
            let snapshot = client.snapshot()?;
            ensure!(
                spec.model
                    .as_ref()
                    .is_none_or(|model| Some(model) == snapshot.model.as_ref()),
                "native attachment cannot change model defaults"
            );
            let transcript = snapshot.registration.transcript_target()?;
            let mut driver = Self {
                client: Some(client),
                snapshot: snapshot.clone(),
                transcript,
                events: VecDeque::new(),
                approvals: HashSet::new(),
                retired_approvals: HashSet::new(),
                pending_prompt: None,
                pending_model: None,
                pending_interrupt: None,
                attaching_compaction: None,
            };
            driver.events.push_back(HarnessEvent::Ready {
                runtime_thread_id: session,
                transcript_path: Some(driver.transcript.clone()),
            });
            if let Some(turn) = &snapshot.turn {
                driver.events.push_back(HarnessEvent::TurnStarted {
                    turn_id: turn.clone(),
                    prompt: None,
                });
            }
            for tool in &snapshot.tools {
                if let (Some(id), Some(tool)) = (tool["id"].as_str(), tool["tool"].as_str()) {
                    driver.events.push_back(HarnessEvent::ItemStarted {
                        item_id: id.into(),
                        kind: ItemKind::ToolCall,
                        tool: Some(tool.into()),
                    });
                }
            }
            if snapshot.compacting {
                let item_id = format!("{}:compaction", snapshot.generation);
                driver.attaching_compaction = Some(item_id.clone());
                driver.events.push_back(HarnessEvent::ItemStarted {
                    item_id,
                    kind: ItemKind::ContextCompaction,
                    tool: None,
                });
            }
            driver.update_approvals();
            Ok(driver)
        })()
        .map_err(LaunchError::not_spawned)
    }

    pub fn runtime_thread_id(&self) -> Option<String> {
        Some(self.snapshot.session.clone())
    }
    pub fn transcript_path(&self) -> Option<PathBuf> {
        Some(self.transcript.clone())
    }
    pub fn has_active_turn(&self) -> bool {
        self.snapshot.turn.is_some()
    }

    fn client(&self) -> crate::Result<&Client> {
        self.client
            .as_ref()
            .context("native controller has detached")
    }

    pub async fn start_turn(
        &mut self,
        message: &str,
        guard: Option<TurnGuardAttempt>,
    ) -> TurnStartDispatch {
        if guard.is_some() {
            return TurnStartDispatch::Resolved(TurnStartOutcome::ExplicitRefusal {
                message: "native queue submission cannot apply a permission override".into(),
                retained_mode: None,
            });
        }
        if self.pending_prompt.is_some() {
            return TurnStartDispatch::Resolved(TurnStartOutcome::ConcurrentNotAccepted {
                message: "a native submission is still awaiting its receipt".into(),
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        match self
            .client()
            .and_then(|client| client.submit(&id, "prompt", json!({"text":message})))
        {
            Ok(()) => {
                self.pending_prompt = Some(PendingPrompt {
                    id,
                    steering: false,
                });
                TurnStartDispatch::Awaiting
            }
            Err(error) => TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                message: error.to_string(),
            }),
        }
    }

    pub fn steer(&mut self, message: &str) -> SteerDispatch {
        let submitted = (|| {
            ensure!(
                self.pending_prompt.is_none(),
                "a native submission is awaiting its receipt"
            );
            let id = uuid::Uuid::new_v4().to_string();
            self.client()?
                .submit(&id, "prompt", json!({"text":message}))?;
            self.pending_prompt = Some(PendingPrompt { id, steering: true });
            Ok(())
        })();
        match submitted {
            Ok(()) => SteerDispatch::Awaiting,
            Err(error) => SteerDispatch::Resolved(Err(error)),
        }
    }

    pub fn interrupt(&mut self, expected: Option<&str>) -> InterruptDispatch {
        match self.submit_interrupt(expected) {
            Ok(Some(outcome)) => InterruptDispatch::Resolved(Ok(outcome)),
            Ok(None) => InterruptDispatch::Awaiting,
            Err(error) => InterruptDispatch::Resolved(Err(error)),
        }
    }

    fn submit_interrupt(
        &mut self,
        expected: Option<&str>,
    ) -> crate::Result<Option<InterruptOutcome>> {
        let snapshot = self.client()?.snapshot()?;
        let Some(turn) = snapshot.turn.as_deref() else {
            return Ok(Some(InterruptOutcome::NoLongerActive));
        };
        if expected.is_some_and(|expected| expected != turn) {
            return Ok(Some(InterruptOutcome::NoLongerActive));
        }
        ensure!(
            self.pending_interrupt.is_none(),
            "a native stop is awaiting its receipt"
        );
        let id = uuid::Uuid::new_v4().to_string();
        self.client()?
            .submit(&id, "abort", json!({"turnId":turn}))?;
        self.pending_interrupt = Some((id, turn.to_owned()));
        Ok(None)
    }

    pub async fn answer_approval(&mut self, response: &ApprovalResponse) -> ApprovalOutcome {
        if response.scope != ApprovalScope::Once {
            return ApprovalOutcome::ExplicitRefusal {
                message: "native control has not negotiated this approval form".into(),
                retained: true,
            };
        }
        match self.client().and_then(|client| client.answer(response)) {
            Ok(()) => ApprovalOutcome::AwaitingResolution {
                message: "Waiting for the native approval to resolve".into(),
            },
            Err(error) => ApprovalOutcome::ExplicitRefusal {
                message: error.to_string(),
                retained: self.approvals.contains(&response.approval_id),
            },
        }
    }

    pub fn abandon_pending_approvals(&mut self) -> usize {
        let count = self.approvals.len();
        self.retired_approvals.extend(self.approvals.drain());
        count
    }

    pub async fn model_control(
        &mut self,
        patch: Option<&models::ModelPatch>,
    ) -> crate::Result<Value> {
        self.reconcile_model();
        let snapshot = self.client()?.snapshot()?;
        if let Some(patch) = patch {
            ensure!(
                patch.effort.is_none(),
                "native effort changes are not negotiated"
            );
            let controls = snapshot
                .model_controls
                .as_ref()
                .context("native model changes are not negotiated")?;
            ensure!(
                self.pending_model.is_none() && controls.pending.is_none(),
                "a native model change is awaiting confirmation"
            );
            ensure!(!controls.locked, "native model is managed by policy");
            let requested = patch
                .model
                .as_ref()
                .context("choose a model to change")?
                .as_deref()
                .unwrap_or("default");
            ensure!(
                controls.choices.iter().any(|choice| choice == requested),
                "choose a model offered by the native runtime"
            );
            let id = uuid::Uuid::new_v4().to_string();
            self.client()?
                .submit(&id, "model", json!({"model":requested}))?;
            self.pending_model = Some(id);
        }
        Ok(model_settings(&snapshot, self.pending_model.is_some()))
    }

    fn reconcile_model(&mut self) -> bool {
        if self.pending_model.as_ref().is_some_and(|id| {
            self.client
                .as_ref()
                .and_then(|client| client.result(id))
                .is_some()
        }) {
            self.pending_model = None;
            return true;
        }
        false
    }

    pub async fn runtime_command(&mut self, name: &str, _: Value) -> crate::Result<Value> {
        ensure!(
            matches!(name, "commands" | "commands.list"),
            "this native command is not negotiated"
        );
        Ok(json!({"commands":self.client()?.snapshot()?.commands,"authoritative":true}))
    }

    pub async fn set_permission_mode(&mut self, _: PermissionMode) -> PermissionModeChangeResult {
        Err(PermissionModeChangeError::refused(
            "native permission changes are not negotiated",
        ))
    }

    pub fn permission_mode(&self) -> PermissionMode {
        PermissionMode::Default
    }

    fn observe(&mut self, update: Update) {
        if self.reconcile_model()
            || self.snapshot.model != update.snapshot.model
            || self.snapshot.model_controls != update.snapshot.model_controls
        {
            self.events.push_back(HarnessEvent::ModelUpdated);
        }
        self.snapshot = update.snapshot;
        if !self.snapshot.compacting
            && let Some(item_id) = self.attaching_compaction.take()
        {
            self.events
                .push_back(HarnessEvent::ItemCompleted { item_id });
        }
        for event in update.events {
            let id = || event["id"].as_str().map(String::from);
            let turn = || event["turn"].as_str().map(String::from);
            let mapped = match event["kind"].as_str() {
                Some("turn_started") => turn().map(|turn_id| HarnessEvent::TurnStarted {
                    turn_id,
                    prompt: None,
                }),
                Some("turn_completed") => turn().map(|turn_id| HarnessEvent::TurnCompleted {
                    turn_id,
                    outcome: match event["reason"].as_str() {
                        Some("answer") => TurnOutcome::Ok,
                        Some("aborted" | "interrupted") => TurnOutcome::Interrupted,
                        _ => TurnOutcome::Error,
                    },
                    error: None,
                    cost_usd: None,
                    duration_ms: event["duration_ms"].as_u64(),
                }),
                Some("tool_started" | "compaction_started") => {
                    id().map(|item_id| HarnessEvent::ItemStarted {
                        item_id,
                        kind: if event["kind"] == "tool_started" {
                            ItemKind::ToolCall
                        } else {
                            ItemKind::ContextCompaction
                        },
                        tool: event["tool"].as_str().map(String::from),
                    })
                }
                Some("tool_completed" | "compaction_completed") => {
                    id().map(|item_id| HarnessEvent::ItemCompleted { item_id })
                }
                _ => None,
            };
            if let Some(event) = mapped {
                self.events.push_back(event);
            }
        }
        self.update_approvals();
        if let Some((id, turn)) = &self.pending_interrupt {
            let result = self.client.as_ref().and_then(|client| client.result(id));
            let outcome = if self.snapshot.turn.as_ref() != Some(turn) {
                Some(Ok(InterruptOutcome::NoLongerActive))
            } else {
                result.map(|result| match result["outcome"].as_str() {
                    Some("requested") => Ok(InterruptOutcome::Requested),
                    Some("no_longer_active") => Ok(InterruptOutcome::NoLongerActive),
                    _ => Err("native interruption is still awaiting confirmation".into()),
                })
            };
            if let Some(outcome) = outcome {
                self.pending_interrupt = None;
                // A late stop receipt cannot retire approvals belonging to a newer turn.
                self.events
                    .push_back(HarnessEvent::InterruptResolved(outcome));
            }
        }
        if let Some(PendingPrompt { id, steering }) = &self.pending_prompt {
            let outcome = match self
                .client()
                .ok()
                .and_then(|client| client.result(id))
                .as_ref()
                .and_then(|result| result["outcome"].as_str())
            {
                Some(outcome @ ("accepted" | "command_completed")) => {
                    Some(TurnStartOutcome::NativeSubmission {
                        operation_id: id.clone(),
                        delivery: if outcome == "command_completed" {
                            Delivery::CommandCompleted
                        } else {
                            Delivery::WhenIdle
                        },
                    })
                }
                Some("not_sent") => Some(TurnStartOutcome::ExplicitRefusal {
                    message: "native submission was not delivered".into(),
                    retained_mode: None,
                }),
                Some(_) => Some(TurnStartOutcome::SharedUnknown {
                    message: "native submission is awaiting reconciliation".into(),
                }),
                None => None,
            };
            if let Some(outcome) = outcome {
                let event = if *steering {
                    HarnessEvent::SteerResolved(match outcome {
                        TurnStartOutcome::NativeSubmission { delivery, .. } => Ok(delivery),
                        _ => Err("native prompt delivery is not confirmed".into()),
                    })
                } else {
                    HarnessEvent::TurnStartResolved(outcome)
                };
                self.pending_prompt = None;
                self.events.push_front(event);
            }
        }
    }

    fn update_approvals(&mut self) {
        let pending: HashSet<_> = self
            .snapshot
            .approvals
            .iter()
            .filter_map(|approval| approval["id"].as_str().map(String::from))
            .collect();
        // A stop receipt can precede the native snapshot that removes its approval.
        self.retired_approvals.retain(|id| pending.contains(id));
        let pending: HashSet<_> = pending
            .difference(&self.retired_approvals)
            .cloned()
            .collect();
        for id in self.approvals.difference(&pending) {
            self.events.push_back(HarnessEvent::ApprovalResolved {
                approval_id: id.clone(),
            });
        }
        for approval in &self.snapshot.approvals {
            let Some(id) = approval["id"]
                .as_str()
                .filter(|id| pending.contains(*id) && !self.approvals.contains(*id))
            else {
                continue;
            };
            let (Some(turn), Some(tool)) = (approval["turn"].as_str(), approval["tool"].as_str())
            else {
                continue;
            };
            let input = approval["input"].clone();
            let paths = super::paths_of(&input);
            let kind = if !paths.is_empty() {
                ApprovalKind::FileChange
            } else if matches!(tool, "WebFetch" | "WebSearch") {
                ApprovalKind::PermissionEscalation
            } else {
                ApprovalKind::Exec
            };
            self.events
                .push_back(HarnessEvent::Approval(ApprovalRequest {
                    session_id: String::new(),
                    approval_id: id.into(),
                    turn_id: turn.into(),
                    kind,
                    tool: tool.into(),
                    summary: super::summarize_tool(tool, &input),
                    paths,
                    input,
                    timeout_secs: 0,
                    requires_owner: true,
                    owner_reason: None,
                    can_allow_for_session: false,
                    suggested_permission_mode: None,
                    requested_at: chrono::Utc::now().to_rfc3339(),
                }));
        }
        self.approvals = pending;
    }

    pub async fn next_event(&mut self) -> Option<HarnessEvent> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Some(event);
            }
            let update = self.client().ok()?.next_update().await;
            match update {
                Ok(update) => self.observe(update),
                Err(_) => {
                    self.client.take();
                    return Some(HarnessEvent::Exited { code: None });
                }
            }
        }
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        self.client.take();
        Ok(())
    }
}

fn model_settings(snapshot: &Snapshot, awaiting_receipt: bool) -> Value {
    let controls = snapshot.model_controls.as_ref();
    let pending = awaiting_receipt || controls.is_some_and(|value| value.pending.is_some());
    let mutable = !pending && controls.is_some_and(|value| !value.locked);
    let choices: Vec<_> = controls
        .into_iter()
        .flat_map(|value| &value.choices)
        .map(|id| json!({"id":id,"name":id,"is_default":id == "default"}))
        .collect();
    json!({"model":snapshot.model,
        "selected_model":controls.map(|value| &value.selected).or(snapshot.model.as_ref()),
        "effort":null,"effort_known":false,"pending":null,"settings_unknown":pending,
        "models":choices,"efforts":[],"applied":if pending {"pending"} else {"immediate"},
        "capabilities":{"model":mutable,"effort":false,
            "reset_model":mutable && controls.is_some_and(|value| value.choices.iter().any(|id| id == "default")),
            "reset_effort":false}})
}
