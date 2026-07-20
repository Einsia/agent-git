//! End-to-end tests for Feature C: opt-in convergent at-rest encryption of the agent store.
//!
//! These drive the REAL `agit` binary under an isolated `$HOME`/`$AGIT_HOME` (per-invocation env only —
//! `std::env::set_var` is process-global and races parallel tests), exactly like `tests/cli.rs`. They
//! assert the filter wiring, that committed session blobs are ciphertext while the working tree stays
//! plaintext, that git status is clean (convergence under real git), and the export/import key flow.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agit");
const MARKER: &str = "MARKER_PLAINTEXT_SECRETLESS_9f3a2b";

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
        self.agit_env(&[], args)
    }
    fn agit_env(&self, envs: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
        let mut c = self.cmd(BIN);
        c.args(args);
        for (k, v) in envs {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
    /// Raw bytes of a committed blob at `HEAD:<rel>` in the store.
    fn committed_bytes(&self, rel: &str) -> Vec<u8> {
        let o = self
            .cmd("git")
            .arg("-C")
            .arg(self.agent())
            .args(["cat-file", "-p", &format!("HEAD:{rel}")])
            .output()
            .unwrap();
        assert!(o.status.success(), "no committed blob at {rel}");
        o.stdout
    }
    fn git_agent(&self, args: &[&str]) -> (i32, String) {
        let o = self.cmd("git").arg("-C").arg(self.agent()).args(args).output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).to_string())
    }
    fn config_get(&self, key: &str) -> String {
        self.git_agent(&["config", "--get", key]).1.trim().to_string()
    }
}

/// A benign transcript carrying a recognizable, secretless plaintext marker.
fn transcript() -> String {
    format!("{{\"role\":\"user\",\"content\":\"{MARKER} please refactor\"}}\n")
}

/// The env-partitioned relative path git tracks for a written session (found by walking the store).
fn find_rel(store: &Path, file: &str) -> String {
    fn walk(dir: &Path, file: &str, root: &Path) -> Option<String> {
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(f) = walk(&p, file, root) {
                    return Some(f);
                }
            } else if p.file_name().and_then(|n| n.to_str()) == Some(file) {
                return Some(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
        None
    }
    walk(&store.join("sessions"), file, store).expect("written session should be found")
}

// Test 10 — `agent_encrypt` wires everything: committed .gitattributes, three local filter configs,
// a minted 0600 key.
#[test]
fn encrypt_wires_gitattributes_filter_and_key() {
    let r = Repo::new();
    let (code, _out, err) = r.agit(&["a", "encrypt", "--yes"]);
    assert_eq!(code, 0, "encrypt should succeed: {err}");

    // .gitattributes is committed at the store root with the sessions line.
    let attrs = r.committed_bytes(".gitattributes");
    let attrs = String::from_utf8_lossy(&attrs);
    assert!(
        attrs.contains("sessions/** filter=agit-crypt"),
        "committed .gitattributes must bind sessions to the filter: {attrs}"
    );

    // The three local filter configs are set.
    assert!(r.config_get("filter.agit-crypt.clean").contains("crypt-clean"));
    assert!(r.config_get("filter.agit-crypt.smudge").contains("crypt-smudge"));
    assert_eq!(r.config_get("filter.agit-crypt.required"), "true");

    // The key is minted 0600 under $AGIT_HOME/crypt/.
    let key = r.path().join("agit-home/crypt/agit-crypt.key");
    assert!(key.exists(), "the master key must be minted");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the key must be 0600");
    }
}

// Test 11 — a committed session blob is CIPHERTEXT (AGITCRYPT magic, no marker) while the working-tree
// file stays plaintext.
// Test 12 — git status is clean immediately after commit + checkout (convergence under real git).
#[test]
fn committed_blob_is_ciphertext_worktree_is_plaintext_and_status_clean() {
    let r = Repo::new();
    assert_eq!(r.agit(&["a", "encrypt", "--yes"]).0, 0);

    r.write_session("sessions/proj/claude-code/sess.jsonl", &transcript());
    let rel = find_rel(&r.agent(), "sess.jsonl");
    assert_eq!(r.git_agent(&["add", "--", &rel]).0, 0, "git add should apply the clean filter");
    let (c, _out, err) = r.agit(&["a", "commit", "-m", "snap"]);
    assert_eq!(c, 0, "commit of an encrypted store should succeed: {err}");

    // Committed blob: ciphertext, self-describing magic, and NOT the plaintext marker.
    let blob = r.committed_bytes(&rel);
    assert!(blob.starts_with(b"AGITCRYPT\x00"), "committed blob must start with the AGITCRYPT magic");
    assert_eq!(blob[10], 2, "wire version byte must be 2 (keyed format carrying the key-id)");
    assert_eq!(
        u32::from_le_bytes(blob[11..15].try_into().unwrap()),
        0,
        "a freshly enabled store seals under key-id 0"
    );
    let blob_str = String::from_utf8_lossy(&blob);
    assert!(!blob_str.contains(MARKER), "the plaintext marker must NOT survive into the committed blob");

    // Working tree file is plaintext (smudge decrypted it / it was never touched).
    let wt = std::fs::read_to_string(r.agent().join(&rel)).unwrap();
    assert!(wt.contains(MARKER), "the working-tree file must be plaintext");

    // Test 12: convergence — no spurious "modified".
    let status = r.git_agent(&["status", "--porcelain"]).1;
    assert!(status.trim().is_empty(), "git status must be clean after commit, got:\n{status}");

    // And it stays clean after a real re-checkout.
    std::fs::remove_file(r.agent().join(&rel)).unwrap();
    assert_eq!(r.git_agent(&["checkout", "--", &rel]).0, 0, "checkout must decrypt cleanly");
    let wt2 = std::fs::read_to_string(r.agent().join(&rel)).unwrap();
    assert!(wt2.contains(MARKER), "re-checkout must restore the exact plaintext");
    let status2 = r.git_agent(&["status", "--porcelain"]).1;
    assert!(status2.trim().is_empty(), "git status must be clean after checkout, got:\n{status2}");
}

// Test 13 — commit + a simulated push of an encrypted (benign) store is NOT blocked by the secret gate.
#[test]
fn encrypted_store_commit_and_push_not_blocked_by_gate() {
    let r = Repo::new();
    assert_eq!(r.agit(&["a", "encrypt", "--yes"]).0, 0);
    r.write_session("sessions/proj/claude-code/sess.jsonl", &transcript());
    let rel = find_rel(&r.agent(), "sess.jsonl");
    assert_eq!(r.git_agent(&["add", "--", &rel]).0, 0);
    assert_eq!(r.agit(&["a", "commit", "-m", "snap"]).0, 0, "commit must not be gate-blocked");

    // A bare remote stands in for the public git host; push through agit's push (which runs the gate).
    let bare = tempfile::tempdir().unwrap();
    let bare_path = bare.path().to_string_lossy().to_string();
    assert!(Command::new("git").args(["init", "-q", "--bare", &bare_path]).status().unwrap().success());
    assert_eq!(r.git_agent(&["remote", "add", "origin", &bare_path]).0, 0);

    let (code, _out, err) = r.agit(&["a", "push", "origin", "main"]);
    assert_eq!(code, 0, "push of an encrypted store must not be gate-blocked: {err}");

    // The remote now holds ciphertext.
    let o = Command::new("git")
        .args(["-C", &bare_path, "cat-file", "-p", &format!("main:{rel}")])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(o.stdout.starts_with(b"AGITCRYPT\x00"), "the pushed blob must be ciphertext");
}

// Test 14 — `--export` then `--import` into a SECOND $AGIT_HOME installs an identical key, and that
// clone's crypt-smudge decrypts a committed blob to the original plaintext.
#[test]
fn export_import_roundtrips_key_and_second_home_decrypts() {
    let r = Repo::new();
    assert_eq!(r.agit(&["a", "encrypt", "--yes"]).0, 0);
    r.write_session("sessions/proj/claude-code/sess.jsonl", &transcript());
    let rel = find_rel(&r.agent(), "sess.jsonl");
    assert_eq!(r.git_agent(&["add", "--", &rel]).0, 0);
    assert_eq!(r.agit(&["a", "commit", "-m", "snap"]).0, 0);
    let blob = r.committed_bytes(&rel);

    // Export the key to a file.
    let keyfile = r.path().join("shared.key");
    let (code, _o, err) = r.agit(&["a", "encrypt", "--export", keyfile.to_str().unwrap()]);
    assert_eq!(code, 0, "export should succeed: {err}");

    // A second, independent AGIT_HOME. Import the key there.
    let home2 = r.path().join("home2");
    std::fs::create_dir_all(&home2).unwrap();
    let (code, _o, err) = r.agit_env(
        &[("AGIT_HOME", home2.to_str().unwrap())],
        &["a", "encrypt", "--import", keyfile.to_str().unwrap()],
    );
    // Import may report "no agent resolves here" for the filter-wiring step, but installing the key succeeds.
    assert_eq!(code, 0, "import should succeed: {err}");

    // The two key files are byte-identical.
    let k1 = std::fs::read_to_string(r.path().join("agit-home/crypt/agit-crypt.key")).unwrap();
    let k2 = std::fs::read_to_string(home2.join("crypt/agit-crypt.key")).unwrap();
    assert_eq!(k1.trim(), k2.trim(), "imported key must be identical");

    // The second home's crypt-smudge decrypts the committed ciphertext to the original plaintext.
    let mut child = Command::new(BIN)
        .arg("crypt-smudge")
        .env("HOME", r.path())
        .env("AGIT_HOME", &home2)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&blob).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "smudge under the imported key must succeed");
    let decrypted = String::from_utf8_lossy(&out.stdout);
    assert!(decrypted.contains(MARKER), "the imported key must decrypt to the original plaintext");
}

// Test 15 — encrypting a store that has a `hub` remote requires --yes in non-interactive mode.
#[test]
fn hub_remote_requires_confirmation() {
    let r = Repo::new();
    // Give the store a `hub` remote (the repo's convention).
    assert_eq!(r.git_agent(&["remote", "add", "hub", "https://example.invalid/agent.git"]).0, 0);

    // Non-interactive without --yes: the gate must refuse (nonzero) and NOT enable.
    let (code, _out, err) = r.agit(&["a", "encrypt"]);
    assert_ne!(code, 0, "a hub-remote store must not enable without confirmation");
    assert!(
        err.contains("hub") || err.contains("confirmation") || err.contains("--yes"),
        "the refusal must explain the hub gate: {err}"
    );
    assert!(
        r.config_get("filter.agit-crypt.clean").is_empty(),
        "the filter must not have been wired on refusal"
    );

    // With --yes it proceeds.
    let (code, _o, err) = r.agit(&["a", "encrypt", "--yes"]);
    assert_eq!(code, 0, "encrypt --yes should proceed past the hub gate: {err}");
    assert!(!r.config_get("filter.agit-crypt.clean").is_empty());
}

// Test 16 — with the key removed, checkout of an encrypted path fails loudly (required=true) rather
// than emitting ciphertext into the working tree.
#[test]
fn checkout_fails_loudly_without_the_key() {
    let r = Repo::new();
    assert_eq!(r.agit(&["a", "encrypt", "--yes"]).0, 0);
    r.write_session("sessions/proj/claude-code/sess.jsonl", &transcript());
    let rel = find_rel(&r.agent(), "sess.jsonl");
    assert_eq!(r.git_agent(&["add", "--", &rel]).0, 0);
    assert_eq!(r.agit(&["a", "commit", "-m", "snap"]).0, 0);

    // Remove the key and the working-tree file, then force a re-smudge on checkout.
    std::fs::remove_file(r.path().join("agit-home/crypt/agit-crypt.key")).unwrap();
    std::fs::remove_file(r.agent().join(&rel)).unwrap();

    let (code, _out) = r.git_agent(&["checkout", "--", &rel]);
    assert_ne!(code, 0, "checkout must fail when the key is absent (required=true)");

    // It must NOT have written ciphertext into the working tree.
    if let Ok(bytes) = std::fs::read(r.agent().join(&rel)) {
        assert!(
            !bytes.starts_with(b"AGITCRYPT\x00"),
            "a failed smudge must never leave ciphertext in the working tree"
        );
    }
}

// Test 17 — key rotation, end to end: `a encrypt --rotate` mints a new current key (retaining the old),
// the on-disk key file becomes a keyring, going-forward blobs seal under the NEW key-id, and a blob
// committed under the OLD key still checks out (the retired key decrypts it).
#[test]
fn rotate_re_encrypts_forward_and_old_blobs_still_decrypt() {
    let r = Repo::new();
    assert_eq!(r.agit(&["a", "encrypt", "--yes"]).0, 0);

    // Commit a session under the original key (id 0).
    r.write_session("sessions/proj/claude-code/sess.jsonl", &transcript());
    let rel = find_rel(&r.agent(), "sess.jsonl");
    assert_eq!(r.git_agent(&["add", "--", &rel]).0, 0);
    assert_eq!(r.agit(&["a", "commit", "-m", "snap"]).0, 0);
    let pre_rotate = r.git_agent(&["rev-parse", "HEAD"]).1.trim().to_string();

    // The committed blob is v2 under key-id 0.
    let blob0 = r.committed_bytes(&rel);
    assert_eq!(blob0[10], 2, "pre-rotate blob is the keyed wire (v2)");
    assert_eq!(u32::from_le_bytes(blob0[11..15].try_into().unwrap()), 0, "sealed under key-id 0");

    // Rotate: mint a new current key, retaining the old, and re-encrypt the working tree.
    let (code, out, err) = r.agit(&["a", "encrypt", "--rotate", "--yes"]);
    assert_eq!(code, 0, "rotate should succeed: {err}");
    assert!(out.contains("key-id 1"), "rotate reports the new current key-id: {out}");

    // The key file is now a keyring: a `current = 1` line and BOTH keys retained.
    let keyfile = std::fs::read_to_string(r.path().join("agit-home/crypt/agit-crypt.key")).unwrap();
    assert!(keyfile.contains("current = 1"), "keyring names the new current key:\n{keyfile}");
    assert!(keyfile.contains("key 0 =") && keyfile.contains("key 1 ="), "both keys retained:\n{keyfile}");

    // Going-forward: the re-encrypted committed blob is now sealed under key-id 1.
    let blob1 = r.committed_bytes(&rel);
    assert_eq!(blob1[10], 2, "post-rotate blob is still v2");
    assert_eq!(u32::from_le_bytes(blob1[11..15].try_into().unwrap()), 1, "re-encrypted under the new key-id 1");
    assert_ne!(blob0, blob1, "the ciphertext changed under the new key");

    // git status is clean after rotation (convergence holds under the new current key).
    let status = r.git_agent(&["status", "--porcelain"]).1;
    assert!(status.trim().is_empty(), "status must be clean after rotation, got:\n{status}");

    // The OLD blob (committed under key-id 0) still decrypts: check out the pre-rotate commit's version
    // and confirm the working tree is plaintext — the retired key is what makes this possible.
    std::fs::remove_file(r.agent().join(&rel)).unwrap();
    assert_eq!(
        r.git_agent(&["checkout", &pre_rotate, "--", &rel]).0,
        0,
        "checkout of a pre-rotation (key-id 0) blob must succeed via the retired key"
    );
    let wt = std::fs::read_to_string(r.agent().join(&rel)).unwrap();
    assert!(wt.contains(MARKER), "the retired key must decrypt the old blob to plaintext");
}
