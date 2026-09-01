//! Common command-target parsing.
//!
//! Human-facing commands use one spelling for a repository and a ref:
//! `owner/repo@branch-or-ref`.  A few commands still accept their historical
//! positional/flag pairs; those are normalized here so the command itself does
//! not need to know how the target was written.

use crate::Result;
use crate::domain::refs::{self, Base, RefSpec, RepoSel, Tail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Fully qualified repository when one was written; `None` means context.
    pub repo: Option<String>,
    /// The base ref, without a trailing selector. `None` means no ref was written.
    pub base: Option<String>,
    /// `#n`, `~n`, `:path`, etc. kept separate so branch-only commands can reject it.
    pub tail: Tail,
}

pub fn parse(input: &str) -> Result<Target> {
    let spec = refs::parse(input)?;
    Ok(from_spec(spec))
}

/// Parse a ref that is already scoped to a selected repository.
///
/// A bare ref may contain `/` (`topic/foo`), which the public grammar normally
/// reserves for an `owner/repo` prefix.  Prefixing a private sentinel lets the
/// existing parser validate all selectors while preserving the slash in the
/// branch name; the sentinel is removed before the caller resolves the ref.
pub fn parse_local(input: &str) -> Result<RefSpec> {
    let qualified = format!("__agit_target__/__agit_ref@{input}");
    let parsed = refs::parse(&qualified)?;
    Ok(RefSpec {
        repo: RepoSel::Context,
        base: parsed.base,
        tail: parsed.tail,
    })
}

/// Parse a target, reading a slash string as a local branch of the context
/// repository when — and only when — that branch exists there. Repository
/// syntax keeps its meaning for every other input: an `owner/repo` that
/// matches no local branch still names the remote repository.
pub fn parse_preferring_local(cwd: &std::path::Path, input: &str) -> Result<Target> {
    Ok(from_spec(parse_spec_preferring_local(cwd, input)?))
}

/// The `RefSpec` form of [`parse_preferring_local`], for commands that hand
/// specs to the resolver directly.
pub fn parse_spec_preferring_local(cwd: &std::path::Path, input: &str) -> Result<RefSpec> {
    parse_spec_with_local(context_repo_quiet(cwd).as_ref(), input)
}

/// Same preference for callers that already selected the repository.
pub fn parse_spec_for_repo(repo: &crate::domain::repo::Repo, input: &str) -> Result<RefSpec> {
    parse_spec_with_local(Some(repo), input)
}

/// The context repository, silently: the caller may not need a context at all
/// (an explicit `owner/repo@ref` target), so a missing binding prints nothing.
/// Only the repository binding is consulted — a workspace that is bound but
/// has no pinned branch still owns its local branch names.
fn context_repo_quiet(cwd: &std::path::Path) -> Option<crate::domain::repo::Repo> {
    let slug = crate::commands::context::repo_for(cwd).ok()?;
    let slug = crate::commands::context::qualify(&slug);
    let (owner, name) = crate::commands::parse_slug(&slug).ok()?;
    let dir = crate::infra::config::repo_dir(&owner, &name).ok()?;
    crate::domain::repo::Repo::open(dir)
}

fn parse_spec_with_local(repo: Option<&crate::domain::repo::Repo>, input: &str) -> Result<RefSpec> {
    if input.contains('/')
        && let Some(repo) = repo
        && let Ok(local) = parse_local(input)
        && let Base::Name(name) = &local.base
        && repo.has_ref(&format!("refs/heads/{name}"))
    {
        return Ok(local);
    }
    refs::parse(input)
}

pub fn from_spec(spec: RefSpec) -> Target {
    let repo = match spec.repo {
        RepoSel::Slug(owner, name) => Some(format!("{owner}/{name}")),
        RepoSel::Local(name) => Some(name),
        RepoSel::Context => None,
    };
    let base = match spec.base {
        Base::At => Some("@".to_string()),
        Base::Name(name) => Some(name),
        Base::Default => None,
    };
    Target {
        repo,
        base,
        tail: spec.tail,
    }
}

/// Convert a normalized target back to the refs representation for commands
/// that need to resolve it after selecting the repository.
pub fn to_spec(target: Target) -> RefSpec {
    let repo = match target.repo {
        Some(slug) => match slug.split_once('/') {
            Some((owner, name)) => RepoSel::Slug(owner.to_string(), name.to_string()),
            None => RepoSel::Local(slug),
        },
        None => RepoSel::Context,
    };
    let base = match target.base.as_deref() {
        None => Base::Default,
        Some("@") => Base::At,
        Some(name) => Base::Name(name.to_string()),
    };
    RefSpec {
        repo,
        base,
        tail: target.tail,
    }
}

/// Return a branch-like target. Historic selectors are not branches and must
/// be rejected by commands such as push/pull/commit.
pub fn branch_only(input: &str) -> Result<Target> {
    let t = parse(input)?;
    if t.base.is_none() {
        anyhow::bail!("`{input}` does not name a branch; use `<owner>/<repo>@<branch>`");
    }
    if t.base.as_deref() == Some("@") {
        anyhow::bail!(
            "`{input}` is the current-session shorthand; write the branch explicitly when naming a repository"
        );
    }
    if t.tail != Tail::None {
        anyhow::bail!(
            "`{input}` names a historic point, not a branch; remove the trailing selector"
        );
    }
    Ok(t)
}

/// Reconstruct a ref after the repository prefix has been removed. This is
/// useful when a command resolves the repository separately but delegates ref
/// resolution to the existing refs module.
pub fn ref_text(t: &Target) -> Option<String> {
    let base = t.base.as_deref()?;
    let tail = match &t.tail {
        Tail::None => String::new(),
        Tail::Tilde(n) => format!("~{n}"),
        Tail::Turn(n) if *n == refs::LAST_TURN => "#-1".into(),
        Tail::Turn(n) => format!("#{n}"),
        Tail::Event { turn, index } => {
            let turn = if *turn == refs::LAST_TURN {
                "-1".to_string()
            } else {
                turn.to_string()
            };
            format!("#{turn}.{index}")
        }
        Tail::Range { a, b } => {
            let fmt = |n: &u32| {
                if *n == refs::LAST_TURN {
                    "#-1".to_string()
                } else {
                    format!("#{n}")
                }
            };
            format!("{}..{}", fmt(a), fmt(b))
        }
        Tail::Path(path) => format!(":{path}"),
    };
    Some(format!("{base}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_forms_are_normalized() {
        assert_eq!(
            parse("alice/payments@refund-fix").unwrap(),
            Target {
                repo: Some("alice/payments".into()),
                base: Some("refund-fix".into()),
                tail: Tail::None,
            }
        );
        assert_eq!(
            parse("alice/payments@refund-fix#3.2").unwrap().repo,
            Some("alice/payments".into())
        );
        assert_eq!(parse("@").unwrap().base, Some("@".into()));
    }

    #[test]
    fn branch_only_rejects_points() {
        assert!(branch_only("alice/payments@refund-fix#3").is_err());
        assert!(branch_only("alice/payments").is_err());
        assert!(branch_only("alice/payments@refund-fix").is_ok());
    }

    #[test]
    fn existing_local_branch_with_slashes_wins_over_repo_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(&dir.path().join("work")).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "one"]).unwrap();
        for name in ["alice/ci-notes", "deadbeef/release/upload-self/001"] {
            repo.git(&["branch", name]).unwrap();
            let local = parse_spec_with_local(Some(&repo), name).unwrap();
            assert_eq!(local.repo, RepoSel::Context);
            assert_eq!(local.base, Base::Name(name.into()));
        }
        let tailed = parse_spec_with_local(Some(&repo), "alice/ci-notes#2").unwrap();
        assert_eq!(tailed.base, Base::Name("alice/ci-notes".into()));
        assert_eq!(tailed.tail, Tail::Turn(2));
        let remote = parse_spec_with_local(Some(&repo), "bob/ci-notes").unwrap();
        assert_eq!(remote.repo, RepoSel::Slug("bob".into(), "ci-notes".into()));
    }

    #[test]
    fn local_preference_needs_a_repo_and_an_existing_branch() {
        assert_eq!(
            parse_spec_with_local(None, "alice/ci-notes").unwrap().repo,
            RepoSel::Slug("alice".into(), "ci-notes".into())
        );
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(&dir.path().join("work")).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "one"]).unwrap();
        assert!(parse_spec_with_local(Some(&repo), "deadbeef/release/x/1").is_err());
        assert_eq!(
            parse_spec_with_local(Some(&repo), "alice/ci-notes")
                .unwrap()
                .repo,
            RepoSel::Slug("alice".into(), "ci-notes".into())
        );
    }

    #[test]
    fn local_refs_keep_slashes_in_branch_names() {
        let spec = parse_local("topic/foo").unwrap();
        assert_eq!(spec.repo, RepoSel::Context);
        assert_eq!(spec.base, Base::Name("topic/foo".into()));
        assert_eq!(spec.tail, Tail::None);
    }
}
