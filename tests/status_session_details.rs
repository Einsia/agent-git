use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SID: &str = "00000000-0000-4000-8000-000000000017";

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
    #[cfg(unix)]
    Fifo {
        device: u64,
        inode: u64,
        mode: u32,
    },
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, Entry> {
    walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::{FileTypeExt, MetadataExt};
                if entry.file_type().is_fifo() {
                    let metadata = fs::symlink_metadata(entry.path()).unwrap();
                    return (
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        Entry::Fifo {
                            device: metadata.dev(),
                            inode: metadata.ino(),
                            mode: metadata.mode(),
                        },
                    );
                }
            }
            let kind = if entry.file_type().is_symlink() {
                Entry::Symlink(fs::read_link(entry.path()).unwrap())
            } else if entry.file_type().is_dir() {
                Entry::Directory
            } else {
                Entry::File(fs::read(entry.path()).unwrap())
            };
            (entry.path().strip_prefix(root).unwrap().to_owned(), kind)
        })
        .collect()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn status_output(command: &mut Command) -> Output {
    #[cfg(not(unix))]
    {
        command.output().unwrap()
    }
    #[cfg(unix)]
    {
        use std::io::{Read, Seek};
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};

        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                unsafe { libc::killpg(self.0.id() as i32, libc::SIGKILL) };
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        // Files keep the outer watchdog independent of a blocked child's pipe lifecycle.
        let mut stdout = tempfile::tempfile().unwrap();
        let mut stderr = tempfile::tempfile().unwrap();
        command
            .process_group(0)
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap());
        let mut child = OwnedChild(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(45);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "status exceeded its outer watchdog"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout.rewind().unwrap();
        stderr.rewind().unwrap();
        stdout.read_to_end(&mut out).unwrap();
        stderr.read_to_end(&mut err).unwrap();
        Output {
            status,
            stdout: out,
            stderr: err,
        }
    }
}

struct ReplacedObject {
    path: PathBuf,
    saved: tempfile::TempDir,
}

impl ReplacedObject {
    fn new(lab: &Lab, path: &str, fifo: bool) -> Self {
        let oid = lab.git(&["rev-parse", &format!("HEAD:{path}")]);
        assert_eq!(oid.len(), 40);
        let path = lab
            .repo
            .root()
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]);
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        let saved = tempfile::tempdir().unwrap();
        fs::rename(&path, saved.path().join("object")).unwrap();
        let guard = Self { path, saved };
        if fifo {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let path = std::ffi::CString::new(guard.path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            #[cfg(not(unix))]
            panic!("FIFO fixtures require Unix");
        } else {
            fs::write(&guard.path, b"SYNTHETIC-invalid-git-object").unwrap();
        }
        guard
    }
}

impl Drop for ReplacedObject {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        fs::rename(self.saved.path().join("object"), &self.path).unwrap();
    }
}

fn message(role: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"response_item","payload":{"type":"message","role":role,"content":[{"type":"input_text","text":text}]}})
    )
}

fn status_diagnostics(status: &serde_json::Value) -> String {
    assert_eq!(status["schema_version"], 1);
    let mut out = Vec::new();
    let selection = &status["selection"];
    if let (Some(repo), Some(branch)) = (selection["repo"].as_str(), selection["branch"].as_str()) {
        out.push(format!("{repo} @ {branch}"));
        out.push(format!("via: {}", selection["source"].as_str().unwrap()));
    } else {
        assert!(selection["repo"].is_null() && selection["branch"].is_null());
        out.push(format!(
            "session target unavailable: {}",
            selection["reason"].as_str().unwrap()
        ));
    }
    if status["sessions"]["incomplete"].as_bool().unwrap() {
        out.push("incomplete inventory".into());
    }
    for row in status["sessions"]["items"].as_array().unwrap() {
        out.push(row["pending_activity"].as_str().unwrap().into());
        out.push(row["local_instance"].as_str().unwrap().into());
    }
    let missing = &status["unadopted"];
    if missing["checked"] == true {
        let sessions = missing["sessions"].as_array().unwrap();
        let errors = missing["errors"].as_array().unwrap();
        assert_eq!(missing["incomplete"], !errors.is_empty());
        if sessions.is_empty() && errors.is_empty() {
            out.push("no unadopted sessions found in the checked indexes".into());
        } else if !sessions.is_empty() {
            out.push(format!("{} sessions not adopted yet", sessions.len()));
            for row in sessions {
                out.push(link::short(row["session_id"].as_str().unwrap()));
            }
        }
        for error in errors {
            out.push(error["message"].as_str().unwrap().into());
        }
    } else {
        assert_eq!(missing["checked"], false);
        assert!(
            missing["sessions"].is_null()
                && missing["errors"].is_null()
                && missing["incomplete"].is_null()
        );
    }
    out.join("\n")
}

struct Lab {
    root: tempfile::TempDir,
    repo: Repo,
    store: Store,
    native: PathBuf,
    prefix: String,
    claim: link::Link,
    head: String,
    hub: TcpListener,
}

#[test]
fn status_git_object_failure_preserves_evidence_and_recovers_without_repair() {
    let lab = Lab::new();
    let healthy = inventory(lab.root.path());
    let object = ReplacedObject::new(&lab, meta::FILE, false);
    lab.expect(
        "unavailable: incomplete or inaccessible native evidence",
        "current claim; process unverified",
    );
    drop(object);
    assert_eq!(inventory(lab.root.path()), healthy);
    lab.expect(
        "1 user turns with pending activity",
        "current claim; process unverified",
    );
}

#[cfg(unix)]
#[test]
fn status_git_object_fifos_bound_metadata_sequences_events_and_legacy_pairs() {
    for stage in ["metadata", "sequence", "event", "legacy"] {
        let mut lab = Lab::new();
        if stage == "legacy" {
            lab.use_legacy_storage();
        }
        let path = match stage {
            "metadata" => meta::FILE.to_owned(),
            "sequence" => meta::LOG_FILE.to_owned(),
            "event" => {
                let sequence = fs::read_to_string(lab.repo.root().join(meta::LOG_FILE)).unwrap();
                meta::event_path(sequence.lines().next().unwrap()).unwrap()
            }
            "legacy" => meta::LEGACY_LOG_FILE.to_owned(),
            _ => unreachable!(),
        };
        lab.expect_versions(
            "1 user turns with pending activity",
            "current claim; process unverified",
            &[Some("2")],
        );
        let healthy = inventory(lab.root.path());
        let object = ReplacedObject::new(&lab, &path, true);
        // These are the actual startup prerequisites; no recovery ref may bypass their early exit.
        assert_eq!(lab.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert!(
            lab.git(&["for-each-ref", "refs/agit/layout-v0/"])
                .is_empty()
        );
        assert!(!lab.root.path().join("agit/layout-v1.complete").exists());
        let versions: &[Option<&str>] = if stage == "metadata" {
            &[None, Some("1"), Some("2")]
        } else {
            &[Some("2")]
        };
        lab.expect_versions(
            "unavailable: incomplete or inaccessible native evidence",
            "current claim; process unverified",
            versions,
        );
        drop(object);
        assert_eq!(inventory(lab.root.path()), healthy);
        lab.expect_versions(
            "1 user turns with pending activity",
            "current claim; process unverified",
            &[Some("2")],
        );
    }
}

impl Lab {
    // Child processes and native indexes share the same spelling for the workspace directory.
    fn cwd(&self) -> PathBuf {
        #[cfg(not(windows))]
        {
            fs::canonicalize(self.root.path().join("work")).unwrap()
        }
        #[cfg(windows)]
        {
            let cwd = fs::canonicalize(self.root.path().join("work")).unwrap();
            let path = cwd.to_str().unwrap();
            if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
                PathBuf::from(format!(r"\\{share}"))
            } else {
                PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
            }
        }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let root = self.root.path();
        let mut command = Command::new(program);
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("AGIT_HOME", root.join("agit"))
            .env("CODEX_HOME", root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMP", root.join("tmp"))
            .env("TEMP", root.join("tmp"))
            .env("TMPDIR", root.join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("empty-config"))
            .env("GIT_AUTHOR_NAME", "Status fixture")
            .env("GIT_AUTHOR_EMAIL", "status@example.invalid")
            .env("GIT_COMMITTER_NAME", "Status fixture")
            .env("GIT_COMMITTER_EMAIL", "status@example.invalid")
            .env(
                "AGIT_HUB_URL",
                format!("http://{}", self.hub.local_addr().unwrap()),
            )
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(self.cwd())
            .stdin(Stdio::null());
        command
    }

    fn git(&self, args: &[&str]) -> String {
        success(
            self.command("git")
                .arg("-C")
                .arg(self.repo.root())
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in ["work", "tmp", "agit/repos/alice/notes"] {
            fs::create_dir_all(root.path().join(name)).unwrap();
        }
        let repo = Repo::at(root.path().join("agit/repos/alice/notes"));
        let store = Store::at(root.path().join("agit/store"));
        let native = root
            .path()
            .join("codex/sessions/2026/09/10")
            .join(format!("rollout-2026-09-10T00-00-00-{SID}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        let prefix = format!(
            "{}\n{}{}",
            json!({"type":"session_meta","payload":{"id":SID}}),
            message("user", "PRIVATE_SETTLED_PROMPT"),
            message("assistant", "PRIVATE_SETTLED_REPLY")
        );
        fs::write(
            &native,
            format!(
                "{prefix}{}{}",
                message("user", "PRIVATE_PENDING_PROMPT"),
                message("assistant", "PRIVATE_PENDING_REPLY")
            ),
        )
        .unwrap();
        let mut claim = link::Link::new("codex", SID, None);
        claim.owner = Some("alice".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("topic".into());
        link::write(&store, &claim).unwrap();
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let mut lab = Self {
            root,
            repo,
            store,
            native,
            prefix,
            claim,
            head: String::new(),
            hub,
        };
        lab.git(&["init", "--quiet", "--initial-branch=main"]);
        lab.git(&["config", "commit.gpgsign", "false"]);
        lab.git(&["config", "core.logAllRefUpdates", "false"]);
        lab.git(&["config", "gc.auto", "0"]);
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(&lab.prefix, "codex", &session);
        storage::write_snapshot(lab.repo.root(), &log, &log).unwrap();
        meta::write(
            lab.repo.root(),
            &meta::Meta::new(session, "codex".into(), "/fixture".into()),
        )
        .unwrap();
        lab.git(&["add", "-A"]);
        lab.git(&["commit", "-m", "saved session prefix"]);
        lab.head = lab.git(&["rev-parse", "HEAD"]);
        lab.git(&["branch", "topic"]);
        lab
    }

    fn save(&self) {
        link::write(&self.store, &self.claim).unwrap();
    }

    fn commit_protected_prefix(&mut self, protected: &str) {
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(protected, "codex", &session);
        storage::write_snapshot(self.repo.root(), &log, &log).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", "protected session prefix"]);
        self.head = self.git(&["rev-parse", "HEAD"]);
        self.git(&["branch", "-f", "topic", "HEAD"]);
    }

    #[cfg(unix)]
    fn use_legacy_storage(&mut self) {
        let mut snapshot = meta::read(self.repo.root()).unwrap();
        let log = transcript::wrap_lines(&self.prefix, "codex", &snapshot.session);
        fs::remove_file(self.repo.root().join(meta::LOG_FILE)).unwrap();
        fs::remove_file(self.repo.root().join(meta::VIEW_FILE)).unwrap();
        fs::remove_dir_all(self.repo.root().join(meta::EVENTS_DIR)).unwrap();
        snapshot.layout = meta::LayoutVersion::V0;
        meta::write(self.repo.root(), &snapshot).unwrap();
        fs::write(self.repo.root().join(meta::LEGACY_LOG_FILE), &log).unwrap();
        fs::write(self.repo.root().join(meta::LEGACY_VIEW_FILE), &log).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", "legacy saved session prefix"]);
        self.head = self.git(&["rev-parse", "HEAD"]);
        self.git(&["branch", "-f", "topic", "HEAD"]);
    }

    fn expect(&self, pending: &str, badge: &str) {
        self.expect_versions(pending, badge, &[None, Some("1"), Some("2")]);
    }

    fn expect_versions(&self, pending: &str, badge: &str, versions: &[Option<&str>]) {
        self.expect_versions_using(pending, badge, versions, status_output);
    }

    fn expect_versions_using(
        &self,
        pending: &str,
        badge: &str,
        versions: &[Option<&str>],
        run_status: impl Fn(&mut Command) -> Output,
    ) {
        for &version in versions {
            let before = inventory(self.root.path());
            let head = self.git(&["symbolic-ref", "HEAD"]);
            let refs = self.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
            let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
            if let Some(version) = version {
                command.args(["--json", "--json-version", version]);
            }
            let output = run_status(command.arg("status"));
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = if let Some(version) = version {
                assert!(output.stderr.is_empty());
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
                assert_eq!(value["ok"], true);
                assert_eq!(value["result"]["format"], "json");
                let rows = value["result"]["value"]["sessions"]["items"]
                    .as_array()
                    .unwrap();
                let selected = rows
                    .iter()
                    .find(|row| row["session_id"] == SID && row["runtime"] == "codex")
                    .unwrap();
                assert_eq!(selected["owner"], "alice");
                assert_eq!(selected["repository_name"], "notes");
                assert_eq!(selected["branch"], "topic");
                assert_eq!(selected["target"], "alice/notes@topic");
                assert!(
                    selected["pending_activity"]
                        .as_str()
                        .unwrap()
                        .contains(pending)
                );
                assert_eq!(selected["local_instance"], badge);
                let expected_head = if badge.starts_with("current")
                    || badge.starts_with("busy")
                    || badge.starts_with("stale")
                    || badge.starts_with("conflicting")
                {
                    Some(meta::short(&meta::id_from_sha(&self.head)))
                } else {
                    None
                };
                assert_eq!(
                    selected["last_commit"],
                    serde_json::to_value(expected_head).unwrap()
                );
                assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE_"));
                None
            } else {
                Some(String::from_utf8(output.stdout).unwrap())
            };
            if let Some(text) = text {
                assert!(
                text.contains(
                    "session\truntime\trepo\tbranch\tlast commit\tpending activity\tlocal instance"
                ),
                "{text}"
            );
                let selected = text
                    .lines()
                    .find(|line| line.starts_with(&format!("{SID}\tcodex\t")))
                    .expect("selected session row missing");
                let fields = selected.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 7, "{text}");
                assert_eq!(fields[2], "alice/notes");
                assert_eq!(fields[3], "topic");
                if badge.starts_with("current")
                    || badge.starts_with("busy")
                    || badge.starts_with("stale")
                    || badge.starts_with("conflicting")
                {
                    assert_eq!(fields[4], meta::short(&meta::id_from_sha(&self.head)));
                } else {
                    assert_eq!(fields[4], "—");
                }
                assert!(fields[5].contains(pending), "{selected}");
                assert_eq!(fields[6], badge);
                assert!(!text.contains("PRIVATE_"), "native content leaked: {text}");
            }
            assert_eq!(
                inventory(self.root.path()),
                before,
                "status changed owned files or directories"
            );
            assert_eq!(self.git(&["symbolic-ref", "HEAD"]), head);
            assert_eq!(
                self.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(
                self.hub.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    fn check_status(&self, args: &[&str], env: &[(&str, &str)], check: impl Fn(&str)) {
        self.check_status_using_inventory(args, env, inventory, check);
    }

    fn check_status_using_inventory(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        read_inventory: impl Fn(&Path) -> BTreeMap<PathBuf, Entry>,
        check: impl Fn(&str),
    ) {
        for (quiet, version) in [
            (false, None),
            (true, None),
            (false, Some("1")),
            (false, Some("2")),
        ] {
            let before = read_inventory(self.root.path());
            let head = self.git(&["symbolic-ref", "HEAD"]);
            let refs = self.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
            let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
            command.envs(env.iter().copied());
            if quiet {
                command.arg("--quiet");
            }
            if let Some(version) = version {
                command.args(["--json", "--json-version", version]);
            }
            let output = status_output(command.arg("status").args(args));
            assert!(output.status.success(), "{output:?}");
            let text = if let Some(version) = version {
                assert!(output.stderr.is_empty());
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
                assert_eq!(value["ok"], true);
                status_diagnostics(&value["result"]["value"])
            } else {
                // Human warnings use stderr; structured diagnostics come from their JSON fields.
                format!(
                    "{}\n{}",
                    String::from_utf8(output.stdout).unwrap(),
                    String::from_utf8(output.stderr).unwrap()
                )
            };
            check(&text);
            assert!(!text.contains("PRIVATE_"), "native content leaked: {text}");
            assert_eq!(read_inventory(self.root.path()), before);
            assert_eq!(self.git(&["symbolic-ref", "HEAD"]), head);
            assert_eq!(
                self.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(
                self.hub.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    fn seed_runtime_index(&self) {
        let cwd = self.cwd();
        let path = self.root.path().join("codex/state_5.sqlite");
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
                 CREATE TABLE threads (
                    id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT,
                    first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER,
                    archived INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, cwd, first_user_message,
                 thread_source, updated_at_ms) VALUES (?1, ?2, ?3, ?4, 'user', 1)",
                rusqlite::params![
                    SID,
                    self.native.to_str().unwrap(),
                    cwd.to_str().unwrap(),
                    "PRIVATE_INDEX_PREVIEW"
                ],
            )
            .unwrap();
        connection.close().unwrap();
    }
}

// A carrier refusal must gate index discovery as well as native row inspection.
fn assert_carrier_result(output: Output, version: Option<&str>, complete: bool, missing: bool) {
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    if let Some(version) = version {
        assert!(diagnostics.is_empty());
        let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(envelope["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(envelope["ok"], true);
        let status = &envelope["result"]["value"];
        assert_eq!(status["sessions"]["items"], json!([]));
        assert_eq!(status["sessions"]["total"], 0);
        assert_eq!(status["sessions"]["incomplete"], !complete);
        assert_eq!(status["store_path"].is_null(), complete);
        assert_eq!(status["unadopted"]["checked"], missing);
        if missing {
            assert_eq!(status["unadopted"]["incomplete"], !complete);
            if complete {
                assert_eq!(status["unadopted"]["sessions"].as_array().unwrap().len(), 1);
                assert_eq!(status["unadopted"]["sessions"][0]["session_id"], SID);
                assert_eq!(status["unadopted"]["errors"], json!([]));
            } else {
                assert_eq!(status["unadopted"]["sessions"], json!([]));
                assert_eq!(
                    status["unadopted"]["errors"],
                    json!([{
                        "runtime": "claims",
                        "message": "unavailable: claim inventory incomplete; adoption cannot be determined"
                    }])
                );
            }
        } else {
            assert!(status["unadopted"]["sessions"].is_null());
            assert!(status["unadopted"]["incomplete"].is_null());
            assert!(status["unadopted"]["errors"].is_null());
        }
    } else {
        assert_eq!(
            text.contains("no sessions adopted yet."),
            complete,
            "{text}"
        );
        assert_eq!(text.contains("incomplete inventory"), !complete, "{text}");
        assert!(!text.contains("no unadopted sessions found"), "{text}");
        if missing {
            if complete {
                assert!(text.contains("1 sessions not adopted yet"), "{text}");
            } else {
                assert!(
                    diagnostics.contains("adoption cannot be determined"),
                    "stdout={text} stderr={diagnostics}"
                );
                assert!(!text.contains("sessions not adopted yet"), "{text}");
            }
        }
    }
    assert!(!text.contains("PRIVATE_"), "{text}");
    assert!(!diagnostics.contains("PRIVATE_"), "{diagnostics}");
}

fn check_carrier(lab: &Lab, complete: bool) {
    for missing in [false, true] {
        for (quiet, version) in [
            (false, None),
            (true, None),
            (false, Some("1")),
            (false, Some("2")),
        ] {
            let before = inventory(lab.root.path());
            let head = lab.git(&["symbolic-ref", "HEAD"]);
            let refs = lab.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
            let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
            if quiet {
                command.arg("--quiet");
            }
            if let Some(version) = version {
                command.args(["--json", "--json-version", version]);
            }
            command.arg("status");
            if missing {
                command.arg("--check-missing");
            }
            assert_carrier_result(command.output().unwrap(), version, complete, missing);
            assert_eq!(inventory(lab.root.path()), before);
            assert_eq!(lab.git(&["symbolic-ref", "HEAD"]), head);
            assert_eq!(
                lab.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(
                lab.hub.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
}

#[test]
fn missing_store_and_regular_file_carrier_have_distinct_inventory_results() {
    let lab = Lab::new();
    lab.seed_runtime_index();
    fs::rename(lab.store.root(), lab.root.path().join("retained-store")).unwrap();
    assert!(!lab.store.root().exists());
    check_carrier(&lab, true);
    fs::write(lab.store.root(), b"PRIVATE_UNREADABLE_CLAIMS").unwrap();
    check_carrier(&lab, false);
}

#[cfg(unix)]
#[test]
fn symlinked_and_dangling_store_carriers_do_not_enter_native_discovery() {
    let lab = Lab::new();
    lab.seed_runtime_index();
    let retained = lab.root.path().join("retained-store");
    fs::rename(lab.store.root(), &retained).unwrap();
    for target in [retained, lab.root.path().join("absent-store")] {
        std::os::unix::fs::symlink(target, lab.store.root()).unwrap();
        check_carrier(&lab, false);
        fs::remove_file(lab.store.root()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn unreadable_store_retains_unknown_claims_without_reading_runtime_indexes() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let lab = Lab::new();
    lab.seed_runtime_index();
    let home = lab.root.path().join("denied-home");
    let store = home.join("store");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("PRIVATE_CLAIM"), b"retained").unwrap();
    for path in [
        lab.root.path(),
        home.as_path(),
        &lab.root.path().join("work"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    for version in [None, Some("1"), Some("2")] {
        let before = inventory(lab.root.path());
        let refs = lab.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
        fs::set_permissions(&store, fs::Permissions::from_mode(0o0)).unwrap();
        let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
        command.env("AGIT_HOME", &home);
        // A privileged test runner must not bypass the carrier's read permission boundary.
        if unsafe { libc::geteuid() } == 0 {
            command.gid(65534).uid(65534);
        }
        if let Some(version) = version {
            command.args(["--json", "--json-version", version]);
        }
        let output = command.args(["status", "--check-missing"]).output();
        assert_eq!(
            fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0
        );
        fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
        assert_carrier_result(output.unwrap(), version, false, true);
        assert_eq!(inventory(lab.root.path()), before);
        assert_eq!(
            lab.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
            refs
        );
        assert_eq!(
            lab.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn check_missing_never_classifies_an_omitted_adoption_as_unadopted() {
    let lab = Lab::new();
    lab.seed_runtime_index();
    let claim_path = link::link_path(&lab.store, "codex", SID);
    let original = fs::read(&claim_path).unwrap();
    lab.check_status(&["--check-missing"], &[], |text| {
        assert!(
            text.contains("no unadopted sessions found in the checked indexes"),
            "{text}"
        );
        assert!(!text.contains("sessions not adopted yet"), "{text}");
    });
    let mut oversized = original.clone();
    oversized.resize(1024 * 1024 + 1, b' ');
    fs::write(&claim_path, oversized).unwrap();
    assert_eq!(link::read(&claim_path).unwrap().session_id, SID);
    lab.check_status(&["--check-missing"], &[], |text| {
        assert!(
            text.contains("unavailable: claim inventory incomplete; adoption cannot be determined"),
            "{text}"
        );
        assert!(!text.contains("sessions not adopted yet"), "{text}");
        assert!(
            !text.contains("no unadopted sessions found in the checked indexes"),
            "{text}"
        );
    });
    fs::write(&claim_path, &original).unwrap();
    lab.check_status(&["--check-missing"], &[], |text| {
        assert!(
            text.contains("no unadopted sessions found in the checked indexes"),
            "{text}"
        );
    });
    fs::remove_file(&claim_path).unwrap();
    lab.check_status(&[], &[], |text| {
        assert!(!text.contains("sessions not adopted yet"), "{text}");
        assert!(
            !text.contains("no unadopted sessions found in the checked indexes"),
            "{text}"
        );
    });
    lab.check_status(&["--check-missing"], &[], |text| {
        assert!(text.contains("1 sessions not adopted yet"), "{text}");
        assert!(text.contains(&link::short(SID)), "{text}");
        assert!(
            !text.contains("no unadopted sessions found in the checked indexes"),
            "{text}"
        );
    });
}

fn close_in_wal_mode(path: &Path) {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE wal_probe(value TEXT);
                        INSERT INTO wal_probe VALUES ('synthetic');",
        )
        .unwrap();
    connection.close().unwrap();
    assert_eq!(&fs::read(path).unwrap()[18..20], &[2, 2]);
    for suffix in ["-wal", "-shm"] {
        assert!(!PathBuf::from(format!("{}{suffix}", path.display())).exists());
    }
}

#[test]
fn default_status_never_creates_runtime_database_sidecars() {
    let mut lab = Lab::new();
    lab.seed_runtime_index();
    close_in_wal_mode(&lab.root.path().join("codex/state_5.sqlite"));
    lab.expect(
        "1 user turns with pending activity (1 newly started)",
        "current claim; process unverified",
    );

    fs::remove_file(link::link_path(&lab.store, "codex", SID)).unwrap();
    lab.claim.source = "opencode".into();
    lab.claim.session_id = "ses_status_wal_fixture".into();
    lab.save();
    let database = lab.root.path().join("data/opencode/opencode.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    close_in_wal_mode(&database);
    let metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "opencode".into(),
        "/fixture".into(),
    );
    meta::write(lab.repo.root(), &metadata).unwrap();
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "-m", "OpenCode metadata"]);
    lab.git(&["branch", "-f", "topic", "HEAD"]);
    lab.check_status(&[], &[], |text| {
        assert!(
            text.contains("unavailable: native database evidence is invalid"),
            "{text}"
        );
        assert!(!text.contains("no unsettled content"), "{text}");
    });
}

#[test]
fn status_context_uses_the_bounded_claim_snapshot_for_runtime_vetoes() {
    let lab = Lab::new();
    let mut oversized = lab.claim.to_json().unwrap();
    oversized.extend(std::iter::repeat_n(' ', 1024 * 1024));
    fs::write(lab.store.root().join("codex/unrelated.json"), oversized).unwrap();
    lab.check_status(&[], &[("AGIT_SESSION", "alice/notes@topic")], |text| {
        assert!(text.contains("alice/notes @ topic"), "{text}");
        assert!(text.contains("via: AGIT_SESSION"), "{text}");
        assert!(text.contains("incomplete inventory"), "{text}");
    });
    lab.check_status(
        &[],
        &[
            ("AGIT_SESSION", "alice/notes@topic"),
            ("CODEX_SESSION_ID", SID),
        ],
        |text| {
            assert!(
                text.contains("session target unavailable: runtime claim inventory is incomplete"),
                "{text}"
            );
            assert!(!text.contains("alice/notes @ topic"), "{text}");
        },
    );
}

#[test]
fn status_context_does_not_bypass_the_entry_budget_for_explicit_session_identity() {
    let lab = Lab::new();
    for index in 0..4100 {
        fs::write(
            lab.store.root().join(format!("codex/claim-{index}.json")),
            "{}",
        )
        .unwrap();
    }
    lab.check_status(&[], &[("AGIT_SESSION", "alice/notes@topic")], |text| {
        assert!(text.contains("alice/notes @ topic"), "{text}");
        assert!(text.contains("incomplete inventory"), "{text}");
    });
    lab.check_status(
        &[],
        &[
            ("AGIT_SESSION", "alice/notes@topic"),
            ("CODEX_SESSION_ID", SID),
        ],
        |text| {
            assert!(
                text.contains("session target unavailable: runtime claim inventory is incomplete"),
                "{text}"
            );
            assert!(!text.contains("alice/notes @ topic"), "{text}");
        },
    );
}

#[test]
fn status_runtime_claims_veto_but_never_replace_the_explicit_target() {
    let mut lab = Lab::new();
    lab.check_status(&[], &[("CODEX_SESSION_ID", SID)], |text| {
        assert!(
            text.contains("no session target supplied through AGIT_SESSION"),
            "{text}"
        );
        assert!(!text.contains("alice/notes @ topic"), "{text}");
    });
    lab.check_status(
        &[],
        &[
            ("AGIT_SESSION", "alice/notes@topic"),
            ("CODEX_SESSION_ID", SID),
        ],
        |text| assert!(text.contains("alice/notes @ topic"), "{text}"),
    );
    for target in ["alice/notes@other", "bob/notes@topic"] {
        lab.check_status(
            &[],
            &[("AGIT_SESSION", target), ("CODEX_SESSION_ID", SID)],
            |text| {
                assert!(
                    text.contains("no session target supplied through AGIT_SESSION"),
                    "{text}"
                );
                assert!(!text.contains("via: AGIT_SESSION"), "{text}");
            },
        );
    }
    lab.claim.owner = None;
    lab.save();
    lab.check_status(
        &[],
        &[
            ("AGIT_SESSION", "bob/notes@topic"),
            ("CODEX_SESSION_ID", SID),
        ],
        |text| assert!(text.contains("bob/notes @ topic"), "{text}"),
    );
    lab.claim.branch = None;
    lab.save();
    lab.check_status(
        &[],
        &[
            ("AGIT_SESSION", "bob/notes@topic"),
            ("CODEX_SESSION_ID", SID),
        ],
        |text| {
            assert!(
                text.contains("session target unavailable: runtime claim identity is incomplete"),
                "{text}"
            );
            assert!(!text.contains("via: AGIT_SESSION"), "{text}");
        },
    );
}

#[test]
fn status_counts_pending_turns_without_treating_a_claim_as_a_live_process() {
    let lab = Lab::new();
    lab.expect(
        "1 user turns with pending activity (1 newly started)",
        "current claim; process unverified",
    );
    fs::write(&lab.native, &lab.prefix).unwrap();
    lab.expect(
        "no unsettled content in the verified native snapshot",
        "current claim; process unverified",
    );
    fs::write(&lab.native, format!("{}{{\"type\":", lab.prefix)).unwrap();
    lab.expect(
        "incomplete trailing record",
        "current claim; process unverified",
    );
}

// A fork's settled prefix includes inherited records; reading only the leaf must not make a
// healthy adopted session unavailable or count a parent's later work as local activity.
#[test]
fn status_observes_codex_fork_history_without_opening_indexes_or_changing_carriers() {
    fn ordinal_message(ordinal: u64, role: &str, text: &str) -> String {
        let mut value: serde_json::Value = serde_json::from_str(&message(role, text)).unwrap();
        value["ordinal"] = ordinal.into();
        format!("{value}\n")
    }

    let mut lab = Lab::new();
    let parent_id = "00000000-0000-4000-8000-000000000018";
    let parent_path = lab
        .root
        .path()
        .join("codex/archived_sessions")
        .join(format!("rollout-2026-09-10T00-00-00-{parent_id}.jsonl"));
    fs::create_dir_all(parent_path.parent().unwrap()).unwrap();
    let parent_header = format!(
        "{}\n",
        json!({"ordinal":0,"type":"session_meta","payload":{"id":parent_id}})
    );
    let parent_body = format!(
        "{}{}",
        ordinal_message(1, "user", "PRIVATE_PARENT_SETTLED_PROMPT"),
        ordinal_message(2, "assistant", "PRIVATE_PARENT_SETTLED_REPLY")
    );
    let parent_prefix = format!("{parent_header}{parent_body}");
    let parent_full = format!(
        "{parent_prefix}{}",
        ordinal_message(3, "user", "PRIVATE_PARENT_CONTINUED_ELSEWHERE")
    );
    fs::write(&parent_path, &parent_full).unwrap();
    let child_header = format!(
        "{}\n",
        json!({
            "ordinal":3,"type":"session_meta","payload":{
                "id":SID,"history_mode":"paginated","forked_from_id":parent_id,
                "history_base":{
                    "thread_id":parent_id,"end_ordinal_exclusive":3,
                    "end_byte_offset":parent_prefix.len()
                }
            }
        })
    );
    let child_settled = format!(
        "{}{}",
        ordinal_message(4, "user", "PRIVATE_CHILD_SETTLED_PROMPT"),
        ordinal_message(5, "assistant", "PRIVATE_CHILD_SETTLED_REPLY")
    );
    let child_full = format!(
        "{child_header}{child_settled}{}{}",
        ordinal_message(6, "user", "PRIVATE_CHILD_PENDING_PROMPT"),
        ordinal_message(7, "assistant", "PRIVATE_CHILD_PENDING_REPLY")
    );
    fs::write(&lab.native, &child_full).unwrap();
    // A malformed index cannot authorize another carrier or create maintenance sidecars.
    fs::write(
        lab.root.path().join("codex/state_5.sqlite"),
        b"PRIVATE_INDEX_MUST_NOT_BE_OPENED",
    )
    .unwrap();
    let captured_prefix = format!("{child_header}{parent_body}{child_settled}");
    lab.commit_protected_prefix(&captured_prefix);
    let pending = "2 events, 1 user turns with pending activity (1 newly started), 0 ToolUse calls";
    let check = |expected: &str| {
        lab.check_status(&[], &[("AGIT_SESSION", "alice/notes@topic")], |text| {
            assert!(text.contains("alice/notes @ topic"), "{text}");
            assert!(text.contains(expected), "{text}");
            assert!(text.contains("current claim; process unverified"), "{text}");
            if expected.starts_with("unavailable:") || expected.contains("budget") {
                assert!(!text.contains(pending), "{text}");
                assert!(
                    !text.contains("no unsettled content in the verified native snapshot"),
                    "{text}"
                );
            } else {
                assert!(!text.contains("unavailable:"), "{text}");
            }
        });
    };
    check(pending);
    fs::remove_file(&parent_path).unwrap();
    check("unavailable: native session missing");
    fs::write(&parent_path, &parent_header).unwrap();
    check("unavailable: incomplete or inaccessible native evidence");
    fs::write(&parent_path, vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    check("unavailable: inspection budget exhausted");
    fs::write(&parent_path, &parent_full).unwrap();
    check(pending);
    fs::write(&lab.native, format!("{child_full}{{\"type\":")).unwrap();
    check("an incomplete trailing record is still being written");
    fs::write(&lab.native, &child_full).unwrap();
    check(pending);
}

#[test]
fn status_rejects_ancestor_repository_evidence_and_accepts_gitfiles() {
    let lab = Lab::new();
    let expected_git = lab.repo.root().join(".git");
    let ancestor_git = lab.repo.root().parent().unwrap().join(".git");
    fs::rename(&expected_git, &ancestor_git).unwrap();
    fs::create_dir(&expected_git).unwrap();
    lab.expect(
        "local branch missing or unreadable",
        "branch unavailable; process unverified",
    );
    for version in ["1", "2"] {
        let before = inventory(lab.root.path());
        let output = status_output(lab.command(env!("CARGO_BIN_EXE_agit")).args([
            "--json",
            "--json-version",
            version,
            "status",
        ]));
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let repositories = value["result"]["value"]["repositories"].as_array().unwrap();
        let selected = repositories
            .iter()
            .find(|row| row["repo"] == "alice/notes")
            .unwrap();
        assert!(selected["branches"]["items"].is_null());
        assert!(selected["branches"]["error"].as_str().is_some());
        assert_eq!(inventory(lab.root.path()), before);
        assert_eq!(
            lab.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    fs::remove_dir(&expected_git).unwrap();
    fs::rename(&ancestor_git, &expected_git).unwrap();
    lab.expect(
        "1 user turns with pending activity (1 newly started)",
        "current claim; process unverified",
    );

    let separate_git = lab.root.path().join("separate-agent.git");
    fs::rename(&expected_git, &separate_git).unwrap();
    fs::write(
        &expected_git,
        format!("gitdir: {}\n", separate_git.display()),
    )
    .unwrap();
    lab.expect(
        "1 user turns with pending activity (1 newly started)",
        "current claim; process unverified",
    );
    fs::remove_file(&expected_git).unwrap();
    fs::rename(&separate_git, &expected_git).unwrap();
}

#[test]
fn status_branch_listing_accepts_relative_agit_home() {
    let lab = Lab::new();
    for version in [None, Some("1"), Some("2")] {
        let before = inventory(lab.root.path());
        let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
        command.env("AGIT_HOME", "../agit");
        if let Some(version) = version {
            command.args(["--json", "--json-version", version]);
        }
        let output = status_output(command.arg("status"));
        assert!(output.status.success(), "{output:?}");
        if version.is_some() {
            assert!(output.stderr.is_empty());
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let repositories = value["result"]["value"]["repositories"].as_array().unwrap();
            let selected = repositories
                .iter()
                .find(|row| row["repo"] == "alice/notes")
                .unwrap();
            assert!(selected["branches"]["error"].is_null());
            let branch = selected["branches"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == "topic")
                .unwrap();
            assert_eq!(branch["head"], lab.head);
        } else {
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.lines().any(|line| {
                line.starts_with(&format!(
                    "alice/notes\ttopic\t{}\t",
                    meta::short(&meta::id_from_sha(&lab.head))
                ))
            }));
        }
        assert_eq!(inventory(lab.root.path()), before);
        assert_eq!(
            lab.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn status_distinguishes_busy_stale_superseded_and_missing_claim_evidence() {
    let mut lab = Lab::new();
    // Inventory reads bracket the held lock; Windows blocks reads through other handles.
    drop(link::lock(&lab.store, "codex", SID).unwrap());
    lab.expect_versions_using(
        "claim update in progress",
        "busy claim update; process unverified",
        &[None, Some("1"), Some("2")],
        |command| {
            let held = link::lock(&lab.store, "codex", SID).unwrap();
            let output = status_output(command);
            drop(held);
            output
        },
    );
    lab.expect(
        "1 user turns with pending activity",
        "current claim; process unverified",
    );
    lab.claim.materialized_from = Some("b".repeat(40));
    lab.save();
    lab.expect(
        "materialized baseline differs",
        "stale baseline; process unverified",
    );
    lab.claim.superseded_by = Some("codex/other-native-instance".into());
    lab.save();
    lab.expect(
        "not inspected: superseded instance",
        "superseded; process unverified",
    );
    lab.claim.materialized_from = None;
    lab.claim.superseded_by = None;
    lab.save();
    fs::remove_file(&lab.native).unwrap();
    lab.expect(
        "native session missing",
        "current claim; process unverified",
    );
    lab.git(&["update-ref", "-d", "refs/heads/topic"]);
    lab.expect(
        "local branch missing or unreadable",
        "branch unavailable; process unverified",
    );
}

#[test]
fn status_distinguishes_merge_exploration_from_superseded_instances() {
    let mut lab = Lab::new();
    let role = agit::domain::merge_archive::MergeArchiveRole {
        generation: uuid::Uuid::now_v7().to_string(),
        slug: "alice/notes".into(),
        branch: "topic".into(),
        origin_head: lab.head.clone(),
        logical_session: format!("agit-{}", "a".repeat(40)),
    };
    role.validate(lab.head.len()).unwrap();
    lab.claim.merge_archive = Some(role);
    lab.save();
    lab.expect(
        "not inspected: merge exploration",
        "merge exploration; process unverified",
    );
    fs::remove_file(&lab.native).unwrap();
    lab.expect(
        "not inspected: merge exploration",
        "merge exploration; process unverified",
    );
    lab.claim.superseded_by = Some("codex/other-native-instance".into());
    lab.save();
    lab.expect(
        "not inspected: superseded instance",
        "superseded; process unverified",
    );
}

#[test]
fn status_reports_oversized_native_evidence_and_competing_claims_without_guessing_zero() {
    let lab = Lab::new();
    fs::write(&lab.native, vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    lab.expect(
        "inspection budget exhausted",
        "current claim; process unverified",
    );
    let mut other = lab.claim.clone();
    other.session_id = "00000000-0000-4000-8000-000000000018".into();
    link::write(&lab.store, &other).unwrap();
    lab.expect(
        "competing local claims",
        "conflicting claims; process unverified",
    );
}

#[test]
fn status_stops_dense_native_records_at_the_record_budget() {
    let lab = Lab::new();
    let dense = format!("{}{}malformed\n", lab.prefix, "{}\n".repeat(16_384));
    assert!(dense.len() < 2 * 1024 * 1024);
    fs::write(&lab.native, dense).unwrap();
    lab.expect(
        "inspection budget exhausted",
        "current claim; process unverified",
    );
}

#[test]
fn malformed_claim_inventory_keeps_pending_counts_unavailable() {
    let lab = Lab::new();
    fs::write(lab.store.root().join("codex/broken.json"), "[]\n").unwrap();
    lab.expect(
        "claim inventory changed or incomplete",
        "claim unverified; process unverified",
    );
}

#[test]
fn small_protected_history_refuses_an_oversized_dictionary_without_readonly_writes() {
    let mut lab = Lab::new();
    let placeholder = format!("{{{{AGIT_SECRET_V1:{SID}:sec_{}}}}}", "a".repeat(32));
    let protected = lab.prefix.replace("PRIVATE_SETTLED_PROMPT", &placeholder);
    lab.commit_protected_prefix(&protected);
    let path = lab
        .repo
        .root()
        .join(".git/agit/secret-dictionary/vault.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, vec![b' '; 256 * 1024 + 1]).unwrap();
    lab.expect(
        "inspection budget exhausted",
        "current claim; process unverified",
    );
}

#[cfg(unix)]
#[test]
fn protected_pending_history_uses_a_small_existing_vault_and_refuses_large_file_keys() {
    use agit::domain::secret_filter::{FileKeyStore, RepositoryDictionary};
    let mut lab = Lab::new();
    let path = lab
        .repo
        .root()
        .join(".git/agit/secret-dictionary/vault.json");
    let key_dir = lab.root.path().join("agit/keystore");
    let dictionary = RepositoryDictionary::new(path.clone(), FileKeyStore::new(key_dir.clone()));
    dictionary
        .block_add("fixture", "PRIVATE_SETTLED_PROMPT".to_owned().into(), false)
        .unwrap();
    let protected = dictionary.protect_existing_jsonl(&lab.prefix).unwrap().text;
    assert!(protected.contains("{{AGIT_SECRET_V1:"));
    lab.commit_protected_prefix(&protected);
    let config = lab.root.path().join("agit/config.json");
    fs::write(config, r#"{"secrets.keystore":"file"}"#).unwrap();
    fs::remove_file(path.parent().unwrap().join("vault.lock")).unwrap();
    lab.expect(
        "1 user turns with pending activity (1 newly started)",
        "current claim; process unverified",
    );
    let key = fs::read_dir(key_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = fs::read(&key).unwrap();
    let mut oversized = original.clone();
    oversized.resize(65, b' ');
    fs::write(&key, &oversized).unwrap();
    lab.expect(
        "inspection budget exhausted",
        "current claim; process unverified",
    );
    fs::write(&key, original).unwrap();
    lab.expect(
        "1 user turns with pending activity (1 newly started)",
        "current claim; process unverified",
    );
}

#[test]
fn pagination_preserves_identity_rows_and_bounds_native_detail_inspection() {
    let lab = Lab::new();
    for index in 0..8 {
        let mut claim = lab.claim.clone();
        claim.session_id = format!("00000000-0000-4000-8000-{index:012}");
        claim.branch = Some(format!("unavailable-{index}"));
        link::write(&lab.store, &claim).unwrap();
    }
    for version in [None, Some("1"), Some("2")] {
        for (offset, limit) in [(0, 9), (8, 1)] {
            let before = inventory(lab.root.path());
            let refs = lab.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
            let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
            if let Some(version) = version {
                command.args(["--json", "--json-version", version]);
            }
            let output = command
                .args([
                    "status",
                    "--offset",
                    &offset.to_string(),
                    "--limit",
                    &limit.to_string(),
                ])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let expected = if offset == 0 {
                "unavailable: per-page inspection limit"
            } else {
                "2 events, 1 user turns with pending activity (1 newly started), 0 ToolUse calls"
            };
            if let Some(version) = version {
                assert!(output.stderr.is_empty(), "{output:?}");
                let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(document["schema_version"], version.parse::<u64>().unwrap());
                let page = &document["result"]["value"]["sessions"];
                assert_eq!(page["total"], 9);
                assert_eq!(page["offset"], offset);
                assert_eq!(page["limit"], limit);
                assert_eq!(page["incomplete"], false);
                assert!(page["next_offset"].is_null());
                let rows = page["items"].as_array().unwrap();
                assert_eq!(rows.len(), limit as usize);
                let row = rows.iter().find(|row| row["session_id"] == SID).unwrap();
                assert_eq!(row["runtime"], "codex");
                assert_eq!(row["pending_activity"], expected);
                assert_eq!(row["last_commit"].is_null(), offset == 0);
                assert_eq!(
                    row["local_instance"],
                    if offset == 0 {
                        "not inspected; process unverified"
                    } else {
                        "current claim; process unverified"
                    }
                );
            } else {
                assert_eq!(
                    std::str::from_utf8(&output.stderr).unwrap().trim(),
                    "→ --check-missing lists this repo’s unadopted sessions"
                );
                let text = std::str::from_utf8(&output.stdout).unwrap();
                let row = text
                    .lines()
                    .find(|line| line.starts_with(&format!("{SID}\tcodex\t")))
                    .unwrap();
                assert_eq!(row.split('\t').nth(5), Some(expected));
            }
            assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE_"));
            assert_eq!(inventory(lab.root.path()), before);
            assert_eq!(
                lab.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(
                lab.hub.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
}

// The writer stays in the test process; status must coordinate from its isolated child.
mod opencode_status {
    use super::*;
    use rusqlite::Connection;
    use serde_json::Value;
    use sha2::{Digest, Sha256};

    const SESSION: &str = "ses_status_selected";
    #[cfg(unix)]
    const INVENTORY_ROOT: &str = "AGIT_STATUS_TEST_INVENTORY_ROOT";
    #[cfg(unix)]
    const INVENTORY_PREFIX: &str = "AGIT_STATUS_TEST_INVENTORY=";

    // Closing a read alias in the writer process releases its POSIX SQLite locks.
    #[cfg(unix)]
    #[test]
    #[ignore = "owned inventory subprocess only"]
    fn inventory_child() {
        let Some(root) = std::env::var_os(INVENTORY_ROOT) else {
            return;
        };
        let root = PathBuf::from(root);
        assert!(root.is_absolute());
        println!(
            "\n{INVENTORY_PREFIX}{}",
            serde_json::to_string(&inventory(&root)).unwrap()
        );
    }

    fn message(id: &str, created: i64, data: Value) -> Value {
        json!({"id":id,"kind":"message","session_id":SESSION,"time_created":created,"data":data})
    }
    fn part(id: &str, host: &str, created: i64, data: Value) -> Value {
        json!({"id":id,"kind":"part","session_id":SESSION,"message_id":host,"time_created":created,"data":data})
    }
    fn baseline() -> Vec<Value> {
        vec![
            message("user", 10, json!({"role":"user"})),
            part(
                "prompt",
                "user",
                11,
                json!({"type":"text","text":"PRIVATE_SETTLED_PROMPT"}),
            ),
            message(
                "assistant",
                20,
                json!({"role":"assistant","parentID":"user"}),
            ),
            part(
                "reply",
                "assistant",
                21,
                json!({"type":"text","text":"PRIVATE_SETTLED_REPLY"}),
            ),
            part(
                "tool",
                "assistant",
                22,
                json!({"type":"tool","tool":"bash","callID":"original-call","state":{"status":"running","input":{}}}),
            ),
        ]
    }
    fn canonical(rows: &[Value]) -> String {
        let metadata = json!({"directory":"/fixture","id":SESSION,"kind":"opencode.meta","parent_id":null,"project_id":"project","time_created":1,"version":"fixture"});
        let mut records = vec![(1, 0, SESSION.to_owned(), metadata.to_string())];
        for row in rows {
            let id = row["id"].as_str().unwrap();
            let created = row["time_created"].as_i64().unwrap();
            let text = if row["kind"] == "message" {
                format!(
                    "{{\"id\":{},\"kind\":\"message\",\"session_id\":{},\"time_created\":{created},\"data\":{}}}",
                    json!(id),
                    json!(SESSION),
                    row["data"]
                )
            } else {
                format!(
                    "{{\"id\":{},\"kind\":\"part\",\"message_id\":{},\"session_id\":{},\"time_created\":{created},\"data\":{}}}",
                    json!(id),
                    row["message_id"],
                    json!(SESSION),
                    row["data"]
                )
            };
            records.push((created, u8::from(row["kind"] == "part"), id.into(), text));
        }
        records.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        records
            .into_iter()
            .map(|(_, _, _, text)| format!("{text}\n"))
            .collect()
    }
    struct Fixture {
        lab: Lab,
        path: PathBuf,
        writer: Option<Connection>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // The writer releases Windows handles before the owned home is removed.
            if let Some(writer) = self.writer.take() {
                let _ = writer.close();
            }
        }
    }
    impl Fixture {
        fn new() -> Self {
            let mut lab = Lab::new();
            fs::remove_file(link::link_path(&lab.store, "codex", SID)).unwrap();
            lab.claim = link::Link::new("opencode", SESSION, None);
            lab.claim.owner = Some("alice".into());
            lab.claim.agent = Some("notes".into());
            lab.claim.branch = Some("topic".into());
            lab.save();
            let path = lab.root.path().join("data/opencode/opencode.db");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let writer = Connection::open(&path).unwrap();
            writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
                CREATE TABLE session(id TEXT PRIMARY KEY,project_id TEXT,parent_id TEXT,directory TEXT,time_created INTEGER,version TEXT);
                CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);
                CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT);").unwrap();
            writer
                .execute(
                    "INSERT INTO session VALUES(?1,'project',NULL,'/fixture',1,'fixture')",
                    [SESSION],
                )
                .unwrap();
            // Unselected sessions cannot inflate the selected claim's semantic counts.
            writer.execute_batch("INSERT INTO session VALUES('ses_foreign','project',NULL,'/foreign',1,'fixture');
                INSERT INTO message VALUES('foreign-user','ses_foreign',30,'{\"role\":\"user\"}');
                INSERT INTO part VALUES('foreign-prompt','ses_foreign','foreign-user',31,'{\"type\":\"text\",\"text\":\"PRIVATE_FOREIGN_PROMPT\"}');").unwrap();
            let mut fixture = Self {
                lab,
                path,
                writer: Some(writer),
            };
            fixture.replace(&baseline());
            fixture.save_log(&baseline());
            fixture
                .writer()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
            fixture
        }
        fn writer(&self) -> &Connection {
            self.writer.as_ref().unwrap()
        }
        fn replace(&self, rows: &[Value]) {
            let transaction = self.writer().unchecked_transaction().unwrap();
            transaction
                .execute("DELETE FROM part WHERE session_id=?1", [SESSION])
                .unwrap();
            transaction
                .execute("DELETE FROM message WHERE session_id=?1", [SESSION])
                .unwrap();
            for row in rows {
                let id = row["id"].as_str().unwrap();
                let created = row["time_created"].as_i64().unwrap();
                if row["kind"] == "message" {
                    transaction
                        .execute(
                            "INSERT INTO message VALUES(?1,?2,?3,?4)",
                            rusqlite::params![id, SESSION, created, row["data"].to_string()],
                        )
                        .unwrap();
                } else {
                    transaction
                        .execute(
                            "INSERT INTO part VALUES(?1,?2,?3,?4,?5)",
                            rusqlite::params![
                                id,
                                SESSION,
                                row["message_id"].as_str().unwrap(),
                                created,
                                row["data"].to_string()
                            ],
                        )
                        .unwrap();
                }
            }
            transaction.commit().unwrap();
        }
        fn save_log(&mut self, rows: &[Value]) {
            let session = format!("agit-{}", "b".repeat(40));
            let log = transcript::wrap_lines(&canonical(rows), "opencode", &session);
            storage::write_snapshot(self.lab.repo.root(), &log, &log).unwrap();
            meta::write(
                self.lab.repo.root(),
                &meta::Meta::new(session, "opencode".into(), "/fixture".into()),
            )
            .unwrap();
            self.lab.git(&["add", "-A"]);
            self.lab.git(&["commit", "-m", "selected OpenCode prefix"]);
            self.lab.head = self.lab.git(&["rev-parse", "HEAD"]);
            self.lab.git(&["branch", "-f", "topic", "HEAD"]);
        }
        fn close(&mut self) {
            self.writer.take().unwrap().close().unwrap();
        }
        fn isolated_inventory(&self) -> BTreeMap<PathBuf, Entry> {
            #[cfg(not(unix))]
            {
                // Windows locks belong to the SQLite handle, not other handles opened for inventory.
                inventory(self.lab.root.path())
            }
            #[cfg(unix)]
            {
                let mut command = self.lab.command(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "opencode_status::inventory_child",
                        "--ignored",
                        "--nocapture",
                        "--test-threads",
                        "1",
                    ])
                    .env(INVENTORY_ROOT, self.lab.root.path());
                let output = status_output(&mut command);
                assert!(
                    output.status.success(),
                    "inventory subprocess failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(output.stderr.is_empty());
                let text = String::from_utf8(output.stdout).unwrap();
                let mut reports = text
                    .lines()
                    .filter_map(|line| line.strip_prefix(INVENTORY_PREFIX));
                let result =
                    serde_json::from_str(reports.next().expect("inventory report missing"))
                        .unwrap();
                assert!(reports.next().is_none(), "inventory report repeated");
                result
            }
        }
        fn check(&self, expected: &[&str], forbidden: &[&str]) {
            self.lab.check_status_using_inventory(
                &[],
                &[("AGIT_SESSION", "alice/notes@topic")],
                |_| self.isolated_inventory(),
                |text| {
                    assert!(text.contains("alice/notes @ topic"), "{text}");
                    for expected in expected {
                        assert!(text.contains(expected), "{text}");
                    }
                    for forbidden in forbidden {
                        assert!(!text.contains(forbidden), "{text}");
                    }
                },
            );
        }
    }

    #[test]
    fn selected_opencode_database_alone_counts_without_creating_sidecars() {
        let mut fixture = Fixture::new();
        let mut rows = baseline();
        rows.extend([
            message("next-user", 30, json!({"role":"user"})),
            part(
                "next-prompt",
                "next-user",
                31,
                json!({"type":"text","text":"PRIVATE_PENDING_PROMPT"}),
            ),
        ]);
        fixture.replace(&rows);
        fixture.close();
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!PathBuf::from(format!("{}{suffix}", fixture.path.display())).exists());
        }
        fixture.check(
            &[
                "2 added, 0 updated, 0 missing native events",
                "1 user turns with pending activity (1 newly started)",
                "0 new ToolUse calls",
                "current claim; process unverified",
            ],
            &["unavailable:"],
        );
    }

    #[test]
    fn selected_opencode_live_wal_keeps_revision_deletion_and_compaction_semantics() {
        let fixture = Fixture::new();
        fixture.check(
            &["no unsettled content in the verified native snapshot"],
            &["unavailable:"],
        );
        let mut revised = baseline();
        revised[3]["data"]["text"] = json!("PRIVATE_REVISED_REPLY");
        revised[4]["data"]["state"] =
            json!({"status":"completed","input":{},"output":"PRIVATE_TOOL_OUTPUT"});
        fixture.replace(&revised);
        assert!(
            fs::metadata(PathBuf::from(format!("{}-wal", fixture.path.display())))
                .unwrap()
                .len()
                > 32
        );
        fixture.check(
            &[
                "0 added, 2 updated, 0 missing native events",
                "1 user turns with pending activity (0 newly started)",
                "0 new ToolUse calls",
            ],
            &["unavailable:"],
        );
        revised.remove(3);
        fixture.replace(&revised);
        fixture.check(
            &[
                "1 missing native events",
                "semantic counts are lower bounds",
            ],
            &["no unsettled content", "unavailable:"],
        );
        let mut compacted = baseline();
        compacted.extend([
            message("boundary", 30, json!({"role":"user"})),
            part(
                "compaction",
                "boundary",
                31,
                json!({"type":"compaction","auto":true}),
            ),
            message(
                "summary",
                40,
                json!({"role":"assistant","parentID":"boundary","mode":"compaction"}),
            ),
            part(
                "summary-text",
                "summary",
                41,
                json!({"type":"text","text":"PRIVATE_COMPACTION"}),
            ),
        ]);
        fixture.replace(&compacted);
        fixture.check(
            &["0 newly started", "1 changed compactions"],
            &["unavailable:"],
        );
    }

    #[test]
    fn selected_opencode_materialized_baseline_is_verified_before_counting() {
        let mut fixture = Fixture::new();
        let original = canonical(&baseline());
        fixture.lab.claim.materialized_from = Some(fixture.lab.head.clone());
        fixture.lab.claim.baseline_bytes = Some(original.len() as u64);
        fixture.lab.claim.baseline_hash = Some(hex::encode(Sha256::digest(original.as_bytes())));
        fixture.lab.save();
        fixture.check(
            &["no unsettled content in the verified native snapshot"],
            &["unavailable:"],
        );
        let mut rows = baseline();
        rows.push(part(
            "continued",
            "assistant",
            50,
            json!({"type":"text","text":"PRIVATE_CONTINUED"}),
        ));
        fixture.replace(&rows);
        fixture.check(
            &["1 added, 0 updated", "0 newly started"],
            &["unavailable:"],
        );
        rows[1]["data"]["text"] = json!("PRIVATE_CHANGED_BASELINE");
        fixture.replace(&rows);
        fixture.check(
            &["unavailable:"],
            &["no unsettled content", "user turns with pending activity"],
        );
    }

    #[test]
    fn selected_opencode_missing_invalid_and_over_budget_databases_refuse_without_writes() {
        let mut fixture = Fixture::new();
        fixture
            .writer()
            .execute("DELETE FROM session WHERE id=?1", [SESSION])
            .unwrap();
        fixture.check(
            &["unavailable: native session missing"],
            &["no unsettled content", "user turns with pending activity"],
        );
        fixture.close();
        let original = fs::read(&fixture.path).unwrap();
        for bytes in [b"not SQLite".to_vec(), vec![0; 16 * 1024 * 1024 + 1]] {
            fs::write(&fixture.path, bytes).unwrap();
            fixture.check(
                &["unavailable:"],
                &["no unsettled content", "user turns with pending activity"],
            );
        }
        fs::write(&fixture.path, original).unwrap();
        fs::remove_file(&fixture.path).unwrap();
        fixture.check(
            &["unavailable:"],
            &["no unsettled content", "user turns with pending activity"],
        );
    }
}
