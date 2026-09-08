//! Prefix discovery reuses immutable evidence while rechecking changed paths at each cut.

use super::super::{ReadStats, charge_expansion};
use crate::{
    Result,
    domain::{meta, repo::Repo, storage},
};
use anyhow::Context as _;
use std::{collections::BTreeMap, sync::Arc};

struct Proof {
    id: String,
    oid: String,
    bytes: u64,
}

#[derive(Default)]
pub(super) struct Evidence {
    records: Vec<(usize, Arc<Proof>)>,
    active: BTreeMap<String, Arc<Proof>>,
    next: usize,
    previous: Option<String>,
}

impl Evidence {
    pub(super) fn insert(&mut self, position: usize, id: &str, oid: &str, bytes: usize) {
        self.records.push((
            position,
            Arc::new(Proof {
                id: id.into(),
                oid: oid.into(),
                bytes: bytes as u64,
            }),
        ));
    }

    pub(super) fn sort(&mut self) {
        self.records.sort_by_key(|(position, _)| *position);
    }

    pub(super) fn validate_at(
        &mut self,
        repo: &Repo,
        sha: &str,
        end: usize,
        stats: &mut ReadStats,
    ) -> Result<()> {
        let next = self
            .records
            .partition_point(|(position, _)| *position < end);
        anyhow::ensure!(next >= self.next, "native context prefix moved backwards");
        let mut checks = BTreeMap::new();
        // A discarded envelope still determines whether a native dependency exists. Its
        // classification is reusable only while the immutable event path keeps its object.
        if let Some(previous) = &self.previous
            && !self.active.is_empty()
        {
            repo.git_stream_split(
                &[
                    "diff-tree",
                    "-r",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=none",
                    "--name-only",
                    "-z",
                    previous,
                    sha,
                    "--",
                    meta::EVENTS_DIR,
                ],
                0,
                |path| {
                    charge_expansion(
                        &mut stats.context_changed_path_bytes,
                        path.len(),
                        1,
                        storage::MAX_MATERIALIZED_BYTES,
                    )?;
                    stats.context_changed_paths = stats
                        .context_changed_paths
                        .checked_add(1)
                        .context("native context changed-path count overflow")?;
                    if let Ok(path) = std::str::from_utf8(path)
                        && let Some(proof) = self.active.get(path)
                    {
                        checks.insert(path.to_owned(), proof.clone());
                    }
                    Ok(())
                },
            )?;
        }
        for (_, proof) in &self.records[self.next..next] {
            checks.insert(meta::event_path(&proof.id)?, proof.clone());
        }
        stats.context_path_checks = stats
            .context_path_checks
            .checked_add(checks.len())
            .context("native context path-check count overflow")?;
        let proofs: Vec<_> = checks.values().collect();
        let asks = checks.keys().map(|path| format!("{sha}:{path}")).collect();
        let mut cursor = 0;
        repo.git_cat_file_batch_check(asks, |oid, kind, bytes| {
            let proof = proofs[cursor];
            cursor += 1;
            anyhow::ensure!(
                kind == "blob" && oid == proof.oid && bytes == proof.bytes,
                "native context event {} is missing or corrupt at turn {sha}",
                proof.id
            );
            Ok(())
        })?;
        // Only a complete check can advance the cut used to prove unchanged paths.
        self.active.extend(checks);
        self.next = next;
        self.previous = Some(sha.into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_trees_still_validate_new_prefix_objects_and_failures_keep_the_frontier() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let first_id = "a".repeat(meta::EVENT_ID_HEX_LEN);
        let second_id = "b".repeat(meta::EVENT_ID_HEX_LEN);
        let first_path = repo.root().join(meta::event_path(&first_id).unwrap());
        let second_path = repo.root().join(meta::event_path(&second_id).unwrap());
        for path in [&first_path, &second_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "unreadable\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("initial paths").unwrap();
        let first = repo.git(&["rev-parse", "HEAD"]).unwrap();
        std::fs::write(repo.root().join("phase"), "next").unwrap();
        repo.add_all().unwrap();
        repo.commit("advance the LOG boundary").unwrap();
        let second = repo.git(&["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            repo.git(&["rev-parse", &format!("{first}:events")])
                .unwrap(),
            repo.git(&["rev-parse", &format!("{second}:events")])
                .unwrap()
        );
        std::fs::write(&second_path, "second\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("restore the carrier object").unwrap();
        let carrier = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let mut evidence = Evidence::default();
        for (position, id) in [(0, first_id), (1, second_id)] {
            let path = meta::event_path(&id).unwrap();
            let oid = repo
                .git(&["rev-parse", &format!("{carrier}:{path}")])
                .unwrap();
            evidence.insert(
                position,
                &id,
                &oid,
                std::fs::metadata(repo.root().join(path)).unwrap().len() as usize,
            );
        }
        evidence.sort();
        let mut stats = ReadStats::default();
        evidence.validate_at(&repo, &first, 1, &mut stats).unwrap();
        assert!(evidence.validate_at(&repo, &second, 2, &mut stats).is_err());
        assert_eq!(stats.context_changed_paths, 0);
        assert_eq!(stats.context_path_checks, 2);
        assert_eq!(evidence.previous.as_deref(), Some(first.as_str()));
        assert_eq!(evidence.next, 1);
        assert!(
            evidence
                .validate_at(&repo, "missing-cut", 2, &mut stats)
                .is_err()
        );
        assert_eq!(evidence.previous.as_deref(), Some(first.as_str()));
        evidence
            .validate_at(&repo, &carrier, 2, &mut stats)
            .unwrap();
        assert_eq!(evidence.next, 2);
        assert_eq!(stats.context_changed_paths, 1);
        assert_eq!(stats.context_path_checks, 3);
    }
}
