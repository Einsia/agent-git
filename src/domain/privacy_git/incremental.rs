//! Authenticated local preparation records bind a source to its checked public commit.

use super::*;
use crate::domain::privacy_publication::ProjectionDependencies;
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    pub ciphertext_key: String,
    pub projected: String,
    pub dependencies: ProjectionDependencies,
}

pub(super) struct Index {
    directory: std::path::PathBuf,
    scope: String,
    entries: BTreeMap<String, Entry>,
}

impl Index {
    pub fn open(repo: &Repo, scope: &str) -> Result<Self> {
        let directory = repo.root().join(".git/privacy-preparation");
        let mut index = Self {
            directory,
            scope: scope.into(),
            entries: BTreeMap::new(),
        };
        let Some(plain) = super::state::load(&index.directory, scope)? else {
            return Ok(index);
        };
        let (version, build, entries): (u32, String, BTreeMap<String, Entry>) =
            serde_json::from_slice(&plain).context("invalid incremental privacy cache")?;
        ensure!(
            version == 1,
            "unsupported incremental privacy cache version"
        );
        if build == env!("AGIT_BUILD_ID") {
            index.entries = entries;
        }
        Ok(index)
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    pub fn insert(&mut self, key: String, value: Entry) {
        self.entries.insert(key, value);
    }

    pub fn save(&self) -> Result<()> {
        let plain = Zeroizing::new(serde_json::to_vec(&(
            1,
            env!("AGIT_BUILD_ID"),
            &self.entries,
        ))?);
        super::state::save(&self.directory, &self.scope, &plain)
    }
}

pub(super) fn dependencies(source: &Repo, redactor: &Redactor) -> Result<String> {
    digest_json(&json!({
        "build": env!("AGIT_BUILD_ID"),
        "redactor": redactor.publication_fingerprint()?,
        "hydrator": crate::domain::secret_filter::RepositoryDictionary::open(source.root())?.publication_fingerprint()?,
    }))
}
