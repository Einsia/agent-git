#[path = "support/worktrees.rs"]
mod worktrees;

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
        assert!(combined.contains("an earlier agit command stopped before it finished"));
        assert!(combined.contains("`agit config --list` finishes it"));
        assert!(!combined.contains("agit save"), "{combined}");
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["exit_code"], 4);
        }
        assert_eq!(files(&home), before);
    }
}

/// Evidence a running command still holds is not a stopped command: status says to wait for it
/// rather than naming a step that would contend with it.
#[test]
fn status_waits_for_a_command_that_is_still_running() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let recovery = home.join("layout-v1-recovery");
    fs::create_dir_all(&recovery).unwrap();
    fs::write(
        recovery.join("pending-clone-history-1-0"),
        b"owner/history\n",
    )
    .unwrap();
    let lock = fs::File::create(home.join("layout-v1.lock")).unwrap();
    lock.lock_shared().unwrap();
    let before = files(&home);
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("another agit command is still running"),
        "{stderr}"
    );
    assert!(
        stderr.contains("run this again when it has finished"),
        "{stderr}"
    );
    assert_eq!(files(&home), before);
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

/// A conversation that waits for the privacy check before its first landing has no session link
/// yet, and `agit status` still shows the wait: what ends it, or that the next attempt tries
/// again. A status that listed only linked waits would leave such a conversation stalled unseen.
#[test]
fn a_wait_before_a_conversations_first_landing_shows_in_status() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("agit");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("privacy-pending.json"),
        serde_json::json!({
            "native-slow-0001": {"reason": "deadline", "turns": 1, "since": 0},
            "native-rule-0002": {"reason": "findings_cap", "turns": 1, "since": 0},
        })
        .to_string(),
    )
    .unwrap();
    let output = status(&home, "http://127.0.0.1:9", &["status"]);
    assert_success(&output);
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("a conversation not saved yet (")
            && said.contains("privacy check pending (deadline)")
            && said.contains("; the next attempt tries again"),
        "{said}"
    );
    assert!(
        said.contains("privacy check pending (findings_cap)")
            && said.contains("`agit scan` shows where they are"),
        "{said}"
    );
}

/// A repository moved away from its linked worktree, with a fresh one made in its old place: the
/// worktree's `.git` names a registration the fresh repository doesn't have, so Git can't say
/// where that worktree's storage is, while the moved repository's own checkout reads fine.
fn moved_from_its_worktree(home: &Path) {
    let old = home.join("repos/owner/old");
    let repo = committed_repo(&old);
    let worktree = home.join("worktrees/owner/old/main");
    fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        worktree.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    fs::rename(&old, home.join("repos/owner/moved")).unwrap();
    committed_repo(&old);
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Status in a folder whose conversations don't go to a repository Git can't inspect goes on, and
/// says which repository it leaves out, in which worktree, and how to see Git's answer. A guard
/// that stopped on every repository would block status everywhere over one it doesn't read.
#[test]
fn status_goes_on_past_a_repository_git_cannot_inspect_that_this_folder_does_not_use() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    for args in status_arguments() {
        let output = status(&home, "http://127.0.0.1:1", &args);
        assert_success(&output);
        let said = said(&output);
        assert!(
            said.contains("Git couldn't tell where owner/moved's storage is, in its worktree ")
                && said.contains("worktrees/owner/old/main: ")
                && said.contains(
                    "; nothing in this folder goes to owner/moved, so this leaves it out"
                )
                && said.contains("see Git's answer in full: `git -C ")
                && said
                    .contains(" rev-parse --git-common-dir`; `agit doctor` checks the repository"),
            "{args:?}: {said}"
        );
    }
}

/// A worktree Git can't inspect doesn't hide pending recovery in another worktree of the same
/// repository: status stops on the recovery, whichever of the two Git lists first. A check that
/// gave up on the repository at its first unreadable worktree would leave it out and go on,
/// reading an interrupted checkout as settled.
#[test]
fn pending_recovery_stops_status_even_after_a_worktree_git_cannot_inspect() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    let repo = committed_repo(&home.join("repos/owner/both"));
    for name in ["one", "two"] {
        let path = home.join("worktrees/owner/both").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        repo.git(&[
            "worktree",
            "add",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ])
        .unwrap();
    }
    // The first worktree Git lists is the one it can't inspect; the second holds the journal.
    let listed: Vec<PathBuf> = repo
        .git(&["worktree", "list", "--porcelain"])
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .skip(1)
        .map(PathBuf::from)
        .collect();
    assert_eq!(listed.len(), 2, "{listed:?}");
    fs::write(
        listed[0].join(".git"),
        "gitdir: /nonexistent/agit-fixture/.git/worktrees/gone\n",
    )
    .unwrap();
    let pending = Repo::at(&listed[1]);
    let git_path = PathBuf::from(
        pending
            .git(&["rev-parse", "--git-path", "agit-checkout-transaction.json"])
            .unwrap(),
    );
    let journal = if git_path.is_absolute() {
        git_path
    } else {
        listed[1].join(git_path)
    };
    fs::write(&journal, b"pending journal").unwrap();
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    let said = said(&output);
    assert_eq!(output.status.code(), Some(4), "{said}");
    assert!(said.contains("pending recovery"), "{said}");
    assert!(!said.contains("so this leaves it out"), "{said}");
}

/// Status in a folder bound to a repository Git can't inspect stops and names it: what status
/// reads there can't be shown as settled.
#[test]
fn status_stops_on_a_repository_git_cannot_inspect_that_this_folder_uses() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    let binding = agit::domain::workspace::Workspace {
        dir: fixture.path().to_string_lossy().into_owned(),
        repo: "owner/moved".into(),
    };
    let path = home.join("workspaces").join(
        agit::domain::workspace::path_for(fixture.path())
            .unwrap()
            .file_name()
            .unwrap(),
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_string(&binding).unwrap()).unwrap();
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    let said = said(&output);
    assert_eq!(output.status.code(), Some(4), "{said}");
    assert!(
        said.contains(
            "local storage preparation failed: Git couldn't tell where owner/moved's storage is, in its worktree "
        ) && said.contains("see Git's answer in full: `git -C "),
        "{said}"
    );
    assert!(!said.contains("so this leaves it out"), "{said}");
}

/// Once left out at startup, the repository is not read later in the same command: status's
/// table of repositories lists the fresh one and not the moved one. Without the mark, the table
/// reads the moved repository's own checkout, which Git can read, and shows it as settled.
#[test]
fn a_repository_left_out_at_startup_is_not_read_later_in_the_command() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    assert_success(&output);
    let table = String::from_utf8_lossy(&output.stdout);
    assert!(table.contains("owner/old"), "{table}");
    assert!(!table.contains("owner/moved"), "{table}");
}

/// The worktree repair refuses, before it touches any link, a stale registration whose place the
/// repository of that name took back for a worktree of its own, and a locked worktree. Either way
/// every worktree's link stays as it was and nothing moves. A repair that relinked first and moved
/// second would take another repository's worktree, or leave one linked to this repository from
/// another repository's place when the move failed.
#[test]
fn the_worktree_repair_refuses_a_reused_place_and_a_locked_worktree_untouched() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    let place = home.join("worktrees/owner/old/main");
    fs::remove_dir_all(&place).unwrap();
    Repo::at(home.join("repos/owner/old"))
        .git(&[
            "worktree",
            "add",
            "--detach",
            place.to_str().unwrap(),
            "HEAD",
        ])
        .unwrap();
    let link = fs::read(place.join(".git")).unwrap();
    let output = status(
        &home,
        "http://127.0.0.1:1",
        &["doctor", "--repair-worktrees", "owner/moved"],
    );
    let reused = said(&output);
    assert_eq!(output.status.code(), Some(4), "{reused}");
    assert!(
        reused.contains(
            "it is owner/old's worktree now, and owner/moved's record of it is out of date; nothing was moved"
        ),
        "{reused}"
    );
    assert_eq!(fs::read(place.join(".git")).unwrap(), link, "{reused}");
    assert!(!home.join("worktrees/owner/moved").exists(), "{reused}");

    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    let place = home.join("worktrees/owner/old/main");
    Repo::at(home.join("repos/owner/moved"))
        .git(&["worktree", "lock", place.to_str().unwrap()])
        .unwrap();
    let link = fs::read(place.join(".git")).unwrap();
    let output = status(
        &home,
        "http://127.0.0.1:1",
        &["doctor", "--repair-worktrees", "owner/moved"],
    );
    let locked = said(&output);
    assert_eq!(output.status.code(), Some(4), "{locked}");
    assert!(
        locked.contains("it is locked; `git worktree unlock ")
            && locked.contains("if nothing uses it; nothing was moved"),
        "{locked}"
    );
    assert_eq!(fs::read(place.join(".git")).unwrap(), link, "{locked}");
    assert!(!home.join("worktrees/owner/moved").exists(), "{locked}");
}

/// The worktree repair writes only the link of the worktree it moves. A second registration of
/// the same repository, outside agit's worktrees, whose path the original-name repository has
/// since used for a worktree of its own, keeps that repository's link; and a file standing where a
/// folder on the way to the target belongs stops the repair before any link changes. Git's own
/// repair rewrites every registration's link, so the second worktree would open the copy.
#[test]
fn the_worktree_repair_touches_only_the_worktree_it_moves() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    let side = fixture.path().join("elsewhere/side");
    fs::create_dir_all(side.parent().unwrap()).unwrap();
    Repo::at(home.join("repos/owner/moved"))
        .git(&[
            "worktree",
            "add",
            "--detach",
            side.to_str().unwrap(),
            "HEAD",
        ])
        .unwrap();
    fs::remove_dir_all(&side).unwrap();
    Repo::at(home.join("repos/owner/old"))
        .git(&[
            "worktree",
            "add",
            "--detach",
            side.to_str().unwrap(),
            "HEAD",
        ])
        .unwrap();
    let side_link = fs::read(side.join(".git")).unwrap();

    let blocked = home.join("worktrees/owner/moved");
    fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    fs::write(&blocked, b"a file where a folder belongs").unwrap();
    let place = home.join("worktrees/owner/old/main");
    let link = fs::read(place.join(".git")).unwrap();
    let output = status(
        &home,
        "http://127.0.0.1:1",
        &["doctor", "--repair-worktrees", "owner/moved"],
    );
    let refused = said(&output);
    assert_eq!(output.status.code(), Some(4), "{refused}");
    assert!(
        refused.contains("worktrees/owner/moved; nothing was moved"),
        "{refused}"
    );
    assert_eq!(fs::read(place.join(".git")).unwrap(), link, "{refused}");

    fs::remove_file(&blocked).unwrap();
    let output = status(
        &home,
        "http://127.0.0.1:1",
        &["doctor", "--repair-worktrees", "owner/moved"],
    );
    let repaired = said(&output);
    assert_success(&output);
    assert!(
        home.join("worktrees/owner/moved/main/.git").exists(),
        "{repaired}"
    );
    assert_eq!(
        fs::read(side.join(".git")).unwrap(),
        side_link,
        "{repaired}"
    );
}

/// `agit doctor` is never stopped by a repository Git can't inspect, and runs its other checks.
/// A worktree sitting in another repository's place is named with that place and the one command
/// that moves it into its own repository's place; after that command, the worktree opens there,
/// every worktree names its repository, and status no longer leaves the repository out. A repair
/// that rewrote the worktree's link where it stands would leave it naming the wrong repository.
#[test]
fn doctor_names_a_misplaced_worktree_and_its_repair_moves_it_home() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("home");
    moved_from_its_worktree(&home);
    for args in [vec!["doctor"], vec!["doctor", "--repo", "owner/moved"]] {
        let output = status(&home, "http://127.0.0.1:1", &args);
        let said = said(&output);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {said}");
        assert!(
            said.contains("owner/moved")
                && said.contains("its worktree ")
                && said.contains("worktrees/owner/old/main sits in owner/old's place, and Git can't reach its storage there; `agit doctor --repair-worktrees owner/moved` moves it to owner/moved's place"),
            "{args:?}: {said}"
        );
        assert!(
            said.contains("sign-in"),
            "{args:?}: the other checks ran: {said}"
        );
        assert!(!said.contains("local storage"), "{args:?}: {said}");
    }
    let repair = status(
        &home,
        "http://127.0.0.1:1",
        &["doctor", "--repair-worktrees", "owner/moved"],
    );
    let repaired = said(&repair);
    assert_success(&repair);
    assert!(
        repaired.contains("moved ") && repaired.contains("worktrees/owner/moved/main"),
        "{repaired}"
    );
    let moved = home.join("worktrees/owner/moved/main");
    assert!(Repo::at(&moved).git(&["status", "--porcelain"]).is_ok());
    assert!(!home.join("worktrees/owner/old").exists(), "{repaired}");
    worktrees::assert_each_names_its_repository(&home);
    let output = status(&home, "http://127.0.0.1:1", &["status"]);
    assert_success(&output);
    let after = said(&output);
    assert!(!after.contains("leaves it out"), "{after}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("owner/moved"),
        "{after}"
    );
}
