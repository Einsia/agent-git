use super::*;

impl Entry {
    /// An ordinary ancestor can reenter delivery, but only its own remote receipt acknowledges it.
    pub(crate) fn prepare_retained_ancestor(
        repo: &Repo,
        request: &SupervisorPushRequest,
    ) -> Result<Option<Self>> {
        let original = Self::load(repo, request)?.context("publication intent is missing")?;
        if original.prepared.is_some() || original.publication.is_some() {
            return Ok(Some(original));
        }
        for confirmed in Self::records(
            repo,
            &request.branch,
            &original.capture.native_session_id,
            &original.capture.runtime,
        )?
        .into_iter()
        .rev()
        {
            let (Some(publication), Some(notification), Some(acknowledged)) = (
                confirmed.publication.as_ref(),
                confirmed.notification.as_ref(),
                confirmed.acknowledged.as_ref(),
            ) else {
                continue;
            };
            if publication.mode.is_encrypted()
                || confirmed.request.repository != request.repository
                || confirmed.request.destination != request.destination
                || confirmed.capture.session_id != original.capture.session_id
            {
                continue;
            }
            acknowledged.validate(notification)?;
            confirmed.request.verify(publication)?;
            if !publication.ancestors(repo)?.contains(&request.source) {
                continue;
            }
            let mut candidate = PublicationReceipt {
                source: request.source.clone(),
                published: request.source.clone(),
                projected_session_id: None,
                ..publication.clone()
            };
            let session = candidate.session_id(repo)?;
            ensure!(
                session == publication.session_id(repo)?,
                "retained publication ancestor belongs to another session"
            );
            candidate.projected_session_id = Some(session);
            request.verify(&candidate)?;

            // Traverse immutable Git objects outside the lease; recheck both records before writing.
            let (path, _lock) = locked_path(repo, request)?;
            let mut current =
                Self::load(repo, request)?.context("publication intent is missing")?;
            if current != original {
                return Ok(Some(current));
            }
            ensure!(
                Self::load(repo, &confirmed.request)?.as_ref() == Some(&confirmed),
                "confirming publication changed during recovery"
            );
            current.prepared = Some(candidate);
            current.write(&path)?;
            return Ok(Some(current));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{meta, privacy_receipt::PublicationMode};

    /// A later confirmed push revives an interrupted ancestor without inventing its acknowledgement.
    #[test]
    fn retained_ordinary_ancestor_requires_its_own_receiver_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(directory.path()).unwrap();
        repo.git(&["config", "user.name", "Recovery fixture"])
            .unwrap();
        repo.git(&["config", "user.email", "recovery@example.invalid"])
            .unwrap();
        repo.git(&["checkout", "-b", "s/work"]).unwrap();
        meta::write(
            repo.root(),
            &meta::Meta::new(
                format!("agit-{}", "a".repeat(40)),
                "codex".into(),
                String::new(),
            ),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("First captured turn").unwrap();
        let mut first = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            notification_id: None,
            repository: "owner/project".into(),
            branch: "s/work".into(),
            source: repo.git(&["rev-parse", "HEAD"]).unwrap(),
            destination: crate::hub::identity::RemoteIdentity::new(
                "https://hub.example",
                "00000000-0000-0000-0000-000000000001",
            )
            .unwrap(),
        };
        let capture = Capture {
            session_id: "logical".into(),
            native_session_id: "native".into(),
            runtime: "codex".into(),
            generation: 1,
            incarnation: Some("earlier-owner".into()),
            through_seq: Some(7),
        };
        let intent = Entry::begin(&repo, &mut first, capture.clone()).unwrap();
        fs::write(repo.root().join("turn.txt"), "Next captured turn").unwrap();
        repo.add_all().unwrap();
        repo.commit("Following captured turn").unwrap();
        let mut next = SupervisorPushRequest {
            notification_id: None,
            source: repo.git(&["rev-parse", "HEAD"]).unwrap(),
            ..first.clone()
        };
        Entry::begin(
            &repo,
            &mut next,
            Capture {
                incarnation: Some("replacement-owner".into()),
                ..capture.clone()
            },
        )
        .unwrap();
        let publication = PublicationReceipt {
            version: 2,
            mode: PublicationMode::Ordinary,
            repository: next.repository.clone(),
            branch: next.branch.clone(),
            source: next.source.clone(),
            published: next.source.clone(),
            projected_session_id: None,
            destination: next.destination.clone(),
            url: "https://hub.example/owner/project.git".into(),
            policy_digest: None,
            recipient: None,
        };
        Entry::complete(&repo, &next, &publication).unwrap();
        assert!(
            Entry::prepare_retained_ancestor(&repo, &first)
                .unwrap()
                .is_none()
        );
        let executor = Executor {
            owner: agit_peer::access::Principal {
                issuer: "https://hub.example".into(),
                account_id: "owner".into(),
            },
            device_id: "device".into(),
            credential_epoch: 1,
        };
        let receipt = |notification: &Notification| Receipt {
            version: 1,
            receipt_id: uuid::Uuid::new_v4().to_string(),
            notification_id: notification.notification_id.clone(),
            binding_digest: notification.digest().unwrap(),
            repository_id: notification.repository_id.clone(),
            public_commit: notification.public_commit.clone(),
        };
        let confirmed = Entry::bind_notification(&repo, &next, executor.clone()).unwrap();
        let newer_ack = receipt(&confirmed);
        Entry::acknowledge(&repo, &next, &newer_ack).unwrap();

        let mut other = SupervisorPushRequest {
            notification_id: None,
            destination: crate::hub::identity::RemoteIdentity::new(
                "https://hub.example",
                "00000000-0000-0000-0000-000000000002",
            )
            .unwrap(),
            ..first.clone()
        };
        Entry::begin(&repo, &mut other, capture.clone()).unwrap();
        assert!(
            Entry::prepare_retained_ancestor(&repo, &other)
                .unwrap()
                .is_none()
        );
        let recovered = Entry::prepare_retained_ancestor(&repo, &first)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.notification_id, intent.notification_id);
        assert_eq!(recovered.capture, capture);
        assert_eq!(recovered.prepared.as_ref().unwrap().published, first.source);
        assert!(recovered.publication.is_none() && recovered.acknowledged.is_none());
        assert_eq!(
            Entry::prepare_retained_ancestor(&repo, &first).unwrap(),
            Some(recovered)
        );
        assert!(Entry::acknowledge(&repo, &first, &newer_ack).is_err());
        let notification = Entry::bind_notification(&repo, &first, executor).unwrap();
        assert_eq!(notification.public_commit, first.source);
        Entry::acknowledge(&repo, &first, &receipt(&notification)).unwrap();
        assert!(
            Entry::load(&repo, &first)
                .unwrap()
                .unwrap()
                .acknowledged
                .is_some()
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), next.source);
    }
}
