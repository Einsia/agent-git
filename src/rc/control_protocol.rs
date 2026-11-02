//! Shared local daemon control messages.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    StopIfIdle {
        instance_id: String,
        build_id: String,
    },
    ReloadSecrets,
    /// A Claude Code terminal loaded this conversation; `pid` is its process, and `loaded` the
    /// branch its plugin saw it continue, once it has.
    TerminalOpened {
        thread_id: String,
        pid: u32,
        plugin: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loaded: Option<super::lease::Loaded>,
    },
    /// The headless writer of this conversation finished a turn on record `leaf`.
    WriterLeaf {
        thread_id: String,
        leaf: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Stopping,
    Busy {
        blockers: Vec<String>,
    },
    InstanceChanged,
    SecretsReloaded {
        generation: u64,
        rules: usize,
    },
    /// The terminal's lease state, absent when the daemon does not run this conversation, and
    /// the line the terminal shows while its own input must wait.
    TerminalOpened {
        state: Option<crate::protocol::LeaseState>,
        notice: Option<String>,
    },
    Recorded,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<super::build_identity::DaemonIdentity>,
    pub pid: u32,
    pub hub: String,
    pub online: bool,
    pub uptime_secs: u64,
    pub agit_version: String,
    pub sessions: Vec<SessionLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLine {
    pub session_id: String,
    pub runtime: String,
    pub status: String,
    pub last_seq: u64,
}
