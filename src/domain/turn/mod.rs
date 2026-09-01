//! Turn splitting and the hash chain.
//!
//! # A turn = from the person speaking to just before the person speaks again
//!
//! It includes **every** tool round trip in between. Measurement forces this definition.
//! Counting tool feedback as the start of a turn too ("feedback from a user or a tool opens a
//! turn") has two problems:
//!
//! | Session | Lines | With tool feedback | User text only |
//! |---|---|---|---|
//! | CC 7.1 MB | 3176 | **746 turns** | 73 turns |
//! | CC 1.1 MB | 344 | 87 turns | 7 turns |
//! | Codex 1.9 MB | 90 | 3 turns | 2 turns |
//!
//! First, 673 of those 746 CC turns are `tool_result`, while a Codex session of the same order
//! of magnitude has only 2-3 turns — one "turn" concept differing by two orders of magnitude
//! between the two sides does not line up across runtimes.
//!
//! Second, and harder: cutting at a `tool_result` boundary leaves a **dangling `tool_use`** (the
//! assistant issued the call and the result is cut away), and the model in the restored session
//! sees a tool call that never returns. On the Codex side the line distance from `function_call`
//! to `output` is 1, and a cut landing in between dangles the same way.
//!
//! Cutting only at real user prompts puts the boundary outside tool-call pairing by construction.
//!
//! # A turn hash is only for looking and comparing, not a version ID
//!
//! ```text
//! turn hash = hex(SHA256(previous turn hash ‖ 0x00 ‖ normalized content of this turn))[..40]
//! ```
//!
//! It carries **no** timestamp, no machine identity, no account name. That is deliberate, and it
//! is the entire value of this module:
//!
//! The version ID is the snapshot ID (see [`crate::domain::meta`]); it covers the raw transcript
//! bytes and is responsible for "naming one definite state" and for integrity. A turn hash
//! answers a different question — **"from which turn did your copy of this session and mine
//! diverge"**. That is answerable only when "a semantically identical turn gets the same hash
//! for different people".
//!
//! Stuffing the local ed25519 fingerprint and the turn's end time into the hash means two people
//! never compute the same turn hash and [`Chain::fork_point`] is identically 0 — fork-point
//! detection exists in the code while never taking effect in reality. Leaving identity and
//! timestamps out is exactly what buys it back.
//!
//! The cost is that "saying the same sentence twice gets the same hash". That is not a problem:
//! a turn hash is never persisted, never used as a tag, never part of any numbering; where
//! uniqueness is needed, the snapshot ID is what is used.
//!
//! Hence no `agit-` prefix here — that prefix marks a snapshot ID, and mixing the two suggests a
//! turn hash could also be handed to `agit clone x/y:<hash>`.
//!
//! # There is no "open turn"
//!
//! The chain is a list of turns, and the last turn is no different from the ones before it. A
//! `closed` / `open` distinction is needed only when the version ID comes from the last closed
//! turn; under the snapshot model the version covers everything present at the moment of the
//! commit, so neither that distinction nor the machinery it needs to work around Claude Code
//! having no end-of-turn signal exists here.
//!
//! [`crate::adapter::EventKind::TurnEnd`] stays in the IR (Codex data really carries that signal,
//! and rendering and counting have to recognize it), but it decides nothing about the version.
//!
//! # The chain recomputes after the fact
//!
//! A hash is **computed**, not recorded. The transcript is append-only, so reading the file once
//! at any moment recomputes the whole chain, identical to the letter with what was computed turn
//! by turn (observed as a session grew from 73 turns to 74: not one of the first 73 hashes
//! changed). Nothing has to be stored on disk for the chain.
//!
//! Contrast Claude Code's own scheme: every record observed is a UUID v4 (messages with identical
//! content get different uuids). A random identity cannot be recomputed, and that is what forces
//! such a scheme to record every turn.

use crate::adapter::{Event, EventKind, Session};
use sha2::{Digest, Sha256};

/// Count the user turns that have ended.
///
/// Codex and Cursor have a reliable `TurnEnd` signal; they do not write that marker while the
/// last turn is still running, so a bare `UserPrompt` count must not report an open turn as
/// completed. Claude Code and OpenCode have no equivalent end marker, so only real user prompts
/// can be counted.
pub fn completed_count(session: &Session) -> usize {
    let prompts = session.counts().prompts;
    let ends = session
        .events
        .iter()
        .filter(|event| event.kind == EventKind::TurnEnd)
        .count();
    if supports_turn_end(session.runtime.as_str()) {
        ends.min(prompts)
    } else {
        prompts
    }
}

fn supports_turn_end(runtime: &str) -> bool {
    matches!(runtime, "codex" | "cursor")
}

/// Hex length of a hash.
const HEX_LEN: usize = 40;

/// How many hex characters display truncates to.
///
/// Enough to tell turns apart inside one session (a chain runs a few dozen turns), and short
/// enough to fit one table column.
const SHORT_LEN: usize = 12;

/// One turn.
#[derive(Debug, Clone)]
pub struct Turn {
    /// Starts at 1.
    pub index: usize,
    pub hash: String,
    /// What the person said in this turn (the one-line summary).
    pub gist: String,
    /// Timestamp of the last event in this turn. **Not part of the hash**, display only.
    pub at: Option<String>,
    /// How many IR events this turn contains.
    pub events: usize,
    /// Of those, the ones the IR cannot express and drops (encrypted reasoning, vendor-proprietary
    /// encodings).
    ///
    /// Recorded to keep "lossy" visible: `agit log` can mark how much a turn dropped.
    pub dropped: usize,
}

/// The turn chain cut out of one session.
#[derive(Debug, Clone, Default)]
pub struct Chain {
    pub turns: Vec<Turn>,
}

impl Chain {
    /// Hash at the tip of the chain. Answers "which turn is this session on now".
    pub fn tip(&self) -> Option<&str> {
        self.turns.last().map(|t| t.hash.as_str())
    }

    /// Root = the hash of the first turn.
    pub fn root(&self) -> Option<&str> {
        self.turns.first().map(|t| t.hash.as_str())
    }

    pub fn hashes(&self) -> Vec<&str> {
        self.turns.iter().map(|t| t.hash.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// The fork point with another chain: the length of the longest common prefix.
    ///
    /// Both chains are lists, so this is O(n) and needs no graph algorithm. This is the main gain
    /// of a per-turn hash over a session-level hash — the fork point is exact to one turn.
    pub fn fork_point(&self, other: &Chain) -> usize {
        self.turns
            .iter()
            .zip(other.turns.iter())
            .take_while(|(a, b)| a.hash == b.hash)
            .count()
    }
}

/// The short form of a turn hash (for display).
pub fn short(hash: &str) -> String {
    hash.chars().take(SHORT_LEN).collect()
}

/// Compute the hash of one turn.
fn turn_hash(parent: Option<&str>, normalized: &str) -> String {
    let mut h = Sha256::new();
    // The separator is \0: it cannot appear inside a hash, so no shift of the boundary between
    // the two fields produces the same input.
    h.update(parent.unwrap_or("").as_bytes());
    h.update([0]);
    h.update(normalized.as_bytes());
    hex::encode(h.finalize())[..HEX_LEN].to_string()
}

/// Normalize the events of one turn into the string to be hashed.
///
/// **Only the part the IR models**, without the runtime's wrapper fields (uuid, parentUuid,
/// sessionId, file paths). Those fields necessarily differ after `agit clone` (`mint_id()` mints
/// new UUIDs, `render` re-renders), and including them gives the same conversation a different
/// hash on two machines — recognizing "the same conversation" is a turn hash's only job.
///
/// `EventKind::Other` **takes no part** — it is what the IR does not model (encrypted reasoning,
/// vendor-proprietary encodings) and a cross-runtime conversion drops it anyway. Including it
/// makes the hash differ across runtimes. How much was dropped is recorded in `Turn::dropped`, so
/// the loss is visible rather than silent.
///
/// Integrity of those excluded bytes is carried by the snapshot ID (it hashes the raw transcript
/// bytes), so keeping only the semantics here is safe.
fn normalize(events: &[&Event]) -> String {
    let mut s = String::new();
    for e in events {
        // `TurnEnd` takes no part either: it is a runtime signal rather than content, and only
        // Codex has it, so including it gives the same conversation different hashes under two
        // runtimes.
        //
        // Compact boundaries (both forms) take no part either, for the `TurnEnd` reason carried a
        // step further: the two runtimes' compact mechanisms are **fundamentally different**
        // (Claude Code writes a lossy summary tens of thousands of characters long, Codex writes
        // one filter marker carrying a window number), so once the same conversation has been
        // compacted by each of them the body of the boundary event cannot match. Including them
        // makes the cross-runtime hash differ; integrity of the boundary itself is carried by the
        // snapshot ID (it hashes the raw transcript bytes).
        // Tool output takes no part either, for the same kind of reason as `TurnEnd` but harder:
        //
        // A CC session has hundreds of `tool_result` (673 in the 3176-line one), while a Codex
        // session of the same order of magnitude has only 2-3 `function_call_output`; granularity
        // and shape both differ. Including it makes the same conversation hash differently under
        // the two runtimes — and recognizing "the same conversation" is a turn hash's only job
        // (see the module header).
        //
        // It must not count toward `Turn::dropped` either: that number means "what the IR cannot
        // express and a conversion loses", and tool output **is in the IR**, it just takes no part
        // in identity. Mixing it in distorts the "lossy" signal.
        if e.kind == EventKind::Other
            || e.kind == EventKind::TurnEnd
            || e.kind == EventKind::ToolResult
            || e.kind.is_compact()
        {
            continue;
        }
        // Type tag + body. The tool name takes part, because "which tool was called" is
        // substantive content.
        s.push_str(match e.kind {
            EventKind::UserPrompt => "u",
            // An interjection takes part in the hash: it is content the model really read in
            // this turn.
            EventKind::UserInterjection => "i",
            EventKind::AssistantReply => "a",
            EventKind::ToolUse => "t",
            EventKind::FileEdit => "e",
            EventKind::CompactFiltered
            | EventKind::CompactSummary
            | EventKind::TurnEnd
            | EventKind::ToolResult
            | EventKind::Other => unreachable!("filtered above"),
        });
        s.push('\x1f');
        if let Some(t) = &e.tool {
            s.push_str(t);
            s.push('\x1f');
        }
        if let Some(t) = &e.text {
            s.push_str(t);
        }
        s.push('\x1e');
    }
    s
}

/// Cut a session's events into turns and return each turn's **event indices** (into
/// `session.events`).
///
/// # Why the split is exposed on its own
///
/// [`chain_of`] answers only "what is each turn's hash", which is enough for `agit log`. A
/// consumer that renders a session as "one card per turn" (the web transcript) also needs to know
/// **which events belong to which turn**. Having it implement the splitting rule again gives the
/// rule "a turn starts when a person speaks" two implementations — two implementations drift apart
/// sooner or later and hand the same session different turn counts, and the turn ordinal appears
/// in sharable links.
///
/// So the rule is written here once, and `chain_of` goes through it too.
pub fn groups_of(session: &Session) -> Vec<Vec<usize>> {
    // Every UserPrompt opens a new turn.
    //
    // Events before the first UserPrompt (a system-injected environment block and the like) go
    // into the preamble. It is not a turn (nobody spoke), and it cannot be dropped either — it
    // takes part in the first turn's hash, because it really is part of the context the model saw
    // in that turn.
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut preamble: Vec<usize> = vec![];

    for (i, e) in session.events.iter().enumerate() {
        if e.kind == EventKind::UserPrompt {
            groups.push(vec![i]);
        } else if let Some(last) = groups.last_mut() {
            last.push(i);
        } else {
            preamble.push(i);
        }
    }

    // Preamble events are merged into the first turn.
    if let Some(first) = groups.first_mut() {
        let mut merged = preamble;
        merged.append(first);
        *first = merged;
    }

    groups
}

/// Cut turns out of the IR and compute the chain.
pub fn chain_of(session: &Session) -> Chain {
    let mut turns = vec![];
    let mut parent: Option<String> = None;

    for (i, idx) in groups_of(session).into_iter().enumerate() {
        let g: Vec<&Event> = idx.iter().map(|&j| &session.events[j]).collect();
        let hash = turn_hash(parent.as_deref(), &normalize(&g));

        let gist = g
            .iter()
            .find(|e| e.kind == EventKind::UserPrompt)
            .and_then(|e| e.text.as_deref())
            .map(|t| {
                let one_line = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let s: String = one_line.chars().take(60).collect();
                if one_line.chars().count() > 60 {
                    format!("{s}…")
                } else {
                    s
                }
            })
            .unwrap_or_default();

        turns.push(Turn {
            index: i + 1,
            hash: hash.clone(),
            gist,
            at: g.iter().rev().find_map(|e| e.timestamp.clone()),
            events: g.iter().filter(|e| e.kind != EventKind::Other).count(),
            dropped: g.iter().filter(|e| e.kind == EventKind::Other).count(),
        });
        parent = Some(hash);
    }

    Chain { turns }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Event;

    fn ev(kind: EventKind, text: &str, ts: Option<&str>) -> Event {
        Event {
            kind,
            text: Some(text.to_string()),
            timestamp: ts.map(String::from),
            paths: vec![],
            tool: None,
            line: None,
        }
    }

    fn session(events: Vec<Event>) -> Session {
        Session {
            id: "s".into(),
            runtime: "codex".into(),
            cwd: None,
            events,
        }
    }

    #[test]
    fn an_open_codex_prompt_is_not_a_completed_turn() {
        let s = session(vec![ev(EventKind::UserPrompt, "still working", None)]);
        assert_eq!(completed_count(&s), 0);
    }

    #[test]
    fn runtimes_without_end_markers_count_user_prompts() {
        let mut s = session(vec![ev(EventKind::UserPrompt, "done", None)]);
        s.runtime = "claude-code".into();
        assert_eq!(completed_count(&s), 1);
    }

    #[test]
    fn codex_counts_only_the_turns_with_end_markers() {
        let s = session(vec![
            ev(EventKind::UserPrompt, "closed", None),
            ev(EventKind::TurnEnd, "", None),
            ev(EventKind::UserPrompt, "still working", None),
        ]);
        assert_eq!(completed_count(&s), 1);
    }

    #[test]
    fn turns_are_cut_at_user_prompts_only() {
        // A turn = a person speaks → just before they speak again, with every tool round trip
        // in between.
        let s = session(vec![
            ev(EventKind::UserPrompt, "fix rotation bug", Some("t1")),
            ev(EventKind::AssistantReply, "check the exif", Some("t2")),
            ev(EventKind::ToolUse, "read exif.py", Some("t3")),
            ev(EventKind::AssistantReply, "found it", Some("t4")),
            ev(EventKind::UserPrompt, "add a test", Some("t5")),
            ev(EventKind::ToolUse, "write test", Some("t6")),
        ]);
        let c = chain_of(&s);
        assert_eq!(c.len(), 2, "two human utterances make two turns");
        assert_eq!(
            c.turns[0].events, 4,
            "tool round trips all count inside this turn"
        );
        assert_eq!(c.turns[1].events, 2);
    }

    /// The last turn is no different from the ones before it.
    ///
    /// Treating the last turn as "open" and keeping it out of the version leaves a Claude Code
    /// session where "a person said one thing and the agent worked a long time" with no version ID
    /// at all. Under the snapshot model the version is everything present at the moment of the
    /// commit, so no end-of-turn signal is needed here.
    #[test]
    fn the_last_turn_is_an_ordinary_turn() {
        let s = session(vec![
            ev(
                EventKind::UserPrompt,
                "make a subfolder and research it",
                Some("t1"),
            ),
            ev(EventKind::ToolUse, "ran for a long time", Some("t2")),
            ev(EventKind::AssistantReply, "finished", Some("t3")),
        ]);
        let c = chain_of(&s);
        assert_eq!(
            c.len(),
            1,
            "a turn with no end marker still counts as a turn"
        );
        assert!(c.tip().is_some());
    }

    /// Adding a runtime end marker changes nothing.
    ///
    /// Making it both the closing condition and an exclusion from the hash is a patch over an
    /// asymmetry between two runtimes. It decides nothing about the version, and the chain does
    /// not depend on it.
    #[test]
    fn turn_end_marker_affects_neither_hash_nor_count() {
        let body = vec![
            ev(EventKind::UserPrompt, "do something", Some("t1")),
            ev(EventKind::AssistantReply, "done", Some("t2")),
        ];
        let mut with_marker = body.clone();
        with_marker.push(Event {
            kind: EventKind::TurnEnd,
            text: None,
            timestamp: Some("t3".into()),
            paths: vec![],
            tool: None,
            line: None,
        });

        let a = chain_of(&session(body));
        let b = chain_of(&session(with_marker));
        assert_eq!(a.len(), b.len(), "the turn count does not change");
        assert_eq!(a.turns[0].hash, b.turns[0].hash, "the hash does not change");
    }

    /// **Tool output takes no part in identity.**
    ///
    /// This pins cross-runtime comparability. A 3176-line CC session has 673 `tool_result` where a
    /// Codex session of the same order of magnitude has only 2-3 `function_call_output` —
    /// granularity and shape both differ. Including it in the hash makes the same conversation
    /// compute a different value on each side and [`Chain::fork_point`] degenerates to a
    /// constant 0, which is the only reason a turn hash exists.
    ///
    /// The turn count and the `events` count **do** change (tool output really is something that
    /// happened in this turn); only the hash stays put.
    #[test]
    fn tool_output_does_not_move_the_turn_hash() {
        let body = vec![
            ev(EventKind::UserPrompt, "check lr in config", Some("t1")),
            ev(EventKind::ToolUse, "cat config.yaml", Some("t2")),
            ev(EventKind::AssistantReply, "it is 2e-3", Some("t4")),
        ];
        let mut with_output = body.clone();
        // Inserted after the call and before the reply — that is where it sits in a real corpus.
        with_output.insert(2, ev(EventKind::ToolResult, "lr: 2.000e-03", Some("t3")));

        let a = chain_of(&session(body));
        let b = chain_of(&session(with_output));
        assert_eq!(
            a.turns[0].hash, b.turns[0].hash,
            "tool output must not change turn identity"
        );
        assert_eq!(a.len(), b.len(), "tool output must not cut an extra turn");
        assert_eq!(
            b.turns[0].dropped, 0,
            "tool output is in the IR, not dropped; it must not pollute the lossy count"
        );
    }

    /// Hundreds of tool outputs inside one turn must not be cut into hundreds of turns.
    ///
    /// The module header carries the observed numbers: treating tool feedback as a turn boundary
    /// turns a 73-turn session into 746 turns. This pins that conclusion as new event variants are
    /// added.
    #[test]
    fn hundreds_of_tool_outputs_are_still_one_turn() {
        let mut events = vec![ev(EventKind::UserPrompt, "run the full eval", Some("t1"))];
        for i in 0..300 {
            events.push(ev(EventKind::ToolUse, "bash", Some("t2")));
            events.push(ev(
                EventKind::ToolResult,
                &format!("step {i} done"),
                Some("t3"),
            ));
        }
        let c = chain_of(&session(events));
        assert_eq!(c.len(), 1, "one human utterance is one turn");
    }

    /// **Semantic content alone decides a turn hash** — the reason it carries no identity and no
    /// clock.
    ///
    /// Two different people, two different machines, two different moments: as long as that turn's
    /// semantic content is the same, the turn hash is the same. Without it,
    /// [`Chain::fork_point`] always returns 0.
    #[test]
    fn identical_content_hashes_the_same_for_different_people() {
        let content = vec![
            ev(
                EventKind::UserPrompt,
                "refactor error handling",
                Some("2026-01-01T00:00:00Z"),
            ),
            ev(
                EventKind::AssistantReply,
                "look at the state",
                Some("2026-01-01T00:01:00Z"),
            ),
        ];
        // The same content, with wholly different timestamps on another machine (re-rendered).
        let same_content_later = vec![
            ev(
                EventKind::UserPrompt,
                "refactor error handling",
                Some("2026-06-30T23:59:00Z"),
            ),
            ev(EventKind::AssistantReply, "look at the state", None),
        ];
        let a = chain_of(&session(content));
        let b = chain_of(&session(same_content_later));
        assert_eq!(
            a.turns[0].hash, b.turns[0].hash,
            "identity and timestamps must stay out of the hash"
        );
    }

    #[test]
    fn content_alone_decides_the_hash() {
        let a = turn_hash(None, "same content");
        let b = turn_hash(None, "same content");
        assert_eq!(a, b, "reproducible");
        assert_ne!(
            a,
            turn_hash(None, "other content"),
            "different content → different hash"
        );
        assert_ne!(
            a,
            turn_hash(Some("prev"), "same content"),
            "the parent takes part"
        );
        assert_eq!(a.len(), HEX_LEN);
    }

    #[test]
    fn hash_has_no_version_prefix() {
        // An `agit-` prefix suggests it could be handed to `agit clone x/y:<hash>`.
        let h = turn_hash(None, "x");
        assert!(
            !h.starts_with(crate::domain::meta::ID_PREFIX),
            "a turn hash must not look like a snapshot ID: {h}"
        );
        assert_eq!(short(&h).len(), SHORT_LEN);
    }

    #[test]
    fn hashes_are_stable_when_the_session_grows() {
        // The core property of append-only: an existing turn's hash is fixed forever.
        // This is what "the chain recomputes after the fact" rests on.
        let base = vec![
            ev(EventKind::UserPrompt, "first prompt", Some("t1")),
            ev(EventKind::AssistantReply, "answer one", Some("t2")),
            ev(EventKind::UserPrompt, "second prompt", Some("t3")),
            ev(EventKind::AssistantReply, "answer two", Some("t4")),
        ];
        let c1 = chain_of(&session(base.clone()));

        let mut grown = base;
        grown.push(ev(EventKind::UserPrompt, "third prompt", Some("t5")));
        let c2 = chain_of(&session(grown));

        assert_eq!(c2.len(), c1.len() + 1);
        for (i, old) in c1.turns.iter().enumerate() {
            assert_eq!(
                old.hash,
                c2.turns[i].hash,
                "turn #{} must keep its hash",
                i + 1
            );
        }
    }

    /// Appending a tool round trip inside a turn changes **that turn's** hash.
    ///
    /// That is harmless: a turn hash is not a version ID, and the version is fixed by the snapshot
    /// ID at the moment of the commit.
    #[test]
    fn appending_within_a_turn_changes_only_that_turn() {
        let base = vec![
            ev(EventKind::UserPrompt, "one", Some("t1")),
            ev(EventKind::AssistantReply, "answer", Some("t2")),
            ev(EventKind::UserPrompt, "two", Some("t3")),
        ];
        let c1 = chain_of(&session(base.clone()));

        let mut with_tool = base;
        with_tool.push(ev(EventKind::ToolUse, "run a command", Some("t4")));
        let c2 = chain_of(&session(with_tool));

        assert_eq!(
            c1.len(),
            c2.len(),
            "the turn count does not change (nobody spoke)"
        );
        assert_eq!(
            c1.turns[0].hash, c2.turns[0].hash,
            "earlier turns are unaffected"
        );
        assert_ne!(
            c1.turns[1].hash, c2.turns[1].hash,
            "the turn still growing changes"
        );
    }

    #[test]
    fn other_events_are_excluded_but_counted() {
        // Encrypted reasoning and the like take no part in the hash (otherwise the hash differs
        // across runtimes), but how much was dropped must be reportable.
        let with_other = session(vec![
            ev(EventKind::UserPrompt, "question", Some("t1")),
            Event {
                kind: EventKind::Other,
                text: Some("encrypted reasoning".into()),
                timestamp: Some("t2".into()),
                paths: vec![],
                tool: None,
                line: None,
            },
        ]);
        let without = session(vec![ev(EventKind::UserPrompt, "question", Some("t1"))]);
        let c1 = chain_of(&with_other);
        let c2 = chain_of(&without);
        assert_eq!(c1.turns[0].dropped, 1, "the dropped count must be visible");
        assert_eq!(c2.turns[0].dropped, 0);
        assert_eq!(
            c1.turns[0].hash, c2.turns[0].hash,
            "Other takes no part in normalization, so the hash matches across runtimes"
        );
    }

    /// Compact boundaries **take no part** in a turn hash.
    ///
    /// The two runtimes' compact mechanisms are fundamentally different (a summary runs tens of
    /// thousands of characters, a filter is one line of marker text), so including them makes the
    /// same conversation hash differently across runtimes — the same reason `Other` and `TurnEnd`
    /// are excluded. Integrity of the boundary is carried by the snapshot ID.
    #[test]
    fn compact_boundaries_do_not_affect_turn_hashes() {
        let with_boundary = session(vec![
            ev(EventKind::UserPrompt, "question", Some("t1")),
            ev(
                EventKind::CompactSummary,
                "This session is being continued…",
                Some("t2"),
            ),
            ev(
                EventKind::CompactFiltered,
                "context window #1, 9 messages kept",
                Some("t3"),
            ),
            ev(EventKind::AssistantReply, "answer", Some("t4")),
        ]);
        let without = session(vec![
            ev(EventKind::UserPrompt, "question", Some("t1")),
            ev(EventKind::AssistantReply, "answer", Some("t4")),
        ]);
        assert_eq!(
            chain_of(&with_boundary).tip(),
            chain_of(&without).tip(),
            "compact boundaries take no part in normalization"
        );
    }

    #[test]
    fn preamble_before_first_prompt_is_merged_into_turn_one() {
        // The system-injected environment block sits before the first human utterance. It is not
        // a turn, but it takes part in the first turn's hash — it really is the context the model
        // saw in that turn.
        let s = session(vec![
            ev(
                EventKind::AssistantReply,
                "<environment_context>",
                Some("t0"),
            ),
            ev(EventKind::UserPrompt, "start", Some("t1")),
            ev(EventKind::UserPrompt, "second turn", Some("t2")),
        ]);
        let c = chain_of(&s);
        assert_eq!(c.len(), 2);
        assert_eq!(c.turns[0].index, 1, "the preamble is not a turn of its own");
        assert_eq!(
            c.turns[0].events, 2,
            "the preamble merges into the first turn"
        );
        assert_eq!(
            c.turns[0].gist, "start",
            "the gist still comes from what the person said"
        );
    }

    /// Fork-point detection: the reason a turn hash exists.
    #[test]
    fn fork_point_is_the_longest_common_prefix() {
        let common = vec![
            ev(EventKind::UserPrompt, "one", Some("t1")),
            ev(EventKind::UserPrompt, "two", Some("t2")),
            ev(EventKind::UserPrompt, "three", Some("t3")),
        ];
        let mut a = common.clone();
        a.push(ev(EventKind::UserPrompt, "direction A", Some("t4")));
        let mut b = common.clone();
        // The B side's timestamps differ too — in reality the two sides run separately and their
        // clocks necessarily fall out of step.
        b.push(ev(EventKind::UserPrompt, "direction B", Some("t9")));

        let ca = chain_of(&session(a));
        let cb = chain_of(&session(b));
        assert_eq!(ca.len(), 4);
        assert_eq!(ca.fork_point(&cb), 3, "the fork point is exact to a turn");
        assert_eq!(ca.root(), cb.root(), "the root is shared");
        assert_ne!(
            ca.turns[3].hash, cb.turns[3].hash,
            "turns after the fork must differ"
        );
    }

    #[test]
    fn different_roots_are_detectable() {
        let a = session(vec![ev(EventKind::UserPrompt, "fix album bug", Some("t1"))]);
        let b = session(vec![ev(EventKind::UserPrompt, "set up CI", Some("t1"))]);
        let ca = chain_of(&a);
        let cb = chain_of(&b);
        assert_ne!(
            ca.root(),
            cb.root(),
            "different roots must be distinguishable"
        );
        assert_eq!(ca.fork_point(&cb), 0, "there is no common prefix");
    }

    #[test]
    fn an_empty_session_has_an_empty_chain() {
        let c = chain_of(&session(vec![]));
        assert!(c.is_empty());
        assert!(c.tip().is_none());
        assert_eq!(c.fork_point(&Chain::default()), 0);
    }

    /// An interjection during a turn opens no new turn: it belongs to the turn already running,
    /// and the turn's subject stays the opening utterance. It does take part in that turn's hash —
    /// the model really read it.
    #[test]
    fn an_interjection_stays_inside_its_turn_but_shapes_its_hash() {
        let with = session(vec![
            ev(EventKind::UserPrompt, "fix rotation bug", Some("t1")),
            ev(EventKind::AssistantReply, "read the code first", Some("t2")),
            ev(EventKind::UserInterjection, "also add a test", Some("t3")),
            ev(EventKind::AssistantReply, "ok", Some("t4")),
        ]);
        let groups = groups_of(&with);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0], vec![0, 1, 2, 3]);
        let chain = chain_of(&with);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.turns[0].gist, "fix rotation bug");

        let without = session(vec![
            ev(EventKind::UserPrompt, "fix rotation bug", Some("t1")),
            ev(EventKind::AssistantReply, "read the code first", Some("t2")),
            ev(EventKind::AssistantReply, "ok", Some("t4")),
        ]);
        assert_ne!(
            chain.turns[0].hash,
            chain_of(&without).turns[0].hash,
            "an interjection is content the model read; it must take part in turn identity"
        );
    }
}
