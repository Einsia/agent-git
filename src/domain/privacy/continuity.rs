//! Settled content remains immutable when a local recovery dictionary is unavailable.

use super::projector::tokens;
use serde_json::Value;

/// A legacy opaque span has no locally comparable original. All surrounding content and
/// structure must still agree; this never authorizes rewriting the settled representation.
/// `original` is the settled value at the same place, if it has one there: under a name that was
/// restored, in full or in part, it has none, and a missing value must never read as `null`.
fn compatible(original: Option<&Value>, stored: &Value, live: &Value) -> bool {
    if original == Some(live) {
        return true;
    }
    match (stored, live) {
        (Value::String(stored), Value::String(live)) => compatible_string(stored, live),
        (Value::Array(stored), Value::Array(live)) => {
            stored.len() == live.len()
                && stored.iter().zip(live).enumerate().all(|(index, (a, b))| {
                    compatible(original.and_then(|original| original.get(index)), a, b)
                })
        }
        (Value::Object(stored), Value::Object(live)) => {
            stored.len() == live.len() && members_compatible(original, stored, live)
        }
        _ => stored == live,
    }
}

/// Payload member names are protected like values, so a settled name can be a placeholder too.
/// Each such name stands for one transcript name the settled record doesn't have, whose value
/// must still agree; every other name must be there as it is. Without this, a record whose name
/// can't be restored here never continues again.
fn members_compatible(
    original: Option<&Value>,
    stored: &serde_json::Map<String, Value>,
    live: &serde_json::Map<String, Value>,
) -> bool {
    let settled = |key: &String| original.and_then(|original| original.get(key));
    let mut opaque = Vec::new();
    for (key, value) in stored {
        match live.get(key) {
            Some(other) => {
                if !compatible(settled(key), value, other) {
                    return false;
                }
            }
            None if tokens(key).next().is_some() => opaque.push((key, value)),
            None => return false,
        }
    }
    let spare: Vec<_> = live
        .iter()
        .filter(|(name, _)| !stored.contains_key(*name))
        .collect();
    let candidates: Vec<Vec<usize>> = opaque
        .iter()
        .map(|&(key, value)| {
            (0..spare.len())
                .filter(|&index| {
                    let (name, other) = spare[index];
                    compatible_string(key, name) && compatible(settled(key), value, other)
                })
                .collect()
        })
        .collect();
    let mut holder = vec![None; spare.len()];
    (0..opaque.len())
        .all(|key| assign(key, &candidates, &mut holder, &mut vec![false; spare.len()]))
}

/// Gives opaque name `key` a transcript name of its own, moving an earlier one to another of its
/// candidates when that frees one: two placeholders can fit the same names in either order.
fn assign(
    key: usize,
    candidates: &[Vec<usize>],
    holder: &mut [Option<usize>],
    seen: &mut [bool],
) -> bool {
    for &name in &candidates[key] {
        if seen[name] {
            continue;
        }
        seen[name] = true;
        let current = holder[name];
        if current.is_none_or(|other| assign(other, candidates, holder, seen)) {
            holder[name] = Some(key);
            return true;
        }
    }
    false
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
            let continues = compatible(Some(content), &comparisons[index].content, &value)
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

    /// Under a name restored for the comparison, in part or in full, the saved record holds no
    /// value, and that is not a saved `null`: the value is compared with the restored record's.
    #[test]
    fn a_restored_name_never_lets_its_value_become_null() {
        let token = || {
            crate::domain::privacy::detector::placeholder::typed_token("secret", "high-entropy")
                .unwrap()
        };
        let session = "agit-0123456789abcdef0123456789abcdef01234567";
        let saved = |name: &str| {
            let record =
                serde_json::json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{name:4}}});
            crate::domain::transcript::wrap_lines(&format!("{record}\n"), "claude-code", session)
        };
        let live = |value: Value| {
            let record = serde_json::json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{"left/right":value}}});
            format!("{record}\n")
        };
        let (a, b) = (token(), token());
        for (committed, comparable) in [
            (saved(&format!("{a}/{b}")), saved(&format!("left/{b}"))),
            (saved(&a), saved("left/right")),
        ] {
            assert!(
                retain_prefix(&committed, &comparable, &live(4.into()))
                    .unwrap()
                    .is_some()
            );
            for changed in [Value::Null, 5.into()] {
                assert!(
                    retain_prefix(&committed, &comparable, &live(changed.clone()))
                        .unwrap()
                        .is_none(),
                    "{changed}"
                );
            }
        }
    }

    /// A name replaced when it was saved, such as a usage record's `cacheCreationInputTokens`
    /// under the entropy opt-in, continues where its original can't be restored: it stands for
    /// one name the transcript has and the saved record doesn't, and that name's value must still
    /// agree. The saved line is what settlement keeps.
    #[test]
    fn a_placeholder_member_name_continues_without_its_original() {
        let token = || {
            crate::domain::privacy::detector::placeholder::typed_token("secret", "high-entropy")
                .unwrap()
        };
        let session = "agit-0123456789abcdef0123456789abcdef01234567";
        let saved = |record: &Value| {
            crate::domain::transcript::wrap_lines(&format!("{record}\n"), "claude-code", session)
        };
        let cost = |name: &str, value: u64| serde_json::json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{"inputTokens":1,name:value}}});
        let name = token();
        let committed = saved(&cost(&name, 4));
        let record = cost("cacheCreationInputTokens", 4);
        let next = serde_json::json!({"type":"user","message":{"role":"user","content":"next"}});
        let live = format!("{record}\n{next}\n");
        let (end, projected) = retain_prefix(&committed, &committed, &live)
            .unwrap()
            .unwrap();
        assert_eq!(end, format!("{record}\n").len());
        assert!(projected.contains(&name));
        for changed in [
            cost("cacheCreationInputTokens", 5),
            serde_json::json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{"inputTokens":1,"cacheCreationInputTokens":4,"outputTokens":2}}}),
            serde_json::json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{"outputTokens":1,"cacheCreationInputTokens":4}}}),
        ] {
            let live = format!("{changed}\n{next}\n");
            assert!(
                retain_prefix(&committed, &committed, &live)
                    .unwrap()
                    .is_none(),
                "{changed}"
            );
        }
        // Two placeholder names fit the transcript's names as a whole, not in the order met.
        let (first, second) = {
            let (a, b) = (token(), token());
            if a < b { (a, b) } else { (b, a) }
        };
        let committed = saved(
            &serde_json::json!({"type":"progress","data":{first.as_str():token(),second.as_str():"x"}}),
        );
        let fits = format!(
            "{}\n",
            serde_json::json!({"type":"progress","data":{"a":"x","b":"y"}})
        );
        assert!(
            retain_prefix(&committed, &committed, &fits)
                .unwrap()
                .is_some()
        );
        let misses = format!(
            "{}\n",
            serde_json::json!({"type":"progress","data":{"a":"y","b":"z"}})
        );
        assert!(
            retain_prefix(&committed, &committed, &misses)
                .unwrap()
                .is_none()
        );
    }
}
