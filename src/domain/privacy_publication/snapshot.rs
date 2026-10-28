//! Resolve device-local placeholders before generating public and encrypted session views.

use super::*;
use crate::domain::{
    privacy::{projector, service::PublicationHydrator},
    repo::Repo,
};

pub(crate) struct PublicationSnapshot {
    source_log: Zeroizing<String>,
    log: Zeroizing<String>,
    view: Zeroizing<String>,
    metadata: Meta,
    protected_values: BTreeSet<String>,
}

impl PublicationSnapshot {
    pub fn capture(repo: &Repo, log: &str, view: &str, metadata: &Meta) -> Result<Self> {
        check_input(log, view)?;
        storage::snapshot_files(log, view)?;
        meta::validate(metadata)?;
        let records = storage::parse_envelopes(log)?;
        let native = Zeroizing::new(transcript::unwrap_strict(log)?);
        let mut metadata = metadata.clone();
        let source_metadata = Zeroizing::new(serde_json::to_string(&metadata)?);
        let (hydrated, protected_values) = if projector::tokens(&native).next().is_some()
            || projector::tokens(&source_metadata).next().is_some()
        {
            let preserve_protocol_ids = records
                .iter()
                .any(|record| projector::has_wrapped_carrier(&record.content));
            let mut hydration = PublicationHydrator::new(repo.root(), preserve_protocol_ids)?;
            let hydrated = hydration.jsonl(&native)?;
            hydration.metadata(&mut metadata)?;
            (hydrated, hydration.used_values()?)
        } else {
            (native, BTreeSet::new())
        };
        PrivateLayer::check_session_size(hydrated.len(), 0)?;
        let contents = hydrated
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (restored, restored_view) = restore_events(records, contents, view)?;
        Ok(Self {
            source_log: Zeroizing::new(log.to_owned()),
            log: restored,
            view: restored_view,
            metadata,
            protected_values,
        })
    }

    pub fn project(
        self,
        policy: &PrivacyPolicy,
        aliases: &mut PathAliasStore,
        branch: Option<&str>,
        redactor: &Redactor,
    ) -> Result<SessionProjection> {
        let mut projection = project_session(
            policy,
            aliases,
            branch,
            &self.log,
            &self.view,
            &self.metadata,
            redactor,
        )?;
        projection
            .private
            .protected_values
            .extend(self.protected_values);
        projection.validate_private()?;
        projection.source_log = self.source_log;
        Ok(projection)
    }
}

fn restore_events(
    records: Vec<Envelope>,
    contents: Vec<Value>,
    view: &str,
) -> Result<(Zeroizing<String>, Zeroizing<String>)> {
    ensure!(
        records.len() == contents.len(),
        "hydration changed the session record count"
    );
    let mut unique = Vec::<Envelope>::new();
    let mut sources = BTreeMap::<String, usize>::new();
    let mut identities = BTreeMap::new();
    let mut sequence = Vec::new();
    for (mut record, content) in records.into_iter().zip(contents) {
        let source = storage::envelope_line(&record);
        if let Some(&index) = sources.get(&source) {
            ensure!(
                unique[index].content == content,
                "publication hydration is inconsistent for a repeated event"
            );
            sequence.push(index);
            continue;
        }
        let index = unique.len();
        ensure!(
            identities
                .insert(EvidenceReference::from_record(&record).key(), index)
                .is_none(),
            "publication has ambiguous source evidence identity"
        );
        sources.insert(source, index);
        sequence.push(index);
        record.content = content;
        unique.push(record);
    }
    let dependencies: Vec<_> = unique
        .iter()
        .map(|record| {
            record
                .content
                .get(EVIDENCE_FIELD)
                .and_then(|value| serde_json::from_value::<EvidenceReference>(value.clone()).ok())
                .filter(|reference| reference.version == 1)
                .and_then(|reference| identities.get(&reference.key()).copied())
        })
        .collect();
    let mut dependents = vec![Vec::new(); unique.len()];
    let mut ready = std::collections::VecDeque::new();
    for (index, dependency) in dependencies.iter().enumerate() {
        if let Some(dependency) = dependency {
            dependents[*dependency].push(index);
        } else {
            ready.push_back(index);
        }
    }
    let mut completed = 0;
    while let Some(index) = ready.pop_front() {
        if let Some(dependency) = dependencies[index] {
            unique[index].content[EVIDENCE_FIELD] =
                serde_json::to_value(EvidenceReference::from_record(&unique[dependency]))?;
        }
        unique[index].object_hash = transcript::object_hash(&unique[index].content);
        completed += 1;
        ready.extend(dependents[index].iter().copied());
    }
    ensure!(
        completed == unique.len(),
        "publication evidence references contain a cycle"
    );
    let lines: Vec<_> = unique.iter().map(storage::envelope_line).collect();
    let mut restored = Zeroizing::new(String::new());
    for index in sequence {
        PrivateLayer::check_session_size(restored.len().saturating_add(lines[index].len()), 0)?;
        restored.push_str(&lines[index]);
    }
    let mut restored_view = Zeroizing::new(String::new());
    for line in view.split_inclusive('\n') {
        let index = sources
            .get(line)
            .context("VIEW is outside the source LOG")?;
        let line = &lines[*index];
        PrivateLayer::check_session_size(
            restored.len(),
            restored_view.len().saturating_add(line.len()),
        )?;
        restored_view.push_str(line);
    }
    storage::snapshot_files(&restored, &restored_view)?;
    Ok((restored, restored_view))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repeated quotes resolve transitive evidence even when their sources appear later in LOG.
    #[test]
    fn repeated_forward_evidence_keeps_view_identity_after_hydration() {
        let session = format!("agit-{}", "a".repeat(40));
        let original = json!({"type":"user","message":{"role":"user","content":"protected"}});
        let evidence = json!({"version":1,"source":"claude-code","session":session,"object_hash":transcript::object_hash(&original)});
        let quote = json!({"type":"user","message":{"role":"user","content":"quotation"},EVIDENCE_FIELD:evidence});
        let outer = json!({"type":"user","message":{"role":"user","content":"outer quotation"},EVIDENCE_FIELD:{"version":1,"source":"claude-code","session":session,"object_hash":transcript::object_hash(&quote)}});
        let outer_line = transcript::wrap_lines(&outer.to_string(), "claude-code", &session);
        let quoted = transcript::wrap_lines(&quote.to_string(), "claude-code", &session);
        let source = transcript::wrap_lines(&original.to_string(), "claude-code", &session);
        let log = format!("{outer_line}{quoted}{source}{quoted}{outer_line}");
        let view = format!("{outer_line}{quoted}{outer_line}");
        let hydrated = json!({"type":"user","message":{"role":"user","content":"restored"}});
        let expected = transcript::object_hash(&hydrated);
        let (restored, view) = restore_events(
            storage::parse_envelopes(&log).unwrap(),
            vec![outer.clone(), quote.clone(), hydrated, quote, outer],
            &view,
        )
        .unwrap();
        let records = storage::parse_envelopes(&restored).unwrap();
        assert_eq!(records[1].content[EVIDENCE_FIELD]["object_hash"], expected);
        assert_eq!(
            records[0].content[EVIDENCE_FIELD]["object_hash"],
            records[1].object_hash
        );
        assert_eq!(
            storage::envelope_line(&records[0]),
            storage::envelope_line(&records[4])
        );
        assert_eq!(
            storage::envelope_line(&records[1]),
            storage::envelope_line(&records[3])
        );
        assert_eq!(
            *view,
            format!(
                "{}{}{}",
                storage::envelope_line(&records[0]),
                storage::envelope_line(&records[1]),
                storage::envelope_line(&records[0])
            )
        );
        storage::snapshot_files(&restored, &view).unwrap();
    }
}
