//! Reference syntax: `owner/repo@ref`, `@`, `ref~n`, `ref#n`, `ref#n.k`, `ref:path`.
//!
//! # Design (matches the PRD's "reference syntax" section exactly)
//!
//! ```text
//! owner/repo                  a repo on the hub; the bare repo name works when locally unique
//! owner/repo@<ref>            a ref inside a remote / someone else's repo
//! <branch> <tag> <sha prefix> a ref inside the current repo (sha prefix ≥ 4, ambiguity errors)
//! @                           the current session's branch (self-reference, from AGIT_SESSION)
//! <ref>~n                     n commits back
//! <ref>#n                     turn n of that branch (1-based; #-1 is the last turn)
//! <ref>#n.k                   event k of turn n
//! <ref>#a..#b                 a turn range
//! @#n / @#n.k                 turn n of the current branch / its event k (`@#` written together)
//! <ref>:<path>                a file in that commit's tree
//! ```
//!
//! A branch name, a tag name and a sha prefix matching at once is an error that lists every
//! hit — nothing is ranked and picked for the user. `#` only ever appears written together with
//! the ref and the branch-name character set excludes `#`, so `ref#8` is never ambiguous.
//!
//! This module only **parses** (string → struct) and **resolves** (struct + repo → commit /
//! turn / event coordinates). It does not decide "who an omitted target applies to" — that is
//! `context` in the commands layer (explicit argument → AGIT_SESSION → workspace pin → cwd
//! match).

use crate::Result;

/// The repo qualifier in one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSel {
    /// No repo written: the context resolution chain decides.
    Context,
    /// `owner/repo`.
    Slug(String, String),
    /// A bare repo name (usable when it is locally unique; ambiguity errors).
    Local(String),
}

/// The base of a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// `@`: the current session's branch. Resolved only from AGIT_SESSION / the harness
    /// environment variables.
    At,
    /// A branch name / tag name / sha prefix (≥ 4). All three matching at once is an error.
    Name(String),
    /// A repo with no ref: `owner/repo`.
    Default,
}

/// The trailing selector of a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tail {
    None,
    /// `~n`: n commits back along the first-parent chain.
    Tilde(u32),
    /// `#n`: turn n (u32::MAX stands for `#-1`, the last turn).
    Turn(u32),
    /// `#n.k`: event k of turn n.
    Event {
        turn: u32,
        index: u32,
    },
    /// `#a..#b`: a turn range (both ends included; either end may be u32::MAX for -1).
    Range {
        a: u32,
        b: u32,
    },
    /// `:<path>`: a file in that commit's tree.
    Path(String),
}

/// A parsed reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSpec {
    pub repo: RepoSel,
    pub base: Base,
    pub tail: Tail,
}

/// The internal representation of `#-1` (the last turn).
pub const LAST_TURN: u32 = u32::MAX;

/// Parse a reference string. [`parse`] touches neither disk nor environment; `@` is still
/// [`Base::At`] here.
pub fn parse(input: &str) -> Result<RefSpec> {
    let s = input.trim();
    if s.is_empty() {
        anyhow::bail!("empty reference");
    }

    // 1. Trailing selectors peel right to left: `:path`, `~n`, `#...`. The `#5` after `@`
    //    belongs to the whole token (`@#5`), so the repo prefix comes off before the tail.
    // 2. Repo prefix: `owner/repo@...`, `owner/repo` (exactly one `/`, both sides valid).
    let (repo_part, rest) = split_repo(s)?;
    let (base_str, tail) = split_tail(rest)?;
    let base = match base_str {
        None => Base::Default,
        Some("@") => Base::At,
        Some(name) => {
            validate_name(name)?;
            if repo_part.is_none() && name.contains('@') && !name.starts_with('@') {
                // `repo@ref` (a locally unique repo name with the owner omitted)
                let (r, b) = name.split_once('@').unwrap_or((name, ""));
                return Ok(RefSpec {
                    repo: RepoSel::Local(r.to_string()),
                    base: if b.is_empty() {
                        Base::Default
                    } else if b == "@" {
                        Base::At
                    } else {
                        validate_name(b)?;
                        Base::Name(b.to_string())
                    },
                    tail,
                });
            }
            Base::Name(name.to_string())
        }
    };
    Ok(RefSpec {
        repo: repo_part.unwrap_or(RepoSel::Context),
        base,
        tail,
    })
}

/// Peel off the `owner/repo` prefix (and the `@` separator inside `owner/repo@xx`).
fn split_repo(s: &str) -> Result<(Option<RepoSel>, &str)> {
    // `owner/repo@ref` or `owner/repo`: exactly one `/`, with a valid owner segment before it.
    if let Some((head, after_slash)) = s.split_once('/') {
        // A selector character in head (the `:` of `main:memory/team.md`) means this `/`
        // belongs to a trailing path rather than a repo prefix — hand it back unchanged.
        if head.is_empty() || head.contains(['@', '~', '#', ':']) {
            return Ok((None, s));
        }
        if after_slash.is_empty() {
            anyhow::bail!(
                "`{s}` is not a valid reference: owner/repo needs exactly one `/` with both sides non-empty"
            );
        }
        // after_slash may hold one more `/` (branch names allow it, e.g. issue-bot/fix-123),
        // but only the first segment is the repo name; everything after `@` is the ref.
        let (name, rest) = match after_slash.split_once('@') {
            Some((n, r)) => (n, r),
            None => {
                // The whole `owner/repo` is the repo and the ref defaults. A `/` from a
                // branch name may not follow the repo segment while a ref selector is also
                // present — in `owner/repo/x#3`, `repo/x` is not a valid repo name, so reject
                // it rather than swallowing a branch name into the repo segment.
                if after_slash.contains(['~', '#', ':']) || after_slash.contains('/') {
                    anyhow::bail!(
                        "`{s}` is not valid: the ref selector after `owner/repo` must be separated by `@`, e.g. `{head}/{}@<ref>`",
                        after_slash.split(['~', '#', ':', '/']).next().unwrap_or("")
                    );
                }
                (after_slash, "")
            }
        };
        if name.is_empty() {
            anyhow::bail!("`{s}` has an empty repo name");
        }
        let repo = RepoSel::Slug(head.to_string(), name.to_string());
        return Ok((Some(repo), if rest.is_empty() { "" } else { rest }));
    }
    Ok((None, s))
}

/// Peel the tail off the rest: `:path` first (after the last `:`), then `~n`, then `#...`.
fn split_tail(s: &str) -> Result<(Option<&str>, Tail)> {
    if s.is_empty() {
        return Ok((None, Tail::None));
    }
    let mut tail = Tail::None;
    let mut body = s;

    // `:path` is the outermost layer (a path may itself contain `#` or `~`) — but `@#5`
    // has no `:`.
    if let Some(idx) = body.rfind(':') {
        let path = &body[idx + 1..];
        if path.is_empty() {
            anyhow::bail!("`{s}`: the path after `:` is empty");
        }
        // Guards against misreading something like `agit-abc:53` once `owner/repo` has been
        // peeled off — what stands before `:` must be a non-empty reference.
        if idx == 0 {
            anyhow::bail!("`{s}`: the reference before `:` is empty");
        }
        tail = Tail::Path(path.to_string());
        body = &body[..idx];
    }

    // `~n`
    if let Some(idx) = body.rfind('~') {
        if tail == Tail::None {
            let n: u32 = body[idx + 1..].parse().map_err(|_| {
                anyhow::anyhow!("`{s}`: `~` must be followed by a non-negative integer")
            })?;
            tail = Tail::Tilde(n);
            body = &body[..idx];
        } else {
            anyhow::bail!(
                "`{s}`: `~n:path` combos are not supported yet (resolve `ref~n` first, then take the path)"
            );
        }
    }

    // `#...`
    if let Some(idx) = body.find('#') {
        if tail != Tail::None {
            anyhow::bail!("`{s}`: conflicting trailing selectors");
        }
        tail = parse_turn_selector(&body[idx + 1..]).map_err(|e| e.context(format!("`{s}`")))?;
        body = &body[..idx];
    }

    if body.is_empty() {
        // In `@#5` the base is `@`, not empty. Reaching here means a word-leading bare `#` as
        // in `#5`, which the design does not support (bash reads a word-starting `#` as a
        // comment and it silently disappears) — reject it explicitly.
        anyhow::bail!(
            "a leading bare `#` is not supported: write `<ref>#n` or `@#n` (bash swallows a word-starting `#` as a comment)"
        );
    }
    Ok((Some(body), tail))
}

/// `#n` / `#-1` / `#n.k` / `#a..#b`.
fn parse_turn_selector(s: &str) -> Result<Tail> {
    if let Some((a, b)) = s.split_once("..") {
        return Ok(Tail::Range {
            a: turn_no(a)?,
            b: turn_no(b)?,
        });
    }
    if let Some((n, k)) = s.split_once('.') {
        return Ok(Tail::Event {
            turn: turn_no(n)?,
            index: k
                .parse()
                .map_err(|_| anyhow::anyhow!("in `#n.k`, k must be a positive integer"))?,
        });
    }
    Ok(Tail::Turn(turn_no(s)?))
}

fn turn_no(s: &str) -> Result<u32> {
    // Each end of the range form `#a..#b` carries its own `#`; peel it off.
    let s = s.strip_prefix('#').unwrap_or(s);
    if s == "-1" {
        return Ok(LAST_TURN);
    }
    let n: u32 = s.parse().map_err(|_| {
        anyhow::anyhow!("turn numbers must be positive integers or `-1` (last turn); got `{s}`")
    })?;
    if n == 0 {
        anyhow::bail!("turn numbers start at 1 (`#-1` means the last turn)");
    }
    Ok(n)
}

/// Character-set validation for a branch name / tag name / sha prefix.
///
/// A branch name allows `/` but **excludes** `#`, `~`, `:`, `@` and ` ` — that is the
/// precondition for unambiguous parsing (the same reasoning behind git banning `~^:` in a
/// refname).
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("empty ref name");
    }
    if let Some(c) = name
        .chars()
        .find(|c| matches!(c, '#' | '~' | ':' | '@' | ' ' | '\t'))
    {
        anyhow::bail!("ref name `{name}` contains illegal character `{c}`");
    }
    Ok(())
}

// ─────────────────────── Resolution ───────────────────────

/// The result of resolution: one concrete commit (plus optional in-turn event / in-tree file
/// coordinates).
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The branch name resolved to (when the base is a branch); None when the base is a tag
    /// or a sha.
    pub branch: Option<String>,
    /// The commit SHA (full 40 hex).
    pub sha: String,
    /// The turn ordinal when the tail names turn n (`#-1` already replaced with the real
    /// ordinal).
    pub turn: Option<u32>,
    /// The event ordinal of `#n.k`.
    pub event_index: Option<u32>,
    /// The turn range (both ends are real ordinals).
    pub range: Option<(u32, u32)>,
    /// The in-tree path of `:path`.
    pub path: Option<String>,
}

/// Resolve a reference to a concrete commit inside one repo.
///
/// `repo` is the local repo already resolved. Ambiguity (a branch, tag and sha matching at
/// once; a sha prefix matching several objects) always errors and lists every hit — nothing is
/// ranked and picked for the user (PRD: better an error than a guess).
pub fn resolve(repo: &crate::domain::repo::Repo, spec: &RefSpec) -> Result<Resolved> {
    let base_ref = match &spec.base {
        Base::At => anyhow::bail!(
            "`@` must be substituted with the session branch before resolving \
             (commands::context::substitute_at); the resolver never reads the environment"
        ),
        Base::Default => repo
            .current_branch()
            .ok_or_else(|| anyhow::anyhow!("HEAD is detached and no ref was given"))?,
        Base::Name(name) => name.clone(),
    };

    // Check for a branch / tag / sha-prefix collision.
    let sha = resolve_base(repo, &base_ref)?;
    let branch = repo
        .has_ref(&format!("refs/heads/{base_ref}"))
        .then_some(base_ref);

    let mut resolved = Resolved {
        branch,
        sha,
        turn: None,
        event_index: None,
        range: None,
        path: None,
    };

    match &spec.tail {
        Tail::None => {}
        Tail::Path(p) => resolved.path = Some(p.clone()),
        Tail::Tilde(n) => {
            resolved.sha = tilde(repo, &resolved.sha, *n)?;
            resolved.branch = None;
        }
        Tail::Turn(n) => {
            let t = turn_commit(repo, &resolved.sha, *n)?;
            resolved.turn = Some(t.0);
            resolved.sha = t.1;
            resolved.branch = None;
        }
        Tail::Event { turn, index } => {
            let t = turn_commit(repo, &resolved.sha, *turn)?;
            resolved.turn = Some(t.0);
            resolved.event_index = Some(*index);
            resolved.sha = t.1;
            resolved.branch = None;
        }
        Tail::Range { a, b } => {
            let chain = Chain::read(repo, &resolved.sha)?;
            let ta = turn_in(&chain, *a)?;
            let tb = turn_in(&chain, *b)?;
            resolved.range = Some((ta.0, tb.0));
            resolved.sha = tb.1;
            resolved.branch = None;
        }
    }
    Ok(resolved)
}

/// "not any branch / tag / sha prefix in this repo" — kept apart from real errors like
/// ambiguity and corruption: a caller holding this one can take another route (looking the
/// session id up in the store, say), while any other error must stop and have the user say
/// what they mean.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot resolve `{}`: not a branch, tag, or commit prefix in this repo",
            self.0
        )
    }
}

impl std::error::Error for NotFound {}

/// Whether this error is plainly "not found" (see [`NotFound`]).
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotFound>().is_some()
}

/// Whether a ref (`refs/heads/x`, `refs/tags/x`) exists — this looks at the ref itself and
/// never touches the object it points at.
///
/// Three answers, never conflated: `Ok(true)` it is there; `Ok(false)` the name really does not
/// exist; `Err` the probe itself could not answer — the ref store is unreadable, this is not a
/// repo, or the name exists but is a symbolic ref pointing at a branch that does not exist (git
/// skips it as "absent", but to the user it is a broken branch, not a name to go looking for
/// elsewhere).
fn ref_exists(repo: &crate::domain::repo::Repo, full_ref: &str) -> Result<bool> {
    let (code, _, stderr) = repo.git_status(&["show-ref", "--verify", "--quiet", full_ref])?;
    match code {
        Some(0) => Ok(true),
        Some(1) => {
            if let Some(target) = repo.git_opt(&["symbolic-ref", "-q", full_ref]) {
                anyhow::bail!(
                    "`{full_ref}` is a symbolic ref to `{}`, which does not exist",
                    target.trim()
                );
            }
            Ok(false)
        }
        _ => anyhow::bail!("cannot probe `{full_ref}`: {stderr}"),
    }
}

/// Peel a rev that is **known to exist** down to a commit.
///
/// `agit tag v1 <ref> -m "..."` produces a tag object, and its sha is not the commit's sha.
/// Feeding it to `git commit-tree -p <tag object>` gives `fatal: not a valid 'commit'
/// object` — `run` / `fork` then break on every annotated tag while lightweight tags happen to
/// work, so the trap springs only when "the author wrote a real description for the milestone".
/// Peeling at the **single** entry point of resolution makes every consumer right at once.
///
/// A failure here is git's failure (the ref points at a missing or corrupt object, the tag does
/// not peel to a commit), not "no such name" — the caller has already asked about existence, so
/// this path must never flatten an error into absence.
fn peel_to_commit(repo: &crate::domain::repo::Repo, rev: &str) -> Result<String> {
    let sha = repo
        .git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .map_err(|e| anyhow::anyhow!("`{rev}` exists but does not resolve to a commit: {e}"))?;
    Ok(sha.trim().to_string())
}

/// What the object store answers for a sha prefix: `Ok(None)` no such object, `Ok(Some)` a
/// unique hit, ambiguity an error — it never picks between two candidates for the user.
fn object_by_prefix(repo: &crate::domain::repo::Repo, prefix: &str) -> Result<Option<String>> {
    let mut kind = String::new();
    repo.git_cat_file_batch_check(vec![prefix.to_string()], |_, k, _| {
        kind = k.to_string();
        Ok(())
    })?;
    match kind.as_str() {
        "missing" => Ok(None),
        "ambiguous" => anyhow::bail!("`{prefix}` matches more than one object, write more digits"),
        _ => peel_to_commit(repo, prefix).map(Some),
    }
}

/// Resolve a branch name / tag / sha prefix to a full SHA; ambiguity errors and lists every
/// hit. The two ids the web interface shows are both `agit-` + 40 hex: the session declaration
/// (the `session` field of session/meta.json) and the version id (a commit sha). Branches and
/// tags may not use this prefix, so parsing it leaves no room for ambiguity.
///
/// A session declaration maps first to **the branch that declares it** (with no local branch,
/// the single remote of the same name); when no branch declares it but it points at a branch
/// head, that branch is the answer too — both kinds of web link land on one answer.
pub fn version_alias(repo: &crate::domain::repo::Repo, name: &str) -> Option<String> {
    // The shape of an id is owned by the meta contract alone (lowercase hex; uppercase is not
    // a valid id).
    if !crate::domain::meta::is_bare_id(name) {
        return None;
    }
    let hex = crate::domain::meta::sha_from_id(name)?;
    // (branch name, head sha). With a local branch present, a remote head of the same name is
    // only its mirror and is not collected.
    let mut heads: Vec<(String, String)> = vec![];
    // A prefix rather than `/*`: fnmatch's `*` does not cross `/`, so branch names carrying a
    // slash would be missed.
    if let Some(list) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname:short) %(objectname)",
        "refs/heads",
    ]) {
        for line in list.lines() {
            if let Some((b, sha)) = line.split_once(' ') {
                heads.push((b.to_string(), sha.to_string()));
            }
        }
    }
    if let Some(list) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname:short) %(objectname)",
        "refs/remotes",
    ]) {
        for line in list.lines() {
            let Some((short, sha)) = line.split_once(' ') else {
                continue;
            };
            let Some((_, branch)) = short.split_once('/') else {
                continue;
            };
            // A remote with the same name at the same head is only the local branch's
            // mirror; the same name at a **different head** stays — the latest tip the web
            // interface hands out may exist only on the remote, and once it folds back to the
            // branch it is `run` that aligns the local branch (fast-forward).
            if branch == "HEAD" || heads.iter().any(|(b, s)| b == branch && s == sha) {
                continue;
            }
            heads.push((branch.to_string(), sha.to_string()));
        }
    }
    let mut by_session: Vec<&str> = vec![];
    let mut by_tip: Vec<&str> = vec![];
    // Read meta in one batch: one git process per leaf turns this path into N+1.
    let shas: Vec<String> = heads.iter().map(|(_, sha)| sha.clone()).collect();
    let metas = crate::domain::meta::at_refs(repo, &shas);
    for ((branch, sha), m) in heads.iter().zip(metas) {
        if sha == hex {
            by_tip.push(branch);
        }
        if m.is_some_and(|m| m.session == name) {
            by_session.push(branch);
        }
    }
    by_session.sort();
    by_session.dedup();
    by_tip.sort();
    by_tip.dedup();
    // A version id pointing exactly at a branch head is the most specific pointer, so it goes
    // first; otherwise the session declaration decides — every commit on a line carries the
    // declaration, but collecting by **branch head** leaves one candidate per line. Once a fork
    // has several branches sharing one declaration there is no picking for the user: the alias
    // is abandoned and resolution falls back to the object.
    match (&by_tip[..], &by_session[..]) {
        ([one], _) => Some((*one).to_string()),
        ([], [one]) => Some((*one).to_string()),
        _ => None,
    }
}

fn resolve_base(repo: &crate::domain::repo::Repo, name: &str) -> Result<String> {
    let mut hits: Vec<(String, String)> = Vec::new();
    let branch_ref = format!("refs/heads/{name}");
    if ref_exists(repo, &branch_ref)? {
        hits.push((format!("branch {name}"), peel_to_commit(repo, &branch_ref)?));
    }
    let tag_ref = format!("refs/tags/{name}");
    if ref_exists(repo, &tag_ref)? {
        hits.push((format!("tag {name}"), peel_to_commit(repo, &tag_ref)?));
    }
    // The remote-tracking fallback: a freshly cloned repo has no local branch, and the `exp`
    // of `run owner/repo@exp` exists only as refs/remotes/origin/exp.
    //
    // Two rules, neither of them vague:
    // - With a local branch of the same name, the remote one is a mirror of the same thing — it
    //   **does not count**, or every resolve is ambiguous for anyone who has pushed once (a
    //   branch's natural shadow).
    // - With no local branch and several remotes carrying the name, the user must say which.
    if !hits
        .iter()
        .any(|(what, _)| what == &format!("branch {name}"))
        && let Some(list) = repo.git_opt(&[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/remotes/*/{name}"),
        ])
    {
        let remotes: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
        if remotes.len() == 1 {
            let sha = peel_to_commit(repo, remotes[0])?;
            let short = remotes[0]
                .strip_prefix("refs/remotes/")
                .unwrap_or(remotes[0]);
            hits.push((format!("remote branch {short}"), sha));
        } else if remotes.len() > 1 {
            let list = remotes
                .iter()
                .map(|r| format!("  - remote branch {}", &r["refs/remotes/".len()..]))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("`{name}` exists on multiple remotes — pick one:\n{list}");
        }
    }
    // Version id: how the web interface writes a commit — strip the prefix and look the object
    // up (the shape is owned by the meta id contract alone). `refs/tags/agit-<sha>` is a name
    // the version contract reserves: `<sha>` is itself the version identity, so a tag of that
    // name must peel to the same object — when they agree the tag hit is enough (the bare
    // object no longer stacks into an ambiguity); when they disagree the tag is corrupt or
    // squatted and must error on the spot, or every command sharing this read path silently
    // opens a different snapshot.
    if crate::domain::meta::is_bare_id(name)
        && let Some(hex) = crate::domain::meta::sha_from_id(name)
        && let Some(sha) = object_by_prefix(repo, hex)?
    {
        if let Some((_, tag_sha)) = hits.iter().find(|(what, _)| what == &format!("tag {name}")) {
            if tag_sha != &sha {
                anyhow::bail!(
                    "version tag `{name}` points at {} but the id names {} — the tag is corrupt or reused; fix or delete `refs/tags/{name}`",
                    &tag_sha[..9.min(tag_sha.len())],
                    &sha[..9.min(sha.len())]
                );
            }
        } else {
            hits.push((format!("version {name}"), sha));
        }
    }
    // Sha prefix: only ≥ 4 hex digits take part. Ambiguity is an error, not a miss — the user
    // writes more digits.
    if name.len() >= 4
        && name.bytes().all(|b| b.is_ascii_hexdigit())
        && let Some(sha) = object_by_prefix(repo, name)?
    {
        hits.push((format!("sha prefix {name}"), sha));
    }
    match hits.as_slice() {
        [] => Err(NotFound(name.to_string()).into()),
        [(_, sha)] => Ok(sha.clone()),
        many => {
            let list = many
                .iter()
                .map(|(what, _)| format!("  - {what}"))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("`{name}` is ambiguous, be more specific:\n{list}")
        }
    }
}

/// n steps back along the first-parent chain.
fn tilde(repo: &crate::domain::repo::Repo, sha: &str, n: u32) -> Result<String> {
    repo.git_opt(&["rev-parse", "--verify", &format!("{sha}~{n}")])
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("`{sha}` does not have {n} commits behind it"))
}

/// The first-parent chain, from the root to `head`.
pub fn first_parent_chain(repo: &crate::domain::repo::Repo, head: &str) -> Result<Vec<String>> {
    let list = repo
        .git_opt(&["rev-list", "--first-parent", "--reverse", head])
        .ok_or_else(|| anyhow::anyhow!("can’t read the history of `{head}`"))?;
    Ok(list
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// A commit on the first-parent chain together with its `session/meta.json` (None when the
/// file is absent).
#[derive(Debug, Clone)]
pub struct ChainEntry {
    pub sha: String,
    pub meta: Option<crate::domain::meta::Meta>,
}

/// The first-parent chain of one branch together with every commit's metadata — the table
/// `<ref>#n` and the left column of `agit log` share.
///
/// A turn ordinal is the `turn` field of `session/meta.json`, **not** the nth commit on the
/// first-parent chain: a branch's history also holds the birth commit, the file commit of `-m`,
/// the identity commit of a fork and merge commits, none of which take a turn ordinal. Only a
/// `kind: turn` commit counts — the merge / view / file kinds all **inherit** the head's turn
/// ordinal, and counting them makes `B#1` point at the merge commit that inherited number 1.
///
/// `declared` separates two kinds of "the table is empty":
/// * **no** commit on the chain carries AgentGit metadata — ordinary git history pushed in from
///   outside, where `#n` falls back to "the nth commit from the root", the only meaning it can
///   have there;
/// * the chain has metadata but no turn has been settled yet (a session line fresh from
///   import / new, a file line) — `#n` has nothing to name and must say "no turns" instead of
///   passing the branch head off as the last turn.
///
/// The number of git processes it takes to read a whole chain is independent of the chain's
/// length (`rev-list` lists the shas, `cat-file` batch-reads the metadata): `#n` is the everyday
/// entry point of show / fork / tag / export / diff, and one git process per commit would scale
/// the cost of a reference command linearly with the length of the session.
#[derive(Debug, Clone)]
pub struct Chain {
    pub entries: Vec<ChainEntry>,
    pub declared: bool,
}

impl Chain {
    /// Read the first-parent chain of `head`. Corrupt metadata is an error, not absence.
    pub fn read(repo: &crate::domain::repo::Repo, head: &str) -> Result<Chain> {
        use crate::domain::repo::ObjectBody;
        let shas = first_parent_chain(repo, head)?;
        let asks: Vec<String> = shas
            .iter()
            .map(|sha| format!("{sha}:{}", crate::domain::meta::FILE))
            .collect();
        // Ask first which commit trees hold the file: `--batch-check` reports what it cannot
        // read as `missing` data, while `--batch` treats it as a failure — a missing meta is a
        // legal state here.
        let mut present: Vec<usize> = vec![];
        let mut i = 0usize;
        repo.git_cat_file_batch_check(asks.clone(), |_, kind, _| {
            if kind != "missing" {
                present.push(i);
            }
            i += 1;
            Ok(())
        })?;
        let mut metas: Vec<Option<crate::domain::meta::Meta>> = vec![None; shas.len()];
        let mut cursor = 0usize;
        repo.git_cat_file_batch(
            present.iter().map(|&i| asks[i].clone()).collect(),
            usize::MAX,
            |_, _, body| {
                let idx = present[cursor];
                cursor += 1;
                let ObjectBody::Read(bytes) = body else {
                    anyhow::bail!("{} is too large to read", asks[idx]);
                };
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("{} is not UTF-8", asks[idx]))?;
                metas[idx] = Some(crate::domain::meta::parse_strict(text, &shas[idx])?);
                Ok(())
            },
        )?;
        let declared = metas.iter().any(Option::is_some);
        let entries = shas
            .into_iter()
            .zip(metas)
            .map(|(sha, meta)| ChainEntry { sha, meta })
            .collect();
        Ok(Chain { entries, declared })
    }

    /// The turn ordinal the `i`th commit (0-based) prints: a commit that settled a turn
    /// carries its ordinal and other commits carry none; in history with no declaration at all
    /// every commit is numbered by position.
    pub fn label(&self, i: usize) -> Option<u32> {
        if !self.declared {
            return Some(i as u32 + 1);
        }
        let m = self.entries.get(i)?.meta.as_ref()?;
        (m.kind == crate::domain::meta::Kind::Turn)
            .then_some(m.turn)
            .flatten()
    }

    /// "turn ordinal → the commit that settled it", by first-parent from the root to the head.
    pub fn turns(&self) -> Vec<(u32, &str)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| self.label(i).map(|t| (t, e.sha.as_str())))
            .collect()
    }
}

/// Replace `#-1` ([`LAST_TURN`]) with the real turn ordinal.
pub fn real_turn(repo: &crate::domain::repo::Repo, head: &str, n: u32) -> Result<u32> {
    if n != LAST_TURN {
        return Ok(n);
    }
    Ok(turn_commit(repo, head, n)?.0)
}

/// The commit that settled turn n. `n == LAST_TURN` means the last turn.
///
/// Returns (the real turn ordinal, the commit SHA). This is the single resolution point the
/// whole `<ref>#n` family (`show` / `fork` / `tag` / `export` / `diff` / `merge pick` ...)
/// shares: the numbers they see and the numbers the left column of `agit log` prints must be
/// one and the same.
pub fn turn_commit(repo: &crate::domain::repo::Repo, head: &str, n: u32) -> Result<(u32, String)> {
    turn_in(&Chain::read(repo, head)?, n)
}

/// [`turn_commit`], but searching a chain that is already read — the two ends of a range do
/// not read it twice.
pub fn turn_in(chain: &Chain, n: u32) -> Result<(u32, String)> {
    let table = chain.turns();
    let Some(last) = table.last() else {
        anyhow::bail!("this branch has no settled turns yet, so there is nothing for `#n` to name");
    };
    if n == LAST_TURN {
        return Ok((last.0, last.1.to_string()));
    }
    if n == 0 {
        anyhow::bail!("turn numbers start at 1");
    }
    // One turn ordinal is settled once; when a duplicate really shows up (history rewritten
    // from outside) the **earliest** one wins — the first landing is that turn and the rest
    // retell it.
    table
        .iter()
        .find(|(t, _)| *t == n)
        .map(|(t, sha)| (*t, sha.to_string()))
        .ok_or_else(|| {
            let have: Vec<String> = table.iter().map(|(t, _)| t.to_string()).collect();
            anyhow::anyhow!(
                "this branch has no turn {n} (settled turns: {})",
                have.join(", ")
            )
        })
}

/// How many turns this branch has settled; history with no declared turn ordinal falls back to
/// the first-parent commit count.
pub fn count_turns(repo: &crate::domain::repo::Repo, head: &str) -> Result<u32> {
    Ok(Chain::read(repo, head)?.turns().len() as u32)
}

/// A stretch of realistically shaped history shared by tests: birth, claim, turns, fork, file
/// and merge are all present.
#[cfg(test)]
pub(crate) mod fixtures {
    use crate::domain::meta::{self, Meta};
    use crate::domain::repo::Repo;
    use crate::domain::{storage, transcript};

    pub(crate) fn claim() -> String {
        format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN))
    }

    pub(crate) fn user_line(turn: u32) -> String {
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"PROMPT-{turn}\"}}}}"
        )
    }

    pub(crate) fn assistant_line(turn: u32) -> String {
        format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"REPLY-{turn}\"}}]}}}}"
        )
    }

    /// Settle one turn: LOG gains this turn's two lines, the VIEW stays in step with LOG, and
    /// meta records the turn ordinal.
    pub(crate) fn settle_turn(r: &Repo, turn: u32) {
        let mut log = r.show("HEAD", meta::LOG_FILE).unwrap_or_default();
        if !log.is_empty() && !log.ends_with('\n') {
            log.push('\n');
        }
        let raw = format!("{}\n{}\n", user_line(turn), assistant_line(turn));
        log.push_str(&transcript::wrap_lines(&raw, "claude-code", &claim()));
        storage::write_snapshot(r.root(), &log, &log).unwrap();
        let mut m = Meta::new(claim(), "claude-code".into(), "/w".into());
        m.turn = Some(turn);
        meta::write(r.root(), &m).unwrap();
        r.add_all().unwrap();
        assert!(r.commit(&format!("agit: turn {turn}")).unwrap());
    }

    /// Land a commit on the current branch head that inherits the turn ordinal and changes
    /// only kind (the identity commit of a fork and the file commit of `-m` share this shape).
    pub(crate) fn inherit_commit(r: &Repo, kind: meta::Kind, message: &str) {
        let mut m = meta::read_at_ref(r, "HEAD").expect("HEAD has meta");
        m.kind = kind;
        meta::write(r.root(), &m).unwrap();
        std::fs::write(r.root().join("AGENTS.md"), format!("# {message}\n")).unwrap();
        r.add_all().unwrap();
        assert!(r.commit(message).unwrap());
    }

    /// `main` (the init file line) → `s1` (claim + turns 1..3, then turn 4) → `f1` (forked
    /// from turn 3 of s1, lands turn 4 and one file commit, then merges s1 in).
    ///
    /// The first-parent chain of `f1` is therefore [init, claim, t1, t2, t3, fork, t4, file,
    /// merge]: **nine commits, four turn ordinals**. Returns (the temp dir, the repo).
    pub(crate) fn forked_history() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("qa")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::ensure_session_dir(r.root()).unwrap();

        meta::write(r.root(), &Meta::new_file_line()).unwrap();
        std::fs::write(r.root().join("AGENTS.md"), "# shared\n").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: init").unwrap());

        r.git(&["checkout", "-q", "-b", "s1"]).unwrap();
        meta::write(
            r.root(),
            &Meta::new_session_line("claude-code".into(), "/w".into()),
        )
        .unwrap();
        storage::write_snapshot(r.root(), "", "").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: claim session line s1").unwrap());
        for turn in 1..=3 {
            settle_turn(&r, turn);
        }

        r.git(&["checkout", "-q", "-b", "f1"]).unwrap();
        inherit_commit(&r, meta::Kind::File, "agit: fork f1 from s1");
        settle_turn(&r, 4);
        inherit_commit(&r, meta::Kind::File, "add note");

        r.git(&["checkout", "-q", "s1"]).unwrap();
        settle_turn(&r, 4);
        r.git(&["checkout", "-q", "f1"]).unwrap();
        r.git(&["merge", "-s", "ours", "--no-commit", "s1"])
            .unwrap();
        inherit_commit(&r, meta::Kind::Merge, "agit: merge s1 into f1");
        assert_eq!(
            r.git(&["rev-list", "--parents", "-n", "1", "HEAD"])
                .unwrap()
                .split_whitespace()
                .count(),
            3,
            "the merge commit has two parents"
        );
        (d, r)
    }

    /// The sha of the turn commit numbered `turn` on `f1` (found by subject, independent of
    /// the code under test).
    pub(crate) fn turn_sha(r: &Repo, turn: u32) -> String {
        let list = r
            .git(&["log", "--first-parent", "--format=%H%x00%s", "f1"])
            .unwrap();
        list.lines()
            .find_map(|l| {
                let (sha, subject) = l.split_once('\0')?;
                (subject == format!("agit: turn {turn}")).then(|| sha.to_string())
            })
            .expect("turn commit exists")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_branch() {
        let r = parse("refund-fix").unwrap();
        assert_eq!(r.repo, RepoSel::Context);
        assert_eq!(r.base, Base::Name("refund-fix".into()));
        assert_eq!(r.tail, Tail::None);
    }

    #[test]
    fn parses_owner_repo_with_ref() {
        let r = parse("alice/ci-notes@v2").unwrap();
        assert_eq!(r.repo, RepoSel::Slug("alice".into(), "ci-notes".into()));
        assert_eq!(r.base, Base::Name("v2".into()));
        let r = parse("alice/ci-notes").unwrap();
        assert_eq!(r.base, Base::Default);
    }

    #[test]
    fn parses_turn_event_range_path() {
        assert_eq!(parse("refund-fix#5").unwrap().tail, Tail::Turn(5));
        assert_eq!(
            parse("@#8.2").unwrap().tail,
            Tail::Event { turn: 8, index: 2 }
        );
        assert_eq!(parse("@#8.2").unwrap().base, Base::At);
        assert_eq!(
            parse("main#3..#7").unwrap().tail,
            Tail::Range { a: 3, b: 7 }
        );
        assert_eq!(parse("b#-1").unwrap().tail, Tail::Turn(LAST_TURN));
        assert_eq!(
            parse("main:memory/team.md").unwrap().tail,
            Tail::Path("memory/team.md".into())
        );
        assert_eq!(parse("hotfix~2").unwrap().tail, Tail::Tilde(2));
    }

    #[test]
    fn rejects_bare_hash_selector_and_zero() {
        assert!(parse("#5").is_err());
        assert!(parse("b#0").is_err());
    }

    /// An annotated tag must resolve to the **commit** sha, not the tag object's.
    ///
    /// `agit tag v1 <ref> -m "..."` builds a tag object; handing its sha to
    /// `git commit-tree -p` gives `fatal: not a valid 'commit' object`, so the main line of
    /// "one `agit run author/paper-repro@v1` in a README" breaks for every tag that carries a
    /// description — lightweight tags happen to work, so the trap springs only when the author
    /// took the trouble to write one.
    #[test]
    fn annotated_and_lightweight_tags_both_resolve_to_a_commit() {
        let tmp = std::env::temp_dir().join(format!("agit-refs-tags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let repo = crate::domain::repo::Repo::init(&tmp.join("work")).unwrap();
        std::fs::write(repo.root().join("f.txt"), "x").unwrap();
        repo.git(&["add", "f.txt"]).unwrap();
        repo.git(&["commit", "-m", "one"]).unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        repo.git(&["tag", "light", "HEAD"]).unwrap();
        repo.git(&[
            "tag",
            "-a",
            "v1",
            "HEAD",
            "-m",
            "milestone: table 3 reproduced",
        ])
        .unwrap();
        // Precondition: the two kinds of tag have different object shas to begin with, or this
        // test proves nothing.
        let tag_obj = repo
            .git(&["rev-parse", "refs/tags/v1"])
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(tag_obj, head, "an annotated tag points at a tag object");

        for name in ["light", "v1"] {
            let r = resolve(&repo, &parse(name).unwrap()).unwrap();
            assert_eq!(r.sha, head, "`{name}` must resolve to a commit");
            // What comes out has to work as a commit (`run` / `fork` feed it on directly).
            assert_eq!(
                repo.git(&["cat-file", "-t", &r.sha]).unwrap().trim(),
                "commit"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A freshly cloned repo has only remote-tracking branches: `run owner/repo@exp` must
    /// still resolve.
    #[test]
    fn remote_tracking_branch_is_resolvable() {
        let tmp = std::env::temp_dir().join(format!("agit-refs-remote-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let upstream = tmp.join("upstream.git");
        std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&upstream)
            .output()
            .unwrap();
        let work = tmp.join("work");
        let repo = crate::domain::repo::Repo::init(&work).unwrap();
        std::fs::write(work.join("f.txt"), "x").unwrap();
        repo.git(&["add", "f.txt"]).unwrap();
        repo.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "one",
        ])
        .unwrap();
        repo.git(&["push", &upstream.to_string_lossy(), "HEAD:refs/heads/exp"])
            .unwrap();
        // Simulate a clone: this second one has only refs/remotes/origin/exp, no local branch
        let clone = tmp.join("clone");
        let crepo = crate::domain::repo::Repo::init(&clone).unwrap();
        crepo
            .git(&[
                "fetch",
                &upstream.to_string_lossy(),
                "+refs/heads/*:refs/remotes/origin/*",
            ])
            .unwrap();
        let spec = parse("exp").unwrap();
        let r = resolve(&crepo, &spec).unwrap();
        assert_eq!(r.sha, repo.git(&["rev-parse", "HEAD"]).unwrap().trim());
        // The shadow case: once a local branch of the same name exists, the one under
        // refs/remotes is a mirror of the same thing and must not count as another candidate —
        // otherwise every resolve is ambiguous for anyone who has pushed once.
        crepo
            .git(&["branch", "exp", "refs/remotes/origin/exp"])
            .unwrap();
        let r2 = resolve(&crepo, &spec).unwrap();
        assert_eq!(r2.sha, r.sha);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The n of `<ref>#n` is a turn ordinal, not the nth commit on the first-parent chain.
    ///
    /// Besides turn commits, a real branch's chain carries birth, claim, fork identity, file and
    /// merge commits, none of which take a turn ordinal, and the last four **inherit** the
    /// head's ordinal. Counting by position, `f1#3` points at turn 1; counting inherited values,
    /// `f1#4` collides with the file commit and the merge commit. Under either counting,
    /// `fork` / `tag` / `export` silently get the wrong commit.
    #[test]
    fn turn_refs_index_settled_turns_only() {
        let (_d, r) = fixtures::forked_history();
        let head = r.git(&["rev-parse", "f1"]).unwrap().trim().to_string();
        let chain = Chain::read(&r, &head).unwrap();
        assert_eq!(
            chain.entries.len(),
            9,
            "precondition: nine commits, four turn ordinals"
        );
        assert!(chain.declared);
        assert_eq!(count_turns(&r, &head).unwrap(), 4);
        // The left column: only turn commits carry a number, and they are the commits the
        // table lists.
        let labels: Vec<u32> = (0..9).filter_map(|i| chain.label(i)).collect();
        assert_eq!(labels, vec![1, 2, 3, 4]);
        assert_eq!(
            chain
                .turns()
                .iter()
                .map(|(_, sha)| sha.to_string())
                .collect::<Vec<_>>(),
            (1..=4)
                .map(|t| fixtures::turn_sha(&r, t))
                .collect::<Vec<_>>()
        );

        for turn in 1..=4u32 {
            let spec = parse(&format!("f1#{turn}")).unwrap();
            let got = resolve(&r, &spec).unwrap();
            assert_eq!(got.turn, Some(turn));
            assert_eq!(got.sha, fixtures::turn_sha(&r, turn), "f1#{turn}");
        }
        // `#-1` is the commit of the last turn, not the branch head (the head is a merge).
        let last = resolve(&r, &parse("f1#-1").unwrap()).unwrap();
        assert_eq!(last.turn, Some(4));
        assert_eq!(last.sha, fixtures::turn_sha(&r, 4));
        assert_ne!(last.sha, head);
        // Both ends of a range are turn ordinals too.
        let range = resolve(&r, &parse("f1#2..#3").unwrap()).unwrap();
        assert_eq!(range.range, Some((2, 3)));
        assert_eq!(range.sha, fixtures::turn_sha(&r, 3));
        // With no turn 5, say so rather than reading the fifth commit on the chain.
        let e = resolve(&r, &parse("f1#5").unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no turn 5"), "{e}");
    }

    /// History that never declared a turn ordinal (an ordinary git branch pushed in from
    /// outside) falls back to counting by position.
    #[test]
    fn turn_refs_fall_back_to_position_without_declared_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(&tmp.path().join("work")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut shas = vec![];
        for i in 1..=3 {
            std::fs::write(repo.root().join("f.txt"), i.to_string()).unwrap();
            repo.git(&["add", "f.txt"]).unwrap();
            repo.git(&["commit", "-m", &format!("c{i}")]).unwrap();
            shas.push(repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string());
        }
        let branch = repo.current_branch().unwrap();
        let second = resolve(&repo, &parse(&format!("{branch}#2")).unwrap()).unwrap();
        assert_eq!(second.sha, shas[1]);
        let last = resolve(&repo, &parse(&format!("{branch}#-1")).unwrap()).unwrap();
        assert_eq!((last.turn, last.sha), (Some(3), shas[2].clone()));
    }

    /// A branch with AgentGit metadata but no settled turn (a session line fresh from
    /// import / new, the file line from init): `#n` has nothing to name and must say "no turns"
    /// instead of falling back to counting by position — that would pass the init commit off as
    /// turn 1 and the branch head off as the last turn.
    #[test]
    fn declared_history_without_turns_names_no_turn() {
        let (_d, r) = fixtures::forked_history();
        // The claim commit of s1: a session line with no turns yet.
        let claim = r.git(&["rev-parse", "s1~4"]).unwrap().trim().to_string();
        let chain = Chain::read(&r, &claim).unwrap();
        assert!(chain.declared && chain.turns().is_empty());
        for n in [1, LAST_TURN] {
            let e = turn_in(&chain, n).unwrap_err().to_string();
            assert!(e.contains("no settled turns"), "#{n}: {e}");
        }
        assert_eq!(count_turns(&r, &claim).unwrap(), 0);
        // The file line (main) behaves the same.
        let e = resolve(&r, &parse("main#1").unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no settled turns"), "{e}");
    }

    /// A miss and an ambiguity are two outcomes: the first can take another route, the second
    /// must have the user say what they mean.
    #[test]
    fn not_found_is_distinguishable_from_ambiguity() {
        let (_d, r) = fixtures::forked_history();
        let miss = resolve(&r, &parse("nope").unwrap()).unwrap_err();
        assert!(is_not_found(&miss), "{miss:#}");
        let sha = r.git(&["rev-parse", "s1"]).unwrap();
        r.git(&["tag", "f1", sha.trim()]).unwrap();
        let amb = resolve(&r, &parse("f1").unwrap()).unwrap_err();
        assert!(!is_not_found(&amb), "{amb:#}");
        assert!(amb.to_string().contains("ambiguous"), "{amb:#}");
    }

    /// "the name does not exist" and "the name exists but git cannot peel it to a commit" are
    /// two different things: a tag pointing at a blob (or at a corrupt object) must not be
    /// reported as a miss — that quietly sends `show` off to look in the store instead.
    #[test]
    fn a_broken_ref_is_an_error_not_a_miss() {
        let (_d, r) = fixtures::forked_history();
        std::fs::write(r.root().join("blob.txt"), "not a commit\n").unwrap();
        let blob = r.git(&["hash-object", "-w", "blob.txt"]).unwrap();
        r.git(&["update-ref", "refs/tags/broken", blob.trim()])
            .unwrap();
        let e = resolve(&r, &parse("broken").unwrap()).unwrap_err();
        assert!(!is_not_found(&e), "{e:#}");
        assert!(
            e.to_string().contains("does not resolve to a commit"),
            "{e:#}"
        );
        // A symbolic ref pointing at a branch that does not exist: git itself skips it as
        // "absent", and here it has to be reported as broken.
        r.git(&["symbolic-ref", "refs/heads/dangling", "refs/heads/nothere"])
            .unwrap();
        let e = resolve(&r, &parse("dangling").unwrap()).unwrap_err();
        assert!(!is_not_found(&e), "{e:#}");
        assert!(e.to_string().contains("symbolic ref"), "{e:#}");
    }
}

#[cfg(test)]
mod version_id_tests {
    use super::fixtures;
    use super::*;

    /// Both `agit-...` ids the web interface shows fold back to their branch: a version id (a
    /// branch head sha) goes first, and a session declaration folds back through the single line
    /// holding it; once a fork has several lines sharing the declaration the alias is dropped.
    /// The parser strips the prefix of a version id and matches it as an object, so the
    /// read-path commands accept it too.
    #[test]
    fn web_ids_fold_back_to_their_branch() {
        let (_d, r) = fixtures::forked_history();
        let tip = r
            .git(&["rev-parse", "refs/heads/f1"])
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(
            version_alias(&r, &format!("agit-{tip}")).as_deref(),
            Some("f1"),
            "a version id pointing at a branch head folds back to that branch"
        );
        assert_eq!(
            version_alias(&r, &fixtures::claim()),
            None,
            "f1 and s1 share one session declaration, so no line can be picked for the user"
        );
        assert_eq!(
            resolve_base(&r, &format!("agit-{tip}")).unwrap(),
            tip,
            "a version id resolves as an object once the prefix is stripped"
        );
    }

    /// The version contract reserves `refs/tags/agit-<sha>` and `<sha>` is the version identity
    /// itself: a tag of that name pointing at the same object is a single hit (it does not stack
    /// into an ambiguity); pointing at another object is corruption and must error rather than
    /// silently open a different snapshot.
    #[test]
    fn a_reserved_version_tag_must_match_its_embedded_oid() {
        let (_d, r) = fixtures::forked_history();
        let tip = r
            .git(&["rev-parse", "refs/heads/f1"])
            .unwrap()
            .trim()
            .to_string();
        let vid = format!("agit-{tip}");
        r.git(&["tag", &vid, &tip]).unwrap();
        assert_eq!(resolve_base(&r, &vid).unwrap(), tip, "agreement is one hit");
        r.git(&["tag", "-d", &vid]).unwrap();
        r.git(&["tag", &vid, "refs/heads/s1"]).unwrap();
        let err = resolve_base(&r, &vid).unwrap_err().to_string();
        assert!(
            err.contains("corrupt or reused"),
            "a tag on the wrong object must be reported as corrupt: {err}"
        );
    }

    /// After a fetch the local branch can be behind the remote: a latest tip the web interface
    /// hands out that lives only on the remote still folds back to that branch — aligning the
    /// local branch (fast-forward) is `run`'s job after the fold.
    #[test]
    fn a_web_tip_ahead_of_a_stale_local_branch_still_folds() {
        let (_d, r) = fixtures::forked_history();
        let old = r
            .git(&["rev-parse", "refs/heads/f1"])
            .unwrap()
            .trim()
            .to_string();
        // f1 on the remote has moved one commit ahead while the local f1 sits at the old head.
        let tree = r
            .git(&["rev-parse", "HEAD^{tree}"])
            .unwrap()
            .trim()
            .to_string();
        let advanced = r
            .git(&["commit-tree", &tree, "-p", &old, "-m", "remote moved on"])
            .unwrap()
            .trim()
            .to_string();
        r.git(&["update-ref", "refs/remotes/origin/f1", &advanced])
            .unwrap();
        assert_eq!(
            version_alias(&r, &format!("agit-{advanced}")).as_deref(),
            Some("f1"),
            "a newer head on the remote still folds back to the branch; run aligns the local one"
        );
        assert_eq!(
            version_alias(&r, &format!("agit-{old}")).as_deref(),
            Some("f1"),
            "an id pointing at the local head folds back too"
        );
    }

    /// Uppercase hex is not a valid id: the shape is owned by the meta contract alone.
    #[test]
    fn an_uppercase_id_is_not_a_version_id() {
        let (_d, r) = fixtures::forked_history();
        assert_eq!(version_alias(&r, &format!("agit-{}", "A".repeat(40))), None);
    }

    /// When a declaration belongs to one line only, the session id folds back to that branch.
    #[test]
    fn a_uniquely_held_session_declaration_folds_back() {
        let (_d, r) = fixtures::forked_history();
        let head = r.current_branch();
        if head.as_deref() == Some("s1") {
            r.git(&["switch", "f1"]).unwrap();
        }
        r.git(&["branch", "-D", "s1"]).unwrap();
        assert_eq!(version_alias(&r, &fixtures::claim()).as_deref(), Some("f1"));
    }
}
