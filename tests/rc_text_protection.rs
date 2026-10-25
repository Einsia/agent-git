//! Remote-control text leaves this device only as content the privacy worker protected in full.
//! The redactor every RC frame's text passes through (prompts, steer messages, turn errors,
//! approval summaries, native event text and streamed deltas) is driven against the real
//! worker, which this test binary hosts itself: a PEM private key reaches the reader as one
//! placeholder from BEGIN through END, also when a stream holds it in the tail it flushes at the
//! end of an item or cuts it across a long chunk; a streamed value is recognized by context an
//! earlier release carried; text and JSON values the worker could not process are withheld,
//! never passed through; and a stream that dropped a fragment delivers nothing more for that
//! item.

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
            "a key in a stream's tail",
            a_key_in_a_streams_tail_is_one_placeholder,
        ),
        (
            "a dropped fragment",
            a_dropped_fragment_ends_delivery_for_its_item,
        ),
        (
            "a key cut by a long chunk",
            a_key_cut_by_a_long_chunk_leaves_no_body_line,
        ),
        (
            "a lower-case key",
            a_lower_case_key_is_held_back_like_any_other,
        ),
        (
            "a value whose context left earlier",
            a_value_is_protected_when_its_context_left_earlier,
        ),
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
/// and the text around it is unchanged. A pass over single lines replaces only the BEGIN line.
fn a_key_in_one_text_is_one_placeholder(_home: &Path) {
    let redactor = Redactor::new(Persona::default());
    let before = "deploy with this key:\n";
    let after = "\nthen rotate it";
    let prompt = format!("{before}{}{after}", key());
    let report = redactor.scrub(&prompt);
    assert_one_placeholder(&report.text, before, after);
    assert_eq!(redactor.try_scrub(&prompt).unwrap().text, report.text);
}

/// A key at the end of a streamed item waits in the stream's held tail and becomes one placeholder
/// when the item ends. A flush that protected the tail line by line would deliver the body.
fn a_key_in_a_streams_tail_is_one_placeholder(_home: &Path) {
    let redactor = Redactor::new(Persona::default());
    let before = format!(
        "{}\nwriting key.pem:\n",
        "ordinary output line\n".repeat(40)
    );
    let after = "\n";
    let text = format!("{before}{}{after}", key());
    let mut stream = redactor.stream();
    let mut delivered = String::new();
    for chunk in text.as_bytes().chunks(97) {
        delivered.push_str(&stream.push(std::str::from_utf8(chunk).unwrap()).text);
    }
    delivered.push_str(&stream.flush().text);
    assert_one_placeholder(&delivered, &before, after);
}

/// A fragment too large to protect is dropped, and with it the rest of the item: the dropped
/// fragment ends with a key's BEGIN line, so the body that follows would read as ordinary text.
fn a_dropped_fragment_ends_delivery_for_its_item(_home: &Path) {
    let redactor = Redactor::new(Persona::default());
    let mut stream = redactor.stream();
    let oversized = format!(
        "{}\n-----BEGIN RSA PRIVATE KEY-----\n",
        "x".repeat(agit::domain::privacy::projector::MAX_UNIT_BYTES)
    );
    let mut delivered = stream.push(&oversized).text;
    delivered.push_str(
        &stream
            .push(&format!(
                "{}\n-----END RSA PRIVATE KEY-----\n",
                BODY.join("\n")
            ))
            .text,
    );
    delivered.push_str(&stream.push("ordinary output after the key\n").text);
    delivered.push_str(&stream.flush().text);
    assert_eq!(delivered, WITHHELD);
}

/// A long chunk that opens a key, followed by chunks that carry the body and a split END line,
/// delivers the key as one placeholder and the output after it unchanged. A stream that released
/// a long chunk whole, without holding back the open key, would deliver the body as ordinary text.
fn a_key_cut_by_a_long_chunk_leaves_no_body_line(_home: &Path) {
    let redactor = Redactor::new(Persona::default());
    let mut stream = redactor.stream();
    let long = format!(
        "{}-----BEGIN RSA PRIVATE KEY-----\n{}\n",
        "ordinary output line\n".repeat(7000),
        BODY[0]
    );
    let mut delivered = stream.push(&long).text;
    for chunk in [
        format!("{}\n", BODY[1]),
        format!("{}\n-----END RSA PRI", BODY[2]),
        "VATE KEY-----\nafter the key\n".to_owned(),
    ] {
        delivered.push_str(&stream.push(&chunk).text);
    }
    delivered.push_str(&stream.flush().text);
    for line in BODY {
        assert!(!delivered.contains(line), "a key body line was delivered");
    }
    assert!(!delivered.contains("PRIVATE KEY"));
    assert!(delivered.ends_with("after the key\n"));
    assert_eq!(
        agit::domain::privacy::projector::tokens(&delivered).count(),
        1
    );
}

/// A key header in lower case is held back like any other, so the block reaches the viewer whole
/// and is replaced as one placeholder; held back until its footer is complete, also on one long
/// line whose push ends inside the footer; and withheld when the item ends before its footer. A
/// hold-back that recognized only upper-case headers, or took a partial footer for a closed key,
/// would release the header and then the body as ordinary text.
fn a_lower_case_key_is_held_back_like_any_other(_home: &Path) {
    let redactor = Redactor::new(Persona::default());
    let mut stream = redactor.stream();
    let long = format!(
        "{}-----begin rsa private key-----\n{}\n{}\n",
        "ordinary output line\n".repeat(7000),
        BODY[0],
        "ordinary output line\n".repeat(20)
    );
    let mut delivered = stream.push(&long).text;
    delivered.push_str(
        &stream
            .push(&format!(
                "{}\n-----end rsa private key-----\n",
                BODY[1..].join("\n")
            ))
            .text,
    );
    delivered.push_str(&stream.push("after the key\n").text);
    delivered.push_str(&stream.flush().text);
    for line in BODY {
        assert!(!delivered.contains(line), "a key body line was delivered");
    }
    assert!(!delivered.contains("private key-----"));
    assert!(delivered.ends_with("after the key\n"));
    assert_eq!(
        agit::domain::privacy::projector::tokens(&delivered).count(),
        1
    );

    let mut stream = redactor.stream();
    let mut delivered = stream
        .push(&format!(
            "{}-----begin rsa private key-----\\n{}\\n-----end rsa pri",
            "ordinary words ".repeat(9000),
            BODY.join("\\n")
        ))
        .text;
    delivered.push_str(&stream.push("vate key-----\\n after the key\n").text);
    delivered.push_str(&stream.flush().text);
    for line in BODY {
        assert!(!delivered.contains(line), "a key body line was delivered");
    }
    assert!(delivered.ends_with("after the key\n"));

    let mut stream = redactor.stream();
    let mut delivered = String::new();
    let open = format!(
        "{}-----begin rsa private key-----\n{}\n",
        "ordinary output line\n".repeat(20),
        BODY.join("\n")
    );
    for chunk in open.as_bytes().chunks(97) {
        delivered.push_str(&stream.push(std::str::from_utf8(chunk).unwrap()).text);
    }
    delivered.push_str(&stream.flush().text);
    for line in BODY {
        assert!(
            !delivered.contains(line),
            "an unterminated key's body was delivered"
        );
    }
    assert!(delivered.ends_with(WITHHELD), "{delivered}");
}

/// A value reaches the viewer replaced even when what marks it as a credential, its name, its
/// keyword or its command line, left the stream in an earlier release: a password assignment in
/// small chunks, a multi-line curl command with an Authorization header, the same command with a
/// body larger than the window always carried before its header (also across Markdown
/// paragraphs, in small chunks, and with the carried window's edge inside the command's name), a
/// keyword rule whose value is on the next line, and a line too long to wait for its newline with
/// the cut right at the separator. What
/// arrives equals the whole text protected at once, placeholder for placeholder. A stream that
/// protected each release without enough of the text released before it would deliver these
/// values raw; one that echoed the context or replaced the wrong bytes would differ from it.
fn a_value_is_protected_when_its_context_left_earlier(_home: &Path) {
    let password = "c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0";
    let bearer = "9f8E7d6C5b4A39281706f5e4d3c2b1a0ffeeddcc";
    let adafruit = "k3j9x2m8q7w1e5r4t6y0u2i9o8p7a6s5";
    let lines = "ordinary output line\n".repeat(12);
    let cases = [
        (
            password,
            format!(
                "[db]\npassword = \"{password}\"\n{} {lines}",
                "x".repeat(40)
            ),
        ),
        (
            bearer,
            format!(
                "Run this:\ncurl https://api.internal.example.com/v1/items \\\n  -H \"Authorization: Bearer {bearer}\"\n{lines}"
            ),
        ),
        (
            adafruit,
            format!("settings.yaml:\nadafruit_io_key:\n  {adafruit}\n{lines}"),
        ),
    ];
    // A command with more text between it and its header than the window always carried: a
    // large body on its own line, the same command on one line too long to wait for its newline,
    // and Markdown paragraphs around a large block, whose blank lines double the line breaks.
    let body = "word ".repeat(28 * 1024);
    let block = format!("{{\"items\": [{}]}}", "\"value\", ".repeat(2000));
    let large = [
        format!(
            "curl https://api.internal.example.com/v1/items \\\n  -d '{body}' \\\n  -H \"Authorization: Bearer {bearer}\"\n{lines}"
        ),
        format!(
            "curl https://api.internal.example.com/v1/items -d '{body}' -H \"Authorization: Bearer {bearer}\"\n{lines}"
        ),
    ];
    let redactor = Redactor::new(Persona::default());
    // Placeholders differ only in their identity; the text around them must not.
    let shape = |text: &str| {
        let mut shaped = String::new();
        let mut at = 0;
        for (start, end, _) in agit::domain::privacy::projector::tokens(text) {
            shaped.push_str(&text[at..start]);
            shaped.push_str("{{T}}");
            at = end;
        }
        shaped.push_str(&text[at..]);
        shaped
    };
    let streamed = |text: &str, size: usize| {
        let mut stream = redactor.stream();
        let mut delivered = String::new();
        for chunk in text.as_bytes().chunks(size) {
            delivered.push_str(&stream.push(std::str::from_utf8(chunk).unwrap()).text);
        }
        delivered.push_str(&stream.flush().text);
        delivered
    };
    let paragraphs = format!(
        "I'll call the API with curl first.\n\n```json\n{block}\n```\n\nThen retry with this header:\n\n-H \"Authorization: Bearer {bearer}\"\n{lines}"
    );
    let runs = cases
        .iter()
        .flat_map(|(secret, text)| [1, 4, 16].map(|size| (*secret, text.clone(), size)))
        .chain(large.into_iter().map(|text| (bearer, text, 4096)))
        .chain([(bearer, paragraphs, 16)]);
    for (secret, text, size) in runs {
        let whole = redactor.scrub(&text).text;
        assert!(!whole.contains(secret));
        let delivered = streamed(&text, size);
        assert!(!delivered.contains(secret), "{delivered}");
        assert_eq!(shape(&delivered), shape(&whole), "chunks of {size}");
        assert_eq!(
            agit::domain::privacy::projector::tokens(&delivered).count(),
            1
        );
    }
    assert!(streamed(&cases[0].1, 4).contains("password = "));
    // A first release that ends just past the always-carried window from `curl`'s start, so the
    // window's edge falls inside the command's name, at each position within it. The lengths
    // follow the stream's window and its lookahead hold for this policy.
    for inside in 1..=3 {
        let first = format!(
            "curl {}{}\n{}",
            "word ".repeat(3275),
            "y".repeat(3 + inside),
            "x".repeat(128)
        );
        let second = format!(" -H \"Authorization: Bearer {bearer}\"\n");
        let mut stream = redactor.stream();
        let mut delivered = stream.push(&first).text;
        delivered.push_str(&stream.push(&second).text);
        delivered.push_str(&stream.flush().text);
        assert!(!delivered.contains(bearer), "{delivered}");
        assert_eq!(
            shape(&delivered),
            shape(&redactor.scrub(&format!("{first}{second}")).text)
        );
    }
    let suffix = format!("= \"{password}\"");
    let long = format!(
        "{} password {suffix}{}",
        "x".repeat(128 * 1024),
        "z".repeat(128 - suffix.len())
    );
    let mut stream = redactor.stream();
    let mut delivered = stream.push(&long).text;
    delivered.push_str(&stream.push("\nafter the long line\n").text);
    delivered.push_str(&stream.flush().text);
    assert!(
        !delivered.contains(password),
        "a long line delivered its password"
    );
    let whole = redactor
        .scrub(&format!("{long}\nafter the long line\n"))
        .text;
    assert_eq!(shape(&delivered), shape(&whole));
}

/// Text, stream output and JSON values the worker cannot process are withheld: the marker
/// replaces them and the fallible forms report an error. An implementation that passed the input
/// through on a worker failure would publish it unprotected.
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
    let mut stream = redactor.stream();
    let mut delivered = String::new();
    for chunk in prompt.as_bytes().chunks(97) {
        delivered.push_str(&stream.push(std::str::from_utf8(chunk).unwrap()).text);
    }
    delivered.push_str(&stream.flush().text);
    assert_eq!(delivered, WITHHELD);
    let value = serde_json::json!({"command": format!("cat <<EOF > key.pem\n{}\nEOF", key())});
    let withheld = redactor.scrub_json(&value).value;
    assert!(withheld.get("protection_error").is_some(), "{withheld}");
    assert!(!withheld.to_string().contains(BODY[0]));
    assert!(redactor.try_scrub_json(&value).is_err());
    // SAFETY: as above.
    unsafe { std::env::set_var("AGIT_HOME", home) };
    agit::domain::privacy::service::invalidate();
}
