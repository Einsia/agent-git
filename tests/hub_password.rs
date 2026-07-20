//! End-to-end coverage of the admin CLI password-reset door: `agit-hub user passwd <name>` reads the
//! new password from stdin (never argv), re-hashes it with argon2, and the user can then log in with
//! the new password while the old one is refused. Driven through the real binary + the public store
//! API, so it exercises the same path an operator recovering a locked-out account would.

use std::io::Write;
use std::process::{Command, Stdio};

const HUB: &str = env!("CARGO_BIN_EXE_agit-hub");

/// Run an agit-hub subcommand, feeding `stdin` to it. Returns the exit code. `AGIT_HUB_DB` /
/// `AGIT_HUB_S3_ENDPOINT` are stripped so the command hits the zero-config SQLite store under `root`,
/// never a developer's real Postgres/S3.
fn hub(root: &std::path::Path, args: &[&str], stdin: &str) -> i32 {
    let mut child = Command::new(HUB)
        .args(args)
        .arg("--root")
        .arg(root)
        .env_remove("AGIT_HUB_DB")
        .env_remove("AGIT_HUB_S3_ENDPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agit-hub");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait().expect("wait agit-hub").code().unwrap_or(-1)
}

#[tokio::test]
async fn cli_user_passwd_resets_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create the user with an initial password (stdin, not argv).
    assert_eq!(hub(root, &["user", "add", "bob"], "bob-old-password-1\n"), 0, "user add should succeed");

    // The old password logs in; a bogus one does not (baseline).
    let store = agit::hub::store::Store::open_sqlite(root).await.unwrap();
    assert!(agit::hub::auth::verify_login(&store, "bob", "bob-old-password-1").await.is_some());

    // Admin reset via the CLI, new password on stdin.
    assert_eq!(hub(root, &["user", "passwd", "bob"], "bob-new-password-2\n"), 0, "user passwd should succeed");

    // Re-open the store (the CLI wrote through its own short-lived connection) and confirm the swap.
    let store = agit::hub::store::Store::open_sqlite(root).await.unwrap();
    assert!(agit::hub::auth::verify_login(&store, "bob", "bob-new-password-2").await.is_some(), "new password logs in");
    assert!(agit::hub::auth::verify_login(&store, "bob", "bob-old-password-1").await.is_none(), "old password is refused");

    // Resetting an unknown user is a clean failure, not a silent create.
    assert_ne!(hub(root, &["user", "passwd", "ghost"], "whatever-password-1\n"), 0, "unknown user must fail");
    assert!(agit::hub::store::Store::open_sqlite(root).await.unwrap().user("ghost").await.is_none());
}
