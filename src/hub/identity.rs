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

// ─────────────────────── the aid cache: name is a label, aid is the identity ───────────────────────

/// What the Hub should do with its cached aid after reading the store again.
///
/// The Hub keys its URLs on **name** (they have to stay readable), but a name is a mutable label:
/// renaming must not touch the identity, and the same name coming back backed by a *different* store
/// is a different memory wearing an old label. Everything that distinction needs is decided here —
/// pure, so the awkward cases get tests rather than a debate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AidVerdict {
    /// Nothing to learn (no aid in the store yet), or the cache already agrees. Leave it alone.
    Unchanged,
    /// First sighting of this store's identity → cache it.
    Learn(String),
    /// The store's aid is not the one cached under this name: the repo was deleted and recreated, or
    /// a different store was pushed over it. **Detectable, and not silently overwritten** — the name
    /// now points at a different memory than every `.agit.toml` that recorded this aid believes.
    Replaced { was: String, now: String },
    /// Another agent already holds this aid. Two agents may never share one, so refuse to cache it:
    /// whoever got there first keeps it.
    Conflict { aid: String, held_by: String },
    /// The aid is the one this agent was **forked from**, and the source still holds it.
    ///
    /// Same outcome as `Conflict` — refuse to cache, the source keeps it — but a different *fact*: a
    /// clone carries its source's agent.toml until someone rebinds it, so this is the expected state
    /// of a fresh fork rather than two unrelated stores claiming one identity. Only one of the two is
    /// worth an alert, and calling both `Conflict` is what buries the real one.
    Inherited { aid: String, from: String },
}

/// Compare what the store says against what the Hub cached.
///
/// - `cached` — the aid in agents.json for `name` (None = never learned).
/// - `seen`   — the aid just read out of agent.toml (None = empty repo / no identity committed).
/// - `holder` — the agent that already has `seen` cached, if any (the uniqueness lookup).
/// - `forked_from_aid` — the **aid** of the agent this one was cloned from, if any. Keyed on the aid
///   and not on the source's name, for the reason the whole module exists: the name is a mutable
///   label, so lineage matched by name would turn a routine fork back into a reported collision the
///   day someone renamed its source. **Lineage, not permission**: it only ever splits one
///   already-refused outcome into two, and can never turn a refusal into a cache.
///
/// `seen = None` never clears the cache: an agent whose HEAD is temporarily unreadable has not lost
/// its identity, and forgetting it would let a *different* store slide in unnoticed afterwards.
pub fn reconcile(
    name: &str,
    cached: Option<&str>,
    seen: Option<&str>,
    holder: Option<&str>,
    forked_from_aid: Option<&str>,
) -> AidVerdict {
    let Some(seen) = seen else {
        return AidVerdict::Unchanged;
    };
    if cached == Some(seen) {
        return AidVerdict::Unchanged;
    }
    // Uniqueness first: an aid another agent already holds is not learnable, whether this is a first
    // sighting or a replacement. (A holder that *is* this agent is the cache we are refreshing.)
    if let Some(h) = holder {
        if h != name {
            // Seeing the very aid we were forked from is the expected state of a clone, not a
            // collision. Neither branch caches — whoever holds it keeps it either way — so this only
            // decides which fact gets reported and alerted on.
            return match forked_from_aid == Some(seen) {
                true => AidVerdict::Inherited { aid: seen.to_string(), from: h.to_string() },
                false => AidVerdict::Conflict { aid: seen.to_string(), held_by: h.to_string() },
            };
        }
    }
    match cached {
        None => AidVerdict::Learn(seen.to_string()),
        Some(was) => AidVerdict::Replaced { was: was.to_string(), now: seen.to_string() },
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
pub(crate) fn toml_string(text: &str, section: Option<&str>, key: &str) -> Option<String> {
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
pub(crate) fn strip_comment(line: &str) -> &str {
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

    // ── the aid cache ──

    #[test]
    fn first_sighting_is_learned() {
        assert_eq!(reconcile("pay", None, Some("agt_a"), None, None), AidVerdict::Learn("agt_a".into()));
    }

    #[test]
    fn agreeing_cache_is_left_alone() {
        assert_eq!(reconcile("pay", Some("agt_a"), Some("agt_a"), Some("pay"), None), AidVerdict::Unchanged);
    }

    #[test]
    fn empty_repo_never_clears_a_learned_aid() {
        // A store whose HEAD is unreadable has not lost its identity. Clearing it would also let a
        // different store be pushed over the name and read as a plain first sighting afterwards.
        assert_eq!(reconcile("pay", Some("agt_a"), None, None, None), AidVerdict::Unchanged);
        assert_eq!(reconcile("pay", None, None, None, None), AidVerdict::Unchanged);
    }

    #[test]
    fn a_recreated_repo_is_detected_not_swallowed() {
        // The whole point of item 1: `agit-hub` deleted + recreated under the same name is a
        // *different memory* wearing an old label, and every .agit.toml pointing at the old aid
        // deserves to find out.
        assert_eq!(
            reconcile("pay", Some("agt_old"), Some("agt_new"), None, None),
            AidVerdict::Replaced { was: "agt_old".into(), now: "agt_new".into() }
        );
    }

    #[test]
    fn two_agents_may_never_share_an_aid() {
        // A store cloned and pushed to a second name must not make both names claim one identity —
        // by-aid lookup would then have no answer to give.
        assert_eq!(
            reconcile("copy", None, Some("agt_a"), Some("pay"), None),
            AidVerdict::Conflict { aid: "agt_a".into(), held_by: "pay".into() }
        );
        // ...and the conflict outranks a replacement, too: stealing an aid off another agent by
        // force-pushing over your own store is the same theft.
        assert_eq!(
            reconcile("copy", Some("agt_b"), Some("agt_a"), Some("pay"), None),
            AidVerdict::Conflict { aid: "agt_a".into(), held_by: "pay".into() }
        );
    }

    #[test]
    fn holding_your_own_aid_is_not_a_conflict_with_yourself() {
        // The uniqueness lookup finds *this* agent when refreshing its own cache — that is the
        // normal path, not a collision.
        assert_eq!(reconcile("pay", Some("agt_a"), Some("agt_a"), Some("pay"), None), AidVerdict::Unchanged);
    }

    // ── lineage: a fork is not a collision ──

    #[test]
    fn a_fork_carrying_its_sources_aid_is_inherited_not_a_conflict() {
        // A clone carries the source's agent.toml until someone rebinds it, so the source still
        // holding that aid is the EXPECTED state — the same outcome as a conflict (nobody caches it)
        // but not the same fact, and only one of the two is worth waking someone for.
        assert_eq!(
            reconcile("fork", None, Some("agt_a"), Some("pay"), Some("agt_a")),
            AidVerdict::Inherited { aid: "agt_a".into(), from: "pay".into() }
        );
    }

    #[test]
    fn lineage_survives_the_source_being_renamed() {
        // Why lineage is keyed on the aid and not on the source's name: renaming `pay` to `billing`
        // must not turn a routine fork back into a reported collision. The name is a label; the aid
        // is the identity — the module's whole point, applied to itself.
        assert_eq!(
            reconcile("fork", None, Some("agt_a"), Some("billing"), Some("agt_a")),
            AidVerdict::Inherited { aid: "agt_a".into(), from: "billing".into() },
            "the holder is the same memory under a new label"
        );
    }

    #[test]
    fn lineage_does_not_excuse_a_collision_with_a_stranger() {
        // THE test that keeps the fix honest. A fork of `pay` whose store is force-pushed with an
        // UNRELATED agent's aid is a real theft, and having lineage must not launder it into
        // "inherited" — that would turn the fork feature into a way to silence the alarm.
        assert_eq!(
            reconcile("fork", None, Some("agt_zz"), Some("other"), Some("agt_a")),
            AidVerdict::Conflict { aid: "agt_zz".into(), held_by: "other".into() }
        );
    }

    #[test]
    fn lineage_is_matched_on_the_aid_not_on_being_a_fork() {
        // Being a fork of *something* is not the fact that matters; seeing *the aid you were forked
        // from* is. Anything looser labels a stranger's aid "inherited" the moment an agent has any
        // lineage at all.
        assert_eq!(
            reconcile("fork", None, Some("agt_a"), Some("pay"), None),
            AidVerdict::Conflict { aid: "agt_a".into(), held_by: "pay".into() },
            "no lineage recorded → a held aid is still a collision"
        );
    }

    #[test]
    fn a_rebound_fork_learns_its_own_aid_with_no_special_casing() {
        // The way out: rebind locally, push a fresh aid nobody holds. Lineage is irrelevant then —
        // it is a first sighting like any other, so the fork stops reporting inherited forever.
        assert_eq!(
            reconcile("fork", None, Some("agt_new"), None, Some("agt_a")),
            AidVerdict::Learn("agt_new".into())
        );
    }

    #[test]
    fn lineage_never_widens_what_reconcile_would_have_cached() {
        // Lineage only ever splits one already-refused outcome in two. Every verdict that does NOT
        // involve someone else holding the aid must be identical with and without it — otherwise
        // `forked_from` has become a permission rather than a label.
        for (cached, seen, holder) in [
            (None, Some("agt_a"), None),
            (Some("agt_a"), Some("agt_a"), Some("pay")),
            (Some("agt_old"), Some("agt_new"), None),
            (Some("agt_a"), None, None),
        ] {
            assert_eq!(
                reconcile("pay", cached, seen, holder, None),
                reconcile("pay", cached, seen, holder, Some("anything")),
                "lineage changed a verdict it has no business changing: {cached:?} {seen:?} {holder:?}"
            );
        }
    }
}
