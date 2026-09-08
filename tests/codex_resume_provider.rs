//! Provider repair changes only the receiving runtime's portable configuration.

use serde_json::{Value, json};
use sha2::Digest as _;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead as _, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime},
};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const HUB: &str = "http://127.0.0.1:1";
const PRIVATE: &str = "SYNTHETIC-PRIVATE-CONFIG-MUST-NOT-ESCAPE";

fn main() {
    if std::env::args().nth(1).as_deref() == Some("app-server") {
        fake_app_server();
        return;
    }
    assert!(
        std::env::var_os("AGIT_TEST_CODEX_RPC_LOG").is_none(),
        "the fake runtime was invoked with an unexpected command"
    );
    let cases: &[(&str, fn())] = &[
        (
            "materialized provider and immutable source",
            materialized_provider_is_local,
        ),
        (
            "indexed resume and index precedence",
            indexed_resume_is_read_only,
        ),
        (
            "unindexed native resume",
            unindexed_resume_uses_rollout_header,
        ),
        (
            "registered provider identity",
            registered_provider_is_preserved,
        ),
        ("unknown registry", unknown_registry_is_not_reinterpreted),
        ("opaque provider identity", other_provider_names_are_opaque),
        ("unknown native index", unknown_index_is_not_reinterpreted),
        (
            "selected native index directory",
            selected_index_directory_is_respected,
        ),
        (
            "native index version authority",
            future_index_does_not_select_provider,
        ),
        (
            "invalid native metadata",
            invalid_native_metadata_is_not_reinterpreted,
        ),
        (
            "relative receiving directory",
            relative_cwd_is_resolved_once,
        ),
    ];
    for (name, run) in cases {
        println!("running: {name}");
        run();
        println!("passed: {name}");
    }
}

fn fake_app_server() {
    let log_path = std::env::var_os("AGIT_TEST_CODEX_RPC_LOG").expect("isolated RPC log");
    let response_path =
        std::env::var_os("AGIT_TEST_CODEX_RPC_RESPONSE").expect("isolated RPC response");
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .unwrap();
    let pid = std::process::id();
    writeln!(
        log,
        "{}",
        json!({"pid":pid,"event":"spawn","args":std::env::args().skip(1).collect::<Vec<_>>(),
            "cwd":std::env::current_dir().unwrap()})
    )
    .unwrap();
    log.flush().unwrap();
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(log, "{}", json!({"pid":pid,"request":request})).unwrap();
        log.flush().unwrap();
        let reply = match request["method"].as_str() {
            Some("initialize") => json!({"id":request["id"],"result":{}}),
            Some("initialized") => continue,
            Some("thread/read") => {
                let mut value = fake_thread_metadata(&request, Path::new(&response_path));
                value["id"] = request["id"].clone();
                writeln!(
                    log,
                    "{}",
                    json!({"pid":pid,"event":"thread/read-result","response":value})
                )
                .unwrap();
                log.flush().unwrap();
                value
            }
            Some("config/read") => {
                let mode = fs::read_to_string(&response_path).unwrap();
                if mode == "EOF" {
                    return;
                }
                let mut value: Value = serde_json::from_str(&mode).unwrap();
                value["id"] = request["id"].clone();
                writeln!(
                    log,
                    "{}",
                    json!({"pid":pid,"event":"config/read-result",
                        "config":value.pointer("/result/config"),
                        "sqlite_home_env":std::env::var_os("CODEX_SQLITE_HOME").map(PathBuf::from)})
                )
                .unwrap();
                log.flush().unwrap();
                value
            }
            method => panic!("the preparation queried a mutable native method: {method:?}"),
        };
        writeln!(stdout, "{reply}").unwrap();
        stdout.flush().unwrap();
    }
}

fn fake_thread_metadata(request: &Value, response_path: &Path) -> Value {
    if let Some(value) = std::env::var_os("AGIT_TEST_CODEX_METADATA_RESPONSE") {
        return serde_json::from_str(value.to_str().unwrap()).unwrap();
    }
    let sid = request["params"]["threadId"].as_str().unwrap();
    let provider = fake_native_provider(sid, response_path);
    match provider {
        Ok(provider) => json!({"result":{"thread":{"id":sid,"modelProvider":provider}}}),
        Err(()) => json!({"error":{"code":-32600,"message":PRIVATE}}),
    }
}

fn fake_native_provider(sid: &str, response_path: &Path) -> Result<String, ()> {
    use rusqlite::OptionalExtension as _;

    let native_home = PathBuf::from(std::env::var_os("CODEX_HOME").ok_or(())?);
    let config = fs::read(response_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let selected = config
        .as_ref()
        .and_then(|value| value.pointer("/result/config/sqlite_home"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CODEX_SQLITE_HOME").map(PathBuf::from))
        .unwrap_or_else(|| native_home.clone());
    let index = std::env::current_dir()
        .map_err(|_| ())?
        .join(selected)
        .join("state_5.sqlite");
    if index.exists() {
        let connection = rusqlite::Connection::open_with_flags(
            index,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| ())?;
        let provider = connection
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                [sid],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| ())?;
        if let Some(provider) = provider {
            return Ok(provider);
        }
    }
    for entry in walkdir::WalkDir::new(native_home.join("sessions")) {
        let entry = entry.map_err(|_| ())?;
        if !entry.file_type().is_file() {
            continue;
        }
        let text = fs::read_to_string(entry.path()).map_err(|_| ())?;
        let first = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .ok_or(())?;
        let header: Value = serde_json::from_str(first).map_err(|_| ())?;
        if header.pointer("/payload/id").and_then(Value::as_str) == Some(sid) {
            return header
                .pointer("/payload/model_provider")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(());
        }
    }
    Err(())
}

#[derive(Debug, PartialEq, Eq)]
struct SavedFile {
    bytes: Vec<u8>,
    modified: SystemTime,
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, SavedFile> {
    if !root.exists() {
        return BTreeMap::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                SavedFile {
                    bytes: fs::read(entry.path()).unwrap(),
                    modified: entry.metadata().unwrap().modified().unwrap(),
                },
            )
        })
        .collect()
}

fn ordinary_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = path.to_str().unwrap();
        if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{share}"));
        }
        PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
    }
    #[cfg(not(windows))]
    path.to_owned()
}

struct Lab {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    bin: PathBuf,
    native: PathBuf,
    rpc_log: PathBuf,
    response: PathBuf,
    sqlite_home: Option<PathBuf>,
    metadata_response: Option<Value>,
}

impl Lab {
    fn new(provider: &str, response: Value) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = ordinary_path(&temporary.path().canonicalize().unwrap());
        let home = root.join("home");
        let codex_home = home.join(".codex");
        let store = root.join("agit");
        let work = root.join("workspace with spaces");
        let bin = root.join("bin");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let native = codex_home
            .join("sessions/2026/09/08")
            .join(format!("rollout-2026-09-08T00-00-00-{SID}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        let records = [
            json!({"type":"session_meta","timestamp":"2026-09-08T00:00:00Z",
                "payload":{"id":SID,"cwd":work,"timestamp":"2026-09-08T00:00:00Z",
                    "model_provider":provider,"history_mode":"legacy",
                    "base_instructions":{"text":"SYNTHETIC-NATIVE-INSTRUCTIONS"},
                    "originator":"codex_cli_rs"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"SYNTHETIC-PROVIDER-QUESTION"}]}}),
            json!({"type":"response_item","payload":{"type":"reasoning",
                "encrypted_content":"opaque-native-evidence","summary":[]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":"SYNTHETIC-PROVIDER-ANSWER"}]}}),
        ];
        fs::write(
            &native,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        fs::write(home.join("empty-gitconfig"), "").unwrap();
        let executable = std::env::current_exe().unwrap();
        fs::copy(&executable, bin.join("codex")).unwrap();
        #[cfg(windows)]
        fs::copy(&executable, bin.join("codex.exe")).unwrap();
        let lab = Self {
            rpc_log: root.join("rpc.jsonl"),
            response: root.join("response.json"),
            _temporary: temporary,
            home,
            codex_home,
            store,
            work,
            bin,
            native,
            sqlite_home: None,
            metadata_response: None,
        };
        fs::write(&lab.response, response.to_string()).unwrap();
        agit::infra::credentials::save_at(
            &lab.store.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(HUB).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(HUB.into()),
                access_token: "synthetic".into(),
                refresh_token: "synthetic".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        lab.success(&["init", "qa", "--no-bind"]);
        lab.success(&["import", SID, "--from", "codex", "--into", "me/qa@work"]);
        assert!(
            !lab.rpc_log.exists(),
            "adoption queried native configuration"
        );
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let paths = std::iter::once(self.bin.clone()).chain(std::env::split_paths(&inherited_path));
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.home)
            .env("CODEX_HOME", &self.codex_home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_TEST_CODEX_RPC_LOG", &self.rpc_log)
            .env("AGIT_TEST_CODEX_RPC_RESPONSE", &self.response)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_YES", "1")
            .current_dir(&self.work);
        if let Some(sqlite_home) = &self.sqlite_home {
            command.env("CODEX_SQLITE_HOME", sqlite_home);
        }
        if let Some(response) = &self.metadata_response {
            command.env("AGIT_TEST_CODEX_METADATA_RESPONSE", response.to_string());
        }
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", &self.home);
        }
        command
    }

    fn success(&self, args: &[&str]) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
        command.args(args);
        let output = bounded_output(command);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let text = output_text(&output);
        assert!(
            !text.contains(PRIVATE),
            "private native configuration escaped: {args:?}"
        );
        output
    }

    fn git(&self, args: &[&str]) -> String {
        let mut command = self.command("git");
        command.arg("-C").arg(self.repo()).args(args);
        let output = bounded_output(command);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repo(&self) -> PathBuf {
        self.store.join("repos/me/qa")
    }

    fn seed_index(&self, provider: Option<&str>) -> PathBuf {
        self.seed_index_at(&self.codex_home, provider)
    }

    fn seed_index_at(&self, directory: &Path, provider: Option<&str>) -> PathBuf {
        fs::create_dir_all(directory).unwrap();
        let path = directory.join("state_5.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT,
                first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER,
                archived INTEGER NOT NULL DEFAULT 0, model_provider TEXT
            );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, cwd, first_user_message, thread_source,
                updated_at_ms, model_provider) VALUES (?1, ?2, ?3, ?4, 'user', ?5, ?6)",
                rusqlite::params![
                    SID,
                    self.native.to_str().unwrap(),
                    self.work.to_str().unwrap(),
                    "SYNTHETIC-PROVIDER-QUESTION",
                    1_788_825_600_000_i64,
                    provider
                ],
            )
            .unwrap();
        connection.close().unwrap();
        path
    }

    fn claim(&self, branch: &str) -> (String, Value) {
        let mut matching = Vec::new();
        for entry in fs::read_dir(self.store.join("store/codex")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if value["branch"] == branch && value["superseded_by"].is_null() {
                matching.push((
                    path.file_stem().unwrap().to_str().unwrap().to_owned(),
                    value,
                ));
            }
        }
        assert_eq!(
            matching.len(),
            1,
            "the branch does not have a unique active claim"
        );
        matching.pop().unwrap()
    }

    fn rollout(&self, id: &str) -> PathBuf {
        let paths: Vec<_> = walkdir::WalkDir::new(self.codex_home.join("sessions"))
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with(&format!("-{id}.jsonl"))
            })
            .collect();
        assert_eq!(
            paths.len(),
            1,
            "the native identity does not resolve uniquely"
        );
        paths.into_iter().next().unwrap()
    }

    fn assert_rpc(&self, queried: bool) {
        self.assert_rpc_at(queried, &self.work);
    }

    fn assert_native_metadata(&self, sid: &str, provider: &str) {
        let replies: Vec<Value> = fs::read_to_string(&self.rpc_log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|value| value["event"] == "thread/read-result")
            .collect();
        assert!(
            !replies.is_empty(),
            "native metadata did not return a result"
        );
        for reply in replies {
            assert!(reply["response"].get("error").is_none(), "{reply}");
            assert_eq!(
                reply
                    .pointer("/response/result/thread/id")
                    .and_then(Value::as_str),
                Some(sid)
            );
            assert_eq!(
                reply
                    .pointer("/response/result/thread/modelProvider")
                    .and_then(Value::as_str),
                Some(provider)
            );
        }
    }

    fn assert_rpc_at(&self, queried: bool, expected_cwd: &Path) {
        if !queried {
            assert!(
                !self.rpc_log.exists(),
                "a native preflight was unexpectedly started"
            );
            return;
        }
        let log = fs::read_to_string(&self.rpc_log).expect("configuration lookup did not execute");
        let mut methods: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let mut metadata_reads = 0;
        for line in log.lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            let pid = value["pid"].as_u64().unwrap();
            if value["event"] == "spawn" {
                assert_eq!(value["args"], json!(["app-server"]));
                assert_eq!(value["cwd"], json!(expected_cwd));
                assert!(methods.insert(pid, Vec::new()).is_none());
                continue;
            }
            if value["event"] == "config/read-result" {
                let response: Value =
                    serde_json::from_slice(&fs::read(&self.response).unwrap()).unwrap();
                assert_eq!(value["config"], json!(response.pointer("/result/config")));
                assert_eq!(value["sqlite_home_env"], json!(self.sqlite_home));
                continue;
            }
            if value["event"] == "thread/read-result" {
                assert_eq!(value["response"]["id"], 3);
                continue;
            }
            let request = &value["request"];
            let method = request["method"].as_str().unwrap();
            assert!(
                matches!(
                    method,
                    "initialize" | "initialized" | "thread/read" | "config/read"
                ),
                "a mutable native session operation was attempted: {method}"
            );
            if method == "initialize" {
                assert_eq!(request["id"], 1);
            }
            if method == "thread/read" {
                metadata_reads += 1;
                assert_eq!(request["id"], 3);
                assert_eq!(request["params"]["includeTurns"], false);
                let sid = request["params"]["threadId"].as_str().unwrap();
                assert_eq!(records(&self.rollout(sid))[0]["payload"]["id"], sid);
            }
            if method == "config/read" {
                assert_eq!(request["id"], 2);
                assert_eq!(request["params"]["cwd"], json!(expected_cwd));
                assert_eq!(request["params"]["includeLayers"], false);
            }
            methods.get_mut(&pid).unwrap().push(method.to_owned());
        }
        assert!(!methods.is_empty());
        assert!(
            metadata_reads > 0,
            "native provider metadata was never read"
        );
        for sequence in methods.values() {
            let sequence = sequence.iter().map(String::as_str).collect::<Vec<_>>();
            assert!(
                sequence == ["initialize", "initialized", "config/read"]
                    || sequence == ["initialize", "initialized", "thread/read", "config/read"]
                    || sequence == ["initialize", "initialized", "thread/read"],
                "unexpected native preflight sequence: {sequence:?}"
            );
        }
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore as _;
        for path in [
            self.store.join("secret-filter/vault.json"),
            self.repo().join(".git/agit/secret-dictionary/vault.json"),
        ] {
            if let Ok(bytes) = fs::read(path)
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && let Some(id) = value.get("vault_id").and_then(Value::as_str)
            {
                let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
            }
        }
    }
}

fn bounded_output(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the isolated CLI did not finish its preparation: {command:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn absent_registry() -> Value {
    json!({"result":{"config":{"model_providers":{},"private_probe":PRIVATE}}})
}

fn registered_registry() -> Value {
    json!({"result":{"config":{"model_providers":{"OpenAI":{
        "name":"A distinct custom provider","base_url":"http://127.0.0.1:1/custom",
        "env_key":"SYNTHETIC_PROVIDER_KEY"}},"private_probe":PRIVATE}}})
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_original_objects(lab: &Lab, original: &BTreeMap<PathBuf, SavedFile>) {
    let now = snapshot(&lab.repo().join(".git/objects"));
    for (path, saved) in original {
        assert_eq!(
            now.get(path).map(|file| &file.bytes),
            Some(&saved.bytes),
            "an existing Git object changed: {path:?}"
        );
    }
}

fn materialized_provider_is_local() {
    let lab = Lab::new("OpenAI", absent_registry());
    let original = fs::read(&lab.native).unwrap();
    let source_head = lab.git(&["rev-parse", "refs/heads/work"]);
    let raw_before = lab
        .success(&["show", "me/qa@work", "--raw", "--log-only"])
        .stdout;
    let objects = snapshot(&lab.repo().join(".git/objects"));
    lab.success(&[
        "fork",
        "me/qa@work",
        "-b",
        "portable",
        "--resume",
        "--no-launch",
    ]);
    let (id, claim) = lab.claim("portable");
    assert_ne!(id, SID);
    let installed = lab.rollout(&id);
    let installed_bytes = fs::read(&installed).unwrap();
    let materialized = records(&installed);
    assert_eq!(materialized[0]["payload"]["model_provider"], "openai");
    assert_eq!(materialized[0]["payload"]["id"], id);
    assert_eq!(
        materialized[0]["payload"]["base_instructions"],
        json!({"text":"SYNTHETIC-NATIVE-INSTRUCTIONS"})
    );
    assert!(
        materialized
            .iter()
            .any(|record| record["payload"]["encrypted_content"] == "opaque-native-evidence")
    );
    assert_eq!(claim["baseline_bytes"], installed_bytes.len() as u64);
    assert_eq!(
        claim["baseline_hash"],
        hex::encode(sha2::Sha256::digest(&installed_bytes))
    );
    assert_eq!(fs::read(&lab.native).unwrap(), original);
    assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), source_head);
    assert_eq!(
        lab.success(&["show", "me/qa@work", "--raw", "--log-only"])
            .stdout,
        raw_before
    );
    assert_original_objects(&lab, &objects);
    let native_before = snapshot(&lab.codex_home);
    let resumed = lab.success(&["resume", "me/qa@portable", "--no-launch"]);
    assert!(output_text(&resumed).contains("reusing the prepared runtime session"));
    assert!(output_text(&resumed).contains(&format!("codex resume {id}")));
    assert_eq!(snapshot(&lab.codex_home), native_before);
    assert_eq!(lab.claim("portable"), (id, claim));
    lab.assert_rpc(true);
}

fn indexed_resume_is_read_only() {
    for header in ["OpenAI", "openai", "custom"] {
        let lab = Lab::new(header, absent_registry());
        lab.seed_index(Some("OpenAI"));
        let before = snapshot(&lab.codex_home);
        let claim = lab.claim("work");
        let head = lab.git(&["rev-parse", "refs/heads/work"]);
        let objects = snapshot(&lab.repo().join(".git/objects"));
        let output = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        let text = output_text(&output);
        assert!(
            text.contains("reusing the local native session (zero-copy)"),
            "{text}"
        );
        assert!(text.contains(&format!("codex resume {SID}")), "{text}");
        assert!(text.contains("model_provider=\"openai\""), "{text}");
        assert_eq!(snapshot(&lab.codex_home), before);
        assert_eq!(lab.claim("work"), claim);
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
        assert_original_objects(&lab, &objects);
        lab.assert_rpc(true);
    }
}

fn unindexed_resume_uses_rollout_header() {
    for empty_index in [false, true] {
        let lab = Lab::new("OpenAI", absent_registry());
        if empty_index {
            let path = lab.seed_index(Some("OpenAI"));
            let connection = rusqlite::Connection::open(path).unwrap();
            connection.execute("DELETE FROM threads", []).unwrap();
            connection.close().unwrap();
        }
        let before = snapshot(&lab.codex_home);
        let output = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        let text = output_text(&output);
        assert!(
            text.contains("reusing the local native session (zero-copy)"),
            "{text}"
        );
        assert!(text.contains(&format!("codex resume {SID}")), "{text}");
        assert!(text.contains("model_provider=\"openai\""), "{text}");
        assert_eq!(snapshot(&lab.codex_home), before);
        lab.assert_rpc(true);
    }
}

fn registered_provider_is_preserved() {
    let lab = Lab::new("OpenAI", registered_registry());
    let index = lab.seed_index(Some("OpenAI"));
    let index_bytes = fs::read(&index).unwrap();
    let before = snapshot(&lab.codex_home);
    let resumed = lab.success(&["resume", "me/qa@work", "--no-launch"]);
    assert!(!output_text(&resumed).contains("model_provider="));
    assert_eq!(snapshot(&lab.codex_home), before);
    let forked = lab.success(&[
        "fork",
        "me/qa@work",
        "-b",
        "portable",
        "--resume",
        "--no-launch",
    ]);
    assert!(!output_text(&forked).contains("model_provider="));
    let (id, _) = lab.claim("portable");
    assert_eq!(
        records(&lab.rollout(&id))[0]["payload"]["model_provider"],
        "OpenAI"
    );
    assert_eq!(fs::read(index).unwrap(), index_bytes);
    lab.assert_rpc(true);
}

fn unknown_registry_is_not_reinterpreted() {
    let responses = [
        json!({"error":{"code":-32601,"message":PRIVATE}}).to_string(),
        json!({"result":{"config":{"private_probe":PRIVATE}}}).to_string(),
        json!({"result":{"config":{"model_providers":null,"private_probe":PRIVATE}}}).to_string(),
        json!({"result":{"config":{"model_providers":[],"private_probe":PRIVATE}}}).to_string(),
        "EOF".into(),
    ];
    for response in responses {
        let lab = Lab::new("OpenAI", absent_registry());
        fs::write(&lab.response, response).unwrap();
        let index = lab.seed_index(Some("OpenAI"));
        let index_bytes = fs::read(&index).unwrap();
        let before = snapshot(&lab.codex_home);
        let resumed = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        assert!(!output_text(&resumed).contains("model_provider="));
        assert_eq!(snapshot(&lab.codex_home), before);
        let forked = lab.success(&[
            "fork",
            "me/qa@work",
            "-b",
            "portable",
            "--resume",
            "--no-launch",
        ]);
        assert!(!output_text(&forked).contains("model_provider="));
        let (id, _) = lab.claim("portable");
        assert_eq!(
            records(&lab.rollout(&id))[0]["payload"]["model_provider"],
            "OpenAI"
        );
        assert_eq!(fs::read(index).unwrap(), index_bytes);
        lab.assert_rpc(true);
    }
}

fn other_provider_names_are_opaque() {
    for (header, indexed) in [
        ("openai", Some("openai")),
        ("OPENAI", Some("OPENAI")),
        ("OpenAi", Some("OpenAi")),
        ("azure", Some("azure")),
        ("OpenAI", Some("custom")),
    ] {
        let lab = Lab::new(header, absent_registry());
        lab.seed_index(indexed);
        let before = snapshot(&lab.codex_home);
        let resumed = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        assert!(!output_text(&resumed).contains("model_provider="));
        assert_eq!(snapshot(&lab.codex_home), before);
        lab.assert_rpc(true);
    }
}

fn unknown_index_is_not_reinterpreted() {
    for mode in ["null", "missing-column", "malformed"] {
        let lab = Lab::new("OpenAI", absent_registry());
        let path = lab.seed_index(None);
        match mode {
            "null" => {}
            "missing-column" => {
                let connection = rusqlite::Connection::open(&path).unwrap();
                connection
                    .execute("ALTER TABLE threads DROP COLUMN model_provider", [])
                    .unwrap();
                connection.close().unwrap();
            }
            "malformed" => fs::write(path, "SYNTHETIC-NOT-A-NATIVE-INDEX").unwrap(),
            _ => unreachable!(),
        }
        let before = snapshot(&lab.codex_home);
        let resumed = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        assert!(!output_text(&resumed).contains("model_provider="));
        assert_eq!(snapshot(&lab.codex_home), before);
        lab.assert_rpc(true);
    }
}

fn selected_index_directory_is_respected() {
    for mode in [
        "configured-alternate",
        "environment-alternate",
        "configured-default",
    ] {
        let mut lab = Lab::new("OpenAI", absent_registry());
        lab.seed_index(Some("OpenAI"));
        let alternate = lab.home.join("alternate sqlite home");
        lab.seed_index_at(&alternate, Some("custom"));
        let mut response = absent_registry();
        response["result"]["config"]["model_providers"]["custom"] = json!({
            "name":"The alternate index provider",
            "base_url":"http://127.0.0.1:1/custom",
            "env_key":"SYNTHETIC_PROVIDER_KEY"
        });
        match mode {
            "configured-alternate" => {
                response["result"]["config"]["sqlite_home"] = json!(alternate);
                lab.sqlite_home = Some(lab.codex_home.clone());
            }
            "environment-alternate" => lab.sqlite_home = Some(alternate.clone()),
            "configured-default" => {
                response["result"]["config"]["sqlite_home"] = json!(lab.codex_home);
                lab.sqlite_home = Some(alternate.clone());
            }
            _ => unreachable!(),
        }
        fs::write(&lab.response, response.to_string()).unwrap();
        let default_before = snapshot(&lab.codex_home);
        let alternate_before = snapshot(&alternate);
        let claim = lab.claim("work");
        let head = lab.git(&["rev-parse", "refs/heads/work"]);
        let output = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        let text = output_text(&output);
        assert!(
            text.contains("reusing the local native session (zero-copy)"),
            "{mode}: {text}"
        );
        assert!(
            text.contains(&format!("codex resume {SID}")),
            "{mode}: {text}"
        );
        if mode == "configured-default" {
            assert!(text.contains("model_provider=\"openai\""), "{mode}: {text}");
        } else {
            assert!(!text.contains("model_provider="), "{mode}: {text}");
        }
        assert_eq!(snapshot(&lab.codex_home), default_before, "{mode}");
        assert_eq!(snapshot(&alternate), alternate_before, "{mode}");
        assert_eq!(lab.claim("work"), claim, "{mode}");
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head, "{mode}");
        lab.assert_rpc(true);
        lab.assert_native_metadata(
            SID,
            if mode == "configured-default" {
                "OpenAI"
            } else {
                "custom"
            },
        );
    }
}

fn future_index_does_not_select_provider() {
    let mut response = absent_registry();
    response["result"]["config"]["model_providers"]["custom"] = json!({
        "name":"The current native index provider",
        "base_url":"http://127.0.0.1:1/custom",
        "env_key":"SYNTHETIC_PROVIDER_KEY"
    });
    let lab = Lab::new("OpenAI", response);
    lab.seed_index(Some("custom"));
    let future = lab.seed_index_at(&lab.home.join("future index fixture"), Some("OpenAI"));
    fs::rename(future, lab.codex_home.join("state_6.sqlite")).unwrap();
    let before = snapshot(&lab.codex_home);
    let claim = lab.claim("work");
    let head = lab.git(&["rev-parse", "refs/heads/work"]);
    let output = lab.success(&["resume", "me/qa@work", "--no-launch"]);
    let text = output_text(&output);
    assert!(
        text.contains("reusing the local native session (zero-copy)"),
        "{text}"
    );
    assert!(text.contains(&format!("codex resume {SID}")), "{text}");
    assert!(!text.contains("model_provider="), "{text}");
    assert_eq!(snapshot(&lab.codex_home), before);
    assert_eq!(lab.claim("work"), claim);
    assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
    lab.assert_rpc(true);
    lab.assert_native_metadata(SID, "custom");
}

fn invalid_native_metadata_is_not_reinterpreted() {
    for response in [
        json!({"error":{"code":-32600,"message":PRIVATE}}),
        json!({"result":{"thread":{"id":"another-native-thread","modelProvider":"OpenAI"}}}),
        json!({"result":{"thread":{"id":SID,"modelProvider":""}}}),
        json!({"result":{"thread":{"id":SID,"modelProvider":7}}}),
        json!({"result":{"thread":{"id":SID}}}),
        json!({"result":{}}),
    ] {
        let mut lab = Lab::new("OpenAI", absent_registry());
        lab.seed_index(Some("OpenAI"));
        lab.metadata_response = Some(response);
        let before = snapshot(&lab.codex_home);
        let claim = lab.claim("work");
        let output = lab.success(&["resume", "me/qa@work", "--no-launch"]);
        let text = output_text(&output);
        assert!(!text.contains("model_provider="), "{text}");
        assert!(text.contains(&format!("codex resume {SID}")), "{text}");
        assert_eq!(snapshot(&lab.codex_home), before);
        assert_eq!(lab.claim("work"), claim);
        lab.assert_rpc(true);
        for line in fs::read_to_string(&lab.rpc_log).unwrap().lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_ne!(
                value.pointer("/request/method").and_then(Value::as_str),
                Some("config/read")
            );
        }
    }
}

fn relative_cwd_is_resolved_once() {
    let lab = Lab::new("OpenAI", absent_registry());
    let relative = "target subdir's";
    let destination = lab.work.join(relative);
    fs::create_dir(&destination).unwrap();
    let original = fs::read(&lab.native).unwrap();
    let source_head = lab.git(&["rev-parse", "refs/heads/work"]);
    let output = lab.success(&[
        "fork",
        "me/qa@work",
        "-b",
        "portable",
        "--resume",
        "--no-launch",
        "--cwd",
        relative,
    ]);
    let (id, _) = lab.claim("portable");
    let text = output_text(&output);
    let quoted = format!("'{}'", destination.to_string_lossy().replace('\'', "'\\''"));
    assert!(
        text.contains(&format!("codex resume {id} --cd {quoted}")),
        "{text}"
    );
    let doubled = destination.join(relative);
    assert!(!text.contains(doubled.to_string_lossy().as_ref()), "{text}");
    assert_eq!(
        records(&lab.rollout(&id))[0]["payload"]["model_provider"],
        "openai"
    );
    assert_eq!(fs::read(&lab.native).unwrap(), original);
    assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), source_head);
    lab.assert_rpc_at(true, &destination);
}
