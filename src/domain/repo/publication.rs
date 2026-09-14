//! Immutable publication roots for local inspection and explicit Git refspecs.
//!
//! A plan identifies selected history and tag metadata; it does not collect payloads,
//! approve publication, or contact a remote. Callers must retain the frozen object IDs
//! when inspecting or publishing and separately enforce their review policy.
//! Verification compares a fresh observation; it does not lock the refs.

use crate::domain::repo::Repo;
use anyhow::{Context, Result, ensure};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};

const MAX_REFS: usize = 4096;
const MAX_HEADS: usize = 64;
const MAX_COMMITS: usize = 4096;
const MAX_TAG_DEPTH: usize = 32;
const MAX_OBJECT_BYTES: usize = 1024 * 1024;
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_READS: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FrozenRef {
    name: String,
    oid: String,
}

impl FrozenRef {
    /// The full destination ref name, independent of the current checkout.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The unpeeled source object ID retained by the plan.
    pub fn oid(&self) -> &str {
        &self.oid
    }

    /// A literal object source prevents later ref movement from selecting different content.
    pub fn refspec(&self) -> String {
        format!("{}:{}", self.oid, self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PublicationPlan {
    heads: Vec<FrozenRef>,
    tags: Vec<FrozenRef>,
    commit_objects: Vec<String>,
    tag_objects: Vec<String>,
}

impl PublicationPlan {
    /// Selected branch roots and the existing main file line, sorted by full ref name.
    pub fn heads(&self) -> &[FrozenRef] {
        &self.heads
    }

    /// Tags whose peeled commit belongs to the captured raw ancestry.
    pub fn tags(&self) -> &[FrozenRef] {
        &self.tags
    }

    /// Every captured commit object, including ancestors hidden by local topology overlays.
    pub fn commit_objects(&self) -> &[String] {
        &self.commit_objects
    }

    /// Every annotated tag object in the selected chains, including unreferenced inner tags.
    pub fn tag_objects(&self) -> &[String] {
        &self.tag_objects
    }

    /// Capture explicit branch names and their locally available publication metadata.
    /// Missing objects, malformed metadata and exhausted inspection budgets return an error.
    pub fn freeze(repo: &Repo, branches: &[String]) -> Result<Self> {
        ensure!(
            !branches.is_empty(),
            "audit needs a selected publication branch"
        );
        ensure!(
            branches.len() <= MAX_HEADS,
            "audit publication branch limit exceeded"
        );
        let mut reader = Reader::new(repo);
        let references = reader.references()?;
        let mut names: BTreeSet<String> = branches
            .iter()
            .map(|branch| format!("refs/heads/{branch}"))
            .collect();
        if references.contains_key("refs/heads/main") {
            names.insert("refs/heads/main".into());
        }
        let heads: Vec<FrozenRef> = names
            .into_iter()
            .map(|name| {
                let oid = references
                    .get(&name)
                    .context("a selected publication branch is unavailable")?;
                Ok(FrozenRef {
                    name,
                    oid: oid.clone(),
                })
            })
            .collect::<Result<_>>()?;

        // Raw parent pointers define published history. Grafts, shallow boundaries, replacement
        // refs and commit-graph caches cannot remove an ancestor from this set.
        let mut commits = BTreeSet::new();
        let mut pending: Vec<String> = heads
            .iter()
            .map(|reference| reference.oid.clone())
            .collect();
        while let Some(oid) = pending.pop() {
            if commits.contains(&oid) {
                continue;
            }
            ensure!(
                commits.len() < MAX_COMMITS,
                "audit publication history limit exceeded"
            );
            let body = reader.object(&oid, "commit")?;
            pending.extend(commit_parents(&body)?);
            commits.insert(oid);
        }

        let mut tags = Vec::new();
        let mut tag_objects = BTreeSet::new();
        for (name, oid) in references
            .iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
        {
            if let Some((commit, objects)) = reader.tag_commit(oid)?
                && commits.contains(&commit)
            {
                tags.push(FrozenRef {
                    name: name.clone(),
                    oid: oid.clone(),
                });
                tag_objects.extend(objects);
            }
        }
        Ok(Self {
            heads,
            tags,
            commit_objects: commits.into_iter().collect(),
            tag_objects: tag_objects.into_iter().collect(),
        })
    }

    /// Refuse if a fresh local observation differs from the captured plan.
    /// Callers still publish literal object refspecs because this comparison does not lock refs.
    pub fn verify(&self, repo: &Repo, branches: &[String]) -> Result<()> {
        ensure!(
            Self::freeze(repo, branches)? == *self,
            "publication refs changed during audit; rerun the audit before publishing"
        );
        Ok(())
    }
}

struct Reader {
    repo: Repo,
    prefix: Vec<String>,
    bytes: usize,
    reads: usize,
}

impl Reader {
    fn new(repo: &Repo) -> Self {
        Self {
            repo: repo.clone().local_objects_only(),
            // Repo already selects the root with -C. These paths must stay relative to that
            // root so a relative AGIT_HOME cannot be applied twice or discover a parent repo.
            prefix: vec![
                "--git-dir".into(),
                ".git".into(),
                "--work-tree".into(),
                ".".into(),
            ],
            bytes: 0,
            reads: 0,
        }
    }

    fn read(&mut self, args: &[&str], limit: usize) -> Result<Vec<u8>> {
        ensure!(
            self.reads < MAX_READS,
            "audit publication read limit exceeded"
        );
        self.reads += 1;
        let mut command: Vec<&str> = self.prefix.iter().map(String::as_str).collect();
        command.extend_from_slice(args);
        let output = self.repo.inspection_output(&command, limit)?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "audit publication data is unavailable locally"
        );
        Ok(output.stdout)
    }

    fn references(&mut self) -> Result<BTreeMap<String, String>> {
        let bytes = self.read(
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
            ],
            MAX_OBJECT_BYTES,
        )?;
        let text =
            std::str::from_utf8(&bytes).context("audit publication refs are not valid UTF-8")?;
        ensure!(
            text.is_empty() || text.ends_with('\n'),
            "audit publication refs are incomplete"
        );
        let mut refs = BTreeMap::new();
        for line in text.lines() {
            ensure!(
                refs.len() < MAX_REFS,
                "audit publication reference limit exceeded"
            );
            let (oid, name) = line
                .split_once(' ')
                .context("audit publication refs are malformed")?;
            ensure!(
                valid_oid(oid)
                    && name.len() <= 1024
                    && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/")),
                "audit publication refs are malformed"
            );
            ensure!(
                refs.insert(name.into(), oid.into()).is_none(),
                "audit publication refs are repeated"
            );
        }
        Ok(refs)
    }

    fn object(&mut self, oid: &str, kind: &str) -> Result<Vec<u8>> {
        ensure!(valid_oid(oid), "audit publication object id is malformed");
        let remaining = MAX_METADATA_BYTES
            .checked_sub(self.bytes)
            .context("audit publication metadata limit exceeded")?;
        let body = self.read(&["cat-file", kind, oid], remaining.min(MAX_OBJECT_BYTES))?;
        self.bytes += body.len();
        let actual = match oid.len() {
            40 => digest::<sha1::Sha1>(kind, &body),
            64 => digest::<sha2::Sha256>(kind, &body),
            _ => unreachable!("object id length is validated before reading"),
        };
        ensure!(
            actual == oid,
            "audit publication object does not match its id"
        );
        Ok(body)
    }

    fn tag_commit(&mut self, oid: &str) -> Result<Option<(String, Vec<String>)>> {
        let mut current = oid.to_owned();
        let mut seen = BTreeSet::new();
        let mut objects = Vec::new();
        for _ in 0..MAX_TAG_DEPTH {
            ensure!(
                seen.insert(current.clone()),
                "audit tag chain repeats an object"
            );
            let kind = self.read(&["cat-file", "-t", &current], 32)?;
            match kind.as_slice() {
                b"commit\n" => return Ok(Some((current, objects))),
                b"tag\n" => {
                    let body = self.object(&current, "tag")?;
                    objects.push(current);
                    current = tag_target(&body)?;
                }
                b"tree\n" | b"blob\n" => return Ok(None),
                _ => anyhow::bail!("audit tag target has an unsupported object type"),
            }
        }
        anyhow::bail!("audit tag chain limit exceeded")
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest<D: Digest>(kind: &str, body: &[u8]) -> String {
    let mut hash = D::new();
    hash.update(format!("{kind} {}\0", body.len()));
    hash.update(body);
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn header(body: &[u8]) -> Result<&str> {
    let end = body
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .context("audit publication object has no complete header")?;
    std::str::from_utf8(&body[..end]).context("audit publication object header is malformed")
}

fn commit_parents(body: &[u8]) -> Result<Vec<String>> {
    let header = header(body)?;
    let mut parents = Vec::new();
    let mut tree = None;
    for line in header.lines() {
        if let Some(oid) = line.strip_prefix("tree ") {
            ensure!(
                tree.replace(oid).is_none() && valid_oid(oid),
                "audit commit tree is malformed"
            );
        } else if let Some(oid) = line.strip_prefix("parent ") {
            ensure!(valid_oid(oid), "audit commit parent is malformed");
            parents.push(oid.into());
        }
    }
    ensure!(tree.is_some(), "audit commit tree is missing");
    Ok(parents)
}

fn tag_target(body: &[u8]) -> Result<String> {
    let header = header(body)?;
    let mut target = None;
    for line in header.lines() {
        if let Some(oid) = line.strip_prefix("object ") {
            ensure!(
                target.replace(oid).is_none() && valid_oid(oid),
                "audit tag target is malformed"
            );
        }
    }
    target
        .map(str::to_owned)
        .context("audit tag target is missing")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let result = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "owned Git fixture command failed: {args:?}"
        );
        String::from_utf8(result.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn initialize(root: &std::path::Path) -> String {
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Audit fixture"]);
        git(root, &["config", "user.email", "audit@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "tag.gpgsign", "false"]);
        git(root, &["commit", "--allow-empty", "-qm", "base"]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn repository() -> (tempfile::TempDir, Repo, String) {
        let directory = tempfile::tempdir().unwrap();
        let base = initialize(directory.path());
        let repo = Repo::at(directory.path());
        (directory, repo, base)
    }

    /// Relative and absolute spellings must identify the same exact repository carrier.
    #[test]
    fn relative_repository_root_keeps_exact_git_carrier() {
        let cwd = std::env::current_dir().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("agit-audit-relative-")
            .tempdir_in(&cwd)
            .unwrap();
        let base = initialize(directory.path());
        git(directory.path(), &["branch", "selected"]);
        git(directory.path(), &["tag", "base-version", &base]);
        let absolute = Repo::at(directory.path());
        let relative = Repo::at(directory.path().strip_prefix(&cwd).unwrap());
        assert!(absolute.root().is_absolute());
        assert!(relative.root().is_relative());
        let branches = vec!["selected".into()];
        let expected = PublicationPlan::freeze(&absolute, &branches).unwrap();
        assert_eq!(expected.commit_objects, [base]);
        assert_eq!(
            PublicationPlan::freeze(&relative, &branches).unwrap(),
            expected
        );
        expected.verify(&relative, &branches).unwrap();
    }

    /// A missing or invalid nested Git carrier cannot borrow the enclosing repository's refs.
    #[test]
    fn missing_nested_carrier_never_discovers_ancestor() {
        let (_directory, repo, base) = repository();
        let refs = git(repo.root(), &["show-ref"]);
        let nested = repo.root().join("nested checkout");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), base);
        let nested_repo = Repo::at(&nested);
        assert!(PublicationPlan::freeze(&nested_repo, &["main".into()]).is_err());
        std::fs::create_dir(nested.join(".git")).unwrap();
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), base);
        assert!(PublicationPlan::freeze(&nested_repo, &["main".into()]).is_err());
        assert_eq!(git(repo.root(), &["show-ref"]), refs);
    }

    /// Explicit .git anchoring must follow a valid linked worktree gitfile without discovery.
    #[test]
    fn linked_worktree_gitfile_keeps_selected_publication_roots() {
        let (_directory, repo, _base) = repository();
        let linked_directory = tempfile::tempdir().unwrap();
        let linked = linked_directory.path().join("linked checkout");
        git(
            repo.root(),
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert!(linked.join(".git").is_file());
        let branches = vec!["main".into()];
        let expected = PublicationPlan::freeze(&repo, &branches).unwrap();
        assert_eq!(
            PublicationPlan::freeze(&Repo::at(&linked), &branches).unwrap(),
            expected
        );
    }

    /// Ref movement cannot change the literal sources the approved plan would publish.
    #[test]
    fn frozen_refs_include_main_and_unpeeled_reachable_tags() {
        let (_directory, repo, base) = repository();
        let root = repo.root();
        git(root, &["branch", "selected"]);
        git(root, &["tag", "-a", "reviewed", "-m", "tag disclosure"]);
        let tag = git(root, &["rev-parse", "refs/tags/reviewed"]);
        assert_ne!(tag, base);
        git(root, &["checkout", "-qb", "unselected"]);
        git(
            root,
            &["commit", "--allow-empty", "-qm", "unselected history"],
        );
        git(root, &["tag", "unselected-version"]);
        let branches = vec!["selected".to_owned()];
        let plan = PublicationPlan::freeze(&repo, &branches).unwrap();
        assert_eq!(
            plan.heads
                .iter()
                .map(|reference| reference.name.as_str())
                .collect::<Vec<_>>(),
            ["refs/heads/main", "refs/heads/selected"]
        );
        assert_eq!(
            plan.tags,
            vec![FrozenRef {
                name: "refs/tags/reviewed".into(),
                oid: tag.clone()
            }]
        );
        assert_eq!(plan.tags[0].refspec(), format!("{tag}:refs/tags/reviewed"));
        plan.verify(&repo, &branches).unwrap();
        let next = git(root, &["rev-parse", "HEAD"]);
        git(root, &["update-ref", "refs/heads/selected", &next, &base]);
        assert!(plan.verify(&repo, &branches).is_err());
        assert_eq!(
            plan.heads[1].refspec(),
            format!("{base}:refs/heads/selected")
        );
        git(root, &["update-ref", "refs/heads/selected", &base, &next]);
        git(root, &["update-ref", "refs/tags/reviewed", &base, &tag]);
        assert!(plan.verify(&repo, &branches).is_err());
    }

    /// A topology overlay cannot hide a raw ancestor or its publication tag.
    #[test]
    fn raw_parent_selection_ignores_local_topology_overlays() {
        let (_directory, repo, base) = repository();
        let root = repo.root();
        git(root, &["tag", "base-version", &base]);
        git(root, &["commit", "--allow-empty", "-qm", "tip"]);
        let tip = git(root, &["rev-parse", "HEAD"]);
        let branches = vec!["main".to_owned()];
        let plan = PublicationPlan::freeze(&repo, &branches).unwrap();
        std::fs::write(root.join(".git/shallow"), format!("{tip}\n")).unwrap();
        std::fs::write(root.join(".git/info/grafts"), format!("{tip}\n")).unwrap();
        assert_eq!(PublicationPlan::freeze(&repo, &branches).unwrap(), plan);
        assert_eq!(plan.tags[0].oid, base);
    }

    /// Missing history is an incomplete audit, even when a local shallow boundary hides it.
    #[test]
    fn unavailable_raw_parent_refuses_publication_plan() {
        let (_directory, repo, base) = repository();
        git(repo.root(), &["commit", "--allow-empty", "-qm", "tip"]);
        let tip = git(repo.root(), &["rev-parse", "HEAD"]);
        std::fs::write(repo.root().join(".git/shallow"), format!("{tip}\n")).unwrap();
        let object = repo
            .root()
            .join(".git/objects")
            .join(&base[..2])
            .join(&base[2..]);
        #[cfg(windows)]
        {
            let mut permissions = std::fs::metadata(&object).unwrap().permissions();
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "This Windows-only fixture clears the read-only attribute on a Git object."
            )]
            permissions.set_readonly(false);
            std::fs::set_permissions(&object, permissions).unwrap();
        }
        std::fs::remove_file(object).unwrap();
        assert!(PublicationPlan::freeze(&repo, &["main".into()]).is_err());
    }

    /// A nested tag keeps its outer message in the plan rather than collapsing to a commit.
    #[test]
    fn nested_tag_sources_keep_each_unpeeled_object() {
        let (_directory, repo, base) = repository();
        git(
            repo.root(),
            &["tag", "-a", "inner", "-m", "inner disclosure"],
        );
        git(
            repo.root(),
            &[
                "-c",
                "advice.nestedTag=false",
                "tag",
                "-a",
                "outer",
                "inner",
                "-m",
                "outer disclosure",
            ],
        );
        let inner = git(repo.root(), &["rev-parse", "refs/tags/inner"]);
        git(repo.root(), &["tag", "-d", "inner"]);
        let plan = PublicationPlan::freeze(&repo, &["main".into()]).unwrap();
        assert_eq!(plan.tags.len(), 1);
        assert_eq!(plan.tag_objects.len(), 2);
        assert!(plan.tag_objects.contains(&inner));
        for reference in &plan.tags {
            assert_ne!(reference.oid, base);
            assert_eq!(
                reference.oid,
                git(repo.root(), &["rev-parse", &reference.name])
            );
        }
    }
}
