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
        lab.success(&["import", ID, "--from", "codex", "--into", "me/qa@work"]);
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
