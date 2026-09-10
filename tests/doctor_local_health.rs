use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Lab {
    _root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: Repo,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let work = root.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(
            home.join("config.json"),
            if cfg!(windows) {
                "{\"secrets.keystore\":\"os\"}\n"
            } else {
                "{\"secrets.keystore\":\"file\"}\n"
            },
        )
        .unwrap();
        let repo = Repo::init(&home.join("repos/alice/chosen")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("Synthetic doctor history").unwrap();
        repo.git(&["branch", "work"]).unwrap();
        Self {
            _root: root,
            home,
            work,
            repo,
        }
    }

    fn doctor(&self) -> Output {
        self.doctor_command().output().unwrap()
    }

    fn doctor_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(["doctor", "--repo", "alice/chosen"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self._root.path())
            .env("USERPROFILE", self._root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self._root.path().join("absent-gitconfig"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
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

    fn credential(&self, access: &str, refresh: &str) -> PathBuf {
        let directory = self.home.join("credentials");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("127.0.0.1_1.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "username": "alice",
                "hub": "http://127.0.0.1:1",
                "access_token": "synthetic-private-access",
                "refresh_token": "synthetic-private-refresh",
                "access_expires_at": access,
                "refresh_expires_at": refresh,
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn doctor_distinguishes_local_credential_expiry_without_printing_or_changing_tokens() {
    let lab = Lab::new();
    let future = "2099-01-01T00:00:00Z";
    let past = "2000-01-01T00:00:00Z";
    for (access, refresh, expected) in [
        (future, future, "access valid; refresh valid"),
        (past, future, "access expired; refresh valid"),
        (future, past, "access valid; refresh expired"),
        (past, past, "access expired; refresh expired"),
        ("invalid-expiry", future, "access unknown; refresh valid"),
        (future, "invalid-expiry", "access valid; refresh unknown"),
        (
            "🙂🙂🙂🙂🙂🙂",
            "invalid-expiry",
            "access unknown; refresh unknown",
        ),
    ] {
        let path = lab.credential(access, refresh);
        let before = fs::read(&path).unwrap();
        let output = lab.doctor();
        let rendered = text(&output);
        assert!(output.status.success(), "{rendered}");
        assert!(rendered.contains(expected), "{expected}: {rendered}");
        assert!(!rendered.contains("synthetic-private"), "{rendered}");
        assert!(!rendered.contains("invalid-expiry"), "{rendered}");
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn doctor_reports_an_open_merge_and_target_movement_without_changing_the_transaction() {
    let lab = Lab::new();
    let head = lab.repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    let tx = agit::domain::mergetx::Tx {
        mode: None,
        exploration: None,
        generation: None,
        target: "work".into(),
        source: head.clone(),
        source_repo: Some("alice/chosen".into()),
        source_branch: None,
        base: head.clone(),
        target_head: head.clone(),
        source_head: head,
        picked: Vec::new(),
        summary: None,
    };
    agit::domain::mergetx::lock(lab.repo.root(), &tx).unwrap();
    let path = lab
        .repo
        .common_dir()
        .unwrap()
        .join(agit::domain::mergetx::LOCK_FILE);
    let before = fs::read(&path).unwrap();
    let output = lab.doctor();
    let rendered = text(&output);
    assert!(output.status.success(), "{rendered}");
    assert!(rendered.contains("merge transaction: open"), "{rendered}");
    assert_eq!(fs::read(&path).unwrap(), before);

    fs::write(lab.repo.root().join("memory.txt"), "Synthetic next version").unwrap();
    lab.repo.add_all().unwrap();
    lab.repo.commit("Synthetic target advancement").unwrap();
    let next = lab.repo.git(&["rev-parse", "HEAD"]).unwrap();
    lab.repo
        .git(&["update-ref", "refs/heads/work", &next])
        .unwrap();
    let output = lab.doctor();
    let rendered = text(&output);
    assert!(output.status.success(), "{rendered}");
    assert!(
        rendered.contains("merge transaction: target moved"),
        "{rendered}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn inherited_git_repository_routing_cannot_redirect_doctor_to_a_foreign_transaction() {
    let chosen = Lab::new();
    let foreign = Lab::new();
    let head = foreign.repo.git(&["rev-parse", "HEAD"]).unwrap();
    let tx = agit::domain::mergetx::Tx {
        mode: None,
        exploration: None,
        generation: None,
        target: "foreign-transaction-marker".into(),
        source: head.clone(),
        source_repo: Some("alice/chosen".into()),
        source_branch: None,
        base: head.clone(),
        target_head: head.clone(),
        source_head: head,
        picked: Vec::new(),
        summary: None,
    };
    agit::domain::mergetx::lock(foreign.repo.root(), &tx).unwrap();
    let directory = foreign.repo.common_dir().unwrap();
    let record = directory.join(agit::domain::mergetx::LOCK_FILE);
    let before = fs::read(&record).unwrap();
    for name in ["GIT_DIR", "GIT_COMMON_DIR"] {
        let output = chosen
            .doctor_command()
            .env(name, &directory)
            .output()
            .unwrap();
        let rendered = text(&output);
        assert!(output.status.success(), "{name}: {rendered}");
        assert!(
            !rendered.contains("foreign-transaction-marker"),
            "{name}: {rendered}"
        );
        assert!(
            !rendered.contains("merge transaction:"),
            "{name}: {rendered}"
        );
        assert_eq!(fs::read(&record).unwrap(), before);
    }
}

#[test]
fn a_duplicate_branch_registration_cannot_hide_the_primary_worktree_view() {
    let lab = Lab::new();
    lab.repo.git(&["checkout", "work"]).unwrap();
    let snapshot = Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        lab.work.to_string_lossy().into_owned(),
    );
    meta::write(lab.repo.root(), &snapshot).unwrap();
    let log = agit::domain::transcript::wrap_lines(
        "{\"message\":\"Synthetic history\"}\n",
        &snapshot.runtime,
        &snapshot.session,
    );
    agit::domain::storage::write_snapshot(lab.repo.root(), &log, &log).unwrap();
    lab.repo.add_all().unwrap();
    lab.repo
        .commit("Synthetic session for duplicate registration")
        .unwrap();
    let duplicate = lab._root.path().join("duplicate-worktree");
    lab.repo
        .git(&[
            "worktree",
            "add",
            "--force",
            duplicate.to_str().unwrap(),
            "work",
        ])
        .unwrap();
    let invalid = b"not a valid event id\n";
    fs::write(lab.repo.root().join(meta::VIEW_FILE), invalid).unwrap();
    let output = lab.doctor();
    let rendered = text(&output);
    assert!(output.status.success(), "{rendered}");
    assert!(rendered.contains("VIEW is unreadable"), "{rendered}");
    assert_eq!(
        fs::read(lab.repo.root().join(meta::VIEW_FILE)).unwrap(),
        invalid
    );
}
