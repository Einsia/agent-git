//! Immutable seed reuse leaves current page records and object resolution local to each reader.

use super::Evidence;
use crate::domain::repo::Repo;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

const MAX_SEEDS: usize = 16;
const MAX_SEED_PAYLOAD_BYTES: usize = 1024 * 1024;
static SEEDS: LazyLock<Mutex<VecDeque<(Key, Evidence)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(Clone, PartialEq, Eq)]
struct Carrier {
    path: PathBuf,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
    #[cfg(not(unix))]
    modified: Option<SystemTime>,
}

impl Carrier {
    fn read(path: &Path) -> std::io::Result<Self> {
        let path = path.canonicalize()?;
        let metadata = std::fs::metadata(&path)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            path,
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: (metadata.dev(), metadata.ino()),
            #[cfg(not(unix))]
            modified: metadata.modified().ok(),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(in crate::domain::secrets) struct Key {
    repository: Carrier,
    common: Carrier,
    cwd: Carrier,
    runtime: String,
    native: String,
    roots: Vec<String>,
    dictionary: String,
}

impl Key {
    pub(in crate::domain::secrets) fn new(
        repo: &Repo,
        cwd: &Path,
        runtime: &str,
        native: &str,
        roots: &[String],
        dictionary: String,
    ) -> crate::Result<Self> {
        Ok(Self {
            repository: Carrier::read(repo.root())?,
            common: Carrier::read(&repo.common_dir()?)?,
            cwd: Carrier::read(cwd)?,
            runtime: runtime.into(),
            native: native.into(),
            roots: roots.to_vec(),
            dictionary,
        })
    }
}

pub(in crate::domain::secrets) fn restore(key: &Key, evidence: &mut Evidence) -> bool {
    let Ok(mut seeds) = SEEDS.lock() else {
        return false;
    };
    let Some(index) = seeds.iter().position(|(saved, _)| saved == key) else {
        return false;
    };
    let entry = seeds.remove(index).expect("selected seed exists");
    *evidence = entry.1.clone();
    // Referenced working repositories can change independently of immutable session commits.
    evidence.resolved.clear();
    seeds.push_back(entry);
    true
}

pub(in crate::domain::secrets) fn save(key: Key, evidence: &Evidence) {
    if retained_payload_bytes(&key, evidence) > MAX_SEED_PAYLOAD_BYTES {
        return;
    }
    let Ok(mut seeds) = SEEDS.lock() else { return };
    seeds.retain(|(saved, _)| saved != &key);
    if seeds.len() == MAX_SEEDS {
        seeds.pop_front();
    }
    let mut seed = evidence.clone();
    seed.resolved.clear();
    seeds.push_back((key, seed));
}

fn retained_payload_bytes(key: &Key, evidence: &Evidence) -> usize {
    let path_bytes = |path: &Path| path.as_os_str().len();
    let operation_bytes = |operation: &super::Operation| {
        path_bytes(&operation.root) + operation.program.len() + operation.verb.len()
    };
    path_bytes(&key.repository.path)
        + path_bytes(&key.common.path)
        + path_bytes(&key.cwd.path)
        + key.runtime.len()
        + key.native.len()
        + key.dictionary.len()
        + key.roots.iter().map(String::len).sum::<usize>()
        + path_bytes(evidence.agent.root())
        + path_bytes(&evidence.cwd)
        + evidence
            .calls
            .iter()
            .chain(&evidence.completed)
            .map(|(id, operation)| id.len() + operation_bytes(operation))
            .sum::<usize>()
        + evidence.seen_calls.iter().map(String::len).sum::<usize>()
        + evidence
            .proven
            .iter()
            .map(|(path, value, _)| path_bytes(path) + value.len())
            .sum::<usize>()
        + evidence
            .cwd_aliases
            .iter()
            .map(|path| path_bytes(path))
            .sum::<usize>()
}
