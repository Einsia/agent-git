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
mod prov;
mod router;
mod scan;
mod server;
mod smarthttp;

use std::path::PathBuf;

use crate::cli::{add_cmd, list_cmd, org_cmd, token_cmd, user_cmd};
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
        "org" => org_cmd(&root, &args),
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
         agit-hub token add <name> [--user <owner>] [--agent <owner>/<name>]\n\
                            [--read|--write] [--ttl-days N]   issue an access token\n\
         agit-hub token list                                  list tokens (metadata only)\n\
         agit-hub token rm <id>                               revoke a token\n\
         agit-hub org invite <org> <user> [--role R]          invite a user into an org (pending)\n\
         agit-hub org invitations <org>                       list an org's pending invitations\n\
         agit-hub org transfer <org> <new_owner>              hand org ownership to a member\n\
         agit-hub org rm <org>                                delete an empty org\n\n\
         First step: agit-hub user add <you> --admin\n\
         Hosted repos are bare git. Publish with: agit -a push http://HOST:PORT/<owner>/<name>.git (with a write token)\n\n\
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
    use agit::hub::blob::Blobs;
    use agit::hub::session::Sessions;
    use agit::hub::store::{now_iso, Org, OrgMember, Store, User};
    use agit::hub::{auth, kdf};

    use crate::api::{
        api_me_invitations, api_org_delete, api_org_invitations, api_org_members, api_org_transfer, api_orgs_create, api_register,
    };
    use crate::cli::create_agent;
    use crate::http::Resp;
    use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC, REGISTER_BURST, REGISTER_RATE_PER_SEC};
    use crate::router::gate;
    use crate::server::{Cfg, Ctx, CtxInner};

    async fn test_ctx(registration: bool) -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        // FsBlobs under the tempdir — the zero-config test backend (no S3 env, so Blobs::open picks fs).
        let blobs = Blobs::open(dir.path()).await.unwrap();
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
            blobs,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
            // Mirror production's tight registration bucket, so the per-IP throttle is exercised for real.
            register_rl: Arc::new(TokenBuckets::with_rate(REGISTER_RATE_PER_SEC, REGISTER_BURST)),
            metrics: Arc::new(agit::hub::metrics::Metrics::new()),
            escrow: crate::server::EscrowKeypair {
                secret: [7u8; 32],
                public: agit::agent::x25519_public_from_secret(&[7u8; 32]),
            },
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
                ..Default::default()
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
        let r = api_register(&ctx, None, br#"{"username":"alice","password":"correct horse"}"#).await;
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
        let r = api_register(&ctx, None, br#"{"username":"eve","password":"password-123","is_admin":true}"#).await;
        assert_eq!(r.status, 200);
        assert!(!ctx.store.user("eve").await.unwrap().is_admin);
    }

    #[tokio::test]
    async fn register_duplicate_username_is_409_not_500() {
        let (_d, ctx) = test_ctx(true).await;
        assert_eq!(api_register(&ctx, None, br#"{"username":"alice","password":"password-123"}"#).await.status, 200);
        assert_eq!(api_register(&ctx, None, br#"{"username":"alice","password":"password-456"}"#).await.status, 409);
    }

    #[tokio::test]
    async fn register_rejects_short_password_and_bad_username() {
        let (_d, ctx) = test_ctx(true).await;
        assert_eq!(api_register(&ctx, None, br#"{"username":"bob","password":"short"}"#).await.status, 400);
        assert_eq!(api_register(&ctx, None, br#"{"username":"Bad Name","password":"password-123"}"#).await.status, 400);
    }

    #[tokio::test]
    async fn register_is_403_when_disabled() {
        let (_d, ctx) = test_ctx(false).await;
        let r = api_register(&ctx, None, br#"{"username":"alice","password":"password-123"}"#).await;
        assert_eq!(r.status, 403);
        assert!(ctx.store.user("alice").await.is_none(), "nothing is created when signup is off");
    }

    #[tokio::test]
    async fn register_is_rate_limited_per_ip() {
        let (_d, ctx) = test_ctx(true).await;
        let flooder: IpAddr = "203.0.113.7".parse().unwrap();
        let bystander: IpAddr = "203.0.113.8".parse().unwrap();
        // Distinct usernames, so any rejection is the rate limit talking and never a duplicate 409.
        // More attempts than REGISTER_BURST, so the bucket is certain to drain within the loop.
        let mut statuses = Vec::new();
        for i in 0..(REGISTER_BURST as usize + 4) {
            let body = format!(r#"{{"username":"flood{i}","password":"password-123"}}"#);
            statuses.push(api_register(&ctx, Some(flooder), body.as_bytes()).await.status);
        }
        assert_eq!(statuses[0], 200, "the first signup from an IP is always allowed");
        assert!(statuses.contains(&429), "a sustained sweep from one IP is eventually throttled (429)");
        assert_eq!(*statuses.last().unwrap(), 429, "once the bucket drains, the throttle sticks");
        // A different IP keeps its own bucket, untouched by the flooder.
        let r = api_register(&ctx, Some(bystander), br#"{"username":"newcomer","password":"password-123"}"#).await;
        assert_eq!(r.status, 200, "a different address is not charged for another IP's flood");
        // No resolvable client IP → fail open (never blocked on a missing ConnectInfo), matching the
        // connection limiter's behaviour for the same case.
        let r = api_register(&ctx, None, br#"{"username":"anonip","password":"password-123"}"#).await;
        assert_eq!(r.status, 200, "a missing client IP fails open");
    }

    // ── organizations ──

    #[tokio::test]
    async fn gate_hides_owner_and_agent_existence_uniformly() {
        // The (owner, name) non-disclosure invariant: for a fixed caller, a private agent, a missing
        // agent under a real owner, and an entirely unknown owner all collapse to the SAME outcome —
        // no 404-for-missing-owner vs 403-for-invisible-agent oracle.
        let (_d, ctx) = test_ctx(false).await;
        add_user(&ctx.store, "daru").await;
        add_user(&ctx.store, "kaisen").await;
        create_agent(&ctx.store, "frontend", "daru", Visibility::Private).await.unwrap();
        create_agent(&ctx.store, "frontend", "kaisen", Visibility::Public).await.unwrap();

        let eve = Caller::user("eve");
        // private daru/frontend, known-owner-missing-agent daru/nope, unknown-owner ghost/frontend.
        let priv_agent = gate(&ctx, &eve, "daru", "frontend", Action::Read).await.unwrap_err().status;
        let missing_agent = gate(&ctx, &eve, "daru", "nope", Action::Read).await.unwrap_err().status;
        let missing_owner = gate(&ctx, &eve, "ghost", "frontend", Action::Read).await.unwrap_err().status;
        assert_eq!((priv_agent, missing_agent, missing_owner), (404, 404, 404), "no existence oracle for a logged-in stranger");

        // Anonymous gets a uniform 401 for all three (login required, nonexistent and invisible alike).
        let anon = Caller::anonymous();
        assert_eq!(gate(&ctx, &anon, "daru", "frontend", Action::Read).await.unwrap_err().status, 401);
        assert_eq!(gate(&ctx, &anon, "ghost", "frontend", Action::Read).await.unwrap_err().status, 401);

        // Coexistence: the two same-named agents resolve independently — daru owns the private one,
        // and kaisen's is public so even a stranger reads it.
        assert!(gate(&ctx, &Caller::user("daru"), "daru", "frontend", Action::Read).await.is_ok());
        assert!(gate(&ctx, &eve, "kaisen", "frontend", Action::Read).await.is_ok(), "kaisen/frontend is public");
        // A token bound to daru/frontend cannot reach kaisen/frontend (cross-owner escalation closed).
        let bound = Caller::user("daru").with_token(Some("daru/frontend"), agit::hub::acl::Scope::Read);
        assert!(gate(&ctx, &bound, "daru", "frontend", Action::Read).await.is_ok());
        assert_eq!(gate(&ctx, &bound, "kaisen", "frontend", Action::Read).await.unwrap_err().status, 404, "bound token can't cross to another owner's same-named agent");
    }

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
                    current_kek_gen: 0,
                    recovery_x25519: String::new(),
                    escrow_mode: "none".into(),
                })
            })
            .await
            .unwrap();
        // A private agent owned by the org.
        create_agent(&ctx.store, "shared", "org:acme", Visibility::Private).await.unwrap();
        // A member reads it through the gate (org membership folded in before decide runs).
        assert!(gate(&ctx, &Caller::user("bob"), "acme", "shared", Action::Read).await.is_ok());
        // A stranger gets 404 — existence stays hidden, exactly as for a plain private agent.
        assert_eq!(gate(&ctx, &Caller::user("eve"), "acme", "shared", Action::Read).await.unwrap_err().status, 404);
        // The org admin can manage it; a plain member (folded to Write) cannot.
        assert!(gate(&ctx, &Caller::user("alice"), "acme", "shared", Action::Manage).await.is_ok());
        assert_eq!(gate(&ctx, &Caller::user("bob"), "acme", "shared", Action::Manage).await.unwrap_err().status, 403);
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

    #[tokio::test]
    async fn org_cannot_demote_its_last_admin() {
        // The DELETE path already refuses to remove the sole admin; the POST/update path must refuse to
        // demote them too, or the same orphaning happens by another name.
        let (_d, ctx) = test_ctx(false).await;
        add_user(&ctx.store, "alice").await;
        add_user(&ctx.store, "bob").await;
        assert_eq!(api_orgs_create(&ctx, &Caller::user("alice"), br#"{"name":"acme"}"#).await.status, 201);
        // alice is the sole admin — demoting her to member would leave the org with no admin → 409.
        let r = api_org_members(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"alice","role":"member"}"#).await;
        assert_eq!(r.status, 409, "demoting the only admin would orphan the org");
        assert!(ctx.store.org("acme").await.unwrap().is_admin("alice"), "the refused demotion left alice an admin");
        // Promote bob so there are two admins.
        assert_eq!(api_org_members(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"admin"}"#).await.status, 200);
        // Now demoting alice is fine — bob still holds the org.
        let r = api_org_members(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"alice","role":"member"}"#).await;
        assert_eq!(r.status, 200, "demoting a non-last admin succeeds");
        let org = ctx.store.org("acme").await.unwrap();
        assert!(!org.is_admin("alice"), "alice is now a plain member");
        assert!(org.is_admin("bob"), "bob remains the org admin");
    }

    // ── org invitations (the consent flow) ──

    fn body_json(r: &Resp) -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null)
    }

    /// Stand up an org "acme" owned by alice (admin) with the given extra members, all real users.
    async fn org_with(ctx: &Ctx, members: &[(&str, &str)]) {
        add_user(&ctx.store, "alice").await;
        assert_eq!(api_orgs_create(ctx, &Caller::user("alice"), br#"{"name":"acme"}"#).await.status, 201);
        for (u, role) in members {
            add_user(&ctx.store, u).await;
            let body = format!(r#"{{"username":"{u}","role":"{role}"}}"#);
            assert_eq!(
                api_org_members(ctx, &Caller::user("alice"), "acme", "", "POST", body.as_bytes()).await.status,
                200,
                "seed member {u}"
            );
        }
    }

    /// admin invite → PENDING; the invited user accepts → member with the invited role.
    #[tokio::test]
    async fn invite_then_accept_makes_a_member_with_the_invited_role() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[]).await;
        add_user(&ctx.store, "bob").await;
        let r = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"admin"}"#).await;
        assert_eq!(r.status, 201, "an admin can invite");
        let id = body_json(&r)["id"].as_str().unwrap().to_string();
        // The invitation is pending and shows up for bob (and in the org's list), but no membership yet.
        assert!(!ctx.store.org("acme").await.unwrap().is_member("bob"), "an invite is not a membership");
        let mine = api_me_invitations(&ctx, &Caller::user("bob")).await;
        assert!(body_json(&mine).as_array().unwrap().iter().any(|i| i["id"] == id.as_str()), "bob sees his own invite");
        // bob accepts → he becomes a member with the invited role (admin).
        let tail = format!("/{id}/accept");
        assert_eq!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &tail, "POST", b"").await.status, 200);
        let org = ctx.store.org("acme").await.unwrap();
        assert!(org.is_member("bob") && org.is_admin("bob"), "bob is now an admin member");
        // The invitation is consumed — a replay accept finds nothing pending.
        assert_eq!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &tail, "POST", b"").await.status, 404);
        assert!(!body_json(&api_me_invitations(&ctx, &Caller::user("bob")).await).as_array().unwrap().iter().any(|i| i["id"] == id.as_str()));
    }

    /// A NON-invited user may not accept someone else's invitation.
    #[tokio::test]
    async fn a_stranger_cannot_accept_someone_elses_invitation() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[]).await;
        add_user(&ctx.store, "bob").await;
        add_user(&ctx.store, "eve").await;
        let r = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"member"}"#).await;
        let id = body_json(&r)["id"].as_str().unwrap().to_string();
        // eve tries to accept bob's invitation — refused, and nobody is added.
        let tail = format!("/{id}/accept");
        let status = api_org_invitations(&ctx, &Caller::user("eve"), "acme", &tail, "POST", b"").await.status;
        assert!(status == 403 || status == 404, "a non-invitee cannot accept (got {status})");
        let org = ctx.store.org("acme").await.unwrap();
        assert!(!org.is_member("eve") && !org.is_member("bob"), "the invite is untouched and unconsumed");
        // ...and bob can still accept his own afterwards.
        assert_eq!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &tail, "POST", b"").await.status, 200);
        assert!(ctx.store.org("acme").await.unwrap().is_member("bob"));
    }

    /// A non-admin member cannot invite anyone.
    #[tokio::test]
    async fn a_non_admin_cannot_invite() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[("bob", "member")]).await;
        add_user(&ctx.store, "carol").await;
        let r = api_org_invitations(&ctx, &Caller::user("bob"), "acme", "", "POST", br#"{"username":"carol","role":"member"}"#).await;
        assert_eq!(r.status, 403, "a plain member cannot invite");
        // A total stranger gets the non-disclosure 404, not a 403.
        add_user(&ctx.store, "eve").await;
        let r = api_org_invitations(&ctx, &Caller::user("eve"), "acme", "", "POST", br#"{"username":"carol","role":"member"}"#).await;
        assert_eq!(r.status, 404, "a non-member can't even tell the org exists");
    }

    /// Inviting someone who is already a member is rejected.
    #[tokio::test]
    async fn inviting_an_existing_member_is_rejected() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[("bob", "member")]).await;
        let r = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"admin"}"#).await;
        assert_eq!(r.status, 409, "bob is already a member");
        // Inviting an unknown account is a 400 (only real users, like member-add).
        let r = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"ghost","role":"member"}"#).await;
        assert_eq!(r.status, 400);
        // A duplicate pending invite is refused too.
        add_user(&ctx.store, "carol").await;
        assert_eq!(api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"carol"}"#).await.status, 201);
        assert_eq!(api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"carol"}"#).await.status, 409);
    }

    /// Declining leaves no membership and consumes the invite.
    #[tokio::test]
    async fn decline_leaves_no_membership() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[]).await;
        add_user(&ctx.store, "bob").await;
        let r = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"member"}"#).await;
        let id = body_json(&r)["id"].as_str().unwrap().to_string();
        let tail = format!("/{id}/decline");
        assert_eq!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &tail, "POST", b"").await.status, 200);
        assert!(!ctx.store.org("acme").await.unwrap().is_member("bob"), "decline grants nothing");
        // The invite is no longer pending, so it can't be accepted after the fact.
        let accept = format!("/{id}/accept");
        assert_eq!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &accept, "POST", b"").await.status, 404);
    }

    /// An admin revokes a still-pending invitation; it stops being pending and cannot be accepted.
    #[tokio::test]
    async fn revoke_removes_a_pending_invite() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[]).await;
        add_user(&ctx.store, "bob").await;
        let r = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"bob","role":"member"}"#).await;
        let id = body_json(&r)["id"].as_str().unwrap().to_string();
        let tail = format!("/{id}");
        // A non-admin cannot revoke.
        assert!(matches!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &tail, "DELETE", b"").await.status, 403 | 404));
        // The admin revokes it.
        assert_eq!(api_org_invitations(&ctx, &Caller::user("alice"), "acme", &tail, "DELETE", b"").await.status, 204);
        // It's gone from the pending listing, and bob can no longer accept it.
        let listing = api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "GET", b"").await;
        assert!(body_json(&listing).as_array().unwrap().is_empty(), "no pending invites remain");
        let accept = format!("/{id}/accept");
        assert_eq!(api_org_invitations(&ctx, &Caller::user("bob"), "acme", &accept, "POST", b"").await.status, 404);
    }

    /// Transfer hands ownership to an existing member and rejects a non-member / unknown account.
    #[tokio::test]
    async fn transfer_moves_ownership_to_a_member_and_rejects_a_non_member() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[("bob", "member")]).await;
        // Reject an unknown account.
        assert_eq!(api_org_transfer(&ctx, &Caller::user("alice"), "acme", br#"{"new_owner":"ghost"}"#).await.status, 400);
        // Reject a real user who is not a member.
        add_user(&ctx.store, "eve").await;
        assert_eq!(api_org_transfer(&ctx, &Caller::user("alice"), "acme", br#"{"new_owner":"eve"}"#).await.status, 400);
        // A non-admin cannot initiate a transfer.
        assert_eq!(api_org_transfer(&ctx, &Caller::user("bob"), "acme", br#"{"new_owner":"bob"}"#).await.status, 403);
        // The owner hands off to bob (a member): bob becomes admin, alice steps down to member.
        assert_eq!(api_org_transfer(&ctx, &Caller::user("alice"), "acme", br#"{"new_owner":"bob"}"#).await.status, 200);
        let org = ctx.store.org("acme").await.unwrap();
        assert!(org.is_admin("bob"), "bob now owns the org");
        assert!(org.is_member("alice") && !org.is_admin("alice"), "alice stepped down to a plain member");
    }

    /// Delete is refused while the org owns an agent, and succeeds once empty — sweeping memberships and
    /// pending invitations.
    #[tokio::test]
    async fn delete_is_refused_while_nonempty_then_succeeds_and_cleans_up() {
        let (_d, ctx) = test_ctx(false).await;
        org_with(&ctx, &[("bob", "member")]).await;
        add_user(&ctx.store, "carol").await;
        // A dangling pending invitation, to prove it gets swept.
        assert_eq!(api_org_invitations(&ctx, &Caller::user("alice"), "acme", "", "POST", br#"{"username":"carol"}"#).await.status, 201);
        // The org owns an agent → delete is refused with 409.
        create_agent(&ctx.store, "shared", "org:acme", Visibility::Private).await.unwrap();
        assert_eq!(api_org_delete(&ctx, &Caller::user("alice"), "acme").await.status, 409, "cannot delete while it owns agents");
        assert!(ctx.store.org("acme").await.is_some(), "the refused delete left the org intact");
        // A non-admin cannot delete either.
        assert_eq!(api_org_delete(&ctx, &Caller::user("bob"), "acme").await.status, 403);
        // Move the agent away, then delete succeeds and cleans up memberships + invitations.
        crate::api::api_transfer_agent(&ctx, &Caller::user("alice"), "acme", "shared", br#"{"to":"alice"}"#).await;
        assert_eq!(api_org_delete(&ctx, &Caller::user("alice"), "acme").await.status, 204, "an empty org deletes");
        assert!(ctx.store.org("acme").await.is_none(), "the org is gone (memberships with it)");
        assert!(ctx.store.invitations().await.iter().all(|i| i.org != "acme"), "pending invitations were swept");
    }
}

// ── H3: content-addressed blob upload/download, gated through the single acl::decide ──
#[cfg(test)]
mod h3_tests {
    use std::net::IpAddr;
    use std::sync::Arc;

    use agit::hub::acl::{Caller, Visibility};
    use agit::hub::blob::{sha256_hex, Blobs, BLOB_MAX};
    use agit::hub::session::Sessions;
    use agit::hub::store::{now_iso, Member, Org, OrgMember};

    use crate::api::api;
    use crate::cli::create_agent;
    use crate::http::{Req, Resp};
    use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC};
    use crate::server::{Cfg, Ctx, CtxInner};

    async fn test_ctx() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let store = agit::hub::store::Store::open_sqlite(dir.path()).await.unwrap();
        let blobs = Blobs::open(dir.path()).await.unwrap();
        let cfg = Cfg {
            host: "127.0.0.1".parse::<IpAddr>().unwrap(),
            port: 0,
            tls: false,
            insecure: false,
            trusted_proxies: vec![],
            registration: false,
        };
        let ctx = Ctx(Arc::new(CtxInner {
            store,
            blobs,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
            register_rl: Arc::new(TokenBuckets::new()),
            metrics: Arc::new(agit::hub::metrics::Metrics::new()),
            escrow: crate::server::EscrowKeypair {
                secret: [7u8; 32],
                public: agit::agent::x25519_public_from_secret(&[7u8; 32]),
            },
        }));
        (dir, ctx)
    }

    /// A minimal Req view. `target` carries the optional `?sha256=` query the handler reads.
    fn req(method: &str, target: &str, clen: usize) -> Req {
        Req { method: method.to_string(), target: target.to_string(), headers: vec![], content_length: clen }
    }

    /// Give `user` a read-only membership on an existing agent.
    async fn add_read_member(ctx: &Ctx, agent: &str, user: &str) {
        ctx.store
            .update_agents(|list| {
                if let Some(a) = list.iter_mut().find(|a| a.name == agent) {
                    a.members.push(Member { username: user.into(), role: "read".into() });
                }
            })
            .await
            .unwrap();
    }

    fn header<'a>(r: &'a Resp, k: &str) -> Option<&'a str> {
        r.extra.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn write_member_puts_and_authorized_reader_gets_it_back() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"a large-ish artifact that does not belong in git";

        // Owner (admin role → Write) uploads; the server echoes the sha256 it computed.
        let put = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("alice"), None, body).await;
        assert_eq!(put.status, 201);
        let v: serde_json::Value = serde_json::from_slice(&put.body).unwrap();
        let digest = v["sha256"].as_str().unwrap().to_string();
        assert_eq!(digest, sha256_hex(body), "server-computed address");
        assert_eq!(v["size"].as_u64().unwrap(), body.len() as u64);

        // An authorized reader (the owner) fetches it: bytes identical + the hardened download headers.
        let path = format!("/api/agent/alice/art/blob/{digest}");
        let rest = format!("agent/alice/art/blob/{digest}");
        let get = api(&ctx, &req("GET", &path, 0), &rest, &Caller::user("alice"), None, b"").await;
        assert_eq!(get.status, 200);
        assert_eq!(get.body, body);
        assert_eq!(header(&get, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(header(&get, "Content-Security-Policy"), Some("default-src 'none'; sandbox"));
        assert_eq!(header(&get, "Content-Disposition"), Some(format!("attachment; filename=\"{digest}\"").as_str()));
        assert_eq!(get.ctype, "application/octet-stream");
    }

    #[tokio::test]
    async fn re_upload_is_idempotent_same_digest() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"identical bytes";
        let a = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("alice"), None, body).await;
        let b = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("alice"), None, body).await;
        assert_eq!(a.status, 201);
        assert_eq!(b.status, 201);
        let da: serde_json::Value = serde_json::from_slice(&a.body).unwrap();
        let db: serde_json::Value = serde_json::from_slice(&b.body).unwrap();
        assert_eq!(da["sha256"], db["sha256"]);
    }

    #[tokio::test]
    async fn anonymous_put_on_private_agent_is_401() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"x";
        let r = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::anonymous(), None, body).await;
        assert_eq!(r.status, 401, "no credentials on a private agent → one chance to authenticate");
    }

    #[tokio::test]
    async fn read_only_member_put_is_403() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        add_read_member(&ctx, "art", "reader").await;
        let body = b"x";
        let r = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("reader"), None, body).await;
        assert_eq!(r.status, 403, "a reader can see the agent but may not write to it");
    }

    #[tokio::test]
    async fn wrong_claimed_digest_is_409_correct_is_accepted() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"claim me";
        let real = sha256_hex(body);

        // A wrong (but well-formed) claimed digest is rejected rather than trusted.
        let wrong = "0".repeat(64);
        let bad = api(
            &ctx,
            &req("PUT", &format!("/api/agent/alice/art/blob?sha256={wrong}"), body.len()),
            "agent/alice/art/blob",
            &Caller::user("alice"),
            None,
            body,
        )
        .await;
        assert_eq!(bad.status, 409);

        // The correct claim is accepted.
        let ok = api(
            &ctx,
            &req("PUT", &format!("/api/agent/alice/art/blob?sha256={real}"), body.len()),
            "agent/alice/art/blob",
            &Caller::user("alice"),
            None,
            body,
        )
        .await;
        assert_eq!(ok.status, 201);
    }

    #[tokio::test]
    async fn get_on_private_agent_by_a_stranger_is_404_non_disclosure() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"secret artifact";
        let put = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("alice"), None, body).await;
        let digest = serde_json::from_slice::<serde_json::Value>(&put.body).unwrap()["sha256"].as_str().unwrap().to_string();

        // eve has no access — she must not be able to tell "no such blob" from "no access to this agent".
        let rest = format!("agent/alice/art/blob/{digest}");
        let path = format!("/api/{rest}");
        let r = api(&ctx, &req("GET", &path, 0), &rest, &Caller::user("eve"), None, b"").await;
        assert_eq!(r.status, 404, "the blob exists, but a stranger sees the same 404 as a missing agent");
    }

    #[tokio::test]
    async fn get_missing_and_malformed_digests_are_404() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();

        // A well-formed digest that names no blob → 404 (for an authorized reader).
        let absent = "a".repeat(64);
        let rest = format!("agent/alice/art/blob/{absent}");
        let r = api(&ctx, &req("GET", &format!("/api/{rest}"), 0), &rest, &Caller::user("alice"), None, b"").await;
        assert_eq!(r.status, 404);

        // Malformed digests (wrong length / non-hex) → 404, before the backend is ever touched.
        for d in ["zz", "not-a-digest", "ZZZ"] {
            let rest = format!("agent/alice/art/blob/{d}");
            let r = api(&ctx, &req("GET", &format!("/api/{rest}"), 0), &rest, &Caller::user("alice"), None, b"").await;
            assert_eq!(r.status, 404, "malformed digest {d} → 404");
        }
    }

    #[tokio::test]
    async fn oversize_upload_is_413() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        // content_length over the ceiling is refused size-first, before any storage work.
        let clen = (BLOB_MAX + 1) as usize;
        let r = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", clen), "agent/alice/art/blob", &Caller::user("alice"), None, b"small").await;
        assert_eq!(r.status, 413);
    }

    #[tokio::test]
    async fn corrupted_blob_surfaces_500_not_bad_bytes() {
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"honest bytes";
        let put = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("alice"), None, body).await;
        let digest = serde_json::from_slice::<serde_json::Value>(&put.body).unwrap()["sha256"].as_str().unwrap().to_string();

        // Corrupt the stored object under its key, then GET: the read-time re-hash must refuse it.
        let file = ctx.root().join("blobs").join("alice").join("art").join(&digest);
        std::fs::write(&file, b"tampered!").unwrap();
        let rest = format!("agent/alice/art/blob/{digest}");
        let r = api(&ctx, &req("GET", &format!("/api/{rest}"), 0), &rest, &Caller::user("alice"), None, b"").await;
        assert_eq!(r.status, 500, "bytes that no longer hash to their address are not served");
    }

    #[tokio::test]
    async fn rename_carries_blobs_to_the_new_name() {
        // FIX 1: blobs are keyed by the agent NAME, so a rename must move them. Upload under "art",
        // rename art → gallery, and the blob must be reachable under the NEW name and gone from the old.
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "art", "alice", Visibility::Private).await.unwrap();
        let body = b"a renamed artifact";
        let put = api(&ctx, &req("PUT", "/api/agent/alice/art/blob", body.len()), "agent/alice/art/blob", &Caller::user("alice"), None, body).await;
        assert_eq!(put.status, 201);
        let digest = serde_json::from_slice::<serde_json::Value>(&put.body).unwrap()["sha256"].as_str().unwrap().to_string();

        // Rename through the real PATCH route (owner → Manage).
        let renamed = api(&ctx, &req("PATCH", "/api/agent/alice/art", 0), "agent/alice/art", &Caller::user("alice"), None, br#"{"name":"gallery"}"#).await;
        assert_eq!(renamed.status, 200, "rename succeeds");

        // Reachable under the NEW name, end to end (200 + identical bytes).
        let rest = format!("agent/alice/gallery/blob/{digest}");
        let get = api(&ctx, &req("GET", &format!("/api/{rest}"), 0), &rest, &Caller::user("alice"), None, b"").await;
        assert_eq!(get.status, 200, "the blob followed the rename");
        assert_eq!(get.body, body);

        // Unambiguously at the storage layer: present under the new key, absent under the old (not
        // merely hidden by the gate now that "art" no longer exists).
        assert!(ctx.blobs.get("alice", "gallery", &digest).await.unwrap().is_some(), "moved to the new name");
        assert!(ctx.blobs.get("alice", "art", &digest).await.unwrap().is_none(), "not stranded under the old name");
    }

    #[tokio::test]
    async fn purge_closes_the_recycled_name_blob_leak() {
        // FIX 1, the security case: after an agent is purged, a NEW agent created under the SAME name
        // must NOT be able to read the previous owner's private blob. Without the purge-time blob
        // delete, blobs/<name>/ survives, the name gate passes for the new agent, and blobs.get returns
        // the old bytes — the exact recycled-name leak already closed for tokens and MRs.
        let (_d, ctx) = test_ctx().await;
        create_agent(&ctx.store, "temp", "alice", Visibility::Private).await.unwrap();
        let body = b"the previous owner's private bytes";
        let put = api(&ctx, &req("PUT", "/api/agent/alice/temp/blob", body.len()), "agent/alice/temp/blob", &Caller::user("alice"), None, body).await;
        assert_eq!(put.status, 201);
        let digest = serde_json::from_slice::<serde_json::Value>(&put.body).unwrap()["sha256"].as_str().unwrap().to_string();

        // Soft delete, then purge (the two-step destroy), both through the real DELETE route.
        let soft = api(&ctx, &req("DELETE", "/api/agent/alice/temp", 0), "agent/alice/temp", &Caller::user("alice"), None, b"").await;
        assert_eq!(soft.status, 200, "soft delete");
        let purge = api(&ctx, &req("DELETE", "/api/agent/alice/temp?purge=true", 0), "agent/alice/temp", &Caller::user("alice"), None, b"").await;
        assert_eq!(purge.status, 204, "purge empties the trash");

        // A brand-new owner recycles the NAME under their own namespace (mallory/temp).
        create_agent(&ctx.store, "temp", "mallory", Visibility::Private).await.unwrap();
        // Mallory reads the old digest under her own recycled agent: it must be 404, NOT the previous
        // owner's bytes. This is the leak, and it is closed — and per-(owner,name) keys make it doubly
        // so, since alice/temp and mallory/temp are different namespaces to begin with.
        let rest = format!("agent/mallory/temp/blob/{digest}");
        let get = api(&ctx, &req("GET", &format!("/api/{rest}"), 0), &rest, &Caller::user("mallory"), None, b"").await;
        assert_eq!(get.status, 404, "the recycled name cannot read the purged owner's private blob");
        assert!(ctx.blobs.get("alice", "temp", &digest).await.unwrap().is_none(), "the bytes are gone from storage");
    }

    async fn make_org(ctx: &Ctx, name: &str, admin: &str, member: &str) {
        ctx.store
            .update_orgs(|l| {
                l.push(Org {
                    name: name.into(),
                    members: vec![
                        OrgMember { username: admin.into(), role: "admin".into() },
                        OrgMember { username: member.into(), role: "member".into() },
                    ],
                    created: now_iso(),
                    current_kek_gen: 0,
                    recovery_x25519: String::new(),
                    escrow_mode: "none".into(),
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_under_org_hides_org_existence() {
        // FIX 3: a missing org and one the caller can't see must return the SAME response, so create
        // can't be used to probe which orgs exist. eve is a member of nothing.
        let (_d, ctx) = test_ctx().await;
        make_org(&ctx, "acme", "alice", "bob").await;

        let missing =
            api(&ctx, &req("POST", "/api/agents", 0), "agents", &Caller::user("eve"), None, br#"{"name":"x","org":"ghost"}"#).await;
        let forbidden =
            api(&ctx, &req("POST", "/api/agents", 0), "agents", &Caller::user("eve"), None, br#"{"name":"y","org":"acme"}"#).await;
        assert_eq!(missing.status, 404, "no such org → 404");
        assert_eq!(forbidden.status, missing.status, "an existing org a stranger can't see is indistinguishable from a missing one");

        // A member who merely lacks org-admin still gets a distinct 403 — they already know it exists.
        let member = api(&ctx, &req("POST", "/api/agents", 0), "agents", &Caller::user("bob"), None, br#"{"name":"z","org":"acme"}"#).await;
        assert_eq!(member.status, 403, "a member who isn't an org admin is told so, not hidden");
        // The successful path is unchanged: the org admin creates the org-owned agent.
        let admin = api(&ctx, &req("POST", "/api/agents", 0), "agents", &Caller::user("alice"), None, br#"{"name":"w","org":"acme"}"#).await;
        assert_eq!(admin.status, 201, "the org admin still creates org-owned agents");
    }

    #[tokio::test]
    async fn transfer_to_org_hides_org_existence() {
        // FIX 3: same non-disclosure on the transfer path. alice owns "mine" but is not a member of the
        // "secret" org, so transferring to a missing org and to "secret" both return 404.
        let (_d, ctx) = test_ctx().await;
        make_org(&ctx, "secret", "bob", "carol").await;
        create_agent(&ctx.store, "mine", "alice", Visibility::Private).await.unwrap();

        let missing =
            api(&ctx, &req("POST", "/api/agent/alice/mine/transfer", 0), "agent/alice/mine/transfer", &Caller::user("alice"), None, br#"{"org":"ghost"}"#).await;
        let forbidden =
            api(&ctx, &req("POST", "/api/agent/alice/mine/transfer", 0), "agent/alice/mine/transfer", &Caller::user("alice"), None, br#"{"org":"secret"}"#).await;
        assert_eq!(missing.status, 404, "no such org → 404");
        assert_eq!(forbidden.status, missing.status, "a non-member can't tell 'secret exists' from 'no such org'");
    }
}

/// Observability wave: the `/metrics` endpoint (gate + valid exposition), the per-request counter, and
/// the logging init. These drive the real router through `tower`'s `oneshot`, so the observe
/// middleware and the admin gate are exercised end-to-end, not in isolation.
#[cfg(test)]
mod obs_tests {
    use std::net::IpAddr;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt; // oneshot

    use agit::hub::blob::Blobs;
    use agit::hub::metrics::Metrics;
    use agit::hub::session::{self, Sessions};
    use agit::hub::store::{now_iso, Store, User};
    use agit::hub::kdf;

    use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC};
    use crate::router::build;
    use crate::server::{init_tracing, Cfg, Ctx, CtxInner};

    /// A Ctx whose store already holds one admin user named `admin` (password irrelevant — tests mint
    /// the session directly).
    async fn ctx_with_admin() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        let salt = kdf::gen_salt().unwrap();
        let kdf_id = kdf::current_kdf_id();
        store
            .add_user(User {
                username: "admin".into(),
                pw_hash: kdf::hash_password("password-123", &salt, &kdf_id).unwrap(),
                salt,
                kdf: kdf_id,
                is_admin: true,
                created: now_iso(),
                ..Default::default()
            })
            .await
            .unwrap();
        let blobs = Blobs::open(dir.path()).await.unwrap();
        let cfg = Cfg {
            host: "127.0.0.1".parse::<IpAddr>().unwrap(),
            port: 0,
            tls: false,
            insecure: false,
            trusted_proxies: vec![],
            registration: false,
        };
        let ctx = Ctx(Arc::new(CtxInner {
            store,
            blobs,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
            register_rl: Arc::new(TokenBuckets::new()),
            metrics: Arc::new(Metrics::new()),
            escrow: crate::server::EscrowKeypair {
                secret: [7u8; 32],
                public: agit::agent::x25519_public_from_secret(&[7u8; 32]),
            },
        }));
        (dir, ctx)
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn logging_init_is_idempotent_and_does_not_panic() {
        // `try_init` under the hood, so a second call (or a call after another test initialised the
        // global subscriber) is a harmless no-op rather than a panic.
        init_tracing();
        init_tracing();
    }

    #[tokio::test]
    async fn metrics_is_admin_gated_and_returns_valid_exposition() {
        let (_d, ctx) = ctx_with_admin().await;
        let app = build(ctx.clone());

        // Anonymous → 404: the gate must keep /metrics from being world-readable, and it must not even
        // advertise that the route exists.
        let anon =
            app.clone().oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(anon.status(), 404, "/metrics must not be reachable without the admin gate");

        // Admin session → 200 + valid Prometheus exposition text.
        let sid = ctx.sessions.create("admin").unwrap();
        let req = Request::builder()
            .uri("/metrics")
            .header("cookie", format!("{}={sid}", session::COOKIE))
            .body(Body::empty())
            .unwrap();
        let ok = app.clone().oneshot(req).await.unwrap();
        assert_eq!(ok.status(), 200, "an admin may scrape /metrics");
        let text = body_string(ok).await;
        assert!(text.contains("# TYPE http_requests_total counter"), "exposition:\n{text}");
        assert!(text.contains("# TYPE http_request_duration_seconds histogram"));
        assert!(text.contains("http_request_duration_seconds_bucket{le=\"+Inf\"}"));
        assert!(text.contains("# TYPE auth_attempts_total counter"));
        assert!(text.contains("# TYPE git_push_total counter"));
        assert!(text.contains("secret_scan_rejects_total"));
        assert!(text.contains("agit_hub_build_info"));
    }

    #[tokio::test]
    async fn a_request_increments_http_requests_total() {
        let (_d, ctx) = ctx_with_admin().await;
        let app = build(ctx.clone());
        // One anonymous request through the whole stack: /api/me with no creds → 401.
        let r = app.clone().oneshot(Request::builder().uri("/api/me").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 401);
        // The observe middleware should have counted it, folded to method + status class.
        let dump = ctx.metrics.render();
        assert!(dump.contains("http_requests_total{method=\"GET\",status=\"4xx\"} 1"), "metrics dump:\n{dump}");
        assert!(dump.contains("http_request_duration_seconds_count 1"));
    }
}
