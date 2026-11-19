//! A scoped observation proves only requested names and stays bound to one transport reader.

use crate::Result;
use anyhow::{Context, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const HEADER: &str = "X-AgentGit-Fetch-Scope";
const MAX_HEADER_BYTES: usize = 24 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("the requested branch or version is unavailable on the Hub")]
pub(crate) struct Unavailable;

pub(super) struct Selection {
    required: String,
    names: Vec<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionRoot {
    commit: String,
    reference: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionResponse {
    version: u32,
    repository_id: String,
    reader_context: String,
    prefix: String,
    #[serde(deserialize_with = "explicit_selection")]
    selection: Option<VersionRoot>,
}

fn explicit_selection<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<VersionRoot>, D::Error> {
    Option::<VersionRoot>::deserialize(deserializer)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    version: u32,
    repository_id: String,
    reader_context: String,
    refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    name: String,
    #[serde(deserialize_with = "explicit_oid")]
    oid: Option<String>,
}

fn explicit_oid<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    scope: Scope,
    refs: Vec<Observation>,
}

#[derive(Debug, PartialEq, Eq)]
enum RefObservation<'a> {
    Present(&'a str),
    Unavailable,
    Unknown,
}

struct ScopedRefs {
    scope: Scope,
    refs: BTreeMap<String, Option<String>>,
}

impl ScopedRefs {
    fn observation(&self, name: &str) -> RefObservation<'_> {
        match self.refs.get(name) {
            Some(Some(oid)) => RefObservation::Present(oid),
            Some(None) => RefObservation::Unavailable,
            None => RefObservation::Unknown,
        }
    }
}

impl Selection {
    pub(super) fn branch(branch: &str) -> Result<Self> {
        let required = format!("refs/heads/{branch}");
        ensure!(
            valid_name(&required),
            "invalid branch name for scoped fetch"
        );
        let mut names = vec!["refs/heads/main".to_owned()];
        if required != names[0] {
            names.push(required.clone());
        }
        Ok(Self {
            required,
            names,
            version: None,
        })
    }

    pub(super) fn version(
        client: &crate::hub::Client,
        url: &str,
        repository_id: &str,
        version: &str,
    ) -> Result<Self> {
        let prefix = version
            .strip_prefix("agit-")
            .context("invalid version ID")?;
        ensure!(
            (7..=64).contains(&prefix.len())
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid version ID"
        );
        let (root, _) = Self::resolve_version(client, url, repository_id, prefix)?;
        Ok(Self {
            required: root.reference.clone(),
            names: vec![root.reference],
            version: Some(root.commit),
        })
    }

    fn resolve_version(
        client: &crate::hub::Client,
        url: &str,
        repository_id: &str,
        prefix: &str,
    ) -> Result<(VersionRoot, String)> {
        let response: VersionResponse = client.post_expected(
            &Self::endpoint(client, url, "agit-version")?,
            &serde_json::json!({"version":1,"repository_id":repository_id,"prefix":prefix}),
            Some(repository_id),
        )?;
        ensure!(
            response.version == 1
                && response.repository_id == repository_id
                && response.prefix == prefix
                && response.reader_context.len() == 64
                && response
                    .reader_context
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "the Hub returned an invalid version lookup"
        );
        let root = response.selection.ok_or(Unavailable)?;
        ensure!(
            matches!(root.commit.len(), 40 | 64)
                && root.commit.starts_with(prefix)
                && root
                    .commit
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && (root.reference.starts_with("refs/heads/") && valid_name(&root.reference)
                    || root.reference == format!("refs/tags/agit-{}", root.commit)),
            "the Hub returned an invalid version fetch root"
        );
        Ok((root, response.reader_context))
    }

    fn endpoint(client: &crate::hub::Client, url: &str, suffix: &str) -> Result<String> {
        let prefix = format!("{}/", client.base().trim_end_matches('/'));
        let path = url
            .strip_prefix(&prefix)
            .context("the scoped Git destination does not belong to this Hub")?;
        Ok(format!("{}/{suffix}", path.trim_end_matches('/')))
    }

    pub(super) fn clone_name(&self) -> &str {
        self.required
            .strip_prefix("refs/heads/")
            .or_else(|| self.required.strip_prefix("refs/tags/"))
            .expect("validated fetch root")
    }

    fn local_root(&self) -> String {
        self.required.strip_prefix("refs/heads/").map_or_else(
            || self.required.clone(),
            |branch| format!("refs/remotes/origin/{branch}"),
        )
    }

    pub(super) fn refspec(&self) -> String {
        let force = if self.required.starts_with("refs/heads/") {
            "+"
        } else {
            ""
        };
        format!("{force}{}:{}", self.required, self.local_root())
    }

    /// A local object alone is insufficient; the root this transfer fetched must still carry it.
    pub(super) fn verify_version(&self, repo: &crate::domain::repo::Repo) -> Result<String> {
        let commit = self
            .version
            .as_ref()
            .context("version transfer has no pinned commit")?;
        let repo = repo.clone().local_objects_only();
        let tip = repo.git(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{}^{{commit}}", self.local_root()),
        ])?;
        ensure!(
            repo.git(&["cat-file", "-t", commit])? == "commit",
            "the requested version is not a commit"
        );
        if self.required.starts_with("refs/tags/") {
            ensure!(tip == *commit, "the fetched version tag changed its target");
        }
        repo.git(&["merge-base", "--is-ancestor", commit, &tip])
            .context("the fetched root no longer contains the requested version")?;
        Ok(commit.clone())
    }

    fn validate(&self, repository_id: &str, response: Response) -> Result<ScopedRefs> {
        let scope = response.scope;
        ensure!(
            scope.version == 1
                && scope.repository_id == repository_id
                && scope.refs == self.names
                && scope.reader_context.len() == 64
                && scope
                    .reader_context
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && response.refs.len() == self.names.len(),
            "the Hub returned an invalid Git reference scope"
        );
        let mut refs = BTreeMap::new();
        for observation in response.refs {
            ensure!(
                self.names.contains(&observation.name)
                    && observation.oid.as_ref().is_none_or(|oid| {
                        matches!(oid.len(), 40 | 64)
                            && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    && !refs.contains_key(&observation.name),
                "the Hub returned an incomplete or invalid Git reference observation"
            );
            refs.insert(observation.name, observation.oid);
        }
        Ok(ScopedRefs { scope, refs })
    }

    /// Rediscovery runs before each Git attempt, so refreshed credentials never reuse an old scope.
    pub(super) fn header(
        &self,
        client: &crate::hub::Client,
        url: &str,
        repository_id: &str,
    ) -> Result<String> {
        let context = self
            .version
            .as_ref()
            .map(|commit| {
                let (current, context) = Self::resolve_version(client, url, repository_id, commit)?;
                ensure!(
                    current.commit == *commit && current.reference == self.required,
                    "the version's fetch root changed; retry the operation"
                );
                Ok::<_, anyhow::Error>(context)
            })
            .transpose()?;
        let response = client.post_expected(
            &Self::endpoint(client, url, "agit-refs")?,
            &serde_json::json!({
                "version": 1,
                "repository_id": repository_id,
                "reader_context": null,
                "refs": self.names,
            }),
            Some(repository_id),
        )?;
        let observed = self.validate(repository_id, response)?;
        ensure!(
            context
                .as_ref()
                .is_none_or(|context| context == &observed.scope.reader_context),
            "the version lookup's reader changed before transfer"
        );
        if self.required.starts_with("refs/tags/")
            && let Some(commit) = &self.version
        {
            ensure!(
                observed.observation(&self.required) == RefObservation::Present(commit),
                Unavailable
            );
        }
        ensure!(
            matches!(
                observed.observation(&self.required),
                RefObservation::Present(_)
            ),
            Unavailable
        );
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&observed.scope)?);
        ensure!(
            encoded.len() <= MAX_HEADER_BYTES,
            "the Git reference scope exceeds its header budget"
        );
        Ok(encoded)
    }
}

fn valid_name(name: &str) -> bool {
    let Some(short) = name.strip_prefix("refs/heads/") else {
        return false;
    };
    name.len() <= 1024
        && !short.is_empty()
        && !name.contains("..")
        && !name.contains("@{")
        && !name
            .bytes()
            .any(|byte| byte <= 32 || byte == 127 || b"~^:?*[\\".contains(&byte))
        && short.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Partial, substituted and malformed responses cannot turn unknown names into absence.
    #[test]
    fn scoped_observations_require_complete_identity_bound_answers() {
        let selection = Selection::branch("selected").unwrap();
        let valid = serde_json::json!({
            "scope": {"version":1,"repository_id":"repo","reader_context":"a".repeat(64),
                      "refs":["refs/heads/main","refs/heads/selected"]},
            "refs":[{"name":"refs/heads/main","oid":null},
                    {"name":"refs/heads/selected","oid":"b".repeat(40)}]
        });
        let observed = selection
            .validate("repo", serde_json::from_value(valid.clone()).unwrap())
            .unwrap();
        assert_eq!(
            observed.observation("refs/heads/main"),
            RefObservation::Unavailable
        );
        assert_eq!(
            observed.observation("refs/heads/unrequested"),
            RefObservation::Unknown
        );
        for change in [
            "repository",
            "scope",
            "missing",
            "duplicate",
            "extra",
            "oid",
            "context",
        ] {
            let mut value = valid.clone();
            match change {
                "repository" => value["scope"]["repository_id"] = "other".into(),
                "scope" => value["scope"]["refs"][0] = "refs/heads/other".into(),
                "missing" => {
                    value["refs"].as_array_mut().unwrap().pop();
                }
                "duplicate" => value["refs"][0] = value["refs"][1].clone(),
                "extra" => value["refs"][0]["name"] = "refs/heads/other".into(),
                "oid" => value["refs"][1]["oid"] = "not-an-object".into(),
                "context" => value["scope"]["reader_context"] = "unknown".into(),
                _ => unreachable!(),
            }
            assert!(
                selection
                    .validate("repo", serde_json::from_value(value).unwrap())
                    .is_err(),
                "{change}"
            );
        }
        let mut missing = valid;
        missing["refs"][0].as_object_mut().unwrap().remove("oid");
        assert!(serde_json::from_value::<Response>(missing).is_err());
    }
}
