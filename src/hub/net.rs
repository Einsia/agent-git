//! **Pure parsing** at the HTTP boundary: a git smart-http path → (which agent, read or write), and
//! the real client IP behind a proxy.
//!
//! Both have to be pure functions with exhaustive tests — they decide "which agent the authorization
//! check is handed" and "whose account rate limiting charges". The old code had no function for
//! either: the path check was an inline `path.contains(".git/")`, and rate limiting counted the raw
//! peer IP.

use super::acl::Action;
use std::net::IpAddr;

/// Which agent a git smart-http request hits, and what permission it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRoute {
    pub agent: String,
    pub action: Action,
}

/// agent name rules: [A-Za-z0-9._-] only; no `..`, no leading `.`, no path separators, no NUL.
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.contains("..")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// smart-http path → route. Anything outside `/<name>.git/...` is None (not a git request).
///
/// This is the **input to the authorization point**: the old code only asked "does the path contain
/// `.git/`?" and then handed the request to http-backend with `GIT_HTTP_EXPORT_ALL=1` — so once past
/// the "read gate", any repo under root could be pulled. Authorizing at all requires knowing
/// **which repo** first.
pub fn parse_git_path(path: &str, query: &str) -> Option<GitRoute> {
    let rest = path.strip_prefix('/')?;
    let (first, tail) = rest.split_once('/')?; // `/x.git` alone is not a git endpoint; a further segment is required
    let name = first.strip_suffix(".git")?;
    if !valid_agent_name(name) || tail.is_empty() {
        return None;
    }
    let action = match needs_write(path, query) {
        true => Action::Write,
        false => Action::Read,
    };
    Some(GitRoute { agent: name.to_string(), action })
}

/// Whether this request **writes** the repo.
///
/// The test is deliberately conservative: a path hit on `git-receive-pack`, **or**
/// `git-receive-pack` appearing in the query (after percent-decoding), both count as a write.
/// Better to demand write permission once too often than to wave a push through as a read — the only
/// direction a misjudgement may err is toward "demand more permission".
fn needs_write(path: &str, query: &str) -> bool {
    path.ends_with("/git-receive-pack") || percent_decode_lossy(query).contains("git-receive-pack")
}

/// Percent-decoding. Escapes it cannot decode are left as-is — the point here is only "does this
/// mention receive-pack?", not faithful reconstruction.
fn percent_decode_lossy(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = hex_pair(b[i + 1], b[i + 2]) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(a: u8, b: u8) -> Option<u8> {
    let d = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    Some(d(a)? << 4 | d(b)?)
}

/// Whose account rate limiting charges.
///
/// By default (no trusted proxies declared): **use the raw peer IP**, matching the old behaviour —
/// anyone can forge `X-Forwarded-For`, and trusting it on a direct connection gives the per-IP cap
/// away for free.
/// Once trusted proxies are declared: when the peer is a proxy, walk XFF right to left and take the
/// first address that is **not** a trusted proxy — the rightmost was just appended by the nearest
/// (trusted) proxy, while anything to the left can be forged freely by the client.
pub fn client_ip(peer: IpAddr, xff: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    let Some(xff) = xff else {
        return peer;
    };
    for hop in xff.rsplit(',') {
        // Unparseable = XFF is malformed or poisoned: do not guess, fall back to peer (the cost is
        // that these requests share the proxy's quota — conservative, but it does not give the rate
        // limit away).
        let Ok(ip) = hop.trim().parse::<IpAddr>() else {
            return peer;
        };
        if !trusted.contains(&ip) {
            return ip;
        }
    }
    peer // The whole chain is trusted proxies — charge the proxy itself
}

/// `--trusted-proxy 10.0.0.1,10.0.0.2` → an IP list. Unrecognized entries error out (never silently
/// ignore a mistyped proxy address — that would leave you believing rate limiting keys on real IPs
/// when it does not).
pub fn parse_trusted_proxies(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<IpAddr>().map_err(|_| format!("not a valid IP: {x}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    // ── agent names ──

    #[test]
    fn agent_name_rejects_traversal_and_seps() {
        assert!(valid_agent_name("alice"));
        assert!(valid_agent_name("team-store_2"));
        assert!(valid_agent_name("a.b"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name(".."));
        assert!(!valid_agent_name("../etc/passwd"));
        assert!(!valid_agent_name("a/b"));
        assert!(!valid_agent_name(".hidden"));
        assert!(!valid_agent_name("a..b"));
        assert!(!valid_agent_name("a\0b"));
        assert!(!valid_agent_name(&"x".repeat(65)));
    }

    // ── git path → route ──

    #[test]
    fn fetch_paths_need_read() {
        let r = parse_git_path("/alice.git/info/refs", "service=git-upload-pack").unwrap();
        assert_eq!(r, GitRoute { agent: "alice".into(), action: Action::Read });
        assert_eq!(parse_git_path("/alice.git/git-upload-pack", "").unwrap().action, Action::Read);
        // dumb-protocol paths are reads too
        assert_eq!(parse_git_path("/alice.git/HEAD", "").unwrap().action, Action::Read);
        assert_eq!(parse_git_path("/alice.git/objects/info/packs", "").unwrap().action, Action::Read);
    }

    #[test]
    fn push_paths_need_write() {
        assert_eq!(parse_git_path("/alice.git/git-receive-pack", "").unwrap().action, Action::Write);
        // info/refs?service=git-receive-pack is push's first step — it must demand write too, or a
        // read-only user could use it to probe the private branch layout.
        assert_eq!(
            parse_git_path("/alice.git/info/refs", "service=git-receive-pack").unwrap().action,
            Action::Write
        );
    }

    #[test]
    fn percent_encoded_receive_pack_still_needs_write() {
        // http-backend decodes the query itself. Not decoding here would wave a push through as a read.
        let r = parse_git_path("/alice.git/info/refs", "service=git%2Dreceive%2Dpack").unwrap();
        assert_eq!(r.action, Action::Write);
        let r = parse_git_path("/alice.git/info/refs", "service=git%2dreceive%2dpack").unwrap();
        assert_eq!(r.action, Action::Write);
    }

    #[test]
    fn write_classification_errs_toward_write() {
        // Stuffing in extra parameters does not talk it down to the "read" tier.
        let r = parse_git_path("/alice.git/info/refs", "a=1&service=git-receive-pack&b=2").unwrap();
        assert_eq!(r.action, Action::Write);
    }

    #[test]
    fn non_git_paths_are_not_git_routes() {
        assert_eq!(parse_git_path("/api/agents", ""), None);
        assert_eq!(parse_git_path("/", ""), None);
        assert_eq!(parse_git_path("/alice", ""), None);
        assert_eq!(parse_git_path("/alice.git", ""), None); // no further segment
        assert_eq!(parse_git_path("/alice.git/", ""), None);
    }

    #[test]
    fn traversal_in_git_path_is_refused() {
        // The old code's `path.contains(".git/")` would send every one of these straight into
        // http-backend.
        assert_eq!(parse_git_path("/../etc.git/info/refs", ""), None);
        assert_eq!(parse_git_path("/..%2f.git/info/refs", ""), None);
        assert_eq!(parse_git_path("/a/b.git/info/refs", ""), None, "only <name>.git at the root counts");
        assert_eq!(parse_git_path("/.hidden.git/info/refs", ""), None);
        assert_eq!(parse_git_path("/a..b.git/info/refs", ""), None);
    }

    #[test]
    fn nested_repo_path_cannot_escape_to_another_repo() {
        // `/alice.git/../bob.git/info/refs` — the name comes from the first segment, and `..` is
        // stopped by valid_agent_name; the HTTP layer also has a blanket `..` gate. This confirms
        // parsing itself does not read it as anything but alice.
        assert_eq!(parse_git_path("/alice.git/../bob.git/info/refs", "").unwrap().agent, "alice");
    }

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode_lossy("a%2Db"), "a-b");
        assert_eq!(percent_decode_lossy("a+b"), "a+b");
        assert_eq!(percent_decode_lossy("%zz"), "%zz");
        assert_eq!(percent_decode_lossy("%2"), "%2");
        assert_eq!(percent_decode_lossy(""), "");
    }

    // ── the real IP behind a proxy ──

    #[test]
    fn without_trusted_proxy_xff_is_ignored() {
        // Anyone can forge XFF. With no proxy declared it must key on the raw peer, or the per-IP
        // cap may as well not exist.
        let peer = ip("203.0.113.9");
        assert_eq!(client_ip(peer, Some("1.2.3.4"), &[]), peer);
        assert_eq!(client_ip(peer, Some("1.2.3.4, 5.6.7.8"), &[ip("10.0.0.1")]), peer);
    }

    #[test]
    fn trusted_proxy_yields_the_real_client() {
        // Behind a proxy every user shares one peer IP — ignore XFF and they knock each other
        // offline.
        let proxy = ip("10.0.0.1");
        let trusted = vec![proxy];
        assert_eq!(client_ip(proxy, Some("203.0.113.9"), &trusted), ip("203.0.113.9"));
        assert_eq!(client_ip(proxy, Some("203.0.113.9, 10.0.0.1"), &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn spoofed_left_hand_hops_are_ignored() {
        // XFF the client stuffed in itself sits to the left; only the rightmost was just appended by
        // the trusted proxy.
        let proxy = ip("10.0.0.1");
        let trusted = vec![proxy];
        assert_eq!(client_ip(proxy, Some("6.6.6.6, 203.0.113.9"), &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn chain_of_trusted_proxies_walks_left() {
        let (p1, p2) = (ip("10.0.0.1"), ip("10.0.0.2"));
        let trusted = vec![p1, p2];
        assert_eq!(client_ip(p1, Some("203.0.113.9, 10.0.0.2"), &trusted), ip("203.0.113.9"));
        // The whole chain is proxies → charge the proxy itself
        assert_eq!(client_ip(p1, Some("10.0.0.2, 10.0.0.1"), &trusted), p1);
    }

    #[test]
    fn junk_xff_falls_back_to_peer() {
        let proxy = ip("10.0.0.1");
        let trusted = vec![proxy];
        assert_eq!(client_ip(proxy, Some("not-an-ip"), &trusted), proxy);
        assert_eq!(client_ip(proxy, Some(""), &trusted), proxy);
        assert_eq!(client_ip(proxy, None, &trusted), proxy);
        assert_eq!(client_ip(proxy, Some("203.0.113.9, bogus"), &trusted), proxy);
    }

    #[test]
    fn trusted_proxy_parsing() {
        assert_eq!(parse_trusted_proxies("10.0.0.1").unwrap(), vec![ip("10.0.0.1")]);
        assert_eq!(parse_trusted_proxies("10.0.0.1, ::1").unwrap(), vec![ip("10.0.0.1"), ip("::1")]);
        assert!(parse_trusted_proxies("").unwrap().is_empty());
        assert!(parse_trusted_proxies("10.0.0.1,nope").is_err(), "a mistyped proxy address must error, not be silently ignored");
    }
}
