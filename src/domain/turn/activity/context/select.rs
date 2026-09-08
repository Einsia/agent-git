//! Select native structural dependencies while preserving message adjacency and occurrence scope.

use super::Group;
use crate::{
    Result,
    domain::{storage, transcript::Envelope},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(in super::super) struct Selected {
    pub text: String,
    pub lines: usize,
    pub steps: usize,
}

struct Selection<'a> {
    group: &'a Group,
    end: usize,
    messages: usize,
    added_definitions: BTreeMap<&'a str, Vec<usize>>,
    added_markers: BTreeMap<&'a str, Vec<usize>>,
}

impl Selection<'_> {
    fn header(&self, index: usize) -> &super::Record {
        &self.group.records[self.group.messages[index]]
    }

    fn prior_definitions(&self, id: &str) -> &[usize] {
        let definitions = self
            .group
            .definitions
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let end = definitions.partition_point(|&index| index < self.messages);
        &definitions[..end]
    }

    fn marker(&self, index: usize) -> Option<Option<usize>> {
        let header = self.header(index);
        let id = header.header.as_ref().unwrap().0.as_ref();
        let definitions = self.prior_definitions(id);
        let added = self
            .added_definitions
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let next = definitions.partition_point(|&definition| definition <= index);
        let upper = definitions
            .get(next)
            .map(|&next| self.header(next).position)
            .or_else(|| added.first().copied())
            .unwrap_or(usize::MAX);
        let lower = if definitions.len() == 1 && added.is_empty() {
            0
        } else {
            header.position
        };
        if let Some(markers) = self.group.compactions.get(id) {
            let first =
                markers.partition_point(|&record| self.group.records[record].position < lower);
            if let Some(&record) = markers.get(first)
                && self.group.records[record].position < upper.min(self.end)
            {
                return Some(Some(record));
            }
        }
        if let Some(markers) = self.added_markers.get(id) {
            let first = markers.partition_point(|&position| position < lower);
            if markers.get(first).is_some_and(|&position| position < upper) {
                return Some(None);
            }
        }
        None
    }
}

pub(super) fn select<'a>(
    group: &'a Group,
    end: usize,
    selected: &[(usize, &'a Envelope)],
) -> Result<Selected> {
    let mut state = Selection {
        group,
        end,
        messages: group
            .messages
            .partition_point(|&record| group.records[record].position < end),
        added_definitions: BTreeMap::new(),
        added_markers: BTreeMap::new(),
    };
    let mut referenced = BTreeSet::new();
    for (position, envelope) in selected {
        let value = &envelope.content;
        match value["kind"].as_str() {
            Some("message") => {
                let id = value["id"].as_str().unwrap_or_default();
                state
                    .added_definitions
                    .entry(id)
                    .or_default()
                    .push(*position);
                referenced.insert(id);
            }
            Some("part") => {
                let id = value["message_id"].as_str().unwrap_or_default();
                referenced.insert(id);
                if value["data"]["type"] == "compaction" {
                    state.added_markers.entry(id).or_default().push(*position);
                }
            }
            _ => {}
        }
    }
    let mut seeds = BTreeSet::new();
    for id in &referenced {
        if let Some(&last) = state.prior_definitions(id).last() {
            seeds.insert(last);
        }
    }
    if !state.added_definitions.is_empty()
        && let Some(last) = state.messages.checked_sub(1)
    {
        seeds.insert(last);
    }

    let mut headers = BTreeSet::new();
    for index in seeds {
        headers.insert(index);
        if group.consumption.consumed(index) {
            headers.insert(index - 1);
        }
    }
    let mut records = BTreeSet::new();
    let mut barriers = BTreeSet::new();
    let mut previous = None;
    for index in headers {
        let record = group.messages[index];
        if previous.is_some_and(|previous| previous + 1 != index) {
            barriers.insert(group.records[record].position);
        }
        previous = Some(index);
        records.insert(record);
        if let Some(Some(marker)) = state.marker(index) {
            records.insert(marker);
        }
    }
    // An earlier orphan marker can bind to a unique message introduced by the selected cut.
    // Repeated definitions stay ambiguous because every selected definition remains in input.
    for (id, definitions) in &state.added_definitions {
        if state.prior_definitions(id).is_empty()
            && definitions.len() == 1
            && let Some(&record) = group
                .compactions
                .get(*id)
                .and_then(|markers| markers.first())
            && group.records[record].position < end
        {
            records.insert(record);
        }
    }
    let mut gap = 0usize;
    let gap_id = loop {
        let candidate = format!("agit-log-context-gap-{gap}");
        if !group.definitions.contains_key(candidate.as_str())
            && !referenced.contains(candidate.as_str())
        {
            break candidate;
        }
        gap = gap
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native context identity overflow"))?;
    };
    let barrier = format!(
        "{}\n",
        serde_json::json!({"kind":"message", "id":gap_id, "data":{"role":""}})
    );
    let mut output = Selected::default();
    let append = |output: &mut Selected, raw: &str| -> Result<()> {
        anyhow::ensure!(
            output
                .text
                .len()
                .checked_add(raw.len())
                .is_some_and(|size| size <= storage::MAX_MATERIALIZED_BYTES),
            "native structural context exceeds the read limit"
        );
        output.text.push_str(raw);
        output.lines += 1;
        Ok(())
    };
    for record in records {
        let record = &group.records[record];
        if barriers.remove(&record.position) {
            append(&mut output, &barrier)?;
        }
        append(&mut output, &record.raw)?;
    }
    Ok(output)
}
