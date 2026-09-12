//! Local prefix evidence is observable before an import has authority to change state.

use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const NATIVE_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const HUB: &str = "http://127.0.0.1:1";

struct Lab {
    temporary: tempfile::TempDir,
    home: PathBuf,
    agit: PathBuf,
    work: PathBuf,
    native: PathBuf,
}

fn turn(text: &str) -> String {
    format!(
        "{}\n{}\n",
        serde_json::json!({"type":"user","message":{"role":"user","content":text}}),
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"SYNTHETIC-ANSWER"}]}}),
    )
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let agit = temporary.path().join("agit");
        let work = temporary.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let native = home
            .join(".claude/projects/synthetic")
            .join(format!("{NATIVE_ID}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        fs::write(
            &native,
            format!(
                "{}{}",
                turn("SYNTHETIC-PREFIX"),
                turn("SYNTHETIC-CONTINUATION")
            ),
        )
        .unwrap();
        Self {
            temporary,
            home,
            agit,
            work,
            native,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.agit)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_SESSION", "ignored/unselected@branch")
            .env("GIT_CONFIG_GLOBAL", self.home.join("absent-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("NO_COLOR", "1")
            .env("CI", "1")
            .current_dir(&self.work);
        #[cfg(windows)]
        command.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn save_credential(&self) {
        agit::infra::credentials::save_at(
            &self.agit.join("credentials").join(format!(
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
    }

    fn preview(&self, version: &str) -> Output {
        self.command()
            .args([
                "--json",
                "--json-version",
                version,
                "import",
                "--from=claude-code",
                "--into=me/repo@imported",
                "--propose-lineage",
                "--",
                NATIVE_ID,
            ])
            .output()
            .unwrap()
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.temporary.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                let bytes = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (entry.path().to_owned(), bytes)
            })
            .collect()
    }

    fn repository(&self, legacy: bool) -> (Repo, String) {
        let repo = Repo::init(&self.agit.join("repos/me/repo")).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("synthetic shared file line").unwrap();
        let base = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/source"])
            .unwrap();
        repo.git(&["update-ref", "refs/heads/source", &base])
            .unwrap();
        let mut metadata = meta::Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            self.work.to_str().unwrap().into(),
        );
        if legacy {
            metadata.layout = meta::LayoutVersion::V0;
        }
        meta::write(repo.root(), &metadata).unwrap();
        let log =
            transcript::wrap_lines(&turn("SYNTHETIC-PREFIX"), "claude-code", &metadata.session);
        if legacy {
            fs::write(repo.root().join(meta::LEGACY_LOG_FILE), &log).unwrap();
            fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), &log).unwrap();
        } else {
            storage::write_snapshot(repo.root(), &log, &log).unwrap();
        }
        repo.add_all().unwrap();
        repo.commit("synthetic selected prefix").unwrap();
        let source = repo.git(&["rev-parse", "HEAD"]).unwrap();
        (repo, source)
    }
}

fn document(output: &Output, version: u64) -> Value {
    assert!(output.status.success(), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], "cli-output");
    assert_eq!(document["schema_version"], version);
    assert_eq!(document["result"]["format"], "json");
    assert_eq!(document["result"]["value"]["schema"], "import-lineage");
    assert_eq!(document["result"]["value"]["operation"], "preview");
    assert_eq!(
        document["result"]["value"]["semantic_discovery_available"],
        false
    );
    if version == 1 {
        assert!(document.get("fix").is_none());
    } else {
        assert_eq!(document["fix"], serde_json::json!([]));
    }
    document
}

#[test]
fn a_missing_local_repository_is_explicit_and_preview_does_not_initialize_state() {
    let lab = Lab::new();
    let before = lab.state();
    for version in [1, 2] {
        let result = document(&lab.preview(&version.to_string()), version);
        let report = &result["result"]["value"];
        assert_eq!(report["scan_state"], "incomplete");
        assert_eq!(
            report["unavailable"],
            serde_json::json!([{"commit":null,"reason":"local_repository"}])
        );
        assert_eq!(
            report["independent"]["argv"],
            serde_json::json!([
                "agit",
                "import",
                "--from=claude-code",
                "--into=me/repo@imported",
                "--independent",
                "--",
                NATIVE_ID
            ])
        );
        assert_eq!(lab.state(), before);
        assert!(!lab.agit.exists());
    }
}

#[test]
fn preview_reads_legacy_and_current_prefixes_without_claims_migration_or_credentials() {
    for legacy in [false, true] {
        let lab = Lab::new();
        let (repo, source) = lab.repository(legacy);
        repo.git(&["branch", "alias", &source]).unwrap();
        let before = lab.state();
        for version in [1, 2] {
            let result = document(&lab.preview(&version.to_string()), version);
            let report = &result["result"]["value"];
            assert_eq!(report["scan_state"], "complete", "{report}");
            let candidates = report["candidates"].as_array().unwrap();
            assert_eq!(candidates.len(), 1, "{report}");
            assert_eq!(candidates[0]["commit"], source);
            assert_eq!(candidates[0]["completed_turns"], 1);
            assert_eq!(candidates[0]["records"], 2);
            assert_eq!(candidates[0]["evidence"], "exact_native_records");
            assert_eq!(
                candidates[0]["apply"]["argv"][4],
                format!("--onto={source}")
            );
            assert_eq!(
                candidates[0]["apply"]["env"]["AGIT_HOME"],
                lab.agit.to_str().unwrap()
            );
            assert_eq!(candidates[0]["apply"]["env"]["AGIT_HUB_URL"], HUB);
            assert_eq!(lab.state(), before);
            assert!(!lab.agit.join("store").exists());
            assert!(!lab.agit.join("credentials").exists());
        }
    }
}

#[test]
fn unreadable_claim_and_partial_native_evidence_never_become_empty_history() {
    let lab = Lab::new();
    lab.repository(false);
    let link = lab
        .agit
        .join("store/claude-code")
        .join(format!("{NATIVE_ID}.json"));
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    for bytes in [
        b"[]".as_slice(),
        b"{\"baseline_bytes\":\"unknown\"}",
        b"{",
        b"{\"branch\":\"a\",\"branch\":\"b\"}",
    ] {
        fs::write(&link, bytes).unwrap();
        let before = lab.state();
        let value = document(&lab.preview("2"), 2);
        assert_eq!(
            value["result"]["value"]["unavailable"][0]["reason"],
            "session_link"
        );
        assert!(
            value["result"]["value"]["candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(lab.state(), before);
    }
    fs::remove_file(link).unwrap();
    for (bytes, reason) in [
        (b"{\"type\":\"user\"}".as_slice(), "native_incomplete"),
        (b"not json\n", "native_records"),
    ] {
        fs::write(&lab.native, bytes).unwrap();
        let before = lab.state();
        let value = document(&lab.preview("2"), 2);
        assert_eq!(value["result"]["value"]["unavailable"][0]["reason"], reason);
        assert_eq!(lab.state(), before);
    }
}

#[test]
fn explicit_preview_arguments_do_not_fall_back_to_process_or_directory_identity() {
    let lab = Lab::new();
    let before = lab.state();
    for args in [
        vec!["import", "--propose-lineage"],
        vec![
            "import",
            NATIVE_ID,
            "--propose-lineage",
            "--into=me/repo@imported",
        ],
        vec![
            "import",
            "@",
            "--from=claude-code",
            "--propose-lineage",
            "--into=me/repo@imported",
        ],
        vec![
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--propose-lineage",
            "--into=me/repo",
        ],
        vec![
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--propose-lineage",
            "--into=me/repo@main",
        ],
        vec![
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--propose-lineage",
            "--into=me/repo@imported",
            "--independent",
        ],
    ] {
        let output = lab.command().args(["--json"]).args(&args).output().unwrap();
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert_eq!(lab.state(), before);
    }
    let absent = lab
        .command()
        .args([
            "--json",
            "import",
            "--from=claude-code",
            "--propose-lineage",
            "--into=me/repo@imported",
            "--",
            "aaaaaaaa",
        ])
        .output()
        .unwrap();
    assert!(!absent.status.success());
    assert_eq!(lab.state(), before);
}

#[test]
fn a_returned_base_command_uses_the_frozen_commit_and_settles_the_continuation() {
    let lab = Lab::new();
    let (repo, source) = lab.repository(false);
    let value = document(&lab.preview("2"), 2);
    let action = &value["result"]["value"]["candidates"][0]["apply"];
    repo.git(&["branch", "alias", &source]).unwrap();
    lab.save_credential();
    let argv = action["argv"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|item| item.as_str().unwrap())
        .collect::<Vec<_>>();
    let mut command = lab.command();
    command
        .arg("--json")
        .args(&argv)
        .current_dir(action["cwd"].as_str().unwrap());
    for (key, value) in action["env"].as_object().unwrap() {
        command.env(key, value.as_str().unwrap());
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        repo.git_status(&[
            "merge-base",
            "--is-ancestor",
            &source,
            "refs/heads/imported"
        ])
        .unwrap()
        .0,
        Some(0)
    );
    let log = storage::materialize_at(repo.root(), "refs/heads/imported", meta::LOG_FILE).unwrap();
    let view =
        storage::materialize_at(repo.root(), "refs/heads/imported", meta::VIEW_FILE).unwrap();
    assert_eq!(log, view);
    assert_eq!(log.matches("SYNTHETIC-PREFIX").count(), 1);
    assert_eq!(log.matches("SYNTHETIC-CONTINUATION").count(), 1);
    let link = agit::domain::link::get(
        &agit::domain::store::Store::at(lab.agit.join("store")),
        "claude-code",
        NATIVE_ID,
    )
    .unwrap();
    assert_eq!(link.owner.as_deref(), Some("me"));
    assert_eq!(link.agent.as_deref(), Some("repo"));
    assert_eq!(link.branch.as_deref(), Some("imported"));
}

#[test]
fn printed_commands_keep_the_selected_directory_routing_and_literal_branch() {
    let mut lab = Lab::new();
    let selected = lab.work.join("selected's workspace");
    fs::create_dir_all(&selected).unwrap();
    let relative = "relative's-store";
    lab.agit = selected.join(relative);
    let (repo, source) = lab.repository(false);
    lab.save_credential();
    let branch = "literal'branch";
    let destination = format!("--into=me/repo@{branch}");
    let before = lab.state();
    let preview = lab
        .command()
        .env("AGIT_HOME", relative)
        .args([
            "-C",
            selected.to_str().unwrap(),
            "import",
            "--from=claude-code",
        ])
        .args([&destination, "--propose-lineage", "--", NATIVE_ID])
        .output()
        .unwrap();
    assert!(preview.status.success(), "{preview:?}");
    assert_eq!(lab.state(), before);
    let text = String::from_utf8(preview.stderr).unwrap();
    let printed = text
        .lines()
        .find(|line| line.starts_with("  ") && line.contains(&format!("--onto={source}")))
        .unwrap()
        .trim_start();
    let configured = lab.command();
    let mut shell = Command::new(if cfg!(windows) { "pwsh" } else { "sh" });
    shell.env_clear();
    for (key, value) in configured.get_envs() {
        if let Some(value) = value {
            shell.env(key, value);
        }
    }
    let mut path = vec![
        PathBuf::from(env!("CARGO_BIN_EXE_agit"))
            .parent()
            .unwrap()
            .to_owned(),
    ];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    shell
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("AGIT_HOME", relative)
        .env("AGIT_HUB_URL", "https://wrong-routing.invalid")
        .current_dir(&lab.work);
    if cfg!(windows) {
        shell.args(["-NoProfile", "-NonInteractive", "-Command", printed]);
    } else {
        shell.args(["-c", printed]);
    }
    let output = shell.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let head = format!("refs/heads/{branch}");
    assert_eq!(
        repo.git_status(&["merge-base", "--is-ancestor", &source, &head])
            .unwrap()
            .0,
        Some(0)
    );
    let log = storage::materialize_at(repo.root(), &head, meta::LOG_FILE).unwrap();
    assert_eq!(log.matches("SYNTHETIC-PREFIX").count(), 1);
    assert_eq!(log.matches("SYNTHETIC-CONTINUATION").count(), 1);
    let link = agit::domain::link::get(
        &agit::domain::store::Store::at(lab.agit.join("store")),
        "claude-code",
        NATIVE_ID,
    )
    .unwrap();
    assert_eq!(link.owner.as_deref(), Some("me"));
    assert_eq!(link.agent.as_deref(), Some("repo"));
    assert_eq!(link.branch.as_deref(), Some(branch));
    assert!(!lab.work.join(relative).exists());
}

#[cfg(unix)]
#[test]
fn preview_inspects_the_exact_linked_checkout_with_embedded_line_separators() {
    let lab = Lab::new();
    let (repo, source) = lab.repository(false);
    let prefix = lab.temporary.path().join("other");
    let unrelated = Repo::init(&prefix).unwrap();
    fs::write(
        unrelated.root().join("fixture"),
        b"synthetic independent checkout",
    )
    .unwrap();
    unrelated.add_all().unwrap();
    unrelated.commit("synthetic unrelated history").unwrap();
    let linked = lab.temporary.path().join("other\n\nsuffix");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "linked",
        linked.to_str().unwrap(),
        &source,
    ])
    .unwrap();
    let clean = document(&lab.preview("2"), 2);
    assert!(
        !clean["result"]["value"]["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["reason"] == "recovery")
    );
    let participant = Repo::at(&linked);
    let journal = participant
        .git_path("agit-checkout-transaction.json")
        .unwrap();
    fs::write(&journal, b"{}\n").unwrap();
    let before = lab.state();
    for version in [1, 2] {
        let observed = document(&lab.preview(&version.to_string()), version);
        assert_eq!(observed["result"]["value"]["scan_state"], "incomplete");
        assert!(
            observed["result"]["value"]["unavailable"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["reason"] == "recovery")
        );
        assert_eq!(lab.state(), before);
    }
}

#[cfg(unix)]
#[test]
fn preview_reports_only_rejected_worktree_framing_as_a_missing_git_capability() {
    use std::os::unix::fs::PermissionsExt;

    let lab = Lab::new();
    let (_, source) = lab.repository(false);
    let lookup = Command::new("/bin/sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    assert!(lookup.status.success());
    let real_git = fs::canonicalize(String::from_utf8(lookup.stdout).unwrap().trim()).unwrap();
    let shims = lab.temporary.path().join("git-shims");
    fs::create_dir(&shims).unwrap();
    let shim = shims.join("git");
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(shims.clone()).chain(std::env::split_paths(&inherited)),
    )
    .unwrap();
    for (status, reason) in [(129, "git_worktree_format"), (128, "recovery")] {
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\ncase \" $* \" in *\" worktree list --porcelain -z \"*) exit {status} ;; esac\nexec {} \"$@\"\n",
                agit::ui::session::shell_arg(real_git.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let before = lab.state();
        for version in [1, 2] {
            let output = lab
                .command()
                .env("PATH", &path)
                .args([
                    "--json",
                    "--json-version",
                    &version.to_string(),
                    "import",
                    "--from=claude-code",
                    "--into=me/repo@imported",
                    "--propose-lineage",
                    "--",
                    NATIVE_ID,
                ])
                .output()
                .unwrap();
            let observed = document(&output, version);
            let report = &observed["result"]["value"];
            assert_eq!(report["scan_state"], "incomplete");
            assert_eq!(
                report["unavailable"],
                serde_json::json!([{ "commit": null, "reason": reason }])
            );
            assert_eq!(report["candidates"], serde_json::json!([]));
            assert_eq!(lab.state(), before);
        }
        if status == 129 {
            let output = lab
                .command()
                .env("PATH", &path)
                .args([
                    "import",
                    "--from=claude-code",
                    "--into=me/repo@imported",
                    "--propose-lineage",
                    "--",
                    NATIVE_ID,
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(
                String::from_utf8(output.stderr)
                    .unwrap()
                    .contains("normally Git 2.36 or newer")
            );
            assert_eq!(lab.state(), before);
        }
    }
    let before = lab.state();
    for version in [1, 2] {
        let observed = document(&lab.preview(&version.to_string()), version);
        assert_eq!(observed["result"]["value"]["scan_state"], "complete");
        assert_eq!(
            observed["result"]["value"]["candidates"][0]["commit"],
            source
        );
        assert_eq!(lab.state(), before);
    }
    let help = lab.command().args(["import", "--help"]).output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("NUL-framed worktree output"));
    assert!(help.contains("Git 2.36 or newer"));
    assert_eq!(lab.state(), before);
}

#[test]
fn preview_never_fetches_missing_candidate_or_recovery_objects() {
    use std::io::{Read, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    for recovery in [false, true] {
        let lab = Lab::new();
        let (repo, head) = lab.repository(false);
        let path = if recovery {
            let old = repo.git(&["rev-parse", &format!("{head}^1")]).unwrap();
            repo.git(&["update-ref", "refs/agit/layout-v0/source", &old])
                .unwrap();
            meta::FILE.to_owned()
        } else {
            let ids = storage::parse_sequence(
                &fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap(),
            )
            .unwrap();
            meta::event_path(&ids[0]).unwrap()
        };
        let object = repo.git(&["rev-parse", &format!("{head}:{path}")]).unwrap();
        let objects = repo.git_path("objects").unwrap();
        fs::remove_file(objects.join(&object[..2]).join(&object[2..])).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/repository", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_hits = hits.clone();
        let worker_stop = stop.clone();
        let server = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            while !worker_stop.load(Ordering::SeqCst)
                && start.elapsed() < std::time::Duration::from_secs(20)
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        worker_hits.fetch_add(1, Ordering::SeqCst);
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut request = [0; 4096];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        for (name, value) in [
            ("core.repositoryformatversion", "1"),
            ("extensions.partialclone", "origin"),
            ("remote.origin.promisor", "true"),
            ("remote.origin.partialclonefilter", "blob:none"),
            ("remote.origin.url", &url),
        ] {
            repo.git(&["config", name, value]).unwrap();
        }
        let control = Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repo.root())
            .args([
                "-c",
                "http.proxy=",
                "-c",
                "credential.helper=",
                "cat-file",
                "blob",
                &object,
            ])
            .env_remove("GIT_NO_LAZY_FETCH")
            .env_remove("GIT_ALLOW_PROTOCOL")
            .output()
            .unwrap();
        let control_hits = hits.swap(0, Ordering::SeqCst);
        let before = lab.state();
        let output = lab.preview("2");
        let preview_hits = hits.load(Ordering::SeqCst);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(!control.status.success());
        assert!(
            control_hits > 0,
            "the ordinary missing-object read must contact its promisor"
        );
        assert_eq!(preview_hits, 0);
        let value = document(&output, 2);
        assert_eq!(value["result"]["value"]["scan_state"], "incomplete");
        assert!(
            value["result"]["value"]["unavailable"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["reason"]
                    == if recovery {
                        "recovery"
                    } else {
                        "stored_evidence"
                    })
        );
        assert_eq!(lab.state(), before);
    }
}

#[test]
fn opencode_preview_reads_database_rows_without_publishing_an_export_cache() {
    let lab = Lab::new();
    lab.repository(false);
    let database = lab.home.join(".local/share/opencode/opencode.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER, version TEXT);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);
        INSERT INTO session VALUES ('ses_selected', 'project', NULL, '/explicit', 1, 10, 'v');
        INSERT INTO message VALUES ('message', 'ses_selected', 2, '{\"role\":\"user\"}');
        INSERT INTO part VALUES ('part', 'ses_selected', 'message', 3, '{\"type\":\"text\",\"text\":\"SYNTHETIC-OPENCODE\"}');").unwrap();
    connection
        .execute(
            "UPDATE session SET directory = ?1 WHERE id = 'ses_selected'",
            [lab.work.to_str().unwrap()],
        )
        .unwrap();
    drop(connection);
    let command = || {
        lab.command()
            .args([
                "--json",
                "import",
                "--from=opencode",
                "--into=me/repo@imported",
                "--propose-lineage",
                "--",
                "ses_selected",
            ])
            .output()
            .unwrap()
    };
    let before = lab.state();
    let result = document(&command(), 2);
    assert_eq!(result["result"]["value"]["native"]["runtime"], "opencode");
    assert_eq!(result["result"]["value"]["scan_state"], "complete");
    assert_eq!(lab.state(), before);
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute("UPDATE part SET data = x'7b7d'", [])
        .unwrap();
    drop(connection);
    let before = lab.state();
    let result = document(&command(), 2);
    assert_eq!(
        result["result"]["value"]["unavailable"][0]["reason"],
        "native_database"
    );
    assert_eq!(lab.state(), before);
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        if let Ok(bytes) = fs::read(self.agit.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

#[test]
fn undecided_noninteractive_imports_report_actions_without_writing_state() {
    for existing in [false, true] {
        let lab = Lab::new();
        if existing {
            lab.repository(false);
        }
        let before = lab.state();
        for version in ["1", "2"] {
            for flag in ["--no-tui", "--yes"] {
                let output = lab
                    .command()
                    .args([
                        "--json",
                        "--json-version",
                        version,
                        flag,
                        "import",
                        NATIVE_ID,
                        "--from=claude-code",
                        "--into=me/repo@imported",
                    ])
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(8), "{output:?}");
                let document: Value = serde_json::from_slice(&output.stdout).unwrap();
                let report = &document["result"]["value"];
                assert_eq!(report["operation"], "choice_required");
                assert_eq!(
                    report["candidates"].as_array().unwrap().len(),
                    usize::from(existing)
                );
                if version == "1" {
                    assert!(document.get("fix").is_none());
                } else {
                    assert_eq!(
                        document["fix"].as_array().unwrap().len(),
                        usize::from(existing) + 1
                    );
                }
                assert_eq!(lab.state(), before);
            }
        }
        let output = lab
            .command()
            .args([
                "import",
                NATIVE_ID,
                "--from=claude-code",
                "--into=me/repo@imported",
                "--privacy",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(lab.state(), before);
    }
}

/// Terminal queries can span reads; answering them preserves every application byte.
fn append_terminal_output(
    output: &mut Vec<u8>,
    writer: &mut dyn std::io::Write,
    chunk: &[u8],
) -> std::io::Result<()> {
    const QUERY: &[u8] = b"\x1b[6n";
    const REPLY: &[u8] = b"\x1b[1;1R";
    let start = output.len().saturating_sub(QUERY.len() - 1);
    output.extend_from_slice(chunk);
    let mut answered = false;
    for window in output[start..].windows(QUERY.len()) {
        if window == QUERY {
            writer.write_all(REPLY)?;
            answered = true;
        }
    }
    if answered {
        writer.flush()?;
    }
    Ok(())
}

#[test]
fn terminal_cursor_queries_are_answered_across_chunks_without_consuming_output() {
    let input = b"before\x1b[6nbetween\x1b[6nafter";
    for split in 0..=input.len() {
        let mut output = Vec::new();
        let mut replies = Vec::new();
        append_terminal_output(&mut output, &mut replies, &input[..split]).unwrap();
        append_terminal_output(&mut output, &mut replies, &input[split..]).unwrap();
        assert_eq!(output, input);
        assert_eq!(replies, b"\x1b[1;1R\x1b[1;1R");
    }
    let mut output = Vec::new();
    let mut replies = Vec::new();
    for byte in input {
        append_terminal_output(&mut output, &mut replies, std::slice::from_ref(byte)).unwrap();
    }
    assert_eq!(output, input);
    assert_eq!(replies, b"\x1b[1;1R\x1b[1;1R");

    output.clear();
    replies.clear();
    append_terminal_output(&mut output, &mut replies, b"\x1b[5n\x1b[6x\x1b[").unwrap();
    assert!(replies.is_empty());
    append_terminal_output(&mut output, &mut replies, b"6").unwrap();
    assert!(replies.is_empty());
    append_terminal_output(&mut output, &mut replies, b"n").unwrap();
    assert_eq!(output, b"\x1b[5n\x1b[6x\x1b[6n");
    assert_eq!(replies, b"\x1b[1;1R");
}

#[test]
fn terminal_query_response_failures_preserve_received_diagnostics() {
    struct RefuseReply(bool);
    impl std::io::Write for RefuseReply {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0 {
                Ok(bytes.len())
            } else {
                Err(std::io::Error::other("synthetic terminal write failure"))
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("synthetic terminal flush failure"))
        }
    }
    for flush in [false, true] {
        let mut output = Vec::new();
        let error = append_terminal_output(&mut output, &mut RefuseReply(flush), b"message\x1b[6n")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(if flush { "flush" } else { "write" })
        );
        assert_eq!(output, b"message\x1b[6n");
    }
}

struct Dialogue {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    reader: std::sync::mpsc::Receiver<Vec<u8>>,
    output: Vec<u8>,
}

impl Dialogue {
    fn start(lab: &Lab, args: &[&str]) -> Self {
        Self::start_with_environment(lab, args, &[])
    }

    fn start_with_environment(
        lab: &Lab,
        args: &[&str],
        environment: &[(&str, &std::path::Path)],
    ) -> Self {
        use std::io::Read;
        let template = lab.command();
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in template.get_envs() {
            if name != "CI"
                && name != "AGIT_SESSION"
                && let Some(value) = value
            {
                command.env(name, value);
            }
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        command.env("TERM", "xterm-256color");
        command.args(args);
        command.cwd(&lab.work);
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 40,
                cols: 180,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(size) = reader.read(&mut buffer) {
                if size == 0 || send.send(buffer[..size].to_vec()).is_err() {
                    break;
                }
            }
        });
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        Self {
            child,
            _master: pty.master,
            writer,
            reader: receive,
            output: Vec::new(),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn prompt(&mut self) {
        self.wait_text("Choose the import lineage");
    }

    fn wait_text(&mut self, needle: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(bytes) = self
                .reader
                .recv_timeout(std::time::Duration::from_millis(20))
            {
                self.receive(&bytes);
            }
            if self.text().contains(needle) {
                return;
            }
            assert!(self.child.try_wait().unwrap().is_none(), "{}", self.text());
            assert!(std::time::Instant::now() < deadline, "{}", self.text());
        }
    }

    fn receive(&mut self, bytes: &[u8]) {
        append_terminal_output(&mut self.output, self.writer.as_mut(), bytes).unwrap_or_else(
            |error| panic!("terminal reply failed: {error}; output: {}", self.text()),
        );
    }

    fn send(&mut self, keys: &[u8]) {
        use std::io::Write;
        self.writer.write_all(keys).unwrap();
        self.writer.flush().unwrap();
    }

    fn finish(&mut self) -> u32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        loop {
            while let Ok(bytes) = self.reader.try_recv() {
                self.receive(&bytes);
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                while let Ok(bytes) = self
                    .reader
                    .recv_timeout(std::time::Duration::from_millis(30))
                {
                    self.receive(&bytes);
                }
                return status.exit_code();
            }
            assert!(std::time::Instant::now() < deadline, "{}", self.text());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Drop for Dialogue {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn choice(lab: &Lab) -> Dialogue {
    let mut dialogue = Dialogue::start(
        lab,
        &[
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--into=me/repo@imported",
        ],
    );
    dialogue.prompt();
    dialogue
}

#[test]
fn interactive_default_cancel_never_adopts_even_a_unique_candidate() {
    for existing in [false, true] {
        let lab = Lab::new();
        if existing {
            lab.repository(false);
        }
        let before = lab.state();
        let mut dialogue = choice(&lab);
        assert_eq!(lab.state(), before);
        dialogue.send(b"\r");
        assert_eq!(dialogue.finish(), 0, "{}", dialogue.text());
        assert_eq!(lab.state(), before);
    }
}

#[test]
fn accepted_lineage_rechecks_native_claim_and_destination_before_adoption() {
    for mutation in [
        "append",
        "rewrite",
        "replace",
        "claim",
        "target",
        "repository",
        "common",
        "cwd",
    ] {
        let lab = Lab::new();
        lab.save_credential();
        let (repo, source) = lab.repository(false);
        let mut dialogue = choice(&lab);
        match mutation {
            "append" => {
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&lab.native)
                    .unwrap()
                    .write_all(turn("later native content").as_bytes())
                    .unwrap();
            }
            "rewrite" => fs::write(&lab.native, turn("other native content")).unwrap(),
            "replace" => {
                let bytes = fs::read(&lab.native).unwrap();
                fs::rename(&lab.native, lab.native.with_extension("old")).unwrap();
                fs::write(&lab.native, bytes).unwrap();
            }
            "claim" => {
                let store = agit::domain::store::Store::at(lab.agit.join("store"));
                let mut link = agit::domain::link::Link::new("claude-code", NATIVE_ID, None);
                link.naming_ignored = true;
                agit::domain::link::write(&store, &link).unwrap();
            }
            "target" => {
                repo.git(&["update-ref", "refs/heads/imported", &source])
                    .unwrap();
            }
            "repository" => {
                let prior = lab.agit.join("repos/me/prior");
                let before = lab.state();
                match fs::rename(repo.root(), &prior) {
                    Ok(()) => {}
                    Err(error) if cfg!(windows) && error.raw_os_error() == Some(5) => {
                        assert_eq!(lab.state(), before);
                        dialogue.send(b"\r");
                        assert_eq!(dialogue.finish(), 0, "{}", dialogue.text());
                        assert_eq!(lab.state(), before);
                        fs::rename(repo.root(), prior).unwrap();
                        continue;
                    }
                    Err(error) => panic!("repository replacement failed: {error}"),
                }
                lab.repository(false);
            }
            "common" => {
                fs::rename(repo.root().join(".git"), lab.agit.join("prior.git")).unwrap();
                lab.repository(false);
            }
            "cwd" => {
                let original = fs::read_to_string(&lab.native).unwrap();
                fs::write(
                    &lab.native,
                    format!(
                        "{}\n{original}",
                        serde_json::json!({"type":"system","sessionId":NATIVE_ID,"cwd":lab.work})
                    ),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let changed = lab.state();
        dialogue.send(b"\x1b[B\r");
        assert_eq!(dialogue.finish(), 7, "{mutation}: {}", dialogue.text());
        assert_eq!(lab.state(), changed, "{mutation}");
    }
}

#[test]
fn selected_candidate_survives_alias_movement_and_repeat_import_stays_idempotent() {
    let lab = Lab::new();
    lab.save_credential();
    let (repo, source) = lab.repository(false);
    repo.git(&["switch", "main"]).unwrap();
    let mut dialogue = choice(&lab);
    let main = repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
    repo.git(&["update-ref", "refs/heads/source", &main])
        .unwrap();
    dialogue.send(b"\x1b[B\r");
    assert_eq!(dialogue.finish(), 0, "{}", dialogue.text());
    assert_eq!(
        repo.git(&["merge-base", &source, "refs/heads/imported"])
            .unwrap(),
        source
    );
    let store = agit::domain::store::Store::at(lab.agit.join("store"));
    let link = agit::domain::link::get(&store, "claude-code", NATIVE_ID).unwrap();
    assert_eq!(link.branch.as_deref(), Some("imported"));
    let tip = repo.git(&["rev-parse", "refs/heads/imported"]).unwrap();
    let output = lab
        .command()
        .args([
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--into=me/repo@imported",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/imported"]).unwrap(),
        tip
    );
}

#[test]
fn explicit_independent_selection_creates_a_real_line_after_the_choice() {
    let lab = Lab::new();
    lab.save_credential();
    let before = lab.state();
    let mut dialogue = choice(&lab);
    assert_eq!(lab.state(), before);
    dialogue.send(b"\x1b[B\r");
    dialogue.wait_text("Automatically push settled turns from this repository?");
    dialogue.send(b"\x1b[B\x1b[B\r");
    assert_eq!(dialogue.finish(), 0, "{}", dialogue.text());
    let repo = Repo::open(lab.agit.join("repos/me/repo")).unwrap();
    assert_eq!(repo.auto_push_override().unwrap(), Some(false));
    assert!(repo.has_ref("refs/heads/main"));
    assert!(repo.has_ref("refs/heads/imported"));
    let store = agit::domain::store::Store::at(lab.agit.join("store"));
    let link = agit::domain::link::get(&store, "claude-code", NATIVE_ID).unwrap();
    assert_eq!(link.owner.as_deref(), Some("me"));
    assert_eq!(link.branch.as_deref(), Some("imported"));
}

#[test]
fn first_independent_import_confirms_assets_from_the_frozen_native_directory() {
    for runtime in ["codex", "claude-code"] {
        let lab = Lab::new();
        lab.save_credential();
        let project = lab.temporary.path().join("native-project");
        for (directory, label) in [(&lab.work, "caller"), (&project, "native")] {
            let skill = directory.join(format!(".claude/skills/{label}/SKILL.md"));
            fs::create_dir_all(skill.parent().unwrap()).unwrap();
            fs::write(
                directory.join("AGENTS.md"),
                format!("SYNTHETIC-{label}-AGENTS\n"),
            )
            .unwrap();
            fs::write(skill, format!("SYNTHETIC-{label}-SKILL\n")).unwrap();
        }
        let native = if runtime == "codex" {
            let native = lab
                .home
                .join(".codex/sessions")
                .join(format!("rollout-2026-09-09T00-00-00-{NATIVE_ID}.jsonl"));
            fs::create_dir_all(native.parent().unwrap()).unwrap();
            let records = [
                serde_json::json!({"type":"session_meta","payload":{"id":NATIVE_ID,"cwd":project}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"SYNTHETIC-QUESTION"}]}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"SYNTHETIC-ANSWER"}]}}),
            ];
            fs::write(
                &native,
                records
                    .iter()
                    .map(|record| format!("{record}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            let database =
                rusqlite::Connection::open(lab.home.join(".codex/state_5.sqlite")).unwrap();
            database.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER)").unwrap();
            database
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2, ?3, 'SYNTHETIC-QUESTION', 'user', 1, 0)",
                    rusqlite::params![
                        NATIVE_ID,
                        native.to_str().unwrap(),
                        project.to_str().unwrap()
                    ],
                )
                .unwrap();
            native
        } else {
            let original = fs::read_to_string(&lab.native).unwrap();
            fs::write(
                &lab.native,
                format!(
                    "{}\n{original}",
                    serde_json::json!({"type":"system","sessionId":NATIVE_ID,"cwd":project})
                ),
            )
            .unwrap();
            lab.native.clone()
        };
        let before = lab.state();
        let from = format!("--from={runtime}");
        let mut dialogue = Dialogue::start(
            &lab,
            &["import", NATIVE_ID, &from, "--into=me/repo@imported"],
        );
        dialogue.prompt();
        assert_eq!(lab.state(), before);
        dialogue.send(b"\x1b[B\r");
        dialogue.wait_text("Automatically push settled turns from this repository?");
        dialogue.send(b"\r");
        dialogue.wait_text("adopt AGENTS.md");
        assert!(
            dialogue
                .text()
                .contains(&project.join("AGENTS.md").display().to_string()),
            "{}",
            dialogue.text()
        );
        assert!(
            !dialogue
                .text()
                .contains(&lab.work.join("AGENTS.md").display().to_string()),
            "{}",
            dialogue.text()
        );
        dialogue.send(b"\r");
        dialogue.wait_text("adopt skills/native/SKILL.md");
        dialogue.send(b"\r");
        assert_eq!(dialogue.finish(), 0, "{}", dialogue.text());
        let repo = Repo::open(lab.agit.join("repos/me/repo")).unwrap();
        assert_eq!(
            repo.git(&["show", "main:AGENTS.md"]).unwrap(),
            "SYNTHETIC-native-AGENTS"
        );
        assert_eq!(
            repo.git(&["show", "main:skills/native/SKILL.md"]).unwrap(),
            "SYNTHETIC-native-SKILL"
        );
        assert!(repo.git(&["show", "main:skills/caller/SKILL.md"]).is_err());
        let store = agit::domain::store::Store::at(lab.agit.join("store"));
        let link = agit::domain::link::get(&store, runtime, NATIVE_ID).unwrap();
        assert_eq!(link.cwd.as_deref(), project.to_str());
        assert_eq!(
            fs::read(&native).unwrap(),
            *before.get(&native).unwrap().as_ref().unwrap()
        );
        for (path, bytes) in &before {
            if (path.starts_with(&lab.work) || path.starts_with(&project))
                && let Some(bytes) = bytes
            {
                assert_eq!(fs::read(path).unwrap(), *bytes);
            }
        }
    }
}

#[test]
fn duplicate_native_directory_metadata_never_falls_back_to_caller_assets() {
    for runtime in ["claude-code", "codex"] {
        let lab = Lab::new();
        lab.save_credential();
        fs::write(lab.work.join("AGENTS.md"), "SYNTHETIC-CALLER\n").unwrap();
        let project = serde_json::to_string(&lab.temporary.path().join("native-project")).unwrap();
        if runtime == "codex" {
            let native = lab
                .home
                .join(".codex/sessions")
                .join(format!("rollout-2026-09-09T00-00-00-{NATIVE_ID}.jsonl"));
            fs::create_dir_all(native.parent().unwrap()).unwrap();
            fs::write(native, format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{NATIVE_ID}\",\"cwd\":{project}}},\"payload\":{{\"id\":\"{NATIVE_ID}\",\"cwd\":{project}}}}}\n")).unwrap();
        } else {
            fs::write(&lab.native, format!("{{\"type\":\"user\",\"sessionId\":\"{NATIVE_ID}\",\"cwd\":{project},\"cwd\":{project}}}\n")).unwrap();
        }
        let before = lab.state();
        let from = format!("--from={runtime}");
        let output = lab
            .command()
            .args([
                "--json",
                "import",
                NATIVE_ID,
                &from,
                "--into=me/repo@imported",
                "--propose-lineage",
            ])
            .output()
            .unwrap();
        let document = document(&output, 2);
        let report = &document["result"]["value"];
        assert_eq!(report["scan_state"], "incomplete");
        assert!(
            report["unavailable"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["reason"] == "stored_evidence")
        );
        let mut dialogue = Dialogue::start(
            &lab,
            &["import", NATIVE_ID, &from, "--into=me/repo@imported"],
        );
        assert_eq!(dialogue.finish(), 8, "{}", dialogue.text());
        assert!(!dialogue.text().contains("Choose the import lineage"));
        assert!(!dialogue.text().contains("adopt AGENTS.md"));
        assert_eq!(lab.state(), before);
    }
}

/// The displayed choice is checked again after another writer releases the claim lock.
#[test]
fn accepted_choice_is_revalidated_inside_the_branch_and_claim_locks() {
    use fs2::FileExt as _;
    use sha2::Digest as _;
    let lab = Lab::new();
    lab.save_credential();
    let (repo, _) = lab.repository(false);
    let before_refs = repo.git(&["show-ref"]).unwrap();
    let mut dialogue = choice(&lab);
    let store = agit::domain::store::Store::at(lab.agit.join("store"));
    let guard = agit::domain::link::lock(&store, "claude-code", NATIVE_ID).unwrap();
    let mut digest = sha2::Sha256::new();
    digest.update(b"me/repo\0imported");
    let branch_lock = store
        .root()
        .join(".locks/branches")
        .join(format!("{}.lock", hex::encode(digest.finalize())));
    dialogue.send(b"\x1b[B\r");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&branch_lock)
        {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    fs2::FileExt::unlock(&file).unwrap();
                }
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    break;
                }
                Err(error) => panic!("{error}"),
            }
        }
        assert!(
            dialogue.child.try_wait().unwrap().is_none(),
            "{}",
            dialogue.text()
        );
        assert!(std::time::Instant::now() < deadline, "{}", dialogue.text());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fs::write(
        &lab.native,
        turn("changed while waiting for the claim lock"),
    )
    .unwrap();
    drop(guard);
    assert_eq!(dialogue.finish(), 7, "{}", dialogue.text());
    assert_eq!(repo.git(&["show-ref"]).unwrap(), before_refs);
    assert!(agit::domain::link::get(&store, "claude-code", NATIVE_ID).is_none());
}

#[test]
fn inherited_git_routing_cannot_redirect_an_accepted_choice() {
    let lab = Lab::new();
    lab.save_credential();
    lab.repository(false);
    let foreign = Repo::init(&lab.temporary.path().join("foreign")).unwrap();
    fs::write(foreign.root().join("evidence"), "unrelated repository").unwrap();
    foreign.add_all().unwrap();
    foreign.commit("unrelated root").unwrap();
    let git_dir = foreign.root().join(".git");
    let before = lab.state();
    let mut dialogue = Dialogue::start_with_environment(
        &lab,
        &[
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--into=me/repo@imported",
        ],
        &[("GIT_DIR", &git_dir)],
    );
    dialogue.prompt();
    assert_eq!(lab.state(), before);
    dialogue.send(b"\x1b[B\r");
    assert_eq!(dialogue.finish(), 7, "{}", dialogue.text());
    assert!(dialogue.text().contains("unset GIT_DIR"));
    assert_eq!(lab.state(), before);
}

#[test]
fn cancelling_the_bare_picker_creates_no_store_or_rc_directory() {
    let mut lab = Lab::new();
    lab.work = lab.work.canonicalize().unwrap();
    #[cfg(windows)]
    {
        let path = lab.work.to_str().unwrap();
        lab.work = if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{share}"))
        } else {
            PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
        };
    }
    let selected = lab
        .home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&lab.work))
        .join(format!("{NATIVE_ID}.jsonl"));
    fs::create_dir_all(selected.parent().unwrap()).unwrap();
    fs::rename(&lab.native, &selected).unwrap();
    lab.native = selected;
    let before = lab.state();
    let mut dialogue = Dialogue::start(&lab, &["import"]);
    dialogue.wait_text("agit import");
    dialogue.send(b"q");
    assert_eq!(dialogue.finish(), 0, "{}", dialogue.text());
    assert_eq!(lab.state(), before);
}

#[test]
fn explicit_link_only_privacy_keeps_offline_copy_and_link_behavior() {
    let lab = Lab::new();
    let original = fs::read(&lab.native).unwrap();
    let output = lab
        .command()
        .args([
            "import",
            NATIVE_ID,
            "--from=claude-code",
            "--link-only",
            "--privacy",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(&lab.native).unwrap(), original);
    let store = agit::domain::store::Store::at(lab.agit.join("store"));
    let links = agit::domain::link::list(&store);
    assert_eq!(links.len(), 1);
    assert_ne!(links[0].session_id, NATIVE_ID);
    assert_eq!(links[0].source, "claude-code");
    assert!(links[0].owner.is_none() && links[0].agent.is_none() && links[0].branch.is_none());
    assert!(!lab.agit.join("repos").exists());
    assert!(!lab.agit.join("credentials").exists());
}
