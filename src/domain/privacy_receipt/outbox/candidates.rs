use super::*;

impl Entry {
    /// A different generated candidate needs its own immutable notification and retained outcome.
    pub fn prepare_candidate(
        repo: &Repo,
        request: &SupervisorPushRequest,
        receipt: &PublicationReceipt,
    ) -> Result<SupervisorPushRequest> {
        request.verify(receipt)?;
        ensure!(
            request.notification_id.is_some(),
            "publication intent is not selected"
        );
        let (path, _lock) = locked_path(repo, request)?;
        let mut original = Self::load(repo, request)?.context("publication intent is missing")?;
        let existing = original.publication.as_ref().or(original.prepared.as_ref());
        if existing.is_none() {
            original.prepared = Some(receipt.clone());
            original.write(&path)?;
            return Ok(request.clone());
        }
        if existing == Some(receipt) {
            return Ok(request.clone());
        }
        for saved in Self::family(repo, request)? {
            if saved.capture == original.capture
                && saved.publication.as_ref().or(saved.prepared.as_ref()) == Some(receipt)
            {
                let mut selected = request.clone();
                selected.notification_id = Some(saved.notification_id);
                return Ok(selected);
            }
        }
        let directory = path.parent().context("publication directory is missing")?;
        check_capacity(directory)?;
        let mut selected = request.clone();
        let notification_id = uuid::Uuid::now_v7().to_string();
        selected.notification_id = Some(notification_id.clone());
        let candidate = Self {
            version: 2,
            notification_id,
            capture: original.capture,
            notification_source: None,
            request: selected.clone(),
            prepared: Some(receipt.clone()),
            publication: None,
            notification: None,
            acknowledged: None,
        };
        candidate.write(&candidate.record_path(directory)?)?;
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changed projections cannot overwrite uncertain evidence or allocate a new ID on each retry.
    #[test]
    fn publication_candidates_retain_distinct_outcomes_and_reuse_exact_bindings() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let mut request = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            notification_id: None,
            repository: "owner/project".into(),
            branch: "work".into(),
            source: "a".repeat(40),
            destination: crate::hub::identity::RemoteIdentity::new(
                "https://hub.invalid",
                "00000000-0000-0000-0000-000000000001",
            )
            .unwrap(),
        };
        let capture = Capture {
            session_id: "logical".into(),
            native_session_id: "native".into(),
            runtime: "codex".into(),
            generation: 1,
            incarnation: None,
            through_seq: None,
        };
        let mut original = Entry::begin(&repo, &mut request, capture.clone()).unwrap();
        // A legacy filename must remain addressable alongside independently retained candidates.
        let path = path(&repo, &request).unwrap();
        original.version = 1;
        let legacy_path = original.record_path(path.parent().unwrap()).unwrap();
        original.write(&legacy_path).unwrap();
        fs::remove_file(path).unwrap();
        let receipt = PublicationReceipt {
            version: 1,
            mode: Default::default(),
            repository: request.repository.clone(),
            branch: request.branch.clone(),
            source: request.source.clone(),
            published: "b".repeat(40),
            projected_session_id: Some(format!("agit-{}", "c".repeat(40))),
            destination: request.destination.clone(),
            url: "https://hub.invalid/owner/project.git".into(),
            policy_digest: Some("policy".into()),
            recipient: Some("recipient".into()),
        };
        assert_eq!(
            Entry::prepare_candidate(&repo, &request, &receipt).unwrap(),
            request
        );
        let before = fs::read(&legacy_path).unwrap();
        let changed = PublicationReceipt {
            published: "d".repeat(40),
            ..receipt.clone()
        };
        let selected = Entry::prepare_candidate(&repo, &request, &changed).unwrap();
        assert_ne!(selected.notification_id, request.notification_id);
        assert_eq!(fs::read(&legacy_path).unwrap(), before);
        assert_eq!(
            Entry::prepare_candidate(&repo, &request, &changed).unwrap(),
            selected
        );
        assert_eq!(
            Entry::prepare_candidate(&repo, &selected, &receipt)
                .unwrap()
                .notification_id,
            request.notification_id
        );
        let mut retry = request.clone();
        retry.notification_id = None;
        assert_eq!(
            Entry::begin(&repo, &mut retry, capture.clone())
                .unwrap()
                .notification_id,
            selected.notification_id.clone().unwrap()
        );
        Entry::complete(&repo, &selected, &changed).unwrap();
        assert!(Entry::complete(&repo, &request, &changed).is_err());
        retry.notification_id = None;
        let retained = Entry::begin(&repo, &mut retry, capture).unwrap();
        assert_eq!(retained.publication.as_ref(), Some(&changed));
        assert_eq!(
            retained.notification_id,
            selected.notification_id.clone().unwrap()
        );
        assert!(
            Entry::load(&repo, &request)
                .unwrap()
                .unwrap()
                .publication
                .is_none()
        );
        let pending = Entry::pending(&repo, "work", "native", "codex").unwrap();
        assert_eq!(pending.len(), 2);
        assert_ne!(pending[0].notification_id, pending[1].notification_id);
        let forged = SupervisorPushRequest {
            notification_id: Some(uuid::Uuid::new_v4().to_string()),
            ..request.clone()
        };
        assert!(Entry::prepare_candidate(&repo, &forged, &changed).is_err());
    }
}
