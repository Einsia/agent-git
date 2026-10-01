//! Resolve device-local placeholders before generating public and encrypted session views.

use super::*;
use crate::domain::{
    privacy::{projector::Mode, service},
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
        let recovered = service::manage(
            Some(repo.root()),
            super::super::privacy::management::Command {
                action: "used_values".into(),
                global: false,
                id: None,
                name: None,
                secret: Some(format!("{}\n{}", native.as_str(), source_metadata.as_str())),
            },
        );
        let protected_values = recovered
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let hydrated = service::transform(Some(repo.root()), &native, Mode::HydrateJsonl);
        service::hydrate_metadata(repo.root(), &mut metadata);
        let hydrated = Zeroizing::new(hydrated.content);
        PrivateLayer::check_session_size(hydrated.len(), 0)?;
        let contents = hydrated
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            contents.len() == records.len(),
            "hydration changed the session record count"
        );
        let mut restored = Zeroizing::new(String::new());
        let mut mapped = BTreeMap::new();
        let mut references = BTreeMap::new();
        for (mut record, mut content) in records.into_iter().zip(contents) {
            let source_line = storage::envelope_line(&record);
            let source_key = EvidenceReference::from_record(&record).key();
            if let Some(Ok(reference)) = content
                .get(EVIDENCE_FIELD)
                .map(|value| serde_json::from_value::<EvidenceReference>(value.clone()))
                && reference.version == 1
                && let Some(updated) = references.get(&reference.key())
            {
                content[EVIDENCE_FIELD] = serde_json::to_value(updated)?;
            }
            record.content = content;
            record.object_hash = transcript::object_hash(&record.content);
            references.insert(source_key, EvidenceReference::from_record(&record));
            let line = storage::envelope_line(&record);
            if let Some(previous) = mapped.insert(source_line, line.clone()) {
                ensure!(
                    previous == line,
                    "hydration has ambiguous evidence provenance"
                );
            }
            PrivateLayer::check_session_size(restored.len().saturating_add(line.len()), 0)?;
            restored.push_str(&line);
        }
        let mut restored_view = Zeroizing::new(String::new());
        for line in view.split_inclusive('\n') {
            let line = mapped.get(line).context("VIEW is outside the source LOG")?;
            PrivateLayer::check_session_size(
                restored.len(),
                restored_view.len().saturating_add(line.len()),
            )?;
            restored_view.push_str(line);
        }
        storage::snapshot_files(&restored, &restored_view)?;
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
        projection.private.validate()?;
        projection.source_log = self.source_log;
        Ok(projection)
    }
}
