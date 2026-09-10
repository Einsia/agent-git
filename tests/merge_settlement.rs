//! Merge admission preserves every local writer whose evidence is not fully settled.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

const ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const HUB: &str = "http://127.0.0.1:1";

struct Lab {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    native: PathBuf,
}

fn message(role: &str, text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{
            "type":"message","role":role,"content":[{"type":if role == "user" {"input_text"} else {"output_text"},"text":text}]
        }})
    )
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let store = temporary.path().join("agit");
        let work = temporary.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let native = home
            .join(".codex/sessions/2026/09/08")
            .join(format!("rollout-2026-09-08T00-00-00-{ID}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        fs::write(
            &native,
            format!(
                "{}\n{}{}",
                serde_json::json!({"type":"session_meta","payload":{"id":ID,"cwd":work}}),
                message("user", "SYNTHETIC-SETTLED"),
                message("assistant", "SYNTHETIC-REPLY")
            ),
        )
        .unwrap();
        let lab = Self {
            _temporary: temporary,
            home,
            store,
            work,
            native,
        };
        agit::infra::credentials::save_at(
            &lab.credential(),
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
        lab.success(&[
            "import",
            ID,
            "--from",
            "codex",
            "--into",
            "me/qa@work",
            "--independent",
        ]);
        let head = lab.git(&["rev-parse", "refs/heads/work"]);
        lab.git(&["branch", "source", &head]);
        lab
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", HUB)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_YES", "1");
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
        let output = self.command().args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        output
    }

    fn repo(&self) -> PathBuf {
        self.store.join("repos/me/qa")
    }
    fn credential(&self) -> PathBuf {
        self.store.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(HUB).unwrap()
        ))
    }
    fn link(&self) -> PathBuf {
        self.store.join("store/codex").join(format!("{ID}.json"))
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(args)
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn merge(&self) -> Output {
        self.command()
            .args(["merge", "me/qa@source", "--into", "me/qa@work", "--manual"])
            .output()
            .unwrap()
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        for path in [
            self.store.join("secret-filter/vault.json"),
            self.repo().join(".git/agit/secret-dictionary/vault.json"),
        ] {
            if let Ok(bytes) = fs::read(path)
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(id) = value.get("vault_id").and_then(serde_json::Value::as_str)
            {
                let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
            }
        }
    }
}

#[test]
fn incomplete_or_unverifiable_claims_refuse_before_opening_a_merge() {
    for change in [
        "no-auth",
        "unanswered",
        "open-tool",
        "partial",
        "invalid-utf8",
        "truncated",
        "rewritten",
        "missing",
        "materialized-tail",
        "missing-baseline-hash",
    ] {
        let lab = Lab::new();
        let settled = fs::read(&lab.native).unwrap();
        match change {
            "no-auth" => {
                fs::remove_file(lab.credential()).unwrap();
                fs::write(
                    &lab.native,
                    format!(
                        "{}{}{}",
                        String::from_utf8(settled.clone()).unwrap(),
                        message("user", "SYNTHETIC-PENDING"),
                        message("assistant", "SYNTHETIC-PENDING-REPLY")
                    ),
                )
                .unwrap();
            }
            "unanswered" => {
                fs::write(
                    &lab.native,
                    format!(
                        "{}{}",
                        String::from_utf8(settled.clone()).unwrap(),
                        message("user", "SYNTHETIC-PENDING")
                    ),
                )
                .unwrap();
            }
            "open-tool" => {
                let call = serde_json::json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"synthetic-call","arguments":"{}"}});
                fs::write(
                    &lab.native,
                    format!(
                        "{}{}{call}\n",
                        String::from_utf8(settled.clone()).unwrap(),
                        message("user", "SYNTHETIC-PENDING")
                    ),
                )
                .unwrap();
            }
            "partial" => {
                let mut bytes = settled.clone();
                bytes.extend_from_slice(b"{\"type\":");
                fs::write(&lab.native, bytes).unwrap();
            }
            "invalid-utf8" => {
                let mut bytes = settled.clone();
                bytes.push(0xff);
                fs::write(&lab.native, bytes).unwrap();
            }
            "truncated" => {
                fs::write(&lab.native, &settled[..settled.len() / 2]).unwrap();
            }
            "rewritten" => {
                fs::write(
                    &lab.native,
                    String::from_utf8(settled.clone())
                        .unwrap()
                        .replace("SYNTHETIC-SETTLED", "SYNTHETIC-REWRITTEN"),
                )
                .unwrap();
            }
            "missing" => {
                fs::remove_file(&lab.native).unwrap();
            }
            "materialized-tail" | "missing-baseline-hash" => {
                use sha2::Digest as _;
                let mut link: serde_json::Value =
                    serde_json::from_slice(&fs::read(lab.link()).unwrap()).unwrap();
                link["baseline_bytes"] = serde_json::json!(settled.len());
                if change == "materialized-tail" {
                    link["baseline_hash"] =
                        serde_json::json!(hex::encode(sha2::Sha256::digest(&settled)));
                    fs::write(
                        &lab.native,
                        format!(
                            "{}{}",
                            String::from_utf8(settled.clone()).unwrap(),
                            message("user", "SYNTHETIC-PENDING")
                        ),
                    )
                    .unwrap();
                }
                fs::write(lab.link(), serde_json::to_vec(&link).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        let before = lab.git(&["rev-parse", "refs/heads/work"]);
        let link = fs::read(lab.link()).unwrap();
        let native = fs::read(&lab.native).ok();
        let out = lab.merge();
        assert!(!out.status.success(), "{change}: {out:?}");
        assert!(!lab.repo().join(".git/AGIT_MERGE_TX").exists(), "{change}");
        assert_eq!(
            lab.git(&["rev-parse", "refs/heads/work"]),
            before,
            "{change}"
        );
        assert_eq!(fs::read(lab.link()).unwrap(), link, "{change}");
        assert_eq!(fs::read(&lab.native).ok(), native, "{change}");
    }
}

#[test]
fn settled_claims_and_claimless_manual_merges_do_not_require_credentials() {
    for kind in ["native", "materialized", "claimless"] {
        let lab = Lab::new();
        fs::remove_file(lab.credential()).unwrap();
        match kind {
            "materialized" => {
                use sha2::Digest as _;
                let bytes = fs::read(&lab.native).unwrap();
                let mut link: serde_json::Value =
                    serde_json::from_slice(&fs::read(lab.link()).unwrap()).unwrap();
                link["baseline_bytes"] = serde_json::json!(bytes.len());
                link["baseline_hash"] =
                    serde_json::json!(hex::encode(sha2::Sha256::digest(&bytes)));
                fs::write(lab.link(), serde_json::to_vec(&link).unwrap()).unwrap();
            }
            "claimless" => {
                fs::remove_file(lab.link()).unwrap();
            }
            _ => {}
        }
        let head = lab.git(&["rev-parse", "refs/heads/work"]);
        let out = lab.merge();
        assert!(out.status.success(), "{kind}: {out:?}");
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
        assert!(lab.repo().join(".git/AGIT_MERGE_TX").exists());
    }
}

#[test]
fn complete_pending_content_is_settled_before_manual_merge() {
    let lab = Lab::new();
    let before = lab.git(&["rev-parse", "refs/heads/work"]);
    let content = format!(
        "{}{}{}",
        fs::read_to_string(&lab.native).unwrap(),
        message("user", "SYNTHETIC-PENDING"),
        message("assistant", "SYNTHETIC-PENDING-REPLY")
    );
    fs::write(&lab.native, content).unwrap();
    let out = lab.merge();
    assert!(out.status.success(), "{out:?}");
    assert_ne!(lab.git(&["rev-parse", "refs/heads/work"]), before);
    let view = lab.success(&["show", "me/qa@work", "--raw"]);
    assert!(String::from_utf8_lossy(&view.stdout).contains("SYNTHETIC-PENDING"));
}

#[test]
fn native_settlement_proof_hydrates_secrets_without_changing_the_dictionary() {
    use std::io::Write as _;
    use std::process::Stdio;

    for dictionary_present in [true, false] {
        let lab = Lab::new();
        let mut register = lab
            .command()
            .args(["secrets", "add", "synthetic", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        register
            .stdin
            .take()
            .unwrap()
            .write_all(b"SYNTHETIC-PRIVATE-VALUE\n")
            .unwrap();
        assert!(register.wait().unwrap().success());
        let native = format!(
            "{}{}{}",
            fs::read_to_string(&lab.native).unwrap(),
            message("user", "SYNTHETIC-PRIVATE-VALUE"),
            message("assistant", "SYNTHETIC-SECRET-REPLY")
        );
        fs::write(&lab.native, &native).unwrap();
        lab.success(&["commit", "me/qa@work"]);
        fs::remove_file(lab.credential()).unwrap();
        let dictionary = lab.repo().join(".git/agit/secret-dictionary/vault.json");
        let before = fs::read(&dictionary).unwrap();
        if !dictionary_present {
            fs::remove_file(&dictionary).unwrap();
        }
        let head = lab.git(&["rev-parse", "refs/heads/work"]);
        let out = lab.merge();
        assert_eq!(out.status.success(), dictionary_present, "{out:?}");
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
        assert_eq!(fs::read_to_string(&lab.native).unwrap(), native);
        if dictionary_present {
            assert_eq!(fs::read(&dictionary).unwrap(), before);
        } else {
            assert!(!dictionary.exists());
            assert!(!lab.repo().join(".git/AGIT_MERGE_TX").exists());
            fs::write(dictionary, before).unwrap();
        }
    }
}

#[test]
#[ignore]
fn transaction_control_child_probe() {
    let ready = std::env::var_os("AGIT_TEST_CONTROL_READY").expect("probe readiness path");
    let release =
        PathBuf::from(std::env::var_os("AGIT_TEST_CONTROL_RELEASE").expect("probe release path"));
    fs::write(ready, "ready").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !release.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the parent did not release its child probe"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn transaction_control_is_not_inherited_by_a_running_child() {
    use agit::domain::mergetx::ControlGuard;
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    let ready = directory.path().join("ready");
    let release = directory.path().join("release");
    let guard = ControlGuard::acquire(directory.path()).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", "transaction_control_child_probe"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AGIT_TEST_CONTROL_READY", &ready)
        .env("AGIT_TEST_CONTROL_RELEASE", &release)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child probe did not become ready");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(guard);
    let (sender, receiver) = std::sync::mpsc::channel();
    let root = directory.path().to_owned();
    let contender = std::thread::spawn(move || {
        let _guard = ControlGuard::acquire(&root).unwrap();
        sender.send(()).unwrap();
    });
    let acquired = receiver.recv_timeout(Duration::from_secs(5)).is_ok();
    let child_still_running = child.try_wait().unwrap().is_none();
    fs::write(release, "release").unwrap();
    assert!(child.wait().unwrap().success());
    contender.join().unwrap();
    assert!(
        acquired,
        "the runtime retained the parent's transaction control lock"
    );
    assert!(
        child_still_running,
        "the lock became available only after the child exited"
    );
}

#[cfg(unix)]
#[test]
fn an_aborted_merge_is_not_revived_after_summary_input_finishes() {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::time::{Duration, Instant};

    for replace_transaction in [false, true] {
        let lab = Lab::new();
        let opened = lab.merge();
        assert!(opened.status.success(), "{opened:?}");
        let fifo = lab.work.join("summary.fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let mut summary = lab
            .command()
            .args(["merge", "--into", "me/qa@work", "summary", "-F"])
            .arg(&fifo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut input = loop {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(file) => break file,
                Err(error)
                    if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = summary.kill();
                    let _ = summary.wait();
                    panic!("summary input did not open: {error}");
                }
            }
        };
        let mut abort = lab
            .command()
            .args(["merge", "--abort", "--into", "me/qa@work"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let aborted = loop {
            if let Some(status) = abort.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let replacement = if replace_transaction && aborted.is_some_and(|status| status.success()) {
            let reopened = lab.merge();
            assert!(reopened.status.success(), "{reopened:?}");
            Some(fs::read(lab.repo().join(".git/AGIT_MERGE_TX")).unwrap())
        } else {
            None
        };
        input.write_all(b"synthetic completed summary").unwrap();
        drop(input);
        let summary_status = summary.wait().unwrap();
        let abort_status = abort.wait().unwrap();
        assert!(
            aborted.is_some_and(|status| status.success()),
            "abort waited for unrelated input"
        );
        assert!(abort_status.success());
        assert!(
            !summary_status.success(),
            "a delayed summary revived the aborted transaction"
        );
        if let Some(replacement) = replacement {
            assert_eq!(
                fs::read(lab.repo().join(".git/AGIT_MERGE_TX")).unwrap(),
                replacement
            );
        } else {
            assert!(!lab.repo().join(".git/AGIT_MERGE_TX").exists());
        }
    }
}

#[test]
fn transaction_status_does_not_create_mutation_control() {
    let lab = Lab::new();
    let control = lab.repo().join(".git/AGIT_MERGE_TX.control");
    assert!(!control.exists());
    let absent = lab
        .command()
        .args(["merge", "--status", "--into", "me/qa@work"])
        .output()
        .unwrap();
    assert!(!absent.status.success());
    assert!(!control.exists());
    assert!(lab.merge().status.success());
    fs::remove_file(&control).unwrap();
    let transaction_path = lab.repo().join(".git/AGIT_MERGE_TX");
    let before = fs::read(&transaction_path).unwrap();
    lab.success(&["merge", "--status", "--into", "me/qa@work"]);
    assert!(!control.exists());
    assert_eq!(fs::read(transaction_path).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn settlement_recovery_hint_preserves_the_literal_branch_argument() {
    use std::os::unix::fs::PermissionsExt as _;

    for branch in ["work;false", "work'quote‘’‚‛"] {
        let lab = Lab::new();
        lab.git(&["branch", "-m", "work", branch]);
        let mut link: serde_json::Value =
            serde_json::from_slice(&fs::read(lab.link()).unwrap()).unwrap();
        link["branch"] = serde_json::json!(branch);
        fs::write(lab.link(), serde_json::to_vec(&link).unwrap()).unwrap();
        fs::remove_file(lab.credential()).unwrap();
        fs::write(
            &lab.native,
            format!(
                "{}{}{}",
                fs::read_to_string(&lab.native).unwrap(),
                message("user", "SYNTHETIC-PENDING"),
                message("assistant", "SYNTHETIC-PENDING-REPLY")
            ),
        )
        .unwrap();
        let target = format!("me/qa@{branch}");
        let output = lab
            .command()
            .args(["merge", "me/qa@source", "--into", &target, "--manual"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        let recovery = diagnostic
            .split('`')
            .find(|part| part.starts_with("agit commit "))
            .unwrap();
        let bin = lab.work.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("agit");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$AGIT_TEST_RECOVERY_ARGS\"\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let capture = lab.work.join("recovery-arguments");
        let result = Command::new("/bin/sh")
            .args(["-c", recovery])
            .current_dir(&lab.work)
            .env_clear()
            .env("PATH", &bin)
            .env("AGIT_TEST_RECOVERY_ARGS", &capture)
            .output()
            .unwrap();
        assert!(result.status.success(), "{recovery}: {result:?}");
        assert_eq!(
            fs::read(capture).unwrap(),
            format!("commit\0{target}\0").as_bytes()
        );
    }
}

/// A canceled runtime must not borrow authority from a replacement transaction on the same target.
#[test]
fn merge_agent_generation_cannot_modify_a_replacement_transaction() {
    let lab = Lab::new();
    assert!(lab.merge().status.success());
    let transaction_path = lab.repo().join(".git/AGIT_MERGE_TX");
    let original: serde_json::Value =
        serde_json::from_slice(&fs::read(&transaction_path).unwrap()).unwrap();
    let previous_generation = original["generation"].as_str().unwrap();
    lab.success(&["merge", "--abort", "--into", "me/qa@work"]);
    assert!(lab.merge().status.success());
    let before = fs::read(&transaction_path).unwrap();
    let current: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_ne!(current["generation"].as_str().unwrap(), previous_generation);
    let head = lab.git(&["rev-parse", "refs/heads/work"]);
    for arguments in [
        vec!["merge", "--into", "me/qa@work", "pick", "me/qa@source#1"],
        vec!["merge", "--into", "me/qa@work", "drop", "me/qa@source#1"],
        vec![
            "merge",
            "--into",
            "me/qa@work",
            "summary",
            "-m",
            "obsolete runtime intent",
        ],
        vec!["merge", "--into", "me/qa@work", "--continue"],
        vec!["merge", "--into", "me/qa@work", "--abort"],
    ] {
        let output = lab
            .command()
            .args(&arguments)
            .env("AGIT_MERGE_TX", "me/qa@work")
            .env("AGIT_MERGE_GENERATION", previous_generation)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{arguments:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("generation"));
        assert_eq!(fs::read(&transaction_path).unwrap(), before);
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
    }
    let valid = lab
        .command()
        .args([
            "merge",
            "--into",
            "me/qa@work",
            "summary",
            "-m",
            "current runtime intent",
        ])
        .env("AGIT_MERGE_TX", "me/qa@work")
        .env(
            "AGIT_MERGE_GENERATION",
            current["generation"].as_str().unwrap(),
        )
        .output()
        .unwrap();
    assert!(valid.status.success(), "{valid:?}");
    let recorded: serde_json::Value =
        serde_json::from_slice(&fs::read(&transaction_path).unwrap()).unwrap();
    assert_eq!(recorded["summary"], "current runtime intent");
    lab.success(&["merge", "--abort", "--into", "me/qa@work"]);
}

#[cfg(unix)]
struct ArchiveTerminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    _writer: Box<dyn std::io::Write + Send>,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
}

#[cfg(unix)]
impl ArchiveTerminal {
    fn start(lab: &Lab, exit: u8) -> Self {
        Self::start_with_runtime(lab, exit, true)
    }

    fn start_with_runtime(lab: &Lab, exit: u8, available: bool) -> Self {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let bin = lab.home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        if available {
            let shim = bin.join("codex");
            fs::write(&shim, "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$AGIT_SESSION\" \"$AGIT_MERGE_TX\" \"$AGIT_MERGE_GENERATION\" \"$*\" > \"$ARCHIVE_LAUNCH_RECEIPT\"\nexit \"$ARCHIVE_EXIT_CODE\"\n").unwrap();
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut path = vec![bin.clone()];
        if available {
            path.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
        } else {
            for tool in ["git", "sh"] {
                std::os::unix::fs::symlink(agit::adapter::which(tool).unwrap(), bin.join(tool))
                    .unwrap();
            }
        }
        let template = lab.command();
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (key, value) in template.get_envs() {
            if key != "CI"
                && let Some(value) = value
            {
                command.env(key, value);
            }
        }
        command.env("PATH", std::env::join_paths(path).unwrap());
        command.env("ARCHIVE_EXIT_CODE", exit.to_string());
        command.env("ARCHIVE_LAUNCH_RECEIPT", lab.home.join("launch-receipt"));
        command.env("TERM", "xterm-256color");
        command.args([
            "merge",
            "me/qa@source",
            "--into",
            "me/qa@work",
            "--as",
            "codex",
        ]);
        command.cwd(&lab.work);
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let (send, output) = std::sync::mpsc::channel();
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
            _writer: writer,
            output,
        }
    }

    fn finish(mut self) -> (u32, String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut output = Vec::new();
        loop {
            if let Ok(bytes) = self
                .output
                .recv_timeout(std::time::Duration::from_millis(10))
            {
                output.extend_from_slice(&bytes);
                assert!(
                    output.len() <= 1024 * 1024,
                    "controlled runtime output exceeded its bound"
                );
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                while let Ok(bytes) = self.output.try_recv() {
                    output.extend_from_slice(&bytes);
                }
                return (
                    status.exit_code(),
                    String::from_utf8_lossy(&output).into_owned(),
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{}",
                String::from_utf8_lossy(&output)
            );
        }
    }
}

#[cfg(unix)]
impl Drop for ArchiveTerminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The old complete turn is saved before Tx freezes; only the installed instance receives authority.
#[test]
#[cfg(unix)]
fn session_agent_start_settles_old_content_then_activates_archive_before_spawn() {
    use agit::domain::{link, merge_archive, mergetx, meta, repo::Repo, storage, store::Store};
    for pending in [false, true] {
        let lab = Lab::new();
        let original = lab.git(&["rev-parse", "refs/heads/work"]);
        let source = lab.git(&["rev-parse", "refs/heads/source"]);
        if pending {
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .open(&lab.native)
                .unwrap()
                .write_all(
                    format!(
                        "{}{}",
                        message("user", "SYNTHETIC-BEFORE-ARCHIVE"),
                        message("assistant", "SYNTHETIC-COMPLETE-REPLY")
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
        let native = fs::read(&lab.native).unwrap();
        let (status, output) = ArchiveTerminal::start(&lab, 0).finish();
        assert_eq!(status, 4, "{output}");
        assert!(
            output.contains(
                "archive merge child exited before the merge landed; the transaction remains open"
            ),
            "{output}"
        );
        let repo = Repo::open(lab.repo()).unwrap();
        let tx = mergetx::read(repo.root()).unwrap().unwrap();
        assert_eq!(tx.mode, Some(mergetx::Mode::SessionAgent));
        assert!(tx.summary.is_none());
        assert_eq!(tx.source_head, source);
        assert_eq!(tx.target_head, lab.git(&["rev-parse", "refs/heads/work"]));
        assert_eq!(tx.target_head != original, pending);
        let log = storage::materialize_at(repo.root(), &tx.target_head, meta::LOG_FILE).unwrap();
        assert_eq!(log.contains("SYNTHETIC-BEFORE-ARCHIVE"), pending);
        let binding = tx.exploration.as_ref().unwrap();
        assert_eq!(binding.role.origin_head, tx.target_head);
        assert_eq!(Some(&binding.role.generation), tx.generation.as_ref());
        assert_ne!(binding.native.session_id, ID);
        let journal = merge_archive::read(repo.root(), &binding.role.generation)
            .unwrap()
            .unwrap();
        assert_eq!(journal.phase, merge_archive::ArchivePhase::Open);
        assert!(journal.activation.is_none());
        assert_eq!(journal.consumed, binding.installed);
        assert_eq!(journal.previous_claims.len(), 1);
        let store = Store::at(lab.store.join("store"));
        let successor =
            link::read_archive_link_snapshot(&store, "codex", &binding.native.session_id)
                .unwrap()
                .unwrap();
        assert!(
            successor
                .link
                .is_archive_for(&binding.role, "codex", &binding.native.session_id)
        );
        assert_eq!(successor.link.baseline_bytes, Some(binding.installed.bytes));
        let previous = link::read_archive_link_snapshot(&store, "codex", ID)
            .unwrap()
            .unwrap();
        assert_eq!(previous.json, journal.previous_claims[0].retired_json);
        assert_eq!(fs::read(&lab.native).unwrap(), native);
        let receipt = fs::read_to_string(lab.home.join("launch-receipt")).unwrap();
        let rows: Vec<_> = receipt.lines().collect();
        assert_eq!(
            &rows[..3],
            &["me/qa@work", "me/qa@work", binding.role.generation.as_str()]
        );
        assert!(rows[3..].join("\n").contains(&binding.native.session_id));
        assert!(rows[3..].join("\n").contains("merge"));
        lab.success(&["merge", "--abort", "--into", "me/qa@work"]);
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), tx.target_head);
        assert_eq!(
            fs::read_to_string(lab.link()).unwrap(),
            journal.previous_claims[0].original_json
        );
    }
}

/// A failed runtime keeps the same generation cancellable without summary or a surviving source ref.
#[test]
#[cfg(unix)]
fn failed_archive_runtime_keeps_installed_authority_for_explicit_abort() {
    use agit::domain::{merge_archive, mergetx, repo::Repo};
    let lab = Lab::new();
    let head = lab.git(&["rev-parse", "refs/heads/work"]);
    let (status, output) = ArchiveTerminal::start(&lab, 7).finish();
    assert_eq!(status, 4, "{output}");
    let repo = Repo::open(lab.repo()).unwrap();
    let tx = mergetx::read(repo.root()).unwrap().unwrap();
    let binding = tx.exploration.as_ref().unwrap();
    let old_link = merge_archive::read(repo.root(), &binding.role.generation)
        .unwrap()
        .unwrap()
        .previous_claims[0]
        .original_json
        .clone();
    let before = fs::read(repo.git_path(mergetx::LOCK_FILE).unwrap()).unwrap();
    let repeated = lab
        .command()
        .args(["merge", "me/qa@source", "--into", "me/qa@work"])
        .output()
        .unwrap();
    assert_eq!(repeated.status.code(), Some(4), "{repeated:?}");
    assert_eq!(
        fs::read(repo.git_path(mergetx::LOCK_FILE).unwrap()).unwrap(),
        before
    );
    assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
    lab.git(&["update-ref", "-d", "refs/heads/source"]);
    lab.success(&["merge", "--abort", "--into", "me/qa@work"]);
    assert!(mergetx::read(repo.root()).unwrap().is_none());
    assert_eq!(
        merge_archive::read(repo.root(), &binding.role.generation)
            .unwrap()
            .unwrap()
            .phase,
        merge_archive::ArchivePhase::Aborted
    );
    assert_eq!(fs::read_to_string(lab.link()).unwrap(), old_link);
    assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
}

/// Runtime absence and unverifiable old records cannot consume the old claim or create a Tx.
#[test]
#[cfg(unix)]
fn archive_start_preflight_refusals_preserve_old_pending_authority() {
    use agit::domain::{mergetx, repo::Repo};
    for change in ["unavailable", "in-flight", "malformed"] {
        let lab = Lab::new();
        let original = fs::read(&lab.native).unwrap();
        let suffix = match change {
            "unavailable" => format!(
                "{}{}",
                message("user", "SYNTHETIC-PENDING"),
                message("assistant", "SYNTHETIC-DONE")
            ),
            "in-flight" => message("user", "SYNTHETIC-PENDING"),
            _ => "{\"incomplete\":".into(),
        };
        fs::write(&lab.native, [original, suffix.into_bytes()].concat()).unwrap();
        let native = fs::read(&lab.native).unwrap();
        let link = fs::read(lab.link()).unwrap();
        let head = lab.git(&["rev-parse", "refs/heads/work"]);
        let (status, output) =
            ArchiveTerminal::start_with_runtime(&lab, 0, change != "unavailable").finish();
        assert_ne!(status, 0, "{change}: {output}");
        assert!(
            output.contains(if change == "unavailable" {
                "not on PATH"
            } else if change == "malformed" {
                "malformed"
            } else {
                "unsettled"
            }),
            "{change}: {output}"
        );
        assert!(
            mergetx::read(Repo::open(lab.repo()).unwrap().root())
                .unwrap()
                .is_none()
        );
        assert!(!lab.home.join("launch-receipt").exists());
        assert_eq!(lab.git(&["rev-parse", "refs/heads/work"]), head);
        assert_eq!(fs::read(&lab.native).unwrap(), native);
        assert_eq!(fs::read(lab.link()).unwrap(), link);
        assert_eq!(
            fs::read_dir(lab.store.join("store/codex"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json"))
                .count(),
            1
        );
    }
}
