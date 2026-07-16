//! Reading an agent's identity out of the store — `agent.toml`.
//!
//! The aid (`agt_<uuid>`) is **minted by the client and committed into the store's history**: it
//! travels with the repo and survives renames, a different Hub, and a different machine. The Hub
//! does not mint aids; it just reads out what `git show <ref>:agent.toml` gives back. Which means an
//! empty repo freshly created by `POST /api/agents`, with nothing pushed yet, has **no aid** — so
//! report null and be honest about it.
//!
//! Only the `agt_` prefix counts. The old scaffold writes `id = "unnamed-agent"` (see
//! `crate::init::scaffold`), and **every store carries that same value** — treating it as an
//! identity would make all old repos share one "identity", which is worse than having none.

/// The identity read out of agent.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// A proper aid.
    Aid(String),
    /// Has an agent.toml, but no `agt_` identity (old store / hand-written placeholder).
    Unidentified,
}

/// Parse agent.toml for the identity. Tolerates both spellings:
///   `id` inside the `[agent]` section (new format), and a top-level `id` (old scaffold).
pub fn parse_agent_toml(text: &str) -> Identity {
    let id = toml_string(text, Some("agent"), "id").or_else(|| toml_string(text, None, "id"));
    match id {
        Some(v) if is_aid(&v) => Identity::Aid(v),
        _ => Identity::Unidentified,
    }
}

/// aid shape gate: `agt_` + non-empty, [A-Za-z0-9-] only. This string lands in JSON and in logs, so
/// validate it as untrusted input first.
pub fn is_aid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("agt_") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 64
        && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Minimal TOML lookup: only enough to read `key = "value"`. No toml dependency — the Hub only needs
/// to recognize one string key, and pulling in a whole parser for that does not pay. Unrecognized →
/// None (and the caller then reports null rather than guessing).
///
/// `section` = None means take the key from the top level (before the first `[...]`).
fn toml_string(text: &str, section: Option<&str>, key: &str) -> Option<String> {
    let mut cur: Option<String> = None;
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            cur = Some(name.trim().to_string());
            continue;
        }
        if cur.as_deref() != section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        // Only strings wrapped in double or single quotes count.
        for q in ['"', '\''] {
            if let Some(inner) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
                return Some(inner.to_string());
            }
        }
        return None;
    }
    None
}

/// Drop the comment after `#`. A `#` inside quotes does not count — `id = "agt_#1"` must not be cut.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut in_s, mut in_d) = (false, false);
    for i in 0..b.len() {
        match b[i] {
            b'\'' if !in_d => in_s = !in_s,
            b'"' if !in_s => in_d = !in_d,
            b'#' if !in_s && !in_d => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_new_layout() {
        let t = r#"
# Agent identity
[agent]
id = "agt_9f1c2d3e-4b5a-6c7d-8e9f-0a1b2c3d4e5f"
name = "reviewer"
created = "2026-07-16T10:00:00Z"
"#;
        assert_eq!(
            parse_agent_toml(t),
            Identity::Aid("agt_9f1c2d3e-4b5a-6c7d-8e9f-0a1b2c3d4e5f".into())
        );
    }

    #[test]
    fn reads_top_level_id_too() {
        // Old stores may put id at the top level — both layouts must be recognized.
        assert_eq!(parse_agent_toml("id = \"agt_abc123\"\n"), Identity::Aid("agt_abc123".into()));
    }

    #[test]
    fn agent_section_wins_over_top_level() {
        let t = "id = \"agt_old\"\n[agent]\nid = \"agt_new\"\n";
        assert_eq!(parse_agent_toml(t), Identity::Aid("agt_new".into()));
    }

    #[test]
    fn legacy_placeholder_is_not_an_identity() {
        // crate::init::scaffold writes this line into **every** store. Using it as an identity =
        // every repo sharing one name.
        assert_eq!(parse_agent_toml("# Agent identity\nid = \"unnamed-agent\"\n"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("[agent]\nid = \"unnamed-agent\"\n"), Identity::Unidentified);
    }

    #[test]
    fn missing_or_junk_is_unidentified() {
        assert_eq!(parse_agent_toml(""), Identity::Unidentified);
        assert_eq!(parse_agent_toml("[agent]\nname = \"x\"\n"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("id = agt_unquoted\n"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("this is not toml"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("[other]\nid = \"agt_x\"\n"), Identity::Unidentified);
    }

    #[test]
    fn aid_shape_is_enforced() {
        assert!(is_aid("agt_abc-123"));
        assert!(!is_aid("agt_"));
        assert!(!is_aid("unnamed-agent"));
        assert!(!is_aid("AGT_abc"));
        // This string goes into JSON / logs: anything with the wrong shape is refused
        assert!(!is_aid("agt_a/b"));
        assert!(!is_aid("agt_a b"));
        assert!(!is_aid("agt_\"x"));
        assert!(!is_aid(&format!("agt_{}", "x".repeat(65))));
    }

    #[test]
    fn trailing_comment_is_stripped() {
        assert_eq!(parse_agent_toml("id = \"agt_abc\"  # this is the identity\n"), Identity::Aid("agt_abc".into()));
        assert_eq!(parse_agent_toml("# id = \"agt_x\"\n"), Identity::Unidentified, "a commented-out key does not count");
    }

    #[test]
    fn comment_stripper_respects_quotes() {
        // A `#` inside quotes does not start a comment. (These values would not pass is_aid's shape
        // gate, so test the comment-stripping step directly.)
        assert_eq!(strip_comment("id = \"a#b\"  # comment").trim(), "id = \"a#b\"");
        assert_eq!(strip_comment("id = 'a#b'").trim(), "id = 'a#b'");
        assert_eq!(strip_comment("# whole-line comment"), "");
        assert_eq!(strip_comment("no comment here"), "no comment here");
    }

    #[test]
    fn single_quotes_work() {
        assert_eq!(parse_agent_toml("[agent]\nid = 'agt_abc'\n"), Identity::Aid("agt_abc".into()));
    }
}
