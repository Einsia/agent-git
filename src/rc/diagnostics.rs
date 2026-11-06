//! Bounded owner-service diagnostics contain routing metadata, never RPC payloads.

use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const FILE_BYTES: u64 = 4 * 1024 * 1024;
const BACKUPS: usize = 3;
/// A failed write, such as on a full disk, suspends the log for this long instead of ending it.
const WRITE_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct Log(Arc<Inner>);

/// The daemon's log once it is open, for the parts of the daemon that run apart from the
/// endpoint: session supervisors, runtime drivers, the session RPC waits and the frame loop.
static DAEMON_LOG: std::sync::OnceLock<Log> = std::sync::OnceLock::new();

/// Makes `log` the one [`event`] writes to. The first log opened in a process stays.
pub(super) fn install(log: &Log) {
    let _ = DAEMON_LOG.set(log.clone());
}

/// Records `event` in the daemon's log when one is open. `metadata` holds ids (every one capped,
/// see [`id`] and [`rpc_id`]), fixed classes, exit codes and timings only: never a prompt,
/// message, tool output, path or error text.
pub(crate) fn event(event: &str, metadata: Value) {
    if let Some(log) = DAEMON_LOG.get() {
        log.record(event, metadata);
    }
}

/// Waits, up to `timeout`, until every event recorded so far is written. The log outlives the
/// daemon's last task, so lines still queued when the process exits are lost: a planned exit then
/// reads like a crash.
pub(crate) fn settle(timeout: Duration) {
    if let Some(log) = DAEMON_LOG.get() {
        log.settle(timeout);
    }
}

/// The newest daemon log in each daemon namespace (the desktop daemon's first), for `agit
/// doctor`. Nothing is created: a namespace without a log has none to name.
pub(super) fn newest_logs() -> Vec<PathBuf> {
    let Ok(home) = crate::infra::config::agit_home() else {
        return Vec::new();
    };
    [super::LOCAL_NAMESPACE, "rc"]
        .iter()
        .filter_map(|namespace| {
            std::fs::read_dir(home.join(namespace))
                .ok()?
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.starts_with("diagnostics-") && name.ends_with(".jsonl")
                })
                .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
                .max_by_key(|(modified, _)| *modified)
                .map(|(_, path)| path)
        })
        .collect()
}

/// The events the daemon-wide log holds whose metadata names `id`, for tests: a log in a
/// private temporary folder is installed first when none is. Waits up to five seconds for
/// `expected` such events, since lines are written on their own thread.
#[cfg(test)]
pub(crate) fn test_events(id: &str, expected: usize) -> Vec<Value> {
    let log = DAEMON_LOG.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap().keep();
        Log::open_in(&directory, "test", FILE_BYTES, WRITE_RETRY).unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events: Vec<Value> = std::fs::read_to_string(log.path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["metadata"].to_string().contains(id))
            .collect();
        if events.len() >= expected || Instant::now() >= deadline {
            return events;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An id as the log keeps it: at most 256 characters. Every id an event carries is capped so.
pub(crate) fn id(value: &str) -> String {
    short(value)
}

/// A request id as the log keeps it: a string capped as [`id`], a number as it is.
pub(crate) fn rpc_id(value: Option<&crate::protocol::RequestId>) -> Value {
    request_id(value)
}

/// Milliseconds since `start`, for a timing field.
pub(crate) fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The name of an enum value's variant (`Accepted` for `Accepted { .. }`): a fixed class, never
/// the text a variant carries.
pub(crate) fn variant<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}")
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(64)
        .collect()
}

struct Inner {
    sender: Option<mpsc::SyncSender<Vec<u8>>>,
    _worker: Option<std::thread::JoinHandle<()>>,
    instance: String,
    path: PathBuf,
    sequence: AtomicU64,
    dropped: Arc<AtomicU64>,
    /// Lines handed to the writer, and lines it has finished with, written or not.
    queued: AtomicU64,
    handled: Arc<AtomicU64>,
}

impl Log {
    pub(super) fn open(instance: &str) -> crate::Result<Self> {
        Self::open_in(&super::rc_dir()?, instance, FILE_BYTES, WRITE_RETRY)
    }

    fn open_in(
        directory: &Path,
        instance: &str,
        limit: u64,
        retry: Duration,
    ) -> crate::Result<Self> {
        let path = directory.join(format!("diagnostics-{instance}.jsonl"));
        let file = private_file(&path)?;
        let (sender, receive) = mpsc::sync_channel::<Vec<u8>>(512);
        let writer_path = path.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        let handled = Arc::new(AtomicU64::new(0));
        let finished = handled.clone();
        let worker = std::thread::Builder::new()
            .name("agitd-diagnostics".into())
            .spawn(move || drain(file, &writer_path, receive, limit, retry, &lost, &finished))?;
        Ok(Self(Arc::new(Inner {
            sender: Some(sender),
            _worker: Some(worker),
            instance: instance.into(),
            path,
            sequence: AtomicU64::new(0),
            dropped,
            queued: AtomicU64::new(0),
            handled,
        })))
    }

    pub(super) fn settle(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let queued = self.0.queued.load(Ordering::Acquire);
        while self.0.handled.load(Ordering::Acquire) < queued && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.0.path
    }

    pub(super) fn record(&self, event: &str, metadata: Value) {
        let mut line = serde_json::to_vec(&json!({
            "time": chrono::Utc::now().to_rfc3339(), "pid": std::process::id(),
            "instance_id": self.0.instance, "sequence": self.0.sequence.fetch_add(1, Ordering::Relaxed),
            "dropped": self.0.dropped.load(Ordering::Relaxed), "event": event, "metadata": metadata,
        })).unwrap_or_default();
        line.push(b'\n');
        if line.len() > 8192 || self.0.sender.as_ref().unwrap().try_send(line).is_err() {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.0.queued.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(super) fn request(&self, client: u64, frame: &crate::protocol::Frame) {
        let params = frame.params.as_ref().unwrap_or(&Value::Null);
        self.record("rpc.received", json!({
            "client": client, "request_id": request_id(frame.id.as_ref()), "method": short(frame.method()),
            "peer_id": label(&params["peer_id"]), "route_id": label(&params["route_id"]),
            "generation": params["generation"].as_u64(), "peer_method": label(&params["method"]),
            "session_id": label(&params["session_id"]),
        }));
    }

    pub(super) fn response(&self, client: u64, frame: &crate::protocol::Frame) {
        let error = frame.error.as_ref();
        let data = error.and_then(|e| e.data.as_ref());
        self.record("rpc.completed", json!({
            "client": client, "request_id": request_id(frame.id.as_ref()), "error_code": error.map(|e| e.code),
            "outcome": data.map(|data| label(&data["outcome"])),
            "operation_id": data.map(|data| label(&data["operation_id"])),
            "kind": data.and_then(|data| token(&data["kind"])),
            "reason": data.and_then(|data| token(&data["reason"])),
            "retryable": data.and_then(|data| data["retryable"].as_bool()),
        }));
    }

    pub(super) fn dispatch(
        &self,
        client: u64,
        original: &crate::protocol::RequestId,
        internal: &crate::protocol::RequestId,
        method: &str,
    ) {
        self.record(
            "executor.dispatch",
            json!({
                "client":client, "request_id":request_id(Some(original)),
                "executor_request_id":request_id(Some(internal)), "method":short(method),
            }),
        );
    }

    pub(super) fn peer(&self, status: &agit_controller::Status) {
        self.record("peer.state", json!({
            "peer_id": status.peer_id, "route_id": status.route_id, "generation": status.generation,
            "state": status.state, "worker_pid": status.worker_pid,
            "error": status.error.as_ref().map(|message| message.chars().take(512).collect::<String>()),
        }));
    }
}

fn label(value: &Value) -> Option<String> {
    value.as_str().map(short)
}

/// Error classifiers are identifiers; anything else may carry a path or message text.
fn token(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    })
}

fn short(value: &str) -> String {
    value.chars().take(256).collect()
}

fn request_id(id: Option<&crate::protocol::RequestId>) -> Value {
    match id {
        Some(crate::protocol::RequestId::Str(value)) => json!(short(value)),
        Some(crate::protocol::RequestId::Num(value)) => json!(value),
        None => Value::Null,
    }
}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn appended_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn rotate(path: &Path) -> std::io::Result<File> {
    for index in (1..=BACKUPS).rev() {
        let source = if index == 1 {
            path.to_owned()
        } else {
            path.with_extension(format!("jsonl.{}", index - 1))
        };
        let destination = path.with_extension(format!("jsonl.{index}"));
        match std::fs::rename(source, destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
    }
    private_file(path)
}

/// Writes lines until every sender is gone. A write failure counts lines as dropped until the
/// retry time, then reopens the file; the drop count in later lines records the interruption.
fn drain(
    file: File,
    path: &Path,
    receive: mpsc::Receiver<Vec<u8>>,
    limit: u64,
    retry: Duration,
    dropped: &AtomicU64,
    handled: &AtomicU64,
) {
    let mut file = Some((file, 0_u64));
    let mut suspended_until = None::<Instant>;
    for line in receive {
        write_line(
            &mut file,
            &mut suspended_until,
            path,
            &line,
            limit,
            retry,
            dropped,
        );
        handled.fetch_add(1, Ordering::AcqRel);
    }
    if let Some((mut file, _)) = file {
        let _ = file.flush();
    }
}

fn write_line(
    file: &mut Option<(File, u64)>,
    suspended_until: &mut Option<Instant>,
    path: &Path,
    line: &[u8],
    limit: u64,
    retry: Duration,
    dropped: &AtomicU64,
) {
    if file.is_none() {
        if suspended_until.is_some_and(|until| Instant::now() < until) {
            dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        *file = appended_file(path)
            .and_then(|file| Ok((file.metadata()?.len(), file)))
            .map(|(bytes, file)| (file, bytes))
            .ok();
    }
    let written = file.as_mut().map(|(current, bytes)| {
        if *bytes > 0 && *bytes + line.len() as u64 > limit {
            *current = rotate(path)?;
            *bytes = 0;
        }
        current.write_all(line)?;
        *bytes += line.len() as u64;
        Ok::<_, std::io::Error>(())
    });
    if !matches!(written, Some(Ok(()))) {
        if suspended_until.is_none() {
            eprintln!("agitd: diagnostic writer suspended: {written:?}");
        }
        *file = None;
        *suspended_until = Some(Instant::now() + retry);
        dropped.fetch_add(1, Ordering::Relaxed);
    } else {
        *suspended_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// The daemon-wide events keep ids, fixed classes and timings: a typed outcome is logged as
    /// its variant's name, never the text it carries, and an id is capped.
    #[test]
    fn daemon_events_keep_ids_classes_and_timings_only() {
        let outcome = crate::rc::harness::TurnStartOutcome::Unknown {
            message: "SYNTHETIC-PRIVATE-CONTENT".into(),
            attempted_mode: None,
        };
        assert_eq!(variant(&outcome), "Unknown");
        assert_eq!(id(&"x".repeat(300)).len(), 256);
        // Opens the daemon-wide log when no other test did; each test reads only its own ids.
        test_events("session-diagnostics-canary", 0);
        event(
            "turn.start_resolved",
            json!({
                "session_id": id("session-diagnostics-canary"),
                "outcome": variant(&outcome),
                "elapsed_ms": 7,
            }),
        );
        let events = test_events("session-diagnostics-canary", 1);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["event"], "turn.start_resolved");
        assert_eq!(events[0]["metadata"]["outcome"], "Unknown");
        assert!(!events[0].to_string().contains("SYNTHETIC-PRIVATE-CONTENT"));
    }

    #[test]
    fn rotation_retains_private_metadata_without_request_content() {
        let directory = tempfile::tempdir().unwrap();
        let log = Log::open_in(directory.path(), "fixture", 512, WRITE_RETRY).unwrap();
        let frame = crate::protocol::Frame::request(
            "turn.start",
            json!({"session_id":"session", "message":"SYNTHETIC-PRIVATE-CONTENT"}),
        );
        for _ in 0..20 {
            log.request(1, &frame);
        }
        let mut inner = Arc::try_unwrap(log.0).ok().unwrap();
        inner.sender.take();
        inner._worker.take().unwrap().join().unwrap();
        let files = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), BACKUPS + 1);
        for file in files {
            #[cfg(unix)]
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            let contents = std::fs::read_to_string(file).unwrap();
            assert!(!contents.contains("SYNTHETIC-PRIVATE-CONTENT"));
            for line in contents.lines() {
                let value: Value = serde_json::from_str(line).unwrap();
                assert_eq!(value["instance_id"], "fixture");
                assert_eq!(value["metadata"]["method"], "turn.start");
            }
        }
    }

    /// Settling waits until every line recorded so far is on disk, with the writer still running:
    /// the daemon's log is never closed, so what is still queued when the process exits is lost
    /// and a planned exit reads like a crash. A settle that returns at once leaves those lines
    /// queued.
    #[test]
    fn settling_writes_every_recorded_line_without_closing_the_log() {
        let directory = tempfile::tempdir().unwrap();
        let log = Log::open_in(directory.path(), "fixture", FILE_BYTES, WRITE_RETRY).unwrap();
        for sequence in 0..200 {
            log.record("fixture.event", json!({ "sequence": sequence }));
        }
        log.settle(Duration::from_secs(10));
        let contents =
            std::fs::read_to_string(directory.path().join("diagnostics-fixture.jsonl")).unwrap();
        assert_eq!(contents.lines().count(), 200);
    }

    /// A writer that hits a failed write resumes once writing works again, and its later lines
    /// say how many were lost. A writer that ends on the first error leaves the daemon without
    /// diagnostics until it restarts.
    #[test]
    fn a_failed_write_suspends_the_log_until_writing_recovers() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs");
        std::fs::create_dir(&directory).unwrap();
        let log = Log::open_in(&directory, "fixture", 512, Duration::from_millis(50)).unwrap();
        let frame = crate::protocol::Frame::request("session.list", json!({}));
        let settle = || std::thread::sleep(Duration::from_millis(100));
        for _ in 0..4 {
            log.request(1, &frame);
        }
        settle();
        // Rotation cannot create the next file while its directory is gone.
        std::fs::remove_dir_all(&directory).unwrap();
        for _ in 0..8 {
            log.request(1, &frame);
        }
        settle();
        std::fs::create_dir(&directory).unwrap();
        settle();
        log.request(1, &frame);
        let mut inner = Arc::try_unwrap(log.0).ok().unwrap();
        inner.sender.take();
        inner._worker.take().unwrap().join().unwrap();
        let contents =
            std::fs::read_to_string(directory.join("diagnostics-fixture.jsonl")).unwrap();
        let last: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert!(last["dropped"].as_u64().unwrap() > 0);
    }
}
