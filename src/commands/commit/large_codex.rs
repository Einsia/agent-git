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
    let path = link.resolve().context("cannot locate the Codex rollout")?;
    let source = native_snapshot::source_at("codex", &link.session_id, path)?;
    ensure!(
        opts.milestone.is_none() && link.pending_milestone.is_none(),
        "large Codex import cannot assign a pending milestone"
    );
    if let Some(checkpoint) = &link.native_checkpoint {
        ensure!(
            checkpoint.tip == tip
                && snapshot.runtime == "codex"
                && snapshot.runtime_instances.contains(&link.session_id)
                && snapshot.baseline_bytes == Some(checkpoint.bytes),
            "large Codex checkpoint does not match the saved turn"
        );
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        codex::lineage_stream::visit(&source, |line| {
            bytes = bytes
                .checked_add(line.len() as u64)
                .context("rollout size overflow")?;
            ensure!(
                bytes <= checkpoint.bytes,
                "large Codex rollout extends beyond the saved checkpoint"
            );
            digest.update(line);
            Ok(())
        })?;
        ensure!(
            bytes == checkpoint.bytes && hex::encode(digest.finalize()) == checkpoint.sha256,
            "large Codex rollout differs from the saved checkpoint"
        );
        ui::success(&format!("already saved on {branch}; no new turns"));
        return Ok(ExitCode::Ok);
    }
    ensure!(
        snapshot.is_session_line()
            && snapshot.turn.is_none()
            && snapshot.runtime == "codex"
            && meta::is_bare_id(&snapshot.session),
        "large Codex import requires a fresh session line"
    );
    let (log, view) =
        storage::local_v1_sequences(repo.root(), &tip)?.context("expected a v1 session line")?;
    ensure!(
        log.is_empty()
            && view.is_empty()
            && repo
                .git(&[
                    "ls-tree",
                    &tip,
                    "--",
                    meta::EVENTS_DIR,
                    "privacy/envelope.json"
                ])?
                .is_empty(),
        "large Codex import cannot replace existing session evidence"
    );
    super::super::plumbing::ensure_v1_upgrade_preflight(repo, &tip)?;
    let (author, email) = commit_author(repo, owner)?;
    snapshot.committer_account = credentials::current_account()?;
    snapshot.kind = Kind::Turn;
    snapshot.runtime_instances = vec![link.session_id.clone()];
    snapshot.capture = opts.capture.then(|| meta::CaptureOrigin {
        runtime: "codex".into(),
        session: link.session_id.clone(),
    });
    snapshot.title =
        crate::adapter::native_title::native_title("codex", &link.session_id, "", None);
    let mut parent = tip.clone();
    let mut turn_parent = tip.clone();
    let mut message = String::new();
    let mut header = None;
    let mut turn_bytes = String::new();
    let mut sequence = String::new();
    let mut view_start = 0;
    let mut source_hash = Sha256::new();
    let mut end = 0u64;
    let mut checkpoint = None;
    let mut turns = 0u32;
    let mut count = 0usize;
    let mut candidates = std::collections::HashMap::new();
    codex::lineage_stream::visit(&source, |line| {
        ensure!(
            turn_bytes.len() + line.len() <= storage::MAX_EVENT_BYTES,
            "a Codex execution exceeds the in-memory turn budget"
        );
        let text = std::str::from_utf8(line)?;
        let record: serde_json::Value = serde_json::from_str(text)?;
        if header.is_none() {
            ensure!(
                record["type"] == "session_meta"
                    && record["payload"]["id"].as_str() == Some(&link.session_id),
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
        source_hash.update(line);
        end = end
            .checked_add(line.len() as u64)
            .context("rollout size overflow")?;
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
                    files.insert(meta::event_path(&id)?, Some(wrapped.into_bytes()));
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
