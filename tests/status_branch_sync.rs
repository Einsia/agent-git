use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: PathBuf,
    hub: TcpListener,
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                entries.insert(relative, Entry::Symlink(fs::read_link(path).unwrap()));
            } else if kind.is_dir() {
                entries.insert(relative, Entry::Directory);
                walk(root, &path, entries);
            } else {
                entries.insert(relative, Entry::File(fs::read(path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[derive(Debug)]
struct Screen {
    lines: Vec<String>,
    warnings: Vec<String>,
}

impl Screen {
    fn rows(&self) -> BTreeSet<String> {
        assert!(
            self.lines
                .iter()
                .any(|line| line == "repo\tbranch\tlast commit\ttracking ref\tstate"),
            "missing branch table: {self:?}"
        );
        let rows = self
            .lines
            .iter()
            .filter(|line| line.starts_with("local/qa\t"))
            .cloned()
            .collect::<Vec<_>>();
        assert!(rows.iter().all(|row| row.split('\t').count() == 5));
        let unique = rows.iter().cloned().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), rows.len(), "duplicate branch rows");
        unique
    }
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home/.agit");
        let work = root.path().join("work");
        let repo = home.join("repos/local/qa");
        for directory in [&repo, &work, &root.path().join("tmp")] {
            fs::create_dir_all(directory).unwrap();
        }
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let lab = Self {
            root,
            home,
            work,
            repo,
            hub,
        };
        lab.git(&["init", "--quiet", "--initial-branch=main"]);
        lab.git(&["config", "commit.gpgsign", "false"]);
        lab.git(&["config", "core.logAllRefUpdates", "false"]);
        lab.git(&["config", "gc.auto", "0"]);
        assert!(!lab.home.join("store").exists());
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let home = self.root.path().join("home");
        let temporary = self.root.path().join("tmp");
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("AGIT_HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("TMPDIR", &temporary)
            .env("TEMP", &temporary)
            .env("TMP", &temporary)
            .env(
                "AGIT_HUB_URL",
                format!("http://{}", self.hub.local_addr().unwrap()),
            )
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join("absent-gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "Synthetic status author")
            .env("GIT_AUTHOR_EMAIL", "status@example.invalid")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00+00:00")
            .env("GIT_COMMITTER_NAME", "Synthetic status author")
            .env("GIT_COMMITTER_EMAIL", "status@example.invalid")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00+00:00")
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            command.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
            for name in ["SystemRoot", "WINDIR", "COMSPEC"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        command
    }

    fn git(&self, args: &[&str]) -> String {
        success(
            self.command("git")
                .current_dir(&self.repo)
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn git_input(&self, args: &[&str], input: &[u8]) -> String {
        let mut child = self
            .command("git")
            .current_dir(&self.repo)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        success(child.wait_with_output().unwrap())
    }

    fn commit(&self, parent: Option<&str>, label: &str) -> String {
        let tree = self.git_input(&["mktree"], b"");
        let mut args = vec!["commit-tree", tree.as_str(), "-m", label];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        self.git(&args)
    }

    fn reference(&self, name: &str, sha: &str) {
        self.git(&["update-ref", name, sha]);
    }

    fn tracking(&self, branch: &str, remote: &str, target: &str) {
        self.git(&[
            "config",
            &format!("remote.{remote}.url"),
            &format!("http://{}/uncontacted.git", self.hub.local_addr().unwrap()),
        ]);
        self.git(&[
            "config",
            &format!("remote.{remote}.fetch"),
            &format!("+refs/heads/*:refs/remotes/{remote}/*"),
        ]);
        self.git(&["config", &format!("branch.{branch}.remote"), remote]);
        self.git(&[
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{target}"),
        ]);
    }

    fn screens(&self) -> Vec<Screen> {
        let mut screens = Vec::new();
        for version in [None, Some("1"), Some("2")] {
            assert!(!self.home.join("store").exists());
            let before = snapshot(self.root.path());
            let head = self.git(&["symbolic-ref", "HEAD"]);
            let refs = self.git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(symref)",
            ]);
            let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
            command.current_dir(&self.work);
            if let Some(version) = version {
                command.args(["--json", "--json-version", version]);
            }
            let output = command.arg("status").output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                snapshot(self.root.path()),
                before,
                "status changed owned files"
            );
            assert_eq!(self.git(&["symbolic-ref", "HEAD"]), head);
            assert_eq!(
                self.git(&[
                    "for-each-ref",
                    "--format=%(refname) %(objectname) %(symref)"
                ]),
                refs
            );
            assert!(!self.home.join("store").exists(), "status created a Store");
            assert_eq!(
                self.hub.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "local status contacted the Hub or a Git remote"
            );
            let screen = if let Some(version) = version {
                assert!(output.stderr.is_empty(), "JSON diagnostics escaped capture");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema"], "cli-output");
                assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
                assert_eq!(value["command"], "status");
                assert_eq!(value["ok"], true);
                assert_eq!(value["exit_code"], 0);
                assert_eq!(value["result"]["format"], "text");
                assert_eq!(value["result"]["kind"], "status");
                if version == "2" {
                    assert_eq!(value["fix"], serde_json::json!([]));
                } else {
                    assert!(value.get("fix").is_none());
                }
                Screen {
                    lines: value["result"]["lines"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|line| line.as_str().unwrap().to_owned())
                        .collect(),
                    warnings: value["diagnostics"]["stderr"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|entry| entry["level"] == "warning")
                        .map(|entry| entry["message"].as_str().unwrap().to_owned())
                        .collect(),
                }
            } else {
                Screen {
                    lines: String::from_utf8(output.stdout)
                        .unwrap()
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(str::to_owned)
                        .collect(),
                    warnings: String::from_utf8(output.stderr)
                        .unwrap()
                        .lines()
                        .filter_map(|line| line.strip_prefix("note "))
                        .map(str::to_owned)
                        .collect(),
                }
            };
            assert!(
                screen
                    .lines
                    .iter()
                    .any(|line| line == "  no sessions adopted yet.")
            );
            assert!(
                !screen
                    .lines
                    .iter()
                    .any(|line| line.contains("never pushed")),
                "missing local evidence must not imply publication history"
            );
            if let Some(first) = screens.first() {
                let first: &Screen = first;
                assert_eq!(screen.lines, first.lines, "JSON changed the status data");
                assert_eq!(screen.warnings, first.warnings);
            }
            screens.push(screen);
        }
        screens
    }

    fn expect(&self, rows: &[String]) {
        let expected = rows.iter().cloned().collect::<BTreeSet<_>>();
        for screen in self.screens() {
            assert_eq!(screen.rows(), expected);
            assert!(screen.warnings.is_empty(), "unexpected warning: {screen:?}");
        }
    }
}

fn row(branch: &str, sha: &str, tracking: &str, state: &str) -> String {
    format!(
        "local/qa\t{branch}\tagit-{}\t{tracking}\t{state}",
        &sha[..8]
    )
}

#[test]
fn local_branches_have_independent_sync_states_without_a_store() {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    let left = lab.commit(Some(&root), "left");
    let tip = lab.commit(Some(&left), "left tip");
    let right = lab.commit(Some(&root), "right");
    for (branch, head, remote) in [
        ("main", &root, &root),
        ("ahead", &tip, &left),
        ("behind", &left, &tip),
        ("diverged", &tip, &right),
        ("fallback", &root, &tip),
    ] {
        lab.reference(&format!("refs/heads/{branch}"), head);
        lab.reference(&format!("refs/remotes/origin/{branch}"), remote);
    }
    lab.reference("refs/heads/synced", &right);
    lab.reference("refs/remotes/peer/synced", &right);
    lab.tracking("synced", "peer", "synced");
    lab.reference("refs/heads/never", &root);
    lab.reference("refs/heads/missing", &root);
    lab.tracking("missing", "origin", "lost");
    lab.reference("refs/remotes/origin/missing", &root);
    lab.reference("refs/remotes/origin/remote-only", &left);
    lab.reference("refs/remotes/peer/topic", &right);
    lab.git(&["symbolic-ref", "refs/heads/alias", "refs/heads/main"]);
    lab.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    lab.reference("refs/tags/not-a-branch", &tip);
    lab.expect(&[
        row(
            "main",
            &root,
            "refs/remotes/origin/main",
            "in sync (ahead 0, behind 0)",
        ),
        row("alias", &root, "refs/heads/main", "symbolic local ref"),
        row(
            "ahead",
            &tip,
            "refs/remotes/origin/ahead",
            "ahead 1, behind 0",
        ),
        row(
            "behind",
            &left,
            "refs/remotes/origin/behind",
            "ahead 0, behind 1",
        ),
        row(
            "diverged",
            &tip,
            "refs/remotes/origin/diverged",
            "diverged (ahead 2, behind 1)",
        ),
        row(
            "fallback",
            &root,
            "refs/remotes/origin/fallback",
            "ahead 0, behind 2",
        ),
        row(
            "synced",
            &right,
            "refs/remotes/peer/synced",
            "in sync (ahead 0, behind 0)",
        ),
        row("never", &root, "—", "no known tracking ref"),
        row(
            "missing",
            &root,
            "refs/remotes/origin/lost",
            "tracking ref unavailable locally",
        ),
        row(
            "origin/missing",
            &root,
            "refs/remotes/origin/missing",
            "remote only; no local tracking branch",
        ),
        row(
            "origin/remote-only",
            &left,
            "refs/remotes/origin/remote-only",
            "remote only; no local tracking branch",
        ),
        row(
            "peer/topic",
            &right,
            "refs/remotes/peer/topic",
            "remote only; no local tracking branch",
        ),
    ]);
}

#[test]
fn each_status_uses_current_known_refs_without_moving_the_primary_head() {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    let middle = lab.commit(Some(&root), "middle");
    let tip = lab.commit(Some(&middle), "tip");
    lab.reference("refs/heads/main", &root);
    lab.reference("refs/heads/topic", &tip);
    lab.tracking("topic", "origin", "topic");
    for (known, state) in [
        (Some(&root), "ahead 2, behind 0"),
        (Some(&middle), "ahead 1, behind 0"),
        (Some(&tip), "in sync (ahead 0, behind 0)"),
        (None, "tracking ref unavailable locally"),
    ] {
        if let Some(known) = known {
            lab.reference("refs/remotes/origin/topic", known);
            lab.git(&["pack-refs", "--all"]);
        } else {
            lab.git(&["update-ref", "-d", "refs/remotes/origin/topic"]);
        }
        lab.expect(&[
            row("main", &root, "—", "no known tracking ref"),
            row("topic", &tip, "refs/remotes/origin/topic", state),
        ]);
        assert_eq!(lab.git(&["rev-parse", "HEAD"]), root);
    }
}

#[test]
fn an_unmapped_explicit_upstream_does_not_fall_back_to_origin() {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    let tip = lab.commit(Some(&root), "local tip");
    lab.reference("refs/heads/main", &tip);
    lab.reference("refs/remotes/origin/main", &tip);
    lab.reference("refs/remotes/peer/trunk", &root);
    lab.tracking("main", "peer", "trunk");
    lab.git(&[
        "config",
        "remote.peer.fetch",
        "+refs/heads/other:refs/remotes/peer/other",
    ]);
    assert!(
        lab.git(&["for-each-ref", "--format=%(upstream)", "refs/heads/main"])
            .is_empty()
    );
    lab.expect(&[
        row("main", &tip, "—", "tracking ref unavailable locally"),
        row(
            "origin/main",
            &tip,
            "refs/remotes/origin/main",
            "remote only; no local tracking branch",
        ),
        row(
            "peer/trunk",
            &root,
            "refs/remotes/peer/trunk",
            "remote only; no local tracking branch",
        ),
    ]);
    lab.git(&[
        "config",
        "remote.peer.fetch",
        "+refs/heads/*:refs/remotes/peer/*",
    ]);
    lab.expect(&[
        row("main", &tip, "refs/remotes/peer/trunk", "ahead 1, behind 0"),
        row(
            "origin/main",
            &tip,
            "refs/remotes/origin/main",
            "remote only; no local tracking branch",
        ),
    ]);
}

/// Explicit upstreams retain their exact names even outside conventional tracking namespaces.
/// A descendant of a missing ref cannot supply the selected upstream's identity.
#[test]
fn custom_upstreams_use_current_local_refs_without_substituting_origin_or_descendants() {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    let middle = lab.commit(Some(&root), "middle");
    let tip = lab.commit(Some(&middle), "tip");
    let other = lab.commit(Some(&root), "other");
    let upstream = "refs/upstreams/peer/trunk";
    lab.reference("refs/heads/main", &tip);
    lab.reference("refs/remotes/origin/main", &tip);
    lab.reference("refs/tags/unrelated", &other);
    lab.tracking("main", "peer", "trunk");
    lab.git(&[
        "config",
        "remote.peer.fetch",
        "+refs/heads/*:refs/upstreams/peer/*",
    ]);
    let origin = row(
        "origin/main",
        &tip,
        "refs/remotes/origin/main",
        "remote only; no local tracking branch",
    );
    for (known, state) in [
        (&root, "ahead 2, behind 0"),
        (&middle, "ahead 1, behind 0"),
        (&other, "diverged (ahead 2, behind 1)"),
        (&tip, "in sync (ahead 0, behind 0)"),
    ] {
        lab.reference(upstream, known);
        lab.git(&["pack-refs", "--all"]);
        assert_eq!(lab.git(&["rev-parse", "main@{upstream}"]), *known);
        lab.expect(&[row("main", &tip, upstream, state), origin.clone()]);
    }
    lab.git(&["update-ref", "-d", upstream]);
    let missing = [
        row("main", &tip, upstream, "tracking ref unavailable locally"),
        origin,
    ];
    lab.expect(&missing);
    lab.reference("refs/upstreams/peer/trunk/child", &root);
    assert_eq!(
        lab.git(&["for-each-ref", "--format=%(refname)", "--", upstream]),
        "refs/upstreams/peer/trunk/child"
    );
    lab.expect(&missing);
}

/// An unreadable selected upstream is an inspection failure, not proof of an absent ref.
#[test]
fn broken_custom_upstreams_are_not_reported_as_missing_tracking_refs() {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    let upstream = "refs/upstreams/peer/trunk";
    lab.reference("refs/heads/main", &root);
    lab.reference(upstream, &root);
    lab.tracking("main", "peer", "trunk");
    lab.git(&[
        "config",
        "remote.peer.fetch",
        "+refs/heads/*:refs/upstreams/peer/*",
    ]);
    lab.expect(&[row("main", &root, upstream, "in sync (ahead 0, behind 0)")]);
    fs::write(
        lab.repo.join(".git").join(upstream),
        "invalid object identity\n",
    )
    .unwrap();
    lab.expect(&[
        "local/qa\t—\t—\t—\tunavailable: branch refs cannot be completely inspected".into(),
    ]);
}

fn truncated_graph(marker: &str, expected: &str) {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    let middle = lab.commit(Some(&root), "middle");
    let tip = lab.commit(Some(&middle), "tip");
    lab.reference("refs/heads/main", &root);
    lab.reference("refs/heads/topic", &tip);
    lab.reference("refs/remotes/origin/topic", &root);
    let complete = [
        row("main", &root, "—", "no known tracking ref"),
        row(
            "topic",
            &tip,
            "refs/remotes/origin/topic",
            "ahead 2, behind 0",
        ),
    ];
    lab.expect(&complete);
    let path = lab.repo.join(".git").join(marker);
    fs::write(&path, format!("{tip}\n")).unwrap();
    let enumerated = lab.git(&["rev-list", &tip, &root]);
    assert!(
        !enumerated.lines().any(|oid| oid == middle),
        "fixture did not truncate ancestry"
    );
    assert!(
        lab.git(&["cat-file", "-p", &tip])
            .lines()
            .any(|line| line == format!("parent {middle}")),
        "fixture must preserve the immutable parent"
    );
    lab.expect(&[
        row("main", &root, "—", "no known tracking ref"),
        row("topic", &tip, "refs/remotes/origin/topic", expected),
    ]);
    fs::remove_file(path).unwrap();
    lab.expect(&complete);
}

#[test]
fn shallow_boundaries_cannot_hide_unpublished_ancestors() {
    truncated_graph(
        "shallow",
        "comparison unavailable: complete immutable ancestry is unavailable locally",
    );
}

#[test]
fn graft_boundaries_cannot_hide_unpublished_ancestors() {
    truncated_graph(
        "info/grafts",
        "comparison unavailable: complete immutable ancestry is unavailable locally",
    );
}

#[test]
fn display_limit_reports_incomplete_without_expanding_history() {
    let lab = Lab::new();
    let root = lab.commit(None, "root");
    lab.reference("refs/heads/main", &root);
    let updates = (0..129)
        .map(|index| format!("update refs/heads/branch-{index:03} {root}\n"))
        .collect::<String>();
    lab.git_input(&["update-ref", "--stdin"], updates.as_bytes());
    assert_eq!(lab.git(&["rev-list", "--all", "--count"]), "1");
    let expected = (0..128)
        .map(|index| {
            row(
                &format!("branch-{index:03}"),
                &root,
                "—",
                "no known tracking ref",
            )
        })
        .collect::<BTreeSet<_>>();
    for screen in lab.screens() {
        assert_eq!(screen.rows(), expected);
        assert_eq!(
            screen.warnings,
            ["status is incomplete: additional branches or repositories exceed the display budget"]
        );
        assert!(
            !screen
                .lines
                .iter()
                .any(|line| line.starts_with("local/qa\tbranch-128\t"))
        );
        assert!(
            !screen
                .lines
                .iter()
                .any(|line| line.starts_with("local/qa\tmain\t"))
        );
    }
}
