use agit::domain::{meta, repo::Repo, storage, transcript};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Lab {
    directory: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: Repo,
}

impl Lab {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("agit");
        let work = directory.path().join("work");
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
        fs::write(home.join("layout-v1.complete"), b"1\n").unwrap();
        let repo = Repo::init(&home.join("repos/alice/history")).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("synthetic file history").unwrap();
        Self {
            directory,
            home,
            work,
            repo,
        }
    }

    fn session(&self, raw: &str) -> String {
        let metadata = meta::Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "claude-code".into(),
            "/synthetic".into(),
        );
        meta::write(self.repo.root(), &metadata).unwrap();
        let log = transcript::wrap_lines(raw, &metadata.runtime, &metadata.session);
        storage::write_snapshot(self.repo.root(), &log, &log).unwrap();
        self.record()
    }

    fn record(&self) -> String {
        self.repo.add_all().unwrap();
        self.repo.commit("synthetic session history").unwrap();
        self.repo.git(&["rev-parse", "HEAD"]).unwrap()
    }

    fn doctor(&self, deep: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.args(["doctor", "--repo", "alice/history"]);
        if deep {
            command.arg("--deep");
        }
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.directory.path())
            .env("USERPROFILE", self.directory.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env("CI", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.directory.path().join("absent-gitconfig"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_NO_LAZY_FETCH", "0")
            .current_dir(&self.work);
        #[cfg(windows)]
        command.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.output().unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn deep_reports_a_bad_ancestor_hidden_by_a_healthy_tip_without_mutation() {
    let lab = Lab::new();
    lab.session("{\"message\":\"hello\"}\n");
    fs::write(
        lab.repo.root().join(meta::VIEW_FILE),
        "not an event sequence\n",
    )
    .unwrap();
    let broken = lab.record();
    lab.session("{\"message\":\"healthy tip\"}\n");
    let before = files(lab.repo.root());
    let shallow = lab.doctor(false);
    assert!(shallow.status.success(), "{}", text(&shallow));
    assert!(!text(&shallow).contains(&broken));
    let deep = lab.doctor(true);
    assert!(deep.status.success(), "{}", text(&deep));
    assert!(text(&deep).contains(&broken), "{}", text(&deep));
    assert!(
        text(&deep).contains("invalid event sequence"),
        "{}",
        text(&deep)
    );
    assert!(text(&deep).contains("refs/heads/main"), "{}", text(&deep));
    assert_eq!(before, files(lab.repo.root()));
}

#[test]
fn deep_keeps_remote_only_and_tag_only_history_in_scope() {
    for reference in ["refs/remotes/origin/archived", "refs/tags/archived"] {
        let lab = Lab::new();
        let main = lab.repo.git(&["rev-parse", "HEAD"]).unwrap();
        lab.repo.git(&["checkout", "--detach", &main]).unwrap();
        lab.session("{\"message\":\"hello\"}\n");
        fs::write(
            lab.repo.root().join(meta::VIEW_FILE),
            "not an event sequence\n",
        )
        .unwrap();
        let broken = lab.record();
        lab.repo.git(&["update-ref", reference, &broken]).unwrap();
        lab.repo.git(&["checkout", "main"]).unwrap();
        let before = files(lab.repo.root());
        let output = lab.doctor(true);
        assert!(output.status.success(), "{}", text(&output));
        assert!(text(&output).contains(&broken), "{}", text(&output));
        assert!(text(&output).contains(reference), "{}", text(&output));
        assert_eq!(before, files(lab.repo.root()));
    }
}
