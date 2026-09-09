//! Target notices describe verified CLI selections without becoming part of exported data.

use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(any(unix, all(windows, target_env = "msvc")))]
use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const REPO: &str = "alice/context";
const SELECTED: &str = "alice/context@selected";
const NEWEST: &str = "alice/context@newest";
const RUNTIME_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

struct Lab {
    temporary: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    repo: Repo,
    first_sha: String,
    selected_sha: String,
    first_raw: String,
    second_raw: String,
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let store = temporary.path().join("agit");
        let work = temporary.path().join("work");
        for path in [&home, &work, &temporary.path().join("empty-git-template")] {
            fs::create_dir_all(path).unwrap();
        }
        let mut lab = Self {
            repo: Repo::at(store.join("repos").join(REPO)),
            temporary,
            home,
            store,
            work: work.canonicalize().unwrap(),
            first_sha: String::new(),
            selected_sha: String::new(),
            first_raw: native_turn("SELECTED-FIRST"),
            second_raw: native_turn("SELECTED-SECOND"),
        };
        lab.initialize(&lab.repo);
        lab.git(&lab.repo, &["switch", "-q", "-c", "selected", "main"]);
        lab.first_sha = lab.settle(
            &lab.repo,
            1,
            &lab.first_raw,
            &lab.first_raw,
            "SELECTED-FIRST",
            "2020-01-01T00:00:00Z",
        );
        lab.selected_sha = lab.settle(
            &lab.repo,
            2,
            &format!("{}{}", lab.first_raw, lab.second_raw),
            &lab.second_raw,
            "SELECTED-SECOND",
            "2021-01-01T00:00:00Z",
        );
        lab.git(&lab.repo, &["switch", "-q", "-c", "newest", "main"]);
        let newest = native_turn("UNSELECTED-NEWEST");
        lab.settle(
            &lab.repo,
            1,
            &newest,
            &newest,
            "UNSELECTED-NEWEST",
            "2030-01-01T00:00:00Z",
        );
        lab.pin_and_adopt_newest();
        lab
    }

    fn isolated(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env(
                "GIT_TEMPLATE_DIR",
                self.temporary.path().join("empty-git-template"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Context echo fixture")
            .env("GIT_AUTHOR_EMAIL", "context-echo@example.test")
            .env("GIT_COMMITTER_NAME", "Context echo fixture")
            .env("GIT_COMMITTER_EMAIL", "context-echo@example.test")
            .current_dir(&self.work);
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

    fn command(&self, session: Option<&str>, args: &[&str]) -> Command {
        let mut command = self.isolated(env!("CARGO_BIN_EXE_agit"));
        command.args(args);
        if let Some(session) = session {
            command.env("AGIT_SESSION", session);
        }
        command
    }

    fn run(&self, session: Option<&str>, args: &[&str]) -> Output {
        self.command(session, args).output().unwrap()
    }

    fn git(&self, repo: &Repo, args: &[&str]) -> String {
        success(
            self.isolated("git")
                .args(["-c", "commit.gpgsign=false", "-C"])
                .arg(repo.root())
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn initialize(&self, repo: &Repo) {
        fs::create_dir_all(repo.root()).unwrap();
        self.git(repo, &["init", "--initial-branch=main"]);
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        fs::write(repo.root().join("AGENTS.md"), "shared base\n").unwrap();
        self.git(repo, &["add", "--all"]);
        self.git(repo, &["commit", "-qm", "initialize shared files"]);
    }

    fn settle(
        &self,
        repo: &Repo,
        turn: u32,
        raw_log: &str,
        raw_view: &str,
        marker: &str,
        date: &str,
    ) -> String {
        let claim = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(raw_log, "claude-code", &claim);
        let view = transcript::wrap_lines(raw_view, "claude-code", &claim);
        storage::write_snapshot(repo.root(), &log, &view).unwrap();
        let mut snapshot = meta::Meta::new(
            claim,
            "claude-code".to_owned(),
            self.work.to_string_lossy().into_owned(),
        );
        snapshot.turn = Some(turn);
        meta::write(repo.root(), &snapshot).unwrap();
        fs::write(repo.root().join("AGENTS.md"), format!("{marker}\n")).unwrap();
        fs::write(
            repo.root().join("payload.txt"),
            format!("{marker}\n\tspaces  \n\n"),
        )
        .unwrap();
        self.git(repo, &["add", "--all"]);
        success(
            self.isolated("git")
                .args(["-c", "commit.gpgsign=false", "-C"])
                .arg(repo.root())
                .args(["commit", "-qm", marker])
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .output()
                .unwrap(),
        );
        self.git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn other_repo(&self) -> (Repo, String) {
        let repo = Repo::at(self.store.join("repos/bob/other"));
        self.initialize(&repo);
        self.git(&repo, &["switch", "-q", "-c", "topic", "main"]);
        let raw = native_turn("OTHER-REPOSITORY");
        let sha = self.settle(
            &repo,
            1,
            &raw,
            &raw,
            "OTHER-REPOSITORY",
            "2022-01-01T00:00:00Z",
        );
        (repo, sha)
    }

    fn pin_and_adopt_newest(&self) {
        let digest = hex::encode(Sha256::digest(self.work.to_string_lossy().as_bytes()));
        let workspaces = self.store.join("workspaces");
        fs::create_dir_all(&workspaces).unwrap();
        fs::write(
            workspaces.join(format!("{}.json", &digest[..16])),
            serde_json::to_vec(&serde_json::json!({
                "dir": self.work,
                "repo": REPO,
                "pinned": "newest",
            }))
            .unwrap(),
        )
        .unwrap();
        let links = self.store.join("store/codex");
        fs::create_dir_all(&links).unwrap();
        fs::write(
            links.join(format!("{RUNTIME_ID}.json")),
            serde_json::to_vec(&serde_json::json!({
                "cwd": self.work,
                "owner": "alice",
                "agent": "context",
                "branch": "newest",
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = fs::read(self.store.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

fn native_turn(marker: &str) -> String {
    [
        serde_json::json!({"type":"user","message":{"role":"user","content":marker}}),
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":format!("REPLY-{marker}")}}),
    ]
    .map(|value| format!("{value}\n"))
    .concat()
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn notice(output: Output, expected: &str) -> String {
    let text = success(output);
    assert_eq!(text.lines().next(), Some(expected), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("target: "))
            .count(),
        1,
        "{text}"
    );
    text
}

fn failed_without_notice(output: Output) {
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!output.stderr.is_empty(), "{output:?}");
}

#[test]
fn qualified_reads_echo_the_explicit_branch_despite_conflicting_environment() {
    let lab = Lab::new();
    let _other = lab.other_repo();
    for session in [None, Some("malformed"), Some("bob/other@topic")] {
        for command in ["log", "show", "view"] {
            let output = lab
                .command(session, &[command, SELECTED])
                .env("CODEX_SESSION_ID", RUNTIME_ID)
                .output()
                .unwrap();
            let text = notice(
                output,
                "target: alice/context@selected (via explicit arguments)",
            );
            assert!(text.contains("SELECTED-SECOND"), "{command}: {text}");
            assert!(!text.contains("UNSELECTED-NEWEST"), "{command}: {text}");
            assert!(!text.contains("OTHER-REPOSITORY"), "{command}: {text}");
        }
    }
}

#[test]
fn omitted_and_at_reads_echo_environment_over_workspace_pin_and_recency() {
    let lab = Lab::new();
    for command in ["log", "show", "view"] {
        for target in [None, Some("@")] {
            let mut args = vec![command];
            args.extend(target);
            let text = notice(
                lab.run(Some(SELECTED), &args),
                "target: alice/context@selected (via AGIT_SESSION)",
            );
            assert!(text.contains("SELECTED-SECOND"), "{args:?}: {text}");
            assert!(!text.contains("UNSELECTED-NEWEST"), "{args:?}: {text}");
        }
    }
}

#[test]
fn unqualified_branch_reads_disclose_the_environment_repository_dependency() {
    let lab = Lab::new();
    for command in ["log", "show", "view"] {
        let text = notice(
            lab.run(Some(NEWEST), &[command, "selected"]),
            "target: alice/context@selected (via explicit arguments + AGIT_SESSION)",
        );
        assert!(text.contains("SELECTED-SECOND"), "{command}: {text}");
        assert!(!text.contains("UNSELECTED-NEWEST"), "{command}: {text}");
    }
}

#[test]
fn missing_malformed_and_stale_environment_never_acquire_a_fallback_target() {
    let lab = Lab::new();
    for session in [
        None,
        Some(""),
        Some("malformed"),
        Some("alice/context@absent"),
        Some("alice/missing@selected"),
    ] {
        for command in ["log", "show", "view"] {
            for target in [None, Some("@")] {
                let mut args = vec![command];
                args.extend(target);
                let output = lab.run(session, &args);
                if command == "log" && session == Some("alice/missing@selected") {
                    assert_eq!(
                        output.status.code(),
                        Some(agit::ExitCode::Ref.as_i32()),
                        "{args:?}: {output:?}"
                    );
                    assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
                } else {
                    failed_without_notice(output);
                }
            }
        }
    }
    for session in [None, Some("malformed"), Some(SELECTED)] {
        for command in ["log", "show", "view"] {
            for target in [None, Some("@")] {
                let mut args = vec![command];
                args.extend(target);
                let output = lab
                    .command(session, &args)
                    .env("CODEX_SESSION_ID", RUNTIME_ID)
                    .output()
                    .unwrap();
                if session == Some(SELECTED) {
                    assert!(
                        String::from_utf8_lossy(&output.stderr).contains("stale target"),
                        "{output:?}"
                    );
                }
                failed_without_notice(output);
            }
        }
    }
}

#[test]
fn historical_notices_identify_the_selected_commit_and_invalid_refs_emit_nothing() {
    let lab = Lab::new();
    let expected = format!(
        "target: alice/context@{} (via explicit arguments)",
        lab.first_sha
    );
    for command in ["log", "show", "view"] {
        let text = notice(
            lab.run(Some(NEWEST), &[command, "alice/context@selected~1"]),
            &expected,
        );
        assert!(text.contains("SELECTED-FIRST"), "{command}: {text}");
        assert!(!text.contains("SELECTED-SECOND"), "{command}: {text}");
        for target in ["alice/context@absent", "alice/context@selected~99"] {
            failed_without_notice(lab.run(Some(SELECTED), &[command, target]));
        }
    }
    failed_without_notice(lab.run(Some(SELECTED), &["show", "alice/context@selected#99.1"]));
}

#[test]
fn raw_event_file_and_export_modes_preserve_their_exact_payload_bytes() {
    let lab = Lab::new();
    let log = format!("{}{}", lab.first_raw, lab.second_raw);
    let event = format!("{}\n", lab.second_raw.lines().nth(1).unwrap());
    let path = b"SELECTED-SECOND\n\tspaces  \n\n";
    for (args, expected) in [
        (vec!["show", SELECTED, "--raw"], lab.second_raw.as_bytes()),
        (vec!["show", "@", "--raw"], lab.second_raw.as_bytes()),
        (
            vec!["show", SELECTED, "--raw", "--log-only"],
            log.as_bytes(),
        ),
        (
            vec!["show", "alice/context@selected#1", "--raw"],
            lab.first_raw.as_bytes(),
        ),
        (vec!["show", "alice/context@selected#2.2"], event.as_bytes()),
        (
            vec!["show", "alice/context@selected#2.2", "--raw"],
            event.as_bytes(),
        ),
        (
            vec!["show", "alice/context@selected:payload.txt"],
            path.as_slice(),
        ),
        (vec!["export", SELECTED], log.as_bytes()),
        (vec!["export", "@", "--format", "jsonl"], log.as_bytes()),
        (vec!["export", SELECTED, "-o", "-"], log.as_bytes()),
        (
            vec!["export", SELECTED, "--view-only"],
            lab.second_raw.as_bytes(),
        ),
    ] {
        let output = lab.run(Some(SELECTED), &args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert_eq!(output.stdout, expected, "{args:?}");
    }
    assert!(!lab.work.join("-").exists());
}

#[test]
fn mcp_subprocess_results_keep_the_tool_payload_without_cli_notices() {
    let lab = Lab::new();
    let rendered = notice(
        lab.run(Some(NEWEST), &["show", SELECTED]),
        "target: alice/context@selected (via explicit arguments)",
    );
    let expected_show = rendered.split_once('\n').unwrap().1;
    let expected_event = format!("{}\n", lab.second_raw.lines().nth(1).unwrap());
    let requests = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"context-echo-test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"show","arguments":{"ref":SELECTED}}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"show","arguments":{"ref":"alice/context@selected#2.2"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"view","arguments":{"ref":SELECTED}}}),
    ];
    let mut server = lab
        .command(Some(NEWEST), &["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = server.stdin.take().unwrap();
        for request in requests {
            writeln!(input, "{request}").unwrap();
        }
    }
    let output = server.wait_with_output().unwrap();
    assert!(output.stderr.is_empty(), "{output:?}");
    let text = success(output);
    let replies: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 4, "{text}");
    for (reply, id) in replies.iter().zip(1..=4) {
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id);
        assert!(reply.get("error").is_none(), "{reply}");
    }
    assert_eq!(replies[1]["result"]["content"][0]["text"], expected_show);
    assert_eq!(replies[2]["result"]["content"][0]["text"], expected_event);
    let view: Value =
        serde_json::from_str(replies[3]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(view.is_array(), "{view}");
    assert_eq!(
        view.as_array().unwrap().len(),
        lab.second_raw.lines().count()
    );
    assert!(view.to_string().contains("SELECTED-SECOND"), "{view}");
    assert!(!text.contains("target: "), "{text}");
}

#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn json_versions_keep_one_closed_envelope_without_a_human_notice() {
    let lab = Lab::new();
    let _other = lab.other_repo();
    for version in ["1", "2"] {
        for command_args in [
            vec!["log", SELECTED],
            vec!["show", SELECTED],
            vec!["view", SELECTED],
            vec!["view", SELECTED, "--json"],
            vec!["show", SELECTED, "--raw"],
            vec!["show", "alice/context@selected#2.2"],
            vec!["show", "alice/context@selected:payload.txt"],
            vec!["export", SELECTED],
            vec!["diff", "alice/context@selected..bob/other@topic", "--view"],
            vec!["diff", "alice/context@selected..bob/other@topic", "--turns"],
            vec!["diff", "alice/context@selected~1..selected", "--files"],
        ] {
            let mut args = vec!["--json", "--json-version", version];
            args.extend(command_args.iter().copied());
            let output = lab.run(Some(NEWEST), &args);
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
            let value: Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("{args:?}: {error}: {output:?}"));
            assert_eq!(value["schema"], "cli-output");
            assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
            assert_eq!(value["command"], command_args[0]);
            assert_eq!(value["ok"], true);
            assert_eq!(value["exit_code"], 0);
            let mut keys: BTreeSet<&str> = [
                "schema",
                "schema_version",
                "command",
                "ok",
                "exit_code",
                "result",
                "diagnostics",
            ]
            .into_iter()
            .collect();
            if version == "2" {
                keys.insert("fix");
            }
            assert_eq!(
                value
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                keys
            );
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("target: "),
                "{args:?}: {value}"
            );
        }
        for command in ["log", "show", "view"] {
            let output = lab.run(
                Some(SELECTED),
                &[
                    "--json",
                    "--json-version",
                    version,
                    command,
                    "alice/context@absent",
                ],
            );
            assert!(!output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["result"]["format"], "empty");
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("target: "),
                "{value}"
            );
        }
    }
}

#[test]
fn quiet_flag_and_environment_remove_only_the_new_notice_from_read_output() {
    let lab = Lab::new();
    for args in [
        vec!["log", SELECTED],
        vec!["show", SELECTED],
        vec!["view", SELECTED],
        vec!["diff", "alice/context@selected~1..selected", "--view"],
        vec!["diff", "alice/context@selected~1..selected", "--turns"],
    ] {
        let human = success(lab.run(Some(NEWEST), &args));
        assert!(human.starts_with("target: "), "{args:?}: {human}");
        let body = human.split_once('\n').unwrap().1;
        let mut quiet_args = vec!["-q"];
        quiet_args.extend(args.iter().copied());
        let flag = success(lab.run(Some(NEWEST), &quiet_args));
        assert_eq!(flag, body, "{quiet_args:?}");
        for quiet in ["", "1"] {
            let env = success(
                lab.command(Some(NEWEST), &args)
                    .env("AGIT_QUIET", quiet)
                    .output()
                    .unwrap(),
            );
            assert_eq!(env, body, "AGIT_QUIET={quiet:?}: {args:?}");
        }
    }
}

#[test]
fn cross_repository_diff_reports_both_verified_points_before_the_report() {
    let lab = Lab::new();
    let (_other, other_sha) = lab.other_repo();
    let expected = format!(
        "target: left=alice/context@{} (via explicit arguments); right=bob/other@{other_sha} (via explicit arguments)",
        lab.selected_sha,
    );
    for mode in ["--view", "--turns"] {
        let text = notice(
            lab.run(
                Some(NEWEST),
                &["diff", "alice/context@selected..bob/other@topic", mode],
            ),
            &expected,
        );
        assert!(
            text.contains(if mode == "--view" {
                "removed"
            } else {
                "B side"
            }),
            "{text}"
        );
        for range in [
            "alice/context@selected..bob/other@absent",
            "alice/context@selected..bob/missing@topic",
        ] {
            failed_without_notice(lab.run(Some(SELECTED), &["diff", range, mode]));
        }
    }
}

#[test]
fn file_diff_keeps_git_patch_bytes_at_the_explicit_historical_points() {
    let lab = Lab::new();
    let expected = lab.git(
        &lab.repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            &lab.first_sha,
            &lab.selected_sha,
            "--",
            ".",
            ":(exclude)session",
            ":(exclude)LOG",
            ":(exclude)VIEW",
            ":(exclude)events",
        ],
    );
    assert!(
        expected.contains("-SELECTED-FIRST") && expected.contains("+SELECTED-SECOND"),
        "{expected}"
    );
    let output = lab.run(
        Some(NEWEST),
        &["diff", "alice/context@selected~1..selected", "--files"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, expected.trim_end().as_bytes());
}

#[test]
fn branch_and_tag_notices_match_the_repository_and_refs_actually_changed() {
    let lab = Lab::new();
    let (other, other_sha) = lab.other_repo();
    notice(
        lab.run(Some("bob/other@topic"), &["branch", "--repo", REPO]),
        "target: alice/context (via explicit arguments)",
    );
    notice(
        lab.run(Some(SELECTED), &["branch"]),
        "target: alice/context (via AGIT_SESSION)",
    );
    lab.git(&lab.repo, &["branch", "rename-me", "selected"]);
    notice(
        lab.run(
            Some("bob/other@topic"),
            &["branch", "--repo", REPO, "rename", "rename-me", "renamed"],
        ),
        "target: alice/context@rename-me (via explicit arguments)",
    );
    notice(
        lab.run(
            Some(SELECTED),
            &["branch", "rename", "renamed", "renamed-env"],
        ),
        "target: alice/context@renamed (via explicit arguments + AGIT_SESSION)",
    );
    assert_eq!(
        lab.git(&lab.repo, &["rev-parse", "refs/heads/renamed-env"])
            .trim(),
        lab.selected_sha
    );
    assert!(!lab.repo.has_ref("refs/heads/rename-me"));
    assert!(!lab.repo.has_ref("refs/heads/renamed"));
    assert_eq!(
        lab.git(&other, &["rev-parse", "refs/heads/topic"]).trim(),
        other_sha
    );
    assert!(!other.has_ref("refs/heads/renamed-env"));
    for (tag, session, target, expected) in [
        (
            "echo-mixed",
            NEWEST,
            "selected",
            "target: alice/context@selected (via explicit arguments + AGIT_SESSION)",
        ),
        (
            "echo-explicit",
            "bob/other@topic",
            SELECTED,
            "target: alice/context@selected (via explicit arguments)",
        ),
    ] {
        notice(lab.run(Some(session), &["tag", tag, target]), expected);
        assert_eq!(
            lab.git(
                &lab.repo,
                &["rev-parse", &format!("refs/tags/{tag}^{{commit}}")]
            )
            .trim(),
            lab.selected_sha
        );
        assert!(!other.has_ref(&format!("refs/tags/{tag}")));
    }
    failed_without_notice(lab.run(Some(SELECTED), &["tag", "must-not-exist", "absent"]));
    assert!(!lab.repo.has_ref("refs/tags/must-not-exist"));
}

#[test]
fn run_selector_refusals_precede_fork_consent_in_every_presentation_mode() {
    let lab = Lab::new();
    let refs_before = lab.git(&lab.repo, &["show-ref"]);
    let link_path = lab.store.join(format!("store/codex/{RUNTIME_ID}.json"));
    let link_before = fs::read(&link_path).unwrap();
    for (selector, expected) in [
        ("absent", agit::ExitCode::Ref),
        ("selected~99", agit::ExitCode::Ref),
        ("selected#99", agit::ExitCode::Ref),
        ("selected#1..2", agit::ExitCode::Usage),
        ("selected:payload.txt", agit::ExitCode::Usage),
        ("selected~1", agit::ExitCode::Interactive),
    ] {
        for mode in [
            "human", "flag", "empty", "one", "protocol", "json1", "json2",
        ] {
            if mode.starts_with("json") && !cfg!(any(unix, all(windows, target_env = "msvc"))) {
                continue;
            }
            for name_fork in [false, true] {
                if name_fork && expected == agit::ExitCode::Interactive {
                    continue;
                }
                let mut args = Vec::new();
                if mode == "flag" {
                    args.push("--quiet");
                }
                if let Some(version) = mode.strip_prefix("json") {
                    args.extend(["--json", "--json-version", version]);
                }
                args.extend(["run", selector, "--no-launch"]);
                if name_fork {
                    args.extend(["-b", "must-not-exist"]);
                }
                let mut command = lab.command(Some(SELECTED), &args);
                match mode {
                    "empty" => {
                        command.env("AGIT_QUIET", "");
                    }
                    "one" => {
                        command.env("AGIT_QUIET", "1");
                    }
                    "protocol" => {
                        command.env("AGIT_PROTOCOL_CHILD", "1");
                    }
                    _ => {}
                }
                let output = command.output().unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(expected.as_i32()),
                    "{mode}: {args:?}: {output:?}"
                );
                let diagnostic = if mode.starts_with("json") {
                    assert!(output.stderr.is_empty(), "{output:?}");
                    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(document["ok"], false);
                    assert_eq!(document["exit_code"], expected.as_i32());
                    document["diagnostics"].to_string()
                } else {
                    if expected != agit::ExitCode::Interactive {
                        assert!(output.stdout.is_empty(), "{output:?}");
                    }
                    String::from_utf8(output.stderr).unwrap()
                };
                assert_eq!(
                    diagnostic.contains("non-interactive runs must pass -b"),
                    expected == agit::ExitCode::Interactive,
                    "{mode}: {args:?}: {diagnostic}"
                );
                assert_eq!(lab.git(&lab.repo, &["show-ref"]), refs_before);
                assert_eq!(fs::read(&link_path).unwrap(), link_before);
                assert_eq!(
                    agit::domain::link::list(&agit::domain::store::Store::at(
                        lab.store.join("store")
                    ))
                    .len(),
                    1
                );
            }
        }
    }
}

#[test]
fn historical_fork_with_prepared_resume_emits_only_its_verified_source_notice() {
    let lab = Lab::new();
    let text = notice(
        lab.run(
            Some(SELECTED),
            &[
                "fork",
                "@#1",
                "-b",
                "historic-fork",
                "--resume",
                "--no-launch",
                "--as",
                "codex",
            ],
        ),
        &format!("target: alice/context@{} (via AGIT_SESSION)", lab.first_sha),
    );
    assert!(text.contains("historic-fork"), "{text}");
    assert_eq!(
        lab.git(&lab.repo, &["rev-parse", "refs/heads/historic-fork^"])
            .trim(),
        lab.first_sha
    );
    assert_eq!(
        lab.git(&lab.repo, &["rev-parse", "refs/heads/selected"])
            .trim(),
        lab.selected_sha
    );
    let view =
        storage::materialize_at(lab.repo.root(), "refs/heads/historic-fork", meta::VIEW_FILE)
            .unwrap();
    assert!(view.contains("SELECTED-FIRST"), "{view}");
    assert!(!view.contains("SELECTED-SECOND"), "{view}");
    let store = agit::domain::store::Store::at(lab.store.join("store"));
    let installed = agit::domain::link::list(&store)
        .into_iter()
        .filter(|link| link.branch.as_deref() == Some("historic-fork"))
        .collect::<Vec<_>>();
    assert_eq!(installed.len(), 1, "{installed:?}");
    assert_eq!(installed[0].source, "codex");
    assert_eq!(installed[0].owner.as_deref(), Some("alice"));
    assert_eq!(installed[0].agent.as_deref(), Some("context"));
}

#[test]
fn merge_recon_reports_source_and_destination_only_after_both_resolve() {
    let lab = Lab::new();
    let before = lab.git(&lab.repo, &["show-ref"]);
    for (session, source, into, expected) in [
        (
            None,
            NEWEST,
            SELECTED,
            "target: into=alice/context@selected (via explicit arguments); from=alice/context@newest (via explicit arguments)",
        ),
        (
            Some(SELECTED),
            "newest",
            "selected",
            "target: into=alice/context@selected (via explicit arguments + AGIT_SESSION); from=alice/context@newest (via explicit arguments + AGIT_SESSION)",
        ),
    ] {
        let text = notice(
            lab.run(session, &["merge", source, "--into", into, "--dry-run"]),
            expected,
        );
        assert!(text.contains("no lock taken, no agent started"), "{text}");
    }
    for (source, into) in [
        ("alice/context@absent", SELECTED),
        (NEWEST, "alice/context@absent"),
    ] {
        failed_without_notice(lab.run(
            Some(SELECTED),
            &["merge", source, "--into", into, "--dry-run"],
        ));
    }
    assert_eq!(lab.git(&lab.repo, &["show-ref"]), before);
}

#[test]
fn scan_notice_reports_repository_scope_and_finds_evidence_on_an_unselected_branch() {
    let lab = Lab::new();
    notice(
        lab.run(Some(NEWEST), &["scan", SELECTED, "--secrets"]),
        "target: repo=alice/context (via explicit arguments)",
    );
    let synthetic_key = "AKIA4X7QZ2M5RT6VW3JH";
    lab.git(
        &lab.repo,
        &[
            "commit",
            "--allow-empty",
            "-qm",
            &format!("synthetic scan fixture {synthetic_key}"),
        ],
    );
    assert!(
        !lab.git(&lab.repo, &["log", "refs/heads/selected", "--format=%B"])
            .contains(synthetic_key)
    );
    let output = lab.run(Some(NEWEST), &["scan", SELECTED, "--secrets"]);
    assert!(!output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.lines().next(),
        Some("target: repo=alice/context (via explicit arguments)")
    );
    assert!(text.contains("aws-access-token"), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("target: "))
            .count(),
        1,
        "{text}"
    );
    failed_without_notice(lab.run(
        Some(NEWEST),
        &["scan", SELECTED, "alice/context@absent", "--secrets"],
    ));
}

#[test]
fn no_op_distillation_names_the_source_branch_and_main_destination() {
    let lab = Lab::new();
    let before = lab.git(&lab.repo, &["show-ref"]);
    for args in [
        vec!["memory", "distill", "--into", SELECTED, "-y"],
        vec!["distill", "--into", SELECTED, "-y"],
    ] {
        let text = notice(
            lab.run(Some(NEWEST), &args),
            "target: from=alice/context@selected (via explicit arguments); into=alice/context@main (via explicit arguments)",
        );
        assert!(text.contains("nothing to distill"), "{text}");
    }
    assert_eq!(lab.git(&lab.repo, &["show-ref"]), before);
}
