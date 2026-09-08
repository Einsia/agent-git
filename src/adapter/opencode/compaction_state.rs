//! Native summary consumption follows occurrence-bound markers at the observed input cut.

use std::collections::{BTreeSet, HashMap};

struct Header {
    summary: bool,
    markers: usize,
}

#[derive(Default)]
struct Definition {
    first: Option<usize>,
    last: Option<usize>,
    forward_markers: usize,
}

#[derive(Default)]
pub(crate) struct CompactionState {
    headers: Vec<Header>,
    definitions: HashMap<String, Definition>,
    breaks: BTreeSet<usize>,
}

impl CompactionState {
    fn update_edge(&mut self, index: usize) {
        if index >= self.headers.len() {
            return;
        }
        if index == 0 || !self.headers[index].summary || self.headers[index - 1].markers == 0 {
            self.breaks.insert(index);
        } else {
            self.breaks.remove(&index);
        }
    }

    pub(crate) fn message(&mut self, id: &str, summary: bool) {
        let index = self.headers.len();
        let definition = self.definitions.entry(id.to_owned()).or_default();
        let mut changed = None;
        let markers = match definition.first {
            None => {
                definition.first = Some(index);
                definition.forward_markers
            }
            Some(first) => {
                if definition.forward_markers > 0 {
                    self.headers[first].markers -= definition.forward_markers;
                    definition.forward_markers = 0;
                    changed = Some(first + 1);
                }
                0
            }
        };
        definition.last = Some(index);
        self.headers.push(Header { summary, markers });
        self.update_edge(index);
        if let Some(changed) = changed {
            self.update_edge(changed);
        }
    }

    pub(crate) fn marker(&mut self, id: &str) {
        let definition = self.definitions.entry(id.to_owned()).or_default();
        match definition.last {
            Some(index) => {
                self.headers[index].markers += 1;
                self.update_edge(index + 1);
            }
            None => definition.forward_markers += 1,
        }
    }

    pub(crate) fn consumed(&self, index: usize) -> bool {
        let start = self
            .breaks
            .range(..=index)
            .next_back()
            .copied()
            .unwrap_or(0);
        (index - start) % 2 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_consumption(records: &[serde_json::Value]) -> (Vec<bool>, usize) {
        let headers: Vec<_> = records
            .iter()
            .enumerate()
            .filter(|(_, record)| record["kind"] == "message")
            .collect();
        let resolve = |position, id: &str| {
            let matching: Vec<_> = headers
                .iter()
                .enumerate()
                .filter(|(_, (_, header))| header["id"] == id)
                .collect();
            matching
                .iter()
                .rev()
                .find(|(_, (line, _))| *line <= position)
                .map(|(index, _)| *index)
                .or_else(|| (matching.len() == 1).then(|| matching[0].0))
        };
        let mut markers = vec![false; headers.len()];
        for (position, record) in records.iter().enumerate() {
            if record["kind"] == "part"
                && record["data"]["type"] == "compaction"
                && let Some(host) = resolve(position, record["message_id"].as_str().unwrap())
            {
                markers[host] = true;
            }
        }
        let mut consumed = vec![false; headers.len()];
        for index in 0..headers.len() {
            if !consumed[index]
                && markers[index]
                && index + 1 < headers.len()
                && headers[index + 1].1["data"]["mode"] == "compaction"
            {
                consumed[index + 1] = true;
            }
        }
        let tools = records
            .iter()
            .enumerate()
            .filter(|(position, record)| {
                record["kind"] == "part"
                    && record["data"]["type"] == "tool"
                    && resolve(*position, record["message_id"].as_str().unwrap())
                        .is_some_and(|host| !consumed[host])
            })
            .count();
        (consumed, tools)
    }

    #[test]
    fn late_markers_and_ambiguous_forward_definitions_update_consumption() {
        let mut state = CompactionState::default();
        state.marker("boundary");
        state.message("boundary", false);
        state.message("summary", true);
        state.marker("summary");
        state.message("tail", true);
        assert!(state.consumed(1));
        assert!(!state.consumed(2));
        state.message("boundary", false);
        assert!(!state.consumed(1));
        assert!(state.consumed(2));
        state.marker("tail");
        state.message("last", true);
        assert!(!state.consumed(4));
    }

    #[test]
    fn incremental_state_matches_full_native_occurrence_projection() {
        use crate::adapter::{Adapter, EventKind};
        let mut state = CompactionState::default();
        let mut records = Vec::new();
        let mut seed = 7u64;
        for step in 0..96 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = format!("host-{}", (seed >> 32) % 7);
            if step % 3 == 0 {
                state.marker(&id);
                records.push(serde_json::json!({"kind":"part", "message_id":id, "data":{"type":"compaction"}}));
            } else {
                let summary = step % 4 != 0;
                state.message(&id, summary);
                records.push(serde_json::json!({"kind":"message", "id":id, "data":{"role":"assistant", "mode":if summary {"compaction"} else {"build"}}}));
                records.push(serde_json::json!({"kind":"part", "message_id":id, "data":{"type":"tool", "tool":"bash"}}));
            }
            let text = records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>();
            let projected = super::super::OpenCode.parse(&text).unwrap();
            let actual = projected
                .events
                .iter()
                .filter(|event| event.kind == EventKind::ToolUse)
                .count();
            let (reference, expected) = reference_consumption(&records);
            assert_eq!(
                reference,
                (0..state.headers.len())
                    .map(|index| state.consumed(index))
                    .collect::<Vec<_>>(),
                "input cut {step}"
            );
            assert_eq!(actual, expected, "input cut {step}");
        }
    }
}
