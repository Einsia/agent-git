//! Local publication refusals preserve their category without publishing content or moving refs.

#[path = "support/lab_env.rs"]
mod lab_env;

use lab_env::LabEnv;

#[path = "support/publication_http.rs"]
mod publication_http;

#[path = "support/startup_cache.rs"]
mod startup_cache;

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
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        startup_cache::seed(&home);
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
        };
        let output = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["config", "hub.url"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let credential = agit::infra::credentials::HubCredential {
            account_id: None,
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
        command.env_clear_local();
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
            .env("AGIT_SKIP_SETUP", "1")
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
        self.push_output(&mut self.push_command(target, mode, supervised))
    }

    fn push_output(&self, command: &mut Command) -> Output {
        publication_http::with_missing_agent_probe(&self.hub, || command.output().unwrap())
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
        assert_output(
            &selected,
            mode,
            1,
            "new destination needs an immutable repository identity",
        );
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

        // An unreadable native header must refuse publication without claiming a parent denied it.
        let refused = lab.seed("alice", "refused", true);
        lab.git(&refused, &["checkout", "-b", "work"]);
        let native = "aaaaaaaa-0000-4000-8000-000000000001";
        let session = format!("agit-{}", "a".repeat(40));
        let header = serde_json::json!({"type":"session_meta","payload":{"id":native,"cwd":lab.root.path().join("work")}});
        let log = agit::domain::transcript::wrap_lines(&format!("{header}\n"), "codex", &session);
        agit::domain::storage::write_snapshot(&refused, &log, &log).unwrap();
        meta::write(
            &refused,
            &meta::Meta::new(session, "codex".into(), String::new()),
        )
        .unwrap();
        lab.git(&refused, &["add", "."]);
        lab.git(&refused, &["commit", "-m", "Save synthetic session"]);
        let store = agit::domain::store::Store::at(lab.home.join("store"));
        let mut claim = agit::domain::link::Link::new("codex", native, None);
        claim.owner = Some("alice".into());
        claim.agent = Some("refused".into());
        claim.branch = Some("work".into());
        claim.claimed_by = Some(agit::domain::link::launch::CONFIRMED.into());
        agit::domain::link::write(&store, &claim).unwrap();
        let directory = lab.root.path().join(".codex/sessions/2026/11/01");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(format!("rollout-2026-11-01T00-00-00-{native}.jsonl")),
            "{\n",
        )
        .unwrap();
        let before = lab.state();
        let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
        let diagnose = |verdict, reason| {
            let result = lab
                .command(env!("CARGO_BIN_EXE_agit"))
                .args(["project", "diagnose-source", "alice/refused@work"])
                .output()
                .unwrap();
            assert!(
                result.status.success() && result.stderr.is_empty(),
                "{result:?}"
            );
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
                serde_json::json!({"verdict":verdict, "reason":reason})
            );
        };
        if mode == "human" {
            diagnose("unverified", "source_or_parent_unavailable");
        }
        if mode == "quiet" {
            command.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            command.args(["--json", "--json-version", version]);
        }
        command.args(["push", "alice/refused@work"]);
        let output = lab.push_output(&mut command);
        assert_output(&output, mode, 7, "could not verify this session");
        assert_output(&output, mode, 7, "retry `agit push alice/refused@work`");
        assert_output(
            &output,
            mode,
            7,
            "no command to reconstruct missing source facts",
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("it is a delegated session"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("it is a delegated session"));
        assert_eq!(lab.state(), before, "{mode}: unreadable session");
        lab.no_requests();

        // Undoing a restorable parent drop clears the inherited hold, not other publication gates.
        let parent = "bbbbbbbb-0000-4000-8000-000000000002";
        let cwd = lab.root.path().join("work");
        let parent_header =
            serde_json::json!({"type":"session_meta","payload":{"id":parent,"cwd":cwd}});
        let parent_session = format!("agit-{}", "b".repeat(40));
        let parent_log = agit::domain::transcript::wrap_lines(
            &format!("{parent_header}\n"),
            "codex",
            &parent_session,
        );
        lab.git(&refused, &["checkout", "-b", "parent", "main"]);
        agit::domain::storage::write_snapshot(&refused, &parent_log, &parent_log).unwrap();
        meta::write(
            &refused,
            &meta::Meta::new(parent_session, "codex".into(), cwd.display().to_string()),
        )
        .unwrap();
        lab.git(&refused, &["add", "."]);
        lab.git(&refused, &["commit", "-m", "Save synthetic parent"]);
        lab.git(&refused, &["checkout", "work"]);
        let parent_refs = lab.git(&refused, &["show-ref"]);
        let mut parent_claim = agit::domain::link::Link::new("codex", parent, Some(&cwd));
        parent_claim.owner = Some("alice".into());
        parent_claim.agent = Some("refused".into());
        parent_claim.branch = Some("parent".into());
        parent_claim.claimed_by = Some(agit::domain::link::launch::CONFIRMED.into());
        agit::domain::link::write(&store, &parent_claim).unwrap();
        fs::write(
            directory.join(format!("rollout-2026-11-01T00-00-00-{parent}.jsonl")),
            format!("{parent_header}\n"),
        )
        .unwrap();
        let child_header = serde_json::json!({"type":"session_meta","payload":{"id":native,"cwd":cwd,"thread_source":"subagent","parent_thread_id":parent}});
        fs::write(
            directory.join(format!("rollout-2026-11-01T00-00-00-{native}.jsonl")),
            format!("{child_header}\n"),
        )
        .unwrap();
        // Let drop finish startup bookkeeping before the refusal's immutable-state snapshot.
        fs::create_dir_all(lab.home.join("projects")).unwrap();
        let dropped = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["project", "drop", "alice/refused@parent"])
            .output()
            .unwrap();
        assert!(dropped.status.success(), "{dropped:?}");
        let before = lab.state();
        if mode == "human" {
            diagnose("parent_policy_denied", "parent_recorded_hold");
        }
        let output = lab.push_output(&mut command);
        assert_output(&output, mode, 7, "agit project undrop alice/refused@parent");
        assert_eq!(lab.state(), before, "{mode}: parent held");
        lab.no_requests();
        let restored = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["project", "undrop", "alice/refused@parent"])
            .output()
            .unwrap();
        assert!(restored.status.success(), "{restored:?}");
        command.arg("--dry-run");
        let released = lab.push_output(&mut command);
        assert_output(
            &released,
            mode,
            1,
            "new destination needs an immutable repository identity",
        );
        assert_eq!(lab.git(&refused, &["show-ref"]), parent_refs);
        lab.no_requests();
    }
}
