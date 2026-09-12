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
    status: Option<serde_json::Value>,
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
            .filter(|line| line.starts_with("local/qa\t") && line.split('\t').count() == 5)
            .cloned()
            .collect::<Vec<_>>();
        assert!(rows.iter().all(|row| row.split('\t').count() == 5));
        let unique = rows.iter().cloned().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), rows.len(), "duplicate branch rows");
        unique
    }
}

fn display_cell(value: &str) -> String {
    let mut chars = value.chars();
    let mut output = String::new();
    for character in chars.by_ref().take(160) {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    if chars.next().is_some() {
        output.push('…');
    }
    output
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
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("PRIVATE-"),
                "status disclosed synthetic private content"
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-"));
            let screen = if let Some(version) = version {
                assert!(output.stderr.is_empty(), "JSON diagnostics escaped capture");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema"], "cli-output");
                assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
                assert_eq!(value["command"], "status");
                assert_eq!(value["ok"], true);
                assert_eq!(value["exit_code"], 0);
                assert_eq!(value["result"]["format"], "json");
                assert_eq!(value["result"]["kind"], "status");
                if version == "2" {
                    assert_eq!(value["fix"], serde_json::json!([]));
                } else {
                    assert!(value.get("fix").is_none());
                }
                let status = &value["result"]["value"];
                assert_eq!(status["sessions"]["total"], 0);
                let mut lines = vec![
                    "  no sessions adopted yet.".to_owned(),
                    "repo\tbranch\tlast commit\ttracking ref\tstate".to_owned(),
                ];
                let mut incomplete = status["repositories_omitted"].as_u64().unwrap() > 0;
                for repo in status["repositories"].as_array().unwrap() {
                    assert_eq!(repo["repo"], "local/qa");
                    let page = &repo["branches"];
                    if let Some(error) = page["error"].as_str() {
                        assert!(page["items"].is_null());
                        lines.push(format!("local/qa\t—\t—\t—\tunavailable: {error}"));
                        continue;
                    }
                    incomplete |= page["omitted"].as_u64().unwrap() > 0;
                    let branches = page["items"].as_array().unwrap();
                    if branches.is_empty() {
                        lines.push("local/qa\t—\t—\t—\tno branch refs".to_owned());
                    }
                    for branch in branches {
                        let state = branch["state"].as_str().unwrap();
                        let compared = state.starts_with("ahead ")
                            || state.starts_with("diverged ")
                            || state.starts_with("in sync ");
                        for field in ["ahead", "behind"] {
                            if compared {
                                let count = branch[field].as_u64().unwrap();
                                assert!(state.contains(&format!("{field} {count}")));
                            } else {
                                assert!(branch[field].is_null());
                            }
                        }
                        let tracking = branch["tracking"].as_str().unwrap();
                        lines.push(row(
                            branch["name"].as_str().unwrap(),
                            branch["head"].as_str().unwrap(),
                            if tracking.is_empty() { "—" } else { tracking },
                            state,
                        ));
                    }
                }
                let shared = &status["shared_files"];
                assert!(shared["incomplete"].is_boolean());
                let mut warnings = if incomplete {
                    vec!["status is incomplete: additional branches or repositories exceed the display budget".to_owned()]
                } else {
                    Vec::new()
                };
                for change in shared["items"].as_array().unwrap() {
                    assert_eq!(change["repo"], "local/qa");
                    let target = if let Some(checkout) = change["checkout"].as_str() {
                        assert!(
                            Path::new(checkout)
                                .canonicalize()
                                .unwrap()
                                .starts_with(self.root.path().canonicalize().unwrap())
                        );
                        format!(
                            "local/qa@{}",
                            change["branch"].as_str().unwrap_or("(detached)")
                        )
                    } else {
                        assert!(change["branch"].is_null());
                        "local/qa".to_owned()
                    };
                    lines.push(format!(
                        "{}\t{}\t{}\t{}",
                        display_cell(&target),
                        change["path"]
                            .as_str()
                            .map(display_cell)
                            .unwrap_or_else(|| "—".into()),
                        change["staged"].as_str().unwrap(),
                        change["local_bytes"].as_str().unwrap()
                    ));
                }
                if shared["incomplete"] == true {
                    warnings.push("shared-file inspection is incomplete; unavailable evidence does not mean unchanged".into());
                }
                let merges = &status["merge_transactions"];
                assert!(merges["incomplete"].is_boolean());
                assert_eq!(merges["repositories_omitted"], 0);
                let transactions = merges["items"].as_array().unwrap();
                if !transactions.is_empty() {
                    lines.push("target\tsource\tprogress".into());
                }
                for tx in transactions {
                    assert_eq!(tx["repo"], "local/qa");
                    assert!(tx.get("summary").is_none());
                    assert!(tx.get("picked").is_none());
                    if let Some(error) = tx["error"].as_str() {
                        assert_eq!(merges["incomplete"], true);
                        for field in [
                            "target",
                            "source",
                            "target_head",
                            "source_head",
                            "picked_count",
                            "summary_ready",
                        ] {
                            assert!(tx[field].is_null());
                        }
                        lines.push(format!("local/qa\t—\t{error}"));
                    } else {
                        for field in ["target_head", "source_head"] {
                            let oid = tx[field].as_str().unwrap();
                            assert!(
                                matches!(oid.len(), 40 | 64)
                                    && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                            );
                        }
                        lines.push(format!(
                            "{}\t{}\topen; {} picks; summary {}",
                            display_cell(&format!("local/qa@{}", tx["target"].as_str().unwrap())),
                            display_cell(tx["source"].as_str().unwrap()),
                            tx["picked_count"].as_u64().unwrap(),
                            if tx["summary_ready"].as_bool().unwrap() {
                                "ready"
                            } else {
                                "missing"
                            }
                        ));
                    }
                }
                if merges["incomplete"] == true {
                    warnings.push("merge transaction inspection is incomplete; unavailable evidence does not mean no transaction".into());
                }
                Screen {
                    status: Some(status.clone()),
                    lines,
                    warnings,
                }
            } else {
                Screen {
                    status: None,
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
                assert_eq!(
                    screen.rows(),
                    first.rows(),
                    "JSON changed the branch status data"
                );
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
    if marker == "info/grafts" {
        lab.git(&["config", "advice.graftFileDeprecated", "false"]);
    }
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

#[test]
fn merge_transactions_report_retained_progress_without_a_store_or_side_effects() {
    let lab = Lab::new();
    let head = lab.commit(None, "merge target");
    lab.reference("refs/heads/main", &head);
    let path = lab.repo.join(".git/AGIT_MERGE_TX");
    let mut tx = serde_json::json!({
        "target":"main", "source":"peer/source@topic", "base":"",
        "source_repo":"peer/source", "source_branch":"topic",
        "target_head":head, "source_head":head, "picked":["peer/source@topic#1"],
        "summary":null, "future":{"retained":true}
    });
    for (summary, expected) in [
        (serde_json::Value::Null, "missing"),
        (serde_json::json!("   "), "missing"),
        (serde_json::json!("PRIVATE-SUMMARY-CONTENT"), "ready"),
    ] {
        tx["summary"] = summary;
        fs::write(&path, serde_json::to_vec(&tx).unwrap()).unwrap();
        for screen in lab.screens() {
            if let Some(status) = &screen.status {
                let row = &status["merge_transactions"]["items"][0];
                assert_eq!(row["target"], "main");
                assert_eq!(row["source"], "peer/source@topic");
                assert_eq!(row["target_head"], head);
                assert_eq!(row["source_head"], head);
                assert_eq!(row["picked_count"], 1);
                assert_eq!(row["summary_ready"], expected == "ready");
                assert!(row["error"].is_null());
                assert_eq!(status["merge_transactions"]["incomplete"], false);
            }
            assert!(
                screen
                    .lines
                    .contains(&"target\tsource\tprogress".to_owned())
            );
            assert!(screen.lines.contains(&format!(
                "local/qa@main\tpeer/source@topic\topen; 1 picks; summary {expected}"
            )));
            assert!(
                !screen
                    .lines
                    .iter()
                    .any(|line| line.contains("PRIVATE-SUMMARY-CONTENT"))
            );
            assert!(screen.warnings.is_empty());
        }
    }
    fs::remove_file(path).unwrap();
    for screen in lab.screens() {
        assert!(
            !screen
                .lines
                .contains(&"target\tsource\tprogress".to_owned())
        );
    }
}

#[test]
fn invalid_merge_evidence_is_unavailable_instead_of_absent() {
    let lab = Lab::new();
    let head = lab.commit(None, "merge target");
    lab.reference("refs/heads/main", &head);
    let path = lab.repo.join(".git/AGIT_MERGE_TX");
    for bytes in [
        b"{broken".to_vec(),
        serde_json::to_vec(&serde_json::json!({
            "target":"../foreign", "source":"peer/source@topic", "base":"",
            "target_head":head, "source_head":head
        }))
        .unwrap(),
        serde_json::to_vec(&serde_json::json!({
            "target":"main", "source":"x".repeat(4097), "base":"",
            "target_head":head, "source_head":head
        }))
        .unwrap(),
        vec![b' '; 1024 * 1024 + 1],
    ] {
        fs::write(&path, bytes).unwrap();
        for screen in lab.screens() {
            assert!(screen.lines.contains(
                &"local/qa\t—\tunavailable: transaction missing, changed, or unreadable".to_owned()
            ));
            assert!(!screen.lines.iter().any(|line| line.contains("../foreign")));
        }
    }
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    for screen in lab.screens() {
        assert!(
            screen
                .lines
                .iter()
                .any(|line| line.contains("unavailable: transaction"))
        );
    }
}

#[cfg(unix)]
#[test]
fn merge_transaction_symlinks_do_not_read_their_destination() {
    let lab = Lab::new();
    let head = lab.commit(None, "merge target");
    lab.reference("refs/heads/main", &head);
    let destination = lab.root.path().join("private-foreign");
    fs::write(&destination, "PRIVATE-FOREIGN-CONTENT").unwrap();
    std::os::unix::fs::symlink(&destination, lab.repo.join(".git/AGIT_MERGE_TX")).unwrap();
    for screen in lab.screens() {
        assert!(
            screen
                .lines
                .iter()
                .any(|line| line.contains("unavailable: transaction"))
        );
        assert!(
            !screen
                .lines
                .iter()
                .any(|line| line.contains("PRIVATE-FOREIGN-CONTENT"))
        );
    }
}

fn shared_rows(screen: &Screen) -> BTreeSet<String> {
    screen
        .lines
        .iter()
        .filter(|line| line.starts_with("local/qa@") && line.split('\t').count() == 4)
        .cloned()
        .collect()
}

fn shared_base(lab: &Lab) {
    fs::create_dir_all(lab.repo.join("memory")).unwrap();
    fs::create_dir_all(lab.repo.join("skills")).unwrap();
    for path in [
        "AGENTS.md",
        "memory/staged.md",
        "memory/deleted.md",
        "skills/removed.md",
    ] {
        fs::write(lab.repo.join(path), b"shared baseline\n").unwrap();
    }
    lab.git(&["add", "--", "AGENTS.md", "memory", "skills"]);
    lab.git(&["commit", "--quiet", "-m", "shared baseline"]);
}

#[test]
fn shared_files_distinguish_staged_local_deleted_untracked_and_intent_to_add() {
    let lab = Lab::new();
    shared_base(&lab);
    fs::write(lab.repo.join("AGENTS.md"), b"PRIVATE-LOCAL-CONTENT\n").unwrap();
    fs::write(lab.repo.join("memory/staged.md"), b"staged content\n").unwrap();
    lab.git(&["add", "--", "memory/staged.md"]);
    fs::write(
        lab.repo.join("memory/staged.md"),
        b"PRIVATE-LATER-CONTENT\n",
    )
    .unwrap();
    fs::remove_file(lab.repo.join("memory/deleted.md")).unwrap();
    lab.git(&["rm", "--", "skills/removed.md"]);
    fs::create_dir_all(lab.repo.join("skills")).unwrap();
    fs::write(
        lab.repo.join("skills/new.md"),
        b"PRIVATE-UNTRACKED-CONTENT\n",
    )
    .unwrap();
    fs::write(lab.repo.join("memory/intent.md"), b"intent bytes\n").unwrap();
    lab.git(&["add", "--intent-to-add", "--", "memory/intent.md"]);
    fs::write(lab.repo.join(".git/info/exclude"), b"skills/ignored.md\n").unwrap();
    fs::write(
        lab.repo.join("skills/ignored.md"),
        b"PRIVATE-IGNORED-CONTENT\n",
    )
    .unwrap();
    fs::write(
        lab.repo.join("unrelated.txt"),
        b"PRIVATE-UNRELATED-CONTENT\n",
    )
    .unwrap();
    let expected = [
        "local/qa@main\tAGENTS.md\tunchanged\tmodified (raw bytes)",
        "local/qa@main\tmemory/staged.md\tmodified\tmodified (raw bytes)",
        "local/qa@main\tmemory/deleted.md\tunchanged\tdeleted",
        "local/qa@main\tskills/removed.md\tdeleted\tabsent",
        "local/qa@main\tskills/new.md\tunchanged\tuntracked",
        "local/qa@main\tmemory/intent.md\tunchanged\tmodified (raw bytes)",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    for screen in lab.screens() {
        assert_eq!(shared_rows(&screen), expected);
        assert!(screen.warnings.is_empty());
        assert!(!screen.lines.iter().any(|line| line.contains("PRIVATE-")
            || line.contains("ignored.md")
            || line.contains("unrelated.txt")));
    }
}

#[test]
fn shared_raw_comparison_does_not_execute_filters_or_fsmonitor() {
    let lab = Lab::new();
    shared_base(&lab);
    lab.git(&["config", "core.autocrlf", "true"]);
    fs::write(lab.repo.join("AGENTS.md"), b"shared baseline\r\n").unwrap();
    fs::write(
        lab.repo.join(".gitattributes"),
        b"AGENTS.md filter=status-probe\n",
    )
    .unwrap();
    lab.git(&[
        "config",
        "filter.status-probe.clean",
        "echo SIDE-EFFECT > filter-ran; exit 1",
    ]);
    lab.git(&[
        "config",
        "filter.status-probe.process",
        "echo SIDE-EFFECT > process-ran; exit 1",
    ]);
    lab.git(&["config", "filter.status-probe.required", "true"]);
    lab.git(&[
        "config",
        "core.fsmonitor",
        "echo SIDE-EFFECT > fsmonitor-ran; exit 1",
    ]);
    for screen in lab.screens() {
        assert_eq!(
            shared_rows(&screen),
            ["local/qa@main\tAGENTS.md\tunchanged\tmodified (raw bytes)".to_owned()]
                .into_iter()
                .collect()
        );
        assert!(screen.warnings.is_empty());
    }
    for path in ["filter-ran", "process-ran", "fsmonitor-ran"] {
        assert!(
            !lab.repo.join(path).exists(),
            "inspection executed a repository command"
        );
    }
}

#[test]
fn shared_changes_use_registered_agent_checkouts_instead_of_the_code_cwd() {
    let lab = Lab::new();
    shared_base(&lab);
    let linked = lab.root.path().join("linked-agent-checkout");
    lab.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "session",
        linked.to_str().unwrap(),
        "main",
    ]);
    fs::write(linked.join("memory/staged.md"), b"PRIVATE-LINKED-CONTENT\n").unwrap();
    let mut code = lab.command("git");
    code.args(["init", "--quiet", "--initial-branch=main"])
        .current_dir(&lab.work);
    success(code.output().unwrap());
    fs::write(lab.work.join("AGENTS.md"), b"PRIVATE-CODE-CONTENT\n").unwrap();
    fs::create_dir(lab.work.join("memory")).unwrap();
    fs::write(lab.work.join("memory/code-only.md"), b"code-only\n").unwrap();
    for screen in lab.screens() {
        assert_eq!(
            shared_rows(&screen),
            ["local/qa@session\tmemory/staged.md\tunchanged\tmodified (raw bytes)".to_owned()]
                .into_iter()
                .collect()
        );
        assert!(
            !screen
                .lines
                .iter()
                .any(|line| line.contains("PRIVATE-") || line.contains("code-only.md"))
        );
    }
}

fn nested_shared_checkout(lab: &Lab) -> PathBuf {
    lab.git(&["commit", "--allow-empty", "--quiet", "-m", "empty main"]);
    let checkout = lab.repo.join("nested-session");
    success(
        lab.command("git")
            .current_dir(&lab.repo)
            .args(["worktree", "add", "--quiet", "-b", "session"])
            .arg(&checkout)
            .arg("main")
            .output()
            .unwrap(),
    );
    fs::write(checkout.join("AGENTS.md"), b"shared baseline\n").unwrap();
    for args in [
        vec!["add", "--", "AGENTS.md"],
        vec!["commit", "--quiet", "-m", "session shared files"],
    ] {
        success(
            lab.command("git")
                .current_dir(&checkout)
                .args(args)
                .output()
                .unwrap(),
        );
    }
    fs::write(checkout.join("AGENTS.md"), b"PRIVATE-NESTED-CONTENT\n").unwrap();
    assert!(!lab.repo.join("AGENTS.md").exists());
    assert!(lab.git(&["ls-tree", "--name-only", "main"]).is_empty());
    checkout
}

fn assert_nested_shared_checkout(lab: &Lab, available: bool) {
    for screen in lab.screens() {
        let expected = if available {
            "local/qa@session\tAGENTS.md\tunchanged\tmodified (raw bytes)"
        } else {
            "local/qa@session\t—\tunavailable\tunavailable: incomplete or changing evidence"
        };
        assert_eq!(
            shared_rows(&screen),
            [expected.to_owned()].into_iter().collect()
        );
        assert_eq!(
            screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete")),
            !available
        );
        if let Some(status) = screen.status {
            assert_eq!(status["shared_files"]["incomplete"], !available);
        }
    }
}

#[test]
fn an_invalid_primary_carrier_does_not_borrow_ancestor_shared_files_or_transactions() {
    let lab = Lab::new();
    shared_base(&lab);
    let head = lab.git(&["rev-parse", "HEAD"]);
    let lock = serde_json::to_vec(&serde_json::json!({
        "target":"main", "source":"peer/source@topic", "base":"",
        "target_head":head, "source_head":head
    }))
    .unwrap();
    fs::write(lab.repo.join(".git/AGIT_MERGE_TX"), &lock).unwrap();
    fs::write(lab.repo.join("AGENTS.md"), b"PRIVATE-AGENT-CONTENT\n").unwrap();
    let check = |available| {
        for screen in lab.screens() {
            if available {
                assert_eq!(
                    shared_rows(&screen),
                    ["local/qa@main\tAGENTS.md\tunchanged\tmodified (raw bytes)".to_owned()]
                        .into_iter()
                        .collect()
                );
                assert!(screen.lines.contains(
                    &"local/qa@main\tpeer/source@topic\topen; 0 picks; summary missing".to_owned()
                ));
            } else {
                assert!(shared_rows(&screen).is_empty());
                assert!(
                    screen.lines.contains(
                        &"local/qa\t—\tunavailable\tunavailable: incomplete or changing evidence"
                            .to_owned()
                    )
                );
                assert!(
                    screen.lines.contains(
                        &"local/qa\t—\tunavailable: transaction missing, changed, or unreadable"
                            .to_owned()
                    )
                );
                assert!(
                    !screen
                        .lines
                        .iter()
                        .any(|line| line.contains("peer/source@topic"))
                );
            }
            if let Some(status) = screen.status {
                assert_eq!(status["shared_files"]["incomplete"], !available);
                assert_eq!(status["merge_transactions"]["incomplete"], !available);
                assert_eq!(status["shared_files"]["items"].as_array().unwrap().len(), 1);
                assert_eq!(
                    status["merge_transactions"]["items"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
            }
        }
    };
    check(true);
    let carrier = lab.repo.join(".git");
    let parent = lab.repo.parent().unwrap();
    let ancestor_carrier = parent.join(".git");
    fs::rename(&carrier, &ancestor_carrier).unwrap();
    fs::create_dir(&carrier).unwrap();
    fs::write(parent.join("AGENTS.md"), b"PRIVATE-ANCESTOR-CONTENT\n").unwrap();
    assert_eq!(
        PathBuf::from(lab.git(&["rev-parse", "--show-toplevel"]))
            .canonicalize()
            .unwrap(),
        parent.canonicalize().unwrap()
    );
    check(false);
    fs::remove_file(ancestor_carrier.join("AGIT_MERGE_TX")).unwrap();
    check(false);
    fs::write(ancestor_carrier.join("AGIT_MERGE_TX"), &lock).unwrap();
    fs::remove_dir(&carrier).unwrap();
    fs::rename(&ancestor_carrier, &carrier).unwrap();
    fs::remove_file(parent.join("AGENTS.md")).unwrap();
    check(true);
}

#[test]
fn a_missing_nested_checkout_pointer_does_not_report_the_parent_as_complete() {
    let lab = Lab::new();
    let checkout = nested_shared_checkout(&lab);
    let pointer = checkout.join(".git");
    let bytes = fs::read(&pointer).unwrap();
    assert_nested_shared_checkout(&lab, true);
    fs::remove_file(&pointer).unwrap();
    assert_nested_shared_checkout(&lab, false);
    fs::create_dir(&pointer).unwrap();
    assert_nested_shared_checkout(&lab, false);
    fs::remove_dir(&pointer).unwrap();
    fs::write(&pointer, bytes).unwrap();
    assert_nested_shared_checkout(&lab, true);
}

#[test]
fn a_checkout_pointer_must_name_its_own_registration_within_the_common_store() {
    let lab = Lab::new();
    let checkout = nested_shared_checkout(&lab);
    let sibling = lab.root.path().join("sibling");
    success(
        lab.command("git")
            .current_dir(&lab.repo)
            .args(["worktree", "add", "--quiet", "-b", "sibling"])
            .arg(&sibling)
            .arg("main")
            .output()
            .unwrap(),
    );
    let pointer = checkout.join(".git");
    let bytes = fs::read(&pointer).unwrap();
    fs::write(&pointer, fs::read(sibling.join(".git")).unwrap()).unwrap();
    assert_nested_shared_checkout(&lab, false);
    #[cfg(unix)]
    {
        fs::remove_file(&pointer).unwrap();
        std::os::unix::fs::symlink(sibling.join(".git"), &pointer).unwrap();
        assert_nested_shared_checkout(&lab, false);
        fs::remove_file(&pointer).unwrap();
    }
    fs::write(&pointer, bytes).unwrap();
    assert_nested_shared_checkout(&lab, true);
}

#[test]
fn duplicate_checkout_backlinks_do_not_attribute_one_worktree_to_two_branches() {
    let lab = Lab::new();
    let checkout = nested_shared_checkout(&lab);
    let sibling = lab.root.path().join("sibling");
    success(
        lab.command("git")
            .current_dir(&lab.repo)
            .args(["worktree", "add", "--quiet", "-b", "sibling"])
            .arg(&sibling)
            .arg("main")
            .output()
            .unwrap(),
    );
    let git_dir = success(
        lab.command("git")
            .current_dir(&sibling)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .unwrap(),
    );
    let backlink = Path::new(&git_dir).join("gitdir");
    let original = fs::read(&backlink).unwrap();
    let toplevel = success(
        lab.command("git")
            .current_dir(&checkout)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap(),
    );
    fs::write(&backlink, format!("{toplevel}/.git\n")).unwrap();
    for screen in lab.screens() {
        assert!(shared_rows(&screen).is_empty());
        assert!(
            screen.lines.iter().any(|line| line
                == "local/qa\t—\tunavailable\tunavailable: incomplete or changing evidence")
        );
        assert!(
            screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete"))
        );
        if let Some(status) = screen.status {
            assert_eq!(status["shared_files"]["incomplete"], true);
            let rows = status["shared_files"]["items"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0]["branch"].is_null());
            assert!(rows[0]["checkout"].is_null());
        }
    }
    fs::write(&backlink, original).unwrap();
    assert_nested_shared_checkout(&lab, true);
}

#[test]
fn unborn_shared_files_and_removed_parent_directories_remain_visible() {
    let lab = Lab::new();
    fs::write(lab.repo.join("AGENTS.md"), b"untracked\n").unwrap();
    for screen in lab.screens() {
        assert_eq!(
            shared_rows(&screen),
            ["local/qa@main\tAGENTS.md\tunchanged\tuntracked".to_owned()]
                .into_iter()
                .collect()
        );
    }
    shared_base(&lab);
    fs::remove_dir_all(lab.repo.join("memory")).unwrap();
    for screen in lab.screens() {
        assert_eq!(
            shared_rows(&screen),
            [
                "local/qa@main\tmemory/deleted.md\tunchanged\tdeleted".to_owned(),
                "local/qa@main\tmemory/staged.md\tunchanged\tdeleted".to_owned(),
            ]
            .into_iter()
            .collect()
        );
        assert!(screen.warnings.is_empty());
    }
}

#[test]
fn unavailable_local_bytes_preserve_known_staged_facts_and_bound_output() {
    let lab = Lab::new();
    shared_base(&lab);
    fs::write(lab.repo.join("AGENTS.md"), b"new staged bytes\n").unwrap();
    lab.git(&["add", "--", "AGENTS.md"]);
    fs::write(lab.repo.join("AGENTS.md"), vec![b'X'; 1024 * 1024 + 1]).unwrap();
    fs::remove_file(lab.repo.join("memory/deleted.md")).unwrap();
    fs::create_dir(lab.repo.join("memory/deleted.md")).unwrap();
    for screen in lab.screens() {
        let rows = shared_rows(&screen);
        assert!(
            rows.iter()
                .any(|row| row.starts_with("local/qa@main\tAGENTS.md\tmodified\tunavailable:"))
        );
        assert!(rows.iter().any(|row| {
            row.starts_with("local/qa@main\tmemory/deleted.md\tunchanged\tunavailable:")
        }));
        assert!(
            screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete"))
        );
    }
    fs::write(lab.repo.join("AGENTS.md"), b"new staged bytes\n").unwrap();
    fs::remove_dir(lab.repo.join("memory/deleted.md")).unwrap();
    fs::write(lab.repo.join("memory/deleted.md"), b"shared baseline\n").unwrap();
    for index in 0..130 {
        fs::write(
            lab.repo.join(format!("skills/new-{index:03}.md")),
            b"bounded\n",
        )
        .unwrap();
    }
    for screen in lab.screens() {
        assert_eq!(shared_rows(&screen).len(), 128);
        assert!(
            screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete"))
        );
    }
}

#[cfg(unix)]
#[test]
fn shared_symlink_parents_and_carriers_do_not_supply_local_bytes() {
    let lab = Lab::new();
    shared_base(&lab);
    let foreign = lab.root.path().join("foreign-shared");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("staged.md"), b"PRIVATE-FOREIGN-CONTENT\n").unwrap();
    fs::write(foreign.join("deleted.md"), b"PRIVATE-FOREIGN-CONTENT\n").unwrap();
    fs::remove_dir_all(lab.repo.join("memory")).unwrap();
    std::os::unix::fs::symlink(&foreign, lab.repo.join("memory")).unwrap();
    fs::remove_file(lab.repo.join("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink(foreign.join("staged.md"), lab.repo.join("AGENTS.md")).unwrap();
    for screen in lab.screens() {
        assert!(
            shared_rows(&screen)
                .iter()
                .filter(|row| row.contains("unavailable:"))
                .count()
                >= 3
        );
        assert!(
            !screen
                .lines
                .iter()
                .any(|line| line.contains("PRIVATE-FOREIGN-CONTENT"))
        );
    }
}

#[test]
fn shared_conflicts_keep_the_staged_fact_without_guessing_a_local_resolution() {
    let lab = Lab::new();
    shared_base(&lab);
    let blob = lab.git_input(&["hash-object", "-w", "--stdin"], b"synthetic conflict\n");
    let mut entries = format!("0 {}\tAGENTS.md\n", "0".repeat(40));
    for stage in 1..=3 {
        entries.push_str(&format!("100644 {blob} {stage}\tAGENTS.md\n"));
    }
    lab.git_input(&["update-index", "--index-info"], entries.as_bytes());
    for screen in lab.screens() {
        assert_eq!(
            shared_rows(&screen),
            ["local/qa@main\tAGENTS.md\tconflicted\tunavailable: unmerged index".to_owned()]
                .into_iter()
                .collect()
        );
        assert!(
            screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete"))
        );
    }
}

#[cfg(unix)]
#[test]
fn shared_named_pipes_are_unavailable_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    let lab = Lab::new();
    shared_base(&lab);
    let path = lab.repo.join("AGENTS.md");
    fs::remove_file(&path).unwrap();
    let before = snapshot(lab.root.path());
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    // The inventory fixture reads ordinary files; inspect this carrier in a child with a deadline.
    let mut child = lab
        .command(env!("CARGO_BIN_EXE_agit"))
        .current_dir(&lab.work)
        .args(["--json", "--json-version", "2", "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("shared-file inspection waited on a named pipe");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["result"]["format"], "json");
    let shared = &value["result"]["value"]["shared_files"];
    assert_eq!(shared["incomplete"], true);
    assert!(shared["items"].as_array().unwrap().iter().any(|change| {
        change["repo"] == "local/qa"
            && change["branch"] == "main"
            && change["path"] == "AGENTS.md"
            && change["staged"] == "unchanged"
            && change["local_bytes"]
                .as_str()
                .unwrap()
                .starts_with("unavailable:")
    }));
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
    fs::remove_file(&path).unwrap();
    assert_eq!(snapshot(lab.root.path()), before);
    assert!(!lab.home.join("store").exists());
    assert_eq!(
        lab.hub.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn typed_shared_and_merge_observations_preserve_exact_identity_in_the_same_report() {
    let lab = Lab::new();
    shared_base(&lab);
    let head = lab.git(&["rev-parse", "HEAD"]);
    let relative = "memory/with space.md";
    fs::write(lab.repo.join(relative), b"PRIVATE-UNTRACKED-CONTENT\n").unwrap();
    let source = format!("peer/source@{}", "topic".repeat(40));
    fs::write(
        lab.repo.join(".git/AGIT_MERGE_TX"),
        serde_json::to_vec(&serde_json::json!({
            "target":"main", "source":source, "base":"", "target_head":head, "source_head":head,
            "picked":["peer/source@topic#1"], "summary":"PRIVATE-SUMMARY-CONTENT"
        }))
        .unwrap(),
    )
    .unwrap();
    for screen in lab.screens() {
        assert!(
            shared_rows(&screen)
                .contains(&format!("local/qa@main\t{relative}\tunchanged\tuntracked"))
        );
        if let Some(status) = screen.status {
            let changes = status["shared_files"]["items"].as_array().unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0]["repo"], "local/qa");
            assert_eq!(changes[0]["branch"], "main");
            assert_eq!(changes[0]["path"], relative);
            assert_eq!(
                Path::new(changes[0]["checkout"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                lab.repo.canonicalize().unwrap()
            );
            assert_eq!(changes[0]["staged"], "unchanged");
            assert_eq!(changes[0]["local_bytes"], "untracked");
            assert_eq!(status["shared_files"]["incomplete"], false);
            let transactions = status["merge_transactions"]["items"].as_array().unwrap();
            assert_eq!(transactions.len(), 1);
            assert_eq!(
                transactions[0]["source"], source,
                "JSON must not use truncated terminal cells"
            );
            assert_eq!(transactions[0]["target_head"], head);
            assert_eq!(transactions[0]["summary_ready"], true);
        } else {
            assert!(
                screen
                    .lines
                    .iter()
                    .any(|line| line.contains(&display_cell(&source)))
            );
            assert!(!screen.lines.iter().any(|line| line.contains(&source)));
        }
    }
}

#[cfg(unix)]
#[test]
fn typed_shared_paths_keep_control_bytes_without_terminal_injection() {
    let lab = Lab::new();
    shared_base(&lab);
    let relative = "memory/name\nwith\ttabs.md";
    fs::write(lab.repo.join(relative), b"PRIVATE-PATH-CONTENT\n").unwrap();
    for screen in lab.screens() {
        if let Some(status) = screen.status {
            assert_eq!(status["shared_files"]["items"][0]["path"], relative);
            assert_eq!(
                status["shared_files"]["items"][0]["local_bytes"],
                "untracked"
            );
        } else {
            assert!(shared_rows(&screen).contains(&format!(
                "local/qa@main\t{}\tunchanged\tuntracked",
                display_cell(relative)
            )));
            assert!(
                !screen
                    .lines
                    .iter()
                    .any(|line| line.contains("with\ttabs.md"))
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_checkout_identity_is_unavailable_without_breaking_the_json_report() {
    use std::os::unix::ffi::OsStringExt;
    let lab = Lab::new();
    shared_base(&lab);
    let checkout = lab
        .root
        .path()
        .join(std::ffi::OsString::from_vec(b"checkout-\xff".to_vec()));
    success(
        lab.command("git")
            .current_dir(&lab.repo)
            .args(["worktree", "add", "--quiet", "-b", "session"])
            .arg(&checkout)
            .arg("main")
            .output()
            .unwrap(),
    );
    fs::write(
        checkout.join("memory/staged.md"),
        b"PRIVATE-CHECKOUT-CONTENT\n",
    )
    .unwrap();
    for screen in lab.screens() {
        assert!(
            screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete"))
        );
        if let Some(status) = screen.status {
            assert_eq!(status["shared_files"]["incomplete"], true);
            let rows = status["shared_files"]["items"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["repo"], "local/qa");
            assert!(rows[0]["checkout"].is_null());
            assert!(rows[0]["path"].is_null());
            assert_eq!(rows[0]["staged"], "unavailable");
        } else {
            assert!(
                screen
                    .lines
                    .iter()
                    .any(|line| line.starts_with("local/qa\t—\tunavailable\tunavailable:"))
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn shared_fifo_index_is_unavailable_and_git_is_reaped_without_touching_evidence() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::process::CommandExt;
    let lab = Lab::new();
    shared_base(&lab);
    let index = lab.repo.join(".git/index");
    let retained = lab.repo.join(".git/retained-index");
    fs::rename(&index, &retained).unwrap();
    let before = snapshot(lab.root.path());
    for version in [None, Some("1"), Some("2")] {
        let name = std::ffi::CString::new(index.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let identity = fs::symlink_metadata(&index).unwrap();
        let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
        command.current_dir(&lab.work).process_group(0);
        if let Some(version) = version {
            command.args(["--json", "--json-version", version]);
        }
        let mut child = command
            .arg("status")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
                let _ = child.wait();
                panic!("shared-file inspection waited on the index FIFO");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        if let Some(version) = version {
            assert!(output.stderr.is_empty(), "{output:?}");
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(document["schema_version"], version.parse::<u64>().unwrap());
            let shared = &document["result"]["value"]["shared_files"];
            assert_eq!(shared["incomplete"], true);
            assert!(shared["items"].as_array().unwrap().iter().any(|row| {
                row["repo"] == "local/qa"
                    && row["branch"] == "main"
                    && row["path"].is_null()
                    && row["staged"] == "unavailable"
                    && row["local_bytes"]
                        .as_str()
                        .unwrap()
                        .starts_with("unavailable:")
            }));
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains("unavailable"));
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("shared-file inspection is incomplete")
            );
        }
        let observed = fs::symlink_metadata(&index).unwrap();
        assert!(observed.file_type().is_fifo());
        assert_eq!(
            (
                observed.dev(),
                observed.ino(),
                observed.mode(),
                observed.len()
            ),
            (
                identity.dev(),
                identity.ino(),
                identity.mode(),
                identity.len()
            )
        );
        fs::remove_file(&index).unwrap();
        assert_eq!(snapshot(lab.root.path()), before);
        assert!(!lab.home.join("store").exists());
        assert_eq!(
            lab.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    fs::rename(retained, index).unwrap();
    for screen in lab.screens() {
        assert!(shared_rows(&screen).is_empty(), "{screen:?}");
        assert!(
            !screen
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-file inspection is incomplete"))
        );
    }
}
