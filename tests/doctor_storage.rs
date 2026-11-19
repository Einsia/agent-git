//! Explicit storage attribution must remain metadata-only even beside damaged local state.

#[path = "support/lab_env.rs"]
mod lab_env;

use lab_env::LabEnv;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

fn output(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let drain = |stream: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stream.take(1 << 20).read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("metadata inspection blocked on a payload or external service");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Output {
        status: child.wait().unwrap(),
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn command(root: &Path, sink: &TcpListener) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .env_clear_local()
        .env("HOME", root.join("native-home"))
        .env("USERPROFILE", root.join("native-home"))
        .env("AGIT_HOME", root.join("unselected-home"))
        .env("PATH", root.join("no-programs"))
        .env(
            "AGIT_HUB_URL",
            format!("http://{}", sink.local_addr().unwrap()),
        )
        .telemetry_sink(sink.local_addr().unwrap().port())
        .env("NO_COLOR", "1")
        .current_dir(root);
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

/// One layout distinguishes originals, publication copies, shared files and incomplete traversal.
/// Damaged recovery state and unreadable payloads must not activate normal doctor/startup readers.
#[test]
fn explicit_storage_reports_observed_categories_without_reading_or_changing_payloads() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("selected");
    let samples = [
        ("repos/alice/sample/.git/objects/ab/object", 13),
        (
            "repos/alice/sample/.git/agit/privacy-publication/scope/.git/objects/object",
            29,
        ),
        (
            "repos/alice/sample/.git/agit/privacy-recovery/commit/manifest.json",
            31,
        ),
        ("repos/alice/sample/.git/lfs/objects/object", 37),
        (
            "repos/alice/sample/.git/objects/pack/tmp_pack_synthetic",
            43,
        ),
        ("worktrees/alice/sample/topic/events/a/b/c/d/event", 11),
        ("credentials/synthetic.json", 19),
        ("store/codex/synthetic.json", 17),
        ("git-runtime/synthetic/bin/git", 23),
        ("layout-v1-recovery/pending-synthetic", 7),
    ];
    for (relative, bytes) in samples {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }
    fs::write(home.join("unclassified"), b"shared").unwrap();
    fs::hard_link(home.join("unclassified"), home.join("store/shared")).unwrap();
    let sparse = fs::File::create(home.join("sparse")).unwrap();
    sparse.set_len(1 << 20).unwrap();
    drop(sparse);
    fs::create_dir_all(root.join("outside")).unwrap();
    fs::write(root.join("outside/excluded"), vec![b'x'; 101]).unwrap();
    fs::create_dir_all(root.join("native-home/.claude")).unwrap();
    let native = root.join("native-home/.claude/settings.json");
    fs::write(&native, b"unread synthetic settings").unwrap();
    let before = snapshot(&root);
    let unread = home.join("credentials/synthetic.json");
    let accessed = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    for path in [&unread, &native] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(accessed))
            .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unread, fs::Permissions::from_mode(0)).unwrap();
    }
    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    sink.set_nonblocking(true).unwrap();
    let result = output(command(&root, &sink).args(["--json", "doctor", "--storage", "selected"]));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let doc: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(doc["schema"], "cli-output");
    assert_eq!(doc["command"], "doctor");
    let report = &doc["result"]["value"];
    assert_eq!(report["schema"], "storage-usage");
    assert_eq!(report["complete"], true);
    for (category, bytes) in [
        ("original_git_objects", 13),
        ("publication_stores", 29),
        ("recovered_originals", 31),
        ("lfs_cache", 37),
        ("temporary_packs", 43),
        ("session_worktrees", 11),
        ("authority_state", 19),
        ("links", 17),
        ("bundled_git", 23),
        ("shared_hardlinks", 6),
        ("unclassified", (1 << 20) + 7),
    ] {
        assert_eq!(
            report["categories"][category]["logical_file_bytes"], bytes,
            "{category}"
        );
    }
    assert_eq!(report["observed"]["logical_file_bytes"], (1 << 20) + 236);
    assert_eq!(report["duplicate_file_entries"], 1);
    assert_eq!(report["observed"]["files"], 12);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut seen = std::collections::HashSet::new();
        let allocated: u64 = walkdir::WalkDir::new(&home)
            .into_iter()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .filter(|metadata| seen.insert((metadata.dev(), metadata.ino())))
            .map(|metadata| metadata.blocks() * 512)
            .sum();
        assert_eq!(report["observed"]["allocated_bytes"], allocated);
    }
    #[cfg(windows)]
    assert!(report["observed"]["allocated_bytes"].is_null());
    for path in [&unread, &native] {
        assert_eq!(fs::metadata(path).unwrap().accessed().unwrap(), accessed);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unread, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), home.join("outside-link")).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let pipe =
            std::ffi::CString::new(home.join("payload-pipe").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
        let partial =
            output(command(&root, &sink).args(["doctor", "--storage=selected", "--json"]));
        assert_eq!(
            partial.status.code(),
            Some(agit::ExitCode::Precondition.as_i32())
        );
        let doc: Value = serde_json::from_slice(&partial.stdout).unwrap();
        let partial = &doc["result"]["value"];
        assert_eq!(partial["complete"], false);
        assert_eq!(partial["gaps"]["skipped_links"], 1);
        assert_eq!(partial["gaps"]["special_entries"], 1);
        assert_eq!(
            partial["observed"]["logical_file_bytes"],
            report["observed"]["logical_file_bytes"]
        );
        let refused = output(command(&root, &sink).args([
            "--json",
            "doctor",
            "--storage",
            "selected/outside-link",
        ]));
        let doc: Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert_eq!(doc["result"]["value"]["complete"], false);
        assert_eq!(doc["result"]["value"]["observed"]["files"], 0);
    }
    let missing = output(command(&root, &sink).args(["--json", "doctor", "--storage", "absent"]));
    let doc: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(doc["result"]["value"]["complete"], false);
    assert_eq!(doc["result"]["value"]["gaps"]["io_errors"], 1);
    let invalid =
        output(command(&root, &sink).args(["--json", "doctor", "--storage", "selected", "--deep"]));
    assert_eq!(invalid.status.code(), Some(agit::ExitCode::Usage.as_i32()));
    assert!(!root.join("unselected-home").exists());
    assert_eq!(snapshot(&root), before);
    assert_eq!(sink.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(!String::from_utf8_lossy(&result.stdout).contains("unread synthetic settings"));
}
