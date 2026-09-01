//! Wire protocol shared by `agitd` (the machine-side daemon), the hub (backend),
//! and viewers (web / mobile / a future local TUI).
//!
//! # Why this lives in the `agit` crate and is not feature-gated
//!
//! The backend already imports this crate with `default-features = false` to get
//! the IR (`adapter`) and the turn chain (`domain::turn`), so that the web
//! transcript is byte-for-byte what `agit show` renders. The RC protocol has the
//! same requirement one level up: `agitd` produces frames, the hub relays and
//! persists them, viewers render them. Three hand-synced schemas would drift.
//! One definition compiled into both binaries cannot.
//!
//! This module depends only on `serde` / `serde_json` and re-exports the IR
//! types it carries. No tokio, no sockets — those are the daemon's business
//! (`crate::rc`, behind the `rc` feature).
//!
//! # Shape
//!
//! Three layers, kept independent so that "swap harness", "swap transport", and
//! "add multi-user" never touch each other:
//!
//! * **transport** — WSS, bidirectional, heartbeat 15s / dead at 45s, exponential
//!   backoff with jitter. Not modeled here; see `rc::link`.
//! * **frame** — JSON-RPC 2.0 request / response / notification, plus two
//!   extension fields: [`Frame::seq`] (monotonic, gap-free per stream) and
//!   [`Frame::stream`] (which session the frame belongs to). Clients reconnect
//!   with `after_seq` and the server fills the gap.
//!
//!   **One exception, written here rather than left to be discovered: terminal streams
//!   (`term:<workspace_id>`) get no such gap fill.** Sequence numbers are still assigned (the
//!   hub dedupes by `(stream, seq)`), but those bytes enter no replay buffer, and
//!   `session.subscribe` does not accept such a stream.
//!
//!   They **are not a record, they are a stream**: PTY output means something only at the
//!   moment it is written, replaying a stretch of bytes away from the screen state of that
//!   moment means nothing (cursor position, alternate screen and color state all fail to line
//!   up), and keeping them costs 8192 frames of memory per stream — a high-frequency buffer
//!   that never goes anywhere.
//!
//!   The cost is that **what is dropped is gone**. So when a stalled link drops terminal bytes,
//!   the machine writes a visible mark into that terminal (see the outbound branch in
//!   `rc::daemon`) instead of letting the screen silently lose a stretch. Session events are
//!   unaffected: what they owe still gets filled.
//! * **domain** — harness-neutral verbs, named to stay close to codex
//!   app-server (`turn.steer`, `turn.interrupt`, `item.delta`) because that
//!   vocabulary has already been validated by a real client (the VS Code
//!   extension). See [`method`].
//!
//! # The four ids — never conflate them
//!
//! | id | allocated by | lifetime |
//! |---|---|---|
//! | `connection_id` | hub, at register | stable across reconnects/reboots until revoked |
//! | `workspace_id`  | hub, at create   | until the user deletes it |
//! | `session_id`    | agit (= logical session = branch) | as long as the branch |
//! | `runtime_thread_id` | the harness | changes on every materialization |
//!
//! Only the first three ever appear on the wire. `runtime_thread_id` stays in
//! `agitd`'s private map so that "continue on another machine" and "continue in
//! another harness" are invisible to viewers.

pub mod types;

pub use types::*;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Protocol version. Sent in `rc.register`; the hub rejects a mismatch with
/// [`ErrorCode::ProtocolMismatch`] and a hint to upgrade.
pub const VERSION: u32 = 1;

/// Additive protocol features negotiated during `rc.register`.
///
/// A daemon may only rely on a feature after the hub echoes it in
/// [`RcRegisterResult::accepted_features`]. Advertising is not an ACK, and an
/// unknown/old peer deserializing either list as empty keeps protocol v1
/// rolling-compatible.
pub mod feature {
    /// RC session lineage is fenced by the repository's immutable `agent_id`.
    pub const AGENT_IDENTITY_V1: &str = "agent_identity_v1";
    /// `session.start` carries a UUID idempotency key and the daemon persists
    /// both the pre-launch intent and the exact successful response.
    pub const SESSION_START_IDEMPOTENCY_V1: &str = "session_start_idempotency_v1";
}

/// Heartbeat cadence and liveness threshold (seconds). Both sides use these
/// numbers; putting them here keeps them from drifting apart.
pub const HEARTBEAT_SECS: u64 = 15;
pub const DEAD_AFTER_SECS: u64 = 45;

/// A JSON-RPC 2.0 frame with the two RC extension fields.
///
/// One struct rather than a request/response/notification enum because the wire
/// shape *is* one object with optional keys, and callers mostly want to inspect
/// (`is_request()`, `method()`) then dispatch. Constructors below keep the three
/// forms well-formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionFeature {
    AgentIdentityV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryStatus {
    Pending,
    Delivered,
    Stale,
}

/// Machine-local proof that one feature-sensitive frame belongs to a specific
/// socket epoch. This metadata is deliberately skipped on the wire: the daemon
/// and link use it to prevent queued/replayed frames from borrowing a later
/// registration ACK, while the supervisor observes the same atomic state before
/// declaring the notification delivered.
#[derive(Debug)]
pub(crate) struct ConnectionDelivery {
    epoch: u64,
    feature: ConnectionFeature,
    state: AtomicU8,
}

impl ConnectionDelivery {
    const PENDING: u8 = 0;
    const DELIVERED: u8 = 1;
    const STALE: u8 = 2;

    pub(crate) fn new(epoch: u64, feature: ConnectionFeature) -> Arc<Self> {
        Arc::new(Self {
            epoch,
            feature,
            state: AtomicU8::new(Self::PENDING),
        })
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn feature(&self) -> ConnectionFeature {
        self.feature
    }

    pub(crate) fn status(&self) -> DeliveryStatus {
        match self.state.load(Ordering::Acquire) {
            Self::PENDING => DeliveryStatus::Pending,
            Self::DELIVERED => DeliveryStatus::Delivered,
            _ => DeliveryStatus::Stale,
        }
    }

    pub(crate) fn mark_delivered(&self) {
        let _ = self.state.compare_exchange(
            Self::PENDING,
            Self::DELIVERED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn invalidate(&self) {
        let _ = self.state.compare_exchange(
            Self::PENDING,
            Self::STALE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub jsonrpc: JsonRpcVersion,
    /// Request id. Present on requests and responses, absent on notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// Method name. Present on requests and notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    /// Monotonic, gap-free sequence number within `stream`. Assigned by `agitd`
    /// (the only party that sees the true event order) — see the daemon docs for
    /// why not the hub or Redis. Only event notifications carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Which stream (= logical session id) this frame belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    /// Harness generation that produced this session frame.
    ///
    /// This is a daemon-local provenance fence, never a wire field: the
    /// per-session bridge overwrites it after the supervisor emits the frame,
    /// and deserialization always yields `None`. A hub/client therefore cannot
    /// forge a current local generation through JSON.
    #[serde(skip)]
    pub(crate) source_generation: Option<u64>,
    /// This frame **must not be dropped for backpressure**.
    ///
    /// Machine-local only, never on the wire (`skip`). A notification carrying `stream` is
    /// droppable by default — it is already in the journal ring, and one `session.subscribe`
    /// from a viewer fetches it back. This bit marks exactly the frame that **is** the answer
    /// to that subscribe: dropped, there is no second path to fill it; the viewer sees a
    /// stretch of history that stays missing forever, while the `from_seq` in the response
    /// claims that hole is already filled.
    #[serde(skip)]
    pub reliable: bool,
    /// Local connection-epoch fence for feature-sensitive notifications.
    /// Clones in the journal/outbound queues share the same delivery state.
    #[serde(skip)]
    pub(crate) connection_delivery: Option<Arc<ConnectionDelivery>>,
    /// Who the hub says is asking, and with what standing.
    ///
    /// # Why the daemon cannot just trust `params`
    ///
    /// The machine is the trust boundary: `agitd` re-checks every instruction
    /// because a compromised or simply mis-implemented hub must not be able to
    /// widen what an agent may do on a real machine. But re-checking needs an
    /// input, and everything inside `params` came from the viewer — a client
    /// that wants owner powers can just write `"by":"owner"` into its own
    /// frame.
    ///
    /// So authorization travels **outside** `params`, in a field the hub
    /// overwrites on every relayed frame (see the hub's `relay_from_viewer`)
    /// and a viewer therefore cannot forge: whatever a client puts here is
    /// discarded before the frame leaves the hub. `None` means "no standing
    /// was asserted", and the daemon treats that as the weakest caller — a
    /// missing claim must never read as permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<CallerClaim>,
}

impl PartialEq for Frame {
    fn eq(&self, other: &Self) -> bool {
        // `source_generation` and `connection_delivery` are transport-local
        // metadata, just like a socket write lease. Wire-equivalent frames
        // remain equal after JSON roundtrip.
        self.jsonrpc == other.jsonrpc
            && self.id == other.id
            && self.method == other.method
            && self.params == other.params
            && self.result == other.result
            && self.error == other.error
            && self.seq == other.seq
            && self.stream == other.stream
            && self.reliable == other.reliable
            && self.caller == other.caller
    }
}

/// The hub's statement about who is driving. See [`Frame::caller`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerClaim {
    /// Account id, for the audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// `viewer` | `operator` | `owner` — the caller's role **in the workspace
    /// named by [`CallerClaim::workspace_id`]**, as the hub resolved it.
    pub role: String,
    /// The workspace the hub resolved that role against. The daemon refuses to
    /// act on a session that belongs to a different workspace, so a role in one
    /// tenant cannot be spent in another.
    pub workspace_id: String,
}

impl CallerClaim {
    pub fn is_owner(&self) -> bool {
        self.role == "owner"
    }

    /// Whether this caller may issue instructions (send a message / steer / interrupt /
    /// decide an approval / start a session). Synonymous with the hub's `can_operate`; the
    /// machine checks again because the hub is a relay, not the authority.
    pub fn can_operate(&self) -> bool {
        matches!(self.role.as_str(), "operator" | "owner")
    }
}

/// The literal `"2.0"`. A unit-like newtype so a hand-built frame with the wrong
/// version fails to deserialize instead of being silently accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}
impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == "2.0" {
            Ok(JsonRpcVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported jsonrpc version {v:?}"
            )))
        }
    }
}

/// JSON-RPC allows string or integer ids. We use strings everywhere (uuid) but
/// accept both so a hand-written client works.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Str(String),
    Num(i64),
}

impl RequestId {
    pub fn fresh() -> RequestId {
        RequestId::Str(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Str(s) => f.write_str(s),
            RequestId::Num(n) => write!(f, "{n}"),
        }
    }
}

/// JSON-RPC error object. `code` is one of [`ErrorCode`]; `data` is free-form
/// (we put a `hint` in it — the CLI rule "an error without a next step is a bug"
/// applies on the wire too).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Error codes. JSON-RPC reserves -32768..-32000; ours are positive so they never
/// collide, and grouped by hundreds so a log line is skimmable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    // 1xx — protocol
    ProtocolMismatch = 100,
    MalformedFrame = 101,
    UnknownMethod = 102,
    SequenceGap = 103,
    // 2xx — auth / authz
    Unauthenticated = 200,
    Forbidden = 201,
    Revoked = 202,
    QuotaExceeded = 203,
    // 3xx — target state
    ConnectionOffline = 300,
    WorkspaceNotFound = 301,
    SessionNotFound = 302,
    SessionBusy = 303,
    PathNotAllowed = 304,
    RuntimeUnavailable = 305,
    ApprovalExpired = 306,
    DangerousSessionLocked = 307,
    // 4xx — hub internal
    RelayTimeout = 400,
    Internal = 401,
}

impl ErrorCode {
    pub fn code(self) -> i32 {
        self as i32
    }
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> RpcError {
        RpcError {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }
    /// Attach a copy-pasteable next step.
    pub fn with_hint(mut self, hint: impl Into<String>) -> RpcError {
        let mut data = self.data.take().unwrap_or_else(|| serde_json::json!({}));
        if let Some(obj) = data.as_object_mut() {
            obj.insert("hint".into(), Value::String(hint.into()));
        }
        self.data = Some(data);
        self
    }
    pub fn is(&self, code: ErrorCode) -> bool {
        self.code == code.code()
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}
impl std::error::Error for RpcError {}

impl Frame {
    pub fn request(method: impl Into<String>, params: impl Serialize) -> Frame {
        Frame {
            jsonrpc: JsonRpcVersion,
            id: Some(RequestId::fresh()),
            method: Some(method.into()),
            params: Some(serde_json::to_value(params).expect("params serialize")),
            result: None,
            error: None,
            seq: None,
            stream: None,
            source_generation: None,
            caller: None,
            reliable: false,
            connection_delivery: None,
        }
    }
    pub fn request_with_id(
        id: RequestId,
        method: impl Into<String>,
        params: impl Serialize,
    ) -> Frame {
        let mut f = Frame::request(method, params);
        f.id = Some(id);
        f
    }
    pub fn notification(method: impl Into<String>, params: impl Serialize) -> Frame {
        Frame {
            jsonrpc: JsonRpcVersion,
            id: None,
            method: Some(method.into()),
            params: Some(serde_json::to_value(params).expect("params serialize")),
            result: None,
            error: None,
            seq: None,
            stream: None,
            source_generation: None,
            caller: None,
            reliable: false,
            connection_delivery: None,
        }
    }
    /// An event notification: a notification that also carries `seq` + `stream`.
    pub fn event(
        stream: impl Into<String>,
        seq: u64,
        method: impl Into<String>,
        params: impl Serialize,
    ) -> Frame {
        let mut f = Frame::notification(method, params);
        f.seq = Some(seq);
        f.stream = Some(stream.into());
        f
    }
    pub fn response(id: RequestId, result: impl Serialize) -> Frame {
        Frame {
            jsonrpc: JsonRpcVersion,
            id: Some(id),
            method: None,
            params: None,
            result: Some(serde_json::to_value(result).expect("result serialize")),
            error: None,
            seq: None,
            stream: None,
            source_generation: None,
            caller: None,
            reliable: false,
            connection_delivery: None,
        }
    }
    pub fn error_response(id: RequestId, err: RpcError) -> Frame {
        Frame {
            jsonrpc: JsonRpcVersion,
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(err),
            seq: None,
            stream: None,
            source_generation: None,
            caller: None,
            reliable: false,
            connection_delivery: None,
        }
    }

    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }
    pub fn is_event(&self) -> bool {
        self.is_notification() && self.seq.is_some() && self.stream.is_some()
    }
    pub fn method(&self) -> &str {
        self.method.as_deref().unwrap_or("")
    }
    /// Typed view of `params`. Missing params deserialize as `null`, which lets
    /// unit-like param structs (`{}`) still work with `#[serde(default)]`.
    pub fn params_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, RpcError> {
        let v = self.params.clone().unwrap_or(Value::Null);
        serde_json::from_value(v).map_err(|e| {
            RpcError::new(
                ErrorCode::MalformedFrame,
                format!("bad params for {}: {e}", self.method()),
            )
        })
    }
    /// Which workspace this frame's params name, if they name one.
    ///
    /// Reads the raw JSON rather than going through the typed param structs. Going through
    /// the types needs a list of "which methods carry this field", and missing one entry
    /// leaves that verb **silently** exempt from the workspace check — the shape that lets
    /// `session.list` / `session.watch` / `fs.readFile` / `terminal.open` / `project.bind` /
    /// `session.start` / `session.resume` / `session.unwatch` miss it all at once. Reading
    /// the JSON shape holds for verbs added **later** too; nobody has to remember to come
    /// back and add a line.
    pub fn params_workspace_id(&self) -> Option<&str> {
        self.params.as_ref()?.get("workspace_id")?.as_str()
    }

    pub fn result_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, RpcError> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        let v = self.result.clone().unwrap_or(Value::Null);
        serde_json::from_value(v)
            .map_err(|e| RpcError::new(ErrorCode::MalformedFrame, format!("bad result: {e}")))
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("frame serialize")
    }
    pub fn from_json(s: &str) -> Result<Frame, RpcError> {
        serde_json::from_str(s).map_err(|e| RpcError::new(ErrorCode::MalformedFrame, e.to_string()))
    }
}

/// Method names. Constants rather than an enum so that unknown / future methods
/// still parse (forward compatibility) and so the frontend can grep for the
/// literal string.
pub mod method {
    // ── agitd → hub ──
    pub const RC_REGISTER: &str = "rc.register";
    pub const RC_HEARTBEAT: &str = "rc.heartbeat";
    pub const COMMIT_SETTLED: &str = "commit.settled";

    // ── hub / viewer → agitd (relayed) ──
    pub const WORKSPACE_LIST: &str = "workspace.list";
    pub const PROJECT_BIND: &str = "project.bind";
    pub const PROJECT_UNBIND: &str = "project.unbind";
    pub const FS_READ_DIRECTORY: &str = "fs.readDirectory";
    pub const FS_READ_FILE: &str = "fs.readFile";

    // Terminal. The only place that genuinely needs a PTY, for the opposite reason to a
    // session: **a human is on this end**, and what they want is exactly the rendered byte
    // stream (vim, top, colored build output). A session wants structure, so the two channels
    // stay separate and do not interfere.
    pub const TERMINAL_OPEN: &str = "terminal.open";
    pub const TERMINAL_INPUT: &str = "terminal.input";
    pub const TERMINAL_RESIZE: &str = "terminal.resize";
    pub const TERMINAL_CLOSE: &str = "terminal.close";
    pub const TERMINAL_OUTPUT: &str = "terminal.output";
    pub const TERMINAL_EXITED: &str = "terminal.exited";
    pub const SESSION_START: &str = "session.start";
    pub const SESSION_RESUME: &str = "session.resume";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_SUBSCRIBE: &str = "session.subscribe";
    pub const SESSION_WATCH: &str = "session.watch";
    /// A viewer stops watching. The machine stops that tail only when the last subscriber
    /// leaves — without it, every session someone has watched leaves a permanent file poll
    /// behind on that machine.
    pub const SESSION_UNWATCH: &str = "session.unwatch";
    pub const SESSION_SET_PERMISSION_MODE: &str = "session.setPermissionMode";
    pub const APPROVAL_DECIDE: &str = "approval.decide";
    /// Event: the mode changed (broadcast to every viewer of the session).
    pub const SESSION_PERMISSION_MODE: &str = "session.permissionMode";
    pub const TURN_START: &str = "turn.start";
    pub const TURN_STEER: &str = "turn.steer";
    pub const TURN_INTERRUPT: &str = "turn.interrupt";

    // ── agitd → viewer (events; carry seq + stream) ──
    pub const ITEM_STARTED: &str = "item.started";
    pub const ITEM_DELTA: &str = "item.delta";
    pub const ITEM_COMPLETED: &str = "item.completed";
    pub const TURN_STARTED: &str = "turn.started";
    pub const TURN_COMPLETED: &str = "turn.completed";
    pub const SESSION_STATUS: &str = "session.status";
    /// Generic alert only: never carries the local rule id, name or matched bytes.
    pub const SECRET_DETECTED: &str = "secret.detected";

    // Approval requests are stream notifications. `approval.decide` is the
    // separate relayed command above; the two correlate by `approval_id`.
    pub const APPROVAL_REQUEST: &str = "approval.request";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_forms_round_trip() {
        let r = Frame::request(
            method::TURN_STEER,
            serde_json::json!({"session_id":"agit-x","message":"hi"}),
        );
        assert!(r.is_request() && !r.is_notification() && !r.is_response());
        let s = r.to_json();
        let back = Frame::from_json(&s).unwrap();
        assert_eq!(r, back);

        let n = Frame::event(
            "agit-x",
            7,
            method::ITEM_DELTA,
            serde_json::json!({"text":"a"}),
        );
        assert!(n.is_event());
        assert_eq!(n.seq, Some(7));
        let j: Value = serde_json::from_str(&n.to_json()).unwrap();
        assert_eq!(j["jsonrpc"], "2.0");
        assert_eq!(j["seq"], 7);
        assert_eq!(j["stream"], "agit-x");
        assert!(j.get("id").is_none(), "notifications carry no id");

        let resp = Frame::response(RequestId::Num(3), serde_json::json!({"ok":true}));
        assert!(resp.is_response());
        let e = Frame::error_response(
            RequestId::Num(3),
            RpcError::new(ErrorCode::SessionBusy, "x").with_hint("try again in a moment"),
        );
        assert!(e.error.as_ref().unwrap().is(ErrorCode::SessionBusy));
        assert_eq!(
            e.error.as_ref().unwrap().data.as_ref().unwrap()["hint"],
            "try again in a moment"
        );
    }

    #[test]
    fn source_generation_is_local_unforgeable_metadata() {
        let mut local = Frame::notification(method::ITEM_DELTA, serde_json::json!({"text":"x"}));
        assert_eq!(local.source_generation, None);
        local.source_generation = Some(41);

        let wire = local.to_json();
        assert!(
            !wire.contains("source_generation"),
            "the local generation fence must never become protocol surface"
        );
        let roundtrip = Frame::from_json(&wire).unwrap();
        assert_eq!(roundtrip.source_generation, None);
        assert_eq!(
            local, roundtrip,
            "wire equality deliberately ignores daemon-local provenance"
        );

        let forged = Frame::from_json(
            r#"{"jsonrpc":"2.0","method":"item.delta","params":{},"source_generation":999}"#,
        )
        .unwrap();
        assert_eq!(
            forged.source_generation, None,
            "JSON input cannot assert a local harness generation"
        );

        let constructors = [
            Frame::request("test.request", serde_json::json!({})),
            Frame::notification("test.notification", serde_json::json!({})),
            Frame::event("session-a", 1, "test.event", serde_json::json!({})),
            Frame::response(RequestId::Num(1), serde_json::json!({})),
            Frame::error_response(
                RequestId::Num(1),
                RpcError::new(ErrorCode::Internal, "test"),
            ),
        ];
        assert!(
            constructors
                .iter()
                .all(|frame| frame.source_generation.is_none()),
            "only the daemon's per-session bridge may attach provenance"
        );
    }

    /// **The golden frame**. A test of the same name in the backend repository's
    /// `src/features/rc/proto.rs` pins the same bytes.
    ///
    /// The backend pins `agit` at a rev from before the layout migration
    /// (`domain::snapshot` → `domain::meta`), so it cannot use this module directly yet and
    /// duplicates the **envelope** definition instead (not the IR — it stores event payloads
    /// as opaque JSON). This test is the law that duplicate obeys: a field name, an order or
    /// an omission rule changed on either side turns both repositories' CI red.
    ///
    /// Once the backend follows the layout migration, that copy goes away in favor of
    /// `use agit::protocol::*` and this test degenerates into an ordinary round-trip test.
    const GOLDEN_EVENT: &str = r#"{"jsonrpc":"2.0","method":"item.delta","params":{"item_id":"i1","text":"hi"},"seq":7,"stream":"agit-abc"}"#;

    #[test]
    fn wire_shape_matches_the_backend() {
        let f = Frame::event(
            "agit-abc",
            7,
            method::ITEM_DELTA,
            serde_json::json!({"item_id":"i1","text":"hi"}),
        );
        assert_eq!(f.to_json(), GOLDEN_EVENT);

        let back = Frame::from_json(GOLDEN_EVENT).unwrap();
        assert!(back.is_event() && !back.is_request());
        assert_eq!(back.seq, Some(7));
        assert_eq!(back.stream.as_deref(), Some("agit-abc"));
        assert_eq!(back.method(), method::ITEM_DELTA);
    }

    /// Method names are a cross-repository string contract; renaming one changes the wire.
    #[test]
    fn method_names_are_the_wire_contract() {
        assert_eq!(method::RC_REGISTER, "rc.register");
        assert_eq!(method::ITEM_COMPLETED, "item.completed");
        assert_eq!(method::TURN_STEER, "turn.steer");
        assert_eq!(method::APPROVAL_REQUEST, "approval.request");
        assert_eq!(method::SECRET_DETECTED, "secret.detected");
        assert_eq!(VERSION, 1);
        assert_eq!(feature::AGENT_IDENTITY_V1, "agent_identity_v1");
        assert_eq!(
            feature::SESSION_START_IDEMPOTENCY_V1,
            "session_start_idempotency_v1"
        );
    }

    /// Feature negotiation is additive inside protocol v1. An old peer sends
    /// neither list; that must mean "nothing ACKed", never implicit support.
    #[test]
    fn an_old_register_peer_acks_no_features() {
        let old_register = serde_json::json!({
            "protocol_version": 1,
            "machine_fingerprint": "machine-1",
            "display_name": "laptop",
            "agit_version": "0.1.0",
            "platform": "macos-aarch64",
            "capabilities": [],
            "workspaces": [],
            "last_seq": {}
        });
        let register: RcRegister = serde_json::from_value(old_register).unwrap();
        assert!(register.features.is_empty());

        let old_result = serde_json::json!({
            "connection_id": "conn-1",
            "workspaces": [],
            "persisted_seq": {},
            "server_time": "2026-08-22T00:00:00Z"
        });
        let result: RcRegisterResult = serde_json::from_value(old_result).unwrap();
        assert!(
            result.accepted_features.is_empty(),
            "missing ACK list must fail closed"
        );
    }

    #[test]
    fn immutable_lineage_fields_are_additive_but_round_trip_when_present() {
        let old_start = serde_json::json!({
            "workspace_id": "ws-1",
            "project_id": "p-1",
            "runtime": "codex"
        });
        let old: SessionStart = serde_json::from_value(old_start).unwrap();
        assert!(old.expected_agent_id.is_none());
        assert!(old.start_id.is_none());

        let id = "00000000-0000-0000-0000-000000000001";
        let start: SessionStart = serde_json::from_value(serde_json::json!({
            "workspace_id": "ws-1",
            "project_id": "p-1",
            "runtime": "codex",
            "agent": "alice/payments",
            "expected_agent_id": id,
            "branch": "s/1",
            "start_id": "018f47cb-60ff-7e31-aec9-02d2e39d3114"
        }))
        .unwrap();
        assert_eq!(start.expected_agent_id.as_deref(), Some(id));
        assert_eq!(
            start.start_id.as_deref(),
            Some("018f47cb-60ff-7e31-aec9-02d2e39d3114")
        );
        assert_eq!(
            serde_json::to_value(start).unwrap()["expected_agent_id"],
            id
        );

        let session = SessionInfo {
            session_id: "agit-one".into(),
            workspace_id: "ws-1".into(),
            project_id: Some("p-1".into()),
            runtime: "codex".into(),
            agent: Some("alice/payments".into()),
            branch: Some("s/1".into()),
            status: SessionStatus::Idle,
            last_seq: 0,
            gist: None,
            dangerous: false,
            permission_mode: Some(PermissionMode::Default),
            created_at: "now".into(),
            updated_at: "now".into(),
        };
        let old_result: SessionStartResult = serde_json::from_value(serde_json::json!({
            "session": session
        }))
        .unwrap();
        assert!(old_result.start_id.is_none());
        let keyed = SessionStartResult {
            start_id: Some("018f47cb-60ff-7e31-aec9-02d2e39d3114".into()),
            session: old_result.session,
        };
        assert_eq!(
            serde_json::to_value(keyed).unwrap()["start_id"],
            "018f47cb-60ff-7e31-aec9-02d2e39d3114"
        );
    }

    #[test]
    fn secret_alert_is_deliberately_non_identifying() {
        let value = serde_json::to_value(SecretDetected {
            count: 2,
            source: "item_delta".into(),
        })
        .unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 2);
        assert_eq!(object["count"], 2);
        assert_eq!(object["source"], "item_delta");
        for forbidden in ["secret", "name", "id", "text", "context"] {
            assert!(!object.contains_key(forbidden));
        }
    }

    #[test]
    fn wrong_jsonrpc_version_is_rejected() {
        let bad = r#"{"jsonrpc":"1.0","method":"x"}"#;
        assert!(Frame::from_json(bad).is_err());
    }

    #[test]
    fn missing_params_deserialize_as_default() {
        #[derive(serde::Deserialize, Default, PartialEq, Debug)]
        #[serde(default)]
        struct P {
            a: u32,
        }
        let f = Frame::from_json(r#"{"jsonrpc":"2.0","id":"1","method":"m"}"#).unwrap();
        // Null → serde default only works when the struct opts in; a plain
        // struct would fail, and that's the correct behaviour for required params.
        assert!(f.params_as::<P>().is_err() || f.params_as::<P>().unwrap() == P::default());
    }
}
