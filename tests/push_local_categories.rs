//! Local publication refusals preserve their category without reaching the Hub or moving refs.

use agit::domain::meta;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    hub: TcpListener,
    base: String,
    vault_backup: Option<Vec<u8>>,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        for name in ["work", "tmp", "templates"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let base = format!("http://{}", hub.local_addr().unwrap());
        let lab = Self {
            root,
            home,
            hub,
            base,
            vault_backup: None,
        };
        let output = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["config", "hub.url"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let credential = agit::infra::credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: Some(lab.base.clone()),
            access_token: "synthetic-publication-token".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "synthetic-publication-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        agit::infra::credentials::save_at(
            &lab.home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&lab.base).unwrap()
            )),
            &credential,
        )
        .unwrap();
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
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
            .env("AGIT_HUB_URL", &self.base)
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", self.root.path().join("templates"))
            .env("GIT_AUTHOR_NAME", "Publication category fixture")
            .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
            .env("GIT_COMMITTER_NAME", "Publication category fixture")
            .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(self.root.path().join("work"))
            .stdin(Stdio::null());
        command
    }

    fn git(&self, path: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn seed(&self, owner: &str, name: &str, metadata: bool) -> PathBuf {
        let path = self.home.join("repos").join(owner).join(name);
        fs::create_dir_all(&path).unwrap();
        self.git(&path, &["init", "--initial-branch=main"]);
        self.git(&path, &["config", "commit.gpgsign", "false"]);
        if metadata {
            meta::write(&path, &meta::Meta::new_file_line()).unwrap();
        } else {
            fs::write(path.join("AGENTS.md"), "Synthetic shared content.\n").unwrap();
        }
        self.git(&path, &["add", "."]);
        self.git(
            &path,
            &["commit", "-m", "Create synthetic publication history"],
        );
        path
    }

    fn push(&self, target: &str, mode: &str, supervised: bool) -> Output {
        self.push_command(target, mode, supervised)
            .output()
            .unwrap()
    }

    fn push_command(&self, target: &str, mode: &str, supervised: bool) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
        if mode == "quiet" {
            command.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            command.args(["--json", "--json-version", version]);
        }
        if supervised {
            command.env(agit::rc::harness::SUPERVISED_HOOK_ENV, "1");
        }
        command.args(["push", target, "--all", "--dry-run"]);
        command
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                let data = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    data,
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

impl Drop for Lab {
    fn drop(&mut self) {
        if let Some(bytes) = &self.vault_backup {
            let _ = fs::write(self.home.join("secret-filter/vault.json"), bytes);
        }
        #[cfg(windows)]
        {
            use agit::domain::secret_filter::KeyStore;
            if let Ok(bytes) = fs::read(self.home.join("secret-filter/vault.json"))
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && let Some(id) = value["vault_id"].as_str()
            {
                let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
            }
        }
    }
}

#[test]
fn secret_scan_preparation_keeps_configuration_policy_and_repair_categories() {
    use agit::domain::secret_filter::{SelectedKeyStore, VaultStore};
    use zeroize::Zeroizing;

    const SECRET: &str = "local-publication-scan-canary";
    for mode in ["human", "quiet", "json1", "json2"] {
        let mut lab = Lab::new();
        let repo = lab.seed("alice", "qa", true);
        let vault_path = lab.home.join("secret-filter/vault.json");
        #[cfg(unix)]
        let keys = SelectedKeyStore::File(agit::domain::secret_filter::FileKeyStore::new(
            lab.home.join("keystore"),
        ));
        #[cfg(not(unix))]
        let keys = SelectedKeyStore::Os(agit::domain::secret_filter::OsKeyStore);
        VaultStore::new(vault_path.clone(), keys)
            .add("publication-canary", Zeroizing::new(SECRET.into()), false)
            .unwrap();
        let vault_bytes = fs::read(&vault_path).unwrap();
        lab.vault_backup = Some(vault_bytes.clone());
        let warm = lab.push("alice/qa", mode, false);
        assert_eq!(warm.status.code(), Some(0), "{warm:?}");
        let before = lab.state();
        let refs = lab.git(&repo, &["show-ref"]);
        lab.no_requests();

        fs::write(&vault_path, b"{").unwrap();
        let blocked = lab.state();
        for allow_secrets in [false, true] {
            let mut command = lab.push_command("alice/qa", mode, false);
            if allow_secrets {
                command.env("AGIT_ALLOW_SECRETS", "1");
            }
            let output = command.output().unwrap();
            assert_output(&output, mode, 4, "is malformed");
            assert_eq!(lab.state(), blocked);
            assert_eq!(lab.git(&repo, &["show-ref"]), refs);
            lab.no_requests();
        }
        let invalid_config = lab
            .push_command("alice/qa", mode, false)
            .env("AGIT_SECRETS_KEYSTORE", "invalid-keystore")
            .output()
            .unwrap();
        assert_output(&invalid_config, mode, 2, "takes `os` or `file`");
        assert_eq!(lab.state(), blocked);
        lab.no_requests();

        fs::write(&vault_path, &vault_bytes).unwrap();
        let repaired = lab.push("alice/qa", mode, false);
        assert_eq!(repaired.status.code(), Some(0), "{repaired:?}");
        assert_eq!(lab.state(), before);
        assert_eq!(lab.git(&repo, &["show-ref"]), refs);
        lab.no_requests();

        fs::write(repo.join("AGENTS.md"), format!("Secret: {SECRET}\n")).unwrap();
        let policy_state = lab.state();
        let policy = lab.push("alice/qa", mode, false);
        assert_output(&policy, mode, 7, "publish blocked");
        assert!(!String::from_utf8_lossy(&policy.stdout).contains(SECRET));
        assert!(!String::from_utf8_lossy(&policy.stderr).contains(SECRET));
        assert_eq!(lab.state(), policy_state);
        let allowed = lab
            .push_command("alice/qa", mode, false)
            .env("AGIT_ALLOW_SECRETS", "1")
            .output()
            .unwrap();
        assert_eq!(allowed.status.code(), Some(0), "{allowed:?}");
        assert_eq!(lab.state(), policy_state);
        assert_eq!(lab.git(&repo, &["show-ref"]), refs);
        lab.no_requests();
    }
}

fn assert_output(output: &Output, mode: &str, code: i32, diagnostic: &str) {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    let text = if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(value["command"], "push");
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], code == 0);
        if version == "1" {
            assert!(value.get("fix").is_none());
        } else {
            assert!(value["fix"].is_array());
        }
        value["diagnostics"]["stderr"].to_string()
    } else {
        String::from_utf8(output.stderr.clone()).unwrap()
    };
    assert!(text.contains(diagnostic), "{text}");
    assert!(!text.contains("synthetic-publication-token"));
}

#[test]
fn local_push_refusals_keep_identity_credentials_and_repository_bytes() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let left = lab.seed("alice", "qa", true);
        let right = lab.seed("other", "qa", true);
        let broken = lab.seed("alice", "broken", false);
        fs::create_dir_all(lab.home.join("repos/alice/not-a-repo")).unwrap();
        let before = lab.state();
        let left_refs = lab.git(&left, &["show-ref"]);
        let right_refs = lab.git(&right, &["show-ref"]);
        let broken_refs = lab.git(&broken, &["show-ref"]);
        for (target, supervised, code, message) in [
            ("alice/absent", false, 3, "no local repo"),
            ("alice/not-a-repo", false, 4, "no local repo"),
            ("alice/broken", false, 4, "no readable session metadata"),
            ("qa", false, 8, "name it in full"),
            ("alice/qa", true, 4, "let agitd push"),
            ("alice/qa@main", false, 2, "cannot be combined"),
        ] {
            assert_output(&lab.push(target, mode, supervised), mode, code, message);
            assert_eq!(lab.state(), before, "{mode}: {target}");
            assert_eq!(lab.git(&left, &["show-ref"]), left_refs);
            assert_eq!(lab.git(&right, &["show-ref"]), right_refs);
            assert_eq!(lab.git(&broken, &["show-ref"]), broken_refs);
            lab.no_requests();
        }
        let selected = lab.push("alice/qa", mode, false);
        assert_eq!(selected.status.code(), Some(0), "{selected:?}");
        assert_eq!(lab.git(&left, &["show-ref"]), left_refs);
        assert_eq!(lab.git(&right, &["show-ref"]), right_refs);
        lab.no_requests();
        let logged_out = Lab::new();
        let credential = logged_out.home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&logged_out.base).unwrap()
        ));
        fs::remove_file(credential).unwrap();
        let before = logged_out.state();
        assert_output(
            &logged_out.push("alice/absent", mode, false),
            mode,
            5,
            "not logged in",
        );
        assert_eq!(logged_out.state(), before);
        logged_out.no_requests();
    }
}
