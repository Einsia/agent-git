//! Activity in the immutable LOG additions of selected turn commits.

use crate::Result;
use crate::domain::meta::{self, Kind, LayoutVersion};
use crate::domain::refs::Chain;
use crate::domain::repo::{ObjectBody, Repo};
use crate::domain::storage;
use crate::domain::transcript::Envelope;
use crate::domain::transcript::display::{SourceKey, source_key};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

mod context;

#[derive(Default)]
struct ReadStats {
    context_payload_bytes: usize,
    native_context_bytes: usize,
    context_structural_steps: usize,
    context_path_checks: usize,
    context_changed_paths: usize,
    context_changed_path_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    pub events: usize,
    pub tools: usize,
}

struct Sequence {
    len: usize,
    bytes: usize,
    digest: [u8; 32],
}

enum Addition {
    Stored(Vec<String>),
    Legacy(Vec<Envelope>),
}

/// Read selected turn additions using their first-parent LOG as the boundary. Sequence snapshots
/// and selected bodies are batched. OpenCode structural context is shared within append-only
/// history, while each native projection stops at its own frozen cut.
pub fn read(repo: &Repo, chain: &Chain, selected: &[usize]) -> Result<BTreeMap<usize, Activity>> {
    read_with_limits(
        repo,
        chain,
        selected,
        storage::MAX_MATERIALIZED_BYTES,
        storage::MAX_MATERIALIZED_BYTES,
    )
}

fn read_with_limits(
    repo: &Repo,
    chain: &Chain,
    selected: &[usize],
    expanded_limit: usize,
    selected_ids_limit: usize,
) -> Result<BTreeMap<usize, Activity>> {
    read_with_stats(
        repo,
        chain,
        selected,
        expanded_limit,
        selected_ids_limit,
        &mut ReadStats::default(),
    )
}

fn read_with_stats(
    repo: &Repo,
    chain: &Chain,
    selected: &[usize],
    expanded_limit: usize,
    selected_ids_limit: usize,
    stats: &mut ReadStats,
) -> Result<BTreeMap<usize, Activity>> {
    anyhow::ensure!(
        selected.iter().all(|&i| i < chain.entries.len()),
        "selected turn is outside the frozen history"
    );
    let turns: BTreeSet<usize> = selected
        .iter()
        .copied()
        .filter(|&i| {
            chain.entries[i]
                .meta
                .as_ref()
                .is_some_and(|m| m.kind == Kind::Turn && !m.session.is_empty())
        })
        .collect();
    let mut needed: BTreeSet<usize> = turns.iter().copied().collect();
    let mut parents = BTreeMap::new();
    for &i in &turns {
        if let Some(parent) = i.checked_sub(1) {
            let m = chain.entries[parent]
                .meta
                .as_ref()
                .context("turn's first parent has no session metadata")?;
            if !m.is_file_line() && !m.session.is_empty() {
                needed.insert(parent);
                parents.insert(i, parent);
            }
        }
    }
    let needed: Vec<usize> = needed.into_iter().collect();
    let asks = needed
        .iter()
        .map(|&i| {
            let entry = &chain.entries[i];
            let path = match entry.meta.as_ref().expect("selected metadata").layout {
                LayoutVersion::V0 => meta::LEGACY_LOG_FILE,
                LayoutVersion::V1 => meta::LOG_FILE,
            };
            format!("{}:{path}", entry.sha)
        })
        .collect();
    let mut sequences = BTreeMap::<usize, Sequence>::new();
    let mut segments: Vec<Vec<usize>> = Vec::new();
    let mut additions = BTreeMap::new();
    let mut expanded_bytes = 0;
    let mut selected_id_bytes = 0usize;
    let mut cursor = 0;
    repo.git_cat_file_batch(asks, storage::MAX_MATERIALIZED_BYTES, |_, kind, body| {
        let i = needed[cursor];
        cursor += 1;
        // Shared prefixes consume the per-object limit, not a cumulative selected-body budget.
        // Only boundary summaries and selected additions survive a snapshot callback.
        let mut snapshot_bytes = 0;
        let text = checked_text(kind, body, &mut snapshot_bytes)?;
        let (ids, legacy) = match chain.entries[i].meta.as_ref().unwrap().layout {
            LayoutVersion::V1 => (storage::parse_sequence(text)?, None),
            LayoutVersion::V0 => {
                let envelopes: Vec<Envelope> = text
                    .split_inclusive('\n')
                    .enumerate()
                    .map(|(index, line)| {
                        anyhow::ensure!(
                            index < storage::MAX_SEQUENCE_EVENTS,
                            "legacy LOG exceeds the event limit"
                        );
                        storage::parse_legacy_envelope_line(line)
                    })
                    .collect::<Result<_>>()?;
                let ids = envelopes
                    .iter()
                    .map(|e| storage::event_id(&storage::envelope_line(e)))
                    .collect::<Result<_>>()?;
                (ids, Some(envelopes))
            }
        };
        let canonical = if legacy.is_some() {
            Cow::Owned(storage::sequence_text(&ids)?)
        } else {
            Cow::Borrowed(text)
        };
        let parent = parents.get(&i).map(|p| &sequences[p]);
        let start = parent.map_or(0, |p| p.len);
        if let Some(parent) = parent {
            anyhow::ensure!(
                canonical.len() >= parent.bytes
                    && <[u8; 32]>::from(Sha256::digest(&canonical.as_bytes()[..parent.bytes]))
                        == parent.digest,
                "turn {} rewrites its first parent's LOG",
                chain.entries[i].sha
            );
        }
        let added_id_bytes = canonical.len() - parent.map_or(0, |p| p.bytes);
        if turns.contains(&i) {
            let extends = segments
                .last()
                .and_then(|segment| segment.last())
                .is_some_and(|previous| {
                    if parent.is_none()
                        || chain.entries[*previous + 1..i].iter().any(|entry| {
                            entry
                                .meta
                                .as_ref()
                                .is_none_or(|meta| meta.is_file_line() || meta.session.is_empty())
                        })
                    {
                        return false;
                    }
                    if chain.entries[i].meta.as_ref().unwrap().layout == LayoutVersion::V0
                        && chain.entries[*previous].meta.as_ref().unwrap().layout
                            == LayoutVersion::V1
                    {
                        return false;
                    }
                    let previous = &sequences[previous];
                    start >= previous.len
                        && canonical.len() >= previous.bytes
                        && <[u8; 32]>::from(Sha256::digest(&canonical.as_bytes()[..previous.bytes]))
                            == previous.digest
                });
            if !extends {
                segments.push(Vec::new());
            }
            segments.last_mut().unwrap().push(i);
        }
        sequences.insert(
            i,
            Sequence {
                len: ids.len(),
                bytes: canonical.len(),
                digest: Sha256::digest(canonical.as_bytes()).into(),
            },
        );
        if turns.contains(&i) {
            additions.insert(
                i,
                match legacy {
                    Some(envelopes) => {
                        let mut added = Vec::new();
                        for envelope in envelopes.into_iter().skip(start) {
                            charge_expansion(
                                &mut expanded_bytes,
                                storage::envelope_line(&envelope).len(),
                                1,
                                expanded_limit,
                            )?;
                            added.push(envelope);
                        }
                        Addition::Legacy(added)
                    }
                    None => {
                        selected_id_bytes = selected_id_bytes
                            .checked_add(added_id_bytes)
                            .context("selected LOG event ID size overflow")?;
                        anyhow::ensure!(
                            selected_id_bytes <= selected_ids_limit,
                            "selected LOG event IDs exceed the read limit"
                        );
                        Addition::Stored(ids.into_iter().skip(start).collect())
                    }
                },
            );
        }
        Ok(())
    })?;

    let mut events = BTreeMap::new();
    let mut asks = Vec::new();
    let mut destinations = Vec::new();
    for (&i, addition) in &additions {
        if let Addition::Stored(ids) = addition {
            for id in ids {
                let key = (i, id.clone());
                let (occurrences, _) = events.entry(key.clone()).or_insert((0usize, None));
                *occurrences += 1;
                if *occurrences == 1 {
                    asks.push(format!(
                        "{}:{}",
                        chain.entries[i].sha,
                        meta::event_path(id)?
                    ));
                    destinations.push(key);
                }
            }
        }
    }
    let mut cursor = 0;
    let mut total_bytes = 0;
    repo.git_cat_file_batch(asks, storage::MAX_EVENT_BYTES, |_, kind, body| {
        let (i, id) = &destinations[cursor];
        cursor += 1;
        let text = checked_text(kind, body, &mut total_bytes)?;
        // Each sequence occurrence expands into native input even when its object is shared.
        charge_expansion(
            &mut expanded_bytes,
            text.len(),
            events[&(*i, id.clone())].0,
            expanded_limit,
        )?;
        let envelope = storage::parse_envelope_line(text)?;
        anyhow::ensure!(
            storage::event_id(text)? == *id,
            "event {id} has invalid content address"
        );
        events.get_mut(&(*i, id.clone())).unwrap().1 = Some(envelope);
        Ok(())
    })?;
    let mut result = BTreeMap::new();
    for segment in segments {
        let mut required = BTreeSet::new();
        for &i in &segment {
            let envelopes: Vec<&Envelope> = match &additions[&i] {
                Addition::Legacy(envelopes) => envelopes.iter().collect(),
                Addition::Stored(ids) => ids
                    .iter()
                    .map(|id| events[&(i, id.clone())].1.as_ref().expect("batched event"))
                    .collect(),
            };
            required.extend(
                envelopes
                    .into_iter()
                    .filter(|envelope| envelope.source == "opencode")
                    .map(source_key),
            );
        }
        let carrier = &chain.entries[*segment.last().unwrap()];
        let mut context = context::Context::read(
            repo,
            &carrier.sha,
            carrier.meta.as_ref().unwrap().layout,
            &required,
            stats,
        )?;
        for i in segment {
            let envelopes = match additions.remove(&i).unwrap() {
                Addition::Legacy(envelopes) => envelopes,
                Addition::Stored(ids) => ids
                    .iter()
                    .map(|id| {
                        events[&(i, id.clone())]
                            .1
                            .as_ref()
                            .expect("batched event")
                            .clone()
                    })
                    .collect(),
            };
            let start = parents.get(&i).map_or(0, |parent| sequences[parent].len);
            result.insert(
                i,
                count(
                    &envelopes,
                    &mut context,
                    start,
                    stats,
                    repo,
                    &chain.entries[i].sha,
                    chain.entries[i].meta.as_ref().unwrap().layout,
                )?,
            );
        }
    }
    Ok(result)
}

fn charge_expansion(
    total: &mut usize,
    bytes: usize,
    occurrences: usize,
    limit: usize,
) -> Result<()> {
    let added = bytes
        .checked_mul(occurrences)
        .context("expanded LOG evidence size overflow")?;
    *total = total
        .checked_add(added)
        .context("expanded LOG evidence size overflow")?;
    anyhow::ensure!(
        *total <= limit,
        "expanded LOG evidence exceeds the read limit"
    );
    Ok(())
}

fn checked_text<'a>(kind: &str, body: ObjectBody<'a>, total: &mut usize) -> Result<&'a str> {
    anyhow::ensure!(kind == "blob", "LOG evidence is not a blob");
    let ObjectBody::Read(bytes) = body else {
        anyhow::bail!("LOG evidence exceeds the read limit");
    };
    *total = total
        .checked_add(bytes.len())
        .context("LOG evidence size overflow")?;
    anyhow::ensure!(
        *total <= storage::MAX_MATERIALIZED_BYTES,
        "selected LOG evidence exceeds the read limit"
    );
    std::str::from_utf8(bytes).context("LOG evidence is not UTF-8")
}

fn count(
    envelopes: &[Envelope],
    context: &mut context::Context,
    start: usize,
    stats: &mut ReadStats,
    repo: &Repo,
    sha: &str,
    layout: LayoutVersion,
) -> Result<Activity> {
    // The envelope supplies the parser identity; a later runtime switch must not reinterpret
    // inherited native records. Separate sessions also keep parser correlation state separate.
    let mut native = BTreeMap::<SourceKey, Vec<(usize, &Envelope)>>::new();
    for (offset, envelope) in envelopes.iter().enumerate() {
        native
            .entry(source_key(envelope))
            .or_default()
            .push((start + offset, envelope));
    }
    if native.keys().any(|key| key.source == "opencode") {
        context.validate_at(repo, sha, layout, start, stats)?;
    }
    let mut tools = 0;
    for (key, records) in native {
        let context = context.select(&key, start, &records)?;
        stats.context_structural_steps = stats
            .context_structural_steps
            .checked_add(context.steps)
            .context("native context step count overflow")?;
        charge_expansion(
            &mut stats.native_context_bytes,
            context.text.len(),
            1,
            usize::MAX,
        )?;
        let selected_start = context.lines;
        let selected_records = records.len();
        let mut input = context.text;
        for (_, envelope) in records {
            let text = serde_json::to_string(&envelope.content)?;
            anyhow::ensure!(
                input
                    .len()
                    .checked_add(text.len() + 1)
                    .is_some_and(|bytes| bytes <= storage::MAX_MATERIALIZED_BYTES),
                "native turn input exceeds the read limit"
            );
            input.push_str(&text);
            input.push('\n');
        }
        let parsed = crate::adapter::get(&key.source)?.parse(&input)?;
        for event in parsed.events {
            let line = event
                .line
                .context("native turn event has no source coordinate")?;
            anyhow::ensure!(
                line < selected_start + selected_records,
                "native turn event has an invalid source coordinate"
            );
            if line >= selected_start && event.kind == crate::adapter::EventKind::ToolUse {
                tools += 1;
            }
        }
    }
    Ok(Activity {
        events: envelopes.len(),
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript;
    use serde_json::json;

    fn native(source: &str, records: &[serde_json::Value]) -> String {
        let raw: String = records.iter().map(|r| format!("{r}\n")).collect();
        transcript::wrap_lines(&raw, source, &format!("agit-{}", "a".repeat(40)))
    }

    fn claude() -> String {
        native(
            "claude-desktop",
            &[
                json!({"type":"user","message":{"role":"user","content":"inspect"}}),
                json!({"type":"assistant","message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"a","name":"Bash","input":{}},
                    {"type":"tool_use","id":"b","name":"Bash","input":{}}
                ]}}),
                json!({"type":"user","message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"a","content":"A"},
                    {"type":"tool_result","tool_use_id":"b","content":"B"}
                ]}}),
                json!({"type":"assistant","message":{"role":"assistant","content":"done"}}),
            ],
        )
    }

    fn codex() -> String {
        native(
            "codex",
            &[
                json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"a","arguments":"{}"}}),
                json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"done"}}),
                json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"b","input":"patch"}}),
                json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"b","output":"done"}}),
            ],
        )
    }

    fn commit(repo: &Repo, log: &str, turn: u32, layout: LayoutVersion) {
        let mut m = meta::Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "codex".into(),
            "/fixture".into(),
        );
        m.turn = Some(turn);
        m.layout = layout;
        meta::write(repo.root(), &m).unwrap();
        match layout {
            LayoutVersion::V0 => {
                std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap()
            }
            LayoutVersion::V1 => storage::write_snapshot(repo.root(), log, log).unwrap(),
        }
        repo.add_all().unwrap();
        assert!(repo.commit(&format!("turn {turn}")).unwrap());
    }

    fn fixture() -> (tempfile::TempDir, Repo) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&temp.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        (temp, repo)
    }

    #[test]
    fn envelope_count_and_native_tool_calls_share_frozen_turn_boundaries() {
        let (_temp, repo) = fixture();
        let first = claude();
        commit(&repo, &first, 1, LayoutVersion::V1);
        let second = format!("{first}{}{}", codex(), claude());
        commit(&repo, &second, 2, LayoutVersion::V1);
        let mut m = meta::read_at_ref(&repo, "HEAD").unwrap();
        m.kind = Kind::View;
        meta::write(repo.root(), &m).unwrap();
        storage::write_snapshot(repo.root(), &second, "").unwrap();
        repo.add_all().unwrap();
        repo.commit("hide the VIEW").unwrap();
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let all = read(&repo, &chain, &[0, 1, 2]).unwrap();
        assert_eq!(
            all[&0],
            Activity {
                events: 4,
                tools: 2
            }
        );
        assert_eq!(
            all[&1],
            Activity {
                events: 8,
                tools: 4
            }
        );
        assert!(!all.contains_key(&2));
        assert_eq!(
            read(&repo, &chain, &[1]).unwrap(),
            BTreeMap::from([(1, all[&1])])
        );
    }

    #[test]
    fn legacy_parent_and_storage_migration_keep_the_same_envelope_boundary() {
        let (_temp, repo) = fixture();
        let first = claude();
        commit(&repo, &first, 1, LayoutVersion::V0);
        commit(&repo, &format!("{first}{}", codex()), 2, LayoutVersion::V1);
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let result = read(&repo, &chain, &[0, 1]).unwrap();
        assert_eq!(
            result[&0],
            Activity {
                events: 4,
                tools: 2
            }
        );
        assert_eq!(
            result[&1],
            Activity {
                events: 4,
                tools: 2
            }
        );
        assert_eq!(read(&repo, &chain, &[0, 1, 0]).unwrap(), result);
    }

    #[test]
    fn malformed_or_missing_selected_evidence_cannot_be_reported_as_zero() {
        for corruption in ["missing", "body", "sequence"] {
            let (_temp, repo) = fixture();
            let first = claude();
            commit(&repo, &first, 1, LayoutVersion::V1);
            let second = format!("{first}{}", codex());
            storage::write_snapshot(repo.root(), &second, &second).unwrap();
            let mut m = meta::read_at_ref(&repo, "HEAD").unwrap();
            m.turn = Some(2);
            meta::write(repo.root(), &m).unwrap();
            let sequence = std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap();
            let id = sequence.lines().last().unwrap();
            let path = repo.root().join(meta::event_path(id).unwrap());
            match corruption {
                "missing" => std::fs::remove_file(path).unwrap(),
                "body" => std::fs::write(path, "{}\n").unwrap(),
                _ => std::fs::write(repo.root().join(meta::LOG_FILE), "broken\n").unwrap(),
            }
            repo.add_all().unwrap();
            repo.commit("invalid turn evidence").unwrap();
            let chain = Chain::read(&repo, "HEAD").unwrap();
            assert!(read(&repo, &chain, &[1]).is_err(), "{corruption}");
        }
    }

    #[test]
    fn filtered_turns_do_not_parse_unselected_native_content() {
        let (_temp, repo) = fixture();
        let unsupported = native("unsupported-runtime", &[json!({"text":"old evidence"})]);
        commit(&repo, &unsupported, 1, LayoutVersion::V1);
        commit(
            &repo,
            &format!("{unsupported}{}", claude()),
            2,
            LayoutVersion::V1,
        );
        let chain = Chain::read(&repo, "HEAD").unwrap();
        assert!(read(&repo, &chain, &[0]).is_err());
        assert_eq!(
            read(&repo, &chain, &[1]).unwrap()[&1],
            Activity {
                events: 4,
                tools: 2
            }
        );
    }

    #[test]
    fn repeated_events_are_charged_before_their_native_content_is_expanded() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let (_temp, repo) = fixture();
            let repeated = claude().repeat(2);
            commit(&repo, &repeated, 1, layout);
            let chain = Chain::read(&repo, "HEAD").unwrap();
            let bytes: usize = repeated
                .split_inclusive('\n')
                .map(|line| storage::parse_legacy_envelope_line(line).unwrap())
                .map(|envelope| storage::envelope_line(&envelope).len())
                .sum();
            assert!(
                read_with_limits(
                    &repo,
                    &chain,
                    &[0],
                    bytes - 1,
                    storage::MAX_MATERIALIZED_BYTES
                )
                .unwrap_err()
                .to_string()
                .contains("expanded LOG evidence exceeds"),
                "{layout:?}"
            );
            assert_eq!(
                read_with_limits(&repo, &chain, &[0], bytes, storage::MAX_MATERIALIZED_BYTES)
                    .unwrap()[&0],
                Activity {
                    events: 8,
                    tools: 4
                }
            );
        }
    }

    #[test]
    fn selected_event_ids_are_bounded_across_independent_session_histories() {
        let (_temp, repo) = fixture();
        let records = claude();
        commit(&repo, &records, 1, LayoutVersion::V1);
        let one_sequence_bytes = repo
            .git(&["cat-file", "-s", "HEAD:LOG"])
            .unwrap()
            .parse::<usize>()
            .unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("file line boundary").unwrap();
        commit(&repo, &records, 1, LayoutVersion::V1);
        let chain = Chain::read(&repo, "HEAD").unwrap();
        assert!(
            read_with_limits(
                &repo,
                &chain,
                &[0, 2],
                storage::MAX_MATERIALIZED_BYTES,
                one_sequence_bytes,
            )
            .unwrap_err()
            .to_string()
            .contains("selected LOG event IDs exceed")
        );
        assert_eq!(
            read_with_limits(
                &repo,
                &chain,
                &[2],
                storage::MAX_MATERIALIZED_BYTES,
                one_sequence_bytes,
            )
            .unwrap()[&2],
            Activity {
                events: 4,
                tools: 2
            }
        );
    }

    #[test]
    fn rewriting_a_parent_sequence_is_not_a_new_turn() {
        let (_temp, repo) = fixture();
        commit(&repo, &claude(), 1, LayoutVersion::V1);
        commit(&repo, &codex(), 2, LayoutVersion::V1);
        let chain = Chain::read(&repo, "HEAD").unwrap();
        assert!(
            read(&repo, &chain, &[1])
                .unwrap_err()
                .to_string()
                .contains("rewrites")
        );
    }
    fn open_message(id: &str, role: &str, mode: Option<&str>) -> serde_json::Value {
        json!({"kind":"message", "id":id, "session_id":"native", "data":{"role":role, "mode":mode}})
    }

    fn open_tool(id: &str, host: &str) -> serde_json::Value {
        json!({"kind":"part", "id":id, "message_id":host, "session_id":"native", "data":{
            "type":"tool", "tool":"bash", "state":{"status":"completed"}
        }})
    }

    #[test]
    fn late_tool_parts_use_prior_hosts_without_recounting_prior_calls() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let (_temp, repo) = fixture();
            let first = native(
                "opencode",
                &[
                    open_message("host", "assistant", None),
                    open_tool("first", "host"),
                ],
            );
            commit(&repo, &first, 1, layout);
            let second = format!(
                "{first}{}",
                native(
                    "opencode",
                    &[
                        open_message("other", "user", None),
                        open_tool("late", "host"),
                    ]
                )
            );
            commit(&repo, &second, 2, LayoutVersion::V1);
            let third = format!(
                "{second}{}",
                native(
                    "opencode",
                    &[
                        open_message("host", "assistant", None),
                        open_tool("first", "host"),
                    ]
                )
            );
            commit(&repo, &third, 3, LayoutVersion::V1);
            let chain = Chain::read(&repo, "HEAD").unwrap();
            let all = read(&repo, &chain, &[0, 1, 2]).unwrap();
            for index in 0..3 {
                assert_eq!(
                    all[&index],
                    Activity {
                        events: 2,
                        tools: 1
                    }
                );
                assert_eq!(read(&repo, &chain, &[index]).unwrap()[&index], all[&index]);
            }
        }
    }

    #[test]
    fn a_later_host_does_not_retroactively_classify_an_orphan_turn() {
        let (_temp, repo) = fixture();
        let first = native("opencode", &[open_tool("early", "host")]);
        commit(&repo, &first, 1, LayoutVersion::V1);
        let second = format!(
            "{first}{}",
            native("opencode", &[open_message("host", "assistant", None)])
        );
        commit(&repo, &second, 2, LayoutVersion::V1);
        let parsed = transcript::display::parse(&second).unwrap();
        assert_eq!(parsed.counts().tools, 1);
        assert_eq!(parsed.events[0].line, Some(0));
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let all = read(&repo, &chain, &[0, 1]).unwrap();
        assert_eq!(
            all[&0],
            Activity {
                events: 1,
                tools: 0
            }
        );
        assert_eq!(
            all[&1],
            Activity {
                events: 1,
                tools: 0
            }
        );
    }

    #[test]
    fn future_compaction_cannot_change_a_settled_turn_projection() {
        let (_temp, repo) = fixture();
        let first = native(
            "opencode",
            &[
                open_message("boundary", "user", None),
                open_message("summary", "assistant", Some("compaction")),
                open_tool("call", "summary"),
            ],
        );
        commit(&repo, &first, 1, LayoutVersion::V1);
        let second = format!(
            "{first}{}",
            native(
                "opencode",
                &[
                    json!({"kind":"part", "id":"compact", "message_id":"boundary", "session_id":"native", "data":{"type":"compaction"}})
                ]
            )
        );
        commit(&repo, &second, 2, LayoutVersion::V1);
        assert_eq!(
            transcript::display::parse(&first).unwrap().counts().tools,
            1
        );
        assert_eq!(
            transcript::display::parse(&second).unwrap().counts().tools,
            0
        );
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let all = read(&repo, &chain, &[0, 1]).unwrap();
        assert_eq!(
            all[&0],
            Activity {
                events: 3,
                tools: 1
            }
        );
        assert_eq!(
            all[&1],
            Activity {
                events: 1,
                tools: 0
            }
        );
    }

    fn open_compaction(host: &str) -> serde_json::Value {
        json!({"kind":"part", "id":"compact", "message_id":host, "session_id":"native", "data":{"type":"compaction"}})
    }

    fn counts_after(prefix: &[serde_json::Value], added: &[serde_json::Value]) -> Activity {
        let (_temp, repo) = fixture();
        let first = native("opencode", prefix);
        commit(&repo, &first, 1, LayoutVersion::V1);
        commit(
            &repo,
            &format!("{first}{}", native("opencode", added)),
            2,
            LayoutVersion::V1,
        );
        let chain = Chain::read(&repo, "HEAD").unwrap();
        read(&repo, &chain, &[1]).unwrap()[&1]
    }

    #[test]
    fn context_neighborhoods_preserve_adjacency_and_cannot_acquire_orphan_parts() {
        let result = counts_after(
            &[
                open_message("boundary", "user", None),
                open_compaction("boundary"),
                open_message("agit-log-context-gap-0", "assistant", None),
                open_message("summary", "assistant", Some("compaction")),
            ],
            &[
                open_tool("boundary-call", "boundary"),
                open_tool("summary-call", "summary"),
                open_tool("orphan", "agit-log-context-gap-1"),
            ],
        );
        assert_eq!(
            result,
            Activity {
                events: 3,
                tools: 2
            }
        );
    }

    #[test]
    fn selected_compactions_and_consumed_predecessors_keep_native_classification() {
        assert_eq!(
            counts_after(
                &[
                    open_message("boundary", "user", None),
                    open_message("summary", "assistant", Some("compaction"))
                ],
                &[open_compaction("boundary"), open_tool("call", "summary")],
            ),
            Activity {
                events: 2,
                tools: 0
            }
        );
        assert_eq!(
            counts_after(
                &[
                    open_message("boundary", "user", None),
                    open_compaction("boundary"),
                    open_message("middle", "assistant", Some("compaction")),
                    open_compaction("middle"),
                    open_message("last", "assistant", Some("compaction")),
                ],
                &[open_tool("call", "last")],
            ),
            Activity {
                events: 1,
                tools: 1
            }
        );
    }

    #[test]
    fn prior_orphan_compaction_needs_a_unique_selected_forward_host() {
        for repeated in [false, true] {
            let mut added = vec![open_message("boundary", "user", None)];
            if repeated {
                added.push(open_message("boundary", "user", None));
            }
            added.extend([
                open_message("summary", "assistant", Some("compaction")),
                open_tool("call", "summary"),
            ]);
            assert_eq!(
                counts_after(&[open_compaction("boundary")], &added),
                Activity {
                    events: added.len(),
                    tools: usize::from(repeated)
                }
            );
        }
    }

    #[test]
    fn later_carrier_cannot_repair_missing_or_corrupt_context_at_an_old_cut() {
        for corruption in [None, Some("{}\n")] {
            let (_temp, repo) = fixture();
            let first = native("opencode", &[open_message("host", "assistant", None)]);
            commit(&repo, &first, 1, LayoutVersion::V1);
            let second = format!("{first}{}", native("opencode", &[open_tool("old", "host")]));
            storage::write_snapshot(repo.root(), &second, &second).unwrap();
            let mut meta = meta::read_at_ref(&repo, "HEAD").unwrap();
            meta.turn = Some(2);
            meta::write(repo.root(), &meta).unwrap();
            let host = repo
                .root()
                .join(meta::event_path(&storage::event_id(&first).unwrap()).unwrap());
            match corruption {
                Some(body) => std::fs::write(&host, body).unwrap(),
                None => std::fs::remove_file(&host).unwrap(),
            }
            repo.add_all().unwrap();
            repo.commit("unreadable context").unwrap();
            std::fs::write(&host, &first).unwrap();
            let third = format!(
                "{second}{}",
                native("opencode", &[open_tool("new", "host")])
            );
            commit(&repo, &third, 3, LayoutVersion::V1);
            let chain = Chain::read(&repo, "HEAD").unwrap();
            assert!(
                read(&repo, &chain, &[1, 2])
                    .unwrap_err()
                    .to_string()
                    .contains("missing or corrupt")
            );
            assert_eq!(
                read(&repo, &chain, &[2]).unwrap()[&2],
                Activity {
                    events: 1,
                    tools: 1
                }
            );
        }
    }

    #[test]
    fn context_resets_after_an_intervening_non_turn_log_rewrite() {
        let (_temp, repo) = fixture();
        let first = native(
            "opencode",
            &[
                open_message("old", "assistant", None),
                open_tool("call", "old"),
            ],
        );
        commit(&repo, &first, 1, LayoutVersion::V1);
        let replacement = native("opencode", &[open_message("new", "assistant", None)]);
        let mut meta = meta::read_at_ref(&repo, "HEAD").unwrap();
        meta.kind = Kind::View;
        meta::write(repo.root(), &meta).unwrap();
        storage::write_snapshot(repo.root(), &replacement, &replacement).unwrap();
        repo.add_all().unwrap();
        repo.commit("replace the saved LOG").unwrap();
        commit(
            &repo,
            &format!(
                "{replacement}{}",
                native("opencode", &[open_tool("late", "old")])
            ),
            2,
            LayoutVersion::V1,
        );
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let all = read(&repo, &chain, &[0, 2]).unwrap();
        assert_eq!(all[&0].tools, 1);
        assert_eq!(all[&2].tools, 0);
    }

    #[test]
    fn omitted_adjacency_evidence_must_exist_at_each_selected_cut() {
        for corruption in [None, Some("{}\n")] {
            let (_temp, repo) = fixture();
            let gap = native("opencode", &[open_message("gap", "assistant", None)]);
            let first = format!(
                "{}{}{}",
                native(
                    "opencode",
                    &[
                        open_message("boundary", "user", None),
                        open_compaction("boundary")
                    ]
                ),
                gap,
                native(
                    "opencode",
                    &[open_message("summary", "assistant", Some("compaction"))]
                )
            );
            commit(&repo, &first, 1, LayoutVersion::V1);
            let second = format!(
                "{first}{}",
                native(
                    "opencode",
                    &[
                        open_tool("boundary-call", "boundary"),
                        open_tool("summary-call", "summary")
                    ]
                )
            );
            storage::write_snapshot(repo.root(), &second, &second).unwrap();
            let mut meta = meta::read_at_ref(&repo, "HEAD").unwrap();
            meta.turn = Some(2);
            meta::write(repo.root(), &meta).unwrap();
            let path = repo
                .root()
                .join(meta::event_path(&storage::event_id(&gap).unwrap()).unwrap());
            match corruption {
                Some(body) => std::fs::write(&path, body).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
            repo.add_all().unwrap();
            repo.commit("unreadable adjacency evidence").unwrap();
            std::fs::write(&path, &gap).unwrap();
            let third = format!(
                "{second}{}",
                native("opencode", &[open_tool("next", "summary")])
            );
            commit(&repo, &third, 3, LayoutVersion::V1);
            let chain = Chain::read(&repo, "HEAD").unwrap();
            assert!(read(&repo, &chain, &[1]).is_err());
            assert!(
                read(&repo, &chain, &[1, 2])
                    .unwrap_err()
                    .to_string()
                    .contains("missing or corrupt")
            );
            assert_eq!(read(&repo, &chain, &[2]).unwrap()[&2].tools, 1);
        }
    }

    #[test]
    fn discarded_prefix_outputs_are_checked_when_active_paths_change() {
        for corruption in [None, Some("{}\n")] {
            let (_temp, repo) = fixture();
            let mut payload = open_tool("output", "host");
            payload["data"]["state"]["output"] = "prior output".into();
            let output = native("opencode", &[payload]);
            let first = format!(
                "{}{output}",
                native("opencode", &[open_message("host", "assistant", None)])
            );
            commit(&repo, &first, 1, LayoutVersion::V1);
            let second = format!(
                "{first}{}",
                native("opencode", &[open_tool("second", "host")])
            );
            commit(&repo, &second, 2, LayoutVersion::V1);
            let third = format!(
                "{second}{}",
                native("opencode", &[open_tool("third", "host")])
            );
            storage::write_snapshot(repo.root(), &third, &third).unwrap();
            let mut meta = meta::read_at_ref(&repo, "HEAD").unwrap();
            meta.turn = Some(3);
            meta::write(repo.root(), &meta).unwrap();
            let path = repo
                .root()
                .join(meta::event_path(&storage::event_id(&output).unwrap()).unwrap());
            match corruption {
                Some(body) => std::fs::write(&path, body).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
            repo.add_all().unwrap();
            repo.commit("unreadable prior output").unwrap();
            std::fs::write(&path, &output).unwrap();
            let fourth = format!(
                "{third}{}",
                native("opencode", &[open_tool("fourth", "host")])
            );
            commit(&repo, &fourth, 4, LayoutVersion::V1);
            let chain = Chain::read(&repo, "HEAD").unwrap();
            assert!(read(&repo, &chain, &[2]).is_err());
            assert!(
                read(&repo, &chain, &[1, 2, 3])
                    .unwrap_err()
                    .to_string()
                    .contains("missing or corrupt")
            );
            assert_eq!(read(&repo, &chain, &[3]).unwrap()[&3].tools, 1);
        }
    }

    #[test]
    fn native_and_logical_session_context_cannot_supply_each_others_hosts() {
        let (_temp, repo) = fixture();
        let first = native("opencode", &[open_message("shared", "assistant", None)]);
        commit(&repo, &first, 1, LayoutVersion::V1);
        let mut foreign_native = open_tool("foreign-native", "shared");
        foreign_native["session_id"] = "another-native".into();
        let second = format!(
            "{first}{}{}",
            native("opencode", &[open_tool("own", "shared"), foreign_native]),
            transcript::wrap_lines(
                &open_tool("foreign-logical", "shared").to_string(),
                "opencode",
                &format!("agit-{}", "b".repeat(40))
            )
        );
        commit(&repo, &second, 2, LayoutVersion::V1);
        let chain = Chain::read(&repo, "HEAD").unwrap();
        assert_eq!(
            read(&repo, &chain, &[0, 1]).unwrap()[&1],
            Activity {
                events: 3,
                tools: 1
            }
        );
        assert_eq!(
            read(&repo, &chain, &[1]).unwrap()[&1],
            Activity {
                events: 3,
                tools: 1
            }
        );
    }

    #[test]
    fn file_line_boundaries_reset_context_even_when_native_prefixes_are_identical() {
        let (_temp, repo) = fixture();
        let first = native(
            "opencode",
            &[
                open_message("host", "assistant", None),
                open_tool("first", "host"),
            ],
        );
        commit(&repo, &first, 1, LayoutVersion::V1);
        let second = format!(
            "{first}{}",
            native("opencode", &[open_tool("second", "host")])
        );
        commit(&repo, &second, 2, LayoutVersion::V1);
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("file line boundary").unwrap();
        let restarted = format!(
            "{second}{}",
            native("opencode", &[open_tool("restarted", "host")])
        );
        commit(&repo, &restarted, 1, LayoutVersion::V1);
        let continued = format!(
            "{restarted}{}",
            native("opencode", &[open_tool("continued", "host")])
        );
        commit(&repo, &continued, 2, LayoutVersion::V1);
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let rows = read(&repo, &chain, &[0, 1, 3, 4]).unwrap();
        for (index, events, tools) in [(0, 2, 1), (1, 1, 1), (3, 4, 3), (4, 1, 1)] {
            assert_eq!(rows[&index], Activity { events, tools });
            assert_eq!(read(&repo, &chain, &[index]).unwrap()[&index], rows[&index]);
        }
        assert_eq!(read(&repo, &chain, &[1, 4]).unwrap()[&4], rows[&4]);
    }

    #[test]
    fn compaction_predecessor_chains_advance_once_and_project_bounded_neighborhoods() {
        let measure = |turns| {
            let (_temp, repo) = fixture();
            let mut log = String::new();
            for turn in 0..turns {
                let host = format!("host-{turn}");
                log.push_str(&native(
                    "opencode",
                    &[
                        open_message(&host, "assistant", Some("compaction")),
                        open_compaction(&host),
                        open_tool(&host, &host),
                    ],
                ));
                commit(&repo, &log, turn + 1, LayoutVersion::V1);
            }
            let chain = Chain::read(&repo, "HEAD").unwrap();
            let selected = (0..chain.entries.len()).collect::<Vec<_>>();
            let mut stats = ReadStats::default();
            let rows = read_with_stats(
                &repo,
                &chain,
                &selected,
                storage::MAX_MATERIALIZED_BYTES,
                storage::MAX_MATERIALIZED_BYTES,
                &mut stats,
            )
            .unwrap();
            for (index, row) in rows {
                assert_eq!(
                    row,
                    Activity {
                        events: 3,
                        tools: usize::from(index % 2 == 0)
                    }
                );
            }
            assert_eq!(stats.context_structural_steps, turns as usize * 2);
            assert!(stats.native_context_bytes < turns as usize * 1024);
            assert_eq!(stats.context_path_checks, (turns as usize - 1) * 3);
            assert!(stats.context_changed_paths <= turns as usize * 3);
            stats
        };
        let small = measure(8);
        let large = measure(32);
        assert_eq!(
            large.context_structural_steps,
            small.context_structural_steps * 4
        );
        assert!(large.native_context_bytes < small.native_context_bytes * 5);
        assert!(large.context_path_checks < small.context_path_checks * 5);
        assert!(large.context_changed_paths <= small.context_changed_paths * 5);
    }

    #[test]
    fn large_prior_payloads_are_read_once_and_only_dependency_neighborhoods_are_reparsed() {
        let measure = |turns| {
            let (_temp, repo) = fixture();
            let mut payload = open_tool("large", "host");
            payload["data"]["state"]["output"] = "x".repeat(256 * 1024).into();
            let mut log = native(
                "opencode",
                &[open_message("host", "assistant", None), payload],
            );
            commit(&repo, &log, 1, LayoutVersion::V1);
            for turn in 1..=turns {
                log.push_str(&native(
                    "opencode",
                    &[
                        open_message(&format!("other-{turn}"), "user", None),
                        open_tool(&format!("call-{turn}"), "host"),
                    ],
                ));
                commit(&repo, &log, turn + 1, LayoutVersion::V1);
            }
            let chain = Chain::read(&repo, "HEAD").unwrap();
            let selected: Vec<_> = (1..chain.entries.len()).collect();
            let mut stats = ReadStats::default();
            let rows = read_with_stats(
                &repo,
                &chain,
                &selected,
                storage::MAX_MATERIALIZED_BYTES,
                storage::MAX_MATERIALIZED_BYTES,
                &mut stats,
            )
            .unwrap();
            assert!(rows.values().all(|row| *row
                == Activity {
                    events: 2,
                    tools: 1
                }));
            assert!(stats.context_payload_bytes <= log.len());
            assert!(stats.native_context_bytes < 1024 * turns as usize);
            assert_eq!(stats.context_path_checks, turns as usize * 2);
            assert_eq!(stats.context_changed_paths, (turns as usize - 1) * 2);
            stats
        };
        let small = measure(8);
        let large = measure(32);
        assert!(large.context_payload_bytes < small.context_payload_bytes * 2);
        assert!(large.native_context_bytes < small.native_context_bytes * 5);
        assert_eq!(large.context_path_checks, small.context_path_checks * 4);
        assert!(large.context_changed_paths <= small.context_changed_paths * 5);

        let (_temp, repo) = fixture();
        let first = claude();
        commit(&repo, &first, 1, LayoutVersion::V1);
        commit(&repo, &format!("{first}{}", codex()), 2, LayoutVersion::V1);
        let chain = Chain::read(&repo, "HEAD").unwrap();
        let mut stats = ReadStats::default();
        read_with_stats(
            &repo,
            &chain,
            &[0, 1],
            storage::MAX_MATERIALIZED_BYTES,
            storage::MAX_MATERIALIZED_BYTES,
            &mut stats,
        )
        .unwrap();
        assert_eq!(stats.context_payload_bytes, 0);
        assert_eq!(stats.native_context_bytes, 0);
    }
}
