//! Bounded owner-service diagnostics contain routing metadata, never RPC payloads.

use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

const FILE_BYTES: u64 = 4 * 1024 * 1024;
const BACKUPS: usize = 3;

#[derive(Clone)]
pub(super) struct Log(Arc<Inner>);

struct Inner {
    sender: Option<mpsc::SyncSender<Vec<u8>>>,
    _worker: Option<std::thread::JoinHandle<()>>,
    instance: String,
    path: PathBuf,
    sequence: AtomicU64,
    dropped: AtomicU64,
}

impl Log {
    pub(super) fn open(instance: &str) -> crate::Result<Self> {
        Self::open_in(&super::rc_dir()?, instance, FILE_BYTES)
    }

    fn open_in(directory: &Path, instance: &str, limit: u64) -> crate::Result<Self> {
        let path = directory.join(format!("diagnostics-{instance}.jsonl"));
        let file = private_file(&path)?;
        let (sender, receive) = mpsc::sync_channel::<Vec<u8>>(512);
        let writer_path = path.clone();
        let worker = std::thread::Builder::new()
            .name("agitd-diagnostics".into())
            .spawn(move || {
                if let Err(error) = drain(file, &writer_path, receive, limit) {
                    eprintln!("agitd: diagnostic writer stopped: {error}");
                }
            })?;
        Ok(Self(Arc::new(Inner {
            sender: Some(sender),
            _worker: Some(worker),
            instance: instance.into(),
            path,
            sequence: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
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
        self.record("rpc.completed", json!({
            "client": client, "request_id": request_id(frame.id.as_ref()), "error_code": error.map(|e| e.code),
            "outcome": error.and_then(|e| e.data.as_ref()).map(|data| label(&data["outcome"])),
            "operation_id": error.and_then(|e| e.data.as_ref()).map(|data| label(&data["operation_id"])),
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
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn drain(
    mut file: File,
    path: &Path,
    receive: mpsc::Receiver<Vec<u8>>,
    limit: u64,
) -> std::io::Result<()> {
    let mut bytes = 0;
    for line in receive {
        if bytes > 0 && bytes + line.len() as u64 > limit {
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
            file = private_file(path)?;
            bytes = 0;
        }
        file.write_all(&line)?;
        bytes += line.len() as u64;
    }
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rotation_retains_private_metadata_without_request_content() {
        let directory = tempfile::tempdir().unwrap();
        let log = Log::open_in(directory.path(), "fixture", 512).unwrap();
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
}
