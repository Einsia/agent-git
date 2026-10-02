//! Native inbox delivery keeps the existing runtime as the sole transcript writer.
//!
//! The queue is the only capability implemented here. Queue acceptance does not grant live
//! steering, interruption, approvals or a second transcript writer. Codex receives messages
//! through `codex queue`; a Claude Code session that runs in another process receives them
//! through that process's messaging socket ([`super::claude_inbox`]).

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_MESSAGE: usize = 16 * 1024;

/// Probe the installed command without resuming a thread or submitting input.
/// Executable metadata keys the cache so an upgrade refreshes capability discovery.
pub async fn queue_available(codex: PathBuf) -> bool {
    use std::sync::{Arc, Mutex, OnceLock};
    type Key = (PathBuf, u64, Option<std::time::SystemTime>);
    type Cache = std::collections::HashMap<Key, Arc<tokio::sync::OnceCell<bool>>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let Ok(metadata) = std::fs::metadata(&codex) else {
        return false;
    };
    let key = (codex.clone(), metadata.len(), metadata.modified().ok());
    let cell = {
        let mut cache = CACHE.get_or_init(Mutex::default).lock().unwrap();
        cache.entry(key).or_default().clone()
    };
    *cell
        .get_or_init(|| async {
            let mut command =
                tokio::process::Command::from(crate::infra::background::command(codex));
            command
                .args(["queue", "--help"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
                Ok(Ok(output)) if output.status.success() => {
                    let help = String::from_utf8_lossy(&output.stdout);
                    help.contains("--thread") && help.contains("--message")
                }
                _ => false,
            }
        })
        .await
}

#[derive(Debug, Deserialize)]
pub struct Request {
    pub workspace_id: String,
    pub session_id: String,
    pub client_msg_id: String,
    pub message: String,
}

impl Request {
    pub fn validate(&self) -> crate::Result<()> {
        ensure!(
            valid_id(&self.session_id),
            "an exact native session UUID is required"
        );
        self.validate_message()
    }

    pub fn validate_message(&self) -> crate::Result<()> {
        ensure!(
            valid_id(&self.client_msg_id),
            "a client message UUID is required"
        );
        ensure!(
            !self.message.trim().is_empty() && self.message.len() <= MAX_MESSAGE,
            "the message must be nonempty and fit the native inbox limit"
        );
        Ok(())
    }
}

pub fn valid_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}

/// The runtime that receives the message.
pub(crate) enum Native {
    /// The Codex executable whose `queue` command delivers it.
    Codex(PathBuf),
    Claude(ClaudeTarget),
}

pub(crate) struct ClaudeTarget {
    /// The process found when the request was prepared; delivery requires the same one.
    pub(crate) live: super::claude_inbox::Live,
    pub(crate) registry: PathBuf,
    pub(crate) queue: PathBuf,
}

pub struct Prepared {
    pub(crate) source: Option<super::runtime_context::RuntimeContext>,
    pub(crate) authority: super::authority::Guard,
    pub(crate) confinement: Option<tokio::sync::watch::Receiver<super::Confinement>>,
    pub(crate) allow_dangerous: bool,
    pub request: Request,
    pub transcript: PathBuf,
    pub cwd: PathBuf,
    pub(crate) native: Native,
    pub hub: String,
    pub account: String,
    pub username: Option<String>,
    pub receipts: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    digest: String,
    status: String,
}

pub(crate) fn sync_directory(path: &Path) -> crate::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(super) fn open_regular(path: &Path) -> crate::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "native inbox input must be a regular file"
    );
    Ok(file)
}

pub(super) fn verify_transcript(path: &Path, session: &str) -> crate::Result<()> {
    verify_open_transcript(&mut open_regular(path)?, session)
}

pub(super) fn verify_open_transcript(file: &mut std::fs::File, session: &str) -> crate::Result<()> {
    let mut bytes = Vec::new();
    file.take(128 * 1024).read_to_end(&mut bytes)?;
    let header = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .context("missing transcript header")?;
    let header: Value = serde_json::from_slice(header)?;
    ensure!(
        header["type"] == "session_meta" && header["payload"]["id"] == session,
        "native transcript identity does not match the requested session"
    );
    Ok(())
}

impl Prepared {
    fn validate_source(&self) -> crate::Result<()> {
        self.authority
            .check()
            .map_err(|error| anyhow::anyhow!(error.message))?;
        if let Some(confinement) = &self.confinement {
            ensure!(
                confinement.has_changed().is_ok(),
                "workspace authority is unavailable"
            );
            super::policy::require_within(&self.cwd, &confinement.borrow().roots)?;
        }
        if let Some(context) = &self.source {
            let roots = super::policy::CanonicalRoots::from_untrusted([self.cwd.clone()]);
            let thread = context.locate(&self.request.session_id, &roots)?;
            ensure!(
                thread.cwd == self.cwd && thread.transcript == self.transcript,
                "native inbox source changed before delivery"
            );
            if !self.allow_dangerous {
                let mode = context.settings(&thread)?.permission_mode;
                ensure!(
                    mode.is_some() && mode != Some(crate::protocol::PermissionMode::Bypass),
                    "native permissions changed; only the owner can queue work in this session"
                );
            }
            super::local_goal::validate_header(
                &thread.transcript,
                &self.request.session_id,
                &self.cwd,
            )?;
        }
        Ok(())
    }

    /// The transcript still belongs to the requested session, and its runtime still accepts
    /// messages from this caller.
    async fn validate_native(&self) -> crate::Result<()> {
        let Native::Claude(_) = &self.native else {
            return verify_transcript(&self.transcript, &self.request.session_id);
        };
        ensure!(
            self.transcript
                .file_name()
                .is_some_and(|name| *name == *format!("{}.jsonl", self.request.session_id)),
            "native transcript identity does not match the requested session"
        );
        open_regular(&self.transcript)?;
        if !self.allow_dangerous {
            let transcript = self.transcript.clone();
            let unchecked = tokio::task::spawn_blocking(move || {
                super::claude_inbox::transcript_ran_unchecked(&transcript)
            })
            .await??;
            if unchecked {
                return Err(crate::protocol::RpcError::new(
                    crate::protocol::ErrorCode::DangerousSessionLocked,
                    "this Claude Code session has run without permission checks, so only its owner may send it messages",
                )
                .into());
            }
        }
        Ok(())
    }

    pub async fn deliver(self) -> crate::Result<Value> {
        self.request.validate()?;
        self.validate_source()?;
        self.validate_native().await?;
        std::fs::create_dir_all(&self.receipts)?;
        ensure!(
            !std::fs::symlink_metadata(&self.receipts)?
                .file_type()
                .is_symlink(),
            "native receipt directory cannot be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.receipts, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        crate::infra::windows_security::private_directory(&self.receipts)?;
        sync_directory(&self.receipts)?;
        if let Some(parent) = self.receipts.parent() {
            sync_directory(parent)?;
        }
        let scope = if let Some(context) = &self.source {
            serde_json::to_vec(&(
                "source-native-inbox-v1",
                &self.hub,
                &self.request.workspace_id,
                &context.source.source_id,
                &self.request.session_id,
                &self.account,
                &self.request.client_msg_id,
            ))?
        } else {
            serde_json::to_vec(&(
                &self.hub,
                &self.request.workspace_id,
                &self.request.session_id,
                &self.account,
                &self.request.client_msg_id,
            ))?
        };
        let key = hex::encode(Sha256::digest(scope));
        let path = self.receipts.join(format!("{key}.json"));
        let digest = hex::encode(Sha256::digest(self.request.message.as_bytes()));
        // A durable receipt is authoritative for retries. Return it before probing the native
        // executable, because the original submission may have succeeded while its response was
        // lost and the executable can be temporarily unavailable during recovery.
        if std::fs::symlink_metadata(&path).is_ok() {
            let mut bytes = Vec::new();
            open_regular(&path)?.take(4096).read_to_end(&mut bytes)?;
            let receipt: Receipt = serde_json::from_slice(&bytes)?;
            ensure!(
                receipt.digest == digest,
                "client message id already belongs to different content"
            );
            return Ok(json!({
                "client_msg_id": self.request.client_msg_id,
                "status": receipt.status
            }));
        }
        match &self.native {
            Native::Codex(codex) => ensure!(
                queue_available(codex.clone()).await,
                "Codex native queue is unavailable"
            ),
            Native::Claude(target) => {
                if !super::claude_inbox::discover_in(&target.registry, &self.request.session_id)
                    .is_some_and(|live| live.same_process(&target.live))
                {
                    return Err(crate::protocol::RpcError::new(
                        crate::protocol::ErrorCode::RuntimeUnavailable,
                        "this Claude Code session is no longer running in the process that was found",
                    )
                    .into());
                }
            }
        }
        self.validate_source()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Persist the claim before native submission; uncertain delivery never permits a replay.
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&serde_json::to_vec(&Receipt {
                    digest: digest.clone(),
                    status: "unknown".into(),
                })?)?;
                file.sync_all()?;
                sync_directory(&self.receipts)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut bytes = Vec::new();
                open_regular(&path)?.take(4096).read_to_end(&mut bytes)?;
                let receipt: Receipt = serde_json::from_slice(&bytes)?;
                ensure!(
                    receipt.digest == digest,
                    "client message id already belongs to different content"
                );
                return Ok(
                    json!({"client_msg_id":self.request.client_msg_id,"status":receipt.status}),
                );
            }
            Err(error) => return Err(error.into()),
        }
        let message = self.username.as_ref().map_or_else(
            || self.request.message.clone(),
            |username| {
                format!(
                    "[AgentGit workspace message from @{username}]\n{}",
                    self.request.message
                )
            },
        );
        match &self.native {
            Native::Codex(codex) => self.queue_codex(codex, &message).await?,
            Native::Claude(target) => {
                if let Err(error) = self.send_claude(target, &key, &message).await {
                    return Err(match error {
                        super::claude_inbox::SendError::NotDelivered(error) => {
                            // Nothing reached the session, so the claim guards no possible
                            // delivery and a retry may send the message.
                            std::fs::remove_file(&path)?;
                            sync_directory(&self.receipts)?;
                            error.context("Claude Code did not receive the message; retry when the session is running")
                        }
                        super::claude_inbox::SendError::Uncertain(error) => error.context(
                            "Claude Code did not confirm delivery; retry with the same client message id",
                        ),
                    });
                }
            }
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.receipts)?;
        file.write_all(&serde_json::to_vec(&Receipt {
            digest,
            status: "queued".into(),
        })?)?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        sync_directory(&self.receipts)?;
        Ok(json!({"client_msg_id":self.request.client_msg_id,"status":"queued"}))
    }

    /// The owner's message passes through the session's queue, where a relay may deliver it as
    /// the session itself; anyone else's goes straight to the socket under the peer token.
    /// Authority is checked at the moment the message becomes visible to a relay or leaves.
    async fn send_claude(
        &self,
        target: &ClaudeTarget,
        key: &str,
        message: &str,
    ) -> Result<(), super::claude_inbox::SendError> {
        use super::claude_inbox::SendError::NotDelivered;
        // Claude Code holds a peer message for approval by the session's live permission mode,
        // which the transcript gate cannot see; a relay would deliver past that hold.
        let queue = self
            .allow_dangerous
            .then(|| super::claude_inbox::Queue::open(target.queue.clone()))
            .transpose()
            .map_err(NotDelivered)?;
        self.validate_source().map_err(NotDelivered)?;
        let mut enqueued = None;
        let admitted = {
            let confinement = self.confinement.as_ref().map(|receiver| receiver.borrow());
            if let Some(confinement) = &confinement {
                super::policy::require_within(&self.cwd, &confinement.roots)
                    .map_err(|error| NotDelivered(error.into()))?;
            }
            self.authority.admit(|| {
                enqueued = queue.as_ref().map(|queue| {
                    queue.enqueue(
                        key,
                        &self.request.client_msg_id,
                        message,
                        &target.live,
                        self.allow_dangerous,
                    )
                });
                true
            })
        };
        if !admitted {
            return Err(NotDelivered(anyhow::anyhow!(
                "request authority expired before native delivery"
            )));
        }
        let Some(queue) = queue else {
            return super::claude_inbox::send_direct(
                &target.live,
                &self.request.client_msg_id,
                message,
            )
            .await;
        };
        enqueued.expect("accepted native delivery attempts the enqueue")?;
        super::claude_inbox::forward(&queue, &target.live, key).await
    }

    async fn queue_codex(&self, codex: &Path, message: &str) -> crate::Result<()> {
        let mut command = tokio::process::Command::from(crate::infra::background::command(codex));
        command
            .args([
                "queue",
                "--thread",
                &self.request.session_id,
                "--message",
                message,
            ])
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(context) = &self.source {
            command.env("CODEX_HOME", &context.source.home);
        }
        self.validate_source()?;
        let mut child = {
            let confinement = self.confinement.as_ref().map(|receiver| receiver.borrow());
            if let Some(confinement) = &confinement {
                super::policy::require_within(&self.cwd, &confinement.roots)?;
            }
            let mut spawned = None;
            let admitted = self.authority.admit(|| {
                spawned = Some(command.spawn());
                true
            });
            ensure!(admitted, "request authority expired before native delivery");
            spawned
                .expect("accepted native delivery attempts process creation")
                .context("could not start the native Codex inbox; delivery is unknown")?
        };
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .context(
                "native inbox did not confirm delivery; retry with the same client message id",
            )??;
        ensure!(
            status.success(),
            "native inbox did not confirm delivery; check that Codex supports `codex queue` and retry with the same client message id"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(root: &Path, session: &str, client_id: &str) -> Prepared {
        Prepared {
            source: None,
            authority: Default::default(),
            confinement: None,
            allow_dangerous: true,
            request: Request {
                workspace_id: "workspace".into(),
                session_id: session.into(),
                client_msg_id: client_id.into(),
                message: "Please investigate this bug.".into(),
            },
            transcript: root.join("transcript.jsonl"),
            cwd: root.into(),
            native: Native::Codex(root.join("codex")),
            hub: "https://hub.test".into(),
            account: "member".into(),
            username: Some("collaborator".into()),
            receipts: root.join("receipts"),
        }
    }

    #[test]
    fn identity_and_size_are_validated_before_native_delivery() {
        let mut value = prepared(
            Path::new("unused"),
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
        );
        assert!(value.request.validate().is_ok());
        value.request.session_id = "a session name".into();
        assert!(value.request.validate().is_err());
        value.request.session_id = uuid::Uuid::new_v4().to_string();
        value.request.message = "x".repeat(MAX_MESSAGE + 1);
        assert!(value.request.validate().is_err());
    }

    /// A Claude Code session running in another process receives the message through its own
    /// socket, so that process stays the only transcript writer. This pins the exact frames, one
    /// delivery per client message id, that a non-owner's message goes out under the peer token
    /// even while a relay runs, that the owner's goes through the relay under the session's own
    /// token, that a held lock which no relay acts on falls back to the peer token, and that a
    /// transcript which ever bypassed permission checks takes messages only from the owner.
    /// Resending on retry, queueing a non-owner's message where a relay sends it as the session,
    /// counting a held lock as delivery, or judging only a later mode each fails here.
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_sessions_get_each_message_once_and_bypass_history_needs_the_owner() {
        use crate::rc::claude_inbox::fixture;
        async fn next(
            connections: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<Value>>,
        ) -> Vec<Value> {
            tokio::time::timeout(Duration::from_secs(5), connections.recv())
                .await
                .unwrap()
                .unwrap()
        }
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let registry = root.path().join("sessions");
        let socket = root.path().join("claude.sock");
        let mut connections = fixture::listen(&socket);
        let peer = "00112233445566778899aabbccddeeff";
        fixture::register(&registry, std::process::id(), &session, &socket, Some(peer));
        let transcript = root.path().join(format!("{session}.jsonl"));
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({"type":"user","sessionId":session,"permissionMode":"default",
                    "message":{"role":"user","content":"Start here."}})
            ),
        )
        .unwrap();
        let live = crate::rc::claude_inbox::discover_in(&registry, &session).unwrap();
        let queue = root.path().join("queue");
        let send = |client: &str, owner: bool| {
            let mut request = prepared(root.path(), &session, client);
            request.transcript = transcript.clone();
            request.allow_dangerous = owner;
            request.native = Native::Claude(ClaudeTarget {
                live: live.clone(),
                registry: registry.clone(),
                queue: queue.clone(),
            });
            request.deliver()
        };
        let frames = |client: &str, token: &str| {
            vec![
                json!({"type":"auth","token":token}),
                json!({"type":"user","session_id":session,"msg_id":client,"from":"agit-rc",
                    "priority":"next","message":{"role":"user",
                    "content":"[AgentGit workspace message from @collaborator]\nPlease investigate this bug."}}),
            ]
        };

        let first = uuid::Uuid::new_v4().to_string();
        assert_eq!(send(&first, false).await.unwrap()["status"], "queued");
        assert_eq!(next(&mut connections).await, frames(&first, peer));
        // The retry answers from its receipt; the next connection belongs to a later message.
        assert_eq!(send(&first, false).await.unwrap()["status"], "queued");

        let child = "0123456789abcdef0123456789abcdef";
        let relay = fixture::relay(&registry, &queue, &session, &socket, child);
        let inbox = crate::rc::claude_inbox::Queue::open(queue.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !inbox.relay_active().unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the relay starts");
        let peers = uuid::Uuid::new_v4().to_string();
        assert_eq!(send(&peers, false).await.unwrap()["status"], "queued");
        assert_eq!(
            next(&mut connections).await,
            frames(&peers, peer),
            "a non-owner's message never passes through the relay"
        );
        let owners = uuid::Uuid::new_v4().to_string();
        assert_eq!(send(&owners, true).await.unwrap()["status"], "queued");
        assert_eq!(
            next(&mut connections).await,
            frames(&owners, child),
            "the relay delivers the owner's message as the session"
        );
        relay.abort();
        let _ = relay.await;

        let held = inbox.try_lock().unwrap().unwrap();
        let fallback = uuid::Uuid::new_v4().to_string();
        assert_eq!(send(&fallback, true).await.unwrap()["status"], "queued");
        assert_eq!(
            next(&mut connections).await,
            frames(&fallback, peer),
            "a lock no relay acts on leaves the message to the daemon"
        );
        assert_eq!(std::fs::read_dir(queue.join("pending")).unwrap().count(), 0);
        drop(held);

        let mut history = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(
            history,
            "{}",
            json!({"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":session})
        )
        .unwrap();
        writeln!(
            history,
            "{}",
            json!({"type":"permission-mode","permissionMode":"default","sessionId":session})
        )
        .unwrap();
        let refused = send(&uuid::Uuid::new_v4().to_string(), false)
            .await
            .unwrap_err();
        assert_eq!(
            refused
                .downcast_ref::<crate::protocol::RpcError>()
                .unwrap()
                .code,
            crate::protocol::ErrorCode::DangerousSessionLocked as i32
        );
        let owner = uuid::Uuid::new_v4().to_string();
        assert_eq!(send(&owner, true).await.unwrap()["status"], "queued");
        assert_eq!(next(&mut connections).await, frames(&owner, peer));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copied_threads_queue_in_their_enrolled_home_and_revocation_prevents_delivery() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let executable = root.path().join("codex");
        std::fs::write(&executable,
            "#!/bin/sh\nif [ \"$2\" = --help ]; then echo --thread --message; exit 0; fi\nprintf '%s\n' \"$@\" >> \"$CODEX_HOME/calls\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry =
            super::super::runtime_sources::Registry::at(root.path().join("registry")).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let client = uuid::Uuid::new_v4().to_string();
        let mut contexts = Vec::new();
        for name in ["profile-a", "profile-b"] {
            let home = root.path().join(name);
            std::fs::create_dir(&home).unwrap();
            let home = home.canonicalize().unwrap();
            let transcript = home.join("history.jsonl");
            std::fs::write(
                &transcript,
                format!(
                    "{}\n",
                    json!({"type":"session_meta","payload":{"id":native,"cwd":project}})
                ),
            )
            .unwrap();
            let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER)").unwrap();
            db.execute(
                "INSERT INTO threads VALUES (?1,?2,?3,'Preview','cli',1,0)",
                rusqlite::params![native, transcript.to_str(), project.to_str()],
            )
            .unwrap();
            let source = registry
                .register(&home, Some(&executable), None, None)
                .unwrap();
            contexts.push(
                super::super::runtime_context::RuntimeContext::resolve(
                    &registry,
                    &source.source_id,
                )
                .unwrap(),
            );
        }
        let request = |context: &super::super::runtime_context::RuntimeContext| {
            let mut request = prepared(root.path(), &native, &client);
            request.cwd = project.clone();
            request.transcript = context.source.home.join("history.jsonl");
            request.source = Some(context.clone());
            request
        };
        for context in &contexts {
            assert_eq!(
                request(context).deliver().await.unwrap()["status"],
                "queued"
            );
            assert_eq!(
                request(context).deliver().await.unwrap()["status"],
                "queued"
            );
            let calls = std::fs::read_to_string(context.source.home.join("calls")).unwrap();
            assert_eq!(calls.matches("--thread").count(), 1);
            assert!(calls.contains(&native));
        }
        let mut uncertain_policy = request(&contexts[1]);
        uncertain_policy.allow_dangerous = false;
        uncertain_policy.request.client_msg_id = uuid::Uuid::new_v4().to_string();
        assert!(uncertain_policy.deliver().await.is_err());
        let (binding, current) = tokio::sync::watch::channel(super::super::Confinement {
            roots: super::super::policy::CanonicalRoots::from_untrusted([project.clone()]),
            ..Default::default()
        });
        let mut unbound = request(&contexts[1]);
        unbound.confinement = Some(current);
        binding.send_replace(Default::default());
        assert!(unbound.deliver().await.is_err());
        let removed = request(&contexts[0]);
        registry.remove(&contexts[0].source.source_id).unwrap();
        assert!(removed.deliver().await.is_err());
        assert_eq!(
            request(&contexts[1]).deliver().await.unwrap()["status"],
            "queued"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn receipts_deduplicate_retries_and_bind_content_to_the_authenticated_member() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            root.path().join("transcript.jsonl"),
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":session}})
            ),
        )
        .unwrap();
        let script = root.path().join("codex");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$2\" = --help ]; then echo --thread --message; exit 0; fi\nprintf '%s\\n' \"$@\" >> native-calls\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            prepared(root.path(), &session, &id)
                .deliver()
                .await
                .unwrap()["status"],
            "queued"
        );
        let first = std::fs::read(root.path().join("native-calls")).unwrap();
        assert!(String::from_utf8_lossy(&first).contains(&session));
        assert!(String::from_utf8_lossy(&first).contains("@collaborator"));
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o000)).unwrap();
        prepared(root.path(), &session, &id)
            .deliver()
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("native-calls")).unwrap(),
            first
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut changed = prepared(root.path(), &session, &id);
        changed.request.message.push('!');
        assert!(changed.deliver().await.is_err());
        let mut other = prepared(root.path(), &session, &id);
        other.account = "another member".into();
        other.deliver().await.unwrap();
        assert!(
            std::fs::read(root.path().join("native-calls"))
                .unwrap()
                .len()
                > first.len()
        );
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$2\" = --help ]; then echo --thread --message; exit 0; fi\nprintf '%s\\n' attempted >> native-calls\nexit 1\n",
        )
        .unwrap();
        let unknown_id = uuid::Uuid::new_v4().to_string();
        assert!(
            prepared(root.path(), &session, &unknown_id)
                .deliver()
                .await
                .is_err()
        );
        let calls = std::fs::read(root.path().join("native-calls")).unwrap();
        assert_eq!(
            prepared(root.path(), &session, &unknown_id)
                .deliver()
                .await
                .unwrap()["status"],
            "unknown"
        );
        assert_eq!(
            std::fs::read(root.path().join("native-calls")).unwrap(),
            calls
        );
        let wrong_session = uuid::Uuid::new_v4().to_string();
        assert!(
            prepared(root.path(), &wrong_session, &id)
                .deliver()
                .await
                .is_err()
        );
    }
}
