//! Ordinary command targets come only from explicit arguments or `AGIT_SESSION`.
//!
//! A runtime identity may reject a stale environment target, but never supplies a replacement.
//! Workspace bindings and discovered transcripts are descriptive state, not session selectors.

use crate::domain::link;
use crate::domain::refs::{Base, RefSpec};
use crate::{ExitCode, Result};

/// The resolved context target.
#[derive(Debug, Clone)]
pub struct Context {
    /// `owner/name`.
    pub repo: String,
    /// The branch (the session line).
    pub branch: String,
    /// The resolution route that matched (printed on the first line).
    pub via: &'static str,
}

impl Context {
    /// Split out owner/name.
    pub fn owner_name(&self) -> Result<(String, String)> {
        super::parse_slug(&self.repo)
    }
}

// `AGIT_SESSION` is encoded and decoded in exactly one place, [`crate::infra::runtime_session`]:
// `rc` writes this variable too, and it does not depend on the `cli` feature, so it cannot reach
// this layer. The two below are thin wrappers for `commands`.
/// The form of `AGIT_SESSION`: `<owner>/<name>@<branch>`.
pub fn encode_session_env(repo: &str, branch: &str) -> String {
    crate::infra::runtime_session::encode_env(repo, branch)
}

/// Decode `AGIT_SESSION` back.
pub fn decode_session_env(v: &str) -> Option<(String, String)> {
    crate::infra::runtime_session::decode_env(v)
}

/// The explicitly injected identity, without runtime discovery.
/// Hooks use this only when deciding whether a startup event may claim its requested branch.
pub fn from_session_env() -> Option<(String, String)> {
    let v = std::env::var("AGIT_SESSION").ok()?;
    match decode_session_env(&v) {
        Some(pair) => Some(pair),
        None => {
            crate::warn("AGIT_SESSION is malformed (expected <owner>/<name>@<branch>) — ignored");
            None
        }
    }
}

/// Resolve the supplied environment target while refusing a conflicting live claim.
pub fn from_env() -> Option<Context> {
    let injected = from_session_env()?;
    pick_env_context(Some(injected), from_harness_env())
}

/// Runtime evidence is a veto, never a source of an implicit command target.
fn pick_env_context(
    injected: Option<(String, String)>,
    harness: Option<(Context, bool)>,
) -> Option<Context> {
    let (repo, branch) = injected?;
    if let Some((live, owner_pinned)) = harness
        && !same_line(&repo, &branch, &live, owner_pinned)
    {
        crate::warn(&format!(
            "AGIT_SESSION says {repo}@{branch}, but this runtime session is adopted onto {}@{}; refusing the stale target. Name owner/repo@branch explicitly or update AGIT_SESSION",
            live.repo, live.branch
        ));
        return None;
    }
    Some(Context {
        repo,
        branch,
        via: "AGIT_SESSION",
    })
}

/// Whether `AGIT_SESSION` and the harness link name the same line.
///
/// `owner_pinned` is whether the link records a namespace of its own. One that does is compared by
/// full slug: the same name and the same branch under a different namespace are two lines (personal
/// `me/qa@work` and organization `einsia/qa@work`), and a stale `AGIT_SESSION` must not pull
/// settlement back into the personal repo. One that does not (a legacy link) is compared by agent
/// name and branch alone: the owner of such a link is filled in from the signed-in account and
/// cannot be filled in at all with nobody signed in, while the owner in `AGIT_SESSION` may be
/// someone else's repo — comparing by owner would judge one line to be two.
fn same_line(repo: &str, branch: &str, live: &Context, owner_pinned: bool) -> bool {
    if owner_pinned {
        return repo == live.repo && branch == live.branch;
    }
    let name = |r: &str| r.rsplit('/').next().unwrap_or(r).to_string();
    name(repo) == name(&live.repo) && branch == live.branch
}

/// Read a registered runtime claim solely to detect stale `AGIT_SESSION` values.
/// The flag records whether its namespace is authoritative rather than a legacy account fallback.
fn from_harness_env() -> Option<(Context, bool)> {
    let Ok(Some(store)) = crate::domain::store::Store::open() else {
        return None;
    };
    let all = link::list(&store);
    for (var, runtime) in crate::infra::runtime_session::ENV_SESSIONS {
        let Ok(sid) = std::env::var(var) else {
            continue;
        };
        if sid.is_empty() {
            continue;
        }
        let hits: Vec<_> = all
            .iter()
            .filter(|l| l.source == *runtime && l.session_id == sid)
            .collect();
        if let [lk] = hits.as_slice()
            && let Some(repo) = slug_of_link(lk)
        {
            let pinned = lk.owner.is_some();
            return Some((
                Context {
                    repo,
                    branch: lk.branch.clone()?,
                    via: "harness session env",
                },
                pinned,
            ));
        }
    }
    None
}

/// Replace `@` with the branch explicitly supplied through `AGIT_SESSION`.
///
/// The resolution layer ([`crate::domain::refs::resolve`]) does not read the environment, so a
/// `Base::At` reaching it is a bug; every command that hands it a ref the user typed passes through
/// here first. A spec that is not `@` comes back unchanged.
pub fn substitute_at(spec: RefSpec) -> Result<RefSpec> {
    if spec.base != Base::At {
        return Ok(spec);
    }
    let ctx = at_context()?;
    Ok(RefSpec {
        base: Base::SessionBranch(ctx.branch),
        ..spec
    })
}

/// Resolve the repository and branch behind `@` from the same environment selection.
pub fn at_context() -> Result<Context> {
    from_env().ok_or_else(|| {
        anyhow::anyhow!(
            "`@` requires a valid AGIT_SESSION=<owner>/<repo>@<branch>; name owner/repo@branch explicitly otherwise"
        )
    })
}

/// Resolve an omitted target without using directory or runtime identity as a fallback.
pub fn resolve(_cwd: &std::path::Path) -> Result<Context> {
    from_env().ok_or_else(|| anyhow::anyhow!(
        "no explicit session target; name <owner>/<repo>@<branch> or set AGIT_SESSION=<owner>/<repo>@<branch>"
    ))
}

/// Resolve the repository portion of the explicitly supplied session environment.
pub fn repo_for(cwd: &std::path::Path) -> Result<String> {
    resolve(cwd).map(|context| context.repo)
}

/// Recover the namespace of an explicitly selected link for hooks and adoption.
/// A recorded namespace wins; legacy links use the current account or the local namespace.
pub fn slug_of_link(lk: &crate::domain::link::Link) -> Option<String> {
    let agent = lk.agent.as_deref()?;
    Some(slug_for(
        agent,
        lk.owner.as_deref(),
        crate::infra::credentials::current_user().as_deref(),
    ))
}

/// The pure test behind [`slug_of_link`]: the recorded namespace > the signed-in account > `local`.
///
/// The last rung is the owner `agit init` gives a repo when nobody is signed in; giving the same
/// name here is what lets a link still resolve to `~/.agit/repos/local/<agent>` on a machine with
/// no credentials, instead of a bare name that cannot get through `parse_slug`.
fn slug_for(agent: &str, owner: Option<&str>, me: Option<&str>) -> String {
    match (owner, me) {
        (Some(owner), _) => format!("{owner}/{agent}"),
        (None, Some(me)) => format!("{me}/{agent}"),
        (None, None) => format!("local/{agent}"),
    }
}

pub fn qualify(repo: &str) -> String {
    if repo.contains('/') {
        return repo.to_string();
    }
    match crate::infra::credentials::current_user() {
        Some(me) => format!("{me}/{repo}"),
        None => repo.to_string(),
    }
}

/// The standard exit code for a command that needs interaction when there are several candidates
/// and no tty.
pub const NEED_INTERACTIVE: ExitCode = ExitCode::Interactive;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_claim_only_vetoes_a_stale_environment_target() {
        let live = Context {
            repo: "me/qa".into(),
            branch: "s2".into(),
            via: "runtime claim",
        };
        assert!(pick_env_context(None, Some((live.clone(), true))).is_none());
        assert!(
            pick_env_context(
                Some(("me/qa".into(), "s1".into())),
                Some((live.clone(), true))
            )
            .is_none()
        );
        let matched =
            pick_env_context(Some(("me/qa".into(), "s2".into())), Some((live, true))).unwrap();
        assert_eq!(
            (matched.repo.as_str(), matched.branch.as_str()),
            ("me/qa", "s2")
        );
        assert_eq!(matched.via, "AGIT_SESSION");
        assert!(pick_env_context(None, None).is_none());
    }

    /// A missing legacy owner cannot prove that the explicit environment target names another line.
    #[test]
    fn a_missing_owner_is_not_a_switched_session() {
        let live = |repo: &str, branch: &str| Context {
            repo: repo.into(),
            branch: branch.into(),
            via: "harness session env",
        };
        // Nobody signed in: the harness side carries only the bare name, still the same line.
        assert!(same_line(
            "nana/payments",
            "refund-fix",
            &live("payments", "refund-fix"),
            false
        ));
        // Signed in: both sides are full slugs.
        assert!(same_line(
            "nana/payments",
            "refund-fix",
            &live("nana/payments", "refund-fix"),
            false
        ));
        // A different branch proves that the supplied target is stale.
        assert!(!same_line(
            "nana/payments",
            "refund-fix",
            &live("payments", "flaky-test"),
            false
        ));
        // A changed agent counts as a change too.
        assert!(!same_line(
            "nana/payments",
            "refund-fix",
            &live("infra", "refund-fix"),
            false
        ));
    }

    /// A link that records a namespace is compared by full slug: the same name and the same branch
    /// under a different namespace are two lines, and a stale `AGIT_SESSION=me/qa@work` must not
    /// pull the organization session in front of you back into the personal repo. An implementation
    /// that compares bare names alone judges the two to be one line and picks `me/qa` back.
    #[test]
    fn a_pinned_namespace_makes_a_same_named_personal_line_a_different_line() {
        let live = Context {
            repo: "einsia/qa".into(),
            branch: "work".into(),
            via: "harness session env",
        };
        assert!(!same_line("me/qa", "work", &live, true));
        assert!(same_line("einsia/qa", "work", &live, true));
        assert_eq!(
            pick_env_context(
                Some(("me/qa".into(), "work".into())),
                Some((live.clone(), true))
            )
            .map(|c| c.repo),
            None,
            "a conflicting runtime claim must refuse the supplied target"
        );
    }

    /// The three rungs from link to slug: the recorded namespace, the signed-in account, and
    /// `local` when nobody is signed in — the last rung is the same name `agit init` gives a repo
    /// with nobody signed in, and must not degrade to a bare name that cannot get through
    /// `parse_slug`.
    #[test]
    fn a_link_resolves_to_a_parseable_slug_even_without_credentials() {
        assert_eq!(slug_for("qa", Some("einsia"), Some("me")), "einsia/qa");
        assert_eq!(slug_for("qa", None, Some("me")), "me/qa");
        assert_eq!(slug_for("qa", None, None), "local/qa");
        assert!(super::super::parse_slug(&slug_for("qa", None, None)).is_ok());
    }
}
