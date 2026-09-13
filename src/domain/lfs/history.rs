//! Reachable pointer enumeration uses only the selected local Git object graph.

use crate::domain::repo::Repo;
use anyhow::{Result, ensure};

/// Selected histories determine upload scope, including files removed from their current tips.
pub fn reachable(repo: &Repo, references: &[String]) -> Result<Vec<super::Pointer>> {
    let repo = repo.clone().local_objects_only();
    if references.is_empty() {
        return Ok(vec![]);
    }
    ensure!(
        references.iter().all(|reference| !reference.is_empty()
            && !reference.starts_with('-')
            && !reference.contains(['\0', '\n', '\r'])),
        "invalid LFS history reference"
    );
    let mut args = vec![
        "rev-list",
        "--objects",
        "--no-object-names",
        "--filter=blob:limit=1024",
    ];
    args.extend(references.iter().map(String::as_str));
    args.push("--");
    let mut pointers = std::collections::BTreeMap::new();
    let mut batch = Vec::new();
    let mut inspected = 0usize;
    let read = |batch: &mut Vec<String>,
                pointers: &mut std::collections::BTreeMap<String, super::Pointer>|
     -> Result<()> {
        repo.git_cat_file_batch(
            std::mem::take(batch),
            super::POINTER_LIMIT - 1,
            |_, kind, body| {
                if kind == "blob"
                    && let crate::domain::repo::ObjectBody::Read(bytes) = body
                    && let Some(pointer) = super::Pointer::parse(bytes)?
                {
                    if let Some(previous) = pointers.insert(pointer.oid.clone(), pointer.clone()) {
                        ensure!(
                            previous == pointer,
                            "LFS history assigns conflicting sizes to one object"
                        );
                    }
                    ensure!(
                        pointers.len() <= 10_000,
                        "LFS history exceeds the supported object count"
                    );
                }
                Ok(())
            },
        )
    };
    repo.git_stream_split(&args, b'\n', |record| {
        if record.is_empty() {
            return Ok(());
        }
        inspected += 1;
        ensure!(
            inspected <= 1_000_000,
            "LFS history inspection exceeded its object budget"
        );
        let oid = std::str::from_utf8(record)?;
        ensure!(
            matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid Git object identity during LFS inspection"
        );
        batch.push(oid.to_owned());
        if batch.len() == 256 {
            read(&mut batch, &mut pointers)?;
        }
        Ok(())
    })?;
    read(&mut batch, &mut pointers)?;
    Ok(pointers.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::lfs::{Pointer, VERSION};

    #[test]
    fn bare_history_includes_removed_payloads_and_direct_blob_tags_without_other_branches() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("work")).unwrap();
        let first = Pointer {
            oid: "a".repeat(64),
            size: 123,
        };
        let text = format!(
            "version {VERSION}\noid sha256:{}\nsize {}\n",
            first.oid, first.size
        );
        std::fs::write(repo.root().join("artifact"), text).unwrap();
        repo.add_all().unwrap();
        repo.commit("Store artifact pointer").unwrap();
        let blob = repo.git(&["rev-parse", "HEAD:artifact"]).unwrap();
        repo.git(&["tag", "blob", &blob]).unwrap();
        repo.git(&["rm", "artifact"]).unwrap();
        repo.commit("Remove artifact from tip").unwrap();
        repo.git(&["branch", "foreign"]).unwrap();
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/foreign"])
            .unwrap();
        std::fs::write(
            repo.root().join("other"),
            format!(
                "version {VERSION}\noid sha256:{}\nsize 456\n",
                "b".repeat(64)
            ),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("Store an unrelated branch payload").unwrap();
        let bare = directory.path().join("objects.git");
        repo.git(&[
            "clone",
            "--bare",
            "--no-hardlinks",
            ".",
            bare.to_str().unwrap(),
        ])
        .unwrap();
        let bare = Repo::at(bare);
        assert_eq!(
            reachable(&bare, &["refs/heads/main".into()]).unwrap(),
            vec![first.clone()]
        );
        assert_eq!(
            reachable(&bare, &["refs/tags/blob".into()]).unwrap(),
            vec![first]
        );
        assert!(reachable(&bare, &["--all".into()]).is_err());
        assert!(reachable(&bare, &["refs/heads/missing".into()]).is_err());
        assert!(reachable(&bare, &[]).unwrap().is_empty());
    }
}
