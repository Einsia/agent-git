use super::*;
use crate::adapter::{codex, native_snapshot};
use crate::domain::privacy::{projector::Mode, service};
use anyhow::ensure;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) fn import(
    store: &Store,
    repo: &Repo,
    branch: &str,
    mut link: Link,
    owner: &str,
    opts: &SettleOpts,
) -> CmdResult {
    let tip = optional_branch_commit(repo, &format!("refs/heads/{branch}"))?
        .context("large Codex import needs a declared session branch")?;
    let mut snapshot = meta::read_at_ref_result(repo, &tip)?.context("missing branch metadata")?;
    let source_link = link.clone();
    ensure!(
        opts.milestone.is_none() && link.pending_milestone.is_none(),
        "large Codex import cannot assign a pending milestone"
    );
    ensure!(
        snapshot.is_session_line()
            && snapshot.runtime == "codex"
            && meta::is_bare_id(&snapshot.session),
        "expected a Codex session line"
    );
    let (log, view) =
        storage::local_v1_sequences(repo.root(), &tip)?.context("expected a v1 session line")?;
    let saved = link.native_checkpoint.as_ref();
    if let Some(checkpoint) = saved {
        ensure!(
            checkpoint.tip == tip
                && snapshot.turn.is_some_and(|turn| turn > 0)
                && snapshot.runtime_instances.contains(&link.session_id)
                && snapshot.baseline_bytes == Some(checkpoint.bytes)
                && checkpoint.bytes > 0
                && !log.is_empty()
                && log.ends_with(&view),
            "large Codex checkpoint does not match the saved turn"
        );
    } else {
        ensure!(
            snapshot.turn.is_none()
                && log.is_empty()
                && view.is_empty()
                && repo
                    .git(&["ls-tree", &tip, "--", meta::EVENTS_DIR])?
                    .is_empty(),
            "large Codex import cannot replace existing session evidence"
        );
    }
    ensure!(
        repo.git(&["ls-tree", &tip, "--", "privacy/envelope.json"])?
            .is_empty(),
        "large Codex import cannot replace encrypted evidence"
    );
    super::super::plumbing::ensure_v1_upgrade_preflight(repo, &tip)?;
    let (author, email) = commit_author(repo, owner)?;
    snapshot.committer_account = credentials::current_account()?;
    snapshot.kind = Kind::Turn;
    if saved.is_none() {
        snapshot.runtime_instances = vec![link.session_id.clone()];
        snapshot.capture = opts.capture.then(|| meta::CaptureOrigin {
            runtime: "codex".into(),
            session: link.session_id.clone(),
        });
        snapshot.title =
            crate::adapter::native_title::native_title("codex", &link.session_id, "", None);
    }
    let mut parent = tip.clone();
    let mut turn_parent = tip.clone();
    let mut message = String::new();
    let mut header = None;
    let mut turn_bytes = String::new();
    let mut sequence = String::new();
    for id in &log {
        sequence.push_str(id);
        sequence.push('\n');
    }
    let mut view_start = sequence.len() - view.iter().map(|id| id.len() + 1).sum::<usize>();
    let mut source_hash = Sha256::new();
    let mut end = 0u64;
    let mut checkpoint = None;
    let mut turns = snapshot.turn.unwrap_or(0);
    let mut count = log.len();
    let previous_ids: std::collections::HashSet<_> = log.into_iter().collect();
    let mut candidates = std::collections::HashMap::new();
    visit(&source_link, |line| {
        let start = end;
        end = end
            .checked_add(line.len() as u64)
            .context("rollout size overflow")?;
        source_hash.update(line);
        if let Some(saved) = saved.filter(|saved| start < saved.bytes) {
            if start == 0 {
                header = Some(serde_json::from_slice(line)?);
            }
            ensure!(
                end <= saved.bytes
                    && (end != saved.bytes
                        || hex::encode(source_hash.clone().finalize()) == saved.sha256),
                "large Codex rollout differs from the saved checkpoint"
            );
            return Ok(());
        }
        ensure!(
            turn_bytes.len() + line.len() <= storage::MAX_EVENT_BYTES,
            "a Codex execution exceeds the in-memory turn budget"
        );
        let text = std::str::from_utf8(line)?;
        let record: serde_json::Value = serde_json::from_str(text)?;
        if header.is_none() {
            ensure!(
                record["type"] == "session_meta"
                    && record["payload"]["id"].as_str() == Some(source_link.native_thread_id()),
                "unexpected Codex rollout identity"
            );
            if let Some(cwd) = record["payload"]["cwd"].as_str() {
                link.cwd.get_or_insert_with(|| cwd.to_owned());
            }
            snapshot.cwd = link.cwd.clone().unwrap_or_else(|| snapshot.cwd.clone());
            snapshot.code = meta::code_of(Path::new(&snapshot.cwd));
            snapshot.completeness = snapshot.code.as_ref().map(|_| Completeness::Unknown);
            service::protect_metadata(repo.root(), &mut snapshot)?;
            header = Some(record.clone());
        } else {
            ensure!(
                record["type"] != "session_meta",
                "unexpected additional Codex header"
            );
        }
        turn_bytes.push_str(text);
        if record["type"] != "event_msg"
            || !matches!(
                record["payload"]["type"].as_str(),
                Some("task_complete" | "turn_aborted")
            )
        {
            return Ok(());
        }
        let records = turn_bytes
            .lines()
            .enumerate()
            .map(|(index, line)| Ok((index, serde_json::from_str::<serde_json::Value>(line)?)));
        let ir = codex::parse_records_after(
            records.collect::<serde_json::Result<Vec<_>>>()?,
            if turns == 0 { None } else { header.as_ref() },
            turns != 0,
        );
        let open = crate::adapter::get("codex")?.open_tool_calls(&turn_bytes);
        let chunks = completed_turn_chunks(&turn_bytes, &ir, &open, turns != 0, None);
        let prompts = ir
            .events
            .iter()
            .filter(|event| event.kind == EventKind::UserPrompt)
            .count();
        ensure!(
            chunks.len() == 1
                && chunks[0].end_byte == turn_bytes.len()
                && (prompts == 1 || (turns != 0 && prompts == 0)),
            "large Codex import cannot prove a complete user turn at byte {end}"
        );
        let chunk = &chunks[0];
        ensure!(
            saved.is_none() || checkpoint.is_some() || prompts == 1,
            "cannot rewrite a previously saved turn without a new user prompt"
        );
        if prompts == 1 {
            turn_parent = parent.clone();
            let prompt =
                service::protect_for_commit(Some(repo.root()), &chunk.prompt, Mode::ProtectText)?;
            turns = turns.checked_add(1).context("turn count overflow")?;
            let subject = crate::domain::turn::gist_of(&prompt);
            let subject = if subject.is_empty() {
                format!("agit: turn #{turns}")
            } else {
                subject
            };
            message = service::protect_for_commit(Some(repo.root()), &subject, Mode::ProtectText)?;
        }
        let protected = service::transform(Some(repo.root()), &turn_bytes, Mode::ProtectJsonl);
        ensure!(
            service::passed(&protected) && protected.omitted == 0,
            "privacy projection did not preserve the whole execution"
        );
        let compact_lines = ir
            .events
            .iter()
            .filter(|event| event.kind.is_compact())
            .filter_map(|event| event.line)
            .collect::<std::collections::HashSet<_>>();
        ensure!(
            turn_bytes.lines().count() == protected.content.lines().count(),
            "privacy projection changed record boundaries"
        );
        let mut files = BTreeMap::new();
        for (index, line) in protected.content.lines().enumerate() {
            if compact_lines.contains(&index) {
                view_start = sequence.len();
            }
            let wrapped = transcript::wrap_lines(line, "codex", &snapshot.session);
            ensure!(
                wrapped.len() <= storage::MAX_EVENT_BYTES && count < storage::MAX_SEQUENCE_EVENTS,
                "event or sequence exceeds v1 storage limits"
            );
            let id = storage::event_id(&wrapped)?;
            let digest = Sha256::digest(wrapped.as_bytes());
            match candidates.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(previous) => {
                    ensure!(*previous.get() == digest, "event identity collision")
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(digest);
                    let path = meta::event_path(&id)?;
                    if previous_ids.contains(&id) {
                        repo.git_cat_file_batch(vec![format!("{tip}:{path}")], storage::MAX_EVENT_BYTES, |_, kind, body| {
                            ensure!(kind == "blob" && matches!(body, crate::domain::repo::ObjectBody::Read(bytes) if bytes == wrapped.as_bytes()), "event identity collision");
                            Ok(())
                        })?;
                    } else {
                        files.insert(path, Some(wrapped.into_bytes()));
                    }
                }
            }
            count += 1;
            sequence.push_str(&id);
            sequence.push('\n');
        }
        snapshot.turn = Some(turns);
        snapshot.baseline_bytes = Some(end);
        files.insert(meta::LOG_FILE.into(), Some(sequence.as_bytes().to_vec()));
        files.insert(
            meta::VIEW_FILE.into(),
            Some(sequence.as_bytes()[view_start..].to_vec()),
        );
        files.insert(
            meta::FILE.into(),
            Some(meta::to_text(&snapshot)?.into_bytes()),
        );
        let tree =
            super::super::plumbing::tree_apply_owned(repo, &parent, files.into_iter().collect())?;
        parent = super::super::plumbing::commit_tree_authored(
            repo,
            &tree,
            &[&turn_parent],
            &message,
            (&author, &email),
            chunk.ended_at,
        )?;
        checkpoint = Some((end, source_hash.clone()));
        turn_bytes.clear();
        Ok(())
    })?;
    if let Some(saved) = saved {
        ensure!(
            end >= saved.bytes,
            "large Codex rollout is shorter than its saved checkpoint"
        );
        if checkpoint.is_none() {
            ui::success(&format!(
                "already saved on {branch}; no new completed turns"
            ));
            return Ok(ExitCode::Ok);
        }
    }
    let (bytes, prefix_hash) = checkpoint.context("large Codex import found no complete turn")?;
    super::super::plumbing::update_branch_cas_and_refresh(repo, branch, &parent, &tip, false)?;
    link.settled_at = Some(chrono::Utc::now().timestamp());
    link.native_checkpoint = Some(link::NativeCheckpoint {
        tip: parent,
        bytes,
        sha256: hex::encode(prefix_hash.finalize()),
    });
    link::write_settled(store, &link)?;
    ui::success(&format!(
        "settled {turns} turns ({count} records) on {branch}"
    ));
    Ok(ExitCode::Ok)
}

fn visit(link: &Link, emit: impl FnMut(&[u8]) -> crate::Result<()>) -> crate::Result<()> {
    #[cfg(feature = "rc")]
    if link.native_binding.is_some() {
        return link.visit_bound_rollout(emit);
    }
    ensure!(
        link.native_binding.is_none(),
        "bound Codex source is unavailable"
    );
    let path = link.resolve().context("cannot locate the Codex rollout")?;
    codex::lineage_stream::visit(
        &native_snapshot::source_at("codex", &link.session_id, path)?,
        emit,
    )
}
