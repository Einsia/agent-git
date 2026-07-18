//! Merge requests — the reviewable object that makes the Hub a place to *collaborate* on memories
//! rather than a viewer of them.
//!
//! **The Hub does not merge anything.** Reconciling two agents' memories is `agit a merge`, and it
//! runs locally, where the code and the LLM are (see
//! docs/plans/2026-07-17-agent-identity-and-handoff-design.md); there is no model on the server and
//! there should not be one. What the Hub hosts is the *artifact* of that merge: the dialogue
//! transcript the local run produced, parked against a target agent so people can read it, comment
//! on it, and record what became of it. An MR here is a review object, not an engine — closer to a
//! pull request's discussion tab than to `git merge`.
//!
//! Both endpoints carry an aid **and** a name. The name is what the URL routes on and what a human
//! reads; the aid is snapshotted at open time so that a rename later cannot quietly turn an MR into
//! a record of a merge between two agents that were never involved. When they disagree afterwards,
//! the aid is right.

use serde::{Deserialize, Serialize};

/// Bounds. An MR is a small review object: everything here is user text arriving over HTTP, and
/// nothing about a title or a comment needs to be unbounded.
pub const TITLE_MAX: usize = 200;
pub const COMMENT_MAX: usize = 8 * 1024;
/// The dialogue transcript is the one genuinely large field (a whole merge conversation), so it gets
/// room — but still a ceiling, since it lands in mrs.json and is served back whole.
pub const TRANSCRIPT_MAX: usize = 512 * 1024;
/// How many MRs one target agent may have open at once. Without this, "open an MR" is an
/// authenticated write amplifier against mrs.json.
pub const OPEN_MAX: usize = 200;
/// How many comments one MR may carry. `COMMENT_MAX` bounds one comment and this bounds how many:
/// without both, a thread is an unbounded append target, and `Store::update_mrs` re-serializes the
/// whole of mrs.json per comment — so the cost of the thread is quadratic, not linear.
pub const COMMENTS_MAX: usize = 500;

/// One end of a merge request: which memory, at which ref.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Endpoint {
    /// The agent's identity, snapshotted when the MR was opened. None = that agent had not committed
    /// an agent.toml yet (an empty store) — honest null beats a made-up id.
    #[serde(default)]
    pub aid: Option<String>,
    /// The owner **namespace segment** (`owner_ns`: user `alice` → `alice`, org `org:acme` → `acme`)
    /// the agent lives under. Part of the endpoint's identity now that a name is unique only within an
    /// owner — `daru/frontend` and `kaisen/frontend` are different memories. `#[serde(default)]` so a
    /// pre-scoping record deserializes (empty), then the v2 migration backfills it.
    #[serde(default)]
    pub owner: String,
    /// The agent's name **at open time**. Renames keep this current (see `Store::update_mrs`), so it
    /// stays a usable link rather than a fossil.
    pub agent: String,
    /// The git ref under discussion.
    #[serde(default = "default_ref")]
    pub git_ref: String,
}

fn default_ref() -> String {
    "main".into()
}

/// An MR's life. `Merged` is recorded, never performed: someone ran `agit a merge` locally and
/// pushed the result, then said so here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Merged,
    Closed,
}

impl State {
    pub fn parse(s: &str) -> Option<State> {
        match s {
            "open" => Some(State::Open),
            "merged" => Some(State::Merged),
            "closed" => Some(State::Closed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Merged => "merged",
            State::Closed => "closed",
        }
    }

    pub fn is_open(self) -> bool {
        self == State::Open
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: usize,
    pub author: String,
    pub body: String,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mr {
    /// Unique **per target agent**, not per Hub: `/api/agent/payments/mrs/1` reads the way a person
    /// expects, and the target's ACL already scopes every route that can reach it.
    pub id: usize,
    pub source: Endpoint,
    pub target: Endpoint,
    pub title: String,
    pub author: String,
    /// "open" | "merged" | "closed". Stored as a string so an unknown value from a hand-edited
    /// mrs.json degrades to "not open" (see `state()`) instead of failing the whole record.
    pub state: String,
    pub created: String,
    #[serde(default)]
    pub updated: String,
    /// What `agit a merge` produced locally. The reason an MR is worth reviewing at all; None = the
    /// MR was opened before the dialogue was run.
    #[serde(default)]
    pub dialogue_transcript: Option<String>,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

impl Mr {
    /// An unparseable state is **not** open: a hand-mangled mrs.json must not resurrect a closed MR
    /// into something that still accepts writes.
    pub fn state(&self) -> Option<State> {
        State::parse(&self.state)
    }

    pub fn is_open(&self) -> bool {
        self.state().map(State::is_open).unwrap_or(false)
    }

    pub fn next_comment_id(&self) -> usize {
        self.comments.iter().map(|c| c.id).max().unwrap_or(0) + 1
    }

    /// Whether the thread has reached `COMMENTS_MAX`. Separate from `is_open`: a full thread and a
    /// settled one are different refusals, and the caller of a full one may still open a new MR.
    pub fn comments_full(&self) -> bool {
        self.comments.len() >= COMMENTS_MAX
    }
}

/// The next free id for one target agent `(seg, name)`. Max + 1, never `len() + 1`: ids must not be
/// reused after a deletion, or a stale link starts pointing at somebody else's review.
pub fn next_id(mrs: &[Mr], seg: &str, name: &str) -> usize {
    mrs.iter().filter(|m| m.target.owner == seg && m.target.agent == name).map(|m| m.id).max().unwrap_or(0) + 1
}

/// Trim and bound one piece of user text. `None` = empty after trimming (absent), `Err` = too long.
/// Truncating silently would corrupt a transcript and call it success.
pub fn bounded(s: &str, max: usize) -> Result<Option<String>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.len() > max {
        return Err(format!("too long: {} bytes, the limit is {max}", t.len()));
    }
    Ok(Some(t.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: usize, target: &str, state: &str) -> Mr {
        Mr {
            id,
            source: Endpoint { aid: Some("agt_src".into()), owner: "alice".into(), agent: "src".into(), git_ref: "main".into() },
            target: Endpoint { aid: Some("agt_dst".into()), owner: "alice".into(), agent: target.into(), git_ref: "main".into() },
            title: "t".into(),
            author: "alice".into(),
            state: state.into(),
            created: "2026-07-16T00:00:00Z".into(),
            updated: String::new(),
            dialogue_transcript: None,
            comments: vec![],
        }
    }

    #[test]
    fn state_roundtrips_and_rejects_junk() {
        for s in [State::Open, State::Merged, State::Closed] {
            assert_eq!(State::parse(s.as_str()), Some(s));
        }
        assert_eq!(State::parse("Open"), None);
        assert_eq!(State::parse("reopened"), None);
    }

    #[test]
    fn an_unreadable_state_is_not_open() {
        // Fail-safe: a hand-mangled mrs.json must not turn a closed MR back into a writable one.
        assert!(mk(1, "pay", "open").is_open());
        assert!(!mk(1, "pay", "closed").is_open());
        assert!(!mk(1, "pay", "merged").is_open());
        assert!(!mk(1, "pay", "OPEN").is_open());
        assert!(!mk(1, "pay", "").is_open());
    }

    #[test]
    fn ids_are_per_target_agent() {
        // Two agents each get their own 1, 2, 3 — the id only has to be unique where it is routed.
        let mrs = vec![mk(1, "pay", "open"), mk(2, "pay", "closed"), mk(1, "other", "open")];
        assert_eq!(next_id(&mrs, "alice", "pay"), 3);
        assert_eq!(next_id(&mrs, "alice", "other"), 2);
        assert_eq!(next_id(&mrs, "alice", "fresh"), 1);
        assert_eq!(next_id(&[], "alice", "pay"), 1);
    }

    #[test]
    fn ids_are_scoped_to_the_owner_too() {
        // Same name under two owners is two different memories, so their MR ids are independent.
        let mut daru = mk(5, "frontend", "open");
        daru.target.owner = "daru".into();
        let mut kaisen = mk(2, "frontend", "open");
        kaisen.target.owner = "kaisen".into();
        let mrs = vec![daru, kaisen];
        assert_eq!(next_id(&mrs, "daru", "frontend"), 6);
        assert_eq!(next_id(&mrs, "kaisen", "frontend"), 3);
    }

    #[test]
    fn ids_are_never_reused_after_a_deletion() {
        // max+1, not len+1: recycling #2 would silently redirect every link to the old #2.
        let mrs = vec![mk(1, "pay", "open"), mk(3, "pay", "closed")];
        assert_eq!(next_id(&mrs, "alice", "pay"), 4);
    }

    #[test]
    fn comment_ids_climb_within_one_mr() {
        let mut m = mk(1, "pay", "open");
        assert_eq!(m.next_comment_id(), 1);
        m.comments.push(Comment { id: 1, author: "a".into(), body: "b".into(), created: String::new() });
        m.comments.push(Comment { id: 2, author: "a".into(), body: "b".into(), created: String::new() });
        assert_eq!(m.next_comment_id(), 3);
    }

    #[test]
    fn a_comment_thread_has_a_ceiling() {
        // COMMENT_MAX bounds one comment; nothing used to bound how many, so a thread was an
        // unbounded disk-fill against mrs.json — one whole-file rewrite per 8KiB comment.
        let mut m = mk(1, "pay", "open");
        assert!(!m.comments_full());
        for id in 1..COMMENTS_MAX {
            m.comments.push(Comment { id, author: "a".into(), body: "b".into(), created: String::new() });
        }
        assert!(!m.comments_full(), "the last free slot must still take a comment");
        m.comments.push(Comment { id: COMMENTS_MAX, author: "a".into(), body: "b".into(), created: String::new() });
        assert!(m.comments_full());
    }

    #[test]
    fn a_full_thread_and_a_settled_one_are_different_refusals() {
        let mut m = mk(1, "pay", "closed");
        assert!(!m.is_open() && !m.comments_full());
        m = mk(1, "pay", "open");
        m.comments = (1..=COMMENTS_MAX)
            .map(|id| Comment { id, author: "a".into(), body: "b".into(), created: String::new() })
            .collect();
        assert!(m.is_open() && m.comments_full());
    }

    #[test]
    fn bounded_trims_reports_absence_and_refuses_to_truncate() {
        assert_eq!(bounded("  hi  ", 10).unwrap().as_deref(), Some("hi"));
        assert_eq!(bounded("   ", 10).unwrap(), None);
        assert_eq!(bounded("", 10).unwrap(), None);
        // Over the limit is an error, never a silent trim — half a transcript reported as success is
        // a corrupted review.
        let e = bounded("abcdefghijk", 10).unwrap_err();
        assert!(e.contains("11") && e.contains("10"), "{e}");
    }

    #[test]
    fn a_ref_defaults_to_main_when_the_json_omits_it() {
        let e: Endpoint = serde_json::from_str(r#"{"agent":"pay"}"#).unwrap();
        assert_eq!(e.git_ref, "main");
        assert_eq!(e.aid, None, "no aid is null, not a fabricated one");
    }
}
