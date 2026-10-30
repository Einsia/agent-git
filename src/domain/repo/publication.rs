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
pub(crate) const MAX_COMMITS: usize = 4096;
const MAX_TAG_DEPTH: usize = 32;
const MAX_OBJECT_BYTES: usize = 1024 * 1024;
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_READS: usize = 8192;
/// Commits of the destination's history read to decide which captured commits it already holds.
/// It only stops a runaway walk: the destination's history is not the selected history, so the
/// selected-history bound does not apply to it.
const MAX_ACCEPTED_HISTORY: usize = 1_000_000;

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

    /// The same destination ref at an ancestor of the frozen source. Callers take `oid` from the
    /// frozen source's own history, so delivering it never publishes content outside the plan.
    #[cfg(feature = "cli")]
    pub(crate) fn staged(&self, oid: &str) -> Self {
        Self {
            name: self.name.clone(),
            oid: oid.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PublicationPlan {
    #[serde(skip)]
    include_main: bool,
    heads: Vec<FrozenRef>,
    tags: Vec<FrozenRef>,
    commit_objects: Vec<String>,
    tag_objects: Vec<String>,
    /// Captured commits the walk stopped at because the destination already holds their history;
    /// their ancestry is not part of the plan.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    boundary: Vec<String>,
    /// Tag names the plan takes even below its boundary (see
    /// [`PublicationPlan::freeze_selected_after_carrying`]).
    #[serde(skip)]
    carried: Vec<String>,
}

/// Inspection selection is independent of the complete publication and verification plan.
#[derive(Clone, Debug, Default)]
pub(crate) struct InspectionScope {
    pub(crate) commits: Vec<String>,
    pub(crate) excluded: BTreeSet<String>,
}

impl InspectionScope {
    pub(crate) fn full(plan: &PublicationPlan) -> Self {
        Self {
            commits: plan.commit_objects.clone(),
            excluded: BTreeSet::new(),
        }
    }

    /// Live advertised IDs are resolved in isolated storage so topology overlays cannot
    /// define exclusions. Unavailable roots contribute no exclusions.
    pub(crate) fn incremental(
        repo: &Repo,
        plan: &PublicationPlan,
        advertised: impl Iterator<Item = String>,
    ) -> Result<Self> {
        use std::io::{Seek, Write};
        let advertised: BTreeSet<_> = advertised.collect();
        ensure!(
            advertised.len() <= MAX_REFS,
            "advertised ref limit exceeded"
        );
        ensure!(
            advertised.iter().all(|oid| valid_oid(oid)),
            "invalid advertised object identity"
        );
        let mut roots = BTreeSet::new();
        repo.git_cat_file_batch_check(advertised.iter().cloned().collect(), |oid, kind, _| {
            ensure!(
                advertised.contains(oid),
                "unexpected baseline object identity"
            );
            if matches!(kind, "commit" | "tag") {
                roots.insert(oid.to_owned());
            }
            Ok(())
        })?;
        if roots.is_empty() {
            return Ok(Self::full(plan));
        }
        let mut input = tempfile::tempfile()?;
        for root in roots {
            writeln!(input, "{root}")?;
        }
        input.rewind()?;
        let mut excluded = BTreeSet::new();
        repo.git_stream_split_stdin_file(
            &["rev-list", "--objects", "--no-object-names", "--stdin"],
            input,
            b'\n',
            |record| {
                let oid = std::str::from_utf8(record)?;
                ensure!(valid_oid(oid), "invalid baseline object identity");
                ensure!(excluded.len() < 1_000_000, "baseline object limit exceeded");
                excluded.insert(oid.to_owned());
                Ok(())
            },
        )?;
        Ok(Self {
            commits: plan
                .commit_objects
                .iter()
                .filter(|oid| !excluded.contains(*oid))
                .cloned()
                .collect(),
            excluded,
        })
    }
}

impl PublicationPlan {
    /// Frozen branch roots, sorted by full ref name.
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

    /// Every local commit reachable from the advertised heads: the history the destination
    /// already holds, as far as this repository has it. Only raw ancestry of live destination
    /// heads counts; heads absent here contribute nothing.
    #[cfg(any(feature = "cli", test))]
    pub(crate) fn held_history(repo: &Repo, advertised: &[String]) -> Result<BTreeSet<String>> {
        let mut reader = Reader::new(repo);
        let roots = reader.advertised_commits(advertised)?;
        if roots.is_empty() {
            return Ok(BTreeSet::new());
        }
        reader.reachable_among(&roots, None)
    }

    /// Tags whose commits the heads reach below the boundary, which the plan leaves out: it takes
    /// only the tags of its own commits. Each comes with its commit and the tag objects on the way
    /// to it. Version tags are not listed; the caller knows which turns it published. A tag
    /// `published` says the destination already has as it is here is skipped, and only when one
    /// remains are the heads' ancestors listed.
    #[cfg(any(feature = "cli", test))]
    pub(crate) fn tags_below_boundary(
        &self,
        repo: &Repo,
        published: impl Fn(&str, &str) -> bool,
    ) -> Result<Vec<(FrozenRef, String, Vec<String>)>> {
        if self.boundary.is_empty() {
            return Ok(Vec::new());
        }
        let mut reader = Reader::new(repo);
        let (references, _) = reader.references()?;
        let mut outside = BTreeMap::<String, Vec<(FrozenRef, Vec<String>)>>::new();
        for (name, oid) in references
            .iter()
            .filter(|(name, oid)| name.starts_with("refs/tags/") && !published(name, oid))
        {
            if let Some((commit, objects)) = reader.tag_commit(oid)?
                && self.commit_objects.binary_search(&commit).is_err()
            {
                outside.entry(commit).or_default().push((
                    FrozenRef {
                        name: name.clone(),
                        oid: oid.clone(),
                    },
                    objects,
                ));
            }
        }
        let tips: Vec<String> = self.heads.iter().map(|head| head.oid.clone()).collect();
        Ok(reader
            .reached(&tips, outside)?
            .into_iter()
            .flat_map(|(commit, tags)| {
                tags.into_iter()
                    .map(move |(tag, objects)| (tag, commit.clone(), objects))
            })
            .collect())
    }

    /// Captured commits whose ancestry the plan leaves out, sorted.
    pub fn boundary(&self) -> &[String] {
        &self.boundary
    }

    /// Every annotated tag object in the selected chains, including unreferenced inner tags.
    pub fn tag_objects(&self) -> &[String] {
        &self.tag_objects
    }

    /// Capture explicit branch names and their locally available publication metadata.
    /// Missing objects, malformed metadata and exhausted inspection budgets return an error.
    pub fn freeze(repo: &Repo, branches: &[String]) -> Result<Self> {
        Self::freeze_roots(
            repo,
            branches,
            true,
            Depth::Until(&BTreeSet::new()),
            &BTreeSet::new(),
        )
    }

    /// The heads `freeze` selects, with none of their history: each head is its own boundary.
    pub fn freeze_tips(repo: &Repo, branches: &[String]) -> Result<Self> {
        Self::freeze_roots(repo, branches, true, Depth::Tips, &BTreeSet::new())
    }

    /// Session publication must not add an independently advancing shared-file line.
    pub fn freeze_selected(repo: &Repo, branches: &[String]) -> Result<Self> {
        Self::freeze_roots(
            repo,
            branches,
            false,
            Depth::Until(&BTreeSet::new()),
            &BTreeSet::new(),
        )
    }

    /// Capture explicit branches down to the history the destination already holds: the walk
    /// stops at a commit in `held` and keeps it as a boundary without its ancestry, so the bounds
    /// apply to what this publication adds. An empty `held` captures the whole history.
    pub fn freeze_selected_after(
        repo: &Repo,
        branches: &[String],
        held: &BTreeSet<String>,
    ) -> Result<Self> {
        Self::freeze_roots(repo, branches, false, Depth::Until(held), &BTreeSet::new())
    }

    /// As [`PublicationPlan::freeze_selected_after`], and the tags named in `carried` are taken
    /// too when the heads reach their commits below the boundary. Their history is not captured:
    /// the caller names only tags whose commits the destination holds.
    pub fn freeze_selected_after_carrying(
        repo: &Repo,
        branches: &[String],
        held: &BTreeSet<String>,
        carried: &BTreeSet<String>,
    ) -> Result<Self> {
        Self::freeze_roots(repo, branches, false, Depth::Until(held), carried)
    }

    fn freeze_roots(
        repo: &Repo,
        branches: &[String],
        include_main: bool,
        depth: Depth,
        carried: &BTreeSet<String>,
    ) -> Result<Self> {
        ensure!(
            !branches.is_empty(),
            "audit needs a selected publication branch"
        );
        ensure!(
            branches.len() <= MAX_HEADS,
            "audit publication branch limit exceeded"
        );
        let mut reader = Reader::new(repo);
        let (references, versions) = reader.references()?;
        let mut names: BTreeSet<String> = branches
            .iter()
            .map(|branch| format!("refs/heads/{branch}"))
            .collect();
        if include_main && references.contains_key("refs/heads/main") {
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

        let tips: BTreeSet<String> = heads.iter().map(|head| head.oid.clone()).collect();
        let held = match depth {
            Depth::Until(held) => held,
            Depth::Tips => &tips,
        };
        // Raw parent pointers define published history. Grafts, shallow boundaries, replacement
        // refs and commit-graph caches cannot remove an ancestor from this set.
        let (commits, boundary) = reader.ancestry_until(tips.iter().cloned().collect(), held)?;

        let mut tags = Vec::new();
        let mut tag_objects = BTreeSet::new();
        // Carried tags below the boundary, by commit, kept only if a head reaches them.
        let mut outside = BTreeMap::<String, Vec<(FrozenRef, Vec<String>)>>::new();
        for (name, oid) in references
            .iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
        {
            if let Some((commit, objects)) = reader.tag_commit(oid)? {
                let reference = FrozenRef {
                    name: name.clone(),
                    oid: oid.clone(),
                };
                if commits.contains(&commit) {
                    tags.push(reference);
                    tag_objects.extend(objects);
                } else if carried.contains(name) {
                    outside
                        .entry(commit)
                        .or_default()
                        .push((reference, objects));
                }
            }
        }
        // A version tag names the commit it points at, so whether it belongs to this plan is
        // known without reading it.
        for (name, oid) in versions {
            if commits.contains(&oid) {
                tags.push(FrozenRef { name, oid });
            } else if carried.contains(&name) {
                outside
                    .entry(oid.clone())
                    .or_default()
                    .push((FrozenRef { name, oid }, Vec::new()));
            }
        }
        let roots: Vec<String> = tips.iter().cloned().collect();
        for (_, reached) in reader.reached(&roots, outside)? {
            for (reference, objects) in reached {
                tags.push(reference);
                tag_objects.extend(objects);
            }
        }
        tags.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self {
            include_main,
            heads,
            tags,
            commit_objects: commits.into_iter().collect(),
            tag_objects: tag_objects.into_iter().collect(),
            boundary: boundary.into_iter().collect(),
            carried: carried.iter().cloned().collect(),
        })
    }

    /// Refuse if a fresh local observation differs from the captured plan.
    /// Callers still publish literal object refspecs because this comparison does not lock refs.
    /// The boundary stands in for the held history: a walk that stops at it again captures the
    /// same commits from unchanged heads.
    pub fn verify(&self, repo: &Repo, branches: &[String]) -> Result<()> {
        let held = self.boundary.iter().cloned().collect();
        ensure!(
            Self::freeze_roots(
                repo,
                branches,
                self.include_main,
                Depth::Until(&held),
                &self.carried.iter().cloned().collect(),
            )? == *self,
            "publication refs changed during audit; rerun the audit before publishing"
        );
        Ok(())
    }

    /// Implicit main remains part of the compared snapshot without consuming a selected slot.
    pub(crate) fn verify_captured(&self, repo: &Repo) -> Result<()> {
        let branches = self
            .heads
            .iter()
            .filter(|reference| {
                !(self.include_main && self.heads.len() > 1 && reference.name == "refs/heads/main")
            })
            .map(|reference| {
                reference
                    .name
                    .strip_prefix("refs/heads/")
                    .map(str::to_owned)
                    .context("captured branch name is invalid")
            })
            .collect::<Result<Vec<_>>>()?;
        self.verify(repo, &branches)
    }
}

/// How far below the selected heads a plan captures history.
enum Depth<'a> {
    /// Down to commits in this set, which become the boundary; empty means the whole history.
    Until(&'a BTreeSet<String>),
    /// The heads alone.
    Tips,
}

/// Whether `ancestor` is reachable from `descendant` through raw parent pointers, however long
/// the history between them. Replacement objects, grafts, the shallow file and the commit-graph
/// cache are ignored, as for a captured ancestry, so a local overlay cannot make a commit appear
/// published. An ancestor that is not a local commit is not reachable; a gap in the descendant's
/// history is an error, never an answer.
pub(crate) fn is_raw_ancestor(repo: &Repo, ancestor: &str, descendant: &str) -> Result<bool> {
    ensure!(
        valid_oid(ancestor) && valid_oid(descendant),
        "invalid ancestry object identity"
    );
    let reader = Reader::new(repo);
    let mut present = false;
    reader
        .repo
        .git_cat_file_batch_check(vec![ancestor.to_owned()], |_, kind, _| {
            present = kind == "commit";
            Ok(())
        })?;
    if !present {
        return Ok(false);
    }
    let mut command = reader.repo.inspection_command(&[]);
    crate::infra::git_runtime::disable_grafts(&mut command);
    command
        .args([
            "--no-replace-objects",
            "--shallow-file",
            "/dev/null",
            "-c",
            "core.commitGraph=false",
        ])
        .args(&reader.prefix)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let output = command
        .output()
        .context("local Git inspection could not start")?;
    // Git also exits 1 when it cannot read the history between the two, so only a silent 1 is an
    // answer.
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) if output.stderr.is_empty() => Ok(false),
        _ => anyhow::bail!("audit publication data is unavailable locally"),
    }
}

struct Reader {
    repo: Repo,
    prefix: Vec<String>,
    bytes: usize,
    reads: usize,
    objects: Option<ObjectPipe>,
}

/// One `cat-file --batch` process answers object reads strictly in request order. Each request
/// is written only after the previous answer has been consumed, so neither pipe can fill while
/// the other side waits. Any error leaves the stream position unknown and discards the process.
struct ObjectPipe {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl ObjectPipe {
    fn start(repo: &Repo, prefix: &[String]) -> Result<Self> {
        let mut args: Vec<&str> = prefix.iter().map(String::as_str).collect();
        args.extend(["cat-file", "--batch"]);
        let mut child = repo
            .inspection_command(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("audit publication data is unavailable locally")?;
        let input = child.stdin.take().context("git cat-file has no stdin")?;
        let output = child.stdout.take().context("git cat-file has no stdout")?;
        Ok(Self {
            child,
            input,
            output: std::io::BufReader::new(output),
        })
    }

    /// The object's type and size; the caller must consume the body with `body` or `skip`.
    fn request(&mut self, oid: &str) -> Result<(String, usize)> {
        use std::io::{BufRead, Write};
        writeln!(self.input, "{oid}")?;
        self.input.flush()?;
        let mut line = String::new();
        self.output.read_line(&mut line)?;
        let fields = line
            .strip_suffix('\n')
            .context("audit publication data is unavailable locally")?
            .split(' ')
            .collect::<Vec<_>>();
        ensure!(
            fields.len() == 3 && fields[0] == oid,
            "audit publication data is unavailable locally"
        );
        Ok((fields[1].to_owned(), fields[2].parse()?))
    }

    fn body(&mut self, size: usize) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut body = vec![0; size + 1];
        self.output.read_exact(&mut body)?;
        ensure!(
            body.pop() == Some(b'\n'),
            "audit publication object stream is malformed"
        );
        Ok(body)
    }

    fn skip(&mut self, size: usize) -> Result<()> {
        use std::io::Read;
        let skipped = std::io::copy(
            &mut (&mut self.output).take(size as u64 + 1),
            &mut std::io::sink(),
        )?;
        ensure!(
            skipped == size as u64 + 1,
            "audit publication object stream is malformed"
        );
        Ok(())
    }
}

impl Drop for ObjectPipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
            objects: None,
        }
    }

    /// Requests one object from the shared batch process, counted against the read budget.
    fn request(&mut self, oid: &str) -> Result<(String, usize)> {
        ensure!(
            self.reads < MAX_READS,
            "audit publication read limit exceeded"
        );
        self.reads += 1;
        if self.objects.is_none() {
            self.objects = Some(ObjectPipe::start(&self.repo, &self.prefix)?);
        }
        let result = self.objects.as_mut().expect("started above").request(oid);
        if result.is_err() {
            self.objects = None;
        }
        result
    }

    fn pipe(&mut self) -> &mut ObjectPipe {
        self.objects.as_mut().expect("a request precedes its body")
    }

    /// Consumes a requested body; an error discards the process with its stream position.
    fn finish<T>(&mut self, read: impl FnOnce(&mut ObjectPipe) -> Result<T>) -> Result<T> {
        let result = read(self.pipe());
        if result.is_err() {
            self.objects = None;
        }
        result
    }

    /// Drops a requested body that is not needed. A body above the object limit is not read
    /// through: the process is discarded instead and the next request starts another.
    fn discard(&mut self, size: usize) -> Result<()> {
        if size > MAX_OBJECT_BYTES {
            self.objects = None;
            return Ok(());
        }
        self.finish(|pipe| pipe.skip(size))
    }

    /// The commits reachable from `pending`, and those of them in `held`, whose parents are not
    /// walked. `MAX_COMMITS` bounds the captured commits, boundary included.
    fn ancestry_until(
        &mut self,
        mut pending: Vec<String>,
        held: &BTreeSet<String>,
    ) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        let mut commits = BTreeSet::new();
        let mut boundary = BTreeSet::new();
        while let Some(oid) = pending.pop() {
            if commits.contains(&oid) {
                continue;
            }
            if held.is_empty() {
                ensure!(
                    commits.len() < MAX_COMMITS,
                    "this branch has more history than one publication can carry ({MAX_COMMITS} \
                     commits); it cannot be published until agit lifts this limit"
                );
            } else {
                ensure!(
                    commits.len() < MAX_COMMITS,
                    "this push adds more history than one publication can carry ({MAX_COMMITS} \
                     commits); publish it in smaller steps"
                );
            }
            let body = self.object(&oid, "commit")?;
            if held.contains(&oid) {
                boundary.insert(oid.clone());
            } else {
                pending.extend(commit_parents(&body)?);
            }
            commits.insert(oid);
        }
        Ok((commits, boundary))
    }

    /// The advertised objects that are commits in this repository.
    #[cfg(any(feature = "cli", test))]
    fn advertised_commits(&mut self, advertised: &[String]) -> Result<Vec<String>> {
        let advertised: BTreeSet<_> = advertised.iter().cloned().collect();
        ensure!(
            advertised.len() <= MAX_REFS,
            "advertised ref limit exceeded"
        );
        ensure!(
            advertised.iter().all(|oid| valid_oid(oid)),
            "invalid advertised object identity"
        );
        let mut roots = Vec::new();
        self.repo.git_cat_file_batch_check(
            advertised.iter().cloned().collect(),
            |oid, kind, _| {
                ensure!(
                    advertised.contains(oid),
                    "unexpected advertised object identity"
                );
                if kind == "commit" {
                    roots.push(oid.to_owned());
                }
                Ok(())
            },
        )?;
        Ok(roots)
    }

    /// The entries of `outside` whose commit `roots` reach. Nothing is walked when it is empty.
    fn reached<T>(
        &mut self,
        roots: &[String],
        outside: BTreeMap<String, T>,
    ) -> Result<Vec<(String, T)>> {
        if outside.is_empty() {
            return Ok(Vec::new());
        }
        let among: Vec<String> = outside.keys().cloned().collect();
        let reachable = self.reachable_among(roots, Some(&among))?;
        Ok(outside
            .into_iter()
            .filter(|(commit, _)| reachable.contains(commit))
            .collect())
    }

    /// The commits reachable from `roots`, limited to `among` (sorted) when it is given.
    ///
    /// Every ancestor of the roots is listed, under its own bound rather than the selected
    /// history's: a walk of every advertised branch under the selected-history bound refuses a
    /// short session once the repository's published branches together outgrow it. Git is not
    /// asked to exclude history (`--not`), because that stops early on commit dates and misses an
    /// ancestor older than its descendants. Replacement objects, grafts, the shallow file and the
    /// commit-graph cache are ignored, so Git sees the same raw parents as the captured ancestry.
    fn reachable_among(
        &mut self,
        roots: &[String],
        among: Option<&[String]>,
    ) -> Result<BTreeSet<String>> {
        use std::io::{BufRead, Seek, Write};
        let mut input = tempfile::tempfile()?;
        for root in roots {
            writeln!(input, "{root}")?;
        }
        input.rewind()?;
        let mut command = self.repo.inspection_command(&[]);
        crate::infra::git_runtime::disable_grafts(&mut command);
        command
            .args([
                "--no-replace-objects",
                "--shallow-file",
                "/dev/null",
                "-c",
                "core.commitGraph=false",
            ])
            .args(&self.prefix)
            .args(["rev-list", "--stdin"])
            .stdin(input)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command
            .spawn()
            .context("local Git inspection could not start")?;
        let stdout = child.stdout.take().expect("piped inspection stdout");
        let mut reachable = BTreeSet::new();
        let mut walked = 0usize;
        let read = std::io::BufReader::new(stdout)
            .lines()
            .try_for_each(|line| -> Result<()> {
                let oid = line.context("accepted publication history could not be read")?;
                ensure!(valid_oid(&oid), "invalid accepted object identity");
                walked += 1;
                ensure!(
                    walked <= MAX_ACCEPTED_HISTORY,
                    "accepted publication history limit exceeded"
                );
                if among.is_none_or(|among| among.binary_search(&oid).is_ok()) {
                    reachable.insert(oid);
                }
                Ok(())
            });
        if read.is_err() {
            let _ = child.kill();
        }
        let status = child
            .wait()
            .context("local Git inspection could not be reaped")?;
        read?;
        ensure!(
            status.success(),
            "audit publication data is unavailable locally"
        );
        Ok(reachable)
    }

    /// Local branches and tags, and apart from them the version tags (see [`version_tag`]).
    ///
    /// Every published turn has a version tag and a fetch brings all of them, so a checkout of a
    /// busy repository holds more of them than [`MAX_REFS`]. They are therefore streamed and
    /// counted against neither that bound nor the read budget; every other ref still is.
    fn references(&mut self) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
        ensure!(
            self.reads < MAX_READS,
            "audit publication read limit exceeded"
        );
        self.reads += 1;
        let mut command: Vec<&str> = self.prefix.iter().map(String::as_str).collect();
        command.extend_from_slice(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/tags",
        ]);
        let mut refs = BTreeMap::new();
        let mut versions = BTreeMap::new();
        self.repo.git_stream_split_inspection(
            &command,
            b'\n',
            crate::infra::local_git::Deadline::new(),
            |line| {
                if line.is_empty() {
                    return Ok(());
                }
                let line = std::str::from_utf8(line)
                    .context("audit publication refs are not valid UTF-8")?;
                let (oid, name) = line
                    .split_once(' ')
                    .context("audit publication refs are malformed")?;
                ensure!(
                    valid_oid(oid)
                        && name.len() <= 1024
                        && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/")),
                    "audit publication refs are malformed"
                );
                let listed = if version_tag(name, oid) {
                    &mut versions
                } else {
                    ensure!(
                        refs.len() < MAX_REFS,
                        "audit publication reference limit exceeded"
                    );
                    &mut refs
                };
                ensure!(
                    listed.insert(name.to_owned(), oid.to_owned()).is_none(),
                    "audit publication refs are repeated"
                );
                Ok(())
            },
        )?;
        Ok((refs, versions))
    }

    fn object(&mut self, oid: &str, kind: &str) -> Result<Vec<u8>> {
        ensure!(valid_oid(oid), "audit publication object id is malformed");
        let remaining = MAX_METADATA_BYTES
            .checked_sub(self.bytes)
            .context("audit publication metadata limit exceeded")?;
        let (actual, size) = self.request(oid)?;
        if actual != kind || size > remaining.min(MAX_OBJECT_BYTES) {
            self.discard(size)?;
            ensure!(
                actual == kind,
                "audit publication data is unavailable locally"
            );
            anyhow::bail!("audit publication object exceeds its read limit");
        }
        let body = self.finish(|pipe| pipe.body(size))?;
        self.bytes += body.len();
        verify_identity(oid, kind, &body)?;
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
            let (kind, size) = self.request(&current)?;
            if kind == "tag" {
                // The batch answer already carries the body that `object` would request again.
                let remaining = MAX_METADATA_BYTES
                    .checked_sub(self.bytes)
                    .context("audit publication metadata limit exceeded")?;
                if size > remaining.min(MAX_OBJECT_BYTES) {
                    self.discard(size)?;
                    anyhow::bail!("audit publication object exceeds its read limit");
                }
                let body = self.finish(|pipe| pipe.body(size))?;
                self.bytes += body.len();
                verify_identity(&current, "tag", &body)?;
                objects.push(current);
                current = tag_target(&body)?;
                continue;
            }
            self.discard(size)?;
            match kind.as_str() {
                "commit" => return Ok(Some((current, objects))),
                "tree" | "blob" => return Ok(None),
                _ => anyhow::bail!("audit tag target has an unsupported object type"),
            }
        }
        anyhow::bail!("audit tag chain limit exceeded")
    }
}

/// Whether `name` is the lightweight version tag agit gives a published commit: it is named by the
/// object it points at. An annotated tag, or one moved to another object, is not one.
fn version_tag(name: &str, oid: &str) -> bool {
    name.strip_prefix("refs/tags/agit-") == Some(oid)
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn verify_identity(oid: &str, kind: &str, body: &[u8]) -> Result<()> {
    let actual = match oid.len() {
        40 => digest::<sha1::Sha1>(kind, body),
        64 => digest::<sha2::Sha256>(kind, body),
        _ => anyhow::bail!("audit publication object id is malformed"),
    };
    ensure!(
        actual == oid,
        "audit publication object does not match its id"
    );
    Ok(())
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

pub(crate) fn commit_parents(body: &[u8]) -> Result<Vec<String>> {
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

    fn git_with_input(root: &std::path::Path, args: &[&str], input: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "owned Git fixture command failed: {args:?}"
        );
    }

    /// A checkout that fetched more version tags than the reference limit (one per published turn
    /// of every session) still freezes a branch, carrying exactly the version tags of its own
    /// commits. Every other tag still counts: a listing that exempted all tags, or none, fails
    /// one of the two halves.
    #[test]
    fn version_tags_do_not_bound_a_freeze_but_other_refs_do() {
        let (directory, repo, base) = repository();
        let root = directory.path();
        git(root, &["branch", "selected"]);
        let mut turns = String::new();
        for index in 0..=MAX_REFS {
            let message = format!("turn {index}\n");
            turns.push_str(&format!(
                "commit refs/imported/turns\nmark :{}\n\
                 committer Fixture <fixture@example.invalid> 0 +0000\ndata {}\n{message}\n",
                index + 1,
                message.len()
            ));
        }
        let marks = root.join(".git/turn-marks");
        git_with_input(
            root,
            &[
                "fast-import",
                "--quiet",
                &format!("--export-marks={}", marks.display()),
            ],
            &turns,
        );
        let mut tags = format!("create refs/tags/agit-{base} {base}\n");
        for line in std::fs::read_to_string(&marks).unwrap().lines() {
            let oid = line.split(' ').nth(1).unwrap();
            tags.push_str(&format!("create refs/tags/agit-{oid} {oid}\n"));
        }
        git_with_input(root, &["update-ref", "--stdin"], &tags);
        git(root, &["update-ref", "-d", "refs/imported/turns"]);
        let branches = vec!["selected".into()];
        let plan = PublicationPlan::freeze(&repo, &branches).unwrap();
        assert_eq!(
            plan.tags
                .iter()
                .map(|tag| tag.name.as_str())
                .collect::<Vec<_>>(),
            [format!("refs/tags/agit-{base}")]
        );

        let releases: String = (0..MAX_REFS)
            .map(|index| format!("create refs/tags/release-{index} {base}\n"))
            .collect();
        git_with_input(root, &["update-ref", "--stdin"], &releases);
        let refused = PublicationPlan::freeze(&repo, &branches).unwrap_err();
        assert!(
            refused.to_string().contains("reference limit exceeded"),
            "{refused:#}"
        );
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
        let selected = PublicationPlan::freeze_selected(&repo, &branches).unwrap();
        assert_eq!(selected.heads.len(), 1);
        assert_eq!(selected.heads[0].name(), "refs/heads/selected");
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
        git(root, &["update-ref", "refs/heads/main", &next, &base]);
        selected.verify(&repo, &branches).unwrap();
        assert!(plan.verify(&repo, &branches).is_err());
        git(root, &["update-ref", "refs/heads/main", &base, &next]);
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

    /// A topology overlay cannot hide raw ancestry or make a local fork remotely accepted.
    #[test]
    fn raw_parent_selection_ignores_local_topology_overlays() {
        let (_directory, repo, base) = repository();
        let root = repo.root();
        git(root, &["tag", "base-version", &base]);
        git(root, &["commit", "--allow-empty", "-qm", "tip"]);
        let tip = git(root, &["rev-parse", "HEAD"]);
        let branches = vec!["main".to_owned()];
        let plan = PublicationPlan::freeze(&repo, &branches).unwrap();
        git(root, &["checkout", "-qb", "historical-fork", &base]);
        git(root, &["commit", "--allow-empty", "-qm", "local fork"]);
        let fork = git(root, &["rev-parse", "HEAD"]);
        git(root, &["replace", "--graft", &tip, &fork]);
        std::fs::write(root.join(".git/shallow"), format!("{tip}\n")).unwrap();
        std::fs::write(root.join(".git/info/grafts"), format!("{tip} {fork}\n")).unwrap();
        assert_eq!(PublicationPlan::freeze(&repo, &branches).unwrap(), plan);
        assert_eq!(plan.tags[0].oid, base);
        let held = PublicationPlan::held_history(&repo, std::slice::from_ref(&tip)).unwrap();
        assert_eq!(held, BTreeSet::from([base.clone(), tip]));
        assert!(
            PublicationPlan::held_history(&repo, &["f".repeat(40)])
                .unwrap()
                .is_empty()
        );
    }

    /// The destination's held history is read however much of it there is, and a short branch
    /// continuing it is captured down to it. A raw walk of every advertised tip under the history
    /// bound refuses here, and with it every session of a repository whose published branches
    /// together outgrow the bound.
    #[test]
    fn accepted_history_ignores_the_length_of_unrelated_published_branches() {
        use std::io::Write;
        let (_directory, repo, base) = repository();
        let root = repo.root();
        let mut stream = String::new();
        for index in 0..=MAX_COMMITS {
            stream.push_str("commit refs/heads/published\n");
            stream.push_str("committer Writer <writer@example.invalid> 1700000000 +0000\n");
            stream.push_str(&format!("data {}\n{index}\n", index.to_string().len() + 1));
            if index == 0 {
                stream.push_str(&format!("from {base}\n"));
            }
        }
        let mut import = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["fast-import", "--quiet"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        import
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(import.wait().unwrap().success());
        let published = git(root, &["rev-parse", "refs/heads/published"]);
        git(root, &["checkout", "-qb", "short", &base]);
        git(root, &["commit", "--allow-empty", "-qm", "short session"]);
        let held = PublicationPlan::held_history(&repo, &[published]).unwrap();
        assert!(held.contains(&base));
        let plan = PublicationPlan::freeze_selected_after(&repo, &["short".into()], &held).unwrap();
        assert_eq!(plan.boundary(), std::slice::from_ref(&base));
        assert_eq!(plan.commit_objects.len(), 2);
    }

    /// A bounded plan takes the tags it is asked to carry when the heads reach their commits below
    /// the boundary, without capturing that history, so a tag on an early commit of a history
    /// longer than one publication can carry is published with a short continuation. A carried
    /// name the heads do not reach, and a tag below the boundary nobody named, stay out. A plan
    /// that captured down to the tag's commit is refused here at the commit bound; one that took
    /// every tag below the boundary would republish the whole history's tags on every push.
    #[test]
    fn a_bounded_plan_carries_named_tags_below_its_boundary() {
        let (_directory, repo, base) = repository();
        let root = repo.root();
        let mut stream = String::new();
        for index in 0..=MAX_COMMITS {
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!("mark :{}\n", index + 1));
            stream.push_str("committer Writer <writer@example.invalid> 1700000000 +0000\n");
            stream.push_str(&format!("data {}\n{index}\n", index.to_string().len() + 1));
            if index == 0 {
                stream.push_str(&format!("from {base}\n"));
            }
        }
        stream.push_str("reset refs/tags/early\nfrom :2\n\n");
        stream.push_str("reset refs/tags/unnamed\nfrom :3\n\n");
        git_with_input(root, &["fast-import", "--quiet"], &stream);
        let published = git(root, &["rev-parse", "main"]);
        let early = git(root, &["rev-parse", "refs/tags/early"]);
        git(root, &["checkout", "-qb", "elsewhere", &base]);
        git(root, &["commit", "--allow-empty", "-qm", "elsewhere"]);
        let elsewhere = git(root, &["rev-parse", "HEAD"]);
        git(root, &["tag", "unreached", &elsewhere]);
        git(root, &["checkout", "-q", "main"]);
        git(root, &["commit", "--allow-empty", "-qm", "continued"]);
        let held = BTreeSet::from([published.clone()]);
        let carried = BTreeSet::from([
            "refs/tags/early".to_owned(),
            "refs/tags/unreached".to_owned(),
        ]);
        let plan = PublicationPlan::freeze_selected_after_carrying(
            &repo,
            &["main".into()],
            &held,
            &carried,
        )
        .unwrap();
        assert_eq!(plan.boundary(), std::slice::from_ref(&published));
        assert_eq!(plan.commit_objects.len(), 2);
        assert_eq!(
            plan.tags,
            [FrozenRef {
                name: "refs/tags/early".into(),
                oid: early,
            }]
        );
        plan.verify(&repo, &["main".into()]).unwrap();
        let bounded =
            PublicationPlan::freeze_selected_after(&repo, &["main".into()], &held).unwrap();
        assert!(bounded.tags.is_empty());
    }

    /// Missing history is an incomplete audit, even when a local shallow boundary hides it: a plan
    /// refuses it, and whether a commit below the gap is an ancestor is an error, not an answer.
    /// Git exits 1 both for "not an ancestor" and for a walk that cannot read the gap, so an
    /// ancestry question that reads only the exit status answers "no" here, and a reclaim then
    /// keeps or drops a record on history it never saw.
    #[test]
    fn unavailable_raw_parent_refuses_publication_plan() {
        let (_directory, repo, base) = repository();
        git(repo.root(), &["commit", "--allow-empty", "-qm", "middle"]);
        let middle = git(repo.root(), &["rev-parse", "HEAD"]);
        git(repo.root(), &["commit", "--allow-empty", "-qm", "tip"]);
        let tip = git(repo.root(), &["rev-parse", "HEAD"]);
        assert!(is_raw_ancestor(&repo, &base, &tip).unwrap());
        std::fs::write(repo.root().join(".git/shallow"), format!("{tip}\n")).unwrap();
        let object = repo
            .root()
            .join(".git/objects")
            .join(&middle[..2])
            .join(&middle[2..]);
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
        assert!(is_raw_ancestor(&repo, &base, &tip).is_err());
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
