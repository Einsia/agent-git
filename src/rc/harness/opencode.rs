//! OpenCode's ACP control plane keeps native SQLite records authoritative.

use super::proc::{LaunchError, Line, Proc};
use super::{
    ApprovalOutcome, HarnessEvent, LaunchSpec, PermissionModeChangeError,
    PermissionModeChangeResult, TurnOutcome, TurnStartConfirmation, TurnStartDispatch,
    TurnStartOutcome,
};
use crate::protocol::{
    ApprovalDecision, ApprovalKind, ApprovalRequest, ApprovalResponse, ApprovalScope, Delivery,
    PermissionMode, RuntimeCapability,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const ACCEPTANCE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_APPROVALS: usize = 128;
const FINAL_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct NativeSnapshot {
    pub bytes: Result<Vec<u8>, String>,
    pub finalized: bool,
}

#[derive(Default)]
struct Shared {
    session: Option<String>,
    snapshot: Option<NativeSnapshot>,
    approvals: usize,
}

enum Command {
    Start(String, oneshot::Sender<TurnStartDispatch>),
    Interrupt(oneshot::Sender<crate::Result<()>>),
    Approve(ApprovalResponse, oneshot::Sender<ApprovalOutcome>),
    Shutdown,
}

pub struct OpenCodeDriver {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<HarnessEvent>,
    shared: Arc<Mutex<Shared>>,
    task: Option<tokio::task::JoinHandle<crate::Result<()>>>,
    shutdown_failure: Option<String>,
}

impl OpenCodeDriver {
    pub async fn launch(spec: LaunchSpec) -> Result<Self, LaunchError> {
        let engine = OpenCodeEngine::launch(spec).await?;
        let (commands, receiver) = mpsc::channel(16);
        let (sender, events) = mpsc::channel(32);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let task_shared = shared.clone();
        let task = tokio::spawn(run_engine(engine, receiver, sender, task_shared));
        Ok(Self {
            commands,
            events,
            shared,
            task: Some(task),
            shutdown_failure: None,
        })
    }

    pub fn runtime_thread_id(&self) -> Option<String> {
        self.shared
            .lock()
            .expect("OpenCode metadata lock")
            .session
            .clone()
    }

    pub fn transcript_path(&self) -> Option<PathBuf> {
        None
    }

    pub fn take_snapshot(&mut self) -> Option<NativeSnapshot> {
        self.shared
            .lock()
            .expect("OpenCode metadata lock")
            .snapshot
            .take()
    }

    pub async fn start_turn(&mut self, message: &str) -> TurnStartDispatch {
        let (send, receive) = oneshot::channel();
        if self
            .commands
            .send(Command::Start(message.into(), send))
            .await
            .is_err()
        {
            return TurnStartDispatch::Resolved(TurnStartOutcome::FatalNotAccepted {
                message: "OpenCode control plane has ended".into(),
            });
        }
        receive.await.unwrap_or_else(|_| {
            TurnStartDispatch::Resolved(TurnStartOutcome::Unknown {
                message: "OpenCode prompt delivery is unknown".into(),
                attempted_mode: None,
            })
        })
    }

    pub async fn steer(&mut self, _message: &str) -> crate::Result<Delivery> {
        anyhow::bail!(
            "OpenCode RC cannot steer an active turn; interrupt it or wait for completion"
        )
    }

    pub async fn interrupt(&mut self) -> crate::Result<()> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::Interrupt(send))
            .await
            .map_err(|_| anyhow::anyhow!("OpenCode control plane has ended"))?;
        receive
            .await
            .map_err(|_| anyhow::anyhow!("OpenCode interrupt outcome is unknown"))?
    }

    pub async fn answer_approval(&mut self, response: &ApprovalResponse) -> ApprovalOutcome {
        let (send, receive) = oneshot::channel();
        if self
            .commands
            .send(Command::Approve(response.clone(), send))
            .await
            .is_err()
        {
            return ApprovalOutcome::ExplicitRefusal {
                message: "OpenCode control plane has ended".into(),
                retained: false,
            };
        }
        receive.await.unwrap_or_else(|_| ApprovalOutcome::Unknown {
            message: "OpenCode approval delivery is unknown".into(),
            attempted_mode: None,
        })
    }

    pub fn abandon_pending_approvals(&mut self) -> usize {
        std::mem::take(
            &mut self
                .shared
                .lock()
                .expect("OpenCode metadata lock")
                .approvals,
        )
    }

    pub fn permission_mode(&self) -> PermissionMode {
        PermissionMode::Default
    }

    pub async fn set_permission_mode(
        &mut self,
        _mode: PermissionMode,
    ) -> PermissionModeChangeResult {
        Err(PermissionModeChangeError::refused(
            "OpenCode RC permission mode cannot be changed",
        ))
    }

    pub async fn next_event(&mut self) -> Option<HarnessEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        if let Some(message) = &self.shutdown_failure {
            anyhow::bail!("{message}");
        }
        let _ = self.commands.send(Command::Shutdown).await;
        if let Some(task) = self.task.take() {
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            };
            if let Err(error) = result {
                let message = format!("OpenCode process-tree termination is unproven: {error}");
                self.shutdown_failure = Some(message.clone());
                anyhow::bail!("{message}");
            }
        }
        Ok(())
    }
}

async fn run_engine(
    mut engine: OpenCodeEngine,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<HarnessEvent>,
    shared: Arc<Mutex<Shared>>,
) -> crate::Result<()> {
    enum Input {
        Command(Option<Command>),
        Line(Option<super::proc::QueuedLine>),
        Timeout,
    }
    loop {
        {
            let mut state = shared.lock().expect("OpenCode metadata lock");
            state.session = engine.runtime_thread_id().map(String::from);
            if let Some(bytes) = engine.snapshot.take() {
                state.snapshot = Some(NativeSnapshot {
                    bytes: Ok(bytes),
                    finalized: false,
                });
            }
            state.approvals = engine.approvals.len();
        }
        let input = if !engine.events.is_empty() {
            tokio::select! {
                command = commands.recv() => Input::Command(command),
                permit = events.reserve() => {
                    let Ok(permit) = permit else { break; };
                    permit.send(engine.events.pop_front().expect("queued event"));
                    continue;
                }
            }
        } else {
            if engine.exited {
                break;
            }
            let deadline = if engine.phase != Phase::Ready {
                Some(engine.deadline)
            } else {
                engine
                    .turn
                    .as_ref()
                    .filter(|turn| !turn.accepted)
                    .map(|turn| turn.deadline)
            };
            tokio::select! {
                command = commands.recv() => Input::Command(command),
                line = engine.proc.next() => Input::Line(line),
                _ = async { match deadline { Some(value) => tokio::time::sleep_until(value).await, None => std::future::pending().await } } => Input::Timeout,
            }
        };
        // Native frame handling owns its I/O until it finishes; viewer polling cannot cancel it.
        match input {
            Input::Command(Some(Command::Start(message, reply))) => {
                if !reply.is_closed() {
                    let result = engine.start_turn(&message).await;
                    let _ = reply.send(result);
                }
            }
            Input::Command(Some(Command::Interrupt(reply))) => {
                if !reply.is_closed() {
                    let result = engine.interrupt().await;
                    let _ = reply.send(result);
                }
            }
            Input::Command(Some(Command::Approve(response, reply))) => {
                if !reply.is_closed() {
                    let result = engine.answer_approval(&response).await;
                    let _ = reply.send(result);
                }
            }
            Input::Command(Some(Command::Shutdown) | None) => break,
            Input::Timeout => {
                let event = engine.fatal("OpenCode ACP response timed out");
                engine.events.push_back(event);
            }
            Input::Line(line) => match line.map(|line| line.into_line()) {
                Some(Line::Json(value)) => {
                    if let Err(error) = engine.frame(value).await {
                        let event = engine.fatal(&error.to_string());
                        engine.events.push_back(event);
                    }
                }
                Some(Line::Notice(_)) => {}
                Some(Line::Fatal(_)) => {
                    let event = engine.fatal("OpenCode ACP lost a protocol frame");
                    engine.events.push_back(event);
                }
                Some(Line::Eof) | None => {
                    engine.exited = true;
                    engine.events.push_back(HarnessEvent::Exited {
                        code: engine.proc.wait().await,
                    });
                }
            },
        }
    }
    engine.proc.shutdown().await?;
    if engine.session.is_some() {
        // A stopped native writer cannot append rows after the exit snapshot is captured.
        let bytes = match tokio::time::timeout(FINAL_SNAPSHOT_TIMEOUT, engine.read_snapshot()).await {
            Ok(Ok(())) => Ok(engine.snapshot.take().expect("read native snapshot")),
            Ok(Err(_)) | Err(_) => Err("OpenCode's final native history could not be read. Resume the session to recover its stored records.".into()),
        };
        shared.lock().expect("OpenCode metadata lock").snapshot = Some(NativeSnapshot {
            bytes,
            finalized: true,
        });
    }
    Ok(())
}

pub fn capability() -> RuntimeCapability {
    RuntimeCapability {
        runtime: "opencode".into(),
        available: crate::adapter::which("opencode").is_some(),
        version: super::probe_version("opencode", &["--version"]),
        steer: None,
        interrupt: true,
        approvals: true,
        partial_messages: true,
        resume: true,
        commands: vec![],
        permission_modes: vec![PermissionMode::Default],
        permission_switch: None,
    }
}

struct Approval {
    native_id: Value,
    allow: String,
    deny: String,
}

struct Turn {
    request_id: u64,
    id: String,
    accepted: bool,
    cancelling: bool,
    deadline: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Initialize,
    Open,
    Mode,
    Model,
    Ready,
}

struct OpenCodeEngine {
    proc: Proc,
    spec: LaunchSpec,
    agent: String,
    phase: Phase,
    phase_request: u64,
    deadline: Instant,
    next_id: u64,
    session: Option<String>,
    turn: Option<Turn>,
    approvals: HashMap<String, Approval>,
    events: VecDeque<HarnessEvent>,
    snapshot: Option<Vec<u8>>,
    source: Option<crate::adapter::native_snapshot::Source>,
    seen_approvals: super::BoundedTurnIds,
    exited: bool,
}

fn launch_env(spec: &LaunchSpec, agent: &str) -> crate::Result<Vec<(String, String)>> {
    let mut config = match std::env::var("OPENCODE_CONFIG_CONTENT") {
        Ok(value) => serde_json::from_str::<Value>(&value)
            .map_err(|_| anyhow::anyhow!("OPENCODE_CONFIG_CONTENT is not valid JSON"))?,
        Err(std::env::VarError::NotPresent) => json!({}),
        Err(_) => anyhow::bail!("OPENCODE_CONFIG_CONTENT is not Unicode"),
    };
    let object = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode inline configuration must be an object"))?;
    let agents = object.entry("agent").or_insert_with(|| json!({}));
    let agents = agents
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode agent configuration must be an object"))?;
    // A fresh agent name prevents a configured agent's allow rules from overriding RC prompts.
    // Nested agents and interactive questions require control planes this driver cannot relay.
    agents.insert(
        agent.into(),
        json!({
            "description":"AgentGit remote workspace", "mode":"primary",
            "permission":{"*":"ask", "task":"deny", "question":"deny"},
            "tools":{"task":false,"question":false}
        }),
    );
    object.insert("default_agent".into(), json!(agent));
    let mut env = super::lineage_env(spec.agit_session.as_ref());
    env.push(("OPENCODE_CONFIG_CONTENT".into(), config.to_string()));
    env.push((
        "OPENCODE_SERVER_PASSWORD".into(),
        uuid::Uuid::new_v4().to_string(),
    ));
    Ok(env)
}

impl OpenCodeEngine {
    pub async fn launch(spec: LaunchSpec) -> Result<Self, LaunchError> {
        if spec.effective_mode() != PermissionMode::Default {
            return Err(LaunchError::not_spawned(anyhow::anyhow!(
                "OpenCode RC supports only the default permission mode"
            )));
        }
        let agent = format!("agit-rc-{}", uuid::Uuid::new_v4().simple());
        let env = launch_env(&spec, &agent).map_err(LaunchError::not_spawned)?;
        let args = ["acp", "--hostname", "127.0.0.1", "--port", "0", "--cwd"]
            .into_iter()
            .map(String::from)
            .chain([spec.cwd.to_string_lossy().into_owned()])
            .collect::<Vec<_>>();
        let proc = Proc::spawn_strict_json("opencode", &args, &spec.cwd, &env)?;
        let mut driver = Self {
            proc,
            spec,
            agent,
            phase: Phase::Initialize,
            phase_request: 1,
            deadline: Instant::now() + HANDSHAKE_TIMEOUT,
            next_id: 1,
            session: None,
            turn: None,
            approvals: HashMap::new(),
            events: VecDeque::new(),
            snapshot: None,
            source: None,
            seen_approvals: super::BoundedTurnIds::default(),
            exited: false,
        };
        if driver
            .request(
                "initialize",
                json!({
                    "protocolVersion":1,"clientCapabilities":{},
                    "clientInfo":{"name":"AgentGit","version":env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
            .is_err()
        {
            let _ = driver.proc.shutdown().await;
            return Err(LaunchError::spawned(anyhow::anyhow!(
                "OpenCode initialization write failed"
            )));
        }
        Ok(driver)
    }

    async fn request(&mut self, method: &str, params: Value) -> crate::Result<u64> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("OpenCode request ids exhausted"))?;
        self.proc
            .write_line(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    pub fn runtime_thread_id(&self) -> Option<&str> {
        (self.phase == Phase::Ready)
            .then_some(self.session.as_deref())
            .flatten()
    }

    async fn read_snapshot(&mut self) -> crate::Result<()> {
        let id = self
            .session
            .clone()
            .ok_or_else(|| anyhow::anyhow!("OpenCode session is missing"))?;
        let cwd = self.spec.cwd.clone();
        let selected = self.source.clone();
        let (source, bytes) = tokio::task::spawn_blocking(move || {
            use crate::adapter::{Adapter, native_snapshot::Limits, opencode::OpenCode};
            let source = match selected {
                Some(source) => source,
                None => OpenCode.lookup_native_readonly(&id, Limits::default())?,
            };
            crate::adapter::opencode::validate_rc_session(&source.path, &id, &cwd)?;
            Ok::<_, anyhow::Error>((
                source.clone(),
                OpenCode
                    .snapshot_native_readonly(&source, Limits::default())?
                    .bytes,
            ))
        })
        .await??;
        self.source = Some(source);
        self.snapshot = Some(bytes);
        Ok(())
    }

    async fn ready(&mut self) -> crate::Result<()> {
        self.read_snapshot().await?;
        self.phase = Phase::Ready;
        self.events.push_back(HarnessEvent::Ready {
            runtime_thread_id: self.session.clone().expect("opened session"),
            transcript_path: None,
        });
        Ok(())
    }

    pub async fn start_turn(&mut self, message: &str) -> TurnStartDispatch {
        let refused = |message: &str| {
            TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                message: message.into(),
            })
        };
        if self.phase != Phase::Ready {
            return refused("OpenCode is still initializing");
        }
        if self.turn.is_some() {
            return TurnStartDispatch::Resolved(TurnStartOutcome::ConcurrentNotAccepted {
                message: "OpenCode already has an active prompt".into(),
            });
        }
        if message.trim_start().starts_with('/') {
            return TurnStartDispatch::Resolved(TurnStartOutcome::ExplicitRefusal {
                message: "OpenCode RC accepts text prompts; native slash commands are not available remotely".into(), retained_mode: None,
            });
        }
        if self.next_id == u64::MAX {
            return TurnStartDispatch::Resolved(TurnStartOutcome::FatalNotAccepted {
                message: "OpenCode request ids exhausted".into(),
            });
        }
        let id = self.next_id;
        self.turn = Some(Turn {
            request_id: id,
            id: format!("opencode-{id}"),
            accepted: false,
            cancelling: false,
            deadline: Instant::now() + ACCEPTANCE_TIMEOUT,
        });
        if self
            .request(
                "session/prompt",
                json!({"sessionId":self.session,"prompt":[{"type":"text","text":message}]}),
            )
            .await
            .is_err()
        {
            return TurnStartDispatch::Resolved(TurnStartOutcome::Unknown {
                message: "OpenCode prompt delivery is unknown".into(),
                attempted_mode: None,
            });
        }
        TurnStartDispatch::Awaiting
    }

    fn accept_turn(&mut self, exact: bool) {
        if let Some(turn) = &mut self.turn
            && !turn.accepted
        {
            turn.accepted = true;
            self.events.push_back(HarnessEvent::TurnStartResolved(
                TurnStartOutcome::Accepted {
                    turn_id: turn.id.clone(),
                    still_running: true,
                    consumed_mode: None,
                    confirmation: if exact {
                        TurnStartConfirmation::Exact
                    } else {
                        TurnStartConfirmation::NotificationOnly
                    },
                },
            ));
        }
    }

    pub async fn interrupt(&mut self) -> crate::Result<()> {
        if let Some(turn) = &mut self.turn {
            turn.cancelling = true;
            self.proc.write_line(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session}})).await?;
            self.cancel_approvals().await?;
        }
        Ok(())
    }

    async fn cancel_permission(&mut self, id: Value) -> crate::Result<()> {
        self.proc
            .write_line(
                &json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"cancelled"}}}),
            )
            .await
    }

    async fn cancel_approvals(&mut self) -> crate::Result<()> {
        // Native permission queues advance only after their outstanding requests receive replies.
        let approvals = std::mem::take(&mut self.approvals);
        self.events
            .retain(|event| !matches!(event, HarnessEvent::Approval(_)));
        for approval in approvals.into_values() {
            self.cancel_permission(approval.native_id).await?;
        }
        Ok(())
    }

    pub async fn answer_approval(&mut self, response: &ApprovalResponse) -> ApprovalOutcome {
        if response.scope != ApprovalScope::Once {
            return ApprovalOutcome::ExplicitRefusal {
                message: "OpenCode RC approvals apply only to this request".into(),
                retained: self.approvals.contains_key(&response.approval_id),
            };
        }
        let Some(approval) = self.approvals.remove(&response.approval_id) else {
            return ApprovalOutcome::ExplicitRefusal {
                message: "OpenCode approval is no longer pending".into(),
                retained: false,
            };
        };
        let option = if response.decision == ApprovalDecision::Allow {
            approval.allow
        } else {
            approval.deny
        };
        match self.proc.write_line(&json!({"jsonrpc":"2.0","id":approval.native_id,"result":{"outcome":{"outcome":"selected","optionId":option}}})).await {
            Ok(()) => ApprovalOutcome::Applied { effective_mode: None },
            Err(_) => ApprovalOutcome::Unknown { message: "OpenCode approval delivery is unknown".into(), attempted_mode: None },
        }
    }

    async fn frame(&mut self, frame: Value) -> crate::Result<()> {
        if frame["jsonrpc"] != "2.0" {
            anyhow::bail!("OpenCode emitted an invalid ACP frame");
        }
        if let Some(method) = frame["method"].as_str() {
            let params = &frame["params"];
            if method == "session/request_permission" {
                return self.permission(&frame).await;
            }
            if frame.get("id").is_some() {
                self.proc.write_line(&json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Client operation is not supported"}})).await?;
                return Ok(());
            }
            if method != "session/update" || self.phase != Phase::Ready {
                return Ok(());
            }
            if params["sessionId"].as_str() != self.session.as_deref() {
                anyhow::bail!("OpenCode update belongs to another session");
            }
            let update = &params["update"];
            if matches!(
                update["sessionUpdate"].as_str(),
                Some(
                    "agent_message_chunk"
                        | "agent_thought_chunk"
                        | "tool_call"
                        | "tool_call_update"
                        | "user_message_chunk"
                )
            ) {
                if self.turn.is_none() {
                    return Ok(());
                }
                self.accept_turn(false);
                if update["sessionUpdate"] == "agent_message_chunk"
                    && update["content"]["type"] == "text"
                    && let Some(text) = update["content"]["text"].as_str()
                {
                    self.events.push_back(HarnessEvent::Delta {
                        item_id: self.turn.as_ref().expect("active turn").id.clone(),
                        text: text.into(),
                    });
                }
            }
            return Ok(());
        }
        let id = frame["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("OpenCode response id is invalid"))?;
        if self.phase != Phase::Ready {
            if id != self.phase_request {
                anyhow::bail!("OpenCode handshake response does not match its request");
            }
            if frame.get("error").is_some() || !frame["result"].is_object() {
                anyhow::bail!("OpenCode ACP handshake was rejected");
            }
            match self.phase {
                Phase::Initialize => {
                    if frame["result"]["protocolVersion"] != 1 {
                        anyhow::bail!("OpenCode ACP protocol version is unsupported");
                    }
                    self.phase = Phase::Open;
                    self.phase_request = if let Some(id) = self.spec.resume_from.clone() {
                        self.session = Some(id.clone());
                        self.read_snapshot().await?;
                        self.request(
                            "session/load",
                            json!({"sessionId":id,"cwd":self.spec.cwd,"mcpServers":[]}),
                        )
                        .await?
                    } else {
                        self.request("session/new", json!({"cwd":self.spec.cwd,"mcpServers":[]}))
                            .await?
                    };
                }
                Phase::Open => {
                    if self.session.is_none() {
                        let id = frame["result"]["sessionId"].as_str().ok_or_else(|| {
                            anyhow::anyhow!("OpenCode did not return a session id")
                        })?;
                        super::validate_native_turn_id(id).map_err(|e| anyhow::anyhow!("{e}"))?;
                        self.session = Some(id.into());
                    }
                    self.phase = Phase::Mode;
                    self.phase_request = self
                        .request(
                            "session/set_mode",
                            json!({"sessionId":self.session,"modeId":self.agent}),
                        )
                        .await?;
                }
                Phase::Mode => {
                    if let Some(model) = self.spec.model.clone() {
                        self.phase = Phase::Model;
                        self.phase_request = self
                            .request(
                                "session/set_model",
                                json!({"sessionId":self.session,"modelId":model}),
                            )
                            .await?;
                    } else {
                        self.ready().await?;
                    }
                }
                Phase::Model => self.ready().await?,
                Phase::Ready => unreachable!(),
            }
            return Ok(());
        }
        let Some(turn) = &self.turn else {
            anyhow::bail!("OpenCode returned a response without an active prompt");
        };
        if id != turn.request_id {
            anyhow::bail!("OpenCode prompt response does not match its request");
        }
        if frame.get("error").is_some() {
            anyhow::bail!("OpenCode prompt failed with an unknown execution outcome");
        }
        let outcome = match frame["result"]["stopReason"].as_str() {
            Some("cancelled") => TurnOutcome::Interrupted,
            Some("end_turn" | "max_tokens" | "max_turn_requests" | "refusal") => TurnOutcome::Ok,
            _ => anyhow::bail!("OpenCode prompt response has no supported stop reason"),
        };
        self.read_snapshot().await?;
        self.accept_turn(true);
        let turn = self.turn.take().expect("matched prompt");
        self.cancel_approvals().await?;
        self.events.push_back(HarnessEvent::TurnCompleted {
            turn_id: turn.id,
            outcome,
            error: None,
            cost_usd: None,
            duration_ms: None,
        });
        Ok(())
    }

    async fn permission(&mut self, frame: &Value) -> crate::Result<()> {
        let params = &frame["params"];
        if self.phase != Phase::Ready || params["sessionId"].as_str() != self.session.as_deref() {
            anyhow::bail!("OpenCode approval has no matching active session");
        }
        let id = &frame["id"];
        if !(id.is_string() || id.as_i64().is_some()) || id.to_string().len() > 256 {
            anyhow::bail!("OpenCode approval id is invalid");
        }
        let key = id.to_string();
        if !self
            .seen_approvals
            .try_insert(key.clone())
            .map_err(|error| anyhow::anyhow!("OpenCode approval identity: {error}"))?
        {
            anyhow::bail!("OpenCode reused an approval identity");
        }
        if self.turn.as_ref().is_none_or(|turn| turn.cancelling) {
            return self.cancel_permission(id.clone()).await;
        }
        if self.approvals.len() >= MAX_APPROVALS || self.approvals.contains_key(&key) {
            anyhow::bail!("OpenCode approval identities are ambiguous or exceed the pending limit");
        }
        let options = params["options"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("OpenCode approval has no options"))?;
        let option = |kind: &str| -> crate::Result<String> {
            let found = options
                .iter()
                .filter(|v| v["kind"] == kind)
                .collect::<Vec<_>>();
            if found.len() != 1 {
                anyhow::bail!("OpenCode approval options are ambiguous");
            }
            let value = found[0]["optionId"]
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 256)
                .ok_or_else(|| anyhow::anyhow!("OpenCode approval option is invalid"))?;
            Ok(value.into())
        };
        let approval = Approval {
            native_id: id.clone(),
            allow: option("allow_once")?,
            deny: option("reject_once")?,
        };
        if approval.allow == approval.deny {
            anyhow::bail!("OpenCode approval options are indistinguishable");
        }
        self.accept_turn(false);
        let tool = &params["toolCall"];
        let kind = if tool["kind"] == "execute" {
            ApprovalKind::Exec
        } else if tool["kind"] == "edit" {
            ApprovalKind::FileChange
        } else {
            ApprovalKind::PermissionEscalation
        };
        self.events
            .push_back(HarnessEvent::Approval(ApprovalRequest {
                approval_id: key.clone(),
                session_id: String::new(),
                turn_id: self.turn.as_ref().expect("active turn").id.clone(),
                kind,
                tool: tool["kind"].as_str().unwrap_or("unknown").into(),
                input: tool["rawInput"].clone(),
                summary: tool["title"]
                    .as_str()
                    .unwrap_or("OpenCode requests permission")
                    .into(),
                paths: tool["locations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["path"].as_str().map(String::from))
                    .collect(),
                timeout_secs: 0,
                requires_owner: true,
                owner_reason: Some(crate::protocol::OwnerReason::Unprovable),
                can_allow_for_session: false,
                suggested_permission_mode: None,
                requested_at: chrono::Utc::now().to_rfc3339(),
            }));
        self.approvals.insert(key, approval);
        Ok(())
    }

    fn fatal(&mut self, message: &str) -> HarnessEvent {
        self.exited = true;
        HarnessEvent::ProtocolInvariant {
            message: message.into(),
            attempted_mode: None,
            confirmation_token: None,
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
