//! Bounded structural context shared by selected cuts of an append-only LOG segment.

use super::{ReadStats, charge_expansion, checked_text};
use crate::Result;
use crate::domain::{meta, repo::Repo, storage, transcript};
use anyhow::Context as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use transcript::display::{SourceKey, source_key};

mod evidence;
mod select;
pub(super) use select::Selected;

#[derive(Default)]
pub(super) struct Context {
    groups: BTreeMap<SourceKey, Group>,
    bytes: usize,
    evidence: evidence::Evidence,
}

struct Record {
    position: usize,
    raw: Arc<str>,
    header: Option<(Arc<str>, bool)>,
    compaction: Option<Arc<str>>,
}

#[derive(Default)]
struct Group {
    records: Vec<Record>,
    messages: Vec<usize>,
    definitions: BTreeMap<Arc<str>, Vec<usize>>,
    compactions: BTreeMap<Arc<str>, Vec<usize>>,
    processed: usize,
    consumption: crate::adapter::opencode::CompactionState,
}

impl Context {
    fn insert(
        &mut self,
        envelope: &transcript::Envelope,
        positions: &[usize],
        required: &BTreeSet<SourceKey>,
    ) -> Result<()> {
        let key = source_key(envelope);
        if !required.contains(&key) {
            return Ok(());
        }
        let Some(raw) = crate::adapter::opencode::activity_context_record(&envelope.content) else {
            return Ok(());
        };
        charge_expansion(
            &mut self.bytes,
            raw.len(),
            positions.len(),
            storage::MAX_MATERIALIZED_BYTES,
        )?;
        let raw: Arc<str> = raw.into();
        let (header, compaction) = if envelope.content["kind"] == "message" {
            (
                Some((
                    Arc::from(envelope.content["id"].as_str().unwrap_or_default()),
                    envelope.content["data"]["mode"] == "compaction",
                )),
                None,
            )
        } else {
            (
                None,
                Some(Arc::from(
                    envelope.content["message_id"].as_str().unwrap_or_default(),
                )),
            )
        };
        let group = self.groups.entry(key).or_default();
        group
            .records
            .extend(positions.iter().map(|&position| Record {
                position,
                raw: raw.clone(),
                header: header.clone(),
                compaction: compaction.clone(),
            }));
        Ok(())
    }

    pub(super) fn read(
        repo: &Repo,
        sha: &str,
        layout: meta::LayoutVersion,
        required: &BTreeSet<SourceKey>,
        stats: &mut ReadStats,
    ) -> Result<Self> {
        let mut context = Self::default();
        if required.is_empty() {
            return Ok(context);
        }
        let path = match layout {
            meta::LayoutVersion::V0 => meta::LEGACY_LOG_FILE,
            meta::LayoutVersion::V1 => meta::LOG_FILE,
        };
        let mut ids = Vec::new();
        repo.git_cat_file_batch(
            vec![format!("{sha}:{path}")],
            storage::MAX_MATERIALIZED_BYTES,
            |_, kind, body| {
                let text = checked_text(kind, body, &mut 0)?;
                match layout {
                    meta::LayoutVersion::V0 => {
                        stats.context_payload_bytes = stats
                            .context_payload_bytes
                            .checked_add(text.len())
                            .context("LOG context read size overflow")?;
                        for (position, line) in text.split_inclusive('\n').enumerate() {
                            anyhow::ensure!(
                                position < storage::MAX_SEQUENCE_EVENTS,
                                "legacy LOG exceeds the event limit"
                            );
                            let envelope = storage::parse_legacy_envelope_line(line)?;
                            context.insert(&envelope, &[position], required)?;
                        }
                    }
                    meta::LayoutVersion::V1 => ids = storage::parse_sequence(text)?,
                }
                Ok(())
            },
        )?;
        let mut occurrences: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (position, id) in ids.into_iter().enumerate() {
            occurrences.entry(id).or_default().push(position);
        }
        let destinations: Vec<_> = occurrences.into_iter().collect();
        let asks = destinations
            .iter()
            .map(|(id, _)| Ok(format!("{sha}:{}", meta::event_path(id)?)))
            .collect::<Result<_>>()?;
        let mut cursor = 0;
        let mut total = 0;
        let mut expanded = 0;
        repo.git_cat_file_batch(asks, storage::MAX_EVENT_BYTES, |oid, kind, body| {
            let (id, positions) = &destinations[cursor];
            cursor += 1;
            let text = checked_text(kind, body, &mut total)?;
            charge_expansion(
                &mut expanded,
                text.len(),
                positions.len(),
                storage::MAX_MATERIALIZED_BYTES,
            )?;
            stats.context_payload_bytes = stats
                .context_payload_bytes
                .checked_add(text.len())
                .context("LOG context read size overflow")?;
            let envelope = storage::parse_envelope_line(text)?;
            anyhow::ensure!(
                storage::event_id(text)? == *id,
                "event {id} has invalid content address"
            );
            context.evidence.insert(positions[0], id, oid, text.len());
            context.insert(&envelope, positions, required)
        })?;
        context.evidence.sort();
        for group in context.groups.values_mut() {
            group.records.sort_by_key(|record| record.position);
            for (index, record) in group.records.iter().enumerate() {
                if let Some((id, _)) = &record.header {
                    group
                        .definitions
                        .entry(id.clone())
                        .or_default()
                        .push(group.messages.len());
                    group.messages.push(index);
                }
                if let Some(id) = &record.compaction {
                    group.compactions.entry(id.clone()).or_default().push(index);
                }
            }
        }
        Ok(context)
    }

    pub(super) fn validate_at(
        &mut self,
        repo: &Repo,
        sha: &str,
        layout: meta::LayoutVersion,
        end: usize,
        stats: &mut ReadStats,
    ) -> Result<()> {
        if layout == meta::LayoutVersion::V0 {
            return Ok(());
        }
        self.evidence.validate_at(repo, sha, end, stats)
    }

    pub(super) fn select(
        &mut self,
        key: &SourceKey,
        end: usize,
        selected: &[(usize, &transcript::Envelope)],
    ) -> Result<Selected> {
        match self.groups.get_mut(key) {
            Some(group) => {
                let before = group.processed;
                let cut = selected.last().map_or(end, |(position, _)| position + 1);
                while let Some(record) = group.records.get(group.processed)
                    && record.position < cut
                {
                    if let Some((id, summary)) = &record.header {
                        group.consumption.message(id, *summary);
                    }
                    if let Some(id) = &record.compaction {
                        group.consumption.marker(id);
                    }
                    group.processed += 1;
                }
                let mut selected = select::select(group, end, selected)?;
                selected.steps = group.processed - before;
                Ok(selected)
            }
            None => Ok(Selected::default()),
        }
    }
}
