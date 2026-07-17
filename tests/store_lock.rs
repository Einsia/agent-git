//! The store lock, proven with real concurrent PROCESSES against one store.
//!
//! A store used to belong to one repo, so its writers were serialized by there only being one of them.
//! One store is now shared by every repo that tracks the agent (§6), which makes `snap`, `restore` and
//! the pairing record concurrent writers to ONE index and ONE HEAD **by design**.
//!
//! Threads would not prove this: the lock is a file, and the racing writers are separate `agit`
//! processes in separate repos. So each case re-executes THIS test binary as a child (the `helper_*`
//! tests below, inert unless their env var is set) and runs the real `agit::session::lock_store`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Set by a parent to turn a `helper_*` test from a no-op into a worker.
const STORE: &str = "AGIT_TEST_LOCK_STORE";
const NO_LOCK: &str = "AGIT_TEST_LOCK_DISABLE";

fn git(dir: &Path, args: &[&str]) -> String {
    // Per-invocation identity only. A `git config --global` here would clobber the developer's real
    // commit identity — it has happened in this project once already, and needed a filter-branch.
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run `n` copies of one helper test as real child processes, all against `store`, and wait for them.
fn race(helper: &str, store: &Path, n: usize, no_lock: bool) -> Vec<std::process::Output> {
    let exe = std::env::current_exe().expect("the test binary must locate itself to re-exec");
    let kids: Vec<_> = (0..n)
        .map(|_| {
            let mut c = Command::new(&exe);
            c.args([helper, "--exact", "--nocapture", "--test-threads=1"])
                .env(STORE, store)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if no_lock {
                c.env(NO_LOCK, "1");
            }
            c.spawn().expect("failed to spawn a competing writer")
        })
        .collect();
    kids.into_iter().map(|k| k.wait_with_output().unwrap()).collect()
}

fn store_from_env() -> Option<PathBuf> {
    std::env::var(STORE).ok().map(PathBuf::from)
}

// ─────────────────────────────── the helpers (children only) ───────────────────────────────

/// One read-modify-write of a shared counter, with the gap held open. Under the lock every writer's
/// increment survives; without it they all read the same value and the last one wins.
#[test]
fn helper_increments_a_counter() {
    let Some(store) = store_from_env() else { return };
    let _lock = (std::env::var(NO_LOCK).is_err()).then(|| agit::session::lock_store(&store).expect("lock"));

    let counter = store.join("counter");
    let n: u64 = std::fs::read_to_string(&counter).unwrap_or_default().trim().parse().unwrap_or(0);
    // The interleaving, forced rather than hoped for: without a lock every writer is inside this gap
    // together, so the race is certain instead of occasional.
    std::thread::sleep(std::time::Duration::from_millis(120));
    std::fs::write(&counter, format!("{}\n", n + 1)).unwrap();
}

/// A real store mutation: stage a file and commit it, the way `snap` does.
#[test]
fn helper_commits_a_session() {
    let Some(store) = store_from_env() else { return };
    let _lock = (std::env::var(NO_LOCK).is_err()).then(|| agit::session::lock_store(&store).expect("lock"));

    let id = std::process::id();
    let dir = store.join("sessions").join(format!("-code-repo-{id}")).join("claude-code");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.jsonl")), "{\"type\":\"user\"}\n").unwrap();
    git(&store, &["add", "-A"]);
    // --no-verify: a minted store installs the secret hooks, and agit's own metadata commits pass it.
    git(&store, &["commit", "-qm", &format!("snap from {id}"), "--no-verify"]);
}

// ─────────────────────────────────────── the cases ───────────────────────────────────────────

/// Mutual exclusion across processes, and the control that proves the lock is what provides it: the
/// SAME race without the lock must lose writes. A concurrency test that passes with the lock removed
/// has proven nothing.
#[test]
fn concurrent_writers_do_not_lose_each_others_writes() {
    const WRITERS: usize = 6;

    let unlocked = tempfile::tempdir().unwrap();
    let outs = race("helper_increments_a_counter", unlocked.path(), WRITERS, true);
    assert!(outs.iter().all(|o| o.status.success()), "the control's writers must run, just unsafely");
    let lost: u64 = std::fs::read_to_string(unlocked.path().join("counter")).unwrap().trim().parse().unwrap();
    assert!(
        lost < WRITERS as u64,
        "the control did NOT race, so this test cannot prove the lock does anything: {lost}/{WRITERS} survived"
    );

    let locked = tempfile::tempdir().unwrap();
    let outs = race("helper_increments_a_counter", locked.path(), WRITERS, false);
    for o in &outs {
        assert!(o.status.success(), "a writer failed: {}", String::from_utf8_lossy(&o.stderr));
    }
    let kept: u64 = std::fs::read_to_string(locked.path().join("counter")).unwrap().trim().parse().unwrap();
    assert_eq!(kept as usize, WRITERS, "the lock must serialize every writer: {kept}/{WRITERS} survived");
}

/// The case §11 names: "shared store has unlocked concurrent writers — restore/record/snap all write
/// one index+HEAD". Several repos snap ONE agent at the same moment; every commit must land, and the
/// store must still be a valid git repo afterwards.
#[test]
fn concurrent_snaps_into_one_shared_store_do_not_corrupt_it() {
    const REPOS: usize = 6;

    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    git(&store, &["init", "-q", "-b", "main", "."]);
    std::fs::write(store.join("agent.toml"), "aid = \"agt_test\"\n").unwrap();
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-qm", "mint", "--no-verify"]);

    let outs = race("helper_commits_a_session", &store, REPOS, false);
    for o in &outs {
        assert!(o.status.success(), "a snapping repo failed: {}", String::from_utf8_lossy(&o.stderr));
    }

    // Every writer's commit survived: none was lost to another's index.
    let commits = git(&store, &["log", "--oneline"]).lines().count();
    assert_eq!(commits, REPOS + 1, "commits were lost to a concurrent writer: {commits} of {}", REPOS + 1);

    // …and the store is still a git repo, not a pile of half-written objects.
    let fsck = Command::new("git").arg("-C").arg(&store).args(["fsck", "--strict"]).output().unwrap();
    assert!(fsck.status.success(), "the shared store is corrupt: {}", String::from_utf8_lossy(&fsck.stderr));

    // Each repo's sessions landed under its OWN env partition, and every one is still readable.
    let sessions = agit::commands::store_sessions(&store);
    assert_eq!(sessions.len(), REPOS, "a session was lost: {sessions:?}", sessions = sessions.len());
    let envs: std::collections::HashSet<_> = sessions.iter().filter_map(|s| s.env_slug.clone()).collect();
    assert_eq!(envs.len(), REPOS, "each repo must keep its own partition, not overwrite a shared one");

    // No lock is left behind to wedge the next writer.
    assert!(!store.join(".git/agit-store.lock").exists(), "the lock must not outlive its holder");
}
