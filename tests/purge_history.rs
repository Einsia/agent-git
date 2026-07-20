//! End-to-end test for `agit a purge-history`: the guard-railed history rewrite that re-encrypts every
//! historical revision of `sessions/**` so NO plaintext of an encrypted transcript survives in ANY commit.
//!
//! It drives the REAL `agit` binary and REAL git under an isolated `$HOME`/`$AGIT_HOME` (per-invocation
//! env only — `std::env::set_var` is process-global and races parallel tests). The scenario mirrors the
//! real gap: a session is committed in the CLEAR before encryption is enabled, so `git cat-file` recovers
//! its plaintext from history even after `agit a encrypt`. The test proves the gap exists, purges it, and
//! asserts (a) no historical revision retains the plaintext marker, (b) the working tree still decrypts to
//! the original plaintext (the smudge path), and (c) every commit SHA changed (the rewrite happened).

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agit");
const MARKER: &str = "MARKER_PURGE_PLAINTEXT_c71e04";

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Repo {
        let r = Repo { dir: tempfile::tempdir().unwrap() };
        r.sh("git init -q -b main .");
        r.sh("git config user.name dev && git config user.email d@x.com");
        r.sh("git config commit.gpgsign false");
        r.write("app.ts", "export const x = 1;\n");
        r.sh("git add -A && git commit -qm seed");
        assert_eq!(r.agit(&["init", "--agent", "testmemory"]).0, 0, "init should succeed");
        r
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The resolved agent store — asked of agit, never hardcoded.
    fn agent(&self) -> PathBuf {
        let (code, out, err) = self.agit(&["a", "rev-parse", "--show-toplevel"]);
        assert_eq!(code, 0, "could not resolve the agent store: {err}");
        PathBuf::from(out.trim())
    }

    fn cmd(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(self.path())
            .env("HOME", self.path())
            .env("AGIT_HOME", self.path().join("agit-home"));
        c
    }
    fn sh(&self, cmd: &str) -> String {
        let o = self.cmd("sh").arg("-c").arg(cmd).output().unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn write_session(&self, rel: &str, content: &str) {
        let p = self.agent().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        let o = self.cmd(BIN).args(args).output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
    fn git_agent(&self, args: &[&str]) -> (i32, String) {
        let o = self.cmd("git").arg("-C").arg(self.agent()).args(args).output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).to_string())
    }
}

/// A benign transcript carrying a recognizable, secretless plaintext marker.
fn transcript() -> String {
    format!("{{\"role\":\"user\",\"content\":\"{MARKER} please refactor\"}}\n")
}

/// Does ANY historical revision of `<rel>` (across every commit) contain `needle`?
fn any_historical_blob_contains(store: &Path, rel: &str, needle: &str) -> bool {
    let revs = Command::new("git")
        .arg("-C")
        .arg(store)
        .args(["rev-list", "--all"])
        .output()
        .unwrap();
    assert!(revs.status.success(), "git rev-list --all failed");
    for rev in String::from_utf8_lossy(&revs.stdout).lines() {
        let rev = rev.trim();
        if rev.is_empty() {
            continue;
        }
        let blob = Command::new("git")
            .arg("-C")
            .arg(store)
            .args(["cat-file", "-p", &format!("{rev}:{rel}")])
            .output()
            .unwrap();
        // The path may not exist in every commit (e.g. the seed commit) — that is fine, skip it.
        if !blob.status.success() {
            continue;
        }
        if String::from_utf8_lossy(&blob.stdout).contains(needle) {
            return true;
        }
    }
    false
}

// The full gap → purge → prove flow, end to end through the real binary and real git filters.
#[test]
fn purge_history_scrubs_plaintext_from_history_and_keeps_worktree_decryptable() {
    let r = Repo::new();
    let store = r.agent();
    let rel = "sessions/proj/claude-code/sess.jsonl";

    // ── 1. Commit a session in the CLEAR, BEFORE encryption is enabled. ──
    r.write_session(rel, &transcript());
    assert_eq!(r.git_agent(&["add", "--", rel]).0, 0, "stage the plaintext session");
    let (c, _o, e) = r.agit(&["a", "commit", "-m", "plaintext session before encryption"]);
    assert_eq!(c, 0, "commit the plaintext session: {e}");
    let pre_encrypt_sha = r.git_agent(&["rev-parse", "HEAD"]).1.trim().to_string();

    // Sanity: this commit's blob is genuinely plaintext (the gap we are about to prove + purge).
    let plain = r.git_agent(&["cat-file", "-p", &format!("{pre_encrypt_sha}:{rel}")]).1;
    assert!(plain.contains(MARKER), "the pre-encryption commit must hold plaintext");

    // ── 2. Enable PER-SESSION encryption (public reader — no hub needed). Forward-only: HEAD becomes
    //       ciphertext, but the earlier commit still holds plaintext. ──
    let (c, _o, e) = r.agit(&["a", "encrypt", "--public", "--yes"]);
    assert_eq!(c, 0, "per-session (public) encryption should enable: {e}");
    let head_after_encrypt = r.git_agent(&["rev-parse", "HEAD"]).1.trim().to_string();

    // The current HEAD's blob is now ciphertext (going-forward encryption worked)…
    let head_blob = r.git_agent(&["cat-file", "-p", &format!("HEAD:{rel}")]).1;
    assert!(!head_blob.contains(MARKER), "HEAD must be re-encrypted after `encrypt`");

    // …but the GAP exists: the OLD commit's blob still recovers the plaintext marker from history.
    assert!(
        any_historical_blob_contains(&store, rel, MARKER),
        "PRECONDITION: plaintext must still be recoverable from history before purge"
    );

    // ── 3. Purge history. ──
    let (c, out, err) = r.agit(&["a", "purge-history", "--yes"]);
    assert_eq!(c, 0, "purge-history should succeed: {err}\n{out}");
    assert!(
        out.contains("filter-branch") || out.contains("filter-repo"),
        "purge must report which backend it used: {out}"
    );
    // It must NOT auto-push — it prints an explicit force-push command for the user to run.
    assert!(out.contains("push --force"), "purge must print the exact force-push command: {out}");

    // ── (a) NO historical revision of the session retains the plaintext marker anymore. ──
    assert!(
        !any_historical_blob_contains(&store, rel, MARKER),
        "after purge, no commit's blob for {rel} may contain the plaintext marker"
    );

    // ── (b) The working-tree checkout STILL decrypts to the original plaintext (the smudge path). ──
    let wt = std::fs::read_to_string(store.join(rel)).unwrap();
    assert!(wt.contains(MARKER), "the working tree must still decrypt to the original plaintext: {wt:?}");

    // A fresh re-checkout also decrypts (the smudge filter recovers plaintext from the ciphertext blob).
    std::fs::remove_file(store.join(rel)).unwrap();
    assert_eq!(r.git_agent(&["checkout", "--", rel]).0, 0, "re-checkout must decrypt cleanly");
    let wt2 = std::fs::read_to_string(store.join(rel)).unwrap();
    assert!(wt2.contains(MARKER), "re-checkout must restore the exact plaintext");

    // ── (c) Commit SHAs changed — the rewrite happened. ──
    let new_head = r.git_agent(&["rev-parse", "HEAD"]).1.trim().to_string();
    assert_ne!(new_head, head_after_encrypt, "purge must rewrite HEAD to a new SHA");
    assert!(
        r.git_agent(&["rev-parse", &format!("{pre_encrypt_sha}^{{commit}}")]).0 != 0
            || !any_historical_blob_contains(&store, rel, MARKER),
        "the pre-encryption plaintext SHA must no longer describe live history holding the marker"
    );

    // The now-committed HEAD blob is ciphertext (self-describing AGITCRYPT magic), not plaintext.
    let final_blob = Command::new("git")
        .arg("-C")
        .arg(&store)
        .args(["cat-file", "-p", &format!("HEAD:{rel}")])
        .output()
        .unwrap();
    assert!(final_blob.status.success());
    assert!(
        final_blob.stdout.starts_with(b"AGITCRYPT\x00"),
        "the rewritten HEAD blob must be ciphertext"
    );

    // The keybox survived the rewrite as PLAINTEXT (it must never be double-encrypted, or unlock breaks).
    let keybox = std::fs::read_to_string(store.join(".agit/keybox.jsonl")).unwrap();
    assert!(keybox.contains("\"kid\":0"), "the keybox must remain readable after purge:\n{keybox}");
}

// A store that is NOT per-session encrypted must be refused with a clear "nothing to purge" message,
// never a silent rewrite of unencrypted history.
#[test]
fn purge_history_refuses_when_not_per_session_encrypted() {
    let r = Repo::new();
    r.write_session("sessions/proj/claude-code/sess.jsonl", &transcript());
    assert_eq!(r.git_agent(&["add", "-A"]).0, 0);
    assert_eq!(r.agit(&["a", "commit", "-m", "plain"]).0, 0);

    let (code, _out, err) = r.agit(&["a", "purge-history", "--yes"]);
    assert_ne!(code, 0, "purge on a non-encrypted store must refuse");
    assert!(
        err.contains("not per-session encrypted") || err.contains("nothing to purge"),
        "the refusal must explain there is nothing to purge: {err}"
    );
}
