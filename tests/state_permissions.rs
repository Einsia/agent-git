//! Permission preparation must let the strict executor save synthetic native turns.

#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const HUB: &str = "http://127.0.0.1:1";
const NATIVE: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const NEXT: &str = "aaaaaaaa-0000-4000-8000-000000000002";

struct Lab {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    project: PathBuf,
    mask: libc::mode_t,
}

impl Lab {
    fn new(mask: libc::mode_t) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("native-home");
        let project = root.join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        Self {
            _temp: temp,
            state: root.join("state"),
            root,
            home,
            project,
            mask,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("AGIT_HOME", &self.state)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("AGIT_TELEMETRY_DISABLED", "1")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .current_dir(&self.project);
        let mask = self.mask;
        // Only the child changes umask; parallel tests retain their own process state.
        unsafe {
            command.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
        command
    }

    fn ok(&self, args: &[&str]) -> Output {
        success(self.command(args).output().unwrap())
    }

    fn prepare(&self) {
        self.ok(&["config", "commit.auto", "true"]);
        agit::infra::credentials::save_at(
            &self.state.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(HUB).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                account_id: None,
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
        self.ok(&["init", "qa", "--no-bind"]);
    }

    fn native(&self, id: &str) -> PathBuf {
        self.home
            .join(".codex/sessions/2026/09/17")
            .join(format!("rollout-2026-09-17T00-00-00-{id}.jsonl"))
    }

    fn append_turn(&self, id: &str, turn: u32) {
        let path = self.native(id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let fresh = !path.exists();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        if fresh {
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":self.project}})
            )
            .unwrap();
        }
        for (role, kind, text) in [
            ("user", "input_text", "SYNTHETIC-QUESTION"),
            ("assistant", "output_text", "SYNTHETIC-ANSWER"),
        ] {
            writeln!(file, "{}", serde_json::json!({"type":"response_item","payload":{"type":"message","role":role,"content":[{"type":kind,"text":format!("{text}-{turn}")}]}})).unwrap();
        }
    }

    fn import(&self, id: &str, branch: &str) {
        self.append_turn(id, 1);
        self.ok(&[
            "import",
            id,
            "--from",
            "codex",
            "--independent",
            "--into",
            &format!("me/qa@{branch}"),
        ]);
    }

    fn settle(&self, id: &str, branch: &str) -> Output {
        self.command(&["commit", "--from-supervisor"])
            .env("AGIT_SESSION", format!("me/qa@{branch}"))
            .env(
                "AGIT_SETTLEMENT_NATIVE",
                serde_json::json!({"runtime":"codex","session_id":id}).to_string(),
            )
            .env(
                "AGIT_RC_SUPERVISOR_COMMIT_RESULT",
                self.root.join("settled-sha"),
            )
            .output()
            .unwrap()
    }

    fn head(&self, branch: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.state.join("repos/me/qa"))
            .args(["rev-parse", &format!("refs/heads/{branch}")])
            .output()
            .unwrap();
        String::from_utf8(success(output).stdout)
            .unwrap()
            .trim()
            .into()
    }

    fn assert_saved(&self, id: &str, branch: &str) {
        let before = self.head(branch);
        success(self.settle(id, branch));
        let head = self.head(branch);
        assert_ne!(
            head, before,
            "settlement must save the newly completed turn"
        );
        assert_eq!(
            fs::read_to_string(self.root.join("settled-sha"))
                .unwrap()
                .trim(),
            head
        );
        let repo = agit::domain::repo::Repo::at(self.state.join("repos/me/qa"));
        let metadata = repo
            .git(&["show", &format!("{head}:session/meta.json")])
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(
            metadata["turn"], 2,
            "the saved version must include the completed turn"
        );
    }
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn sticky_shared_home_allows_watch_and_settlement_without_trusting_replaceable_ancestors() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let lab = Lab::new(0o002);
    fs::set_permissions(&lab.root, fs::Permissions::from_mode(0o3775)).unwrap();
    lab.prepare();
    lab.import(NATIVE, "work");
    lab.append_turn(NATIVE, 2);
    lab.assert_saved(NATIVE, "work");

    struct Stop<'a>(&'a Lab);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = self.0.command(&["rc", "local", "stop"]).output();
        }
    }
    let _stop = Stop(&lab);
    let bin = lab.root.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("codex"), "#!/bin/sh\n[ \"$1\" = --version ]\n").unwrap();
    fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    success(
        lab.command(&["rc", "local", "start", "--detach", "--json"])
            .env("PATH", path)
            .output()
            .unwrap(),
    );
    let mut bridge = tokio::process::Command::from(lab.command(&["rc", "local", "bridge"]))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = bridge.stdin.take().unwrap();
    let mut output = BufReader::new(bridge.stdout.take().unwrap()).lines();
    for (id, method, params) in [
        (
            1,
            "project.bind",
            serde_json::json!({"project_id":"project","local_path":lab.project}),
        ),
        (2, "session.watch", serde_json::json!({"session_id":NATIVE})),
        (3, "session.watch", serde_json::json!({"session_id":NATIVE})),
        (4, "session.watch", serde_json::json!({"session_id":NATIVE})),
    ] {
        if id == 3 {
            fs::set_permissions(&lab.root, fs::Permissions::from_mode(0o2775)).unwrap();
        }
        if id == 4 {
            fs::set_permissions(&lab.root, fs::Permissions::from_mode(0o3775)).unwrap();
            fs::set_permissions(
                lab.state.join("store/codex"),
                fs::Permissions::from_mode(0o777),
            )
            .unwrap();
        }
        let mut params = params;
        params["workspace_id"] = "local-owner".into();
        input
            .write_all(
                format!(
                    "{}\n",
                    serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let frame: serde_json::Value =
                    serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
                if frame["id"] == id {
                    break frame;
                }
            }
        })
        .await
        .unwrap();
        if id >= 3 {
            assert_eq!(response["error"]["code"], 305, "{response}");
        } else {
            assert!(response["error"].is_null(), "{response}");
        }
    }
    assert_eq!(
        fs::metadata(&lab.root).unwrap().permissions().mode() & 0o7777,
        0o3775
    );
}

#[test]
fn fresh_import_and_strict_settlement_are_umask_safe() {
    let lab = Lab::new(0o002);
    lab.prepare();
    lab.import(NATIVE, "work");
    lab.append_turn(NATIVE, 2);
    lab.assert_saved(NATIVE, "work");
    for relative in [
        "",
        "store",
        "store/codex",
        "store/.locks",
        "store/.locks/branches",
        "store/.locks/repositories",
        "repos",
        "repos/me",
        "repos/me/qa",
        "repos/me/qa/.git",
    ] {
        assert_eq!(mode(&lab.state.join(relative)) & 0o022, 0, "{relative}");
    }
    let link = lab.state.join(format!("store/codex/{NATIVE}.json"));
    assert_eq!(mode(&link) & 0o077, 0);
    for entry in walkdir::WalkDir::new(lab.state.join("store")) {
        let entry = entry.unwrap();
        if entry.path().extension().is_some_and(|ext| ext == "lock") {
            assert_eq!(mode(entry.path()) & 0o077, 0, "{}", entry.path().display());
        }
    }
}

fn image(root: &Path) -> std::collections::BTreeMap<PathBuf, (u32, u64, Vec<u8>)> {
    use std::os::unix::fs::MetadataExt;
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            (
                entry.path().to_path_buf(),
                (
                    metadata.mode(),
                    metadata.ino(),
                    if metadata.is_file() {
                        fs::read(entry.path()).unwrap()
                    } else {
                        Vec::new()
                    },
                ),
            )
        })
        .collect()
}

#[test]
fn bounded_legacy_repair_preserves_identity_and_allows_existing_and_new_turns() {
    let lab = Lab::new(0o002);
    lab.prepare();
    lab.import(NATIVE, "work");
    lab.append_turn(NATIVE, 2);
    let link = lab.state.join(format!("store/codex/{NATIVE}.json"));
    let lock = link.with_extension("json.lock");
    for relative in [
        "",
        "store",
        "store/codex",
        "repos",
        "repos/me",
        "repos/me/qa",
        "repos/me/qa/.git",
    ] {
        fs::set_permissions(lab.state.join(relative), fs::Permissions::from_mode(0o775)).unwrap();
    }
    for path in [&link, &lock] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o664)).unwrap();
    }
    let unrelated = lab.state.join("unrelated");
    fs::write(&unrelated, b"SYNTHETIC-UNRELATED").unwrap();
    fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o664)).unwrap();
    fs::set_permissions(&lab.project, fs::Permissions::from_mode(0o775)).unwrap();
    fs::set_permissions(&lab.home, fs::Permissions::from_mode(0o775)).unwrap();
    let native_before = image(&lab.home);
    let project_before = image(&lab.project);
    let head = lab.head("work");
    let refused = lab.settle(NATIVE, "work");
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("--repair-permissions") && error.contains("0775"),
        "{error}"
    );
    assert_eq!(lab.head("work"), head);
    let before = image(&lab.state);
    let targets = [&link, &lock, &lab.state.join("repos/me/qa/.git")];
    for target in targets {
        lab.ok(&["doctor", "--repair-permissions", target.to_str().unwrap()]);
    }
    let after = image(&lab.state);
    assert_eq!(before.len(), after.len());
    for (path, (old_mode, inode, bytes)) in &before {
        let (new_mode, new_inode, new_bytes) = &after[path];
        assert_eq!(new_inode, inode, "{}", path.display());
        assert_eq!(new_bytes, bytes, "{}", path.display());
        let selected = targets.iter().any(|target| target.starts_with(path));
        assert_eq!(
            *new_mode,
            if selected {
                *old_mode & !0o022
            } else {
                *old_mode
            },
            "{}",
            path.display()
        );
    }
    assert_eq!(
        mode(&link),
        0o644,
        "ordinary readable Links do not require private modes"
    );
    for target in targets {
        lab.ok(&["doctor", "--repair-permissions", target.to_str().unwrap()]);
    }
    assert_eq!(image(&lab.state), after, "repeated repair is harmless");
    assert_eq!(image(&lab.home), native_before);
    assert_eq!(image(&lab.project), project_before);
    lab.assert_saved(NATIVE, "work");
    lab.import(NEXT, "next");
    lab.append_turn(NEXT, 2);
    lab.assert_saved(NEXT, "next");
    assert_eq!(mode(&unrelated), 0o664);
}
