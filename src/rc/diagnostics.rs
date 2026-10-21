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

struct Inner {
    sender: Option<mpsc::SyncSender<Vec<u8>>>,
    _worker: Option<std::thread::JoinHandle<()>>,
    instance: String,
    path: PathBuf,
    sequence: AtomicU64,
    dropped: Arc<AtomicU64>,
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
        let worker = std::thread::Builder::new()
            .name("agitd-diagnostics".into())
            .spawn(move || drain(file, &writer_path, receive, limit, retry, &lost))?;
        Ok(Self(Arc::new(Inner {
            sender: Some(sender),
            _worker: Some(worker),
            instance: instance.into(),
            path,
            sequence: AtomicU64::new(0),
            dropped,
        })))
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
) {
    let mut file = Some((file, 0_u64));
    let mut suspended_until = None::<Instant>;
    for line in receive {
        if file.is_none() {
            if suspended_until.is_some_and(|until| Instant::now() < until) {
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            file = appended_file(path)
                .and_then(|file| Ok((file.metadata()?.len(), file)))
                .map(|(bytes, file)| (file, bytes))
                .ok();
        }
        let written = file.as_mut().map(|(current, bytes)| {
            if *bytes > 0 && *bytes + line.len() as u64 > limit {
                *current = rotate(path)?;
                *bytes = 0;
            }
            current.write_all(&line)?;
            *bytes += line.len() as u64;
            Ok::<_, std::io::Error>(())
        });
        if !matches!(written, Some(Ok(()))) {
            if suspended_until.is_none() {
                eprintln!("agitd: diagnostic writer suspended: {written:?}");
            }
            file = None;
            suspended_until = Some(Instant::now() + retry);
            dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            suspended_until = None;
        }
    }
    if let Some((mut file, _)) = file {
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
