//! Quiet output removes routine notices while preserving results and command authority.

use std::path::PathBuf;
use std::process::{Command, Output};

const HUB: &str = "http://127.0.0.1:1";
const SESSION: &str = "bbbbbbbb-0000-4000-8000-000000000001";

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let store = root.path().join("store");
        std::fs::create_dir_all(&home).unwrap();
        Self { root, home, store }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", HUB)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .env("NO_COLOR", "1")
            .current_dir(self.root.path());
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn quiet_command(&self, args: &[&str], mode: &str) -> Command {
        let mut command = self.command(args);
        if mode == "flag" {
            command.arg("--quiet");
        } else if mode != "ordinary" {
            command.env("AGIT_QUIET", mode);
        }
        command
    }

    fn signed_in(&self) {
        agit::infra::credentials::save_at(
            &self.store.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(HUB).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(HUB.into()),
                access_token: "SYNTHETIC".into(),
                refresh_token: "SYNTHETIC".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    fn seed(&self) {
        self.signed_in();
        success(self.run(&["init", "quiet", "--no-bind"]));
        let directory = self.home.join(".codex/sessions/2026/09/09");
        std::fs::create_dir_all(&directory).unwrap();
        let events = [
            serde_json::json!({"type":"session_meta","payload":{"id":SESSION,"cwd":self.root.path()}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"SYNTHETIC-QUIET-QUESTION"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"SYNTHETIC-QUIET-ANSWER"}]}}),
        ];
        std::fs::write(
            directory.join(format!("rollout-2026-09-09T00-00-00-{SESSION}.jsonl")),
            events.map(|event| format!("{event}\n")).concat(),
        )
        .unwrap();
        success(self.run(&[
            "import",
            SESSION,
            "--from",
            "codex",
            "--into",
            "me/quiet@work",
            "--independent",
        ]));
    }

    fn repo(&self) -> agit::domain::repo::Repo {
        agit::domain::repo::Repo::open(self.store.join("repos/me/quiet")).unwrap()
    }

    fn seed_claude_memory(&self) -> PathBuf {
        self.signed_in();
        success(self.run(&["init", "quiet", "--no-bind"]));
        let repo = self.repo();
        std::fs::create_dir_all(repo.root().join("memory")).unwrap();
        std::fs::write(repo.root().join("memory/team.md"), "SYNTHETIC-SHARED\n").unwrap();
        success(self.run(&[
            "commit",
            "me/quiet@main",
            "-m",
            "docs: shared memory",
            "--",
            "memory/team.md",
        ]));
        let cwd = self.root.path().canonicalize().unwrap();
        let config = self.home.join(".claude");
        let project = config
            .join("projects")
            .join(agit::adapter::claude_code::slug_for(&cwd));
        let memory = self.root.path().join("runtime-memory");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(
            config.join("settings.json"),
            serde_json::json!({"autoMemoryDirectory":memory}).to_string(),
        )
        .unwrap();
        let events = [
            serde_json::json!({"type":"user", "sessionId":SESSION, "cwd":cwd,
                "uuid":"synthetic-user", "message":{"role":"user", "content":"SYNTHETIC-MEMORY-QUESTION"}}),
            serde_json::json!({"type":"assistant", "sessionId":SESSION, "cwd":cwd,
                "uuid":"synthetic-assistant", "message":{"role":"assistant", "content":"SYNTHETIC-MEMORY-ANSWER"}}),
        ];
        std::fs::write(
            project.join(format!("{SESSION}.jsonl")),
            events.map(|event| format!("{event}\n")).concat(),
        )
        .unwrap();
        success(self.run(&[
            "import",
            SESSION,
            "--from",
            "claude-code",
            "--into",
            "me/quiet@work",
            "--independent",
        ]));
        memory
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = std::fs::read(self.store.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    output
}

fn silent(output: Output) {
    let output = success(output);
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn quiet_config_mutates_silently_but_getters_remain_data() {
    let lab = Lab::new();
    let ordinary = success(lab.run(&["config", "runtime.default", "codex"]));
    assert!(String::from_utf8_lossy(&ordinary.stdout).contains("runtime.default = codex"));
    silent(lab.run(&["-q", "config", "runtime.default", "claude-code"]));
    assert_eq!(
        success(lab.run(&["-q", "config", "runtime.default"])).stdout,
        b"claude-code\n"
    );
    for value in ["", "1"] {
        silent(
            lab.command(&["config", "runtime.default", "codex"])
                .env("AGIT_QUIET", value)
                .output()
                .unwrap(),
        );
        assert_eq!(
            success(lab.run(&["config", "runtime.default"])).stdout,
            b"codex\n"
        );
    }
    silent(lab.run(&["-q", "config", "--unset", "runtime.default"]));
    assert!(
        String::from_utf8_lossy(&success(lab.run(&["-q", "config", "--list"])).stdout)
            .contains("runtime.default")
    );
}

#[test]
fn quiet_does_not_turn_an_explicit_commit_into_hook_settlement() {
    let lab = Lab::new();
    let regular = lab.run(&["commit", "me/quiet@work"]);
    let quiet = lab.run(&["-q", "commit", "me/quiet@work"]);
    assert_eq!(regular.status.code(), Some(5), "{regular:?}");
    assert_eq!(quiet.status.code(), regular.status.code(), "{quiet:?}");
    assert_eq!(quiet.stderr, regular.stderr);
    assert!(!quiet.stderr.is_empty());
    assert!(!lab.store.join("repos").exists());
}

#[test]
fn settled_noop_and_file_commits_keep_their_writes_without_progress_output() {
    let lab = Lab::new();
    lab.seed();
    let repo = lab.repo();
    let old = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    let regular = success(lab.run(&["commit", "me/quiet@work"]));
    assert!(String::from_utf8_lossy(&regular.stdout).contains("nothing new since"));
    silent(lab.run(&["-q", "commit", "me/quiet@work"]));
    assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), old);
    std::fs::write(repo.root().join("README.md"), "# SYNTHETIC-QUIET-FILE\n").unwrap();
    silent(lab.run(&[
        "-q",
        "commit",
        "me/quiet@main",
        "-m",
        "docs: save shared file",
        "--",
        "README.md",
    ]));
    assert_eq!(
        repo.git(&["show", "refs/heads/main:README.md"])
            .unwrap()
            .trim(),
        "# SYNTHETIC-QUIET-FILE"
    );
}

#[test]
fn quiet_mcp_preserves_noop_and_settlement_results() {
    use std::io::Write as _;
    use std::process::Stdio;

    fn call(lab: &Lab, quiet: Option<&str>) -> String {
        let mut command = lab.command(if quiet == Some("flag") {
            &["-q", "mcp"]
        } else {
            &["mcp"]
        });
        command.env("AGIT_SESSION", "me/quiet@work");
        if let Some(value) = quiet.filter(|value| *value != "flag") {
            command.env("AGIT_QUIET", value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"commit\",\"arguments\":{}}}\n",
            )
            .unwrap();
        let output = success(child.wait_with_output().unwrap());
        assert!(output.stderr.is_empty(), "{output:?}");
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["id"], 1);
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    for quiet in ["flag", "", "1"] {
        let lab = Lab::new();
        lab.seed();
        let repo = lab.repo();
        let before = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
        let regular = call(&lab, None);
        assert!(regular.contains("nothing new since"), "{regular}");
        assert_eq!(call(&lab, Some(quiet)), regular);
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), before);

        let path = lab.home.join(format!(
            ".codex/sessions/2026/09/09/rollout-2026-09-09T00-00-00-{SESSION}.jsonl"
        ));
        let events = [
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"SYNTHETIC-QUIET-MCP-QUESTION"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"SYNTHETIC-QUIET-MCP-ANSWER"}]}}),
        ];
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(events.map(|event| format!("{event}\n")).concat().as_bytes())
            .unwrap();
        let native = std::fs::read(&path).unwrap();
        let result = call(&lab, Some(quiet));
        assert!(result.contains("settled"), "{result}");
        let after = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
        assert_ne!(after, before);
        let saved = success(lab.run(&["-q", "show", "me/quiet@work", "--raw", "--log-only"]));
        assert!(String::from_utf8_lossy(&saved.stdout).contains("SYNTHETIC-QUIET-MCP-ANSWER"));
        assert_eq!(std::fs::read(path).unwrap(), native);
        assert!(call(&lab, Some(quiet)).contains("nothing new since"));
        silent(lab.run(&["-q", "commit", "me/quiet@work"]));
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), after);
    }
}

#[test]
fn successful_push_noop_is_informational_and_quiet_suppresses_it() {
    let lab = Lab::new();
    lab.seed();
    let repo = lab.repo();
    for branch in repo.local_branches() {
        let head = repo
            .git(&["rev-parse", &format!("refs/heads/{branch}")])
            .unwrap();
        repo.git(&[
            "update-ref",
            &format!("refs/remotes/origin/{branch}"),
            head.trim(),
        ])
        .unwrap();
    }
    let ordinary = success(lab.run(&["push", "me/quiet", "--all"]));
    assert!(String::from_utf8_lossy(&ordinary.stdout).contains("nothing to push"));
    assert!(!String::from_utf8_lossy(&ordinary.stderr).contains("error"));
    silent(lab.run(&["-q", "push", "me/quiet", "--all"]));
    let refused = lab.run(&["-q", "push", "me/quiet", "-b", "missing"]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no local branch"));
}

#[test]
fn quiet_keeps_requested_history_raw_and_scan_results() {
    let lab = Lab::new();
    lab.seed();
    for (args, notice) in [
        (
            vec!["log", "me/quiet@work"],
            "target: me/quiet@work (via explicit arguments)\n",
        ),
        (vec!["show", "me/quiet@work", "--raw"], ""),
        (
            vec!["scan", "me/quiet@work", "--secrets"],
            "target: repo=me/quiet (via explicit arguments)\n",
        ),
    ] {
        let ordinary = success(lab.run(&args));
        assert!(!ordinary.stdout.is_empty(), "{args:?}");
        let mut selected = vec!["-q"];
        selected.extend_from_slice(&args);
        let quiet = success(lab.run(&selected));
        assert_eq!(
            quiet.stdout,
            ordinary.stdout.strip_prefix(notice.as_bytes()).unwrap(),
            "{args:?}"
        );
    }
}

#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn quiet_preserves_json_envelopes_and_typed_authentication_recovery() {
    let lab = Lab::new();
    for version in ["1", "2"] {
        let output = lab.run(&[
            "-q",
            "--json",
            "--json-version",
            version,
            "commit",
            "me/quiet@work",
        ]);
        assert_eq!(output.status.code(), Some(5));
        assert!(output.stderr.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(value["exit_code"], 5);
        assert!(
            !value["diagnostics"]["stderr"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        if version == "2" {
            assert_eq!(value["fix"][0]["argv"][1], "login");
        } else {
            assert!(value.get("fix").is_none());
        }
        let getter = success(lab.run(&[
            "-q",
            "--json",
            "--json-version",
            version,
            "config",
            "commit.auto",
        ]));
        let data: serde_json::Value = serde_json::from_slice(&getter.stdout).unwrap();
        assert_eq!(data["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(data["result"]["format"], "json");
        assert_eq!(data["result"]["value"]["operation"], "get");
        assert_eq!(data["result"]["value"]["setting"]["key"], "commit.auto");
        assert!(data["result"]["value"]["setting"]["stored"].is_null());
        assert_eq!(data["result"]["value"]["setting"]["effective"], "true");
    }
}

#[test]
fn quiet_retains_the_half_written_tail_diagnostic_without_settling_it() {
    use std::io::Write as _;
    let lab = Lab::new();
    lab.seed();
    let path = lab.home.join(format!(
        ".codex/sessions/2026/09/09/rollout-2026-09-09T00-00-00-{SESSION}.jsonl"
    ));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":")
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let repo = lab.repo();
    let head = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    let output = success(lab.run(&["-q", "commit", "me/quiet@work"]));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("half-written trailing line"), "{output:?}");
    assert!(!text.contains("nothing new since"), "{output:?}");
    assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), head);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn explicit_empty_branches_refuse_for_both_target_spellings() {
    let lab = Lab::new();
    lab.seed();
    let repo = lab.repo();
    repo.git(&["switch", "--create", "empty", "main"]).unwrap();
    let meta = agit::domain::meta::Meta::new_session_line(
        "codex".into(),
        lab.root.path().to_string_lossy().into_owned(),
    );
    agit::domain::meta::write(repo.root(), &meta).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "agit: declare empty session"])
        .unwrap();
    for branch in repo
        .local_branches()
        .into_iter()
        .filter(|name| name != "empty")
    {
        let head = repo
            .git(&["rev-parse", &format!("refs/heads/{branch}")])
            .unwrap();
        repo.git(&[
            "update-ref",
            &format!("refs/remotes/origin/{branch}"),
            head.trim(),
        ])
        .unwrap();
    }
    let before = repo.git(&["show-ref"]).unwrap();
    for args in [
        vec!["-q", "push", "me/quiet@empty"],
        vec!["-q", "push", "me/quiet", "-b", "empty"],
    ] {
        let output = lab.run(&args);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("no settled turns"),
            "{output:?}"
        );
    }
    silent(lab.run(&["-q", "push", "me/quiet", "--all"]));
    assert_eq!(repo.git(&["show-ref"]).unwrap(), before);
}

#[cfg(unix)]
fn add_secret(lab: &Lab, args: &[&str]) -> Output {
    submit_secret(lab.command(args))
}

#[cfg(unix)]
fn submit_secret(mut command: Command) -> Output {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"SYNTHETIC-QUIET-SECRET-FOR-TESTING\n")
        .unwrap();
    success(child.wait_with_output().unwrap())
}

#[cfg(unix)]
#[test]
fn quiet_secret_creation_preserves_identifiers_and_followup_behavior() {
    let mut ordinary_repository_policy = None;
    for mode in ["ordinary", "flag", "", "1"] {
        for repository_rule in [false, true] {
            let lab = Lab::new();
            lab.signed_in();
            success(lab.run(&["init", "quiet", "--no-bind"]));
            let repo = lab.repo();
            let repo_path = repo.root().to_str().unwrap();
            let add = if repository_rule {
                vec![
                    "secrets",
                    "block",
                    "add",
                    "quiet-created",
                    "--stdin",
                    "--repo",
                    repo_path,
                ]
            } else {
                vec!["secrets", "add", "quiet-created", "--stdin"]
            };
            let output = submit_secret(lab.quiet_command(&add, mode));
            assert!(output.stderr.is_empty(), "{output:?}");
            let text = std::str::from_utf8(&output.stdout).unwrap();
            assert!(text.contains("quiet-created"), "{text}");
            assert!(!text.contains("SYNTHETIC-QUIET-SECRET-FOR-TESTING"));
            let id = text
                .trim()
                .rsplit_once('(')
                .unwrap()
                .1
                .strip_suffix(')')
                .unwrap();
            assert!(!id.is_empty());
            let listing = if repository_rule {
                vec!["secrets", "review", "--repo", repo_path, "--json"]
            } else {
                vec!["secrets", "list", "--json"]
            };
            let records: serde_json::Value =
                serde_json::from_slice(&success(lab.run(&listing)).stdout).unwrap();
            let records = records["result"]["value"].as_array().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["id"], id);
            assert_eq!(records[0]["name"], "quiet-created");
            let remove = if repository_rule {
                vec!["secrets", "block", "remove", id, "--repo", repo_path]
            } else {
                vec!["secrets", "remove", id, "--yes"]
            };
            let removed = success(lab.quiet_command(&remove, mode).output().unwrap());
            if mode != "ordinary" {
                assert!(removed.stdout.is_empty(), "{removed:?}");
                assert!(removed.stderr.is_empty(), "{removed:?}");
            }
            let after: serde_json::Value =
                serde_json::from_slice(&success(lab.run(&listing)).stdout).unwrap();
            let after = after["result"]["value"].as_array().unwrap();
            if repository_rule {
                assert_eq!(after.len(), 1);
                assert_eq!(after[0]["id"], id);
                let policy = serde_json::json!({
                    "origins": after[0]["origins"],
                    "heuristic_disposition": after[0]["heuristic_disposition"],
                    "explicit_block": after[0]["explicit_block"],
                    "effective_protect": after[0]["effective_protect"],
                });
                if mode == "ordinary" {
                    ordinary_repository_policy = Some(policy);
                } else {
                    assert_eq!(Some(policy), ordinary_repository_policy);
                }
            } else {
                assert!(after.is_empty());
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn initialized_vault_status_remains_requested_data_under_quiet() {
    let lab = Lab::new();
    add_secret(&lab, &["secrets", "add", "quiet-status", "--stdin"]);
    let regular = success(lab.run(&["secrets", "status"]));
    assert!(String::from_utf8_lossy(&regular.stdout).contains("vault is healthy"));
    assert_eq!(
        success(lab.run(&["-q", "secrets", "status"])).stdout,
        regular.stdout
    );
}

#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn quiet_json_mutations_keep_their_complete_command_result() {
    let lab = Lab::new();
    for version in ["1", "2"] {
        let args = [
            "--json",
            "--json-version",
            version,
            "config",
            "runtime.default",
            "codex",
        ];
        let regular = success(lab.run(&args));
        let mut quiet = vec!["-q"];
        quiet.extend_from_slice(&args);
        let quiet = success(lab.run(&quiet));
        assert_eq!(quiet.stdout, regular.stdout);
        let value: serde_json::Value = serde_json::from_slice(&quiet.stdout).unwrap();
        assert_eq!(value["result"]["format"], "json");
        assert_eq!(value["result"]["value"]["operation"], "set");
        assert_eq!(
            value["result"]["value"]["setting"]["key"],
            "runtime.default"
        );
        assert_eq!(value["result"]["value"]["setting"]["stored"], "codex");
        assert_eq!(value["result"]["value"]["setting"]["effective"], "codex");
    }
}

#[cfg(unix)]
#[test]
fn quiet_json_secret_registration_returns_the_saved_record_identity() {
    for version in ["1", "2"] {
        let lab = Lab::new();
        let label = format!("quiet-version-{version}");
        let output = add_secret(
            &lab,
            &[
                "-q",
                "--json",
                "--json-version",
                version,
                "secrets",
                "add",
                &label,
                "--stdin",
            ],
        );
        assert!(output.stderr.is_empty());
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["result"]["format"], "text");
        let listed = success(lab.run(&["secrets", "list", "--json"]));
        let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
        let records = &listed["result"]["value"];
        let id = records
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["name"] == label)
            .unwrap()["id"]
            .as_str()
            .unwrap();
        assert!(
            result["result"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains(id))
        );
    }
}

#[test]
fn quiet_memory_sync_commit_and_distill_preserve_their_landings() {
    for mode in ["ordinary", "flag", "", "1"] {
        let lab = Lab::new();
        let memory = lab.seed_claude_memory();
        let repo = lab.repo();
        std::fs::write(memory.join("mine.md"), "SYNTHETIC-LOCAL\n").unwrap();
        let sync = success(
            lab.quiet_command(&["memory", "sync", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        if mode == "ordinary" {
            let stdout = String::from_utf8_lossy(&sync.stdout);
            assert!(
                stdout.contains("memory:") && stdout.contains("→ `work`"),
                "{sync:?}"
            );
            assert!(stdout.contains("file →"), "{sync:?}");
            assert!(
                String::from_utf8_lossy(&sync.stderr).contains("not yet in main"),
                "{sync:?}"
            );
        } else {
            silent(sync);
        }
        assert_eq!(
            repo.git_bytes(&["show", "refs/heads/work:memory/mine.md"])
                .unwrap(),
            b"SYNTHETIC-LOCAL\n"
        );
        assert!(repo.show("refs/heads/main", "memory/mine.md").is_none());
        assert_eq!(
            std::fs::read(memory.join("agit/me/quiet/work/team.md")).unwrap(),
            b"SYNTHETIC-SHARED\n"
        );
        std::fs::write(memory.join("mine.md"), "SYNTHETIC-EDITED\n").unwrap();
        let saved = success(
            lab.quiet_command(&["commit", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        if mode == "ordinary" {
            assert!(
                String::from_utf8_lossy(&saved.stdout).contains("memory:"),
                "{saved:?}"
            );
        } else {
            silent(saved);
        }
        assert_eq!(
            repo.git_bytes(&["show", "refs/heads/work:memory/mine.md"])
                .unwrap(),
            b"SYNTHETIC-EDITED\n"
        );
        let status = success(lab.run(&["memory", "status", "--into", "me/quiet@work"]));
        let quiet_status = success(
            lab.quiet_command(&["memory", "status", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        assert!(!status.stdout.is_empty());
        let expected_header = if mode == "ordinary" {
            "target: me/quiet@work (via explicit arguments)\n"
        } else {
            "  me/quiet @ work\n"
        };
        assert_eq!(
            quiet_status
                .stdout
                .strip_prefix(expected_header.as_bytes())
                .unwrap(),
            status
                .stdout
                .strip_prefix(b"target: me/quiet@work (via explicit arguments)\n")
                .unwrap()
        );
        let diff = success(lab.run(&["memory", "diff", "--into", "me/quiet@work"]));
        assert!(String::from_utf8_lossy(&diff.stdout).contains("SYNTHETIC-EDITED"));
        assert_eq!(
            success(
                lab.quiet_command(&["memory", "diff", "--into", "me/quiet@work"], mode)
                    .output()
                    .unwrap()
            )
            .stdout,
            diff.stdout
        );
        let main = repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
        let refused = lab
            .quiet_command(&["distill", "--into", "me/quiet@work"], mode)
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("confirmation"),
            "{refused:?}"
        );
        assert!(
            String::from_utf8_lossy(&refused.stdout).contains("memory/mine.md"),
            "{refused:?}"
        );
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).unwrap(), main);
        let distilled = success(
            lab.quiet_command(&["distill", "-y", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        if mode != "ordinary" {
            silent(distilled);
        }
        assert_eq!(
            repo.git_bytes(&["show", "refs/heads/main:memory/mine.md"])
                .unwrap(),
            b"SYNTHETIC-EDITED\n"
        );
        let main = repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
        let noop = success(
            lab.quiet_command(&["distill", "-y", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        if mode == "ordinary" {
            assert!(
                String::from_utf8_lossy(&noop.stdout).contains("nothing to distill"),
                "{noop:?}"
            );
        } else {
            silent(noop);
        }
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).unwrap(), main);
    }
}

#[test]
fn quiet_memory_tracking_noop_preserves_policy_and_secret_diagnostics() {
    for mode in ["flag", "", "1"] {
        let lab = Lab::new();
        let memory = lab.seed_claude_memory();
        success(lab.run(&["memory", "sync", "--into", "me/quiet@work"]));
        let repo = lab.repo();
        success(lab.run(&["config", "memory.track", "off"]));
        std::fs::write(memory.join("mine.md"), "SYNTHETIC-NOT-TRACKED\n").unwrap();
        let before = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
        let ordinary = success(lab.run(&["memory", "sync", "--into", "me/quiet@work"]));
        assert!(
            String::from_utf8_lossy(&ordinary.stdout).contains("not collected"),
            "{ordinary:?}"
        );
        silent(
            lab.quiet_command(&["memory", "sync", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), before);
        assert!(repo.show("refs/heads/work", "memory/mine.md").is_none());
        success(lab.run(&["config", "memory.track", "session"]));
        let secret = [
            "-----BEGIN ",
            "RSA PRIVATE KEY-----\nSYNTHETIC\n-----END RSA PRIVATE KEY-----\n",
        ]
        .concat();
        std::fs::write(memory.join("mine.md"), &secret).unwrap();
        let refused = success(
            lab.quiet_command(&["memory", "sync", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
        assert!(refused.stdout.is_empty(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("suspected secret"),
            "{refused:?}"
        );
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), before);
        assert!(repo.show("refs/heads/work", "memory/mine.md").is_none());
        assert_eq!(
            std::fs::read_to_string(memory.join("mine.md")).unwrap(),
            secret
        );
    }
}

#[test]
fn quiet_memory_without_runtime_storage_is_a_successful_noop() {
    let lab = Lab::new();
    lab.seed();
    let before = lab.repo().git(&["rev-parse", "refs/heads/work"]).unwrap();
    let ordinary = success(lab.run(&["memory", "sync", "--into", "me/quiet@work"]));
    assert!(String::from_utf8_lossy(&ordinary.stdout).contains("no per-project memory"));
    for mode in ["flag", "", "1"] {
        silent(
            lab.quiet_command(&["memory", "sync", "--into", "me/quiet@work"], mode)
                .output()
                .unwrap(),
        );
    }
    assert_eq!(
        lab.repo().git(&["rev-parse", "refs/heads/work"]).unwrap(),
        before
    );
}

#[test]
fn quiet_init_preserves_binding_visibility_and_existing_file_refusals() {
    for mode in ["ordinary", "flag", "", "1"] {
        for empty in [false, true] {
            let lab = Lab::new();
            lab.signed_in();
            if empty {
                agit::domain::repo::Repo::init(&lab.store.join("repos/me/quiet")).unwrap();
            }
            let output = success(
                lab.quiet_command(&["init", "quiet", "--private"], mode)
                    .output()
                    .unwrap(),
            );
            if mode == "ordinary" {
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("bound to this directory"),
                    "{output:?}"
                );
            } else {
                silent(output);
            }
            let repo = lab.repo();
            assert_eq!(repo.visibility_preference().as_deref(), Some("private"));
            let head = repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
            assert!(
                agit::domain::meta::read_at_ref(&repo, head.trim())
                    .unwrap()
                    .is_file_line()
            );
            let bindings = std::fs::read_dir(lab.store.join("workspaces"))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(bindings.len(), 1);
            let bytes = std::fs::read(bindings[0].path()).unwrap();
            let binding: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(binding["repo"], "me/quiet");
            assert_eq!(
                binding["dir"],
                lab.root
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            );
            let refused = lab
                .quiet_command(&["init", "quiet"], mode)
                .output()
                .unwrap();
            assert!(!refused.status.success(), "{refused:?}");
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains("already exists"),
                "{refused:?}"
            );
            assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).unwrap(), head);
            assert_eq!(std::fs::read(bindings[0].path()).unwrap(), bytes);
        }
        let lab = Lab::new();
        lab.signed_in();
        let repo = agit::domain::repo::Repo::init(&lab.store.join("repos/me/quiet")).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "SYNTHETIC-EXISTING\n").unwrap();
        let refused = lab
            .quiet_command(&["init", "quiet", "--no-bind"], mode)
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("checkout is not empty"),
            "{refused:?}"
        );
        assert_eq!(
            std::fs::read(repo.root().join("AGENTS.md")).unwrap(),
            b"SYNTHETIC-EXISTING\n"
        );
        assert!(!repo.has_ref("refs/heads/main"));
    }
}

#[test]
fn quiet_seed_keeps_explicit_asset_consent_and_refused_candidates() {
    for mode in ["flag", "", "1"] {
        for consent in [false, true] {
            let lab = Lab::new();
            lab.signed_in();
            std::fs::write(
                lab.root.path().join("AGENTS.md"),
                "SYNTHETIC-PROJECT-ASSET\n",
            )
            .unwrap();
            let mut command = lab.quiet_command(&["init", "quiet", "--seed", "--no-bind"], mode);
            if consent {
                command.arg("--yes");
            }
            let output = success(command.output().unwrap());
            let adopted = lab
                .repo()
                .git_bytes(&["show", "refs/heads/main:AGENTS.md"])
                .unwrap();
            if consent {
                silent(output);
                assert_eq!(adopted, b"SYNTHETIC-PROJECT-ASSET\n");
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("found AGENTS.md"),
                    "{output:?}"
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("nothing adopted"),
                    "{output:?}"
                );
                assert_ne!(adopted, b"SYNTHETIC-PROJECT-ASSET\n");
            }
            assert_eq!(
                std::fs::read(lab.root.path().join("AGENTS.md")).unwrap(),
                b"SYNTHETIC-PROJECT-ASSET\n"
            );
        }
        let lab = Lab::new();
        lab.signed_in();
        silent(
            lab.quiet_command(&["init", "quiet", "--seed", "--no-bind"], mode)
                .output()
                .unwrap(),
        );
        assert!(lab.repo().has_ref("refs/heads/main"));
    }
}

#[test]
fn quiet_branch_governance_keeps_history_and_explicit_delete_authority() {
    for mode in ["ordinary", "flag", "", "1"] {
        let lab = Lab::new();
        lab.seed();
        let repo = lab.repo();
        let original = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
        for args in [
            vec!["fork", "me/quiet@work", "-b", "copy"],
            vec!["branch", "--repo", "me/quiet", "rename", "copy", "renamed"],
            vec!["branch", "--repo", "me/quiet", "seal", "renamed"],
        ] {
            let output = success(lab.quiet_command(&args, mode).output().unwrap());
            if mode == "ordinary" {
                assert!(!output.stdout.is_empty(), "{args:?}");
            } else {
                silent(output);
            }
        }
        assert!(!repo.has_ref("refs/heads/copy"));
        assert!(
            repo.show("refs/heads/renamed", agit::commands::branch::SEAL_FILE)
                .is_some()
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/work"]).unwrap(),
            original
        );
        let head = repo.git(&["rev-parse", "refs/heads/renamed"]).unwrap();
        let noop = success(
            lab.quiet_command(&["branch", "--repo", "me/quiet", "seal", "renamed"], mode)
                .output()
                .unwrap(),
        );
        if mode == "ordinary" {
            assert!(String::from_utf8_lossy(&noop.stdout).contains("already sealed"));
        } else {
            silent(noop);
        }
        let listed = success(
            lab.quiet_command(&["branch", "--repo", "me/quiet"], mode)
                .output()
                .unwrap(),
        );
        assert!(String::from_utf8_lossy(&listed.stdout).contains("renamed"));
        let refused = lab
            .quiet_command(&["branch", "--repo", "me/quiet", "rm", "renamed"], mode)
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("never pushed"),
            "{refused:?}"
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/renamed"]).unwrap(),
            head
        );
        let removed = success(
            lab.quiet_command(
                &["branch", "--repo", "me/quiet", "rm", "renamed", "--force"],
                mode,
            )
            .output()
            .unwrap(),
        );
        if mode != "ordinary" {
            silent(removed);
        }
        assert!(!repo.has_ref("refs/heads/renamed"));
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/work"]).unwrap(),
            original
        );
    }
}

#[test]
fn quiet_revert_retains_evidence_and_refuses_an_absent_view_selection() {
    for mode in ["ordinary", "flag", "", "1"] {
        let lab = Lab::new();
        lab.seed();
        let before = success(lab.run(&["show", "me/quiet@work", "--raw", "--log-only"]));
        let removed = success(
            lab.quiet_command(&["revert", "me/quiet@work#1"], mode)
                .output()
                .unwrap(),
        );
        if mode == "ordinary" {
            assert!(String::from_utf8_lossy(&removed.stdout).contains("evidence in the log"));
        } else {
            silent(removed);
        }
        let after = success(lab.run(&["show", "me/quiet@work", "--raw", "--log-only"]));
        assert!(after.stdout.starts_with(&before.stdout));
        let view = success(lab.run(&["show", "me/quiet@work", "--raw"]));
        assert!(!String::from_utf8_lossy(&view.stdout).contains("SYNTHETIC-QUIET-QUESTION"));
        let head = lab.repo().git(&["rev-parse", "refs/heads/work"]).unwrap();
        let refused = lab
            .quiet_command(&["revert", "me/quiet@work#1"], mode)
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{refused:?}");
        assert!(!refused.stderr.is_empty());
        assert_eq!(
            lab.repo().git(&["rev-parse", "refs/heads/work"]).unwrap(),
            head
        );
    }
}

#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn quiet_json_branch_noop_preserves_the_recorded_result() {
    let lab = Lab::new();
    lab.seed();
    success(lab.run(&["branch", "--repo", "me/quiet", "seal", "work"]));
    for version in ["1", "2"] {
        let args = [
            "--json",
            "--json-version",
            version,
            "branch",
            "--repo",
            "me/quiet",
            "seal",
            "work",
        ];
        let regular = success(lab.run(&args));
        let json: serde_json::Value = serde_json::from_slice(&regular.stdout).unwrap();
        assert!(
            json["result"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains("already sealed"))
        );
        for mode in ["flag", "", "1"] {
            assert_eq!(
                success(lab.quiet_command(&args, mode).output().unwrap()).stdout,
                regular.stdout
            );
        }
    }
}
