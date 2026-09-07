use agit::domain::meta::{self, LayoutVersion, Meta};
use agit::domain::repo::Repo;
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn status(home: &Path, hub: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("AGIT_") {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .current_dir(home.parent().unwrap())
        .env("HOME", home.parent().unwrap())
        .env("USERPROFILE", home.parent().unwrap())
        .env("CODEX_HOME", home.parent().unwrap().join("codex"))
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", hub)
        .env("AGIT_TUI", "0")
        .env_remove("CI")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .output()
        .unwrap()
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_symlink() {
                out.insert(
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    format!("symlink:{}", fs::read_link(entry.path()).unwrap().display())
                        .into_bytes(),
                );
            } else if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), out);
            } else {
                out.insert(
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn status_arguments() -> Vec<Vec<&'static str>> {
    let mut arguments = vec![vec!["status"]];
    if cfg!(unix) {
        arguments.push(vec!["--json", "status"]);
    }
    arguments
}

#[test]
fn fresh_status_neither_contacts_the_hub_nor_creates_storage() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("absent");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    for args in status_arguments() {
        let output = status(&home, &hub, &args);
        assert_success(&output);
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["exit_code"], 0);
        }
        assert!(!home.exists(), "status created storage");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "status contacted the hub"
        );
    }
}

#[test]
fn default_status_does_not_open_runtime_wal_indexes() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    fs::create_dir_all(home.join("store")).unwrap();
    let codex = fixture.path().join("codex");
    fs::create_dir_all(&codex).unwrap();
    let database = codex.join("state_5.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .execute("CREATE TABLE threads(id TEXT)", [])
        .unwrap();
    drop(connection);
    assert!(!codex.join("state_5.sqlite-wal").exists());
    let before = files(fixture.path());
    for args in status_arguments() {
        assert_success(&status(&home, "http://127.0.0.1:1", &args));
        assert_eq!(files(fixture.path()), before);
    }
}

#[test]
fn status_displays_legacy_history_without_migrating_it() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let repo = Repo::init(&home.join("repos/owner/history")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    let mut snapshot = Meta::new_file_line();
    snapshot.layout = LayoutVersion::V0;
    meta::write(repo.root(), &snapshot).unwrap();
    repo.add_all().unwrap();
    repo.commit("legacy history").unwrap();
    fs::create_dir_all(home.join("store")).unwrap();
    let before = files(&home);
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("owner/history"));
    assert_eq!(files(&home), before);
    assert_eq!(
        meta::read_at_ref(&repo, "HEAD").unwrap().layout,
        LayoutVersion::V0
    );
}

#[test]
fn status_refuses_pending_recovery_without_consuming_evidence() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let recovery = home.join("layout-v1-recovery");
    fs::create_dir_all(&recovery).unwrap();
    fs::write(recovery.join("pending-test"), b"pending").unwrap();
    let before = files(&home);
    for args in status_arguments() {
        let output = status(&home, "http://127.0.0.1:1", &args);
        assert_eq!(output.status.code(), Some(4));
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains("pending recovery"));
        assert!(combined.contains("original AgentGit store"));
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["exit_code"], 4);
        }
        assert_eq!(files(&home), before);
    }
}

#[test]
fn status_refuses_a_pre_marker_checkout_journal_without_acquiring_locks() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let repo = Repo::init(&home.join("repos/owner/history")).unwrap();
    fs::write(
        repo.root().join(".git/agit-checkout-transaction.json"),
        b"unreadable pending journal",
    )
    .unwrap();
    let before = files(&home);
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("pending recovery"));
    assert_eq!(files(&home), before);
}

fn committed_repo(path: &Path) -> Repo {
    let repo = Repo::init(path).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("local history").unwrap();
    repo
}

fn assert_visible_checkout_recovery_guard(fixture: &Path, home: &Path, repo: &Repo) {
    fs::create_dir_all(home.join("store")).unwrap();
    for completed in [false, true] {
        if completed {
            fs::write(home.join("layout-v1.complete"), b"1\n").unwrap();
        }
        let clean = files(fixture);
        assert_success(&status(home, "http://127.0.0.1:1", &["status"]));
        assert_eq!(files(fixture), clean);
        let git_path = PathBuf::from(
            repo.git(&["rev-parse", "--git-path", "agit-checkout-transaction.json"])
                .unwrap(),
        );
        let journal = if git_path.is_absolute() {
            git_path
        } else {
            repo.root().join(git_path)
        };
        fs::write(&journal, b"pending journal").unwrap();
        let pending = files(fixture);
        let output = status(home, "http://127.0.0.1:1", &["status"]);
        assert_eq!(output.status.code(), Some(4));
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending recovery"));
        assert_eq!(files(fixture), pending);
        fs::remove_file(journal).unwrap();
    }
}

#[test]
fn status_checks_visible_worktrees_even_after_migration_completed() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let main = committed_repo(&fixture.path().join("original"));
    let linked = home.join("repos/owner/linked");
    fs::create_dir_all(linked.parent().unwrap()).unwrap();
    main.git(&[
        "worktree",
        "add",
        "-b",
        "inspection",
        linked.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    assert_visible_checkout_recovery_guard(fixture.path(), &home, &Repo::at(linked));
}

#[cfg(unix)]
#[test]
fn status_checks_visible_repository_aliases_even_after_migration_completed() {
    for alias_owner in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let original = committed_repo(&fixture.path().join("original/owner/history"));
        let alias = if alias_owner {
            home.join("repos/owner")
        } else {
            home.join("repos/owner/history")
        };
        let source = if alias_owner {
            original.root().parent().unwrap()
        } else {
            original.root()
        };
        fs::create_dir_all(alias.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(source, &alias).unwrap();
        assert_visible_checkout_recovery_guard(fixture.path(), &home, &original);
    }
}
