//! Text that leaves this device to be kept is protected in full by the privacy worker, which this
//! test binary hosts itself: a PEM private key in one text reaches the reader as one placeholder
//! from BEGIN through END; encoded credentials and omitted containers stay protected in stored
//! envelopes; a protected attachment comes back whole; and text and JSON values the worker could
//! not process are withheld, never passed through. Remote control is not on that path: a
//! session's history reaches its viewers as it is.

use agit::domain::redact::{Persona, Redactor, WITHHELD};
use std::path::Path;

/// A named check that runs against the worker this binary hosts.
type Case = (&'static str, fn(&Path));

const BODY: [&str; 3] = [
    "MIIEowIBAAKCAQEAsYnThEtIcKeYbOdYlInEoNe0123456789abcdefghijklmnop",
    "qrstuvwxyzSYNTHETICkeyBODYlineTWO9876543210ABCDEFGHIJKLMNOPQRSTUV",
    "WXYZsyntheticKEYbodyLINEthree0246813579zyxwvutsrqponmlkjihgfedcb==",
];

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if let Some(code) = agit::domain::privacy::worker::entry(&args) {
        std::process::exit(code);
    }
    let home = tempfile::tempdir().unwrap();
    // SAFETY: the cases below run on this thread before any other thread starts, and the
    // privacy workers this process spawns inherit the variables.
    unsafe {
        std::env::set_var("AGIT_HOME", home.path());
        std::env::set_var("AGIT_HUB_URL", "http://127.0.0.1:1");
        std::env::set_var("CLAUDE_CONFIG_DIR", home.path().join("claude"));
    }
    // A cold worker that misses its first foreground budget backs off; wait until it serves, so
    // the cases below measure protection rather than start-up time.
    let started = std::time::Instant::now();
    while Redactor::new(Persona::default())
        .try_scrub("warm-up")
        .is_err()
    {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "the privacy worker never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let cases: &[Case] = &[
        ("a key in one text", a_key_in_one_text_is_one_placeholder),
        (
            "a session's history as it is",
            a_sessions_history_goes_as_it_is,
        ),
        (
            "encoded credentials across the record size boundary",
            encoded_credentials_require_a_complete_semantic_check,
        ),
        (
            "omitted containers stay omitted in stored envelopes",
            omitted_containers_stay_omitted_in_stored_envelopes,
        ),
        (
            "an attachment's placeholder through the service",
            an_attachments_placeholder_is_restored_through_the_service,
        ),
        // Last: it leaves the worker unable to process anything for the cases after it.
        ("an unprocessed text", unprocessed_text_is_withheld),
    ];
    for (name, case) in cases {
        case(home.path());
        println!("ok - {name}");
    }
}

fn key() -> String {
    format!(
        "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
        BODY.join("\n")
    )
}

/// Omitting an unscanned container changes stored content even without credential replacements.
/// Reusing the original envelope would restore unchecked secrets and its obsolete content hash.
fn omitted_containers_stay_omitted_in_stored_envelopes(_home: &Path) {
    use agit::domain::{
        privacy::{projector::Mode, service},
        storage, transcript,
    };
    let row = "ordinary report output ".repeat(8);
    let mut rows =
        vec![row.clone(); agit::domain::privacy::projector::MAX_UNIT_BYTES / row.len() + 1];
    let secret = "c7Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0";
    rows[0] = format!("PASSWORD={secret}");
    let native = serde_json::json!({"type":"response_item", "payload": {
        "type":"function_call_output", "call_id":"omitted-call", "output":{"rows":rows}
    }});
    let line = format!("{native}\n");
    assert!(line.len() > agit::domain::privacy::projector::MAX_UNIT_BYTES);
    let input = transcript::wrap_lines(&line, "codex", &format!("agit-{}", "a".repeat(40)));
    let outcome = service::transform(None, &input, Mode::ProtectEnvelopes);
    assert!(service::passed(&outcome), "{:?}", outcome.reason);
    assert_eq!(outcome.replacements, 0);
    assert_eq!(outcome.omitted, 1);
    assert!(!outcome.content.contains(secret));
    assert!(
        outcome
            .content
            .contains("[content omitted: too large for the privacy check]")
    );
    let original = storage::parse_envelope_line(&input).unwrap();
    let protected = storage::parse_envelope_line(&outcome.content).unwrap();
    assert_eq!(protected.session_id, original.session_id);
    assert_eq!(protected.source, original.source);
    assert_eq!(protected.content["payload"]["call_id"], "omitted-call");
    assert_ne!(protected.object_hash, original.object_hash);
    assert_eq!(
        protected.object_hash,
        transcript::object_hash(&protected.content)
    );
}

/// A file attached as base64 text, which protection encoded again with a placeholder inside, comes
/// back whole through the service's own entry. The placeholder is not visible outside the base64,
/// so a restoration that skipped text showing no placeholder would hand the protected attachment
/// back as if it were restored.
fn an_attachments_placeholder_is_restored_through_the_service(_home: &Path) {
    use agit::domain::privacy::{projector::Mode, service};
    use base64::Engine as _;
    let secret = "c7Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0";
    let file = base64::engine::general_purpose::STANDARD.encode(format!("API_KEY={secret}\n"));
    let record = serde_json::json!({"type":"file","mime":"text/plain","data":file});
    let protected =
        service::protect_for_commit(None, &format!("{record}\n"), Mode::ProtectJsonl).unwrap();
    assert!(!protected.contains(&file), "{protected}");
    let restored = service::transform(None, &protected, Mode::HydrateJsonl);
    assert!(service::passed(&restored), "{:?}", restored.reason);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(restored.content.trim_end()).unwrap(),
        record
    );
}

/// Crossing the frame boundary cannot downgrade encoded tool data to plain-text scanning.
/// Small carriers protect decoded keys; carriers that cannot be checked refuse durable capture.
fn encoded_credentials_require_a_complete_semantic_check(_home: &Path) {
    use agit::domain::{
        privacy::{
            projector::{Mode, Reason},
            service,
        },
        transcript,
    };
    let secret = "AbCdEfGhIjKlMnOp";
    let record = |padding: &str| {
        serde_json::json!({
            "type":"response_item","payload":{"type":"function_call","call_id":"encoded-call",
            "name":"Inspect","arguments":format!(r#"{{"api_\u006bey":"{secret}","padding":{}}}"#, serde_json::json!(padding))}
        })
    };
    let small = record("ordinary report output");
    let protected =
        service::protect_for_commit(None, &small.to_string(), Mode::ProtectJsonl).unwrap();
    assert!(!protected.contains(secret));
    let restored = service::transform(None, &protected, Mode::HydrateJsonl);
    assert!(service::passed(&restored));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&restored.content).unwrap(),
        small
    );

    let line = "ordinary report output ";
    let padding = line.repeat(agit::domain::privacy::projector::MAX_UNIT_BYTES / line.len() + 1);
    let large = format!("{}\n", record(&padding));
    assert!(large.len() > agit::domain::privacy::projector::MAX_UNIT_BYTES);
    let envelope = transcript::wrap_lines(&large, "codex", &format!("agit-{}", "a".repeat(40)));
    for (mode, text) in [
        (Mode::ProtectJsonl, large.as_str()),
        (Mode::ProtectEnvelopes, envelope.as_str()),
    ] {
        let refusal = service::protect_for_commit(None, text, mode).unwrap_err();
        assert_eq!(refusal.reason, Some(Reason::Oversize));
        assert!(!refusal.to_string().contains("next attempt tries again"));
        assert!(!refusal.to_string().contains(secret));
    }
}

/// The protected text keeps everything around the key and puts exactly one placeholder where
/// the whole block was. A pass over single lines replaces only the BEGIN line.
fn assert_one_placeholder(protected: &str, before: &str, after: &str) {
    for line in BODY {
        assert!(!protected.contains(line), "{protected}");
    }
    assert!(!protected.contains("PRIVATE KEY"), "{protected}");
    let inner = protected
        .strip_prefix(before)
        .and_then(|rest| rest.strip_suffix(after))
        .unwrap_or_else(|| panic!("{protected}"));
    let tokens: Vec<_> = agit::domain::privacy::projector::tokens(inner).collect();
    assert_eq!(tokens.len(), 1, "{protected}");
    assert_eq!((tokens[0].0, tokens[0].1), (0, inner.len()), "{protected}");
}

/// A key inside one text, as a prompt or an error carries it, becomes exactly one placeholder
/// and the text around it is unchanged, including when that text is a JSON member name. A pass
/// over single lines exposes the body; a colliding member rename must withhold the JSON rather
/// than expose the original key or silently discard an entry.
fn a_key_in_one_text_is_one_placeholder(_home: &Path) {
    let redactor = Redactor::new(Persona::default());
    let before = "deploy with this key:\n";
    let after = "\nthen rotate it";
    let prompt = format!("{before}{}{after}", key());
    let report = redactor.scrub(&prompt);
    assert_one_placeholder(&report.text, before, after);
    assert_eq!(redactor.try_scrub(&prompt).unwrap().text, report.text);
    let structured = serde_json::json!({"type":"message", prompt.clone(): "payload"});
    let protected = redactor.try_scrub_json(&structured).unwrap().value;
    assert_eq!(
        protected,
        serde_json::json!({"type":"message", report.text.clone(): "payload"})
    );
    let collision = serde_json::json!({prompt: "first", report.text: "second"});
    assert!(redactor.try_scrub_json(&collision).is_err());
    let withheld = redactor.scrub_json(&collision).value;
    assert!(withheld.get("protection_error").is_some());
    assert!(!withheld.to_string().contains(BODY[0]));
}

/// Remote control shows a session's history as it is, whoever's rules would block a value in it:
/// the history is the owner's own, seen by them or by colleagues in their workspace. A linked
/// session whose repository blocks a phrase still shows it, and a link that can't be read or a
/// repository that has moved doesn't refuse the history. A history read that went through the
/// privacy check again would hide the phrase, or refuse while the rules can't load.
fn a_sessions_history_goes_as_it_is(home: &Path) {
    let phrase = "cedar lantern";
    let repo = agit::domain::repo::Repo::init(&home.join("repos/alice/notes")).unwrap();
    agit::domain::secret_filter::RepositoryDictionary::open(repo.root())
        .unwrap()
        .block_add(
            "Private phrase",
            zeroize::Zeroizing::new(phrase.into()),
            true,
        )
        .unwrap();
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let store = agit::domain::store::Store::open_or_init().unwrap();
    let native = "6b3f2c1e-0000-4000-8000-000000000001";
    let mut link = agit::domain::link::Link::new("claude-code", native, Some(&work));
    link.owner = Some("alice".into());
    link.agent = Some("notes".into());
    let link_path = agit::domain::link::write(&store, &link).unwrap();
    let project = home
        .join("claude/projects")
        .join(agit::adapter::claude_code::slug_for(&work));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(format!("{native}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({"type":"user","cwd":work,"message":{"role":"user","content":format!("ask about the {phrase}")}})
        ),
    )
    .unwrap();
    let history = || {
        agit::rc::local_history::read(
            serde_json::json!({"session_id":native,"runtime":"claude-code","cwd":work}),
        )
    };
    assert!(history().unwrap().to_string().contains(phrase));
    std::fs::write(&link_path, "{invalid link").unwrap();
    assert!(history().unwrap().to_string().contains(phrase));
    agit::domain::link::write(&store, &link).unwrap();
    let moved = home.join("unavailable-notes");
    std::fs::rename(repo.root(), &moved).unwrap();
    assert!(history().unwrap().to_string().contains(phrase));
    std::fs::rename(moved, repo.root()).unwrap();
}

/// Text and JSON values the worker cannot process are withheld: the marker replaces them and the
/// fallible forms report an error. An implementation that passed the input through on a worker
/// failure would publish it unprotected.
fn unprocessed_text_is_withheld(home: &Path) {
    // A privacy root that is a file, not a directory, leaves the worker unable to process
    // anything.
    let blocked = home.join("blocked-home");
    std::fs::write(&blocked, "not a directory").unwrap();
    // SAFETY: as in `main`; nothing else runs while this case changes the variable.
    unsafe { std::env::set_var("AGIT_HOME", &blocked) };
    agit::domain::privacy::service::invalidate();
    let redactor = Redactor::new(Persona::default());
    let prompt = format!("deploy with this key:\n{}\n", key());
    assert_eq!(redactor.scrub(&prompt).text, WITHHELD);
    assert!(redactor.try_scrub(&prompt).is_err());
    let value = serde_json::json!({"command": format!("cat <<EOF > key.pem\n{}\nEOF", key())});
    let withheld = redactor.scrub_json(&value).value;
    assert!(withheld.get("protection_error").is_some(), "{withheld}");
    assert!(!withheld.to_string().contains(BODY[0]));
    assert!(redactor.try_scrub_json(&value).is_err());
    // SAFETY: as above.
    unsafe { std::env::set_var("AGIT_HOME", home) };
    agit::domain::privacy::service::invalidate();
}
