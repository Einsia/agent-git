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
            if !compatible(content, &comparisons[index].content, &value) {
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
