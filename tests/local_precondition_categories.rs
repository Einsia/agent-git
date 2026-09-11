//! Local precondition failures preserve command categories and owned state.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    hub: TcpListener,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        for name in ["work", "tmp", "bin"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let lab = Self { root, home, hub };
        let output = lab
            .command("human", &["config", "--list"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        lab
    }

    fn command(&self, mode: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env(
                "AGIT_HUB_URL",
                format!("http://{}", self.hub.local_addr().unwrap()),
            )
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.root.path().join("absent-gitconfig"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(self.root.path().join("work"))
            .stdin(Stdio::null());
        if mode == "quiet" {
            command.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            command.args(["--json", "--json-version", version]);
        }
        command.args(args);
        command
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    entry
                        .file_type()
                        .is_file()
                        .then(|| fs::read(entry.path()).unwrap()),
                )
            })
            .collect()
    }

    fn no_requests(&self) {
        assert_eq!(
            self.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        if let Ok(bytes) = fs::read(self.home.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

fn output_text(output: &Output, mode: &str, command: &str, code: i32) -> String {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{mode}/{command}: {output:?}"
    );
    if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(value["command"], command);
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], code == 0);
        assert!(value["diagnostics"]["stderr"].is_array());
        if version == "1" {
            assert!(value.get("fix").is_none());
        } else {
            assert_eq!(value["fix"], serde_json::json!([]));
        }
        value.to_string()
    } else {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

#[test]
fn caught_input_errors_render_the_original_diagnostic_once() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        for args in [
            vec!["tag", "label", "alice/qa@main#0"],
            vec!["view", "alice/qa@main#0"],
            vec!["log", "alice/qa@main#0"],
            vec!["pull", "alice/qa@main#0"],
            vec!["diff", "alice/qa@main..main#0"],
            vec!["scan", "alice/qa@main#0", "--secrets"],
        ] {
            let output = run_bounded(lab.command(mode, &args));
            let text = output_text(&output, mode, args[0], 2);
            assert_eq!(
                text.matches("turn numbers start at 1").count(),
                1,
                "{args:?}: {text}"
            );
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
    }
}

#[test]
fn propagated_local_failures_are_generic_and_argument_failures_keep_usage() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let blocker = lab.home.join("config.json");
        fs::create_dir(&blocker).unwrap();
        fs::write(blocker.join("owned-evidence"), b"keep this evidence").unwrap();
        let before = lab.state();
        let output = run_bounded(lab.command(mode, &["config", "commit.auto", "false"]));
        let text = output_text(&output, mode, "config", 1);
        assert!(!text.contains("doesn’t take"), "{text}");
        assert_eq!(lab.state(), before);
        lab.no_requests();

        let output = run_bounded(lab.command(mode, &["config", "commit.auto", "not-a-bool"]));
        let text = output_text(&output, mode, "config", 2);
        assert!(text.contains("doesn’t take"), "{text}");
        assert_eq!(lab.state(), before);
        lab.no_requests();

        fs::remove_file(blocker.join("owned-evidence")).unwrap();
        fs::remove_dir(&blocker).unwrap();
        let output = run_bounded(lab.command(mode, &["config", "commit.auto", "false"]));
        output_text(&output, mode, "config", 0);
        let settings: Value = serde_json::from_slice(&fs::read(&blocker).unwrap()).unwrap();
        assert_eq!(settings["commit.auto"], "false");
        let healthy = lab.state();
        for args in [
            vec!["import", "--from", "not-a-runtime"],
            vec![
                "import",
                "owned-session",
                "--from",
                "not-a-runtime",
                "--link-only",
            ],
            vec!["show", "owned-session", "--agent", "bad-slug"],
            vec!["clone", "bad-slug"],
            vec!["clone", "bad-slug@main"],
            vec!["clone", "alice/"],
            vec!["view", "qa@work"],
            vec!["merge", "source", "--into", "qa@work", "--dry-run"],
            vec![
                "merge",
                "source",
                "--into",
                "alice/qa/extra@work",
                "--dry-run",
            ],
            vec!["diff", "alice/qa/extra@work"],
            vec!["diff", "alice/qa@work..bob/notes/extra@topic"],
            vec!["diff", "alice/qa@work...bob/notes/extra@topic"],
            vec!["scan", "alice/qa/extra@work", "--secrets"],
            vec!["revert", "alice/qa/extra@work#1"],
            vec!["resume", "alice/qa/extra@work", "--no-launch"],
        ] {
            let output = run_bounded(lab.command(mode, &args));
            let text = output_text(&output, mode, args[0], 2);
            let expected = if args[0] == "import" {
                "unknown runtime"
            } else {
                "use the <owner>/<agent> form"
            };
            assert!(text.contains(expected), "{text}");
            assert_eq!(lab.state(), healthy);
            lab.no_requests();
        }
    }
}

#[test]
fn rc_landing_runtime_input_is_validated_before_remote_lookup() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        let args = agit::commands::rc::land_argv(
            "alice/qa",
            "aaaaaaaa-0000-4000-8000-000000000152",
            "work",
            "not-a-runtime",
            "bbbbbbbb-0000-4000-8000-000000000152",
            lab.root.path().join("work").to_str().unwrap(),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_bounded(lab.command(mode, &args));
        let text = output_text(&output, mode, "rc", 2);
        assert!(text.contains("unknown runtime"), "{text}");
        assert_eq!(lab.state(), before);
        lab.no_requests();
    }
}

#[test]
fn clone_legacy_identity_input_is_validated_before_remote_lookup() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        for supplied in ["not-an-id", " "] {
            let output = run_bounded(lab.command(
                mode,
                &["clone", "alice/qa", "--adopt-legacy-agent-id", supplied],
            ));
            let text = output_text(&output, mode, "clone", 2);
            assert!(text.contains("invalid --adopt-legacy-agent-id"), "{text}");
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
    }
}

#[test]
fn memory_targets_distinguish_explicit_input_from_execution_failures() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        for command in ["memory", "distill"] {
            for target in ["work", "qa@work", "alice/qa/extra@work"] {
                let output = run_bounded(lab.command(mode, &[command, "--into", target]));
                let text = output_text(&output, mode, command, 2);
                let expected = if target == "work" {
                    "needs the full"
                } else {
                    "use the <owner>/<agent> form"
                };
                assert!(text.contains(expected), "{text}");
                assert_eq!(lab.state(), before);
                lab.no_requests();
            }
        }
    }
}

#[test]
fn lineage_arguments_keep_usage_and_git_failures_keep_their_unclassified_cause() {
    const ID: &str = "ed152010-1111-4444-8888-123456789abc";
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let native = lab
            .root
            .path()
            .join(".claude/projects/owned")
            .join(format!("{ID}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        fs::write(&native, b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"owned lineage native canary\"}}\n").unwrap();
        let before = lab.state();
        for (target, runtime, id, expected) in [
            ("alice/qa", "claude-code", ID, "explicit destination branch"),
            (
                "alice/qa@main",
                "claude-code",
                ID,
                "explicit session branch",
            ),
            ("alice/qa@work#1", "claude-code", ID, "historic selector"),
            ("Alice/qa@work", "claude-code", ID, "lowercase"),
            (
                "alice/qa@bad.lock",
                "claude-code",
                ID,
                "not a valid Git ref",
            ),
            ("alice/qa@work", "not-a-runtime", ID, "unknown runtime"),
            ("alice/qa@work", "claude-code", "@", "native session ID"),
            ("alice/qa@work", "claude-code", "../bad", "not found"),
        ] {
            for preview in [false, true] {
                let mut command =
                    lab.command(mode, &["import", id, "--from", runtime, "--into", target]);
                if preview {
                    command.arg("--propose-lineage");
                }
                let output = run_bounded(command);
                let text = output_text(&output, mode, "import", 2);
                assert!(
                    text.contains(expected),
                    "{target}/{runtime}/{id}/{preview}: {text}"
                );
                assert!(!text.contains("owned lineage native canary"), "{text}");
                assert_eq!(lab.state(), before);
                lab.no_requests();
            }
        }
        for args in [
            vec!["import", ID, "--from", "claude-code", "--propose-lineage"],
            vec!["import", ID, "--into", "alice/qa@work", "--propose-lineage"],
            vec![
                "import",
                "--from",
                "claude-code",
                "--into",
                "alice/qa@work",
                "--propose-lineage",
            ],
        ] {
            let output = run_bounded(lab.command(mode, &args));
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
        let output = run_bounded(lab.command(
            mode,
            &[
                "import",
                ID,
                "--from",
                "claude-code",
                "--into",
                "alice/qa@work",
                "--propose-lineage",
            ],
        ));
        output_text(&output, mode, "import", 0);
        assert_eq!(lab.state(), before);
        lab.no_requests();

        let rejected_git =
            lab.root
                .path()
                .join("bin")
                .join(if cfg!(windows) { "git.exe" } else { "git" });
        fs::write(&rejected_git, b"owned invalid executable format\0").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&rejected_git, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let blocked = lab.state();
        for preview in [false, true] {
            let mut command = lab.command(
                mode,
                &[
                    "import",
                    ID,
                    "--from",
                    "claude-code",
                    "--into",
                    "alice/qa@work",
                ],
            );
            command.env("PATH", lab.root.path().join("bin"));
            if preview {
                command.arg("--propose-lineage");
            }
            let output = run_bounded(command);
            let text = output_text(&output, mode, "import", 1);
            assert!(text.to_ascii_lowercase().contains("git"), "{text}");
            assert!(!text.contains("owned lineage native canary"), "{text}");
            assert_eq!(lab.state(), blocked);
            lab.no_requests();
        }
    }
}

#[test]
fn supervised_commit_refuses_before_identity_and_hook_settlement_stays_a_noop() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        for from_hook in [false, true] {
            let mut command = lab.command(mode, &["commit"]);
            command.env(agit::rc::harness::SUPERVISED_HOOK_ENV, "1");
            if from_hook {
                command.arg("--from-hook");
            }
            let output = command.output().unwrap();
            let text = output_text(&output, mode, "commit", if from_hook { 0 } else { 4 });
            if from_hook {
                if !mode.starts_with("json") {
                    assert!(
                        output.stdout.is_empty() && output.stderr.is_empty(),
                        "{output:?}"
                    );
                }
                assert!(!text.contains("owned by the RC supervisor"));
            } else {
                assert!(text.contains("owned by the RC supervisor"), "{text}");
                assert!(text.contains("live identity lease"), "{text}");
                assert!(!text.contains("not logged in"), "{text}");
            }
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
    }
}

#[test]
fn doctor_reports_missing_git_as_a_local_precondition_without_changing_health_policy() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let warmup = lab.command(mode, &["doctor"]).output().unwrap();
        let text = output_text(&warmup, mode, "doctor", 0);
        assert!(text.contains("git version"), "{text}");
        lab.no_requests();
        let before = lab.state();
        let output = lab
            .command(mode, &["doctor"])
            .env("PATH", lab.root.path().join("bin"))
            .output()
            .unwrap();
        let text = output_text(&output, mode, "doctor", 4);
        assert!(text.contains("unavailable — agit depends on git"), "{text}");
        assert_eq!(lab.state(), before);
        lab.no_requests();
        let output = lab.command(mode, &["doctor"]).output().unwrap();
        let text = output_text(&output, mode, "doctor", 0);
        assert!(text.contains("git version"), "{text}");
        assert_eq!(lab.state(), before);
        lab.no_requests();
    }
}

#[test]
fn setup_storage_failures_preserve_completion_output_and_allow_an_explicit_retry() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let path = lab.root.path().join("work/AGENTS.md");
        fs::create_dir(&path).unwrap();
        let before = lab.state();
        let args = ["setup", "--agents-md", "--completions", "bash"];
        let output = lab.command(mode, &args).output().unwrap();
        let text = output_text(&output, mode, "setup", 4);
        assert!(text.contains("Setup incomplete"), "{text}");
        assert!(!text.contains("All set"), "{text}");
        assert!(text.contains("complete -F"), "{text}");
        assert_eq!(lab.state(), before);
        lab.no_requests();

        fs::remove_dir(&path).unwrap();
        let before = lab.state();
        let output = lab.command(mode, &args).output().unwrap();
        let text = output_text(&output, mode, "setup", 0);
        assert!(text.contains("complete -F"), "{text}");
        assert!(!text.contains("Setup incomplete"), "{text}");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("<!-- agit:begin -->"));
        assert!(contents.contains("<!-- agit:end -->"));
        let installed = lab.state();
        let mut otherwise = installed.clone();
        assert!(
            otherwise
                .remove(&path.strip_prefix(lab.root.path()).unwrap().to_owned())
                .is_some()
        );
        assert_eq!(otherwise, before);
        lab.no_requests();

        let output = lab.command(mode, &args).output().unwrap();
        output_text(&output, mode, "setup", 0);
        assert_eq!(lab.state(), installed);
        lab.no_requests();
    }
}

#[test]
fn valid_grant_storage_failure_is_not_misreported_as_an_invalid_command() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        assert!(
            lab.command("human", &["rc", "grants"])
                .output()
                .unwrap()
                .status
                .success()
        );
        let ledger = lab.home.join("rc/grants.json");
        fs::create_dir(&ledger).unwrap();
        let before = lab.state();
        for (command, code, diagnostic) in [
            ("cargo", 4, "cannot read the local command grants"),
            ("cargo test", 2, "not a bare command name"),
        ] {
            let output = lab
                .command(mode, &["rc", "grant", "synthetic-workspace", command])
                .output()
                .unwrap();
            let text = output_text(&output, mode, "rc", code);
            assert!(text.contains(diagnostic), "{text}");
            if code == 4 {
                assert!(!text.contains("grant a bare command name"), "{text}");
            }
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
        fs::remove_dir(&ledger).unwrap();
        let before = lab.state();
        let output = lab
            .command(mode, &["rc", "grant", "synthetic-workspace", "cargo"])
            .output()
            .unwrap();
        output_text(&output, mode, "rc", 0);
        let grants: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(
            grants,
            serde_json::json!({"heads":{"synthetic-workspace":["cargo"]}})
        );
        let mut after = lab.state();
        assert!(
            after
                .remove(&ledger.strip_prefix(lab.root.path()).unwrap().to_owned())
                .is_some()
        );
        assert_eq!(after, before);
        lab.no_requests();
    }
}

#[test]
fn grant_mutations_preserve_unusable_data_and_unrelated_authorizations() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let output = lab.command(mode, &["rc", "grants"]).output().unwrap();
        output_text(&output, mode, "rc", 0);
        let ledger = lab.home.join("rc/grants.json");
        for bytes in [
            b"{\"heads\":{\"existing-workspace\":[\"git\"]},".as_slice(),
            b"null".as_slice(),
            b"{\"heads\":{\"existing-workspace\":false}}".as_slice(),
            &[0xff, 0xfe],
        ] {
            fs::write(&ledger, bytes).unwrap();
            let before = lab.state();
            for action in ["grant", "ungrant"] {
                let output = lab
                    .command(mode, &["rc", action, "synthetic-workspace", "cargo"])
                    .output()
                    .unwrap();
                let text = output_text(&output, mode, "rc", 4);
                assert!(
                    text.contains("cannot read the local command grants"),
                    "{text}"
                );
                assert_eq!(lab.state(), before);
                lab.no_requests();
            }
            let output = lab.command(mode, &["rc", "grants"]).output().unwrap();
            output_text(&output, mode, "rc", 0);
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
        let original = serde_json::json!({"heads": {
            "existing-workspace": ["git"],
            "synthetic-workspace": ["make"]
        }});
        fs::write(&ledger, serde_json::to_vec(&original).unwrap()).unwrap();
        let before = lab.state();
        for action in ["grant", "ungrant"] {
            let output = lab
                .command(mode, &["rc", action, "synthetic-workspace", "cargo"])
                .output()
                .unwrap();
            output_text(&output, mode, "rc", 0);
            let actual: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
            let expected = if action == "grant" {
                serde_json::json!({"heads": {
                    "existing-workspace": ["git"],
                    "synthetic-workspace": ["cargo", "make"]
                }})
            } else {
                original.clone()
            };
            assert_eq!(actual, expected);
            let mut after = lab.state();
            after.insert(
                ledger.strip_prefix(lab.root.path()).unwrap().to_owned(),
                before[&ledger.strip_prefix(lab.root.path()).unwrap().to_owned()].clone(),
            );
            assert_eq!(after, before);
            lab.no_requests();
        }
    }
}

fn seed_login(lab: &Lab) {
    let hub = format!("http://{}", lab.hub.local_addr().unwrap());
    let credential = agit::infra::credentials::HubCredential {
        username: "alice".into(),
        email: None,
        hub: Some(hub.clone()),
        access_token: "synthetic-claim-token".into(),
        access_expires_at: "2099-01-01T00:00:00Z".into(),
        refresh_token: "synthetic-claim-refresh".into(),
        refresh_expires_at: "2000-01-01T00:00:00Z".into(),
    };
    agit::infra::credentials::save_at(
        &lab.home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&hub).unwrap()
        )),
        &credential,
    )
    .unwrap();
}

#[test]
fn commit_store_blockers_keep_auth_and_ordinary_hook_boundaries() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let store = lab.home.join("store");
        fs::write(&store, b"retained store blocker").unwrap();
        let before = lab.state();
        let output = run_bounded(lab.command(mode, &["commit"]));
        assert_eq!(output.status.code(), Some(5), "{mode}: {output:?}");
        assert_eq!(lab.state(), before);
        lab.no_requests();

        seed_login(&lab);
        let before = lab.state();
        for from_hook in [false, true] {
            let mut command = lab.command(mode, &["commit"]);
            if from_hook {
                command.arg("--from-hook");
            }
            let output = run_bounded(command);
            let text = output_text(&output, mode, "commit", if from_hook { 0 } else { 4 });
            if from_hook {
                assert!(!text.contains("cannot open the local session store"));
                if !mode.starts_with("json") {
                    assert!(output.stdout.is_empty() && output.stderr.is_empty());
                }
            } else {
                assert!(
                    text.contains("cannot open the local session store"),
                    "{text}"
                );
                assert!(text.contains("cannot create store directory"), "{text}");
            }
            for token in ["synthetic-claim-token", "synthetic-claim-refresh"] {
                assert!(!text.contains(token), "{text}");
            }
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }

        let output = run_bounded(lab.command(mode, &["commit", "--not-a-commit-option"]));
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(lab.state(), before);
        lab.no_requests();

        fs::remove_file(&store).unwrap();
        fs::create_dir(&store).unwrap();
        let before = lab.state();
        for target in ["qa@work", "alice/qa/extra@work"] {
            let output = run_bounded(lab.command(mode, &["commit", target]));
            let text = output_text(&output, mode, "commit", 2);
            assert!(text.contains("use the <owner>/<agent> form"), "{text}");
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }

        let output = run_bounded(lab.command(mode, &["commit", "alice/missing@work"]));
        let text = output_text(&output, mode, "commit", 3);
        assert!(
            text.contains("alice/missing doesn’t exist locally"),
            "{text}"
        );
        assert!(!text.contains("cannot open the local session store"));
        assert_eq!(lab.state(), before);
        lab.no_requests();
    }
}

#[test]
fn show_selected_native_payload_failures_preserve_evidence_and_allow_retry() {
    use agit::domain::{link, store::Store};
    const ID: &str = "ed152006-1111-4444-8888-123456789abc";
    const PROMPT: &str = "owned native payload canary";
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let codex = lab.root.path().join(".codex");
        let native = codex.join(format!(
            "sessions/2026/09/11/rollout-2026-09-11T00-00-00-{ID}.jsonl"
        ));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        let valid = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": ID,
                    "cwd": lab.root.path().join("work"),
                    "timestamp": "2026-09-11T00:00:00Z"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": PROMPT}]
                }
            })
        );
        let mut invalid = valid.as_bytes().to_vec();
        invalid.extend_from_slice(b"\xff\n");
        // An adopted file is sufficient for inspection; fixture setup never launches a runtime.
        fs::write(&native, &invalid).unwrap();
        link::write(
            &Store::at(lab.home.join("store")),
            &link::Link::new("codex", ID, Some(&lab.root.path().join("work"))),
        )
        .unwrap();
        let command = |args: &[&str]| {
            let mut command = lab.command(mode, args);
            command.env("CODEX_HOME", &codex);
            command
        };
        let before = lab.state();
        for raw in [false, true] {
            let mut child = command(&["show", ID]);
            if raw {
                child.arg("--raw");
            }
            let output = run_bounded(child);
            let text = output_text(&output, mode, "show", 4);
            assert!(
                text.contains("cannot read selected session content"),
                "{text}"
            );
            assert!(!text.contains(PROMPT), "{text}");
            if mode.starts_with("json") {
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["result"]["format"], "empty");
                assert!(
                    value["diagnostics"]["stderr"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["level"] == "error"
                            && entry["message"]
                                .as_str()
                                .unwrap()
                                .contains("cannot read selected session content"))
                );
            } else {
                assert!(output.stdout.is_empty(), "{output:?}");
            }
            assert_eq!(fs::read(&native).unwrap(), invalid);
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
        let text = output_text(
            &run_bounded(command(&["show", ID, "--agent", "bad-slug"])),
            mode,
            "show",
            2,
        );
        assert!(text.contains("use the <owner>/<agent> form"), "{text}");
        assert!(!text.contains("cannot read selected session content"));
        assert_eq!(lab.state(), before);
        lab.no_requests();

        fs::write(&native, &valid).unwrap();
        let before = lab.state();
        for raw in [false, true] {
            let mut child = command(&["show", ID]);
            if raw {
                child.arg("--raw");
            }
            let output = run_bounded(child);
            let text = output_text(&output, mode, "show", 0);
            assert!(text.contains(PROMPT), "{text}");
            assert!(text.contains(ID), "{text}");
            if raw && !mode.starts_with("json") {
                assert_eq!(output.stdout, valid.as_bytes());
                assert!(output.stderr.is_empty(), "{output:?}");
            }
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
    }
}

#[test]
fn superseded_implicit_claims_are_policy_refusals_without_reading_native_state() {
    use agit::domain::{link, store::Store};
    for mode in ["human", "quiet", "json1", "json2"] {
        for ownerless in [false, true] {
            let lab = Lab::new();
            let store = Store::at(lab.home.join("store"));
            let mut stale = link::Link::new("codex", "superseded-codex", None);
            stale.agent = Some("claim".into());
            stale.owner = (!ownerless).then(|| "alice".into());
            stale.branch = Some("work".into());
            stale.superseded_by = Some("codex/active-codex".into());
            link::write(&store, &stale).unwrap();
            fs::write(
                lab.root.path().join("work/native-canary"),
                b"private native content",
            )
            .unwrap();
            let before = lab.state();
            let unauthenticated = lab
                .command(mode, &["commit"])
                .env("AGIT_SESSION", "alice/claim@work")
                .env("CODEX_SESSION_ID", &stale.session_id)
                .output()
                .unwrap();
            assert_eq!(
                unauthenticated.status.code(),
                Some(5),
                "{unauthenticated:?}"
            );
            assert_eq!(lab.state(), before);
            lab.no_requests();
            seed_login(&lab);
            for multiple in [false, true] {
                if multiple {
                    let mut outer = stale.clone();
                    outer.source = "claude-code".into();
                    outer.session_id = "superseded-claude".into();
                    link::write(&store, &outer).unwrap();
                }
                let before = lab.state();
                for target in [None, Some("@")] {
                    for from_hook in [false, true] {
                        let mut command = lab.command(mode, &["commit"]);
                        if let Some(target) = target {
                            command.arg(target);
                        }
                        if from_hook {
                            command.arg("--from-hook");
                        }
                        command
                            .env("AGIT_SESSION", "alice/claim@work")
                            .env("CODEX_SESSION_ID", &stale.session_id);
                        if multiple {
                            command.env("CLAUDE_CODE_SESSION_ID", "superseded-claude");
                        }
                        let output = command.output().unwrap();
                        let text =
                            output_text(&output, mode, "commit", if from_hook { 0 } else { 7 });
                        if from_hook {
                            assert!(!text.contains("superseded"), "{text}");
                            if !mode.starts_with("json") {
                                assert!(output.stdout.is_empty() && output.stderr.is_empty());
                            }
                        } else {
                            assert!(text.contains("superseded"), "{text}");
                            assert!(!text.contains("private native content"), "{text}");
                        }
                        assert_eq!(lab.state(), before);
                        lab.no_requests();
                    }
                }
            }
        }
    }
}

fn run_bounded(mut command: Command) -> Output {
    use std::io::{Read, Seek};
    use std::time::{Duration, Instant};
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = command
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let read = |file: &mut std::fs::File| {
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.take(1024 * 1024).read_to_end(&mut bytes).unwrap();
        bytes
    };
    let output = Output {
        status,
        stdout: read(&mut stdout),
        stderr: read(&mut stderr),
    };
    assert!(!timed_out, "command exceeded its deadline: {output:?}");
    output
}

fn rc_fixture_command(lab: &Lab, action: &str) -> Command {
    // The child owns its environment; concurrent fixtures must not reroute the test process.
    let template = lab.command("human", &[]);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .envs(
            template
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(template.get_current_dir().unwrap())
        .args(["--exact", "rc_category_fixture_child", "--nocapture"])
        .env("AGIT_RC_CATEGORY_FIXTURE", action)
        .stdin(Stdio::null());
    command
}

#[test]
fn rc_category_fixture_child() {
    let Ok(action) = std::env::var("AGIT_RC_CATEGORY_FIXTURE") else {
        return;
    };
    if action.starts_with("serve-") {
        serve_rc_status(&action);
        return;
    }
    assert_eq!(action, "seed");
    let hub = std::env::var("AGIT_HUB_URL").unwrap();
    agit::rc::identity::save_connection(&agit::rc::identity::Connection {
        connection_id: "synthetic-connection".into(),
        token: "agit_rc_synthetic-category-token".into(),
        hub,
        created_at: "2026-01-01T00:00:00Z".into(),
    })
    .unwrap();
    agit::rc::identity::identity().unwrap();
}

#[test]
fn foreground_rc_preserves_unusable_local_roster_evidence_and_refuses_before_network() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for directory in [false, true] {
            let lab = Lab::new();
            let seed = run_bounded(rc_fixture_command(&lab, "seed"));
            assert!(seed.status.success(), "{seed:?}");
            let fallback = lab.home.join("rc/sessions.fail-closed.json");
            if directory {
                fs::create_dir(&fallback).unwrap();
            } else {
                fs::write(&fallback, b"{ incomplete roster").unwrap();
            }
            let before = lab.state();
            let mut command = lab.command(mode, &["rc", "start"]);
            // Foreground startup may probe installed runtimes, which this local-state fixture excludes.
            command.env("PATH", lab.root.path().join("bin"));
            let output = run_bounded(command);
            let json = mode.starts_with("json");
            let text = output_text(&output, mode, "rc", if json { 8 } else { 4 });
            if json {
                assert!(
                    text.contains("cannot wrap the foreground RC daemon"),
                    "{text}"
                );
                assert!(!text.contains("fail-closed roster snapshot"), "{text}");
            } else {
                assert!(text.contains("fail-closed roster snapshot"), "{text}");
            }
            assert!(!text.contains("not signed in"), "{text}");
            assert!(!lab.home.join("rc/agitd.pid").exists());
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
    }
}

fn serve_rc_status(action: &str) {
    use agit::rc::control::{self, Reply, Request, Status};
    assert!(matches!(action, "serve-busy" | "serve-offline"));
    let listener = control::listen().unwrap();
    control::write_pidfile().unwrap();
    fs::write(
        std::env::var_os("AGIT_RC_CATEGORY_READY").unwrap(),
        b"ready",
    )
    .unwrap();
    for stream in listener.incoming() {
        let mut stop = false;
        control::serve_one(&mut stream.unwrap(), |request| match request {
            Request::Status if action == "serve-busy" => Reply::Error {
                message: "the daemon is busy and did not answer within 2s; retry, or `agit rc stop` if it stays wedged".into(),
            },
            Request::Status => Reply::Status(Status {
                pid: std::process::id(),
                hub: std::env::var("AGIT_HUB_URL").unwrap(),
                online: false,
                ..Default::default()
            }),
            Request::Stop => { stop = true; Reply::Stopping },
            Request::ReloadSecrets => panic!("a status fixture must not reload secrets"),
        }).unwrap();
        if stop {
            break;
        }
    }
    control::clear_pidfile();
}

struct RcFixture(std::process::Child);

impl RcFixture {
    fn start(lab: &Lab, action: &str) -> Self {
        use std::time::{Duration, Instant};
        let ready = lab.root.path().join("rc-ready");
        let mut fixture = Self(
            rc_fixture_command(lab, action)
                .env("AGIT_RC_CATEGORY_READY", &ready)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() {
            assert!(
                fixture.0.try_wait().unwrap().is_none(),
                "control fixture exited before readiness"
            );
            assert!(
                Instant::now() < deadline,
                "control fixture never became ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }

    fn assert_stopped(&mut self) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "control fixture failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "control fixture ignored stop");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for RcFixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn rc_busy_status_is_precondition_but_offline_and_idempotent_stop_remain_successful() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for action in ["serve-busy", "serve-offline"] {
            let lab = Lab::new();
            let mut fixture = RcFixture::start(&lab, action);
            let before = lab.state();
            let output = run_bounded(lab.command(mode, &["rc", "status"]));
            let text = output_text(
                &output,
                mode,
                "rc",
                if action == "serve-busy" { 4 } else { 0 },
            );
            assert!(
                text.contains(if action == "serve-busy" {
                    "daemon is busy"
                } else {
                    "offline (retrying)"
                }),
                "{text}"
            );
            assert_eq!(lab.state(), before);
            lab.no_requests();
            output_text(
                &run_bounded(lab.command(mode, &["rc", "stop"])),
                mode,
                "rc",
                0,
            );
            fixture.assert_stopped();
            let before = lab.state();
            let output = run_bounded(lab.command(mode, &["rc", "status"]));
            let text = output_text(&output, mode, "rc", 4);
            assert!(text.contains("no daemon is running"), "{text}");
            output_text(
                &run_bounded(lab.command(mode, &["rc", "stop"])),
                mode,
                "rc",
                0,
            );
            assert_eq!(lab.state(), before);
            lab.no_requests();
        }
    }
}

fn secret_add_command(lab: &Lab, mode: &str, block: bool, name: &str, secret: &str) -> Command {
    use std::io::{Seek as _, Write as _};
    let mut command = lab.command(mode, &["secrets"]);
    if block {
        command.arg("block");
    }
    command.args(["add", name, "--stdin"]);
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(secret.as_bytes()).unwrap();
    input.rewind().unwrap();
    command.stdin(input);
    command
}

struct OwnedSecretVault {
    path: PathBuf,
    restore: Option<Vec<u8>>,
}

impl OwnedSecretVault {
    fn corrupt(&mut self) {
        self.restore = Some(fs::read(&self.path).unwrap());
        fs::write(&self.path, b"{").unwrap();
    }

    fn restore(&mut self) {
        if let Some(bytes) = self.restore.take() {
            fs::write(&self.path, bytes).unwrap();
        }
    }
}

impl Drop for OwnedSecretVault {
    fn drop(&mut self) {
        if let Some(bytes) = self.restore.take() {
            let _ = fs::write(&self.path, bytes);
        }
        #[cfg(windows)]
        {
            use agit::domain::secret_filter::KeyStore;
            if let Ok(bytes) = fs::read(&self.path)
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && let Some(id) = value["vault_id"].as_str()
            {
                let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
            }
        }
    }
}

#[test]
fn secret_registration_input_is_usage_without_reclassifying_stored_vault_failures() {
    const SECRET: &str = "owned-secret-registration-canary";
    const SHORT: &str = "sH!rt?";
    for mode in ["human", "quiet", "json1", "json2"] {
        for block in [false, true] {
            let lab = Lab::new();
            let long_name = "n".repeat(129);
            let long_secret = "s".repeat(513);
            let before = lab.state();
            for (name, secret, expected) in [
                ("", SECRET, "name cannot be empty"),
                (long_name.as_str(), SECRET, "name is longer than"),
                ("label", "x~!", "must be at least"),
                ("label", SHORT, "require --allow-short"),
                ("label", long_secret.as_str(), "cannot exceed"),
            ] {
                let output = run_bounded(secret_add_command(&lab, mode, block, name, secret));
                let text = output_text(&output, mode, "secrets", 2);
                assert_eq!(text.matches(expected).count(), 1, "{text}");
                assert!(
                    !text.contains(secret),
                    "secret value reached terminal output"
                );
                assert_eq!(lab.state(), before);
                lab.no_requests();
            }
            let args = if block {
                vec!["secrets", "block", "add", "label"]
            } else {
                vec!["secrets", "add", "label"]
            };
            let output = run_bounded(lab.command(mode, &args));
            let text = output_text(&output, mode, "secrets", 8);
            assert!(text.contains("automation must use `--stdin`"), "{text}");
            assert_eq!(lab.state(), before);
            lab.no_requests();

            let vault_path = if block {
                let template = lab.command("human", &[]);
                let mut git = Command::new("git");
                git.env_clear()
                    .envs(
                        template
                            .get_envs()
                            .filter_map(|(key, value)| value.map(|value| (key, value))),
                    )
                    .current_dir(template.get_current_dir().unwrap())
                    .args(["-c", "init.templateDir=", "init", "--initial-branch=main"])
                    .stdin(Stdio::null());
                let output = run_bounded(git);
                assert!(output.status.success(), "{output:?}");
                lab.root
                    .path()
                    .join("work/.git/agit/secret-dictionary/vault.json")
            } else {
                lab.home.join("secret-filter/vault.json")
            };
            // Cleanup follows the owned vault identity even when a corruption assertion unwinds.
            let mut vault = OwnedSecretVault {
                path: vault_path,
                restore: None,
            };
            let output = run_bounded(secret_add_command(&lab, mode, block, "seed", SECRET));
            let text = output_text(&output, mode, "secrets", 0);
            assert!(
                !text.contains(SECRET),
                "secret value reached terminal output"
            );
            assert!(vault.path.is_file());
            lab.no_requests();

            vault.corrupt();
            let corrupt = lab.state();
            let output = run_bounded(secret_add_command(
                &lab,
                mode,
                block,
                "retry",
                "different-owned-registration-canary",
            ));
            let text = output_text(&output, mode, "secrets", 1);
            assert!(!text.contains("different-owned-registration-canary"));
            assert_eq!(lab.state(), corrupt);
            lab.no_requests();

            vault.restore();
            let mut command = secret_add_command(&lab, mode, block, "short-rule", SHORT);
            command.arg("--allow-short");
            let output = run_bounded(command);
            let text = output_text(&output, mode, "secrets", 0);
            assert!(
                !text.contains(SHORT),
                "secret value reached terminal output"
            );
            let value: Value = serde_json::from_slice(&fs::read(&vault.path).unwrap()).unwrap();
            assert_eq!(value["records"].as_array().unwrap().len(), 2);
            lab.no_requests();
        }
    }
}
