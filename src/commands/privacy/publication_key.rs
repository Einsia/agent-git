//! Shared artifacts select a reading key through a verified accepted repository publication.

use crate::{
    domain::{privacy_envelope::ViewingRecipient, privacy_git, repo::Repo},
    hub::{
        Client,
        identity::{self, RemoteIdentity},
    },
};
use anyhow::{Context, Result, ensure};

pub(crate) struct PublicationKey {
    client: Client,
    identity: RemoteIdentity,
    repository: String,
    commit: String,
    recipient: String,
    session: String,
    pub public_key: String,
}

impl PublicationKey {
    pub fn source(&self) -> String {
        format!("{} {}@{}", self.identity.hub, self.repository, self.commit)
    }

    pub fn resolve(repo: &Repo, branch: Option<&str>, commit: &str) -> Result<Self> {
        let identity = identity::read(repo)?.context("default repository encryption requires an accepted publication; publish the snapshot first or use an explicit export public key")?;
        let repository = if let Some(destination) =
            crate::rc::local_repository::publication::Destination::load(repo)?
        {
            ensure!(
                destination.identity == identity,
                "publication destination identity changed"
            );
            destination.repository
        } else {
            let (hub, repository) = super::sources::remote_scope(
                &repo
                    .remote_url()
                    .context("repository has no publication remote")?,
            )?;
            ensure!(
                hub == identity.hub,
                "publication remote belongs to another Hub"
            );
            repository
        };
        let (commit, envelope) = privacy_git::accepted_envelope(repo, branch, commit, &identity)?;
        ensure!(
            envelope.private_payload.wrapped_keys.len() == 1,
            "repository encryption requires one recipient"
        );
        let recipient = envelope.private_payload.wrapped_keys[0].recipient.clone();
        let session = envelope
            .public_projection
            .pointer("/metadata/session")
            .and_then(serde_json::Value::as_str)
            .context("accepted publication has no session identity")?
            .to_owned();
        let client = Client::for_stored_hub(&identity.hub);
        let record = client.publication_unlock_key(&repository, &identity, &commit, &recipient)?;
        ensure!(
            record.session_id == session,
            "accepted publication key belongs to another session"
        );
        Ok(Self {
            client,
            identity,
            repository,
            commit,
            recipient,
            session,
            public_key: record.key.key.public_key,
        })
    }

    pub fn recipient(&self) -> Result<ViewingRecipient> {
        ViewingRecipient::from_base64(self.recipient.clone(), &self.public_key)
    }

    pub fn verify(&self, repo: &Repo) -> Result<()> {
        ensure!(
            identity::read(repo)?.as_ref() == Some(&self.identity),
            "repository identity changed during encryption"
        );
        let current = self.client.publication_unlock_key(
            &self.repository,
            &self.identity,
            &self.commit,
            &self.recipient,
        )?;
        ensure!(
            current.key.key.public_key == self.public_key && current.session_id == self.session,
            "repository viewing recipient changed during preparation; review a fresh artifact"
        );
        Ok(())
    }
}
