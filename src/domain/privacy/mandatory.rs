//! Device-managed exclusions narrow every repository's candidate scope.

use super::repository::{ensure_valid_label, validate_patterns};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_SOURCES: usize = 32;
const MAX_PATTERNS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MandatoryPolicy {
    pub version: u32,
    pub id: String,
    pub revision: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub memory_exclude: Vec<String>,
}

impl MandatoryPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported mandatory privacy policy version"
        );
        ensure_valid_label(&self.id).context("invalid mandatory policy ID")?;
        ensure!(
            !self.revision.is_empty()
                && self.revision.len() <= 256
                && !self.revision.chars().any(char::is_control),
            "invalid mandatory policy revision"
        );
        ensure!(
            self.exclude.len() + self.memory_exclude.len() <= MAX_PATTERNS,
            "mandatory policy exceeds its pattern limit"
        );
        for patterns in [&self.exclude, &self.memory_exclude] {
            validate_patterns(patterns)?;
            ensure!(
                patterns.iter().all(|pattern| pattern.len() <= 1024),
                "mandatory policy pattern exceeds its length limit"
            );
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sources {
    version: u32,
    sources: Vec<PathBuf>,
}

pub(crate) fn load() -> Result<Vec<MandatoryPolicy>> {
    let (system, manifest) = crate::infra::config::privacy_policy_sources()?;
    load_at(&system, &manifest)
}

fn load_at(system: &Path, manifest: &Path) -> Result<Vec<MandatoryPolicy>> {
    let mut policies = Vec::new();
    if let Some(bytes) = read_optional(system)? {
        policies.push(parse(&bytes, system)?);
    }
    if let Some(bytes) = read_optional(manifest)? {
        let sources: Sources = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid privacy source manifest {}", manifest.display()))?;
        ensure!(
            sources.version == 1,
            "unsupported privacy source manifest version"
        );
        ensure!(
            sources.sources.len() <= MAX_SOURCES,
            "too many mandatory privacy sources"
        );
        for path in sources.sources {
            ensure!(
                path.is_absolute()
                    && path
                        .to_str()
                        .is_some_and(|value| !value.chars().any(char::is_control)),
                "mandatory privacy sources must be absolute local paths"
            );
            let bytes = read_optional(&path)?.with_context(|| {
                format!("required privacy source is missing: {}", path.display())
            })?;
            policies.push(parse(&bytes, &path)?);
        }
    }
    let mut ids = BTreeSet::new();
    for source in &policies {
        ensure!(
            ids.insert(&source.id),
            "duplicate mandatory privacy policy ID {}",
            source.id
        );
    }
    Ok(policies)
}

fn parse(bytes: &[u8], path: &Path) -> Result<MandatoryPolicy> {
    let policy: MandatoryPolicy = serde_json::from_slice(bytes)
        .with_context(|| format!("invalid mandatory privacy policy {}", path.display()))?;
    policy.validate()?;
    Ok(policy)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "privacy source is not a regular file: {}",
            path.display()
        ),
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SOURCE_BYTES as u64,
        "privacy source is not a bounded regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_SOURCE_BYTES,
        "privacy source exceeds its byte limit"
    );
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        privacy::{CandidateAction, PrivacyPolicy},
        repo::Repo,
    };
    use serde_json::json;

    /// Repository edits cannot grant access denied by a loaded source or persist that source
    /// as repository authority. Revisions bind consent even when the exclusion text is unchanged.
    #[test]
    fn mandatory_sources_narrow_policy_without_becoming_repository_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        let system = dir.path().join("system.json");
        let organization = dir.path().join("organization.json");
        let manifest = dir.path().join("sources.json");
        fs::write(
            &system,
            json!({"version":1,"id":"system","revision":"s1","exclude":["docs/**"]}).to_string(),
        )
        .unwrap();
        let mut source = json!({"version":1,"id":"organization","revision":"o1","exclude":["src/**"],"memory_exclude":["team.md"]});
        fs::write(&organization, source.to_string()).unwrap();
        fs::write(
            &manifest,
            json!({"version":1,"sources":[organization]}).to_string(),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("docs")).unwrap();
        let mut policy = PrivacyPolicy {
            workspace: Some(dir.path().to_owned()),
            include: vec!["**".into()],
            memory_allow: vec!["**".into()],
            mandatory: load_at(&system, &manifest).unwrap(),
            ..Default::default()
        };
        for name in ["docs/guide.md", "src/main.rs"] {
            assert_eq!(
                policy
                    .evaluate_file(&dir.path().join(name), Some("any"))
                    .action,
                CandidateAction::Excluded
            );
        }
        assert_eq!(
            policy.evaluate_memory("team.md", None).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            policy
                .evaluate_file(&dir.path().join("README.md"), None)
                .action,
            CandidateAction::Allowed
        );
        let before = policy.digest().unwrap();
        policy.save(&repo).unwrap();
        let saved = fs::read_to_string(PrivacyPolicy::path(&repo).unwrap()).unwrap();
        assert!(!saved.contains("mandatory"));
        let local = PrivacyPolicy::load_local(&repo).unwrap();
        assert!(local.mandatory.is_empty());
        assert_eq!(local.exclude, Vec::<String>::new());
        let imported: PrivacyPolicy =
            serde_json::from_value(serde_json::to_value(&policy).unwrap()).unwrap();
        assert!(imported.mandatory.is_empty());
        source["revision"] = json!("o2");
        fs::write(&organization, source.to_string()).unwrap();
        policy.mandatory = load_at(&system, &manifest).unwrap();
        assert_ne!(policy.digest().unwrap(), before);
        fs::remove_file(&organization).unwrap();
        assert!(load_at(&system, &manifest).is_err());
        fs::write(
            &organization,
            b"{\"version\":1,\"id\":\"organization\",\"revision\":\"o2\",\"include\":[\"**\"]}",
        )
        .unwrap();
        assert!(load_at(&system, &manifest).is_err());
    }
}
