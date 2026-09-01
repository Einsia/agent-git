//! Envelope core: the shape, hash and VIEW slicing of **every single** record in v0 JSONL and
//! v1 `events/**`.
//!
//! # Invariants (violating any one of them breaks the wire format)
//!
//! 1. **One envelope per parseable line**. Every v1 event object (and every v0 line) is an
//!    envelope:
//!
//!    ```json
//!    {"_source":"claude-code","_session_id":"agit-...","_object_hash":"...","content":{...raw line...}}
//!    ```
//!
//!    Field declaration order is byte order (serde serializes in declaration order);
//!    `envelope_wire_shape_is_stable` pins it.
//!
//! 2. **The skip rule**: empty lines and lines that fail to parse (a truncated tail) never enter
//!    the repo; once the tail is complete it lands with the next commit. So the envelope sequence
//!    aligns with the **parseable lines** of the live text, not with physical line numbers.
//!
//! 3. `_session_id` is the **branch session declaration**: the `session` field from
//!    `session/meta.json` (of the form `agit-` + 40 hex). It is carried into the envelope
//!    unchanged and **never recomputed from content** — the `sessionId` inside content belongs
//!    to that recording, which is a different thing from the branch identity.
//!
//! 4. `_source` is the normalized runtime id (see [`crate::adapter::normalize`]).
//!
//! 5. `_object_hash = hex(SHA256(serde_json::to_string(&content)))[..40]`. Its canonicality
//!    (independent of key insertion order, `{"n":1e0}` normalized to `1.0`) rests entirely on
//!    serde_json's **default features**: objects go through BTreeMap (keys sorted), numbers
//!    through f64. Cargo.toml carries the comment forbidding `preserve_order` /
//!    `arbitrary_precision`, and the `object_hash_*` tripwire tests fail loudly the day someone
//!    adds one.
//!
//! # Coordinate discipline
//!
//! An IR event's `line` counts **every physical line** (a bad line takes a number), while the
//! envelope sequence has already tightened over bad lines. So VIEW deconstruction **cuts the
//! window on the raw live lines first and wraps the slice afterwards** — never index the wrapped
//! envelope sequence with an IR line number.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::adapter::{self, Session};

/// One envelope line. Field declaration order == wire byte order; do not reorder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    #[serde(rename = "_source")]
    pub source: String,
    #[serde(rename = "_session_id")]
    pub session_id: String,
    #[serde(rename = "_object_hash")]
    pub object_hash: String,
    pub content: serde_json::Value,
}

/// Canonical serialization. Serializing a `Value` cannot fail — keys are always strings and
/// numbers are always valid (NaN/±∞ cannot form a `Number`), so the failure branch is on a par
/// with `unreachable!`.
fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|e| unreachable!("serialization cannot fail here: {e}"))
}

/// The content address of `content`. The single implementation point of a frozen algorithm.
pub fn object_hash(content: &serde_json::Value) -> String {
    hex::encode(Sha256::digest(to_json(content).as_bytes()))[..40].to_string()
}

/// Wrap live transcript text into envelope text: one envelope per parseable line, `\n`-terminated.
///
/// Empty and bad lines are skipped (invariant 2). The return value persists directly as
/// `session/log.jsonl`.
pub fn wrap_lines(text: &str, source: &str, session_id: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let Ok(content) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let env = Envelope {
            source: source.to_string(),
            session_id: session_id.to_string(),
            object_hash: object_hash(&content),
            content,
        };
        out.push_str(&to_json(&env));
        out.push('\n');
    }
    out
}

/// Unwrap back to the raw line text. Any line that is not a valid envelope is an error (the
/// message carries the 1-based line number).
///
/// The lines produced are **canonically serialized** (BTreeMap has reordered the keys) —
/// semantically identical value for value, but the bytes do not reproduce the source file.
/// Downstream comparison always goes through Value/hash, never through the original bytes.
pub fn unwrap_strict(text: &str) -> Result<String> {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let env: Envelope = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("line {} is not a valid envelope: {e}", i + 1))?;
        out.push_str(&to_json(&env.content));
        out.push('\n');
    }
    Ok(out)
}

/// Unwrap back to the raw line text, skipping and counting bad lines. For reading a repo that
/// "an older tool may have written badly".
pub fn unwrap_lossy(text: &str) -> (String, usize /* lines skipped */) {
    let mut out = String::new();
    let mut skipped = 0;
    for line in text.lines() {
        match serde_json::from_str::<Envelope>(line) {
            Ok(env) => {
                out.push_str(&to_json(&env.content));
                out.push('\n');
            }
            Err(_) => skipped += 1,
        }
    }
    (out, skipped)
}

/// How the live transcript stands relative to the committed content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    /// No growth.
    Noop,
    /// Pure append.
    Append,
    /// Something in the middle was rewritten — this session's history is no longer the
    /// committed one.
    Diverged,
}

/// The `_object_hash` sequence of envelope text (unparseable lines are skipped).
///
/// The single test for "how long the committed content is and what each item is":
/// [`view_is_suffix_of`]'s suffix comparison runs on it, and so does doctor's count of "how many
/// lines the VIEW and the transcript each have".
pub fn envelope_hashes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Envelope>(l).ok())
        .map(|e| e.object_hash)
        .collect()
}

/// The content-address sequence of a live transcript (raw line text), one per **parseable** line.
///
/// The same ruler as `_object_hash`: this string of values is exactly what the envelope sequence
/// aligns with (invariant 2). doctor takes it together with [`envelope_hashes`] to answer "how
/// many lines are new since the committed version".
pub fn live_hashes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| object_hash(&v))
        .collect()
}

/// Continuity check: the committed envelope hash sequence must be a prefix of the hash sequence
/// of the live parseable lines.
///
/// An unfinished tail on the live side stays out of the comparison under the skip rule
/// (invariant 2), so "a transcript that stopped halfway, truncated" is never misjudged as
/// Diverged.
pub fn continuity(stored_envelopes: &str, live: &str) -> Continuity {
    compare_hashes(envelope_hashes(stored_envelopes), live_hashes(live))
}

/// The same test, but it **recomputes** each envelope's content address instead of reading the
/// `_object_hash` it carries.
///
/// [`continuity`] reads the stored field, which is right for an envelope stream nobody has
/// touched; it is exactly wrong once the caller has rewritten `content` (hydrating repository
/// placeholders back to plaintext) — the content changed, while the carried address still names
/// the pre-projection one, so every projected session is judged Diverged. Address the content you
/// are holding, and the two sides become comparable.
///
/// Use it only when the caller has just done that rewrite **itself**: by definition it does not
/// verify `_object_hash`, so for an envelope stream of unknown provenance it does not answer
/// "has this history been tampered with".
pub fn continuity_of_content(stored_envelopes: &str, live: &str) -> Continuity {
    let stored = stored_envelopes
        .lines()
        .filter_map(|l| serde_json::from_str::<Envelope>(l).ok())
        .map(|e| object_hash(&e.content))
        .collect();
    compare_hashes(stored, live_hashes(live))
}

fn compare_hashes(stored: Vec<String>, live: Vec<String>) -> Continuity {
    if stored.len() > live.len() || stored.iter().zip(live.iter()).any(|(a, b)| a != b) {
        return Continuity::Diverged;
    }
    if stored.len() == live.len() {
        Continuity::Noop
    } else {
        Continuity::Append
    }
}

/// The physical line number (0-based) of the last compact boundary. A session that has never
/// compacted is None.
pub fn last_compact_boundary(session: &Session) -> Option<usize> {
    session
        .events
        .iter()
        .rev()
        .find_map(|e| if e.kind.is_compact() { e.line } else { None })
}

/// Build the resume VIEW from a live transcript: the raw slice from the last compact boundary
/// (inclusive) to the end of the file; with no boundary, the whole text.
///
/// The cut is on the **raw live lines**, before wrapping (coordinate discipline). The slice
/// returned is then wrapped by [`wrap_lines`] into `session/VIEW`.
pub fn view_of_live(text: &str, runtime: &str) -> Result<String> {
    let session = adapter::get(runtime)?.parse(text)?;
    let Some(boundary) = last_compact_boundary(&session) else {
        return Ok(text.to_string());
    };
    // Byte offset where line `boundary` starts. split_inclusive('\n') yields the same items in
    // the same order as lines(), and newline is ASCII, so the offset always lands on a character
    // boundary.
    let offset: usize = text
        .split_inclusive('\n')
        .take(boundary)
        .map(str::len)
        .sum();
    Ok(text[offset..].to_string())
}

/// Whether this text has to go to the LOG for its bootstrap: the format family is codex and the
/// first parseable line is not session_meta. Callers ask this before doing any I/O —
/// materializing a whole LOG for one bootstrap line is a wasted read.
pub fn needs_bootstrap(view_text: &str, runtime: &str) -> bool {
    if adapter::get(runtime).map(|a| a.format()).ok() != Some("codex") {
        return false;
    }
    !view_text
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .is_some_and(|v| v.get("type").and_then(|x| x.as_str()) == Some("session_meta"))
}

/// Give the LOG's first-line `session_meta` back to a codex text that opens at a compact
/// boundary.
///
/// It acts only when all three hold: the format family is codex (no other family has a bootstrap
/// concept), the first parseable line of `view_text` is not session_meta, and the LOG's first
/// envelope really is session_meta. What comes back is the **original** — history_mode /
/// model_provider / base_instructions, not one key short; the identity keys (id / session_id /
/// cwd) are handled uniformly by the rewrite at load time and are not touched here.
pub fn restore_bootstrap(view_text: &str, log_env_text: &str, runtime: &str) -> String {
    if !needs_bootstrap(view_text, runtime) {
        return view_text.to_string();
    }
    let Some(meta_line) = log_env_text
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Envelope>(l).ok())
        .map(|e| e.content)
        .filter(|c| c.get("type").and_then(|x| x.as_str()) == Some("session_meta"))
        .map(|c| to_json(&c))
    else {
        return view_text.to_string();
    };
    format!(
        "{meta_line}
{view_text}"
    )
}

/// VIEW consistency: `session/VIEW` must be an ordered suffix, by hash, of `session/log.jsonl`.
///
/// The empty VIEW is a suffix of any transcript; an empty transcript accepts only an empty VIEW.
/// Both arguments are envelope text.
pub fn view_is_suffix_of(view_env: &str, transcript_env: &str) -> bool {
    let view = envelope_hashes(view_env);
    let transcript = envelope_hashes(transcript_env);
    let Some(start) = transcript.len().checked_sub(view.len()) else {
        return false;
    };
    transcript[start..] == view[..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};

    const SRC: &str = "claude-code";
    const SID: &str = "agit-0123456789abcdef0123456789abcdef01234567";

    // ── object_hash tripwires ──────────────────────────────────────────

    #[test]
    fn object_hash_is_insertion_order_invariant() {
        // The same object built in two insertion orders. The moment feature `preserve_order`
        // is on, the two serializations' key orders diverge and this assertion goes red.
        let mut m1 = Map::new();
        m1.insert("b".into(), json!(1));
        m1.insert("a".into(), json!(2));
        let mut m2 = Map::new();
        m2.insert("a".into(), json!(2));
        m2.insert("b".into(), json!(1));
        assert_eq!(
            object_hash(&Value::Object(m1)),
            object_hash(&Value::Object(m2)),
            "the hash must not depend on key insertion order"
        );
    }

    #[test]
    fn object_hash_precision_tripwire() {
        // Under default features 1e0 parses to f64 1.0 and prints normalized; the moment
        // `arbitrary_precision` is added, `1e0` is kept verbatim and this fails at once.
        let v: Value = serde_json::from_str(r#"{"n":1e0}"#).unwrap();
        assert_eq!(to_json(&v), r#"{"n":1.0}"#);
    }

    // ── wire shape ─────────────────────────────────────────────────────

    #[test]
    fn envelope_wire_shape_is_stable() {
        let env = Envelope {
            source: "claude-code".into(),
            session_id: SID.into(),
            object_hash: "a".repeat(40),
            content: json!({"k": 1}),
        };
        let expected = format!(
            "{{\"_source\":\"claude-code\",\"_session_id\":\"{SID}\",\"_object_hash\":\"{}\",\"content\":{{\"k\":1}}}}",
            "a".repeat(40)
        );
        assert_eq!(
            to_json(&env),
            expected,
            "field names and order are the wire format; changing them is a break"
        );
    }

    #[test]
    fn bootstrap_restoration_gates() {
        let meta = r#"{"type":"session_meta","timestamp":"t","payload":{"id":"OLD","session_id":"OLD","cwd":"/w","history_mode":"paginated","model_provider":"azure","timestamp":"t"}}"#;
        let meta_v: serde_json::Value = serde_json::from_str(meta).unwrap();
        let log_env = wrap_lines(meta, "codex", SID);
        let view = r#"{"type":"compacted","ordinal":9,"payload":{"window_number":2,"replacement_history":[]}}"#;

        let got = restore_bootstrap(view, &log_env, "codex");
        let first: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(
            first, meta_v,
            "the original comes back on the first line with every key (history_mode / provider)"
        );
        assert!(got.ends_with(view), "the VIEW body is untouched");

        // A VIEW that already has a bootstrap is left alone; so is another format family; so
        // is a LOG whose first line is not meta.
        let with_meta = format!(
            "{meta}
{view}"
        );
        assert_eq!(restore_bootstrap(&with_meta, &log_env, "codex"), with_meta);
        assert_eq!(restore_bootstrap(view, &log_env, "claude-code"), view);
        let other_log = wrap_lines(view, "codex", SID);
        assert_eq!(restore_bootstrap(view, &other_log, "codex"), view);
    }

    #[test]
    fn wrap_skips_empty_and_unparseable_lines() {
        let text = "\n{\"a\":1}\n{\"b\":2}\n{\"truncated\":\n\n";
        let wrapped = wrap_lines(text, SRC, SID);
        let lines: Vec<&str> = wrapped.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "neither an empty line nor a truncated tail enters the repo"
        );
        assert!(wrapped.ends_with('\n'));
        for l in lines {
            serde_json::from_str::<Envelope>(l).unwrap();
        }
    }

    // ── unwrap ─────────────────────────────────────────────────────────

    #[test]
    fn unwrap_roundtrip_semantic_identity() {
        let text = "{\"z\":1,\"a\":2}\n{\"b\":[1,2,3]}\n";
        let wrapped = wrap_lines(text, SRC, SID);
        let unwrapped = unwrap_strict(&wrapped).unwrap();
        let orig: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let back: Vec<Value> = unwrapped
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(orig, back, "a round trip must be semantically identical");
        assert!(
            unwrapped.starts_with("{\"a\":2"),
            "key order is canonicalized: {unwrapped:?}"
        );
    }

    #[test]
    fn unwrap_strict_rejects_garbage() {
        let good = wrap_lines("{\"a\":1}", SRC, SID);
        let bad = format!("{good}garbage line\n");
        let e = unwrap_strict(&bad).unwrap_err().to_string();
        assert!(e.contains('2'), "the error carries the line number: {e}");
    }

    #[test]
    fn unwrap_lossy_counts_skips() {
        let good = wrap_lines("{\"a\":1}\n{\"b\":2}", SRC, SID);
        let mixed = format!("{good}garbage line\n\n");
        let (out, skipped) = unwrap_lossy(&mixed);
        assert_eq!(
            skipped, 2,
            "both the garbage line and the empty line are counted"
        );
        assert_eq!(out.lines().count(), 2);
    }

    // ── continuity ─────────────────────────────────────────────────────

    #[test]
    fn continuity_noop_when_live_extends_nothing() {
        let live = "{\"a\":1}\n{\"b\":2}\n";
        assert_eq!(
            continuity(&wrap_lines(live, SRC, SID), live),
            Continuity::Noop
        );
    }

    #[test]
    fn continuity_append_on_growth() {
        let v1 = "{\"a\":1}\n{\"b\":2}\n";
        let v2 = format!("{v1}{{\"c\":3}}\n");
        assert_eq!(
            continuity(&wrap_lines(v1, SRC, SID), &v2),
            Continuity::Append
        );
    }

    #[test]
    fn continuity_diverged_on_rewrite() {
        let v1 = "{\"a\":1}\n{\"b\":2}\n";
        let rewritten = "{\"a\":1}\n{\"b\":999}\n";
        assert_eq!(
            continuity(&wrap_lines(v1, SRC, SID), rewritten),
            Continuity::Diverged
        );
    }

    #[test]
    fn continuity_ignores_truncated_tail_on_both_sides() {
        let live = "{\"a\":1}\n{\"b\":2}\n";
        let stored = wrap_lines(live, SRC, SID);
        let live_broken = format!("{live}{{\"broken\":");
        assert_eq!(
            continuity(&stored, &live_broken),
            Continuity::Noop,
            "an unfinished tail must not count as divergence"
        );
        let live_done = live_broken.replace("{\"broken\":", "{\"broken\":true}");
        assert_eq!(continuity(&stored, &live_done), Continuity::Append);
    }

    // ── VIEW construction ──────────────────────────────────────────────

    fn claude_user(text: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    fn claude_compact(text: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"isCompactSummary\":true,\"parentUuid\":null,\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    fn claude_assistant(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
        )
    }

    #[test]
    fn view_is_everything_without_compact() {
        let text = claude_user("first message") + &claude_assistant("answer");
        assert_eq!(view_of_live(&text, "claude-code").unwrap(), text);
    }

    #[test]
    fn view_starts_at_last_claude_compact_summary() {
        let pre = claude_user("first message");
        let c1 = claude_compact("SUMMARY-ONE");
        let mid = claude_assistant("middle");
        let c2 = claude_compact("SUMMARY-TWO");
        let tail = claude_assistant("carry on");
        let text = format!("{pre}{c1}{mid}{c2}{tail}");
        assert_eq!(
            view_of_live(&text, "claude-code").unwrap(),
            format!("{c2}{tail}"),
            "the VIEW starts at the summary line of the last compact"
        );
    }

    #[test]
    fn view_starts_at_last_codex_compacted() {
        let meta = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"cwd\":\"/r\"}}\n";
        let u1 = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"ask\"}]}}\n";
        let compacted = "{\"type\":\"compacted\",\"payload\":{\"replacement_history\":[],\"window_number\":1}}\n";
        let u2 = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"carry on\"}]}}\n";
        let text = format!("{meta}{u1}{compacted}{u2}");
        assert_eq!(
            view_of_live(&text, "codex").unwrap(),
            format!("{compacted}{u2}")
        );
    }

    #[test]
    fn view_boundary_uses_ir_line_numbers_over_all_physical_lines() {
        // Bad lines take physical line numbers: a truncated garbage line comes first, the
        // compact boundary after it. Indexing "the wrapped sequence" with an IR line number cuts
        // in the wrong place — this pins that the cut uses physical lines.
        let pre = claude_user("first message");
        let garbage = "{\"type\":\"user\",\"mes\n";
        let c1 = claude_compact("SUMMARY");
        let tail = claude_assistant("carry on");
        let text = format!("{pre}{garbage}{c1}{tail}");
        assert_eq!(
            view_of_live(&text, "claude-code").unwrap(),
            format!("{c1}{tail}"),
            "a garbage line occupies a physical line number; the VIEW is cut on physical lines"
        );
    }

    // ── hash sequences for counting ────────────────────────────────────

    #[test]
    fn hash_sequences_are_what_continuity_compares() {
        let live = "{\"a\":1}\n{\"b\":2}\n";
        let stored = wrap_lines(live, SRC, SID);
        assert_eq!(
            envelope_hashes(&stored),
            live_hashes(live),
            "the same content must yield the same sequence on the envelope side and the live side"
        );

        let grown = format!("{live}{{\"c\":3}}\n");
        assert_eq!(continuity(&stored, &grown), Continuity::Append);
        assert_eq!(
            live_hashes(&grown).len() - envelope_hashes(&stored).len(),
            1,
            "doctor's count of new lines is the length difference between these two sequences"
        );
    }

    #[test]
    fn live_hashes_skip_empty_and_unfinished_lines() {
        assert_eq!(live_hashes("{\"a\":1}\n\n{\"broken\":").len(), 1);
        assert!(envelope_hashes("garbage line\n").is_empty());
    }

    // ── view ⊆ transcript ──────────────────────────────────────────────

    #[test]
    fn view_is_suffix_of_accepts_proper_suffix() {
        let t3 = wrap_lines("{\"a\":1}\n{\"b\":2}\n{\"c\":3}", SRC, SID);
        let view = wrap_lines("{\"b\":2}\n{\"c\":3}", SRC, SID);
        assert!(view_is_suffix_of(&view, &t3));
    }

    #[test]
    fn view_is_suffix_of_rejects_reorder() {
        let t3 = wrap_lines("{\"a\":1}\n{\"b\":2}\n{\"c\":3}", SRC, SID);
        let view = wrap_lines("{\"c\":3}\n{\"b\":2}", SRC, SID);
        assert!(
            !view_is_suffix_of(&view, &t3),
            "a reordering is not a suffix"
        );
    }

    #[test]
    fn view_is_suffix_of_rejects_foreign_hash() {
        let t3 = wrap_lines("{\"a\":1}\n{\"b\":2}\n{\"c\":3}", SRC, SID);
        let view = wrap_lines("{\"x\":9}", SRC, SID);
        assert!(
            !view_is_suffix_of(&view, &t3),
            "a foreign object cannot appear in the VIEW"
        );
    }

    #[test]
    fn view_is_suffix_of_empty_sides() {
        let t3 = wrap_lines("{\"a\":1}\n{\"b\":2}", SRC, SID);
        assert!(
            view_is_suffix_of("", &t3),
            "the empty VIEW is a suffix of any transcript"
        );
        assert!(view_is_suffix_of("", ""));
        assert!(
            !view_is_suffix_of(&wrap_lines("{\"c\":3}", SRC, SID), ""),
            "an empty transcript accepts only an empty VIEW"
        );
    }
}
