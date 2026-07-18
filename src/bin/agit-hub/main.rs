//! agit-hub — AgentGitHub: hosts the team's Agent Stores, readable by people (React SPA) and pullable
//! by agents (JSON API). CLI entry + subcommand dispatch (sync); the HTTP service is on axum + tokio
//! in the submodules.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]

mod api;
mod cli;
mod content;
mod gitplumb;
mod http;
mod limits;
mod router;
mod scan;
mod server;
mod smarthttp;

use std::path::PathBuf;

use crate::cli::{add_cmd, list_cmd, token_cmd, user_cmd};
use crate::scan::pre_receive_cmd;
use crate::server::serve_cmd;

fn main() {
    std::process::exit(run());
}

/// Returns the process exit code — error paths must be non-zero so scripts/CI can notice the
/// failure (don't just exit 0 everywhere).
pub(crate) fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");
    let root = flag(&args, "--root").map(PathBuf::from).unwrap_or_else(default_root);

    match cmd {
        "serve" => serve_cmd(&root, &args),
        "add" => add_cmd(&root, &args),
        "list" => list_cmd(&root),
        "token" => token_cmd(&root, &args),
        "user" => user_cmd(&root, &args),
        "pre-receive" => pre_receive_cmd(&root, &args),
        "-h" | "--help" => {
            print_help();
            0
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            2
        }
    }
}

pub(crate) fn print_help() {
    println!(
        "agit-hub — AgentGitHub (Registry + Sync)\n\n\
         agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]\n\
                        [--tls] [--insecure] [--trusted-proxy IP,IP]      start the Hub\n\
         agit-hub user add <name> [--admin]                   add a user (asks for the password)\n\
         agit-hub user list                                   list users\n\
         agit-hub add <name> [--owner <user>] [--public]      new Agent Store (private by default)\n\
         agit-hub list                                        list hosted agents\n\
         agit-hub token add <name> [--user <owner>] [--agent <name>]\n\
                            [--read|--write] [--ttl-days N]   issue an access token\n\
         agit-hub token list                                  list tokens (metadata only)\n\
         agit-hub token rm <id>                               revoke a token\n\n\
         First step: agit-hub user add <you> --admin\n\
         Hosted repos are bare git. Publish with: agit -a push http://HOST:PORT/<name>.git (with a write token)\n\n\
         Listens on 127.0.0.1 only by default. Serving the network needs --host 0.0.0.0, and without TLS also --insecure."
    );
}

pub(crate) fn default_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agit-hub")
}

pub(crate) fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

pub(crate) fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// The first positional argument not starting with `--` (skipping the first `skip` tokens).
pub(crate) fn positional(args: &[String], skip: usize) -> Option<&String> {
    args.iter().skip(skip).find(|s| !s.starts_with("--"))
}

#[cfg(test)]
mod tests {
    use crate::api::*;
    use crate::content::*;
    use crate::gitplumb::*;
    use crate::http::*;
    use crate::limits::*;
    use crate::router::*;
    use crate::scan::*;
    use crate::server::*;

    use agit::hub::acl::{AgentAcl, Caller, Deny, Lifecycle, Role, Scope, Visibility};
    use std::time::{Duration, Instant};

    #[test]
    fn base64_decodes_basic_credentials() {
        assert_eq!(b64_decode("Z2l0OnNlY3JldDEyMw==").unwrap(), b"git:secret123");
        assert_eq!(b64_decode("YQ").unwrap(), b"ab".split_at(1).0);
        assert_eq!(b64_decode("YWI").unwrap(), b"ab");
    }

    #[test]
    fn credentials_come_from_basic_and_bearer() {
        let req = |auth: &str| Req {
            method: "GET".into(),
            target: "/".into(),
            headers: vec![("Authorization".into(), auth.into())],
            content_length: 0,
        };
        // git puts the token in the password field — treat both halves as candidates.
        assert_eq!(credentials(&req("Basic Z2l0OnNlY3JldDEyMw==")), vec!["secret123", "git"]);
        assert_eq!(credentials(&req("Bearer abc")), vec!["abc"]);
        assert_eq!(credentials(&req("bearer abc")), vec!["abc"]);
        assert!(credentials(&req("")).is_empty());
        let no_auth = Req { method: "GET".into(), target: "/".into(), headers: vec![], content_length: 0 };
        assert!(credentials(&no_auth).is_empty());
    }

    #[test]
    fn cookie_sid_is_read_from_the_header() {
        let req = Req {
            method: "GET".into(),
            target: "/".into(),
            headers: vec![("Cookie".into(), "a=1; agit_session=deadbeef".into())],
            content_length: 0,
        };
        assert_eq!(req.sid().as_deref(), Some("deadbeef"));
    }

    // ── The bind gate (requirement 4) ──

    #[test]
    fn loopback_binds_without_ceremony() {
        assert!(bind_guard("127.0.0.1".parse().unwrap(), false, false).is_ok());
        assert!(bind_guard("::1".parse().unwrap(), false, false).is_ok());
    }

    #[test]
    fn public_bind_without_tls_is_refused() {
        let e = bind_guard("0.0.0.0".parse().unwrap(), false, false).unwrap_err();
        // A refusal must **say why**, not just "refused".
        assert!(e.contains("plaintext"), "{e}");
        assert!(e.contains("--insecure"), "{e}");
        assert!(e.contains("--tls"), "{e}");
        assert!(bind_guard("192.168.1.10".parse().unwrap(), false, false).is_err());
    }

    #[test]
    fn public_bind_needs_tls_or_explicit_insecure() {
        assert!(bind_guard("0.0.0.0".parse().unwrap(), true, false).is_ok(), "--tls lets it through");
        assert!(bind_guard("0.0.0.0".parse().unwrap(), false, true).is_ok(), "--insecure lets it through");
    }

    // ── Session layouts: both the new and the old must be recognized ──

    #[test]
    fn runtimes_are_sorted_peers() {
        // claude-code and codex are peers — alphabetical, neither is "first by default".
        let refs = vec![
            SessionRef { env: None, runtime: "codex".into(), id: "a".into(), path: "sessions/codex/a.jsonl".into() },
            SessionRef { env: None, runtime: "claude-code".into(), id: "b".into(), path: "sessions/claude-code/b.jsonl".into() },
            SessionRef { env: None, runtime: "codex".into(), id: "c".into(), path: "sessions/codex/c.jsonl".into() },
        ];
        assert_eq!(runtimes(&refs), vec!["claude-code", "codex"]);
    }

    #[test]
    fn param_extracts_query_values() {
        assert_eq!(param("a=1&b=2", "b").as_deref(), Some("2"));
        assert_eq!(param("a=1", "b"), None);
        assert_eq!(param("", "b"), None);
        assert_eq!(param("service=git-receive-pack", "service").as_deref(), Some("git-receive-pack"));
    }

    // ── Which status a denial gets: the policy here is "don't leak existence" ──

    fn private_acl() -> AgentAcl {
        AgentAcl {
            name: "secret".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Private,
            lifecycle: Lifecycle::Active,
            members: vec![],
        }
    }

    #[test]
    fn anonymous_denial_is_401_so_the_spa_can_offer_login() {
        let r = deny_resp(&Caller::anonymous(), &private_acl(), Deny::Anonymous);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn denied_stranger_gets_404_not_403() {
        // A 403 admits "this agent exists" — that's an interface for enumerating private agent names.
        // Anyone who can't read gets a 404, identical to "doesn't exist".
        let r = deny_resp(&Caller::user("eve"), &private_acl(), Deny::NoGrant);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn reader_denied_a_write_gets_403() {
        // Someone who can read already knows it exists; nothing left to hide — give them the real reason.
        let acl = AgentAcl {
            name: "secret".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Private,
            lifecycle: Lifecycle::Active,
            members: vec![("bob".into(), Role::Read)],
        };
        let r = deny_resp(&Caller::user("bob"), &acl, Deny::NoGrant);
        assert_eq!(r.status, 403);
    }

    #[test]
    fn token_denials_on_a_readable_agent_are_403() {
        let caller = Caller::user("alice").with_token(None, Scope::Read);
        assert_eq!(deny_resp(&caller, &private_acl(), Deny::TokenScope).status, 403);
        assert_eq!(deny_resp(&caller, &private_acl(), Deny::TokenCannotManage).status, 403);
    }

    #[test]
    fn git_anonymous_denial_challenges_so_git_asks_for_credentials() {
        // Without this header, `git clone` won't ask the user for a password, it just errors out.
        let r = git_deny_resp(&Caller::anonymous(), Deny::Anonymous);
        assert_eq!(r.status, 401);
        assert!(r.extra.iter().any(|(k, v)| k == "WWW-Authenticate" && v.contains("Basic")));
        // Don't challenge someone already authenticated — asking for the password again yields the same answer.
        let r = git_deny_resp(&Caller::user("eve"), Deny::NoGrant);
        assert_eq!(r.status, 403);
        assert!(r.extra.is_empty());
    }

    // ── the per-token budget ──

    #[test]
    fn a_token_spends_its_burst_and_is_then_refused() {
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        for i in 0..TOKEN_BURST as usize {
            assert!(rl.allow_at("tok_a", t0), "request {i} is still inside the burst");
        }
        assert!(!rl.allow_at("tok_a", t0), "the burst is spent — the next one must be refused");
    }

    #[test]
    fn the_budget_refills_over_time() {
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        for _ in 0..TOKEN_BURST as usize {
            rl.allow_at("tok_a", t0);
        }
        assert!(!rl.allow_at("tok_a", t0));
        // One second later there is a second's worth of refill and no more.
        let t1 = t0 + Duration::from_secs(1);
        for i in 0..TOKEN_RATE_PER_SEC as usize {
            assert!(rl.allow_at("tok_a", t1), "refilled request {i}");
        }
        assert!(!rl.allow_at("tok_a", t1), "the refill is the rate, not a fresh burst");
    }

    #[test]
    fn the_refill_never_exceeds_the_burst() {
        // An idle token comes back with a full bucket, not an unbounded one — otherwise a token
        // left alone for a day would bank a day's worth of requests.
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        rl.allow_at("tok_a", t0);
        let later = t0 + Duration::from_secs(86_400);
        for _ in 0..TOKEN_BURST as usize {
            assert!(rl.allow_at("tok_a", later));
        }
        assert!(!rl.allow_at("tok_a", later), "a day idle must not bank a day of requests");
    }

    #[test]
    fn one_tokens_budget_is_not_anothers() {
        // The whole point of charging the credential: a wedged CI token must not lock out the token
        // next to it (which the per-IP cap would, when both sit behind one NAT).
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        for _ in 0..TOKEN_BURST as usize {
            rl.allow_at("tok_a", t0);
        }
        assert!(!rl.allow_at("tok_a", t0));
        assert!(rl.allow_at("tok_b", t0), "a different token has its own budget");
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_panic() {
        // Instant subtraction panics on a negative delta, and two threads can read the clock out of
        // order. Refusing to crash here matters more than the arithmetic being exact.
        let rl = TokenBuckets::new();
        let t1 = Instant::now() + Duration::from_secs(10);
        assert!(rl.allow_at("tok_a", t1));
        assert!(rl.allow_at("tok_a", t1 - Duration::from_secs(5)));
    }

    // ── the pre-receive secret gate ──

    #[test]
    fn batch_output_splits_on_the_declared_size_not_on_newlines() {
        // Blob content contains newlines; splitting on them would cut a blob into pieces and scan
        // the header line as if it were content. git's shape is exactly: `<sha> <type> <size>\n`,
        // then <size> bytes, then a separator newline of its own.
        let raw = b"aaa blob 11\nline1\nline2\nbbb blob 3\nxyz\n";
        let out = parse_batch(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out["aaa"], b"line1\nline2");
        assert_eq!(out["bbb"], b"xyz", "the separator newline must not be eaten out of the next header");
    }

    #[test]
    fn a_missing_object_does_not_shift_every_later_blob_onto_the_wrong_path() {
        // The bug this keys on: "<sha> missing" yields no body, so the old positional zip paired
        // "hi" with the MISSING object's path and every blob after it with its predecessor's. The
        // rejection then named the wrong file — and the path is the whole actionable half of it.
        let raw = b"deadbeef missing\naaa blob 2\nhi\nbbb blob 3\nkey\n";
        let out = parse_batch(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out["aaa"], b"hi");
        assert_eq!(out["bbb"], b"key");
        assert!(!out.contains_key("deadbeef"), "a missing object must contribute no body at all");
        assert!(parse_batch(b"").is_empty());
    }

    #[test]
    fn printable_runs_find_a_key_a_nul_byte_used_to_hide() {
        // One NUL used to skip the blob whole and silently: this is the bypass that made the gate a
        // liar. The strings pass has to still see the key.
        let mut blob = vec![0u8, 1, 2];
        blob.extend_from_slice(b"aws_access_key_id = AKIAIOSFODNN7EXAMPLE");
        blob.extend_from_slice(&[0u8, 0xff]);
        let runs = printable_runs(&blob);
        assert!(runs.contains("AKIAIOSFODNN7EXAMPLE"), "{runs:?}");
        // entropy off, as scan_push runs it for binary: the named rule must not depend on it.
        let hits = agit::scan::scan_text_allow(&runs, false, &agit::scan::Allowlist::empty());
        let rules: Vec<&str> = hits.iter().map(|f| f.rule).collect();
        assert!(rules.contains(&"aws-access-key-id"), "{rules:?}");
    }

    #[test]
    fn printable_runs_drop_the_noise_between_them() {
        // Runs shorter than the minimum are the incidental bytes of any binary — keeping them would
        // hand the rules a haystack made of chaff.
        assert_eq!(printable_runs(&[0, b'a', b'b', 0, 0xff]), "");
        assert_eq!(printable_runs(b"hello world"), "hello world\n");
        assert_eq!(printable_runs(&[b'l', b'o', b'n', b'g', b'e', b'r', 0, b'x']), "longer\n");
        // A run ending at the very end of the blob is still a run.
        assert_eq!(printable_runs(&[0, b'l', b'o', b'n', b'g', b'e', b'r']), "longer\n");
    }

    #[test]
    fn an_unscanned_blob_makes_the_report_incomplete() {
        // `incomplete()` is what `pre_receive_cmd` refuses on. "Found nothing" and "looked at
        // nothing" must never be the same value.
        let mut r = ScanReport { findings: vec![], unscanned: vec![], errored: None };
        assert!(!r.incomplete(), "a clean, complete scan clears the push");
        r.unscanned.push(("big.bin".into(), "past the per-blob bound".into()));
        assert!(r.incomplete());

        let errored = ScanReport { findings: vec![], unscanned: vec![], errored: Some("git failed".into()) };
        assert!(errored.incomplete(), "an IO failure is not a clean scan");
    }

    // ── pagination ──

    #[test]
    fn a_cursor_roundtrips_and_refuses_junk() {
        for key in ["payments", "1", "a.b-c_d", "42"] {
            assert_eq!(cursor_decode(&cursor_encode(key)).as_deref(), Some(key));
        }
        // Opaque means opaque: it must not read as the key it encodes.
        assert_ne!(cursor_encode("payments"), "payments");
        for bad in ["", "zz", "abc", "payments", "的的"] {
            assert_eq!(cursor_decode(bad), None, "{bad:?} must not decode");
        }
        // A cursor is a resume point, not a place to post a novel.
        assert_eq!(cursor_decode(&"61".repeat(300)), None);
    }

    /// `Resp` is deliberately not Debug (it carries response bodies), so unwrap it by hand.
    fn page_of(query: &str) -> Option<Page> {
        page_params(query).ok()
    }

    #[test]
    fn no_limit_means_everything_not_a_default_page() {
        // The embedded SPA does not know what a cursor is. A default page would cap its list with no
        // way for it to ask for the rest — a silent cap in a UI, which is the thing being avoided.
        let p = page_of("").expect("no params is a valid request");
        assert_eq!(p.limit, usize::MAX);
        assert!(p.after.is_none());
    }

    #[test]
    fn a_limit_is_clamped_and_junk_is_refused_rather_than_ignored() {
        assert_eq!(page_of("limit=5").map(|p| p.limit), Some(5));
        // Over the ceiling is clamped, not an error: asking for too much is not an instruction.
        assert_eq!(page_of("limit=99999").map(|p| p.limit), Some(PAGE_MAX));
        // ...but nonsense is refused, never silently treated as "everything".
        for bad in ["limit=0", "limit=-1", "limit=abc", "limit="] {
            assert!(page_of(bad).is_none(), "{bad:?} must be refused");
        }
        assert!(page_of("cursor=nothex").is_none());
        assert_eq!(page_of(&format!("cursor={}", cursor_encode("payments"))).and_then(|p| p.after).as_deref(), Some("payments"));
    }

    // ── the values that reach a git argv slot ──

    #[test]
    fn a_rev_that_could_become_a_git_option_is_refused() {
        // `git show --output=<file>` WRITES A FILE. The rev is concatenated into `<rev>:<path>`, so a
        // leading `-` turns the whole argument into an option — and this value arrives straight off
        // the query string.
        assert!(!valid_rev("--output=/tmp/pwned"));
        assert!(!valid_rev("-o"));
        assert!(!valid_rev("--upload-pack=evil"));
        // ...while the things a caller legitimately says still work.
        assert!(valid_rev("HEAD"));
        assert!(valid_rev("main"));
        assert!(valid_rev("refs/heads/topic"));
        assert!(valid_rev("d43585c9e0f8a1b2c3d4e5f60718293a4b5c6d7e"));
        // Range syntax is not a rev: `from..to` is built here, from two revs checked separately.
        assert!(!valid_rev("a..b"));
        assert!(!valid_rev(""));
    }

    #[test]
    fn a_repo_path_cannot_climb_out_or_break_a_header() {
        for bad in ["../../../etc/passwd", "sessions/../../../etc/passwd", "a/../b", "a//b", "./x", "x/./y", "/etc/passwd", "-x", ""] {
            assert!(!valid_repo_path(bad), "{bad:?} must be refused");
        }
        // Control bytes never reach a header value, quoted or not.
        assert!(!valid_repo_path("a\r\nX-Evil: 1"));
        assert!(!valid_repo_path("a\nb"));
        assert!(!valid_repo_path("a\0b"));
        for ok in ["tracked.txt", "sessions/claude-code/s1.jsonl", "a-b_c.2.json"] {
            assert!(valid_repo_path(ok), "{ok:?} must be allowed");
        }
    }

    #[test]
    fn a_filename_cannot_break_out_of_the_content_disposition_header() {
        // Resp::with writes headers verbatim: a quote ends the value early, a CRLF starts a header of
        // the attacker's choosing. Filtered, not escaped — a filename only has to name the file.
        assert_eq!(safe_filename("sessions/x/s1.jsonl"), "s1.jsonl");
        assert_eq!(safe_filename(r#"a".txt"#), "a.txt");
        assert_eq!(safe_filename("a\r\nX-Evil: 1.txt"), "aX-Evil1.txt");
        assert_eq!(safe_filename("a b;c.txt"), "abc.txt");
        // Nothing usable left → a name, not an empty quoted value.
        assert_eq!(safe_filename("的"), "file");
        assert_eq!(safe_filename("..."), "file");
        assert_eq!(safe_filename(""), "file");
    }

    #[test]
    fn hook_paths_are_shell_quoted() {
        // The hook is a shell script; a path with a space or a quote in it must not become code.
        assert_eq!(shell_quote("/tmp/x"), "'/tmp/x'");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(shell_quote("a';rm -rf /;'"), r"'a'\'';rm -rf /;'\'''");
    }

    // ── MR refs ──

    #[test]
    fn ref_names_reject_traversal_and_option_injection() {
        assert!(valid_ref_name("main"));
        assert!(valid_ref_name("feat/hub"));
        assert!(valid_ref_name("v1.2.3"));
        assert!(!valid_ref_name(""));
        assert!(!valid_ref_name("--upload-pack=evil"), "a leading dash could be read as an option");
        assert!(!valid_ref_name("../etc"));
        assert!(!valid_ref_name("/abs"));
        assert!(!valid_ref_name("a//b"));
        assert!(!valid_ref_name("trailing/"));
        assert!(!valid_ref_name("has space"));
        assert!(!valid_ref_name(&"x".repeat(201)));
    }

    #[test]
    fn json_helpers() {
        let v = json_body(br#"{"name":" x ","empty":"","n":3}"#).unwrap();
        assert_eq!(str_field(&v, "name").as_deref(), Some("x"), "whitespace on both ends is trimmed");
        assert_eq!(str_field(&v, "empty"), None, "an empty string counts as absent");
        assert_eq!(str_field(&v, "n"), None, "a non-string counts as absent");
        assert_eq!(str_field(&v, "nope"), None);
        assert!(json_body(b"not json").is_none());
        assert!(json_body(b"").is_none());
    }
}

/// H4: self-service registration + organizations. These exercise the real handlers against a live
/// SQLite store, so they need a `Ctx` and a tokio runtime (hence the separate module with its own
/// imports).
#[cfg(test)]
mod h4_tests {
    use std::net::IpAddr;
    use std::sync::Arc;

    use agit::hub::acl::{Action, Caller, Visibility};
    use agit::hub::session::Sessions;
    use agit::hub::store::{now_iso, Org, OrgMember, Store, User};
    use agit::hub::{auth, kdf};

    use crate::api::{api_org_members, api_orgs_create, api_register};
    use crate::cli::create_agent;
    use crate::http::Resp;
    use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC};
    use crate::router::gate;
    use crate::server::{Cfg, Ctx, CtxInner};

    async fn test_ctx(registration: bool) -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        let cfg = Cfg {
            host: "127.0.0.1".parse::<IpAddr>().unwrap(),
            port: 0,
            tls: false,
            insecure: false,
            trusted_proxies: vec![],
            registration,
        };
        let ctx = Ctx(Arc::new(CtxInner {
            store,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
        }));
        (dir, ctx)
    }

    async fn add_user(store: &Store, name: &str) {
        let salt = kdf::gen_salt().unwrap();
        let kdf_id = kdf::current_kdf_id();
        store
            .add_user(User {
                username: name.into(),
                pw_hash: kdf::hash_password("password-123", &salt, &kdf_id).unwrap(),
                salt,
                kdf: kdf_id,
                is_admin: false,
                created: now_iso(),
            })
            .await
            .unwrap();
    }

    fn has_set_cookie(r: &Resp) -> bool {
        r.extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
    }

    // ── registration ──

    #[tokio::test]
    async fn register_creates_a_non_admin_user_with_a_session() {
        let (_d, ctx) = test_ctx(true).await;
        let r = api_register(&ctx, br#"{"username":"alice","password":"correct horse"}"#).await;
        assert_eq!(r.status, 200);
        assert!(has_set_cookie(&r), "a successful signup logs the user in");
        let u = ctx.store.user("alice").await.expect("the account persists");
        assert!(!u.is_admin, "self-service signup must never grant admin");
        assert!(auth::verify_login(&ctx.store, "alice", "correct horse").await.is_some(), "the password is argon2-verifiable");
    }

    #[tokio::test]
    async fn register_ignores_an_is_admin_field_in_the_body() {
        // The security invariant: an is_admin in the request body must be ignored outright.
        let (_d, ctx) = test_ctx(true).await;
        let r = api_register(&ctx, br#"{"username":"eve","password":"password-123","is_admin":true}"#).await;
        assert_eq!(r.status, 200);
        assert!(!ctx.store.user("eve").await.unwrap().is_admin);
    }

    #[tokio::test]
    async fn register_duplicate_username_is_409_not_500() {
        let (_d, ctx) = test_ctx(true).await;
        assert_eq!(api_register(&ctx, br#"{"username":"alice","password":"password-123"}"#).await.status, 200);
        assert_eq!(api_register(&ctx, br#"{"username":"alice","password":"password-456"}"#).await.status, 409);
    }

    #[tokio::test]
    async fn register_rejects_short_password_and_bad_username() {
        let (_d, ctx) = test_ctx(true).await;
        assert_eq!(api_register(&ctx, br#"{"username":"bob","password":"short"}"#).await.status, 400);
        assert_eq!(api_register(&ctx, br#"{"username":"Bad Name","password":"password-123"}"#).await.status, 400);
    }

    #[tokio::test]
    async fn register_is_403_when_disabled() {
        let (_d, ctx) = test_ctx(false).await;
        let r = api_register(&ctx, br#"{"username":"alice","password":"password-123"}"#).await;
        assert_eq!(r.status, 403);
        assert!(ctx.store.user("alice").await.is_none(), "nothing is created when signup is off");
    }

    // ── organizations ──

    #[tokio::test]
    async fn org_member_reaches_an_org_owned_agent_and_a_stranger_does_not() {
        let (_d, ctx) = test_ctx(false).await;
        ctx.store
            .update_orgs(|l| {
                l.push(Org {
                    name: "acme".into(),
                    members: vec![
                        OrgMember { username: "alice".into(), role: "admin".into() },
                        OrgMember { username: "bob".into(), role: "member".into() },
                    ],
                    created: now_iso(),
                })
            })
            .await
            .unwrap();
        // A private agent owned by the org.
        create_agent(&ctx.store, "shared", "org:acme", Visibility::Private).await.unwrap();
        // A member reads it through the gate (org membership folded in before decide runs).
        assert!(gate(&ctx, &Caller::user("bob"), "shared", Action::Read).await.is_ok());
        // A stranger gets 404 — existence stays hidden, exactly as for a plain private agent.
        assert_eq!(gate(&ctx, &Caller::user("eve"), "shared", Action::Read).await.unwrap_err().status, 404);
        // The org admin can manage it; a plain member (folded to Write) cannot.
        assert!(gate(&ctx, &Caller::user("alice"), "shared", Action::Manage).await.is_ok());
        assert_eq!(gate(&ctx, &Caller::user("bob"), "shared", Action::Manage).await.unwrap_err().status, 403);
    }

    #[tokio::test]
    async fn org_create_makes_the_creator_an_admin() {
        let (_d, ctx) = test_ctx(false).await;
        let r = api_orgs_create(&ctx, &Caller::user("alice"), br#"{"name":"acme"}"#).await;
        assert_eq!(r.status, 201);
        assert!(ctx.store.org("acme").await.unwrap().is_admin("alice"));
    }

    #[tokio::test]
    async fn org_member_management_is_admin_only_and_needs_a_real_user() {
        let (_d, ctx) = test_ctx(false).await;
        add_user(&ctx.store, "alice").await;
        add_user(&ctx.store, "bob").await;
        assert_eq!(api_orgs_create(&ctx, &Caller::user("alice"), br#"{"name":"acme"}"#).await.status, 201);
        // The org admin adds bob.
        assert_eq!(api_org_members(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"member"}"#).await.status, 200);
        // A non-admin member cannot add anyone.
        assert_eq!(api_org_members(&ctx, &Caller::user("bob"), "acme", "", "POST", br#"{"username":"alice","role":"admin"}"#).await.status, 403);
        // Adding a user who does not exist is refused.
        assert_eq!(api_org_members(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"ghost","role":"member"}"#).await.status, 400);
    }

    #[tokio::test]
    async fn org_cannot_remove_its_last_admin() {
        let (_d, ctx) = test_ctx(false).await;
        assert_eq!(api_orgs_create(&ctx, &Caller::user("alice"), br#"{"name":"acme"}"#).await.status, 201);
        let r = api_org_members(&ctx, &Caller::user("alice"), "acme", "/alice", "DELETE", b"").await;
        assert_eq!(r.status, 409, "removing the only admin would orphan the org");
    }
}
