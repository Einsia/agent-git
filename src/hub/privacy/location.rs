//! Source routes are hints; immutable repository identity survives copies and renames.

use crate::hub::{Client, client::ApiError, identity::RemoteIdentity};
use anyhow::{Result, ensure};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
#[error("source repository is unavailable or inaccessible")]
pub(crate) struct SourceUnavailable;

#[derive(Debug, thiserror::Error)]
#[error("privacy status belongs to another repository")]
pub(super) struct RepositoryMismatch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Location {
    agent_id: String,
    owner: String,
    name: String,
    clone_url: String,
}

impl Client {
    pub(crate) fn repository_location(&self, identity: &RemoteIdentity) -> Result<String> {
        ensure!(
            RemoteIdentity::new(&identity.hub, &identity.agent_id)? == *identity
                && crate::hub::identity::normalize_hub(self.base())? == identity.hub,
            "repository location request belongs to another Hub"
        );
        let location: Location = self
            .get_expected(
                &format!("api/repositories/{}/location", identity.agent_id),
                Some(&identity.agent_id),
            )
            .map_err(|error| {
                if error
                    .downcast_ref::<ApiError>()
                    .is_some_and(|api| api.status == 404)
                {
                    error.context(SourceUnavailable)
                } else {
                    error
                }
            })?;
        let repository = format!("{}/{}", location.owner, location.name);
        ensure!(
            [&location.owner, &location.name].into_iter().all(|part| {
                crate::domain::privacy_envelope::valid_token(part) && part != "." && part != ".."
            }),
            "repository location has an invalid owner or name"
        );
        ensure!(
            location.agent_id == identity.agent_id
                && crate::hub::identity::normalize_hub(&location.clone_url)?
                    == format!("{}/{repository}.git", identity.hub),
            "repository location differs from the requested source identity or Hub"
        );
        Ok(repository)
    }

    /// A rejected route may be resolved once; the operation keeps its exact authority and body.
    pub(super) fn with_repository_route<T>(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        mut request: impl FnMut(&str) -> Result<T>,
    ) -> Result<T> {
        match request(repository) {
            Err(error)
                if error.is::<RepositoryMismatch>()
                    || error.downcast_ref::<ApiError>().is_some_and(|api| {
                        api.status == 412
                            || (api.status == 404 && api.kind != "session_privacy_unregistered")
                    }) =>
            {
                let current = self.repository_location(identity)?;
                if current == repository {
                    return Err(error);
                }
                request(&current)
            }
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::privacy::test_server::server_with_headers;
    use serde_json::json;

    /// Location metadata cannot redirect source authority, and only an explicit absence permits fallback.
    #[test]
    fn location_preserves_hub_mount_identity_and_error_classification() {
        let (anonymous, identity, worker) = server_with_headers(
            |hub| {
                let valid = json!({"agent_id":"00000000-0000-0000-0000-000000000001",
                "owner":"alice","name":"renamed","clone_url":format!("{hub}/Mount/alice/renamed.git")});
                let mut responses = vec![(200, valid.clone())];
                for (field, value) in [
                    ("agent_id", "00000000-0000-0000-0000-000000000002"),
                    ("clone_url", "https://other.invalid/Mount/alice/renamed.git"),
                    ("clone_url", &format!("{hub}/alice/renamed.git")),
                    ("owner", "bob"),
                    ("name", "../renamed"),
                ] {
                    let mut changed = valid.clone();
                    changed[field] = json!(value);
                    responses.push((200, changed));
                }
                for status in [404, 503, 401, 307] {
                    responses.push((
                        status,
                        json!({"kind":"synthetic","error":"location unavailable"}),
                    ));
                }
                responses
            },
            "Cache-Control: private, no-store\r\nLocation: http://127.0.0.1:1/foreign\r\n",
        );
        let client =
            Client::for_hub_with_token(&format!("{}/Mount", anonymous.base()), "synthetic-reader");
        let identity = RemoteIdentity::new(client.base(), &identity.agent_id).unwrap();
        assert_eq!(
            client.repository_location(&identity).unwrap(),
            "alice/renamed"
        );
        for _ in 0..5 {
            assert!(client.repository_location(&identity).is_err());
        }
        for status in [404, 503, 401, 307] {
            let error = client.repository_location(&identity).unwrap_err();
            assert_eq!(error.downcast_ref::<ApiError>().unwrap().status, status);
            assert_eq!(error.is::<SourceUnavailable>(), status == 404);
        }
        for request in worker.join().unwrap() {
            assert!(request.starts_with(&format!(
                "GET /Mount/api/repositories/{}/location ",
                identity.agent_id
            )));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("cache-control: no-store")
            );
            assert!(request.contains("Bearer synthetic-reader"));
        }
    }
}
