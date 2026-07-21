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
        // Stores now inherit the user's git identity (local -> global) instead of a store-local
        // `agit@local`, so point git's GLOBAL config at an isolated file inside this tempdir. Every store
        // spawned here then resolves a stable `tester@agit.test` committer email (as a real machine's
        // `~/.gitconfig` would), so snap is attributable and not refused. Never touches the developer's
        // real `~/.gitconfig`.
        let gitconfig = self.path().join(".agit-test-gitconfig");
        if !gitconfig.exists() {
            std::fs::write(
                &gitconfig,
                "[user]\n\tname = tester\n\temail = tester@agit.test\n[commit]\n\tgpgsign = false\n",
            )
            .ok();
        }
        let mut c = Command::new(program);
        c.current_dir(self.path())
            .env("HOME", self.path())
            .env("AGIT_HOME", self.path().join("agit-home"))
            .env("GIT_CONFIG_GLOBAL", &gitconfig);
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

// Test 15 — a store with a `hub` remote whose hub cannot be reached/confirmed must NOT silently fall back
// to a machine-only key on zero-config `agit a encrypt` (teammates could never decrypt it). It refuses
// with an actionable error, WITH OR WITHOUT --yes, and never wires the filter. (The legitimate no-hub
// machine-global path — encrypt --yes on a remote-less store proceeds — is covered by the tests below,
// e.g. checkout_fails_loudly_without_the_key.)
#[test]
fn hub_remote_encrypt_refuses_silent_machine_global() {
    let r = Repo::new();
    // A `hub` remote is present, but the host is unreachable (owner resolves, org-ness cannot be confirmed).
    assert_eq!(r.git_agent(&["remote", "add", "hub", "https://example.invalid/agent.git"]).0, 0);

    for args in [&["a", "encrypt"][..], &["a", "encrypt", "--yes"][..]] {
        let (code, _out, err) = r.agit(args);
        assert_ne!(code, 0, "a hub-remote store must not silently machine-global encrypt ({args:?}): {err}");
        assert!(
            err.contains("hub") || err.contains("team") || err.contains("confirm"),
            "the refusal must explain the hub/team gate ({args:?}): {err}"
        );
        assert!(
            r.config_get("filter.agit-crypt.clean").is_empty(),
            "the filter must not have been wired on refusal ({args:?})"
        );
    }
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

// ─────────────────────── Wave 2: per-session keybox (individual + public readers) ───────────────────────
//
// These drive the REAL binary AND real git clean/smudge filters end to end: encrypt to a reader, commit
// ciphertext, clone as that reader, `agit crypt unlock`, and read the decrypted working tree. A second,
// non-reader identity proves the fail-closed refusal. All identities are independent machine keys under
// separate $AGIT_HOME roots, so the X25519 wrap/unwrap is exercised across genuinely distinct keys.

/// An independent "machine": its own HOME + AGIT_HOME + a code repo to run agit from.
struct Machine {
    _base: tempfile::TempDir,
    code: PathBuf,
    home: PathBuf,
    agit_home: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let base = tempfile::tempdir().unwrap();
        let code = base.path().join("code");
        let home = base.path().join("home");
        let agit_home = base.path().join("home/agit-home");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // Stores inherit the user's git identity (local -> global); give this machine's isolated HOME a
        // global `.gitconfig` so its store resolves a committer email and snap is not refused. This is the
        // machine's own throwaway HOME, never the developer's real `~/.gitconfig`.
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = tester\n\temail = tester@agit.test\n[commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        let m = Machine { _base: base, code, home, agit_home };
        m.run_git(&["init", "-q", "-b", "main", "."]);
        m.run_git(&["config", "user.name", "dev"]);
        m.run_git(&["config", "user.email", "d@x.com"]);
        m.run_git(&["config", "commit.gpgsign", "false"]);
        m
    }
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(BIN)
            .current_dir(&self.code)
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit_home)
            .args(args)
            .output()
            .unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())
    }
    fn run_git(&self, args: &[&str]) -> (i32, String) {
        let o = Command::new("git").current_dir(&self.code).args(args).output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).to_string())
    }
    /// This machine's X25519 public key (hex), as agit reports it.
    fn x25519(&self) -> String {
        let (c, out, err) = self.agit(&["identity", "show"]);
        assert_eq!(c, 0, "identity show failed: {err}");
        out.lines()
            .find_map(|l| l.trim().strip_prefix("x25519").map(|r| r.trim().to_string()))
            .filter(|h| !h.is_empty())
            .expect("identity show must print an x25519 pubkey")
    }
    /// The resolved active agent store on this machine.
    fn agent_store(&self) -> PathBuf {
        let (c, out, err) = self.agit(&["a", "rev-parse", "--show-toplevel"]);
        assert_eq!(c, 0, "resolve store failed: {err}");
        PathBuf::from(out.trim())
    }
}

/// The env-partitioned relative path of a written session, in an arbitrary store.
fn find_rel_in(store: &Path, file: &str) -> String {
    find_rel(store, file)
}

/// Owner mints an agent, writes+commits an encrypted session, and returns (owner, store, rel).
fn owner_with_encrypted_session(readers: &[&str], public: bool, reader_keys: &[(&str, String)]) -> (Machine, PathBuf, String) {
    let owner = Machine::new();
    assert_eq!(owner.agit(&["init", "--agent", "mem"]).0, 0, "init agent");
    // Pin each reader's key offline (no hub in tests) so the wrap can resolve.
    for (u, k) in reader_keys {
        let (c, _o, e) = owner.agit(&["identity", "pin", u, "--key", k]);
        assert_eq!(c, 0, "pin {u} failed: {e}");
    }
    let mut args = vec!["a", "encrypt", "--yes"];
    let joined = readers.join(",");
    if !readers.is_empty() {
        args.push("--readers");
        args.push(&joined);
    }
    if public {
        args.push("--public");
    }
    let (c, _o, e) = owner.agit(&args);
    assert_eq!(c, 0, "encrypt --readers/--public failed: {e}");

    let store = owner.agent_store();
    let rel = "sessions/proj/claude-code/sess.jsonl";
    let p = store.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, format!("{{\"role\":\"user\",\"content\":\"{MARKER} refactor\"}}\n")).unwrap();
    let real_rel = find_rel_in(&store, "sess.jsonl");
    let og = Command::new("git").arg("-C").arg(&store).args(["add", "--", &real_rel]).output().unwrap();
    assert!(og.status.success(), "git add of the session (clean filter) must succeed");
    assert_eq!(owner.agit(&["a", "commit", "-m", "snap"]).0, 0, "commit encrypted session");
    (owner, store, real_rel)
}

/// Raw committed bytes of `rel` at HEAD in `store`.
fn committed(store: &Path, rel: &str) -> Vec<u8> {
    let o = Command::new("git").arg("-C").arg(store).args(["cat-file", "-p", &format!("HEAD:{rel}")]).output().unwrap();
    assert!(o.status.success(), "no committed blob at {rel}");
    o.stdout
}

// (b) A session encrypted to reader bob: bob clones, `crypt unlock` recovers the CK and decrypts the
// transcript through the REAL smudge filter; a non-reader (mallory) is refused, fail-closed.
#[test]
fn keybox_reader_unlocks_and_nonreader_is_refused() {
    let bob = Machine::new();
    let mallory = Machine::new();
    let bob_key = bob.x25519();
    let mallory_key = mallory.x25519();
    assert_ne!(bob_key, mallory_key, "distinct machines have distinct x25519 keys");

    let (_owner, store, rel) = owner_with_encrypted_session(&["bob"], false, &[("bob", bob_key)]);

    // Committed blob is ciphertext under kid 0, no plaintext marker.
    let blob = committed(&store, &rel);
    assert!(blob.starts_with(b"AGITCRYPT\x00"), "committed session must be ciphertext");
    assert_eq!(u32::from_le_bytes(blob[11..15].try_into().unwrap()), 0, "sealed under kid 0");
    assert!(!String::from_utf8_lossy(&blob).contains(MARKER), "no plaintext in the committed blob");

    // Bob clones the store and unlocks with his identity.
    let store_str = store.to_string_lossy().to_string();
    let (c, _o, e) = bob.agit(&["a", "clone", &store_str]);
    assert_eq!(c, 0, "bob clone failed: {e}");
    let (c, out, e) = bob.agit(&["crypt", "unlock"]);
    assert_eq!(c, 0, "bob (a reader) must unlock: {e}\n{out}");
    let bob_store = bob.agent_store();
    let wt = std::fs::read_to_string(bob_store.join(&rel)).unwrap();
    assert!(wt.contains(MARKER), "bob's working tree must be decrypted plaintext, got: {wt:?}");

    // Mallory clones and is refused (fail-closed): unlock exits nonzero, no plaintext leaks.
    let (c, _o, e) = mallory.agit(&["a", "clone", &store_str]);
    assert_eq!(c, 0, "mallory clone failed: {e}");
    let (c, _o, _e) = mallory.agit(&["crypt", "unlock"]);
    assert_ne!(c, 0, "a non-reader must be refused by crypt unlock");
    let mallory_store = mallory.agent_store();
    if let Ok(bytes) = std::fs::read(mallory_store.join(&rel)) {
        assert!(!String::from_utf8_lossy(&bytes).contains(MARKER), "a refused unlock must never leave plaintext");
    }
    // The repo-local keyring was NOT written for mallory (fail-closed).
    assert!(!mallory_store.join(".git/agit-crypt/keyring").exists(), "no keyring must be written on a failed unlock");
}

// (c) A PUBLIC session: any clone recovers the CK from the repo alone (no key).
#[test]
fn keybox_public_session_unlocks_from_the_repo_alone() {
    let (_owner, store, rel) = owner_with_encrypted_session(&[], true, &[]);
    let blob = committed(&store, &rel);
    assert!(blob.starts_with(b"AGITCRYPT\x00"), "public session content is still encrypted at rest");

    // A brand-new machine with an unrelated identity can unlock a public session.
    let anyone = Machine::new();
    let store_str = store.to_string_lossy().to_string();
    assert_eq!(anyone.agit(&["a", "clone", &store_str]).0, 0, "clone");
    let (c, _o, e) = anyone.agit(&["crypt", "unlock"]);
    assert_eq!(c, 0, "a public session must unlock with no key: {e}");
    let s = anyone.agent_store();
    assert!(std::fs::read_to_string(s.join(&rel)).unwrap().contains(MARKER), "public content decrypts from the repo alone");
}

// (d) `readers add` appends a stanza WITHOUT re-cleaning existing encrypted blobs.
#[test]
fn keybox_readers_add_does_not_recrypt_existing_blobs() {
    let bob = Machine::new();
    let carol = Machine::new();
    let (owner, store, rel) = owner_with_encrypted_session(&["bob"], false, &[("bob", bob.x25519())]);

    let blob_before = committed(&store, &rel);
    let oid_before = String::from_utf8_lossy(&Command::new("git").arg("-C").arg(&store).args(["rev-parse", &format!("HEAD:{rel}")]).output().unwrap().stdout).trim().to_string();

    // Add carol (pinned offline via --key). This must touch ONLY the keybox.
    let (c, _o, e) = owner.agit(&["identity", "pin", "carol", "--key", &carol.x25519()]);
    assert_eq!(c, 0, "pin carol: {e}");
    let (c, out, e) = owner.agit(&["a", "readers", "add", "carol"]);
    assert_eq!(c, 0, "readers add carol: {e}\n{out}");

    let blob_after = committed(&store, &rel);
    let oid_after = String::from_utf8_lossy(&Command::new("git").arg("-C").arg(&store).args(["rev-parse", &format!("HEAD:{rel}")]).output().unwrap().stdout).trim().to_string();
    assert_eq!(blob_before, blob_after, "the encrypted blob must be byte-identical after readers add");
    assert_eq!(oid_before, oid_after, "the blob object id must be unchanged (no re-clean)");

    // Carol can now unlock via her stanza.
    let store_str = store.to_string_lossy().to_string();
    assert_eq!(carol.agit(&["a", "clone", &store_str]).0, 0);
    let (c, _o, e) = carol.agit(&["crypt", "unlock"]);
    assert_eq!(c, 0, "carol must unlock after being added: {e}");
    assert!(std::fs::read_to_string(carol.agent_store().join(&rel)).unwrap().contains(MARKER));
}

// (e) `readers rm` EAGERLY rotates: a NEW commit's ciphertext is under a new kid whose CK is NOT wrapped
// to the removed reader, and the removed reader cannot unwrap the new kid.
#[test]
fn keybox_readers_rm_rotates_and_removed_reader_loses_new_kid() {
    let bob = Machine::new();
    let carol = Machine::new();
    let (owner, store, _rel) = owner_with_encrypted_session(
        &["bob", "carol"],
        false,
        &[("bob", bob.x25519()), ("carol", carol.x25519())],
    );

    // Remove carol — eager rotation to a new kid.
    let (c, out, e) = owner.agit(&["a", "readers", "rm", "carol"]);
    assert_eq!(c, 0, "readers rm carol: {e}\n{out}");
    assert!(out.contains("kid 1"), "rm must rotate to a new kid: {out}");

    // A NEW session commits under the new kid.
    let p = store.join("sessions/proj/claude-code/sess2.jsonl");
    std::fs::write(&p, format!("{{\"role\":\"user\",\"content\":\"{MARKER} second\"}}\n")).unwrap();
    let rel2 = find_rel_in(&store, "sess2.jsonl");
    assert!(Command::new("git").arg("-C").arg(&store).args(["add", "--", &rel2]).output().unwrap().status.success());
    assert_eq!(owner.agit(&["a", "commit", "-m", "snap2"]).0, 0);
    let blob2 = committed(&store, &rel2);
    assert_eq!(u32::from_le_bytes(blob2[11..15].try_into().unwrap()), 1, "the new commit is sealed under kid 1");

    // The committed keybox has NO carol stanza at kid 1 (only bob).
    let keybox = std::fs::read_to_string(store.join(".agit/keybox.jsonl")).unwrap();
    let kid1_to_carol = keybox.lines().any(|l| l.contains("\"kid\":1") && l.contains("\"to\":\"carol\""));
    assert!(!kid1_to_carol, "kid 1 must NOT be wrapped to the removed reader carol:\n{keybox}");
    assert!(keybox.lines().any(|l| l.contains("\"kid\":1") && l.contains("\"to\":\"bob\"")), "kid 1 must be re-wrapped to bob");

    // Carol clones and unlocks: she recovers ONLY the old kid (0), so the kid-1 blob stays ciphertext.
    let store_str = store.to_string_lossy().to_string();
    assert_eq!(carol.agit(&["a", "clone", &store_str]).0, 0);
    let (c, out, _e) = carol.agit(&["crypt", "unlock"]);
    assert_eq!(c, 0, "carol still unlocks her retained kid 0");
    assert!(out.contains("current kid 0") || out.contains("recovered 1"), "carol recovers only the old generation: {out}");
    // The kid-1 session cannot be decrypted by carol (no CK for kid 1): checkout fails or leaves ciphertext.
    let carol_store = carol.agent_store();
    let (_cc, _o) = {
        let o = Command::new("git").arg("-C").arg(&carol_store).args(["checkout", "--", &rel2]).output().unwrap();
        (o.status.code().unwrap_or(-1), o)
    };
    if let Ok(bytes) = std::fs::read(carol_store.join(&rel2)) {
        assert!(!String::from_utf8_lossy(&bytes).contains(MARKER), "carol must NOT decrypt the post-removal (kid 1) content");
    }

    // Bob, still a reader, unlocks BOTH generations and reads the kid-1 content.
    assert_eq!(bob.agit(&["a", "clone", &store_str]).0, 0);
    let (c, _o, e) = bob.agit(&["crypt", "unlock"]);
    assert_eq!(c, 0, "bob unlocks both generations: {e}");
    assert!(std::fs::read_to_string(bob.agent_store().join(&rel2)).unwrap().contains(MARKER), "bob reads the new-kid content");
}
