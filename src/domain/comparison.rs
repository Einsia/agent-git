//! Read-only graph comparison across existing local repositories.

use crate::domain::repo::Repo;
use crate::domain::{meta, storage, transcript, turn};

/// A common prefix of normalized LOG turns is content evidence, never Git ancestry.
/// The count does not identify stored turn commits or authorize dropping any raw events.
pub enum SemanticPrefix {
    Available {
        common: usize,
        left_turns: usize,
        right_turns: usize,
        hash: Option<String>,
    },
    Unavailable(&'static str),
}

impl SemanticPrefix {
    pub fn read(repo: &Repo, left: &str, right: &str) -> crate::Result<Self> {
        let left = Self::chain_at(repo, left)?;
        let right = Self::chain_at(repo, right)?;
        let (Some(left), Some(right)) = (left, right) else {
            return Ok(Self::Unavailable(
                "an endpoint has no AgentGit session declaration",
            ));
        };
        if left.is_empty() || right.is_empty() {
            return Ok(Self::Unavailable(
                "an endpoint has no comparable user turns",
            ));
        }
        let common = left.fork_point(&right);
        Ok(Self::Available {
            common,
            left_turns: left.len(),
            right_turns: right.len(),
            hash: common
                .checked_sub(1)
                .map(|index| left.turns[index].hash.clone()),
        })
    }

    fn chain_at(repo: &Repo, head: &str) -> crate::Result<Option<turn::Chain>> {
        let Some(snapshot) = meta::read_at_ref_result(repo, head)? else {
            anyhow::ensure!(
                !crate::domain::refs::Chain::read(repo, head)?.declared,
                "cannot compare semantic turns: the selected declared history is missing its session metadata"
            );
            return Ok(None);
        };
        if snapshot.is_file_line() || snapshot.session.is_empty() {
            return Ok(Some(turn::Chain::default()));
        }
        let log = storage::materialize_at(repo.root(), head, meta::LOG_FILE)?;
        Ok(Some(turn::chain_of(&transcript::display::parse(&log)?)))
    }
}

/// An object graph spanning the selected repositories without importing into either source.
/// Keep this owner alive while reading through its repository, so temporary alternates remain
/// available for every operation on the frozen endpoint commits.
pub struct Comparison {
    repo: Repo,
    _temporary: Option<tempfile::TempDir>,
}

impl Comparison {
    /// The object view used by ancestry, file, and session reads.
    pub fn repository(&self) -> &Repo {
        &self.repo
    }

    /// Return the unique common ancestor, distinguishing unrelated history from unavailable
    /// ancestry or ambiguous merge bases.
    pub fn merge_base(&self, left: &str, right: &str) -> crate::Result<Option<String>> {
        graph_base(&self.repo, left, right)
    }

    pub fn new(left: &Repo, right: &Repo) -> crate::Result<Self> {
        if left.common_dir()?.canonicalize()? == right.common_dir()?.canonicalize()? {
            return Ok(Self {
                repo: left.clone(),
                _temporary: None,
            });
        }
        let temporary = tempfile::tempdir()?;
        let git = temporary.path().join(".git");
        std::fs::create_dir_all(git.join("objects/info"))?;
        std::fs::create_dir_all(git.join("refs"))?;
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n")?;
        let mut alternates = String::new();
        for source in [left, right] {
            let objects = source.git_path("objects")?.canonicalize()?;
            let path = objects
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("repository object path is not Unicode"))?;
            alternates.push_str(&quote_alternate(path));
            alternates.push('\n');
        }
        std::fs::write(git.join("objects/info/alternates"), alternates)?;
        Ok(Self {
            repo: Repo::at(temporary.path()),
            _temporary: Some(temporary),
        })
    }
}

fn quote_alternate(path: &str) -> String {
    let mut quoted = String::from("\"");
    for character in path.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            character if character.is_ascii_control() => {
                use std::fmt::Write;
                write!(&mut quoted, "\\{:03o}", character as u8).unwrap();
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn graph_base(repo: &Repo, left: &str, right: &str) -> crate::Result<Option<String>> {
    let (code, output, error) = repo.git_status(&["merge-base", "--all", left, right])?;
    match code {
        Some(1) if output.is_empty() && error.is_empty() => Ok(None),
        Some(0) => match output.lines().collect::<Vec<_>>().as_slice() {
            [base] => Ok(Some((*base).to_owned())),
            [] => anyhow::bail!("Git returned no merge-base result"),
            _ => anyhow::bail!("multiple common Git ancestors; ancestry is ambiguous"),
        },
        _ => anyhow::bail!("cannot determine Git ancestry: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs, path::Path};

    fn commit(repo: &Repo, text: &str) -> String {
        fs::write(repo.root().join("AGENTS.md"), text).unwrap();
        repo.add_all().unwrap();
        repo.commit(text).unwrap();
        repo.git(&["rev-parse", "HEAD"]).unwrap()
    }

    fn files(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn comparison_borrows_objects_and_owns_only_its_temporary_directory() {
        let directory = tempfile::tempdir().unwrap();
        let left = Repo::init(&directory.path().join("left")).unwrap();
        let base = commit(&left, "common content");
        let right_path = directory.path().join("right");
        left.git(&[
            "clone",
            "--no-hardlinks",
            left.root().to_str().unwrap(),
            right_path.to_str().unwrap(),
        ])
        .unwrap();
        let right = Repo::at(right_path);
        let a = commit(&left, "left content");
        let b = commit(&right, "right content");
        let left_before = files(left.root());
        let right_before = files(right.root());
        let comparison = Comparison::new(&left, &right).unwrap();
        assert_eq!(comparison.merge_base(&a, &b).unwrap(), Some(base));
        let content = comparison
            .repository()
            .git(&["diff", &a, &b, "--", "AGENTS.md"])
            .unwrap();
        assert!(content.contains("-left content") && content.contains("+right content"));
        let borrowed = comparison.repository().root().to_path_buf();
        assert!(borrowed.exists());
        drop(comparison);
        assert!(!borrowed.exists());
        assert_eq!(files(left.root()), left_before);
        assert_eq!(files(right.root()), right_before);
    }

    #[test]
    fn ancestry_distinguishes_unrelated_missing_and_ambiguous_histories() {
        let directory = tempfile::tempdir().unwrap();
        let left = Repo::init(&directory.path().join("left")).unwrap();
        let right = Repo::init(&directory.path().join("right")).unwrap();
        let root = commit(&left, "left root");
        let unrelated = commit(&right, "right root");
        let graph = Comparison::new(&left, &right).unwrap();
        assert_eq!(graph.merge_base(&root, &unrelated).unwrap(), None);
        assert!(graph.merge_base(&"0".repeat(40), &unrelated).is_err());
        let tree = left.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let a = left
            .git(&["commit-tree", &tree, "-p", &root, "-m", "left ancestry"])
            .unwrap();
        let b = left
            .git(&["commit-tree", &tree, "-p", &root, "-m", "right ancestry"])
            .unwrap();
        let x = left
            .git(&["commit-tree", &tree, "-p", &a, "-p", &b, "-m", "left merge"])
            .unwrap();
        let y = left
            .git(&[
                "commit-tree",
                &tree,
                "-p",
                &b,
                "-p",
                &a,
                "-m",
                "right merge",
            ])
            .unwrap();
        assert!(
            graph
                .merge_base(&x, &y)
                .unwrap_err()
                .to_string()
                .contains("multiple common Git ancestors")
        );
    }

    #[cfg(unix)]
    #[test]
    fn quoted_object_paths_remain_distinct_alternate_entries() {
        let directory = tempfile::Builder::new()
            .prefix("comparison \"quoted\"\npath ")
            .tempdir()
            .unwrap();
        let left = Repo::init(&directory.path().join("left")).unwrap();
        let right = Repo::init(&directory.path().join("right")).unwrap();
        let a = commit(&left, "left content");
        let b = commit(&right, "right content");
        let graph = Comparison::new(&left, &right).unwrap();
        assert_eq!(
            graph
                .repository()
                .git(&["show", &format!("{a}:AGENTS.md")])
                .unwrap(),
            "left content"
        );
        assert_eq!(
            graph
                .repository()
                .git(&["show", &format!("{b}:AGENTS.md")])
                .unwrap(),
            "right content"
        );
    }
}
