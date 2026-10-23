//! Account keys and session registration retain immutable authority across every request.

use crate::domain::privacy_envelope::{ContentKeyResponse, ViewingRecipient};
use crate::domain::session_privacy::{
    AcceptedSnapshot, AccountIdentity, OwnerKeyBinding, SessionOwner, SessionPrivacy,
    SessionPrivacyConfig, SessionScheme, canonical_hub,
};
use crate::hub::{Client, identity::RemoteIdentity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSessionKey {
    pub account_id: String,
    pub key_version: u64,
    pub public_key_algorithm: String,
    pub public_key: String,
}

impl AccountSessionKey {
    pub fn binding(&self) -> OwnerKeyBinding {
        OwnerKeyBinding {
            account_id: self.account_id.clone(),
            key_version: self.key_version,
        }
    }

    pub fn viewing_recipient(&self) -> Result<ViewingRecipient> {
        ensure!(
            self.public_key_algorithm == "x25519",
            "unsupported session public-key algorithm"
        );
        ViewingRecipient::from_base64(self.binding().recipient_id()?, &self.public_key)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKeyReadiness {
    pub ready: bool,
    pub key: AccountSessionKey,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPrivacyResponse {
    pub config: SessionPrivacyConfig,
    pub owner_key: Option<AccountSessionKey>,
}

impl SessionPrivacyResponse {
    pub fn require_owner_key(&self) -> Result<&AccountSessionKey> {
        ensure!(
            self.config.privacy.scheme == SessionScheme::SessionOwnerV2,
            "plain sessions have no encryption recipient"
        );
        let key = self.owner_key.as_ref().context(
            "session owner public key is unavailable; encrypted publication cannot proceed",
        )?;
        self.validate_owner_key()?;
        Ok(key)
    }

    fn validate_owner_key(&self) -> Result<()> {
        self.config.validate()?;
        if let Some(key) = &self.owner_key {
            ensure!(
                self.config.privacy.scheme == SessionScheme::SessionOwnerV2
                    && self.config.privacy.owner
                        == Some(SessionOwner::User(key.account_id.clone())),
                "public key does not belong to the registered session owner"
            );
            key.viewing_recipient()?;
        }
        Ok(())
    }

    fn verify(&self, identity: &RemoteIdentity, session: &str) -> Result<()> {
        self.validate_owner_key()?;
        ensure!(
            self.config.repository_id == identity.agent_id && self.config.session_id == session,
            "privacy configuration belongs to another repository or session"
        );
        ensure!(
            self.config
                .privacy
                .hub
                .as_deref()
                .is_none_or(|hub| hub == identity.hub),
            "privacy configuration belongs to another Hub"
        );
        Ok(())
    }
}

#[derive(Serialize)]
struct Registration<'a> {
    expected_repository_id: &'a str,
    privacy: &'a SessionPrivacy,
}

impl Client {
    /// Identity is read from this client's credentials, never a concurrently switched account.
    pub fn session_account(&self) -> Result<AccountIdentity> {
        ensure!(
            self.checked_access_token()?.is_some(),
            "sign in before managing session publication"
        );
        let account = match self
            .credential_snapshot()
            .and_then(|credential| credential.account_id)
        {
            Some(account) => account,
            None => self
                .me()?
                .account_id
                .context("Hub does not provide immutable account identities")?,
        };
        AccountIdentity::new(self.base(), &account)
    }

    pub fn ensure_session_key(&self, expected: &AccountIdentity) -> Result<SessionKeyReadiness> {
        expected.validate()?;
        ensure!(
            canonical_hub(self.base())? == expected.hub && self.has_token(),
            "session key requires the selected authenticated account"
        );
        let response: SessionKeyReadiness = self.post(
            "api/me/session-key/ensure",
            &serde_json::json!({"expected_account_id": expected.account_id}),
        )?;
        ensure!(
            response.key.account_id == expected.account_id,
            "session key response belongs to another account"
        );
        ensure!(response.ready, "account session key is not ready");
        response.key.viewing_recipient()?;
        Ok(response)
    }

    pub fn session_privacy_config(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
    ) -> Result<SessionPrivacyResponse> {
        let response: SessionPrivacyResponse =
            self.with_repository_route(repository, identity, |repository| {
                let path =
                    self.session_privacy_path(repository, identity, session, "privacy-config")?;
                self.get_expected(&path, Some(&identity.agent_id))
            })?;
        response.verify(identity, session)?;
        Ok(response)
    }

    /// Only the protocol's explicit absence response permits a new registration.
    pub fn registered_session_privacy(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
    ) -> Result<Option<SessionPrivacyResponse>> {
        match self.session_privacy_config(repository, identity, session) {
            Ok(response) => Ok(Some(response)),
            Err(error)
                if error
                    .downcast_ref::<crate::hub::client::ApiError>()
                    .is_some_and(|api| {
                        api.status == 404 && api.kind == "session_privacy_unregistered"
                    }) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn register_session_privacy(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
        local: &SessionPrivacy,
    ) -> Result<SessionPrivacyResponse> {
        local.validate()?;
        ensure!(
            local.hub.as_deref().is_none_or(|hub| hub == identity.hub),
            "session creator belongs to another Hub"
        );
        let response: SessionPrivacyResponse =
            self.with_repository_route(repository, identity, |repository| {
                let path =
                    self.session_privacy_path(repository, identity, session, "privacy-config")?;
                self.post_expected(
                    &path,
                    &Registration {
                        expected_repository_id: &identity.agent_id,
                        privacy: local,
                    },
                    Some(&identity.agent_id),
                )
            })?;
        response.verify(identity, session)?;
        response
            .config
            .verify_registration(&identity.agent_id, session, local)?;
        Ok(response)
    }

    pub fn session_original_key(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        expected: &AcceptedSnapshot,
    ) -> Result<ContentKeyResponse> {
        expected.validate()?;
        ensure!(
            expected.binding.repository_id == identity.agent_id,
            "original-key request belongs to another repository"
        );
        let response: ContentKeyResponse =
            self.with_repository_route(repository, identity, |repository| {
                let path = self.session_privacy_path(
                    repository,
                    identity,
                    &expected.binding.session_id,
                    "original-key",
                )?;
                self.post_private(
                    &path,
                    &serde_json::json!({"snapshot": expected}),
                    &identity.agent_id,
                )
            })?;
        ensure!(
            response.snapshot == *expected,
            "content-key response differs from the requested snapshot"
        );
        Ok(response)
    }

    pub(super) fn session_privacy_path(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
        operation: &str,
    ) -> Result<String> {
        ensure!(
            self.has_token(),
            "session privacy requires a signed-in account"
        );
        self.session_read_path(repository, identity, session, operation)
    }

    pub(super) fn session_read_path(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
        operation: &str,
    ) -> Result<String> {
        ensure!(
            RemoteIdentity::new(&identity.hub, &identity.agent_id)? == *identity
                && canonical_hub(self.base())? == identity.hub,
            "session privacy request belongs to another Hub"
        );
        ensure!(
            crate::domain::meta::is_bare_id(session),
            "session privacy requires an immutable session identity"
        );
        let root = super::repository_path(repository)?;
        let root = root
            .strip_suffix("/privacy")
            .expect("repository privacy path");
        Ok(format!("{root}/sessions/{session}/{operation}"))
    }
}

#[cfg(test)]
mod tests;
