//! `agit run` and `agit resume` continue a branch at its published tip: a local branch that
//! `origin/<branch>` is strictly ahead of is fast-forwarded first, one that diverged from it is
//! refused with the ways out named, and one level with it is left alone.

use agit::domain::{link, meta, repo::Repo, storage, store, transcript};
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Output};

const NATIVE: &str = "cccccccc-0000-4000-8000-000000000003";

/// `local/qa` with branch `work` checked out and `origin/work` at the published tip, two
/// settled turns of one session.
struct Lab {
    temporary: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: Repo,
    session: String,
    published: String,
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("agit");
        let work = temporary.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let repo = Repo::init(&home.join("repos/local/qa")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
            .unwrap();
        let mut lab = Self {
            temporary,
            home,
            work,
            repo,
            session: format!("agit-{}", "a".repeat(40)),
            published: String::new(),
        };
        lab.turn(1, "Turn 1.");
        lab.published = lab.turn(2, "Published turn 2.");
        // A closed port: nothing in these runs may need the Hub.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        lab.repo
            .git(&[
                "remote",
                "add",
                "origin",
                &format!("http://{closed}/local/qa.git"),
            ])
            .unwrap();
        lab.repo
            .git(&["update-ref", "refs/remotes/origin/work", &lab.published])
            .unwrap();
        lab
    }

    /// The runtime's own transcript of turns 1..=`turn`, the last one asking `prompt`.
    fn raw(&self, turn: u32, prompt: &str) -> String {
        let mut raw = String::new();
        for n in 1..=turn {
            let text = if n == turn {
                prompt.to_owned()
            } else {
                format!("Turn {n}.")
            };
            raw.push_str(&format!(
                "{}\n{}\n",
                json!({"type": "user", "sessionId": NATIVE, "cwd": self.work, "uuid": format!("user-{n}"),
                    "message": {"role": "user", "content": text}}),
                json!({"type": "assistant", "sessionId": NATIVE, "cwd": self.work, "uuid": format!("assistant-{n}"),
                    "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}}),
            ));
        }
        raw
    }

    /// Settles turn `turn` on the checked-out branch and returns its commit.
    fn turn(&self, turn: u32, prompt: &str) -> String {
        let raw = self.raw(turn, prompt);
        let events = transcript::wrap_lines(&raw, "claude-code", &self.session);
        storage::write_snapshot(self.repo.root(), &events, &events).unwrap();
        let mut snapshot = meta::Meta::new(
            self.session.clone(),
            "claude-code".into(),
            self.work.to_string_lossy().into_owned(),
        );
        snapshot.turn = Some(turn);
        meta::write(self.repo.root(), &snapshot).unwrap();
        self.repo.add_all().unwrap();
        self.repo.commit(&format!("Turn {turn}")).unwrap();
        self.head()
    }

    fn head(&self) -> String {
        self.repo
            .git(&["rev-parse", "refs/heads/work"])
            .unwrap()
            .trim()
            .to_owned()
    }

    /// Moves `work` back to its first turn, as a checkout last synced before a collaborator
    /// pushed the second.
    fn behind(self) -> Self {
        self.repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();
        self
    }

    /// Leaves the runtime's own transcript of the first turn on disk, claimed by `work` the way
    /// an import claims it: the session this checkout last ran, before the collaborator's turn.
    fn native_session_of_the_first_turn(self) -> Self {
        let project = self
            .temporary
            .path()
            .join(".claude/projects")
            .join(store::slug_for(&self.work));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{NATIVE}.jsonl")),
            self.raw(1, "Turn 1."),
        )
        .unwrap();
        let mut claim = link::Link::new("claude-code", NATIVE, Some(&self.work));
        claim.owner = Some("local".into());
        claim.agent = Some("qa".into());
        claim.branch = Some("work".into());
        link::write(&store::Store::at(self.home.join("store")), &claim).unwrap();
        self
    }

    /// Settles a second turn of its own on top of the first, beside the published one.
    fn diverged(self) -> Self {
        let lab = self.behind();
        lab.turn(2, "Local turn 2.");
        lab
    }

    /// Every transcript the runtime was handed, outside agit's own store.
    fn materialized(&self) -> String {
        fn walk(dir: &std::path::Path, skip: &std::path::Path, out: &mut String) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path == skip {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, skip, out);
                } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                    out.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
                }
            }
        }
        let mut out = String::new();
        walk(self.temporary.path(), &self.home, &mut out);
        out
    }

    fn agit(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.temporary.path())
            .env("AGIT_HOME", &self.home)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.temporary.path().join("empty-gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work);
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", self.temporary.path());
        }
        command.output().unwrap()
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = std::fs::read(self.home.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

const COMMANDS: [&[&str]; 2] = [
    &["run", "local/qa@work", "--no-launch"],
    &["resume", "local/qa@work", "--no-launch"],
];

/// Behind: both commands continue the published tip, with the collaborator's turn in the
/// materialized session. Continuing the older local head without a word passes a check that
/// looks only at the exit status, so the branch head and the materialized VIEW are both read.
#[test]
fn a_branch_behind_its_published_tip_is_fast_forwarded_then_continued() {
    for args in COMMANDS {
        let lab = Lab::new().behind();
        let output = lab.agit(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stdout} {stderr}");
        assert!(
            format!("{stdout}{stderr}").contains("fast-forwarded `work` +1 to the published tip"),
            "{args:?}: {stdout} {stderr}"
        );
        assert_eq!(lab.head(), lab.published, "{args:?}");
        assert!(
            lab.materialized().contains("Published turn 2."),
            "{args:?}: the materialized session lacks the published turn"
        );
    }
}

/// Diverged: both commands stop before materializing anything, name a merge and a fork of the
/// published point that resolve as typed, and leave the local branch where it was.
#[test]
fn a_branch_diverged_from_its_published_tip_is_refused_with_the_ways_out() {
    for args in COMMANDS {
        let lab = Lab::new().diverged();
        let local = lab.head();
        let output = lab.agit(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {stdout} {stderr}");
        assert!(
            stderr.contains("diverged from the published tip"),
            "{stderr}"
        );
        let short = &lab.published[..7];
        assert!(
            stderr.contains(&format!("agit merge local/qa@{short}")),
            "{stderr}"
        );
        assert!(stderr.contains("--into local/qa@work"), "{stderr}");
        assert!(
            stderr.contains(&format!("agit fork local/qa@{short}")),
            "{stderr}"
        );
        assert!(!stdout.contains("materialized VIEW"), "{stdout}");
        assert_eq!(lab.head(), local, "{args:?}");
    }
}

/// Level: nothing moves and nothing is said about the published tip.
#[test]
fn a_branch_level_with_its_published_tip_continues_unchanged() {
    for args in COMMANDS {
        let lab = Lab::new();
        let output = lab.agit(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stdout} {stderr}");
        assert!(
            !format!("{stdout}{stderr}").contains("fast-forwarded"),
            "{stdout} {stderr}"
        );
        assert_eq!(lab.head(), lab.published, "{args:?}");
    }
}

/// Behind, with the runtime's own transcript of the older head still claimed: that native
/// session continues the stale head, so a reuse check judged against the stale head passes and
/// reopens it without the collaborator's turn. Once the branch is fast-forwarded it is not
/// reused, and the session handed to the runtime carries the published turn.
#[test]
fn a_native_session_of_the_stale_head_is_not_reused_after_the_fast_forward() {
    for args in COMMANDS {
        let lab = Lab::new().behind().native_session_of_the_first_turn();
        let stale = lab.head();
        lab.repo
            .git(&["update-ref", "refs/remotes/origin/work", &stale])
            .unwrap();
        let level = lab.agit(&["resume", "local/qa@work", "--no-launch"]);
        assert!(
            String::from_utf8_lossy(&level.stdout).contains("(zero-copy)"),
            "the native session must be reusable at the stale head: {} {}",
            String::from_utf8_lossy(&level.stdout),
            String::from_utf8_lossy(&level.stderr)
        );
        lab.repo
            .git(&["update-ref", "refs/remotes/origin/work", &lab.published])
            .unwrap();

        let output = lab.agit(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stdout} {stderr}");
        assert_eq!(lab.head(), lab.published, "{args:?}");
        assert!(!stdout.contains("zero-copy"), "{args:?}: {stdout}");
        assert!(
            lab.materialized().contains("Published turn 2."),
            "{args:?}: the session handed to the runtime lacks the published turn"
        );
    }
}
