//! Session rendering: timeline and transcript.
//!
//! Two views:
//! - **Timeline** (`agit log`): one column of sessions, one or two lines each. For skimming.
//! - **Transcript** (`agit show`): the conversation inside one session. For reading.
//!
//! Both are line-oriented output a pipe can consume. Full-screen interactive browsing is a
//! different thing, in the `--tui` branch of `commands/show.rs`.

use super::theme;
use crate::adapter::{EventKind, Session};

/// One entry on the timeline.
pub struct Entry {
    pub id: String,
    pub runtime: String,
    pub when: String,
    /// Which code repo the session ran in (the docs require `agit log` to show this).
    pub repo: Option<String>,
    /// The opening prompt — how the reader recognizes "this is the one about the payment
    /// module".
    pub gist: Option<String>,
    pub activity: String,
    pub latest: bool,
    /// How many older revisions this session kept (one per non-append write detected).
    pub revisions: usize,
}

pub fn render_timeline(entries: &[Entry]) -> String {
    let s = theme::symbols();
    let mut out = String::new();

    for (i, e) in entries.iter().enumerate() {
        let node = if e.latest {
            super::accent(s.node)
        } else {
            super::dim(s.node)
        };
        // The id is truncated to 8 characters: enough to tell entries apart without filling
        // the line.
        let short: String = e.id.chars().take(8).collect();
        out.push_str(&format!(
            "{node} {}  {}  {}\n",
            super::bold(&short),
            super::accent(&e.runtime),
            super::dim(&e.when),
        ));

        // The vertical bar threads the detail lines into one column.
        let bar = if i + 1 < entries.len() {
            super::dim(s.vline)
        } else {
            " ".to_string()
        };

        if let Some(g) = &e.gist {
            out.push_str(&format!("{bar}   \"{}\"\n", super::truncate(g, 66)));
        }
        if let Some(r) = &e.repo {
            out.push_str(&format!("{bar}   {}\n", super::dim(r)));
        }
        if !e.activity.is_empty() {
            out.push_str(&format!("{bar}   {}\n", super::dim(&e.activity)));
        }
        // A revision means a non-append write was detected and the older content was kept: an
        // anomaly signal, worth showing as the evidence that the "lossless" promise works.
        if e.revisions > 0 {
            out.push_str(&format!(
                "{bar}   {}\n",
                super::warn_text(&format!(
                    "{} kept older revisions (non-append write detected)",
                    e.revisions
                ))
            ));
        }
        if i + 1 < entries.len() {
            out.push_str(&format!("{bar}\n"));
        }
    }
    out
}

/// Activity summary. Zero-valued items are omitted — "0 edits" is noise.
pub fn activity_summary(s: &Session) -> String {
    let c = s.counts();
    let mut parts = vec![];
    if c.prompts > 0 {
        parts.push(format!("{} prompts", c.prompts));
    }
    if c.replies > 0 {
        parts.push(format!("{} replies", c.replies));
    }
    if c.tools > 0 {
        parts.push(format!("{} tool calls", c.tools));
    }
    if c.edits > 0 {
        parts.push(format!("{} edits", c.edits));
    }
    parts.join(" · ")
}

/// Render the full transcript for reading.
///
/// `max_chars` caps each message — a full transcript can run to megabytes, and dumping that
/// straight to the terminal is unreadable.
pub fn render_transcript(s: &Session, max_chars: usize) -> String {
    let sym = theme::symbols();
    let mut out = String::new();

    for e in &s.events {
        let (label, body) = match e.kind {
            EventKind::UserPrompt => (super::accent("you"), e.text.as_deref().unwrap_or("")),
            // An interjection is still speech inside this turn; the label only marks that it
            // arrived mid-turn.
            EventKind::UserInterjection => (
                super::accent("you (mid-turn)"),
                e.text.as_deref().unwrap_or(""),
            ),
            EventKind::AssistantReply => (super::bold("agent"), e.text.as_deref().unwrap_or("")),
            EventKind::ToolUse => {
                out.push_str(&format!(
                    "  {} {}\n",
                    super::dim(sym.arrow),
                    super::dim(&format!("call: {}", e.tool.as_deref().unwrap_or("tool")))
                ));
                continue;
            }
            EventKind::FileEdit => {
                let files = if e.paths.is_empty() {
                    e.text.clone().unwrap_or_default()
                } else {
                    e.paths.join(", ")
                };
                out.push_str(&format!(
                    "  {} {}\n",
                    super::dim(sym.arrow),
                    super::dim(&format!("edit {}", super::truncate(&files, 68)))
                ));
                continue;
            }
            // Tool output collapses to one line, like `ToolUse`.
            //
            // The body is not dumped: in the observed corpus these are whole config dumps and
            // hundreds of lines of training logs, and pouring them into the conversation flow
            // washes out what the people said. Dropping it is no better — the reader then sees
            // a call followed straight by the agent's conclusion, with no sign that anything
            // came back in between.
            //
            // First line + character count: the first line is usually the most informative one
            // (`Exit code 255` and the like), and the count tells the reader whether to go back
            // to the source for the rest.
            EventKind::ToolResult => {
                let body = e.text.as_deref().unwrap_or("");
                let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                let chars = body.chars().count();
                out.push_str(&format!(
                    "  {} {}\n",
                    super::dim(sym.arrow),
                    super::dim(&format!(
                        "output ({chars} chars) {}",
                        super::truncate(first, 56)
                    ))
                ));
                continue;
            }
            // A compact boundary collapses to one separator line; the summary body never
            // enters the conversation flow.
            //
            // Why: a Claude Code summary is tens of thousands of characters of English
            // boilerplate, a disaster to read mixed into the dialogue; a Codex
            // `replacement_history` is a copy of user input that already appeared above, and
            // rendering it twice is noise. Both only need to be marked "compacted here".
            EventKind::CompactFiltered | EventKind::CompactSummary => {
                let note = if e.kind == EventKind::CompactFiltered {
                    // Filtered: every original message is still there; only the context
                    // window changed.
                    "context compaction (user input preserved verbatim)"
                } else {
                    // Summarized: everything before this point was folded into a summary, and
                    // that is lossy.
                    "context compaction (earlier content summarized; see full transcript)"
                };
                out.push_str(&format!("\n  {} {}\n", super::dim("───"), super::dim(note)));
                continue;
            }
            // What the intermediate representation (IR) does not model (encrypted reasoning
            // and the like) is not rendered. Nor is a turn-end marker — that is a runtime
            // signal, not conversation content.
            EventKind::TurnEnd | EventKind::Other => continue,
        };
        if body.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("\n{label}\n"));
        for line in super::truncate(body, max_chars).lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Event;

    fn sess(events: Vec<Event>) -> Session {
        Session {
            id: "s".into(),
            runtime: "codex".into(),
            cwd: None,
            events,
        }
    }

    #[test]
    fn activity_omits_zeros() {
        let s = sess(vec![
            Event::text(EventKind::UserPrompt, "q", None),
            Event::text(EventKind::UserPrompt, "q2", None),
        ]);
        let sum = activity_summary(&s);
        assert!(sum.contains("2 prompts"));
        assert!(!sum.contains("edits"), "zero counts must not appear: {sum}");
        assert_eq!(activity_summary(&sess(vec![])), "");
    }

    #[test]
    fn transcript_skips_other_events() {
        let s = sess(vec![
            Event::text(EventKind::UserPrompt, "question", None),
            Event {
                kind: EventKind::Other,
                text: Some("sealed reasoning".into()),
                timestamp: None,
                paths: vec![],
                tool: None,
                line: None,
            },
        ]);
        let t = render_transcript(&s, 100);
        assert!(t.contains("question"));
        assert!(
            !t.contains("sealed reasoning"),
            "Other must not render: {t}"
        );
    }

    #[test]
    fn timeline_shows_rescued_revisions() {
        // The revision count is an anomaly signal and must be visible.
        let out = render_timeline(&[Entry {
            id: "abcdef123456".into(),
            runtime: "claude-code".into(),
            when: "3m ago".into(),
            repo: Some("/repo".into()),
            gist: Some("refactor payment module".into()),
            activity: "5 prompts".into(),
            latest: true,
            revisions: 2,
        }]);
        assert!(
            out.contains("abcdef12"),
            "the id is truncated to 8 characters"
        );
        assert!(out.contains("refactor payment module"));
        assert!(
            out.contains("2 kept older revisions"),
            "revisions must be visible: {out}"
        );
    }

    #[test]
    fn timeline_handles_empty_fields() {
        let out = render_timeline(&[Entry {
            id: "x".into(),
            runtime: "codex".into(),
            when: "just now".into(),
            repo: None,
            gist: None,
            activity: String::new(),
            latest: true,
            revisions: 0,
        }]);
        assert!(out.contains('x'));
        assert!(!out.contains("revision"), "0 revisions stay hidden");
    }
}
