//! repo: `~/.agit/repos/<owner>/<name>/`, one git repository.
//!
//! # Why this has to be git
//!
//! An agent's version history must be git, because the backend moves content over gitsync
//! smart-http (there is no API for uploading transcripts). Version ids are tags:
//!
//! ```text
//! refs/heads/main              the default branch (the file line)
//! refs/heads/<branch>          other branches
//! refs/tags/agit-<40hex>       one version
//! session/meta.json            session metadata for this commit (with the layout version)
//! LOG / VIEW                   the ordered v1 event-id sequence (absent on the file line)
//! events/a/b/c/d/<event-id>    the full v1 envelope objects (absent on the file line)
//! memory/  skills/  AGENTS.md  shared files
//! ```
//!
//! **The only copy of the content on this machine is here.** `agit commit` writes straight into
//! this repository's working tree and makes one git commit; `agit push` is `git push`. Nothing
//! stages in between. A staging layer under `~/.agit/drafts/<agent>/` breaks two things: the draft
//! is deleted once push succeeds, so the next commit loses its comparison baseline, and the hint
//! "if the tag push fails, run `agit push` again" becomes impossible to follow (what would be
//! re-pushed is already gone). With the directory layout identical to git's, both failures are
//! structurally impossible.
//!
//! # Two remotes, in git's own words
//!
//! ```text
//! origin     your copy on the hub (pushes go here)
//! upstream   the source (fetch new work, resolve where a parent came from)
//! ```
//!
//! There is no third concept: `.git/config` already persists both, and agit only sets them and
//! reads them. The three kinds of repository configure their remotes like this:
//!
//! | How it came to be | origin | upstream |
//! |---|---|---|
//! | built by your own `agit import` | your copy | none |
//! | `agit clone alice/photo` | **alice's copy** (not pushable) | none |
//! | `agit clone alice/photo --mine` | your own copy of it | alice's copy |
//!
//! A read-only clone deliberately sets no `upstream`: its `origin` is the source, and a second
//! remote pointing at the same place only stops "has an `upstream`" from meaning "this copy is
//! mine".
//!
//! **`@{upstream}` is not this `upstream`.** The former is git's "which remote branch does the
//! current branch track" (usually `origin/main`), and [`Repo::ahead_behind`] uses it; the latter is
//! a remote named `upstream`. Adding the latter does not change the former — the shared name is
//! git's own historical baggage, not a choice made here.
//!
//! Contrast [`crate::domain::store::Store`] — that is only a directory of links, not a repository.
//! The two are separate because they answer different questions: the store records "which sessions
//! this machine tracks", the repo holds "one agent's version history and content".
//!
//! Authenticated git operations (`clone` / `fetch` / `push`) do not live here; they live in
//! [`crate::hub::git`] — authentication is the hub's business.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Remote name of your copy on the hub. Pushes go here.
pub const ORIGIN: &str = "origin";

/// Remote name of the source copy. Only your own copy (`agit clone --mine`) has one.
pub const UPSTREAM: &str = "upstream";

/// Whether an agent name (the repo name) is valid.
///
/// Letters, digits, `-` and `_` only: this name doubles as a directory name and a URL path segment.
///
/// The `agit-` prefix is **not** forbidden. That prefix reserves the **ref** namespace (a snapshot
/// id is `refs/tags/agit-<sha>`, and `agit clone x/y@Z` tells by it whether Z is a version or a
/// branch), while a repo name always sits in the `owner/<name>` position and never mixes with refs,
/// so there is no ambiguity to resolve. The hub does not forbid it either, and mints such names
/// itself (binding `~/Code/agit-web` yields `alice/agit-web`): forbidding it locally makes
/// `agit init` / `agit push <owner/name>` reject a repo that legitimately exists on the hub and
/// that `agit clone` fetches back. For branch names see [`valid_branch_name`].
pub fn valid_name(name: &str) -> Result<()> {
    let n = name.trim();
    if n.is_empty() {
        bail!("name must not be empty");
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("names may only contain letters, digits, `-` and `_` (got {n})");
    }
    Ok(())
}

/// Whether a new branch name is valid: non-empty, and not starting with `agit-`.
///
/// The prefix belongs to snapshot ids: in `agit clone x/y@Z`, a Z starting with `agit-` is a
/// version and anything else is a branch, so a branch named `agit-foo` always resolves on that path
/// to a version that does not exist. The character set is not policed here — branch names may carry
/// `/` and whatever else git allows, `git check-ref-format` is the source of that rule, and git
/// itself applies it when the ref is created.
pub fn valid_branch_name(name: &str) -> Result<()> {
    let n = name.trim();
    if n.is_empty() {
        bail!("branch name must not be empty");
    }
    if n.starts_with(crate::domain::meta::ID_PREFIX) {
        bail!(
            "branch names must not start with `agit-` — that prefix is reserved for version ids ({n})"
        );
    }
    Ok(())
}

/// Spawn a git subprocess. **The only place in this file that builds a git command.**
///
/// # Why it funnels into one construction site
///
/// Every git subprocess must carry `--no-replace-objects`, and `git_opt` / `git_bytes` each have
/// many callers of their own. Adding it site by site (let alone call site by call site) is a
/// contract enforced by remembering: missing one **has no symptom at all**, it only lets the
/// scanner quietly read a replacement object on that path. With one construction site, "could a
/// newly added path miss it" stops being a question answered by conscientiousness — a new path
/// either goes through here or cannot start git at all.
/// `every_git_subprocess_disables_replace_resolution` guards this.
///
/// # What `--no-replace-objects` turns off
///
/// git honours `refs/replace/*` by default, and that is **two** deceptions, not one:
///
/// - `git replace <real> <replacement>`: reading that OID yields the replacement's body.
///   Enumeration proceeds as usual and the content read is fake.
/// - `git replace --graft <tip>` (with no parent): the parent pointer is swapped out.
///   `rev-list --branches` collapses the whole history into a single line, and the commits deep in
///   it **cannot even be enumerated**.
///
/// The refspec of [`crate::commands::push`] is `refs/heads/<branch>` and `refs/tags/*`, **not**
/// `refs/replace/*` — the replacement never leaves this machine and what goes out is the real
/// object. So the local gate reads the replacement, reports clean, and lets it through, while the
/// secret travels intact over the network and lands on the hub. This is not "the client erring on
/// the conservative side"; it points the other way.
///
/// Nor is this only an attack surface: `git filter-repo` and `git replace --graft` both leave
/// `refs/replace/*` behind, and a repository whose secrets were just scrubbed with filter-repo has
/// exactly this shape.
///
/// # Why a command-line flag and not `GIT_NO_REPLACE_OBJECTS=1`
///
/// The two are equivalent, but **a typo fails in opposite directions**: misspell the flag → git
/// reports an unknown option, exits non-zero, and the caller treats it as a failure; misspell one
/// letter of the variable name → silently ignored, green as usual, which is the shape of this bug
/// itself. A gate must fail toward "blocked", never toward "allowed".
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    // Global option slot — must come **before** the subcommand; git rejects it after.
    cmd.arg("--no-replace-objects");
    // Signing is turned off explicitly: an agent repo is agit's own internal record, authorship
    // comes from the sign-in credentials, and it carries none of the gpg policy of the user's code
    // repo. hooks and agitd both commit in an environment with **no tty** — on a machine with a
    // global `commit.gpgsign=true`, gpg cannot ask for the passphrase and fails outright, breaking
    // the whole automatic settlement chain (`gpg: cannot open '/dev/tty'`).
    cmd.args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"]);
    cmd
}

/// The body of an object — **or "I did not read it"**.
///
/// The two variants are not two spellings of one thing, they are two things: bytes that were read,
/// and a statement of how many bytes went unread. Collapsing them into one `Option<&[u8]>`
/// compiles, but it makes "not read" and "empty object" look identical — and to the verdict of a
/// scanning gate those two mean exactly opposite things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectBody<'a> {
    /// The body, taken by the byte count git itself reports.
    Read(&'a [u8]),
    /// Over the caller's cap, **not one byte read in**: only the length git reports is known.
    TooLarge(usize),
}

/// Take the branch name from the **full** refname; anything that is not a branch yields `None`.
///
/// The test runs on the full refname, never on `%(refname:short)`: git abbreviates
/// `refs/remotes/origin/HEAD` to **`origin`**, not `origin/HEAD` — so a "filter out HEAD" check
/// never matches, and a phantom branch named `origin` slips into every answer to "which branches
/// are there" (`agit log <owner/repo>` lists it).
///
/// It also removes a second ambiguity: under full refnames, a local branch genuinely named
/// `origin/x` (`refs/heads/origin/x`) no longer collides with the remote `refs/remotes/origin/x`.
fn branch_name_of(refname: &str) -> Option<String> {
    let name = refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/remotes/origin/"))?;
    // `refs/remotes/origin/HEAD` is that symbolic ref, not a branch.
    (!name.is_empty() && name != "HEAD").then(|| name.to_string())
}

/// A repo in the shape of a git repository.
/// The git config key for the repo-level first-publish visibility preference, see
/// [`Repo::visibility_preference`].
pub const VISIBILITY_PREF_KEY: &str = "agit.visibility";

/// One entry of `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The checked-out branch; None when detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    /// The main checkout (the head of the list).
    pub primary: bool,
}

/// The common git directory for a checkout root, with no git subprocess.
///
/// The main checkout's `.git` is a directory and is already it; a linked worktree's `.git` is a
/// `gitdir: <common>/worktrees/<id>` file, and dropping the last two segments returns to the common
/// directory. State that is "one per repository" — lock files, the secret dictionary — lives in the
/// common directory, and a path built from any worktree has to land in the same place, or two
/// worktrees each watch their own lock.
pub fn common_git_dir(root: &Path) -> PathBuf {
    let dot = root.join(".git");
    if dot.is_dir() {
        return dot;
    }
    let Ok(text) = std::fs::read_to_string(&dot) else {
        return dot;
    };
    let Some(rest) = text.trim().strip_prefix("gitdir:") else {
        return dot;
    };
    let gitdir = PathBuf::from(rest.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        root.join(gitdir)
    };
    if let Some(worktrees) = gitdir.parent()
        && worktrees.file_name().is_some_and(|n| n == "worktrees")
        && let Some(common) = worktrees.parent()
    {
        return common.to_path_buf();
    }
    gitdir
}

#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Repo { root: root.into() }
    }

    /// Open it when it already exists, otherwise None.
    ///
    /// **A relative path never opens.**
    ///
    /// The root here often comes from `config::repo_dir(owner, name).unwrap_or_default()`, and
    /// `unwrap_or_default()` hands back an **empty** `PathBuf`. `"".join(".git")` is the relative
    /// path `.git`, so `.exists()` asks the current working directory — and an error meaning "the
    /// repository path cannot be computed" turns into "quietly operating on whatever repository the
    /// user is standing in".
    ///
    /// The check is `is_absolute` rather than a fix to each `unwrap_or_default()`: this is one gate
    /// shared by every call site, and "someone forgot to handle the Err" happens again. agit's own
    /// repository paths are always `~/.agit/repos/...`, absolute, so this gate catches no real
    /// usage.
    pub fn open(root: impl Into<PathBuf>) -> Option<Repo> {
        let r = root.into();
        if !r.is_absolute() {
            return None;
        }
        r.join(".git").exists().then(|| Repo::at(r))
    }

    /// Open it when it already exists, otherwise initialize it.
    ///
    /// `agit commit` takes this path: the first commit is the moment this repo comes into
    /// existence, and no remote has to exist first (the remote is created at `agit push`).
    pub fn open_or_init(root: &Path) -> Result<Repo> {
        match Repo::open(root) {
            Some(r) => Ok(r),
            None => Repo::init(root),
        }
    }

    /// Initialize.
    ///
    /// `main` is named explicitly as the initial branch: git's default varies with version and with
    /// user configuration, and a branch name that differs across machines makes push/pull fail
    /// inexplicably.
    ///
    /// `--initial-branch` arrived in git 2.28, while Ubuntu 20.04 ships 2.25. Failing outright on
    /// those machines means the user meets, at their first `agit import`, a failure that never
    /// mentions the git version. So old git falls back to "init first, then move HEAD" — the same
    /// result, one step more.
    pub fn init(root: &Path) -> Result<Repo> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("cannot create {}", root.display()))?;
        let out = git_command()
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .output()
            .context("git init failed (is git on PATH?)")?;
        if !out.status.success() {
            init_legacy(root)?;
        }
        let r = Repo::at(root);
        r.ensure_committer()?;
        Ok(r)
    }

    /// Guarantee a usable git committer identity.
    ///
    /// This is a repository agit manages itself and the user never commits into it by hand, so the
    /// identity has a local fallback, which keeps "the user configured no global git identity" from
    /// meaning "no version can be recorded".
    ///
    /// The check runs ahead of every commit, not only at init: a repository obtained by
    /// `agit clone` never goes through init, and it gets committed to just the same.
    pub fn ensure_committer(&self) -> Result<()> {
        if self.committer().is_none() {
            self.git(&["config", "user.name", "agit"])?;
            self.git(&["config", "user.email", "agit@localhost"])?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The absolute path of a file inside this checkout's git directory
    /// (`git rev-parse --git-path`).
    ///
    /// A linked worktree's `.git` is a file, so building `root/.git/<name>` there is ENOTDIR; every
    /// piece of per-worktree state (index, HEAD, checkout journal) is fetched through here.
    pub fn git_path(&self, name: &str) -> Result<PathBuf> {
        let value = self.git(&["rev-parse", "--git-path", name])?;
        Ok(self.absolute(value.trim()))
    }

    /// The git directory shared by every worktree (object store, refs, config, lock files).
    pub fn common_dir(&self) -> Result<PathBuf> {
        let value = self.git(&["rev-parse", "--git-common-dir"])?;
        Ok(self.absolute(value.trim()))
    }

    /// The main checkout's root: the parent of the common git directory. On a linked worktree it
    /// gives the main checkout; on the main checkout it gives itself.
    pub fn common_root(&self) -> Result<PathBuf> {
        let dir = self.common_dir()?;
        Ok(dir.parent().map(Path::to_path_buf).unwrap_or(dir))
    }

    /// Whether this is a linked worktree made by `git worktree add` (`.git` is a file, not a
    /// directory).
    pub fn is_linked_worktree(&self) -> bool {
        self.root.join(".git").is_file()
    }

    fn absolute(&self, value: &str) -> PathBuf {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        }
    }

    /// `git worktree list --porcelain`: the first entry is always the main checkout.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let text = self.git(&["worktree", "list", "--porcelain"])?;
        let mut out = Vec::new();
        for block in text.split("\n\n") {
            let mut path: Option<PathBuf> = None;
            let mut branch = None;
            let mut head = None;
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(p));
                } else if let Some(h) = line.strip_prefix("HEAD ") {
                    head = Some(h.to_string());
                } else if let Some(b) = line.strip_prefix("branch ") {
                    branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
                }
            }
            if let Some(path) = path {
                let primary = out.is_empty();
                out.push(Worktree {
                    path,
                    branch,
                    head,
                    primary,
                });
            }
        }
        Ok(out)
    }

    /// The worktree that has this branch checked out (main or linked).
    pub fn worktree_of(&self, branch: &str) -> Result<Option<Worktree>> {
        Ok(self
            .worktrees()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch)))
    }

    /// Create a linked worktree for a branch that already exists.
    pub fn add_worktree(&self, path: &Path, branch: &str) -> Result<Repo> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let path_arg = path.to_string_lossy().into_owned();
        self.git(&["worktree", "add", "--quiet", "--", &path_arg, branch])?;
        Ok(Repo::at(path))
    }

    /// Remove a linked worktree (the directory too; uncommitted changes inside go with it).
    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        let path_arg = path.to_string_lossy().into_owned();
        self.git(&["worktree", "remove", "--force", "--", &path_arg])?;
        Ok(())
    }

    /// Move a linked worktree's directory.
    pub fn move_worktree(&self, from: &Path, to: &Path) -> Result<()> {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let from_arg = from.to_string_lossy().into_owned();
        let to_arg = to.to_string_lossy().into_owned();
        self.git(&["worktree", "move", "--", &from_arg, &to_arg])?;
        Ok(())
    }

    /// Clear the worktree registrations whose directory is gone.
    pub fn prune_worktrees(&self) -> Result<()> {
        self.git(&["worktree", "prune"])?;
        Ok(())
    }

    /// Once the main checkout has been moved, a linked worktree's `.git` file still points at the
    /// old address; running `worktree repair` from the main checkout side makes git rewrite the
    /// pointer from the gitdir it registered itself. Old git without this subcommand is skipped
    /// silently — there is nothing else to do there.
    pub fn repair_worktrees(&self) {
        let _ = self.git_opt(&["worktree", "repair"]);
    }

    /// Whether the working tree holds anything uncommitted: changes to tracked files **and
    /// untracked files** both count (ignored files do not).
    ///
    /// An untracked file counts as dirty because a memory/ file just written and not yet added has
    /// exactly this shape: judge it clean, swap the checkout, and that file stays behind in a
    /// directory nobody looks at again.
    pub fn is_clean(&self) -> Result<bool> {
        let status = self.git(&["status", "--porcelain", "--untracked-files=all"])?;
        Ok(status.trim().is_empty())
    }

    /// Spawn a git subprocess on this repo: `git --no-replace-objects -C <root> ...`.
    ///
    /// Every global option is added here and the caller appends the subcommand. Rationale in
    /// [`git_command`].
    fn cmd(&self) -> Command {
        let mut cmd = git_command();
        cmd.arg("-C").arg(&self.root);
        cmd
    }

    /// Run git and return stdout.
    ///
    /// Arguments go through an array, not a concatenated string: spaces, `$`, backticks and
    /// semicolons inside a path are never interpreted, and injection is impossible.
    ///
    /// Signing is turned off explicitly: an agent repo is agit's own internal record, authorship
    /// comes from the sign-in credentials, and it carries none of the gpg policy of the user's code
    /// repo. hooks and agitd both run commit in an environment with **no tty** — on a machine with
    /// a global `commit.gpgsign=true`, gpg cannot ask for the passphrase and fails outright,
    /// breaking the whole automatic settlement chain (`gpg: cannot open '/dev/tty'`).
    pub fn git(&self, args: &[&str]) -> Result<String> {
        let out = self
            .cmd()
            .args(args)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Run git and cut stdout into records at `sep`, **streaming**, handing each one to the
    /// callback.
    ///
    /// # Why streaming
    ///
    /// `git_opt` / `git_bytes` go through `Command::output()`, which brings all of stdout into
    /// memory first. For a command like `rev-list`, whose output grows linearly with history, that
    /// means the first publish of a deep-history repository can eat the memory before the scan even
    /// starts.
    ///
    /// This holds **one record** at a time. A callback returning `Err` aborts the read (the
    /// subprocess is killed).
    ///
    /// Records end with `sep`; a trailing piece with no closing `sep` is handed over too (git's
    /// output usually ends with a newline, that piece is empty, and the caller skips it).
    ///
    /// # Splitting on a separator holds only when **the separator cannot occur inside a record**
    ///
    /// That premise is checked case by case. The OIDs `rev-list` gives are hex, so splitting on
    /// `\n` is unambiguous; but **an object's body may contain any byte**, NUL included
    /// (`git hash-object -t commit --literally` makes one and `git push` accepts it). So reading a
    /// body does not go through here, it goes through [`Repo::git_cat_file_batch`] — which frames
    /// by the **byte count** git itself reports.
    pub fn git_stream_split(
        &self,
        args: &[&str],
        sep: u8,
        mut on_record: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        use std::io::Read;
        let mut child = self
            .cmd()
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;

        let mut buf = Vec::with_capacity(64 * 1024);
        let mut chunk = [0u8; 64 * 1024];
        let mut fail: Option<anyhow::Error> = None;
        loop {
            let n = match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    fail = Some(e.into());
                    break;
                }
            };
            buf.extend_from_slice(&chunk[..n]);
            // Hand over every complete record, leaving only the trailing partial one in the
            // buffer.
            while let Some(i) = buf.iter().position(|c| *c == sep) {
                let rec: Vec<u8> = buf.drain(..=i).take(i).collect();
                if let Err(e) = on_record(&rec) {
                    fail = Some(e);
                    break;
                }
            }
            if fail.is_some() {
                break;
            }
        }
        if fail.is_some() {
            let _ = child.kill();
        } else if !buf.is_empty() {
            fail = on_record(&buf).err();
        }
        let status = child.wait()?;
        if let Some(e) = fail {
            return Err(e);
        }
        if !status.success() {
            anyhow::bail!("git {} failed", args.join(" "));
        }
        Ok(())
    }

    /// Hand a batch of OIDs to `git cat-file --batch` and give the callback `(oid, kind, body)` one
    /// at a time.
    ///
    /// The body is **raw bytes**, with no UTF-8 conversion — conversion belongs to the caller and
    /// applies to a single body, for the reason in [`Repo::git_bytes`]. A callback returning `Err`
    /// aborts the read (the subprocess is killed).
    ///
    /// # `max_bytes`: an object over the line **is never asked for its body**
    ///
    /// [`Repo::git_cat_file_batch_check`] runs first (header only, no body decompression) to get
    /// every OID's length; the ones over the line **never enter** the input of `--batch`, and only
    /// the eligible ones are asked for a body.
    ///
    /// "Decide on the header line, discard the body by length" cannot do this, whatever a comment
    /// claims: the `--batch` protocol is "give an OID, get a header plus the whole body", and the
    /// discarded bytes still have to be decompressed by git, still have to fill the pipe, still
    /// have to be read block by block by this process. The only saving is one `Vec` allocation,
    /// with not an ounce of I/O or CPU saved — so a run of oversized objects still manufactures
    /// unbounded work, all the more so because the estimator (`estimate_object_bytes`) deliberately
    /// does **not** count oversized objects against the budget, so they cannot even exhaust it.
    ///
    /// The observed cost of crossing the line: one 300 MiB blob in history, already deleted
    /// (absent from the working tree and from the tip tree, but still reachable), takes a scan's
    /// maxRSS from 52 MB to 682 MB. The server side has `blob_bytes` for the same thing; this is
    /// its local counterpart.
    ///
    /// Handing out [`ObjectBody::TooLarge`] instead of skipping silently is **deliberate**: a skip
    /// moves the object out of the scan surface with no way for the caller to know — and something
    /// never scanned gets reported as clean. Forcing every caller to catch it in the type leaves
    /// no path that can forget to account for it. Callback order is still the order in which the
    /// caller gave the OIDs; the ones over the line are handed out in place, not piled at the
    /// front or the back.
    ///
    /// # Why not `--batch-command`
    ///
    /// The `info` / `contents` subcommands of `cat-file --batch-command` do the filtering and the
    /// reading in **one** process, sparing the extra `--batch-check` started here. But it arrives
    /// in git **2.36**, while this product's pinned floor is **2.28** (README and
    /// `docs/01_setup.md` both say so, for the reason in [`Repo::init`]: Ubuntu 20.04 ships 2.25).
    /// Lifting the floor from 2.28 to 2.36 to spare one process buys some users a scan that will
    /// not run at all — and a gate that cannot run is no gate.
    ///
    /// As for "why not reuse the `--batch-check` of the estimating pass on the same batch": that
    /// pass is a **gate**, and it has to complete in full before any body is read (over budget, not
    /// one object is read); keeping its result around for the scanning pass means holding an
    /// `oid → length` table of the same order as the repository, which is exactly what
    /// `OBJECT_BATCH` exists to avoid. What is reused is this **function**, not the result of one
    /// of its calls.
    ///
    /// # Why framing is by length and not by separator
    ///
    /// Because **an object's body may contain any byte**, including whichever one you pick as the
    /// separator. A NUL inside a commit body is not a theoretical possibility:
    /// `git hash-object -t commit --literally` makes one, and `git push` accepts it (stock git only
    /// records it in `fsck`, as `nulInCommit`). Split on a separator and the record breaks at the
    /// first NUL, the suffix is taken as the start of the next record, and everything after that is
    /// off by one — **and nothing errors**. One such object lets every object behind it slip
    /// silently out of the scan surface.
    ///
    /// Each `cat-file --batch` record is `<oid> <kind> <bytes>\n<body>\n`: the body is taken by
    /// that byte count, independent of what the body holds. Finding the header line's own boundary
    /// by `\n` is safe — it consists only of hex, a type name and decimal digits. (The tag path is
    /// the same, framing by `%(raw:size)`.)
    ///
    /// # stdin has to be written from another thread
    ///
    /// `cat-file --batch` **reads stdin and writes stdout at the same time**. Writing every OID
    /// first and only then reading deadlocks once there are enough OIDs: git stops to wait for us
    /// to read once its stdout pipe is full, while we are blocked writing stdin, each waiting on
    /// the other. So the write goes in another thread and this thread only reads.
    pub fn git_cat_file_batch(
        &self,
        oids: Vec<String>,
        max_bytes: usize,
        mut on_object: impl FnMut(&str, &str, ObjectBody<'_>) -> Result<()>,
    ) -> Result<()> {
        use std::io::{Read, Write};
        if oids.is_empty() {
            return Ok(());
        }

        // ── Filter first, read second ──
        //
        // `slots` corresponds one to one, in order, with the `oids` the caller gave:
        // `Some((oid, kind, length))` is over the line (its body is never requested), `None` is
        // eligible (its body streams back in order from the output of `--batch`). Both sides share
        // the order, so the callback can hand things out in the original order, and "which one is
        // this" never depends on the OID git echoes being literally the one we wrote in.
        let mut slots: Vec<Option<(String, String, u64)>> = Vec::with_capacity(oids.len());
        let eligible: Vec<String> = if max_bytes == usize::MAX {
            // With no cap nothing is filtered out — that `--batch-check` pass is pure overhead.
            slots.resize(oids.len(), None);
            oids
        } else {
            let mut keep = Vec::with_capacity(oids.len());
            let cap = max_bytes as u64;
            self.git_cat_file_batch_check(oids, |oid, kind, size| {
                // `missing` still goes to `--batch`: an object that cannot be read gets its
                // "what we asked for is not there" verdict from that side; filtering it out
                // quietly here turns it into "there is nothing here".
                if kind != "missing" && size > cap {
                    slots.push(Some((oid.to_string(), kind.to_string(), size)));
                } else {
                    slots.push(None);
                    keep.push(oid.to_string());
                }
                Ok(())
            })?;
            keep
        };

        // The whole batch is over the line: not one git process has to start.
        if eligible.is_empty() {
            for slot in slots.iter().flatten() {
                let (oid, kind, size) = slot;
                on_object(
                    oid,
                    kind,
                    ObjectBody::TooLarge(usize::try_from(*size).unwrap_or(usize::MAX)),
                )?;
            }
            return Ok(());
        }

        let mut child = self
            .cmd()
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to run git cat-file --batch")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let writer = std::thread::spawn(move || {
            let mut w = std::io::BufWriter::new(stdin);
            for oid in &eligible {
                // A failed write only means the far side has gone (we killed it, or it exited
                // on its own) — the reading side gives the real verdict, so this side just
                // finishes quietly.
                if w.write_all(oid.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
                    return;
                }
            }
            let _ = w.flush();
            // w is dropped here → stdin closes → git exits after reading EOF.
        });

        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut chunk = [0u8; 64 * 1024];
        let mut fail: Option<anyhow::Error> = None;
        // How far into `slots` we are. git answers in the order it was fed, so "which slot does
        // the next record belong to" only walks forward, stepping over the over-the-line slots on
        // the way — it never compares the OID git echoes against the one we wrote in (feeding
        // something like `HEAD` echoes back the full-length OID).
        let mut cursor = 0usize;
        loop {
            // Hand over the records already complete in the buffer, leaving a partial one to
            // wait for the next chunk.
            while let Some(nl) = buf.iter().position(|c| *c == b'\n') {
                let head = String::from_utf8_lossy(&buf[..nl]).into_owned();
                let mut fields = head.split(' ');
                let (Some(oid), Some(kind), Some(size)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    // `<oid> missing`: the object we asked for cannot be read. That is not
                    // "there is nothing here".
                    fail = Some(anyhow::anyhow!("git cat-file cannot read object: {head}"));
                    break;
                };
                let Ok(size) = size.parse::<usize>() else {
                    fail = Some(anyhow::anyhow!(
                        "cannot parse the object length git reported: {head}"
                    ));
                    break;
                };
                // After filtering this cannot hold. If it does, the two `cat-file` passes report
                // different lengths (the repository changed between the two calls), and the answer
                // then is to **fail** rather than read on: reading on means an unbounded allocation
                // right here, which is the very thing this cap blocks.
                if size > max_bytes {
                    fail = Some(anyhow::anyhow!(
                        "git reported two object lengths ({head}, cap {max_bytes} bytes) — the repository changed while it was read"
                    ));
                    break;
                }
                // git adds a newline of its own after the body.
                let end = nl + 1 + size;
                if buf.len() < end + 1 {
                    break;
                }
                // The over-the-line slots ahead of this record are handed out first, so the
                // order the callback sees is still the order the caller gave the OIDs in, not
                // "eligible first, over-the-line piled at the end".
                while let Some(Some((o, k, n))) = slots.get(cursor) {
                    let body = ObjectBody::TooLarge(usize::try_from(*n).unwrap_or(usize::MAX));
                    if let Err(e) = on_object(o, k, body) {
                        fail = Some(e);
                        break;
                    }
                    cursor += 1;
                }
                if fail.is_some() {
                    break;
                }
                cursor += 1; // this record's own slot
                if let Err(e) = on_object(oid, kind, ObjectBody::Read(&buf[nl + 1..end])) {
                    fail = Some(e);
                    break;
                }
                buf.drain(..end + 1);
            }
            if fail.is_some() {
                break;
            }
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    fail = Some(e.into());
                    break;
                }
            }
        }
        // The output stopped in the middle of a record: an incomplete read is **not** "there is
        // nothing more".
        if fail.is_none() && !buf.is_empty() {
            fail = Some(anyhow::anyhow!(
                "git cat-file output stopped in the middle of a record ({} bytes left over)",
                buf.len()
            ));
        }
        // The over-the-line slots at the very end: the output of `--batch` holds none of them, so
        // the loop above never reaches them.
        if fail.is_none() {
            while let Some(Some((o, k, n))) = slots.get(cursor) {
                let body = ObjectBody::TooLarge(usize::try_from(*n).unwrap_or(usize::MAX));
                if let Err(e) = on_object(o, k, body) {
                    fail = Some(e);
                    break;
                }
                cursor += 1;
            }
        }
        // Every OID asked about has to be accounted for. When git answers with fewer records and
        // still ends cleanly, neither test above notices (`buf` is empty and the process may exit
        // 0) — and an unaccounted-for object reads, at the caller, as "scanned, no hit".
        if fail.is_none() && cursor != slots.len() {
            fail = Some(anyhow::anyhow!(
                "git cat-file answered for {} fewer objects (asked {}, answered {})",
                slots.len() - cursor,
                slots.len(),
                cursor
            ));
        }
        if fail.is_some() {
            let _ = child.kill();
        }
        // After the kill, the write in the writer thread gets EPIPE and returns; it does not hang.
        let _ = writer.join();
        let status = child.wait()?;
        if let Some(e) = fail {
            return Err(e);
        }
        if !status.success() {
            anyhow::bail!("git cat-file --batch failed");
        }
        Ok(())
    }

    /// `git cat-file --batch-check`: **header only, no body**.
    ///
    /// The callback gets `(oid, kind, bytes)`. It exists because "how big is this batch of objects
    /// altogether" is far cheaper than "what is in them" — it is the premise for computing the work
    /// before deciding whether to do it, and it is the batched answer to "is this oid here
    /// locally".
    ///
    /// For an object it cannot read, git prints `<input> missing` and the process still exits
    /// successfully. So that is handed out as **data** (`kind = "missing"`, length 0), not as a
    /// failure: when the caller asks exactly "is it there", an error is the wrong answer.
    ///
    /// stdin is likewise written from another thread, for the reason in
    /// [`Repo::git_cat_file_batch`].
    pub fn git_cat_file_batch_check(
        &self,
        oids: Vec<String>,
        mut on_object: impl FnMut(&str, &str, u64) -> Result<()>,
    ) -> Result<()> {
        use std::io::Write;
        if oids.is_empty() {
            return Ok(());
        }
        let mut child = self
            .cmd()
            .args(["cat-file", "--batch-check"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to run git cat-file --batch-check")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let writer = std::thread::spawn(move || {
            let mut w = std::io::BufWriter::new(stdin);
            for oid in &oids {
                if w.write_all(oid.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
                    return;
                }
            }
            let _ = w.flush();
        });

        // This path's output is only hex, type names and decimal digits, so splitting by line is
        // unambiguous — the reason the body path has to frame by length (a body may contain any
        // byte) does not hold here.
        let mut fail: Option<anyhow::Error> = None;
        {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        fail = Some(e.into());
                        break;
                    }
                };
                let mut f = line.split(' ');
                let Some(oid) = f.next() else { continue };
                let kind = f.next().unwrap_or("missing");
                let size = f.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                if let Err(e) = on_object(oid, kind, size) {
                    fail = Some(e);
                    break;
                }
            }
        }
        if fail.is_some() {
            let _ = child.kill();
        }
        let _ = writer.join();
        let status = child.wait()?;
        if let Some(e) = fail {
            return Err(e);
        }
        if !status.success() {
            anyhow::bail!("git cat-file --batch-check failed");
        }
        Ok(())
    }

    /// Run git and give **raw bytes**: no UTF-8 conversion and no trim.
    ///
    /// Required wherever the output is cut by the byte length git reports: `from_utf8_lossy`
    /// replaces every invalid byte with a three-byte U+FFFD, so the string length and the length
    /// git reports are **no longer equal** and cutting by length goes off by one; worse, a
    /// `split_at` landing inside a U+FFFD panics outright.
    pub fn git_bytes(&self, args: &[&str]) -> Option<Vec<u8>> {
        let out = self.cmd().args(args).output().ok()?;
        out.status.success().then_some(out.stdout)
    }

    /// Run Git and preserve raw stdout while propagating every spawn/exit failure.
    pub fn git_bytes_result(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = self
            .cmd()
            .args(args)
            .output()
            .with_context(|| format!("git {} failed to start", args.join(" ")))?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Run git and hand the exit code back unchanged (along with stdout and stderr).
    ///
    /// For probes that **speak through the exit code**: `show-ref --verify --quiet` uses 1 for
    /// "there is no such ref" and any other non-zero value for "it could not answer at all" (not a
    /// repository, the ref store cannot be read). [`Repo::git`] treats every non-zero as a failure,
    /// which on commands like these cannot tell "no" from "don't know".
    pub fn git_status(&self, args: &[&str]) -> Result<(Option<i32>, String, String)> {
        let out = self
            .cmd()
            .args(args)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;
        Ok((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        ))
    }

    /// The first-publish visibility preference (`agit init --private` writes it, `agit push`
    /// reads it).
    ///
    /// It lives in the repository's own `.git/config` (key [`VISIBILITY_PREF_KEY`]): it is this one
    /// repository's business, travels with the repository, and takes up no global configuration
    /// surface. It is read once, at first publish — once visibility is decided on the hub, later
    /// pushes no longer consult it.
    ///
    /// The read is scoped with `--local`: a same-named key in the global or system config does not
    /// count. Unscoped, one git setting that applies to every repository impersonates "this
    /// repository's preference" and outranks the user's choice in `push.visibility`, and a first
    /// push can make a full transcript public by accident.
    pub fn visibility_preference(&self) -> Option<String> {
        self.git_opt(&["config", "--local", "--get", VISIBILITY_PREF_KEY])
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    pub fn set_visibility_preference(&self, value: &str) -> Result<()> {
        self.git(&["config", VISIBILITY_PREF_KEY, value])?;
        Ok(())
    }

    /// Run git, treating a failure as "not there" (for queries).
    pub fn git_opt(&self, args: &[&str]) -> Option<String> {
        let out = self.cmd().args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    pub fn committer(&self) -> Option<(String, String)> {
        let n = self.git_opt(&["config", "user.name"])?.trim().to_string();
        let e = self.git_opt(&["config", "user.email"])?.trim().to_string();
        (!n.is_empty() && !e.is_empty()).then_some((n, e))
    }

    pub fn head_short(&self) -> Option<String> {
        self.git_opt(&["rev-parse", "--short", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn commit_count(&self) -> usize {
        self.git_opt(&["rev-list", "--count", "HEAD"])
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The URL of a remote.
    pub fn remote(&self, name: &str) -> Option<String> {
        self.git_opt(&["remote", "get-url", name])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn remote_url(&self) -> Option<String> {
        self.remote(ORIGIN)
    }

    /// The URL of the source copy (`upstream`). A read-only clone and an agent you built yourself
    /// have none.
    pub fn upstream_url(&self) -> Option<String> {
        self.remote(UPSTREAM)
    }

    /// Set a remote (add it when absent, update it when present).
    ///
    /// The URL changes: switching hub, or a read-only clone being promoted into your own copy (at
    /// which point origin moves from their copy to yours). So "already exists" is not an error.
    pub fn set_remote_named(&self, name: &str, url: &str) -> Result<()> {
        if self.remote(name).is_some() {
            self.git(&["remote", "set-url", name, url])?;
        } else {
            self.git(&["remote", "add", name, url])?;
        }
        Ok(())
    }

    pub fn set_remote(&self, url: &str) -> Result<()> {
        self.set_remote_named(ORIGIN, url)
    }

    pub fn set_upstream(&self, url: &str) -> Result<()> {
        self.set_remote_named(UPSTREAM, url)
    }

    pub fn current_branch(&self) -> Option<String> {
        self.git_opt(&["symbolic-ref", "--short", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// (ahead, behind) relative to the **tracking branch**. No tracking branch yields None.
    ///
    /// `@{upstream}` is git's tracking branch (usually `origin/main`), **not** the remote named
    /// `upstream`. So this method answers "how far apart are origin and I", and adding an
    /// `upstream` remote or not makes no difference to it.
    pub fn ahead_behind(&self) -> Option<(usize, usize)> {
        self.git_opt(&["rev-parse", "--abbrev-ref", "@{upstream}"])?;
        let out = self.git_opt(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
        let mut it = out.split_whitespace();
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    }

    pub fn add_all(&self) -> Result<()> {
        self.git(&["add", "-A"])?;
        Ok(())
    }

    /// Commit. With no changes it returns Ok(false) and produces no empty commit.
    pub fn commit(&self, message: &str) -> Result<bool> {
        self.ensure_committer()?;
        // `--cached` compares the index against HEAD; success (exit code 0) means no difference.
        let staged = self
            .cmd()
            .args(["diff", "--cached", "--quiet"])
            .status()
            .context("git diff --cached failed")?;
        if staged.success() {
            return Ok(false);
        }
        self.git(&["commit", "-m", message])?;
        Ok(true)
    }

    /// Every version tag, newest to oldest by creation time.
    ///
    /// By time and not lexically: hashes have no order.
    pub fn versions(&self) -> Vec<(String, String, String)> {
        let out = self
            .git_opt(&[
                "for-each-ref",
                "--sort=-creatordate",
                "--format=%(refname:short)\t%(creatordate:short)\t%(subject)",
                "refs/tags/",
            ])
            .unwrap_or_default();
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let mut it = l.split('\t');
                (
                    it.next().unwrap_or("").to_string(),
                    it.next().unwrap_or("").to_string(),
                    it.next().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    /// The local branches pointing at the commit HEAD is on.
    ///
    /// This answers the question a detached HEAD raises: "is there a branch at this position, and
    /// which one". With exactly one, switching back changes no commit; it only makes HEAD a
    /// symbolic ref again.
    pub fn branches_at_head(&self) -> Vec<String> {
        self.git_opt(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "--points-at",
            "HEAD",
            "refs/heads/",
        ])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect()
    }

    /// Switch to a local branch that already exists.
    pub fn switch(&self, branch: &str) -> Result<()> {
        self.git(&["checkout", "--quiet", branch])?;
        Ok(())
    }

    /// Every branch name in this repo: the local ones, plus the ones that exist only on the
    /// remote.
    ///
    /// The answer to "which session lines does this agent have". Asking only
    /// `refs/remotes/origin/*` counts **nothing at all before the first push** — a repository just
    /// built by `agit init` plainly holds branches, and `agit log <owner/repo>` says "no branches"
    /// about them. Anything that genuinely wants only the remote side (building local tracking
    /// branches after a clone) goes through [`Repo::remote_branches`].
    pub fn branches(&self) -> Vec<String> {
        // One `for-each-ref` takes both patterns, rather than two calls merged afterwards:
        // `same_repo_branches()` calls this function once for **every** local repo, and
        // `docs/07_tui.md` §4.1 watches exactly this "multiplied by the repo count" cost.
        let mut all: Vec<String> = self
            .git_opt(&[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/",
                "refs/remotes/origin/",
            ])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| branch_name_of(l.trim()))
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// Remote branch names (`HEAD` excluded).
    ///
    /// Only "do something following the remote" belongs here. To answer "which branches are there"
    /// use [`Repo::branches`].
    pub fn remote_branches(&self) -> Vec<String> {
        self.git_opt(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes/origin/",
        ])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| branch_name_of(l.trim()))
        .collect()
    }

    pub fn has_ref(&self, r: &str) -> bool {
        self.git_opt(&["rev-parse", "--verify", "--quiet", r])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.has_ref(&format!("refs/tags/{tag}"))
    }

    /// Create a tag. Already present is a no-op (one snapshot id points at one state).
    pub fn tag(&self, tag: &str) -> Result<()> {
        if !self.has_tag(tag) {
            self.git(&["tag", tag])?;
        }
        Ok(())
    }

    /// The snapshot tags on HEAD.
    ///
    /// A snapshot id is the tag name, so this answers "which version is checked out right now".
    pub fn tags_at_head(&self) -> Vec<String> {
        self.git_opt(&["tag", "--points-at", "HEAD"])
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|t| t.starts_with(crate::domain::meta::ID_PREFIX))
            .map(String::from)
            .collect()
    }

    /// Local branch names, including branches that have never been pushed.
    pub fn local_branches(&self) -> Vec<String> {
        self.git_opt(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect()
    }

    /// Read the logical content of a file under a ref.
    ///
    /// On this read path `LOG` / `VIEW` mean **this line's logical event sequence**, not an entry
    /// in the tree that happens to carry that name: a v1 tree stores the event-id sequence, v0
    /// stores `session/log.jsonl` / `session/VIEW`, and both are materialized here into full
    /// Envelope JSONL. For the raw text of the blob in the tree (in v0 a root `LOG` may perfectly
    /// well be an ordinary user file) go through [`Repo::show_raw`].
    ///
    /// The whole test is delegated to [`Repo::show_result`]: "the same ref plus the same path, two
    /// functions, two different answers" is the worst way this read path can fail.
    pub fn show(&self, git_ref: &str, path: &str) -> Option<String> {
        if matches!(
            path,
            crate::domain::meta::LOG_FILE | crate::domain::meta::VIEW_FILE
        ) {
            return self.show_result(git_ref, path).ok().flatten();
        }
        self.git_opt(&["show", &format!("{git_ref}:{path}")])
    }

    /// Strict logical read. Missing paths remain distinguishable from corrupt objects, invalid
    /// UTF-8 and Git process failures.
    ///
    /// # `LOG` / `VIEW` resolve to the logical sequence, not a same-named file in the tree
    ///
    /// v0 does **not** count a root `LOG` / `VIEW` as a storage path
    /// ([`crate::domain::meta::is_storage_path_for`]), so the tree of a v0 session line can hold
    /// two leaves at once: the root `LOG` / `VIEW` user files the author committed, and that
    /// layout's real transcript, `session/log.jsonl` / `session/VIEW`.
    ///
    /// Two leaves cannot be squeezed into one return value. An early return of the form "a
    /// same-named entry in the tree wins" hands the user file, the moment it matches, to
    /// `point_view` in [`crate::commands::show`], to `required_sequence` in `export`, and to
    /// `materialize_optional` in revert / cherry-pick: point-in-time display and export print the
    /// wrong content, while revert and cherry-pick parse ordinary text as an Envelope and fail on
    /// the spot.
    ///
    /// So this read path always materializes the logical sequence by layout, regardless of whether
    /// a same-named user file sits in the tree. For the raw text of that blob (which is exactly
    /// what `agit show <ref>:LOG` wants) go through [`Repo::show_raw_result`] — that is the other
    /// read path, and each of the two answers only one question.
    pub fn show_result(&self, git_ref: &str, path: &str) -> Result<Option<String>> {
        if matches!(
            path,
            crate::domain::meta::LOG_FILE | crate::domain::meta::VIEW_FILE
        ) {
            let Some(meta_text) = self.show_raw_result(git_ref, crate::domain::meta::FILE)? else {
                return Ok(None);
            };
            let snapshot: crate::domain::meta::Meta = serde_json::from_str(&meta_text)
                .with_context(|| format!("invalid {} at {git_ref}", crate::domain::meta::FILE))?;
            let stored_path = match (snapshot.layout, path) {
                (crate::domain::meta::LayoutVersion::V0, crate::domain::meta::LOG_FILE) => {
                    crate::domain::meta::LEGACY_LOG_FILE
                }
                (crate::domain::meta::LayoutVersion::V0, crate::domain::meta::VIEW_FILE) => {
                    crate::domain::meta::LEGACY_VIEW_FILE
                }
                (_, path) => path,
            };
            if self.show_raw_result(git_ref, stored_path)?.is_none() {
                return Ok(None);
            }
            return crate::domain::storage::materialize_at(&self.root, git_ref, path).map(Some);
        }
        self.show_raw_result(git_ref, path)
    }

    /// Read the blob in the tree byte for byte (UTF-8), with no v0/v1 logical reassembly.
    pub fn show_raw(&self, git_ref: &str, path: &str) -> Option<String> {
        String::from_utf8(self.git_bytes(&["show", &format!("{git_ref}:{path}")])?).ok()
    }

    /// Strict raw blob read with a real `None` only when the exact path is absent.
    pub fn show_raw_result(&self, git_ref: &str, path: &str) -> Result<Option<String>> {
        let commit = self
            .git(&["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")])?
            .trim()
            .to_owned();
        let entry =
            self.git_bytes_result(&["ls-tree", "-z", "--full-name", &commit, "--", path])?;
        if entry.is_empty() {
            return Ok(None);
        }
        let bytes = self.git_bytes_result(&["cat-file", "blob", &format!("{commit}:{path}")])?;
        String::from_utf8(bytes)
            .with_context(|| format!("{git_ref}:{path} is not UTF-8"))
            .map(Some)
    }

    /// List the file paths under a ref.
    pub fn ls_tree(&self, git_ref: &str) -> Vec<String> {
        self.git_opt(&["ls-tree", "-r", "--name-only", git_ref])
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Strict tree listing for mutation paths where an empty tree and a Git failure are different.
    pub fn ls_tree_result(&self, git_ref: &str) -> Result<Vec<String>> {
        Ok(self
            .git(&["ls-tree", "-r", "--name-only", git_ref])?
            .lines()
            .map(String::from)
            .collect())
    }
}

/// The init path for git < 2.28: `--initial-branch` does not exist, so HEAD is pointed afterwards.
///
/// `symbolic-ref HEAD` is safe on a repository with no commits yet — HEAD already points at a
/// branch that does not exist, and only its name changes.
fn init_legacy(root: &Path) -> Result<()> {
    let out = git_command()
        .arg("init")
        .current_dir(root)
        .output()
        .context("git init failed (is git on PATH?)")?;
    if !out.status.success() {
        bail!(
            "failed to initialize repo at {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = git_command()
        .args(["symbolic-ref", "HEAD", "refs/heads/main"])
        .current_dir(root)
        .output()
        .context("git symbolic-ref failed")?;
    if !out.status.success() {
        bail!(
            "could not set initial branch of {} to main: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// The content of the root `LOG` user file — deliberately not valid Envelope JSONL.
///
/// When a logical read picks it up by mistake, revert / cherry-pick blow up parsing this line
/// instead of quietly producing a result that looks plausible.
#[cfg(test)]
pub(crate) const V0_SHADOWING_USER_LOG: &str = "this is a user file named LOG, not a transcript\n";

/// The content of the root `VIEW` user file. Same as above, deliberately not Envelope JSONL.
#[cfg(test)]
pub(crate) const V0_SHADOWING_USER_VIEW: &str =
    "this is a user file named VIEW, not a transcript\n";

/// Test fixture: one **v0 session line** whose tree holds two leaves at once — the root
/// `LOG` / `VIEW` user files the author committed, and that layout's real transcript,
/// `session/log.jsonl` / `session/VIEW`.
///
/// v0 does not count a root `LOG` / `VIEW` as a storage path
/// ([`crate::domain::meta::is_storage_path_for`]), so this coexistence is legitimate history, not
/// corruption. The two read paths each have to get it right: the logical read
/// (`export` / `revert` / `cherry-pick` / the point-in-time VIEW) takes the transcript, an explicit
/// `ref:path` takes the user file. Regressions on both sides pin to this fixture.
///
/// Returns `(repo, HEAD sha, the materialized text of that v0 transcript)`.
#[cfg(test)]
pub(crate) fn v0_repo_with_shadowing_user_files(root: &Path) -> (Repo, String, String) {
    use crate::domain::meta;

    let repo = Repo::init(root).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

    let mut snapshot = meta::Meta::new(meta::mint_session_id(), "claude-code".into(), "/w".into());
    snapshot.layout = meta::LayoutVersion::V0;
    meta::write(repo.root(), &snapshot).unwrap();

    let transcript = crate::domain::transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        "claude-code",
        &snapshot.session,
    );
    std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), &transcript).unwrap();
    std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), &transcript).unwrap();
    std::fs::write(repo.root().join(meta::LOG_FILE), V0_SHADOWING_USER_LOG).unwrap();
    std::fs::write(repo.root().join(meta::VIEW_FILE), V0_SHADOWING_USER_VIEW).unwrap();
    repo.add_all().unwrap();
    repo.commit("v0 session line carrying same-named user files")
        .unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();

    // precondition: this v0 transcript really does materialize. Otherwise the assertion "the
    // logical read takes the transcript" could hold only because the other side reads nothing.
    assert_eq!(
        crate::domain::storage::materialize_at(repo.root(), &head, meta::LOG_FILE).unwrap(),
        transcript,
        "precondition: the v0 transcript itself must materialize"
    );
    (repo, head, transcript)
}

#[cfg(test)]
mod tests {

    /// The preference is only ever the repo's own: a same-named key that git would
    /// otherwise pick up from the global or command scope must read as "not set".
    /// An implementation that dropped `--local` would let one machine-wide setting
    /// masquerade as every repo's choice.
    #[test]
    fn visibility_preference_reads_only_the_repo_scope() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("agents/alice/photo")).unwrap();
        assert_eq!(r.visibility_preference(), None);

        // A command-scope setting stands in for a non-local same-named key: it is visible to
        // every git call in this process, and `--local` does not read it.
        let out = r
            .cmd()
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", VISIBILITY_PREF_KEY)
            .env("GIT_CONFIG_VALUE_0", "public")
            .args(["config", "--get", VISIBILITY_PREF_KEY])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "public");
        let out = r
            .cmd()
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", VISIBILITY_PREF_KEY)
            .env("GIT_CONFIG_VALUE_0", "public")
            .args(["config", "--local", "--get", VISIBILITY_PREF_KEY])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "a non-local value must not read as the repo's"
        );

        r.set_visibility_preference("private").unwrap();
        assert_eq!(r.visibility_preference().as_deref(), Some("private"));
    }
    use super::*;

    #[test]
    fn init_sets_main_branch_and_fallback_identity() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        assert_eq!(
            r.git(&["symbolic-ref", "--short", "HEAD"]).unwrap(),
            "main",
            "the branch name must be explicit, never git's default"
        );
        assert!(
            r.committer().is_some(),
            "a usable committer identity must exist; without one a user with no git identity cannot publish"
        );
    }

    #[test]
    fn commit_reports_nothing_to_do_instead_of_empty_commit() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "{}").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("first").unwrap());
        assert!(
            !r.commit("second").unwrap(),
            "no changes must not produce an empty commit"
        );
        assert_eq!(r.commit_count(), 1);
    }

    #[test]
    fn git_args_with_spaces_are_not_shell_interpreted() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        let msg = "a message with spaces; $(echo pwned) `date`";
        std::fs::write(r.root().join("x.jsonl"), "{}").unwrap();
        r.add_all().unwrap();
        r.commit(msg).unwrap();
        assert_eq!(
            r.git(&["log", "-1", "--pretty=%s"]).unwrap(),
            msg,
            "arguments must not be interpreted by the shell"
        );
    }

    #[test]
    fn versions_are_ordered_by_time_not_lexically() {
        // Hashes have no lexical order, so the sort has to be by creation time.
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();
        r.git(&["tag", "agit-zzz111"]).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "2").unwrap();
        r.add_all().unwrap();
        r.commit("two").unwrap();
        r.git(&["tag", "agit-aaa222"]).unwrap();

        let v = r.versions();
        assert_eq!(v.len(), 2);
        // Newest first: agit-aaa222 is tagged later and is newer, even though it sorts first
        // lexically.
        assert_eq!(v[0].0, "agit-aaa222");
        assert_eq!(v[1].0, "agit-zzz111");
    }

    #[test]
    fn has_ref_is_quiet_about_missing_refs() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        assert!(!r.has_ref("refs/tags/agit-nope"));
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();
        r.git(&["tag", "agit-yes"]).unwrap();
        assert!(r.has_ref("refs/tags/agit-yes"));
    }

    #[test]
    fn open_requires_an_actual_repo() {
        let d = tempfile::tempdir().unwrap();
        assert!(Repo::open(d.path().join("nope")).is_none());
        let p = d.path().join("real");
        Repo::init(&p).unwrap();
        assert!(Repo::open(&p).is_some());
    }

    /// A cloned repository must be able to commit too.
    ///
    /// Carrying on after `agit clone` and committing takes this path — it does not go through
    /// `init`, so the identity fallback also has to take effect at commit time, or a user with no
    /// global git identity fails on this path. The fallback path for old git has to land on the
    /// same result as `--initial-branch=main`.
    ///
    /// This pins the function directly rather than installing a git 2.25: what can actually break
    /// is "is the branch name right after the fallback", and that is just as observable on a new
    /// git.
    #[test]
    fn legacy_init_also_lands_on_main() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old-git");
        std::fs::create_dir_all(&p).unwrap();
        init_legacy(&p).unwrap();
        let head = std::process::Command::new("git")
            .args(["symbolic-ref", "HEAD"])
            .current_dir(&p)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "refs/heads/main"
        );
    }

    #[test]
    fn a_repo_without_init_can_still_commit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("cloned");
        std::fs::create_dir_all(&p).unwrap();
        // Stand in for a clone: a plain git repository, with no identity fallback from
        // Repo::init.
        assert!(
            std::process::Command::new("git")
                .args(["init", "--initial-branch=main"])
                .current_dir(&p)
                .output()
                .unwrap()
                .status
                .success()
        );
        let r = Repo::at(&p);
        std::fs::write(p.join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("first").unwrap());
        assert_eq!(r.commit_count(), 1);
        assert!(
            r.committer().is_some(),
            "a usable identity must exist after a commit"
        );
    }

    #[test]
    fn open_or_init_is_the_first_commit_path() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("alice").join("photo");
        let a = Repo::open_or_init(&p).unwrap();
        std::fs::write(a.root().join("x.jsonl"), "1").unwrap();
        a.add_all().unwrap();
        a.commit("one").unwrap();
        // The second commit must open the same repository, not create a new one.
        let b = Repo::open_or_init(&p).unwrap();
        assert_eq!(b.commit_count(), 1);
    }

    #[test]
    fn tagging_is_idempotent_and_points_at_head() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();

        let id = "agit-1111111111111111111111111111111111111111";
        r.tag(id).unwrap();
        // One snapshot id points at one state, so tagging again must not fail.
        r.tag(id).unwrap();
        assert!(r.has_tag(id));
        assert_eq!(r.tags_at_head(), vec![id.to_string()]);

        // A non-snapshot tag is not a version.
        r.git(&["tag", "v1.0"]).unwrap();
        assert_eq!(r.tags_at_head(), vec![id.to_string()]);
    }

    /// On a detached HEAD, "how many branches are here" must be answerable.
    ///
    /// `agit clone x/y:<the newest version>` checks out a tag, and where it lands is often exactly
    /// the tip of main: nothing there needs a person to decide, and switching back changes not one
    /// commit. A detached HEAD at a fork point (no branch pointing at it) is what genuinely needs
    /// the user to say something.
    #[test]
    fn branches_at_head_tells_a_tag_checkout_from_a_real_fork_point() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();
        r.git(&["tag", "agit-first"]).unwrap();
        let first = r.head_short().unwrap();

        std::fs::write(r.root().join("a.jsonl"), "2").unwrap();
        r.add_all().unwrap();
        r.commit("two").unwrap();
        r.git(&["tag", "agit-second"]).unwrap();

        // Stopped on the newest version: main is right here, unique and unambiguous.
        r.git(&["checkout", "--quiet", "refs/tags/agit-second"])
            .unwrap();
        assert!(
            r.current_branch().is_none(),
            "HEAD is detached after checking out a tag"
        );
        assert_eq!(r.branches_at_head(), vec!["main".to_string()]);
        r.switch("main").unwrap();
        assert_eq!(r.current_branch().as_deref(), Some("main"));

        // Stopped on an earlier snapshot: no branch points here, which is the intent "start a new
        // branch from here".
        r.git(&["checkout", "--quiet", &first]).unwrap();
        assert!(r.branches_at_head().is_empty());
    }

    #[test]
    fn set_remote_adds_then_updates() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        assert!(r.remote_url().is_none());
        r.set_remote("http://h/a.git").unwrap();
        assert_eq!(r.remote_url().as_deref(), Some("http://h/a.git"));
        // The URL changes after switching hub, so an existing origin must not make this fail.
        r.set_remote("http://other/a.git").unwrap();
        assert_eq!(r.remote_url().as_deref(), Some("http://other/a.git"));
    }

    /// The two remotes are independent, and both must be settable when a read-only clone is
    /// promoted.
    #[test]
    fn origin_and_upstream_are_independent() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();

        // Read-only clone: origin is their copy and there is no upstream. "Has an upstream" is
        // the test for "is this copy mine", so the read-only case must be None.
        r.set_remote("http://h/alice/photo.git").unwrap();
        assert!(r.upstream_url().is_none());

        // Promoted into your own copy: origin moves to your copy, upstream remembers the source.
        r.set_remote("http://h/me/photo.git").unwrap();
        r.set_upstream("http://h/alice/photo.git").unwrap();
        assert_eq!(r.remote_url().as_deref(), Some("http://h/me/photo.git"));
        assert_eq!(
            r.upstream_url().as_deref(),
            Some("http://h/alice/photo.git")
        );
        // Changing upstream leaves origin alone.
        r.set_upstream("http://h/bob/photo.git").unwrap();
        assert_eq!(r.remote_url().as_deref(), Some("http://h/me/photo.git"));
    }

    /// `@{upstream}` (the tracking branch) and a remote named `upstream` are two different things.
    ///
    /// The shared name is git's historical baggage. Conflating them has a concrete consequence:
    /// `agit clone` reads `ahead_behind` for its fast-forward test, and if adding an `upstream`
    /// remote turned that into "compare against upstream", a copy would call itself behind the
    /// author on every fetch and then fast-forward a stretch of history it must not.
    #[test]
    fn adding_an_upstream_remote_does_not_change_the_tracking_branch() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();

        assert!(r.ahead_behind().is_none(), "no tracking branch yields None");
        r.set_upstream("http://h/alice/photo.git").unwrap();
        assert!(
            r.ahead_behind().is_none(),
            "adding a remote named upstream must not conjure a tracking branch out of nowhere"
        );
    }

    /// A repository path that cannot be computed must not open, and above all must not open the
    /// repository in the **current directory**.
    ///
    /// `config::repo_dir(...).unwrap_or_default()` turns an Err into an empty `PathBuf` at its call
    /// sites; `"".join(".git")` is a relative path, so `.exists()` asks the CWD. Without the
    /// `is_absolute` check, `agit fetch ../..` operates on the user's own repository.
    #[test]
    fn a_relative_or_empty_root_never_opens_the_repository_the_user_is_standing_in() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        // An absolute path opens as usual.
        assert!(Repo::open(d.path()).is_some());
        // An empty path and a relative path never open — even with a real `.git` under the CWD.
        assert!(Repo::open(std::path::PathBuf::new()).is_none());
        assert!(Repo::open(std::path::PathBuf::from(".")).is_none());
        assert!(Repo::open(std::path::PathBuf::from("../..")).is_none());
    }

    /// Production code builds a git command in **exactly one** place, and that place turns replace
    /// resolution off.
    ///
    /// # What this pins
    ///
    /// Not "it is written correctly right now" — one test run says that. It pins the **next
    /// person**: a newly added path that starts git and forgets to turn replace resolution off has
    /// no symptom at all, it only lets the scanner quietly read a replacement object on that path.
    /// Without this test, that hole waits until a secret really is pushed to the hub.
    ///
    /// # Why it scans the source
    ///
    /// The contract to express is "every git subprocess goes through [`git_command`]", and the type
    /// system cannot express it: `std::process::Command` is a foreign type and anyone can `new` it
    /// directly. Scanning the source is the closest test available, at the cost of matching
    /// **text** — so only one spelling counts below, and changing the spelling turns this test red
    /// rather than green, which is the right direction.
    ///
    /// Only the half ahead of `#[cfg(test)]` is scanned: tests start git directly to build a
    /// replacement object or a graft (that is the scenario under test) and are not bound by this;
    /// it also keeps the literal below from counting itself.
    #[test]
    fn every_git_subprocess_disables_replace_resolution() {
        let src = include_str!("mod.rs");
        let (prod, _) = src
            .split_once("\n#[cfg(test)]")
            .expect("this file must contain a test module");

        let n = prod.matches("Command::new(\"git\")").count();
        assert_eq!(
            n, 1,
            "exactly one place in production code builds a git command (`git_command`); got {n}\n\
             a new git construction site must go through `git_command`, which carries\n\
             `--no-replace-objects`: without it the `refs/replace/*` left by `git replace` /\n\
             `filter-repo` lets the scanner read a replacement object, or makes history cut away\n\
             by a graft impossible to enumerate, while push sends the real objects."
        );

        // That one place has to actually turn replace resolution off — "exactly one" alone does
        // not guarantee it.
        let after = &prod[prod
            .find("Command::new(\"git\")")
            .expect("the count above found it")..];
        assert!(
            after
                .lines()
                .take(3)
                .any(|l| l.contains("--no-replace-objects")),
            "`git_command` must add `--no-replace-objects` right after it (global option slot, before the subcommand)"
        );
    }

    /// Every path that reads an object must see the **real object**, not the replacement
    /// `refs/replace/*` points at.
    ///
    /// The test above scans the shape of the source; this one scans behaviour: every read
    /// interface runs once, and any one of them missing `--no-replace-objects` turns this red. The
    /// two are complete only together — the source one blocks "a construction site was added",
    /// this one blocks "the construction site is still there but the option was broken".
    #[test]
    fn every_read_path_sees_the_real_object_not_the_replacement() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("the real message").unwrap();
        let real = r.git(&["rev-parse", "HEAD"]).unwrap();
        let tree = r.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        // The replacement: the same tree, a different message, on no branch.
        let decoy = r
            .git(&["commit-tree", &tree, "-m", "the decoy message"])
            .unwrap();
        r.git(&["replace", &real, &decoy]).unwrap();

        // precondition: the replace ref really was created. Otherwise everything below runs
        // empty.
        assert!(
            !r.git(&["for-each-ref", "refs/replace"]).unwrap().is_empty(),
            "precondition: with no replace ref this test pins nothing"
        );

        let real_msg = "the real message";
        assert!(
            r.git(&["cat-file", "commit", &real])
                .unwrap()
                .contains(real_msg),
            "git (the Result form) must not see the replacement"
        );
        assert!(
            r.git_opt(&["cat-file", "commit", &real])
                .unwrap()
                .contains(real_msg),
            "git_opt must not see the replacement"
        );
        assert!(
            String::from_utf8_lossy(&r.git_bytes(&["cat-file", "commit", &real]).unwrap())
                .contains(real_msg),
            "git_bytes must not see the replacement"
        );

        let mut streamed = String::new();
        r.git_stream_split(&["cat-file", "commit", &real], b'\n', |rec| {
            streamed.push_str(&String::from_utf8_lossy(rec));
            streamed.push('\n');
            Ok(())
        })
        .unwrap();
        assert!(
            streamed.contains(real_msg),
            "git_stream_split must not see the replacement"
        );

        let mut batched = String::new();
        r.git_cat_file_batch(vec![real.clone()], usize::MAX, |_, _, body| {
            if let ObjectBody::Read(b) = body {
                batched.push_str(&String::from_utf8_lossy(b));
            }
            Ok(())
        })
        .unwrap();
        assert!(
            batched.contains(real_msg),
            "git_cat_file_batch must not see the replacement"
        );
    }

    /// An object over the cap **never enters memory**, and that fact is expressible.
    ///
    /// Accumulating every record into a `Vec` without bound lets one 300 MiB blob in history (even
    /// one long since deleted, absent from the working tree and the tip tree, merely still
    /// reachable) push a scan's maxRSS from 52 MB to 682 MB. Only a cap applied to the **length
    /// reported on the header line** stops it: decide after the body is accumulated and the
    /// oversized allocation has already happened.
    ///
    /// Equally pinned: it is **not** a silent skip. A skip moves the object out of the scan surface
    /// without the caller knowing, so something never scanned gets reported as clean.
    #[test]
    fn an_oversized_object_is_not_read_into_memory() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(d.path()).unwrap();

        // One large blob and one small one, adjacent in a single batch — the one over the line
        // must not throw off the one behind it.
        let big = 256 * 1024;
        std::fs::write(d.path().join("big.txt"), "x".repeat(big)).unwrap();
        std::fs::write(d.path().join("small.txt"), "hello").unwrap();
        let big_oid = r
            .git_opt(&["hash-object", "-w", "big.txt"])
            .unwrap()
            .trim()
            .to_string();
        let small_oid = r
            .git_opt(&["hash-object", "-w", "small.txt"])
            .unwrap()
            .trim()
            .to_string();

        let limit = 1024;
        let mut seen: Vec<(String, usize)> = vec![];
        let mut read_bytes = 0usize;
        r.git_cat_file_batch(
            vec![big_oid.clone(), small_oid.clone()],
            limit,
            |oid, _, body| {
                match body {
                    ObjectBody::Read(b) => {
                        read_bytes += b.len();
                        seen.push((oid.to_string(), b.len()));
                    }
                    ObjectBody::TooLarge(n) => seen.push((oid.to_string(), usize::MAX - n)),
                }
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            seen,
            vec![(big_oid, usize::MAX - big), (small_oid, "hello".len()),],
            "an object over the line reports as TooLarge with its real length, and the one behind it still reads normally"
        );
        assert_eq!(
            read_bytes,
            "hello".len(),
            "not one of the {big} body bytes of the over-the-line object may be read in"
        );
    }

    /// An object over the line **is never asked to output a body at all** — not "asked for, then
    /// dropped before the buf".
    ///
    /// The test above only sees how many bytes the callback receives, and that number is identical
    /// under either implementation: throw the over-the-line OID at `cat-file --batch` too and drop
    /// it block by block, and git still decompresses, still writes every byte into the pipe, and
    /// this process still reads it block by block, saving only one `Vec` allocation. So "not read
    /// at all" is not observable on the callback side.
    ///
    /// This uses a test only a genuine "never asked" survives instead: **truncate** the object's
    /// loose file. After truncation `--batch-check` still reports the length from the header (it
    /// reads the header only and does not decompress the body), while `--batch`, once asked to
    /// output it, says `unable to stream ... to stdout` and the whole git process exits 128 —
    /// taking the eligible small object in the same batch with it.
    ///
    /// The test is therefore two-valued, looking at neither elapsed time nor maxRSS (both are flaky
    /// as assertions): this batch reads successfully ⟺ not one byte of the over-the-line object's
    /// body ever entered the pipe.
    #[test]
    fn an_oversized_object_body_is_never_streamed_out_of_git() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(d.path()).unwrap();

        // Incompressible content: the loose file is therefore the same order as the body, and
        // cutting the second half necessarily cuts inside the body rather than taking the header
        // with it (which would kill `--batch-check` too and pin something else).
        let big: Vec<u8> = (0..256u32 * 1024)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(d.path().join("big.bin"), &big).unwrap();
        std::fs::write(d.path().join("small.txt"), "hello").unwrap();
        let big_oid = r
            .git_opt(&["hash-object", "-w", "big.bin"])
            .unwrap()
            .trim()
            .to_string();
        let small_oid = r
            .git_opt(&["hash-object", "-w", "small.txt"])
            .unwrap()
            .trim()
            .to_string();

        let loose = d
            .path()
            .join(".git/objects")
            .join(&big_oid[..2])
            .join(&big_oid[2..]);
        let raw = std::fs::read(&loose).unwrap();
        // A loose object is read-only (0444), so write permission is opened before truncating.
        let mut perm = std::fs::metadata(&loose).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&loose, perm).unwrap();
        std::fs::write(&loose, &raw[..raw.len() / 2]).unwrap();

        // precondition one: the header still reads after truncation. Without it, the assertion
        // below becomes a test of "git reports missing" and has nothing to do with the cap.
        let mut head = vec![];
        r.git_cat_file_batch_check(vec![big_oid.clone()], |_, kind, n| {
            head.push((kind.to_string(), n));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            head,
            vec![("blob".to_string(), big.len() as u64)],
            "precondition: --batch-check must still report the length after truncation, or this test pins nothing"
        );
        // precondition two: the body really is unreadable — that is this test's probe.
        assert!(
            r.git_cat_file_batch(vec![big_oid.clone()], usize::MAX, |_, _, _| Ok(()))
                .is_err(),
            "precondition: --batch must not still emit the body after truncation, or this test pins nothing"
        );

        let mut seen: Vec<String> = vec![];
        r.git_cat_file_batch(
            vec![big_oid.clone(), small_oid.clone()],
            1024,
            |oid, kind, body| {
                seen.push(match body {
                    ObjectBody::Read(b) => format!("{} {kind} read {}", &oid[..8], b.len()),
                    ObjectBody::TooLarge(n) => format!("{} {kind} skipped {n}", &oid[..8]),
                });
                Ok(())
            },
        )
        .expect(
            "this batch must read to the end; failing means the over-the-line body was still requested and git died writing it",
        );

        assert_eq!(
            seen,
            vec![
                format!("{} blob skipped {}", &big_oid[..8], big.len()),
                format!("{} blob read 5", &small_oid[..8]),
            ],
            "an object over the line reports as TooLarge in the caller's order, with its real length and kind, and the one behind it still reads normally"
        );
    }

    #[test]
    fn local_branches_are_visible_before_first_push() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        std::fs::write(r.root().join("a.jsonl"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();
        r.git(&["branch", "work"]).unwrap();

        assert_eq!(r.local_branches(), ["main", "work"]);
        assert!(
            r.remote_branches().is_empty(),
            "remote-tracking branches do not exist before the first push"
        );
        // "which branches are there" must be answerable before the first push too: asking only
        // origin makes `agit log <owner/repo>` say "no branches" about local branches that exist.
        assert_eq!(r.branches(), ["main", "work"]);

        // After a push, local and remote each have a `main`, and merging them must not produce
        // two rows — the list shows "how many session lines" from that number.
        let bare = d.path().join("bare.git");
        std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&bare)
            .status()
            .unwrap();
        r.set_remote(&bare.to_string_lossy()).unwrap();
        r.git(&["push", "--quiet", "origin", "main"]).unwrap();
        assert_eq!(r.remote_branches(), ["main"]);
        assert_eq!(
            r.branches(),
            ["main", "work"],
            "a same-named branch counts once"
        );

        // `refs/remotes/origin/HEAD` is not a branch. git abbreviates it to **`origin`** (not
        // `origin/HEAD`), so filtering "HEAD" by the abbreviated name filters nothing — a phantom
        // branch named `origin` slips into every answer to "which branches are there".
        r.git(&["remote", "set-head", "origin", "main"]).unwrap();
        assert!(r.has_ref("refs/remotes/origin/HEAD"));
        assert_eq!(
            r.branches(),
            ["main", "work"],
            "origin/HEAD is not a branch"
        );
        assert_eq!(r.remote_branches(), ["main"]);
    }

    /// A branch name always comes from the **full** refname.
    #[test]
    fn a_branch_name_comes_from_the_full_refname() {
        assert_eq!(branch_name_of("refs/heads/work").as_deref(), Some("work"));
        assert_eq!(
            branch_name_of("refs/remotes/origin/work").as_deref(),
            Some("work")
        );
        // A local branch genuinely named `origin/x` must not collide with the remote one.
        assert_eq!(
            branch_name_of("refs/heads/origin/x").as_deref(),
            Some("origin/x")
        );
        assert_eq!(branch_name_of("refs/remotes/origin/HEAD"), None);
        // No other ref namespace is a branch.
        assert_eq!(branch_name_of("refs/tags/v1"), None);
        assert_eq!(branch_name_of("refs/remotes/upstream/main"), None);
        assert_eq!(branch_name_of("refs/agit/layout-v0/work"), None);
    }

    /// Two read paths, two leaves, each answering only one question — both sides pinned on one
    /// tree.
    ///
    /// v0 does not count a root `LOG` / `VIEW` as a storage path, so the tree of a v0 session line
    /// can hold both the author's `LOG` user file and the `session/log.jsonl` transcript. An
    /// implementation that lets "a same-named entry in the tree" win gives the logical read the
    /// user file; one that always goes raw leaves a v0 session unable to read LOG/VIEW at all.
    /// Both directions are pinned, and breaking either side turns this red.
    #[test]
    fn v0_logical_read_takes_the_transcript_while_raw_read_takes_the_user_file() {
        use crate::domain::meta;
        let d = tempfile::tempdir().unwrap();
        let (r, head, transcript) = v0_repo_with_shadowing_user_files(&d.path().join("shadowed"));

        // The logical read: `point_view` / `export` / revert / cherry-pick all take this one.
        assert_eq!(
            r.show_result(&head, meta::LOG_FILE).unwrap().as_deref(),
            Some(transcript.as_str()),
            "the logical LOG is this line's transcript, not the same-named user file in the tree"
        );
        assert_eq!(
            r.show_result(&head, meta::VIEW_FILE).unwrap().as_deref(),
            Some(transcript.as_str()),
            "the logical VIEW likewise; in v0 it lands at session/VIEW"
        );
        assert_eq!(
            r.show(&head, meta::LOG_FILE).as_deref(),
            Some(transcript.as_str()),
            "the Option form and the Result form must give the same answer"
        );

        // The raw read: `agit show <ref>:LOG` takes this one.
        assert_eq!(
            r.show_raw_result(&head, meta::LOG_FILE).unwrap().as_deref(),
            Some(V0_SHADOWING_USER_LOG),
            "the raw read wants the raw text of the blob in the tree"
        );
        assert_eq!(
            r.show_raw_result(&head, meta::VIEW_FILE)
                .unwrap()
                .as_deref(),
            Some(V0_SHADOWING_USER_VIEW),
        );
    }

    /// A v0 file line has no transcript to speak of: the logical read honestly answers "none",
    /// and the raw read still gives the user file.
    ///
    /// This pins the other half of the test above — an implementation whose logical read also falls
    /// back to the same-named user file passes that test's raw assertions, yet here would pass
    /// ordinary text off as a transcript to export / revert.
    #[test]
    fn v0_file_line_has_no_logical_log_even_with_a_user_file_named_log() {
        use crate::domain::meta;
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("file-line")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut file_line = meta::Meta::new_file_line();
        file_line.layout = meta::LayoutVersion::V0;
        meta::write(r.root(), &file_line).unwrap();
        std::fs::write(r.root().join(meta::LOG_FILE), "updated user log\n").unwrap();
        std::fs::write(r.root().join(meta::VIEW_FILE), "user view\n").unwrap();
        r.add_all().unwrap();
        r.commit("legacy file line").unwrap();

        assert_eq!(r.show_result("HEAD", meta::LOG_FILE).unwrap(), None);
        assert_eq!(r.show("HEAD", meta::VIEW_FILE), None);
        assert_eq!(
            r.show_raw_result("HEAD", meta::LOG_FILE)
                .unwrap()
                .as_deref(),
            Some("updated user log\n"),
            "the file itself is still in the tree — `agit show HEAD:LOG` reads it"
        );
        assert_eq!(
            r.show_raw("HEAD", meta::VIEW_FILE).as_deref(),
            Some("user view\n")
        );
    }

    /// Repo names and branch names answer to one rule each: the character set governs only repo
    /// names (they double as a directory name and a URL segment), the `agit-` prefix governs only
    /// branch names (they have to stay apart from the snapshot id in `owner/repo@<ref>`). One
    /// merged rule over-blocks or under-blocks on one of the two sides.
    #[test]
    fn repo_names_and_branch_names_answer_to_different_rules() {
        assert!(valid_name("photo").is_ok());
        assert!(valid_name("photo-exif_v2").is_ok());
        assert!(valid_name("").is_err());
        assert!(valid_name("has space").is_err());
        assert!(valid_name("has/slash").is_err());
        // A repo name takes up no ref namespace: the `agit-` prefix has no ambiguity to resolve
        // here, and the hub allows it.
        assert!(valid_name("agit-dev").is_ok());
        assert!(valid_name("agit-5aa76353").is_ok());

        // Branch names are what the prefix actually guards: `agit clone x/y@Z` tells a version
        // from a branch by it.
        assert!(valid_branch_name("agit-dev").is_err());
        assert!(valid_branch_name("agit-5aa76353").is_err());
        assert!(valid_branch_name("").is_err());
        assert!(valid_branch_name("main").is_ok());
        assert!(valid_branch_name("hachi/hub-auth").is_ok());
        assert!(valid_branch_name("dev-agit-web").is_ok());
    }
}
