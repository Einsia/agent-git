//! The resolution chain for "who a command that omits its target acts on".
//!
//! # The PRD's "what is the current branch" section, item by item
//!
//! 1. Explicit arguments (positional, `--repo`, `-C <dir>`) — the caller checks these first;
//! 2. the session environment `AGIT_SESSION` (injected when `run`/`resume`/`new`/`merge` start a
//!    runtime, in the form `<owner>/<name>@<branch>`) — the main road for an agent calling agit
//!    inside its own session;
//! 3. the session id environment variable the harness exposes itself;
//! 4. the branch the workspace pins (pinned explicitly by `agit switch`);
//! 5. cwd match: adopted sessions under the bound repo whose link cwd equals the current
//!    directory, used only when there is exactly one; several list the candidates (the caller
//!    decides between a picker and an error, exit code 8/3);
//! 6. all of them fail: error out and give `agit import` or `agit status` as the next step.
//!
//! `@` takes only steps 2 and 3 — the cwd and pin fallbacks do not apply to it (a merge agent and
//! a parallel session can sit in the same directory, and a guess there names the wrong one).
//!
//! Every command echoes the route it resolved through on its first line ([`Context::via`]).

use crate::domain::refs::{Base, RefSpec};
use crate::domain::{link, workspace};
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

/// The "current session id" environment variable the harness exposes itself (step 3).
///
/// Reads `AGIT_SESSION` only (step 2); no fallback of any kind.
///
/// For hooks: what they ask is "who was this process started for", and **not** "which stretch of
/// session is running now" — the answer to that one is in the SessionStart payload.
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

/// Step 3: the session id the harness exposes → look up the link in the store to get what it
/// belongs to.
pub fn from_env() -> Option<Context> {
    let injected = match std::env::var("AGIT_SESSION") {
        Ok(v) => {
            let decoded = decode_session_env(&v);
            if decoded.is_none() {
                crate::warn(
                    "AGIT_SESSION is malformed (expected <owner>/<name>@<branch>) — ignored",
                );
            }
            decoded
        }
        Err(_) => None,
    };
    pick_env_context(injected, from_harness_env())
}

/// How steps 2 and 3 are weighed against each other.
///
/// `AGIT_SESSION` is injected at the moment the runtime starts and does not change anywhere in
/// the process tree; the harness session id follows the **current** transcript. Once `/new` or
/// `/resume` has switched sessions inside the TUI, the former points at the line it started on
/// and the latter is the conversation in front of you — when the two disagree, what the link
/// registers is the truth about the conversation in front of you, and the injected identity is
/// only what it was called when it was born.
fn pick_env_context(
    injected: Option<(String, String)>,
    harness: Option<(Context, bool)>,
) -> Option<Context> {
    match (injected, harness) {
        (Some((repo, branch)), Some((live, pinned)))
            if !same_line(&repo, &branch, &live, pinned) =>
        {
            crate::warn(&format!(
                "AGIT_SESSION says {repo}@{branch}, but this runtime session is adopted onto {}@{} — using the latter",
                live.repo, live.branch
            ));
            Some(Context {
                repo: live.repo,
                branch: live.branch,
                via: "harness session env (AGIT_SESSION points at another line)",
            })
        }
        (Some((repo, branch)), _) => Some(Context {
            repo,
            branch,
            via: "AGIT_SESSION (the agent session hosting this process)",
        }),
        (None, live) => live.map(|(c, _)| c),
    }
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

/// Step 3: harness environment variable → the link in the store → what it belongs to. Only the
/// form we wrote into the link is looked up; a miss, or a link that belongs to nothing yet (the
/// `hooks ingest` pre-registration), gives up.
/// The returned `bool` is whether the link records a namespace (it decides how the comparison
/// against `AGIT_SESSION` runs, see [`same_line`]).
fn from_harness_env() -> Option<(Context, bool)> {
    let Ok(store) = crate::domain::store::Store::open_or_init() else {
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
                    // The branch name is the one the link registered. A prefix of the session id
                    // is not a branch name — the branch is what `-b` gave at import time — and
                    // returning one makes a zero-argument command in a terminal with no
                    // AGIT_SESSION resolve a "branch" that does not exist at all.
                    branch: lk
                        .branch
                        .clone()
                        .unwrap_or_else(|| link::short(&lk.session_id)),
                    via: "harness session env",
                },
                pinned,
            ));
        }
    }
    None
}

/// Replace the `@` in a ref with the current session branch: steps 2 and 3 only, with no pin or
/// cwd fallback.
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
        base: Base::Name(ctx.branch),
        ..spec
    })
}

/// The session identity behind `@`, with repo and branch from **one** read.
///
/// A caller that needs both repo and branch (cherry-pick's source repo) uses this instead of taking
/// the branch from [`substitute_at`] and then the repo from [`from_env`]: on the harness path the
/// link file may be under whole-file rewrite by another settlement process, and two reads tear into
/// two different identities. The refusal is spelled out here, once.
pub fn at_context() -> Result<Context> {
    from_env().ok_or_else(|| {
        anyhow::anyhow!(
            "`@` requires the session environment (AGIT_SESSION or a harness session id); \
             it never falls back to workspace/CWD — name the branch instead"
        )
    })
}

/// The workspace pin (step 4).
pub fn from_workspace(cwd: &std::path::Path) -> Option<Context> {
    let (repo, branch) = workspace::pinned(cwd)?;
    Some(Context {
        repo,
        branch,
        via: "workspace pin (agit switch)",
    })
}

/// cwd match (step 5): returns the candidates (none, one, or several).
pub fn from_cwd(cwd: &std::path::Path) -> Vec<link::Link> {
    let Ok(store) = crate::domain::store::Store::open_or_init() else {
        return Vec::new();
    };
    let cwd_s = cwd.to_string_lossy().to_string();
    link::list(&store)
        .into_iter()
        .filter(|l| l.cwd.as_deref() == Some(cwd_s.as_str()))
        .collect()
}

/// The full zero-argument resolution (without step 1 — those are the caller's own arguments).
///
/// Exactly one candidate at step 5 returns directly; several go back to the caller (packed into an
/// Err with the information; the caller decides exit code 8/3 from the tty).
pub fn resolve(cwd: &std::path::Path) -> Result<Context> {
    if let Some(c) = from_env() {
        return Ok(c);
    }
    if let Some(c) = from_workspace(cwd) {
        return Ok(c);
    }
    let cands = from_cwd(cwd);
    match cands.as_slice() {
        [only] => {
            let repo = slug_of_link(only).ok_or_else(|| {
                anyhow::anyhow!("this session belongs to no agent yet (link-only)")
            })?;
            Ok(Context {
                // Nearly every consumer runs `parse_slug` on `Context::repo` first, so what comes
                // out here has to be a full `owner/agent`: the namespace the link records wins, and
                // only a link without one is filled in from the signed-in account — otherwise a
                // context resolved from cwd fails with "use the <owner>/<agent> form" in command
                // after command, under a name the user never typed.
                repo,
                branch: only
                    .branch
                    .clone()
                    .unwrap_or_else(|| link::short(&only.session_id).to_string()),
                via: "cwd match (this directory’s only adopted session)",
            })
        }
        [] => anyhow::bail!(
            "can’t resolve the target: not in an agent session, no pinned branch, no adopted session in this directory.
  \
  next: adopt one with `agit import`, or check state with `agit status`"
        ),
        many => {
            let mut msg = format!("this directory has {} adopted sessions — I won’t guess:", many.len());
            for l in many {
                let a = slug_of_link(l).unwrap_or_else(|| "(unadopted)".into());
                msg.push_str(&format!("\n  - {a} / {}", link::short(&l.session_id)));
            }
            msg.push_str("\n  next: pin one with `agit switch <branch>`, or write the ref explicitly");
            let e: Result<()> = Err(anyhow::anyhow!(msg));
            e?;
            unreachable!()
        }
    }
}

/// Resolve "which repo" alone, without demanding a branch at the same time.
///
/// For commands where **the branch is already given explicitly** (`agit resume <branch>`,
/// `agit scan <ref>`).
///
/// [`resolve`] demands repo and branch in one go, so a directory that has just finished
/// `agit clone` and holds no adopted session yet fails outright — and "clone, then resume a
/// branch" is exactly the main path of design W3. The directory binding already answers "which
/// repo"; failing to answer "which branch" must not throw away a branch name that was given.
///
/// The order matches [`resolve`], except that step 4 is relaxed to the **binding** rather than the
/// pin alone.
pub fn repo_for(cwd: &std::path::Path) -> Result<String> {
    if let Some(c) = from_env() {
        return Ok(c.repo);
    }
    if let Some(w) = workspace::read(cwd) {
        return Ok(w.repo);
    }
    let cands = from_cwd(cwd);
    match cands.as_slice() {
        [only] => slug_of_link(only)
            .ok_or_else(|| anyhow::anyhow!("this session belongs to no agent yet (link-only)")),
        [] => anyhow::bail!(
            "can’t tell which agent this is about: this directory isn’t bound to one \
             and has no adopted session.
  \
  next: `agit clone <owner>/<name>` here, or name it: `agit resume <owner>/<name>@<branch>`"
        ),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|l| slug_of_link(l).unwrap_or_else(|| "(unadopted)".into()))
                .collect();
            anyhow::bail!(
                "this directory has {} adopted sessions — I won’t guess which agent:\n  - {}\n  \
                 next: name it explicitly, e.g. `<owner>/<name>@<branch>`",
                many.len(),
                names.join("\n  - ")
            )
        }
    }
}

/// Fill a repo that may be a bare name out into `owner/name`.
///
/// A link stores the bare agent name (the only thing it knows when it is born), while every place
/// that looks a repo up by path wants `owner/name`. Left unfilled, a context resolved from cwd
/// fails at `parse_slug` with "use the <owner>/<agent> form" — under a name the user never typed,
/// one we resolved ourselves.
/// The full `owner/agent` a link belongs to: the namespace it records if it has one, otherwise
/// filled in from the signed-in account.
///
/// Every path that goes from a link back to a repo (Stop settlement, SessionStart writing
/// `AGIT_SESSION` back, the legacy `commit` form) takes it from here; otherwise a session in an
/// organization repo is filled in as `<me>/<agent>` on one of them.
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

    /// When the injected `AGIT_SESSION` disagrees with what the conversation in front of you
    /// belongs to, the latter wins; on the same line the injected identity takes effect unchanged
    /// (its owner may be someone else's repo).
    #[test]
    fn a_live_harness_link_overrides_a_stale_agit_session() {
        let live = Context {
            repo: "me/qa".into(),
            branch: "s2".into(),
            via: "harness session env",
        };
        let picked = pick_env_context(
            Some(("me/qa".into(), "s1".into())),
            Some((live.clone(), false)),
        )
        .unwrap();
        assert_eq!(
            (picked.repo.as_str(), picked.branch.as_str()),
            ("me/qa", "s2")
        );
        assert!(picked.via.starts_with("harness session env"));

        let picked = pick_env_context(
            Some(("alice/qa".into(), "s2".into())),
            Some((live.clone(), false)),
        )
        .unwrap();
        assert_eq!(picked.repo, "alice/qa");
        assert!(picked.via.starts_with("AGIT_SESSION"));

        let picked = pick_env_context(Some(("me/qa".into(), "s1".into())), None).unwrap();
        assert_eq!(picked.branch, "s1");

        assert_eq!(
            pick_env_context(None, Some((live, false))).unwrap().branch,
            "s2"
        );
        assert!(pick_env_context(None, None).is_none());
    }

    /// A machine with nobody signed in must not mistake "the owner cannot be filled in" for "the
    /// session was switched".
    ///
    /// A link records the bare agent name, and filling the owner in relies on the signed-in
    /// account — with nobody signed in there is nothing to fill it from. Compared by full slug,
    /// every zero-argument command reports once that `AGIT_SESSION` is stale and then "corrects"
    /// onto the very same branch.
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
        // The branch really did change — this is the one to correct.
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
            Some("einsia/qa".to_string()),
            "the stale injected identity must lose to the pinned harness link"
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
