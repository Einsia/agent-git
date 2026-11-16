//! The async hook itself owns delivery: startup needs no completed turn, no detached child,
//! and no stdout/stderr that Claude Code could consume as context.
#![cfg(unix)]

use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::OpenOptionsExt, net::UnixListener},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SESSION: &str = "cccccccc-0000-4000-8000-000000000003";
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct NativeParent {
    child: Child,
    relay: Option<libc::pid_t>,
}

impl Drop for NativeParent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(pid) = self.relay.take() {
            // Only the PID returned by this fixture's shell belongs to its teardown.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[test]
fn session_start_relay_delivers_silently_and_exits_with_its_native_parent() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("agit");
    let config = root.path().join("claude");
    fs::create_dir_all(store.join("rc")).unwrap();
    fs::write(store.join("rc/workspaces.json"), "{}").unwrap();
    fs::create_dir_all(config.join("sessions")).unwrap();
    let socket = root.path().join("inbox.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    // The shell stands in for the native runtime and waits for its asynchronous hook. Keeping
    // its stdin descriptor explicit gives the background hook the SessionStart payload.
    let child = Command::new("sh")
        .args([
            "-c",
            "exec 3<&0; \"$1\" hooks relay <&3 & relay_pid=$!; echo \"$relay_pid\"; wait \"$relay_pid\"",
            "native-parent",
            env!("CARGO_BIN_EXE_agit"),
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.path())
        .env("AGIT_HOME", &store)
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("AGIT_SKIP_SETUP", "1")
        .env("CLAUDE_CONFIG_DIR", &config)
        .env("CLAUDE_CODE_MESSAGING_TOKEN", TOKEN)
        .env("CLAUDE_CODE_MESSAGING_SOCKET", &socket)
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut native = NativeParent { child, relay: None };
    let mut stdout = BufReader::new(native.child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    native.relay = Some(line.trim().parse().unwrap());
    let pid = native.child.id();
    fs::write(
        config.join(format!("sessions/{pid}.json")),
        json!({"pid": pid, "sessionId": SESSION, "peerProtocol": 1,
            "procStart": "synthetic-native-start", "messagingSocketPath": socket})
        .to_string(),
    )
    .unwrap();
    let payload = json!({"session_id": SESSION, "hook_event_name": "SessionStart"});
    native
        .child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let queue = store.join("claude-inbox").join(SESSION);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !queue.join("pending").is_dir() {
        assert!(
            Instant::now() < deadline,
            "SessionStart did not create the inbox"
        );
        assert!(
            native.child.try_wait().unwrap().is_none(),
            "the hook exited during startup"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let pending = json!({"msg_id": "first-message", "content": "Synthetic first-turn input.",
        "owner": true, "enqueued_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        "pid": pid, "proc_start": "synthetic-native-start"});
    let staging = queue.join("entry.tmp");
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&staging)
        .unwrap()
        .write_all(pending.to_string().as_bytes())
        .unwrap();
    fs::rename(
        staging,
        queue
            .join("pending")
            .join(format!("{}.json", "ab".repeat(32))),
    )
    .unwrap();
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the hook did not deliver before a Stop event"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    // A closed peer still has buffered frames; changing its socket timeout can fail on macOS.
    stream.set_nonblocking(true).unwrap();
    let read_deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        assert!(
            Instant::now() < read_deadline,
            "the relay must finish its frames"
        );
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("{error}"),
        }
    }
    let frames = String::from_utf8(bytes).unwrap();
    let frames: Vec<serde_json::Value> = frames
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames[0], json!({"type": "auth", "token": TOKEN}));
    assert_eq!(frames[1]["message"]["content"], pending["content"]);
    assert_eq!(frames[1]["session_id"], SESSION);
    assert!(native.child.try_wait().unwrap().is_none());

    // Losing the native process must close the hook's output pipes even with its registration
    // and socket path still present. A relay that only tests file existence stays alive here.
    native.child.kill().unwrap();
    native.child.wait().unwrap();
    let mut stderr = native.child.stderr.take().unwrap();
    let (closed, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout.read_to_end(&mut out).unwrap();
        stderr.read_to_end(&mut err).unwrap();
        closed.send((out, err)).unwrap();
    });
    let (out, err) = output
        .recv_timeout(Duration::from_secs(5))
        .expect("the relay must exit promptly with the native parent");
    native.relay = None;
    assert!(
        out.is_empty() && err.is_empty(),
        "the relay must not write hook context"
    );
    assert!(
        !queue.exists(),
        "an idle queue ends with the native session"
    );
    let mut directories = vec![root.path().to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() {
                assert!(
                    !fs::read(entry.path())
                        .unwrap()
                        .windows(TOKEN.len())
                        .any(|bytes| bytes == TOKEN.as_bytes()),
                    "the hook credential must not persist in settings, queues, logs or backups"
                );
            }
        }
    }
}
