//! Settled content remains immutable when a local recovery dictionary is unavailable.

use super::projector::tokens;
use serde_json::Value;

/// A legacy opaque span has no locally comparable original. All surrounding content and
/// structure must still agree; this never authorizes rewriting the settled representation.
fn compatible(original: &Value, stored: &Value, live: &Value) -> bool {
    if original == live {
        return true;
    }
    match (stored, live) {
        (Value::String(stored), Value::String(live)) => compatible_string(stored, live),
        (Value::Array(stored), Value::Array(live)) => {
            stored.len() == live.len()
                && stored
                    .iter()
                    .zip(live)
                    .enumerate()
                    .all(|(index, (a, b))| compatible(&original[index], a, b))
        }
        (Value::Object(stored), Value::Object(live)) => {
            stored.len() == live.len()
                && stored.iter().all(|(key, value)| {
                    live.get(key)
                        .is_some_and(|other| compatible(&original[key], value, other))
                })
        }
        _ => stored == live,
    }
}

/// Unknown opaque strings compare only their surrounding literals, never a guessed original.
pub(crate) fn compatible_string(stored: &str, live: &str) -> bool {
    if stored == live {
        return true;
    }
    let spans: Vec<_> = tokens(stored).collect();
    let Some((first, _, _)) = spans.first() else {
        return false;
    };
    if !live.starts_with(&stored[..*first]) {
        return false;
    }
    let mut cursor = *first;
    for (index, (_, end, _)) in spans.iter().enumerate() {
        let Some(character) = live[cursor..].chars().next() else {
            return false;
        };
        cursor += character.len_utf8();
        let until = spans.get(index + 1).map_or(stored.len(), |next| next.0);
        let literal = &stored[*end..until];
        if index + 1 == spans.len() {
            return live[cursor..].ends_with(literal);
        }
        let Some(at) = live[cursor..].find(literal) else {
            return false;
        };
        cursor += at + literal.len();
    }
    false
}

/// Codex leads each segment of a thread with its own header. A branch keeps the header it first
/// settled, so any header naming the same thread continues it; every later line still has to
/// agree. Without this, a branch whose first line came from a later segment can never settle
/// again once the capture leads with the thread's first header.
fn same_thread_header(committed: &Value, live: &Value) -> bool {
    let thread = |value: &Value| {
        (value["type"] == "session_meta")
            .then(|| value["payload"]["id"].as_str())
            .flatten()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    thread(committed).is_some_and(|id| Some(id) == thread(live))
}

/// Compare a native prefix while retaining its committed content at each physical line.
/// New privacy decisions apply only after the returned native byte boundary.
pub fn retain_prefix(
    committed: &str,
    comparable: &str,
    live: &str,
) -> crate::Result<Option<(usize, String)>> {
    let contents = committed
        .split_inclusive('\n')
        .map(crate::domain::storage::parse_envelope_line)
        .collect::<crate::Result<Vec<_>>>()?;
    let comparisons = comparable
        .split_inclusive('\n')
        .map(crate::domain::storage::parse_envelope_line)
        .collect::<crate::Result<Vec<_>>>()?;
    anyhow::ensure!(
        contents.len() == comparisons.len(),
        "privacy comparison changed the event count"
    );
    let mut projected = String::new();
    let mut cursor = 0;
    let mut index = 0;
    for line in live.split_inclusive('\n') {
        if index == contents.len() {
            break;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            let content = &contents[index].content;
            let continues = compatible(content, &comparisons[index].content, &value)
                || (index == 0 && same_thread_header(content, &value));
            if !continues {
                return Ok(None);
            }
            projected.push_str(&serde_json::to_string(content)?);
            if line.ends_with('\n') {
                projected.push('\n');
            }
            index += 1;
        } else {
            projected.push_str(line);
        }
        cursor += line.len();
    }
    Ok((index == contents.len()).then_some((cursor, projected)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the first line may differ, and only as another header of the same thread; the
    /// committed header is what settlement keeps. A comparison that accepts any differing header
    /// would let another conversation continue the branch.
    #[test]
    fn a_later_segment_header_of_the_same_thread_continues_the_settled_prefix() {
        let header = |id: &str, ordinal: u64| serde_json::json!({"type":"session_meta","ordinal":ordinal,"payload":{"id":id}});
        let record = serde_json::json!({"type":"event_msg","payload":{"type":"user_message"}});
        let committed = crate::domain::transcript::wrap_lines(
            &format!("{}\n{record}\n", header("thread", 9)),
            "codex",
            "agit-0123456789abcdef0123456789abcdef01234567",
        );
        let live = format!("{}\n{record}\n{record}\n", header("thread", 0));
        let (end, projected) = retain_prefix(&committed, &committed, &live)
            .unwrap()
            .unwrap();
        assert_eq!(end, live.len() - format!("{record}\n").len());
        assert!(projected.starts_with(&serde_json::to_string(&header("thread", 9)).unwrap()));
        let other = format!("{}\n{record}\n", header("other", 0));
        assert!(
            retain_prefix(&committed, &committed, &other)
                .unwrap()
                .is_none()
        );
        let changed = format!("{}\n{}\n", header("thread", 0), header("thread", 1));
        assert!(
            retain_prefix(&committed, &committed, &changed)
                .unwrap()
                .is_none()
        );
    }
}
