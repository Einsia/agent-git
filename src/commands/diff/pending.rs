//! Pending activity requires a verified native evidence boundary.

use crate::adapter::EventKind;
use crate::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

mod opencode;
mod status_opencode;
pub(crate) use status_opencode::worker as status_native_worker;

pub(super) fn check_readonly_repository(repo: &Repo) -> crate::Result<()> {
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
    ] {
        ensure!(
            std::env::var_os(name).is_none(),
            "clear Git repository redirection variables before inspecting the selected Agent repository"
        );
    }
    let mut pending = vec![repo.clone()];
    let mut visited = std::collections::HashSet::new();
    while let Some(repo) = pending.pop() {
        check_readonly_config(&repo)?;
        if !visited.insert(repo.root().canonicalize()?) {
            continue;
        }
        // Git inspects initialized submodule worktrees when reporting a dirty gitlink.
        // Their filters and promisor remotes are independent of the enclosing repository.
        let index = repo.git_bytes_result(&[
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--stage",
            "-z",
        ])?;
        for entry in index
            .split(|byte| *byte == 0)
            .filter(|entry| entry.starts_with(b"160000 "))
        {
            let path = entry
                .splitn(2, |byte| *byte == b'\t')
                .nth(1)
                .context("a submodule index entry has no path")?;
            let path = std::str::from_utf8(path)
                .context("a submodule path cannot be inspected as UTF-8")?;
            if let Some(submodule) = Repo::open(repo.root().join(path)) {
                pending.push(submodule);
            }
        }
    }
    Ok(())
}

fn check_readonly_config(repo: &Repo) -> crate::Result<()> {
    let (status, _, _) = repo.git_status(&[
        "config",
        "--get-regexp",
        "^(remote\\..*\\.promisor|extensions\\.partialclone|filter\\..*\\.(clean|process))$",
    ])?;
    match status {
        Some(1) => Ok(()),
        Some(0) => anyhow::bail!(
            "read-only pending inspection requires local objects and no configured clean/process filters; inspect the shared-file diff and native session separately"
        ),
        _ => anyhow::bail!("cannot verify the repository's read-only Git configuration"),
    }
}

pub(super) fn inspect(repo: &Repo, slug: &str, branch: &str) -> crate::Result<String> {
    let branch_ref = format!("refs/heads/{branch}");
    ensure!(
        repo.git_status(&["check-ref-format", &branch_ref])?.0 == Some(0),
        "the selected local branch name is invalid"
    );
    let tip = branch_tip(repo, &branch_ref)?;
    let snapshot = meta::read_at_ref_result(repo, &tip)?
        .context("the selected branch has no session metadata")?;
    if snapshot.is_file_line() {
        return Ok("not applicable; the selected branch is a shared-file line".into());
    }
    let (log, _view) = if snapshot.session.is_empty() {
        (String::new(), String::new())
    } else {
        storage::materialize_pair_at(repo.root(), &tip)
            .context("the selected branch's committed LOG or VIEW cannot be verified")?
    };
    let store = Store::open()?.context("no local session store exists")?;
    let (owner, agent) = super::super::parse_slug(slug)?;
    let active = claims(&store, &owner, &agent, branch)?;
    let [claim] = active.as_slice() else {
        anyhow::bail!("the selected branch does not have exactly one readable active local claim");
    };
    let before = claim.to_json()?;
    let summary = if claim.source == "opencode" {
        ensure!(
            snapshot.runtime.is_empty()
                || snapshot.runtime == claim.source
                || claim.baseline_bytes.is_some(),
            "the native runtime differs from the committed session runtime"
        );
        opencode::inspect(repo, claim, &tip, &log)?
    } else {
        inspect_append_only(repo, claim, &tip, &log, &snapshot.runtime)?
    };
    let after = claims(&store, &owner, &agent, branch)?;
    ensure!(
        after.len() == 1
            && after[0].instance() == claim.instance()
            && after[0].to_json()? == before
            && branch_tip(repo, &branch_ref)? == tip,
        "the branch or native claim changed during inspection; retry the comparison"
    );
    Ok(summary)
}

/// Status reserves one bounded evidence allowance before inspecting a displayed claim.
/// The caller rechecks the exact link inventory and branch head before publishing the row.
pub(crate) fn inspect_status(
    repo: &Repo,
    claim: &link::Link,
    tip: &str,
    budget: &mut crate::adapter::native_snapshot::Budget,
    deadline: crate::infra::local_git::Deadline,
) -> crate::Result<String> {
    budget.reserve(32 * 1024 * 1024)?;
    if claim.source == "opencode" {
        budget.reserve(status_opencode::RESERVATION - 32 * 1024 * 1024)?;
    }
    let limits = status_opencode::limits();
    let output = repo.inspection_output_with_deadline(
        &["show", &format!("{tip}:{}", meta::FILE)],
        1024 * 1024,
        deadline,
    )?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "the committed session metadata is unavailable"
    );
    let snapshot = meta::parse_strict(std::str::from_utf8(&output.stdout)?, tip)?;
    ensure!(
        snapshot.is_session_line(),
        "the claimed branch is not a session line"
    );
    let (log, _view) = if snapshot.session.is_empty() {
        (String::new(), String::new())
    } else {
        storage::materialize_pair_status(repo.root(), tip, limits.bytes, limits.bytes, deadline)?
    };
    ensure!(
        snapshot.runtime.is_empty()
            || snapshot.runtime == claim.source
            || claim.baseline_bytes.is_some(),
        "the native runtime differs from the committed session runtime"
    );
    if claim.source == "opencode" {
        status_opencode::inspect(repo, claim, tip, &log, deadline)
    } else {
        inspect_append_only_with_limits(repo, claim, tip, &log, &snapshot.runtime, Some(limits))
    }
}

fn inspect_append_only(
    repo: &Repo,
    claim: &link::Link,
    tip: &str,
    log: &str,
    runtime: &str,
) -> crate::Result<String> {
    inspect_append_only_with_limits(repo, claim, tip, log, runtime, None)
}

fn inspect_append_only_with_limits(
    repo: &Repo,
    claim: &link::Link,
    tip: &str,
    log: &str,
    runtime: &str,
    limits: Option<crate::adapter::native_snapshot::Limits>,
) -> crate::Result<String> {
    let bytes = if let Some(limits) = limits {
        let runtime = if claim.source == "claude-desktop" {
            "claude-code"
        } else {
            &claim.source
        };
        let runtime = crate::adapter::get(runtime)?.id();
        let source = crate::adapter::native_snapshot::lookup_files_without_database(
            runtime,
            &claim.session_id,
            limits,
        )?;
        if runtime == "codex" {
            crate::adapter::codex::read_pending_bytes(&source, limits)?
        } else {
            crate::adapter::native_snapshot::read_file_bytes(&source.path, limits)?
        }
    } else {
        read_native(claim)?
    };
    let text = std::str::from_utf8(&bytes).context("the native transcript is not valid UTF-8")?;
    let records = if let Some(limits) = limits {
        records_with_limit(text, claim, limits.records)?
    } else {
        records(text, claim)?
    };
    let materialized = claim.baseline_bytes.is_some()
        || claim.baseline_hash.is_some()
        || claim.materialized_from.is_some();
    let boundary = if materialized {
        materialized_boundary(claim, tip, &bytes)?
    } else {
        ensure!(
            runtime.is_empty() || runtime == claim.source,
            "the native runtime differs from the committed session runtime"
        );
        native_boundary(
            repo,
            claim,
            log,
            text,
            &records,
            limits.map(|limits| limits.working_bytes),
        )?
    };
    summarize(claim, text, boundary, &records)
}

fn branch_tip(repo: &Repo, branch_ref: &str) -> crate::Result<String> {
    repo.git(&["rev-parse", "--verify", &format!("{branch_ref}^{{commit}}")])
        .context("the selected local branch cannot be read")
}

fn claims(store: &Store, owner: &str, agent: &str, branch: &str) -> crate::Result<Vec<link::Link>> {
    let mut claims = Vec::new();
    for runtime in crate::adapter::RUNTIMES {
        let directory = store.root().join(runtime);
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => anyhow::bail!("the local claim inventory cannot be read"),
        };
        for entry in entries {
            let entry = entry.context("the local claim inventory cannot be read")?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            ensure!(
                entry.file_type()?.is_file(),
                "the local claim inventory contains a non-regular claim file"
            );
            let claim = link::read(&entry.path())
                .context("the local claim inventory contains an unreadable or malformed claim")?;
            if claim.is_active()
                && claim.agent.as_deref() == Some(agent)
                && claim.branch.as_deref() == Some(branch)
            {
                ensure!(
                    claim.owner.is_some(),
                    "a matching legacy claim has no recorded owner"
                );
                if claim.owner.as_deref() == Some(owner) {
                    claims.push(claim);
                }
            }
        }
    }
    Ok(claims)
}

fn read_native(claim: &link::Link) -> crate::Result<Vec<u8>> {
    let path = match claim.source.as_str() {
        "codex" => crate::adapter::codex::resolve_readonly(&claim.session_id)?,
        "claude-code" | "claude-desktop" => {
            crate::adapter::claude_code::resolve_readonly(&claim.session_id)?
        }
        "cursor" => crate::adapter::cursor::resolve_readonly(&claim.session_id)?,
        _ => anyhow::bail!("the selected runtime has no read-only transcript reader"),
    };
    crate::adapter::native_snapshot::read_file_bytes(
        &path,
        crate::adapter::native_snapshot::Limits {
            bytes: storage::MAX_MATERIALIZED_BYTES,
            working_bytes: storage::MAX_MATERIALIZED_BYTES + 1,
            ..Default::default()
        },
    )
    .context("the selected native transcript carrier cannot be read unchanged")
}

#[derive(Debug)]
struct Record {
    line: usize,
    start: usize,
    end: usize,
    hash: String,
    retained_context: bool,
}

#[derive(Debug)]
struct Records {
    records: Vec<Record>,
    incomplete_tail: bool,
}

fn records(text: &str, claim: &link::Link) -> crate::Result<Records> {
    records_with_limit(text, claim, storage::MAX_SEQUENCE_EVENTS)
}

fn records_with_limit(text: &str, claim: &link::Link, limit: usize) -> crate::Result<Records> {
    let limit = limit.min(storage::MAX_SEQUENCE_EVENTS);
    let mut records = Vec::new();
    let mut start = 0;
    let mut incomplete_tail = false;
    for (line, value) in text.split_inclusive('\n').enumerate() {
        let end = start + value.len();
        if !value.trim().is_empty() {
            // Admit the record before parsing or hashing any of its native content.
            if records.len() >= limit {
                return Err(crate::adapter::native_snapshot::Unavailable::BudgetExceeded.into());
            }
            ensure!(
                value.len() <= storage::MAX_EVENT_BYTES,
                "the native transcript exceeds the record inspection limit"
            );
            match serde_json::from_str::<serde_json::Value>(value) {
                Ok(value) if value.is_object() => {
                    let declared = match claim.source.as_str() {
                        "codex" if value["type"] == "session_meta" => {
                            value["payload"]["id"].as_str()
                        }
                        "claude-code" | "claude-desktop" => value["sessionId"].as_str(),
                        _ => None,
                    };
                    ensure!(
                        declared.is_none_or(|id| id == claim.session_id),
                        "the native transcript identity differs from the selected claim"
                    );
                    records.push(Record {
                        line,
                        start,
                        end,
                        hash: transcript::object_hash(&value),
                        retained_context: claim.source == "codex" && value["type"] == "compacted",
                    });
                }
                Err(error) if !value.ends_with('\n') && error.is_eof() => {
                    incomplete_tail = true;
                }
                _ => anyhow::bail!("the native transcript contains a malformed record"),
            }
        }
        start = end;
    }
    Ok(Records {
        records,
        incomplete_tail,
    })
}

fn materialized_boundary(claim: &link::Link, tip: &str, bytes: &[u8]) -> crate::Result<usize> {
    ensure!(
        claim.materialized_from.as_deref() == Some(tip),
        "the materialized branch-tip evidence is missing or the selected branch has advanced"
    );
    let boundary = usize::try_from(
        claim
            .baseline_bytes
            .context("the materialized byte baseline is missing")?,
    )?;
    let hash = claim
        .baseline_hash
        .as_deref()
        .context("the materialized baseline digest is missing")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|value| value.is_ascii_hexdigit()),
        "the materialized baseline digest is invalid"
    );
    ensure!(
        bytes.len() >= boundary,
        "the native transcript was truncated inside its materialized baseline"
    );
    ensure!(
        hex::encode(Sha256::digest(&bytes[..boundary])) == hash,
        "the native transcript was rewritten inside its materialized baseline"
    );
    ensure!(
        boundary == 0 || boundary == bytes.len() || bytes[boundary - 1] == b'\n',
        "the materialized baseline is not a complete record boundary"
    );
    Ok(boundary)
}

fn native_boundary(
    repo: &Repo,
    claim: &link::Link,
    log: &str,
    live: &str,
    records: &Records,
    hydration_limit: Option<usize>,
) -> crate::Result<usize> {
    let committed = storage::parse_envelopes(log)?;
    ensure!(
        committed
            .iter()
            .all(|envelope| envelope.source == claim.source),
        "the committed LOG is not a native prefix of the selected runtime"
    );
    ensure!(
        records.records.len() >= committed.len(),
        "the native transcript was truncated before the settled prefix ended"
    );
    let direct = committed
        .iter()
        .zip(&records.records)
        .all(|(a, b)| a.object_hash == b.hash);
    if !direct {
        let dictionary = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?;
        let (committed_plain, live_plain) = if let Some(limit) = hydration_limit {
            let mut reports = dictionary
                .hydrate_batch_readonly_with_limits(
                    &[log, live],
                    limit,
                    Some(crate::domain::secret_filter::ReadonlyDictionaryLimits::STATUS),
                )?
                .into_iter();
            let committed = reports
                .next()
                .context("missing committed hydration result")??;
            let live = reports
                .next()
                .context("missing native hydration result")??;
            (committed, live)
        } else {
            dictionary
                .hydrate_pair_readonly(log, live)
                .context("the existing repository secret mappings cannot be read")?
        };
        ensure!(
            committed_plain.unresolved == 0 && live_plain.unresolved == 0,
            "the repository secret mappings needed to verify the settled prefix are unavailable"
        );
        ensure!(
            transcript::continuity_of_content(&committed_plain.text, &live_plain.text)
                != transcript::Continuity::Diverged,
            "the native transcript was rewritten inside the settled prefix"
        );
    }
    Ok(committed
        .len()
        .checked_sub(1)
        .map(|index| records.records[index].end)
        .unwrap_or(0))
}

fn summarize(
    claim: &link::Link,
    text: &str,
    boundary: usize,
    records: &Records,
) -> crate::Result<String> {
    let pending: Vec<_> = records
        .records
        .iter()
        .filter(|record| record.start >= boundary)
        .collect();
    ensure!(
        !records
            .records
            .iter()
            .any(|record| record.start < boundary && record.end > boundary),
        "the settled prefix ends inside a native record"
    );
    ensure!(
        !records.incomplete_tail
            || boundary <= records.records.last().map(|record| record.end).unwrap_or(0),
        "the settled prefix includes an incomplete native record"
    );
    let adapter = crate::adapter::get(&claim.source)?;
    // Parsing the complete source retains tool hosts and compaction context. Parsing only
    // the suffix can turn retained replacement history into apparently new user prompts.
    let session = adapter.parse(text)?;
    ensure!(
        session.id.is_empty() || session.id == claim.session_id,
        "the native transcript identity differs from the selected claim"
    );
    if pending.is_empty() && !records.incomplete_tail {
        return Ok("no unsettled content in the verified native snapshot".into());
    }
    let pending_lines: std::collections::HashSet<_> =
        pending.iter().map(|record| record.line).collect();
    let activity_lines: std::collections::HashSet<_> = pending
        .iter()
        .filter(|record| !record.retained_context)
        .map(|record| record.line)
        .collect();
    let selected = |event: &crate::adapter::Event| {
        event
            .line
            .is_some_and(|line| activity_lines.contains(&line))
    };
    let new_turns = session
        .events
        .iter()
        .filter(|event| selected(event) && event.kind == EventKind::UserPrompt)
        .count();
    let tools = session
        .events
        .iter()
        .filter(|event| selected(event) && event.kind == EventKind::ToolUse)
        .count();
    let turns = crate::domain::turn::groups_of(&session)
        .iter()
        .filter(|group| {
            group.iter().any(|index| {
                selected(&session.events[*index]) && session.events[*index].kind != EventKind::Other
            })
        })
        .count();
    let unattributed = session.events.iter().any(|event| event.line.is_none());
    let modeled: std::collections::HashSet<_> = session
        .events
        .iter()
        .filter_map(|event| event.line)
        .collect();
    let unmodeled: std::collections::HashSet<_> = session
        .events
        .iter()
        .filter(|event| event.kind == EventKind::Other)
        .filter_map(|event| event.line)
        .collect();
    let unclassified = pending_lines
        .iter()
        .filter(|line| !modeled.contains(*line) || unmodeled.contains(*line))
        .count();
    let semantics = if unattributed {
        "turn and tool counts unavailable because native event coordinates are incomplete"
            .to_owned()
    } else {
        format!(
            "{turns} user turns with pending activity ({new_turns} newly started), {tools} ToolUse calls"
        )
    };
    let tail = if records.incomplete_tail {
        "; an incomplete trailing record is still being written"
    } else {
        ""
    };
    let coverage = if unclassified > 0 {
        format!(
            "; {unclassified} events contain unclassified activity, so semantic counts are lower bounds"
        )
    } else {
        String::new()
    };
    Ok(format!(
        "{} events, {semantics}{coverage}{tail}",
        pending.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claim() -> link::Link {
        link::Link::new("codex", "native-session", None)
    }

    fn line(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    fn message(role: &str, text: &str) -> String {
        line(json!({"type":"response_item","payload":{
            "type":"message","role":role,"content":[{"type":"input_text","text":text}]
        }}))
    }

    fn summary(text: &str, boundary: usize) -> String {
        summarize(&claim(), text, boundary, &records(text, &claim()).unwrap()).unwrap()
    }

    #[test]
    fn continuation_counts_new_records_without_recounting_the_settled_turn() {
        let prefix = format!(
            "{}{}",
            message("user", "original"),
            message("assistant", "reply")
        );
        let tail = format!(
            "{}{}{}",
            message("user", "new request"),
            line(
                json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call"}})
            ),
            line(
                json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call","output":"done"}})
            )
        );
        assert_eq!(
            summary(&format!("{prefix}{tail}"), prefix.len()),
            "3 events, 1 user turns with pending activity (1 newly started), 1 ToolUse calls"
        );
        assert_eq!(
            summary(&prefix, prefix.len()),
            "no unsettled content in the verified native snapshot"
        );
    }

    #[test]
    fn appended_reply_belongs_to_the_existing_user_turn() {
        let prefix = message("user", "continue");
        let text = format!("{prefix}{}", message("assistant", "answer"));
        assert_eq!(
            summary(&text, prefix.len()),
            "1 events, 1 user turns with pending activity (0 newly started), 0 ToolUse calls"
        );
    }

    #[test]
    fn codex_compaction_does_not_recount_retained_history() {
        let prefix = format!(
            "{}{}",
            message("user", "retained prompt"),
            message("assistant", "prior reply")
        );
        let compaction = line(
            json!({"type":"compacted","payload":{"replacement_history":[{
                "type":"message","role":"user","content":[{"type":"input_text","text":"retained prompt"}]
            }]}}),
        );
        let tail = format!("{compaction}{}", message("user", "actual next prompt"));
        let result = summary(&format!("{prefix}{tail}"), prefix.len());
        assert!(result.contains("(1 newly started)"), "{result}");
        assert!(!result.contains("(2 newly started)"), "{result}");
        assert!(result.starts_with("2 events"), "{result}");
        let header = line(json!({"type":"session_meta","payload":{"id":"native-session"}}));
        let result = summary(&format!("{header}{tail}"), header.len());
        assert!(result.contains("(1 newly started)"), "{result}");
        assert!(
            result.contains("1 user turns with pending activity"),
            "{result}"
        );
        let result = summary(&format!("{header}{compaction}"), header.len());
        assert!(
            result.contains("0 user turns with pending activity (0 newly started)"),
            "{result}"
        );
    }

    #[test]
    fn record_count_is_distinct_from_tool_use_count() {
        let claim = link::Link::new("claude-code", "native-session", None);
        let text = format!(
            "{}{}",
            line(
                json!({"type":"user","sessionId":"native-session","message":{"role":"user","content":"work"}})
            ),
            line(
                json!({"type":"assistant","sessionId":"native-session","message":{"role":"assistant","content":[
                    {"type":"text","text":"working"},
                    {"type":"tool_use","id":"a","name":"Bash","input":{"command":"true"}},
                    {"type":"tool_use","id":"b","name":"Bash","input":{"command":"true"}}
                ]}})
            )
        );
        let result = summarize(&claim, &text, 0, &records(&text, &claim).unwrap()).unwrap();
        assert_eq!(
            result,
            "2 events, 1 user turns with pending activity (1 newly started), 2 ToolUse calls"
        );
    }

    #[test]
    fn malformed_and_incomplete_native_records_never_become_a_clean_report() {
        assert!(records("{}\nmalformed\n", &claim()).is_err());
        assert!(records("[]\n", &claim()).is_err());
        let prefix = message("user", "settled");
        let text = format!("{prefix}{{\"type\":");
        let result = summary(&text, prefix.len());
        assert!(result.contains("incomplete trailing record"), "{result}");
        assert!(!result.contains("no unsettled"), "{result}");
        assert!(records(&format!("{prefix}{{\"type\":\n"), &claim()).is_err());
    }

    #[test]
    fn record_budget_is_admitted_before_json_parsing() {
        use crate::adapter::native_snapshot::Unavailable;
        let prefix = "{}\n{}\n{}\n";
        assert_eq!(
            records_with_limit(&format!("{prefix}\n  \n"), &claim(), 3)
                .unwrap()
                .records
                .len(),
            3
        );
        for next in [
            "malformed\n",
            "[]\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"foreign\"}}\n",
            "{\"type\":",
        ] {
            let error = records_with_limit(&format!("{prefix}{next}"), &claim(), 3).unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<Unavailable>(),
                    Some(Unavailable::BudgetExceeded)
                ),
                "content beyond the admitted record boundary was inspected: {error:#}"
            );
        }
        let error = records_with_limit("malformed\n", &claim(), 0).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Unavailable>(),
            Some(Unavailable::BudgetExceeded)
        ));
        assert!(
            records_with_limit("malformed\n", &claim(), 1)
                .unwrap_err()
                .to_string()
                .contains("malformed record")
        );
        assert!(
            records_with_limit(&format!("{prefix}{{\"type\":"), &claim(), 4)
                .unwrap()
                .incomplete_tail
        );
        assert_eq!(
            records(&format!("{prefix}{{}}\n"), &claim())
                .unwrap()
                .records
                .len(),
            4
        );
    }

    #[test]
    fn foreign_identity_is_refused_even_inside_an_otherwise_matching_prefix() {
        let first = line(json!({"type":"session_meta","payload":{"id":"native-session"}}));
        let foreign = line(json!({"type":"session_meta","payload":{"id":"someone-else"}}));
        assert!(
            records(&format!("{first}{foreign}"), &claim())
                .unwrap_err()
                .to_string()
                .contains("identity differs")
        );
    }

    #[test]
    fn unknown_native_activity_exposes_incomplete_semantic_counts() {
        let text = line(json!({"type":"future_tool_call","payload":{"tool":"unknown"}}));
        let result = summary(&text, 0);
        assert!(result.starts_with("1 events"), "{result}");
        assert!(
            result.contains("semantic counts are lower bounds"),
            "{result}"
        );
        assert!(!result.contains("no unsettled"), "{result}");
    }

    #[test]
    fn materialization_requires_the_exact_tip_digest_and_record_boundary() {
        let prefix = message("user", "materialized history");
        let text = format!("{prefix}{}", message("assistant", "pending"));
        let mut claim = claim();
        claim.baseline_bytes = Some(prefix.len() as u64);
        claim.baseline_hash = Some(hex::encode(Sha256::digest(prefix.as_bytes())));
        claim.materialized_from = Some("tip".into());
        assert_eq!(
            materialized_boundary(&claim, "tip", text.as_bytes()).unwrap(),
            prefix.len()
        );
        assert!(materialized_boundary(&claim, "other-tip", text.as_bytes()).is_err());
        assert!(
            materialized_boundary(&claim, "tip", &text.as_bytes()[..prefix.len() - 1])
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
        assert!(
            materialized_boundary(&claim, "tip", text.replace("history", "changed").as_bytes())
                .unwrap_err()
                .to_string()
                .contains("rewritten")
        );
        claim.baseline_hash = None;
        assert!(materialized_boundary(&claim, "tip", text.as_bytes()).is_err());
        claim.baseline_bytes = Some(1);
        claim.baseline_hash = Some(hex::encode(Sha256::digest(&text.as_bytes()[..1])));
        assert!(
            materialized_boundary(&claim, "tip", text.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("record boundary")
        );
    }

    #[test]
    fn native_prefix_uses_record_identity_instead_of_length_or_turn_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = Repo::init(temporary.path()).unwrap();
        let prefix = message("user", "same size");
        let log = transcript::wrap_lines(&prefix, "codex", &format!("agit-{}", "a".repeat(40)));
        let text = format!("\n{prefix}\n{}", message("assistant", "appended"));
        let records = records(&text, &claim()).unwrap();
        assert_eq!(
            native_boundary(&repo, &claim(), &log, &text, &records, None).unwrap(),
            prefix.len() + 1
        );
        let rewritten = text.replace("same size", "evil size");
        let error = native_boundary(
            &repo,
            &claim(),
            &log,
            &rewritten,
            &super::records(&rewritten, &claim()).unwrap(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("rewritten"), "{error:#}");
        let truncated = "{}\n";
        let extra_log = format!("{log}{log}");
        assert!(
            native_boundary(
                &repo,
                &claim(),
                &extra_log,
                truncated,
                &super::records(truncated, &claim()).unwrap(),
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("truncated")
        );
    }

    #[test]
    fn claim_inventory_does_not_hide_damaged_claims_or_guess_an_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::at(temporary.path());
        let mut claim = claim();
        claim.owner = Some("alice".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("topic".into());
        let path = link::write(&store, &claim).unwrap();
        assert_eq!(claims(&store, "alice", "notes", "topic").unwrap().len(), 1);
        assert!(claims(&store, "bob", "notes", "topic").unwrap().is_empty());
        claim.owner = None;
        link::write(&store, &claim).unwrap();
        assert!(claims(&store, "alice", "notes", "topic").is_err());
        std::fs::write(path, "malformed").unwrap();
        assert!(claims(&store, "alice", "notes", "topic").is_err());
    }
}
