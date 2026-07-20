//! Server-side signed-push provenance verification (part of the pre-receive gate).
//!
//! For every pushed session that carries a cryptographic provenance sidecar, the hub verifies the
//! provenance SIGNATURE and looks up the committer email's REGISTERED identity key, then records the
//! per-session verdict (an `provenance.verify` audit event) — the server-side equivalent of a "Verified"
//! badge. This is **non-blocking by default**: a legacy, unsigned, or unregistered push is never
//! refused, exactly like a repo host that shows an unverified commit without rejecting it.
//!
//! An operator can OPT IN to enforcement with the hub-wide env flag `AGIT_HUB_PROVENANCE_ENFORCE`
//! (truthy: `1`/`true`/`yes`/`on`), set on the SERVE process so it is inherited by this git-invoked
//! hook. Enforcement refuses ONLY a push that carries a positive KEY MISMATCH — provenance signed by a
//! key that is NOT the registered key of the claimed committer email, i.e. a forgery. An unsigned or
//! unregistered session is still allowed even under enforcement: absence of a signature is not a forgery.
//!
//! Content-tamper (transcript vs recorded digest) stays the CLIENT's read-time job — the server holds
//! only the pushed objects and verifies the signature over the recorded digest, which is exactly what it
//! needs to attribute the key to a person.

use std::path::Path;

use agit::commands::{self, Provenance, ProvenanceStatus, RegisteredIdentity};
use agit::hub::audit;
use agit::hub::store::Store;

use crate::gitplumb::{git, git_bytes};

/// Whether the operator opted into rejecting forged pushes. Read from the environment the serve process
/// passes down to this hook. Absent/blank/false = record-only (the default).
fn enforce_enabled() -> bool {
    matches!(
        std::env::var("AGIT_HUB_PROVENANCE_ENFORCE").ok().as_deref().map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// One pushed session's provenance: its session id (for the audit line) and the parsed record.
struct PushedProvenance {
    session: String,
    prov: Provenance,
}

/// Extract every provenance record the push introduces, from the `*.agit.json` sidecar blobs it brings.
/// Same object range as the secret scan (`--not --all` = only what is arriving), so this is proportional
/// to the push, not the repo. A sidecar that is not JSON, or carries no `provenance` block, is skipped.
fn pushed_provenance(repo: &Path, news: &[String]) -> Vec<PushedProvenance> {
    let mut out = vec![];
    let mut args: Vec<&str> = vec!["rev-list", "--objects"];
    for n in news {
        args.push(n);
    }
    args.push("--not");
    args.push("--all");
    let Some(listing) = git(repo, &args) else {
        return out;
    };
    for line in listing.lines() {
        let Some((sha, path)) = line.split_once(' ') else {
            continue; // commits/tags have no path here
        };
        if !path.ends_with(".agit.json") {
            continue;
        }
        let Some(bytes) = git_bytes(repo, &["cat-file", "blob", sha]) else {
            continue;
        };
        let Some(prov) = parse_sidecar_provenance(&bytes) else {
            continue;
        };
        let session = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".agit.json").to_string();
        out.push(PushedProvenance { session, prov });
    }
    out
}

/// Parse the `provenance` block out of a session sidecar's bytes. Mirrors the client's `sidecar_provenance`
/// (absent/unparsable → `None`, never an error).
fn parse_sidecar_provenance(bytes: &[u8]) -> Option<Provenance> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    serde_json::from_value(v.get("provenance")?.clone()).ok()
}

/// The one-word verdict recorded in the audit line and used to decide enforcement.
fn verdict_word(status: &ProvenanceStatus) -> &'static str {
    match status {
        ProvenanceStatus::VerifiedAs { .. } => "verified-as",
        ProvenanceStatus::KeyMismatch { .. } => "key-mismatch",
        ProvenanceStatus::SignedUnregistered { .. } => "signed-unregistered",
        ProvenanceStatus::Verified { .. } => "signed",
        ProvenanceStatus::Unsigned => "unsigned",
        ProvenanceStatus::ContentTampered { .. } => "content-tampered",
        ProvenanceStatus::BadSignature => "bad-signature",
    }
}

/// Classify one pushed provenance record against the registry. Signature-only self-verify (the server
/// holds no transcript to digest); a valid signature is then attributed via the registry answer.
fn classify(prov: &Provenance, registered: Option<RegisteredIdentity>) -> ProvenanceStatus {
    if !commands::verify_provenance_signature(prov) {
        return ProvenanceStatus::BadSignature;
    }
    let self_status = ProvenanceStatus::Verified {
        aid: prov.aid.clone(),
        email: prov.email.clone(),
        pubkey: prov.pubkey.clone(),
    };
    // No TOFU on the server: the hub IS the registry, so it compares against its own stored key directly.
    commands::attribute_with_registry(self_status, registered, None)
}

/// Verify the provenance of every session a push introduces, recording a per-session verdict and, under
/// enforcement, refusing a forgery. Returns `Some(message)` when the push must be REFUSED (a positive key
/// mismatch, enforcement on) — the caller prints it and exits non-zero. `None` = allow (the default for
/// every verdict, and always when enforcement is off).
///
/// Best-effort and fail-open on infrastructure trouble: if the registry cannot be opened, nothing it
/// could not resolve is treated as "unregistered" and the push is allowed — a store outage must not
/// become a push outage.
pub(crate) fn verify_pushed_provenance(root: &Path, repo: &Path, news: &[String], actor: &str, scoped: &str) -> Option<String> {
    let pushed = pushed_provenance(repo, news);
    if pushed.is_empty() {
        return None;
    }
    // Resolve each session's committer email to a registered identity, read-only and out-of-process (WAL
    // reads never block the live hub). Any failure degrades every lookup to "unregistered".
    let registered = resolve_registry(root, &pushed);
    record_and_enforce(root, actor, scoped, &pushed, &registered, enforce_enabled())
}

/// The pure verdict + enforcement core, given the already-extracted provenance and the registry's answer
/// for each (aligned by index). Records one `provenance.verify` audit line per session; when `enforce`,
/// returns `Some(reject_message)` iff any session is a positive KEY MISMATCH. Unsigned/unregistered
/// sessions are recorded but NEVER rejected — only a forgery is. Split out so the classification and the
/// enforce policy can be tested without a git repo, a live store, or the enforce env flag.
fn record_and_enforce(
    root: &Path,
    actor: &str,
    scoped: &str,
    pushed: &[PushedProvenance],
    registered: &[Option<RegisteredIdentity>],
    enforce: bool,
) -> Option<String> {
    let mut mismatches: Vec<String> = vec![];
    for (i, item) in pushed.iter().enumerate() {
        let status = classify(&item.prov, registered.get(i).cloned().flatten());
        let word = verdict_word(&status);
        let detail = match &status {
            ProvenanceStatus::VerifiedAs { username, email, .. } => {
                format!("{}: {word} {username} <{email}>", item.session)
            }
            ProvenanceStatus::KeyMismatch { email, claimed_username, .. } => {
                format!("{}: {word} email={email} registered-to={claimed_username} (possible forgery)", item.session)
            }
            _ => format!("{}: {word} email={}", item.session, item.prov.email),
        };
        audit::append(root, actor, audit::PROVENANCE_VERIFY, Some(scoped), &detail);
        if enforce {
            if let ProvenanceStatus::KeyMismatch { email, claimed_username, .. } = &status {
                mismatches.push(format!(
                    "  {}  committer {email} is registered to {claimed_username}, but the session was signed by a DIFFERENT key",
                    item.session
                ));
            }
        }
    }

    if mismatches.is_empty() {
        return None;
    }
    audit::append(
        root,
        actor,
        audit::GIT_PUSH_REJECTED,
        Some(scoped),
        &format!("provenance enforce: {} key-mismatch session(s)", mismatches.len()),
    );
    let mut msg = String::new();
    msg.push_str("agit-hub: push REFUSED — provenance KEY MISMATCH (a possible forgery).\n\n");
    for line in &mismatches {
        msg.push_str(line);
        msg.push('\n');
    }
    msg.push_str(
        "\nThis hub enforces signed provenance (AGIT_HUB_PROVENANCE_ENFORCE). A session claiming a\n\
         committer email must be signed by that account's REGISTERED key. Enroll the signing machine\n\
         (agit identity enroll) or commit under the matching identity, then push again.\n\
         Unsigned and unregistered sessions are still accepted — only a positive mismatch is refused.\n",
    );
    Some(msg)
}

/// Open the registry read-only and resolve each pushed session's committer email to a registered
/// identity, aligned by index with `pushed`. All-`None` on any infrastructure failure (fail-open).
fn resolve_registry(root: &Path, pushed: &[PushedProvenance]) -> Vec<Option<RegisteredIdentity>> {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(_) => return vec![None; pushed.len()],
    };
    rt.block_on(async {
        let Ok(store) = Store::open_readonly(root).await else {
            return vec![None; pushed.len()];
        };
        let mut out = Vec::with_capacity(pushed.len());
        for item in pushed {
            // The committer email's VERIFIED account and its whole device-key SET (empty when unknown /
            // unverified / ambiguous) — a push signed by ANY of the account's keys attributes to them.
            let keys = store.get_identity_keys_by_email(&item.prov.email).await;
            let reg = keys.first().map(|k| RegisteredIdentity {
                username: k.username.clone(),
                ed25519_keys: keys.iter().map(|k| k.ed25519_pub.clone()).collect(),
            });
            out.push(reg);
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn signed(home: &Path, email: &str) -> (Provenance, String) {
        let key = agit::agent::load_or_create_signing_key(home).unwrap();
        let content = "the session transcript\n";
        let p = agit::commands::sign_provenance(&key, content, "agt_01", email, "2026-07-20T00:00:00Z");
        (p, hex::encode(key.verifying_key().to_bytes()))
    }

    fn verdicts(root: &Path) -> Vec<String> {
        audit::query(root, None, 100).into_iter().filter(|e| e.action == audit::PROVENANCE_VERIFY).map(|e| e.detail).collect()
    }

    /// A push whose committer email maps to a registered account with the SAME signing key records a
    /// `verified-as` verdict, and is allowed even under enforcement.
    #[test]
    fn records_verified_as_and_allows() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (p, pubkey) = signed(home.path(), "alice@corp.com");
        let pushed = vec![PushedProvenance { session: "sess1".into(), prov: p }];
        // alice has two device keys registered; the signing key is one of them — match ANY.
        let reg = vec![Some(RegisteredIdentity {
            username: "alice".into(),
            ed25519_keys: vec!["11".repeat(32), pubkey],
        })];

        let out = record_and_enforce(root.path(), "alice", "alice/web", &pushed, &reg, true);
        assert!(out.is_none(), "a matching key is not a forgery — never rejected");
        let v = verdicts(root.path());
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("verified-as") && v[0].contains("alice"), "{:?}", v[0]);
    }

    /// The anti-forgery property: a committer email that maps to a registered account whose key DIFFERS
    /// from the signing key is a `key-mismatch`, and under enforcement the push is REFUSED.
    #[test]
    fn enforce_rejects_key_mismatch_forgery() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (p, _signing_key) = signed(home.path(), "alice@corp.com");
        // alice is registered with a DIFFERENT key than the one that signed this session.
        let other = "11".repeat(32);
        let pushed = vec![PushedProvenance { session: "sess1".into(), prov: p }];
        let reg = vec![Some(RegisteredIdentity { username: "alice".into(), ed25519_keys: vec![other] })];

        let out = record_and_enforce(root.path(), "mallory", "alice/web", &pushed, &reg, true);
        let msg = out.expect("a positive key mismatch must be refused under enforcement");
        assert!(msg.contains("KEY MISMATCH"), "{msg}");
        let entries = audit::query(root.path(), None, 100);
        assert!(entries.iter().any(|e| e.action == audit::PROVENANCE_VERIFY && e.detail.contains("key-mismatch")));
        assert!(entries.iter().any(|e| e.action == audit::GIT_PUSH_REJECTED), "the rejection is audited");
    }

    /// Enforcement refuses ONLY a positive mismatch: an unregistered committer (email maps to no account)
    /// and a bad-signature session are both recorded and ALLOWED, never rejected.
    #[test]
    fn enforce_allows_unregistered_and_unsigned() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (registered_ok, _k) = signed(home.path(), "nobody@corp.com");
        let (mut bad, _k2) = signed(home.path(), "nobody@corp.com");
        bad.sig = "00".repeat(64); // a signature that will not verify -> bad-signature, not a mismatch
        let pushed = vec![
            PushedProvenance { session: "unreg".into(), prov: registered_ok },
            PushedProvenance { session: "badsig".into(), prov: bad },
        ];
        // No registered account for either committer.
        let reg = vec![None, None];

        let out = record_and_enforce(root.path(), "u", "u/web", &pushed, &reg, true);
        assert!(out.is_none(), "unregistered + bad-signature are not forgeries — enforcement allows them");
        let v = verdicts(root.path());
        assert!(v.iter().any(|d| d.contains("signed-unregistered")), "{v:?}");
        assert!(v.iter().any(|d| d.contains("bad-signature")), "{v:?}");
    }

    /// `pushed_provenance` extracts provenance from the `*.agit.json` sidecars a push introduces, over a
    /// real git repo with the ref held back (as it is during pre-receive, before any ref moves).
    #[test]
    fn extracts_provenance_from_a_real_push() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| {
            let ok = Command::new("git")
                .args(["-c", "user.email=t@t", "-c", "user.name=t"])
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&ok.stderr));
            String::from_utf8_lossy(&ok.stdout).trim().to_string()
        };
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("README"), "base\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "base"]);
        let base = g(&["rev-parse", "HEAD"]);

        // The "incoming" commit: a session transcript + its provenance sidecar.
        let (p, _pub) = signed(home.path(), "dev@corp.com");
        let dir = repo.path().join("sessions/env/claude-code");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s1.jsonl"), "the session transcript\n").unwrap();
        std::fs::write(dir.join("s1.agit.json"), serde_json::json!({ "provenance": p }).to_string()).unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "session"]);
        let incoming = g(&["rev-parse", "HEAD"]);
        // Hold the ref back so `incoming`'s objects are unreachable from any ref — exactly the pre-receive
        // view (`--not --all` then equals "everything this push brings").
        g(&["update-ref", "refs/heads/main", &base]);

        let pushed = pushed_provenance(repo.path(), &[incoming]);
        assert_eq!(pushed.len(), 1, "the one pushed session's provenance is found");
        assert_eq!(pushed[0].session, "s1");
        assert_eq!(pushed[0].prov.email, "dev@corp.com");
        assert!(commands::verify_provenance_signature(&pushed[0].prov), "the extracted signature verifies");
    }
}
