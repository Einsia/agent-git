//! `agit credential <get|store|erase>` — a git credential helper for HUB hosts.
//!
//! When git needs credentials for a hub URL it runs this helper (wired by `agit a push` as
//! `-c credential.https://<hubhost>.helper=<agit-exe> credential`, scoped to the hub host only). For a
//! `get` on a hub host we run the challenge -> sign -> exchange handshake ([`crate::hubapi::mint_key_token`])
//! and hand git back the account username plus a freshly minted, short-lived bearer token — the same
//! git-smart-http Basic/Bearer auth as before, the token just auto-minted from the enrolled ed25519 key
//! instead of copy-pasted.
//!
//! The helper is DELIBERATELY forgiving: for any non-hub host, an unknown account, or ANY error in the
//! handshake it prints NOTHING and exits 0, so git falls through to its normal helpers / Basic prompt. A
//! push must never hard-fail because key-auth was unavailable.

use anyhow::Result;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::agent;
use crate::hubapi;
use crate::scope;

/// Reuse a cached token only while it still has this many seconds of life left, so a token handed to git
/// comfortably outlasts the push it is for (the mint TTL is minutes; a push is seconds).
const CACHE_MIN_REMAINING_SECS: i64 = 300;

/// `agit credential <op>` — the git credential-helper entry point. `store`/`erase` are all-but no-ops
/// (the token is ephemeral); only `get` does work. Every path exits 0: a helper must never fail a push.
pub fn credential_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()).unwrap_or("") {
        "get" => {
            let input = parse_kv(std::io::stdin().lock());
            if let Some((user, pass)) = credential_get(&input) {
                let out = std::io::stdout();
                let mut h = out.lock();
                // git's protocol: key=value lines, then a blank line. Only these two keys are ours to set.
                let _ = write!(h, "username={user}\npassword={pass}\n\n");
            }
            Ok(0)
        }
        "erase" => {
            // git erases a credential it found unusable. Drop any cached token for that host so the next
            // `get` re-mints rather than re-serving the rejected one.
            let input = parse_kv(std::io::stdin().lock());
            if let Some(host) = input.get("host") {
                cache_forget(host);
            }
            Ok(0)
        }
        "store" => {
            // The token we returned IS the password git is telling us to store; it is ephemeral and we
            // already cache it ourselves, so there is nothing to persist. Drain stdin and succeed.
            let _ = parse_kv(std::io::stdin().lock());
            Ok(0)
        }
        other => {
            eprintln!("usage: agit credential <get|store|erase>  (git invokes this; it speaks git's credential protocol on stdin), got {other:?}");
            Ok(0)
        }
    }
}

/// The pure routing decision for a `get`: given git's parsed input, produce `(username, token)` to emit,
/// or `None` to stay silent. Split from I/O so it is testable without a live hub — it wires the real
/// hub-host check, the persisted account, and the cache-or-mint token source.
fn credential_get(input: &HashMap<String, String>) -> Option<(String, String)> {
    resolve_get(input, is_hub_host, hub_account(), |base| cache_or_mint(base, &hub_account()?))
}

/// The injectable core of [`credential_get`]: decide whether and how to answer, given the parsed input,
/// a hub-host predicate, the account to auth as, and a `base -> token` minter. Silence (`None`) is the
/// safe default at every branch.
fn resolve_get(
    input: &HashMap<String, String>,
    is_hub: impl Fn(&str) -> bool,
    username: Option<String>,
    mint: impl Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    let host = input.get("host")?;
    // git-smart-http is https; accept http too (a local/dev hub), but nothing exotic.
    let protocol = input.get("protocol").map(String::as_str).unwrap_or("https");
    if protocol != "https" && protocol != "http" {
        return None;
    }
    // Only our hub hosts. github/gitlab/other remotes fall straight through to git's own helpers. The
    // push-side wiring already scopes us to the hub host; this is defense in depth.
    if !is_hub(host) {
        return None;
    }
    // No account to auth as (never enrolled, and AGIT_HUB_USER unset) -> fall back to git's Basic prompt.
    let username = username?;
    let base = format!("{protocol}://{host}");
    let token = mint(&base)?;
    Some((username, token))
}

/// Parse git's credential protocol off `reader`: `key=value` lines until a blank line (or EOF). Unknown
/// keys are kept (harmless); a line without `=` is ignored.
fn parse_kv(reader: impl BufRead) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        // A blank line terminates the request per git's protocol.
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

/// The hub account this machine auths as: `AGIT_HUB_USER` wins (an explicit override), else the name
/// persisted at `agit identity register <you>` time in `$AGIT_HOME/identity/hub-account`. `None` when
/// neither is set — the helper then stays silent.
pub fn hub_account() -> Option<String> {
    if let Ok(v) = std::env::var("AGIT_HUB_USER") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let text = std::fs::read_to_string(hub_account_path()?).ok()?;
    let v = text.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Persist `<you>` as the hub account for the credential helper. Best-effort: a write failure is not
/// fatal to `agit identity register` (it only costs the zero-config helper, which can still use
/// `AGIT_HUB_USER`).
pub fn persist_hub_account(username: &str) -> Result<()> {
    let Some(path) = hub_account_path() else { return Ok(()) };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{}\n", username.trim()))?;
    Ok(())
}

fn hub_account_path() -> Option<PathBuf> {
    scope::agit_home().ok().map(|h| h.join("identity").join("hub-account"))
}

// ─────────────────────────── the hub-host predicate ───────────────────────────

/// Is `host` one of the hubs this machine knows — `AGIT_HUB_URL`'s host, or a host of the active agent's
/// bound store remotes? Compared case-insensitively on the full `host[:port]` authority.
fn is_hub_host(host: &str) -> bool {
    hub_host_candidates().iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// The hub host(s) this machine knows, for `agit a push` to scope git's credential helper to (so
/// github/gitlab/other remotes are never touched). Public wrapper over [`hub_host_candidates`].
pub fn hub_hosts() -> Vec<String> {
    let mut hosts = hub_host_candidates();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// The `-c credential.https://<host>.helper=<exe> credential` git options that make git run THIS binary
/// as its credential helper for each hub `host`. Uses the current executable's absolute path (via
/// `std::env::current_exe`) so it resolves even when `agit` is not on `PATH`. Empty when the exe can't be
/// found or there are no hub hosts, in which case the command runs exactly as before. Scoped per host, so
/// a non-hub remote (github/gitlab) never triggers the helper — git only consults it for a matching host.
///
/// Shared by every agit command that shells out to git against a hub (`push`, `pull`, `fetch`, `clone`),
/// so key-auth is wired identically everywhere.
pub fn credential_helper_args(hosts: &[String]) -> Vec<String> {
    let Ok(exe) = std::env::current_exe() else {
        return vec![];
    };
    let exe = exe.display().to_string();
    let mut out = Vec::new();
    for host in hosts {
        out.push("-c".to_string());
        // git parses the helper value as a command line, so the exe path plus the `credential` subcommand
        // is invoked as `<exe> credential <get|store|erase>`.
        out.push(format!("credential.https://{host}.helper={exe} credential"));
    }
    out
}

/// Decide whether a `git clone <url>` should wire key-auth, and how. A clone runs with NO bound agent, so
/// [`hub_hosts`] cannot resolve the clone host; instead we classify the url directly. Returns
/// `Some((host, base))` when `url` is a hub URL — `host` scopes the credential helper `-c` option, `base`
/// (`scheme://host[:port]`) is set as `AGIT_HUB_URL` in the git child's env so the spawned helper's
/// `is_hub_host()` recognises the host and mints. Returns `None` for a github/gitlab/ssh/local url, for
/// which the clone must stay byte-identical to before (no `-c`, no env change).
pub fn clone_cred_plan(url: &str, trusted_hosts: &[String]) -> Option<String> {
    // A clone URL is UNTRUSTED input, so it must never self-certify as a hub. We inject the credential
    // helper ONLY when the URL's host is ALREADY a declared hub for this machine (in `trusted_hosts`,
    // which come from AGIT_HUB_URL and any bound-store remotes). Otherwise an arbitrary https store
    // (github.com, or an attacker's URL implementing /api/auth/challenge) would make the machine sign a
    // challenge and POST the account username + ed25519 public key to whoever the URL points at. Requiring
    // a pre-declared hub is the trust anchor; a non-matching or non-http URL yields None and the clone
    // falls back to git's own credential prompt. The already-set AGIT_HUB_URL is inherited by the helper
    // git spawns, so no environment needs to be forged here.
    let host = hubapi::hub_host(url)?;
    trusted_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)).then_some(host)
}

/// The hosts that count as "a hub" for this machine: `AGIT_HUB_URL` (if set) plus every http(s) remote of
/// the active agent's store. Resolution failures are simply skipped — an empty list means "no hub known",
/// so the helper stays silent (safe).
fn hub_host_candidates() -> Vec<String> {
    let mut hosts = Vec::new();
    if let Ok(u) = std::env::var("AGIT_HUB_URL") {
        if let Some(h) = hubapi::hub_host(u.trim()) {
            hosts.push(h);
        }
    }
    if let Ok(agent) = agent::resolve(None) {
        for (_, url) in agent::store_remotes(&agent.store) {
            if let Some(h) = hubapi::hub_host(&url) {
                hosts.push(h);
            }
        }
    }
    hosts
}

// ─────────────────────────── the on-disk token cache ───────────────────────────
//
// Within one push git may invoke the helper more than once (info/refs then receive-pack). Since each
// invocation is a fresh `agit` process, the only cache that spans them is on disk. It is a 0600 JSON map
// `host -> { token, expires_at }` under `$AGIT_HOME/identity/`, so a push signs ONCE, not once per
// request. A cached token is reused only while it has comfortable life left; otherwise it is re-minted.

fn cache_path() -> Option<PathBuf> {
    scope::agit_home().ok().map(|h| h.join("identity").join("hub-tokens.json"))
}

fn read_cache() -> serde_json::Value {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A still-fresh cached token for `host`, or `None` if absent/expiring-soon/malformed.
fn cache_lookup(host: &str) -> Option<String> {
    let cache = read_cache();
    let entry = cache.get(host)?;
    let token = entry.get("token").and_then(|t| t.as_str())?;
    let expires_at = entry.get("expires_at").and_then(|e| e.as_i64()).unwrap_or(0);
    (expires_at > now_unix() + CACHE_MIN_REMAINING_SECS).then(|| token.to_string())
}

/// Record a freshly minted token for `host` (0600). Best-effort: a failure only costs the cache.
fn cache_store(host: &str, token: &str, expires_at: i64) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut cache = read_cache();
    if let Some(obj) = cache.as_object_mut() {
        obj.insert(host.to_string(), serde_json::json!({ "token": token, "expires_at": expires_at }));
    }
    if let Ok(text) = serde_json::to_string(&cache) {
        // A bearer token is a secret at rest: write it owner-only, never world-readable.
        let _ = agent::write_secret_0600(&path, &text);
    }
}

/// Drop any cached token for `host` (git `erase`).
fn cache_forget(host: &str) {
    let Some(path) = cache_path() else { return };
    let mut cache = read_cache();
    if let Some(obj) = cache.as_object_mut() {
        if obj.remove(host).is_some() {
            if let Ok(text) = serde_json::to_string(&cache) {
                let _ = agent::write_secret_0600(&path, &text);
            }
        }
    }
}

/// A token for `base` (`scheme://host[:port]`): a still-fresh cached one, else a freshly minted one that
/// is then cached. `None` on any handshake failure (the helper then stays silent).
fn cache_or_mint(base: &str, username: &str) -> Option<String> {
    let host = base.split_once("://").map(|(_, h)| h).unwrap_or(base);
    if let Some(tok) = cache_lookup(host) {
        return Some(tok);
    }
    let (token, expires_at) = hubapi::mint_key_token(base, username)?;
    cache_store(host, &token, expires_at);
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// stdin parsing follows git's protocol: key=value lines, terminated by a blank line; anything after
    /// the blank line is NOT read.
    #[test]
    fn parse_kv_reads_until_blank_line() {
        let raw = "protocol=https\nhost=hub.example.com\npath=alice/frontend.git\n\ntrailing=ignored\n";
        let map = parse_kv(std::io::Cursor::new(raw));
        assert_eq!(map.get("protocol").map(String::as_str), Some("https"));
        assert_eq!(map.get("host").map(String::as_str), Some("hub.example.com"));
        assert_eq!(map.get("path").map(String::as_str), Some("alice/frontend.git"));
        assert!(!map.contains_key("trailing"), "nothing past the blank line is read");
    }

    /// A `get` on a HUB host emits the account username and the minted token as the password.
    #[test]
    fn get_on_a_hub_host_emits_password() {
        let inp = input(&[("protocol", "https"), ("host", "hub.example.com")]);
        let got = resolve_get(
            &inp,
            |h| h == "hub.example.com",
            Some("alice".to_string()),
            |base| {
                assert_eq!(base, "https://hub.example.com", "the minter sees scheme://host");
                Some("tok_minted_123".to_string())
            },
        );
        assert_eq!(got, Some(("alice".to_string(), "tok_minted_123".to_string())));
    }

    /// A `get` on a NON-hub host emits nothing (git falls through to its own helpers) — the minter is
    /// never even called.
    #[test]
    fn get_on_a_non_hub_host_emits_nothing() {
        let inp = input(&[("protocol", "https"), ("host", "github.com")]);
        let got = resolve_get(
            &inp,
            |h| h == "hub.example.com", // github.com is NOT our hub
            Some("alice".to_string()),
            |_| panic!("the minter must never run for a non-hub host"),
        );
        assert_eq!(got, None);
    }

    /// With no account (never enrolled, AGIT_HUB_USER unset) the helper stays silent even on a hub host.
    #[test]
    fn get_without_an_account_emits_nothing() {
        let inp = input(&[("protocol", "https"), ("host", "hub.example.com")]);
        let got = resolve_get(&inp, |_| true, None, |_| Some("tok".to_string()));
        assert_eq!(got, None);
    }

    /// A non-http(s) protocol (or a missing host) is silent.
    #[test]
    fn get_ignores_non_http_protocols_and_missing_host() {
        let ssh = input(&[("protocol", "ssh"), ("host", "hub.example.com")]);
        assert_eq!(resolve_get(&ssh, |_| true, Some("alice".into()), |_| Some("t".into())), None);
        let no_host = input(&[("protocol", "https")]);
        assert_eq!(resolve_get(&no_host, |_| true, Some("alice".into()), |_| Some("t".into())), None);
    }

    /// The credential-helper handshake signs the KEY-AUTH assertion, which is DOMAIN-SEPARATED from the
    /// enroll message: the two canonical byte strings carry distinct fixed prefixes, so a signature made
    /// for one protocol can never be replayed as the other. (A direct byte/prefix assertion, mirroring
    /// the hub-side guarantee.)
    #[test]
    fn auth_assertion_is_domain_separated_from_enroll() {
        let auth = agent::identity_auth_message("https://hub.example.com", "alice", "aa", "nonce", 42);
        let enroll = agent::identity_enroll_message("alice", 42, "aa", "bb");
        assert!(auth.starts_with(b"agit-hub-auth-v1\n"), "auth carries the auth-v1 domain prefix");
        assert!(enroll.starts_with(b"agit-identity-enroll-v1\n"), "enroll carries the enroll-v1 domain prefix");
        assert!(!auth.starts_with(b"agit-identity-enroll-v1"), "the auth prefix is NOT the enroll prefix");
        assert_ne!(auth, enroll);
    }

    /// One `-c credential.https://<host>.helper=<exe> credential` pair per host: two entries per host (the
    /// `-c` flag and its value), the value carrying the current exe path and the ` credential` subcommand.
    #[test]
    fn credential_helper_args_emits_one_scoped_pair_per_host() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let args = credential_helper_args(&["h1".to_string(), "h2".to_string()]);
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                format!("credential.https://h1.helper={exe} credential"),
                "-c".to_string(),
                format!("credential.https://h2.helper={exe} credential"),
            ]
        );
        // Each value is scoped to ONE host and names the exe + subcommand git will spawn.
        assert!(args[1].contains(&exe) && args[1].ends_with(" credential"));
    }

    /// No hosts -> no options at all: the git command runs exactly as it would without key-auth.
    #[test]
    fn credential_helper_args_is_empty_for_no_hosts() {
        assert!(credential_helper_args(&[]).is_empty());
    }

    /// A clone url whose host is ALREADY a declared hub yields that host to scope the `-c` helper to. The
    /// user's AGIT_HUB_URL (why the host is trusted) is inherited by the spawned helper, so no env is forged.
    #[test]
    fn clone_cred_plan_injects_only_for_a_declared_hub() {
        let trusted = vec!["hub.example.com".to_string()];
        assert_eq!(
            clone_cred_plan("https://hub.example.com/alice/frontend.git", &trusted),
            Some("hub.example.com".to_string())
        );
        // A port is part of the authority, so the trusted host must match it too.
        let ported = vec!["localhost:8080".to_string()];
        assert_eq!(
            clone_cred_plan("http://localhost:8080/alice/x.git", &ported),
            Some("localhost:8080".to_string())
        );
    }

    /// The security boundary: an arbitrary https url whose host is NOT a declared hub yields None, so an
    /// attacker's (or github/gitlab's) https store can never trigger a signed challenge and leak the
    /// account username + public key. This is hub-vs-non-hub, not merely http-vs-non-http.
    #[test]
    fn clone_cred_plan_refuses_an_undeclared_https_host() {
        let trusted = vec!["hub.example.com".to_string()];
        assert_eq!(clone_cred_plan("https://github.com/alice/frontend.git", &trusted), None);
        assert_eq!(clone_cred_plan("https://gitlab.com/alice/frontend.git", &trusted), None);
        assert_eq!(clone_cred_plan("https://evil.example.net/alice/store.git", &trusted), None);
        // Empty trusted set (no declared hub, e.g. AGIT_HUB_URL unset at clone time) -> never inject.
        assert_eq!(clone_cred_plan("https://hub.example.com/alice/x.git", &[]), None);
    }

    /// An ssh (`git@host:path`) or local url yields None even when the host string is a declared hub: the
    /// mint handshake is an http API, so key-auth is https-only. The clone stays byte-identical to before.
    #[test]
    fn clone_cred_plan_leaves_non_http_urls_alone() {
        let trusted = vec!["hub.example.com".to_string()];
        assert_eq!(clone_cred_plan("git@github.com:alice/frontend.git", &trusted), None);
        assert_eq!(clone_cred_plan("git@gitlab.com:alice/frontend.git", &trusted), None);
        assert_eq!(clone_cred_plan("ssh://git@hub.example.com/alice/x.git", &trusted), None);
        assert_eq!(clone_cred_plan("/srv/local/store.git", &trusted), None);
    }
}
