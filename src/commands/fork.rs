//! `agit fork` — fork a new session off any ref.
//!
//! This is the **only form** "checking out an old state" takes in agit, and it is the form a
//! rollback takes as well — a rollback is a fork off an older node, and history is never
//! rewritten (PRD, "Forking and continuing").
//!
//! The new branch takes that point as its base: it inherits the point's VIEW and shared files
//! (the tree already is that one), but the **session identity must be recast** — the logical
//! session is fixed at the moment the fork is born (fork overrides inheritance from the source).
//! Otherwise two branches claim the same session, the hub's gate (one branch, one session)
//! rejects it, and so does commit's continuity check.
//!
//! By default it only creates the branch and prints the next step; `--resume` launches it in the
//! same breath.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::refs::{self, RefSpec};
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Fork point: a branch head, historic commit, tag, `X#5`, another repo’s ref, a sealed branch.
    pub source: String,
    /// New branch name.
    #[arg(short = 'b', long, value_name = "branch")]
    pub branch: String,
    /// Launch it right away.
    #[arg(long)]
    pub resume: bool,
    /// Switch runtime.
    #[arg(long = "as", value_name = "runtime", requires = "resume")]
    pub as_runtime: Option<String>,
    /// Restore in this directory.
    #[arg(long, value_name = "dir", requires = "resume")]
    pub cwd: Option<PathBuf>,
    /// With --resume: prepare only, don’t launch.
    #[arg(long, requires = "resume")]
    pub no_launch: bool,
}

/// Where a fork starts from: (source repo, source slug, resolved ref).
pub struct ForkBase {
    pub repo: Repo,
    pub slug: String,
    pub resolved: refs::Resolved,
}

/// Reused by run: resolve the fork point.
pub fn resolve_base(source: &str, cwd: &std::path::Path) -> crate::Result<Option<ForkBase>> {
    let spec = refs::parse(source)?;
    let (repo, slug) = match open_source_repo(&spec, cwd)? {
        Some(v) => v,
        None => return Ok(None),
    };
    // Base::At → the branch from the session environment (never falls back to the pin or cwd).
    let spec = match super::context::substitute_at(spec) {
        Ok(s) => s,
        Err(e) => {
            ui::error(&format!("failed to resolve @: {e:#}"));
            return Ok(None);
        }
    };
    match refs::resolve(&repo, &spec) {
        Ok(resolved) => Ok(Some(ForkBase {
            repo,
            slug,
            resolved,
        })),
        Err(e) => {
            ui::error(&format!("failed to resolve `{source}`: {e:#}"));
            Ok(None)
        }
    }
}

/// Open the local repo, restricted to the repo named in the ref.
fn open_source_repo(
    spec: &RefSpec,
    cwd: &std::path::Path,
) -> crate::Result<Option<(Repo, String)>> {
    match &spec.repo {
        refs::RepoSel::Slug(o, n) => {
            let dir = config::repo_dir(o, n)?;
            match Repo::open(&dir) {
                Some(r) => Ok(Some((r, format!("{o}/{n}")))),
                None => {
                    ui::error(&format!("{o}/{n} doesn’t exist locally."));
                    ui::hint(&format!(
                        "fetch it: `agit fetch {o}/{n}` (an existing read-only checkout) or `agit clone {o}/{n}`"
                    ));
                    Ok(None)
                }
            }
        }
        refs::RepoSel::Local(name) => {
            let me = crate::infra::credentials::current_user().unwrap_or_else(|| "local".into());
            let found = super::clone::checkouts_named(&me, name)?;
            match found.as_slice() {
                [only] => Ok(Some((Repo::at(&only.path), only.slug()))),
                [] => {
                    ui::error(&format!("no local repo named `{name}`."));
                    ui::hint(&format!(
                        "`agit repo list` shows what’s local; or write the full owner/{name}"
                    ));
                    Ok(None)
                }
                many => {
                    ui::error(&format!(
                        "`{name}` exists {} times locally — write the full form:",
                        many.len()
                    ));
                    for c in many {
                        eprintln!("  {}", c.slug());
                    }
                    Ok(None)
                }
            }
        }
        refs::RepoSel::Context => {
            let ctx = match super::context::resolve(cwd) {
                Ok(c) => c,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    ui::hint("or name the repo: `agit fork <owner/repo>@<ref> -b <new-name>`");
                    return Ok(None);
                }
            };
            let (o, n) = super::parse_slug(&ctx.repo)?;
            match Repo::open(config::repo_dir(&o, &n)?) {
                Some(r) => Ok(Some((r, ctx.repo))),
                None => {
                    ui::error(&format!("{} doesn’t exist locally.", ctx.repo));
                    ui::hint(&format!("fetch it first: `agit clone {}`", ctx.repo));
                    Ok(None)
                }
            }
        }
    }
}

/// Mint a new logical session id (lineage is fixed when the fork is born).
pub fn mint_claim(source_sha: &str, branch: &str) -> String {
    use sha2::Digest as _;
    let seed = format!("fork:{source_sha}:{branch}:{}", uuid::Uuid::new_v4());
    let hex = hex::encode(sha2::Sha256::digest(seed.as_bytes()));
    format!("{}{}", meta::ID_PREFIX, &hex[..meta::ID_HEX_LEN])
}

/// Create the fork branch, identity-recast commit included. Returns the new branch's head sha.
pub fn fork_branch(
    base: &ForkBase,
    source_desc: &str,
    new_name: &str,
) -> crate::Result<Option<String>> {
    let repo = &base.repo;
    if let Err(e) = crate::domain::repo::valid_branch_name(new_name) {
        ui::error(&format!("{e:#}"));
        ui::hint("pick another name: `agit fork <point> -b <other>`");
        return Ok(None);
    }
    if repo.has_ref(&format!("refs/heads/{new_name}")) {
        ui::error(&format!("branch `{new_name}` already exists."));
        ui::hint("pick another name: `agit fork <point> -b <other>`");
        return Ok(None);
    }
    let base_sha = &base.resolved.sha;
    let Some(base_snap) = meta::read_at_ref(repo, base_sha) else {
        ui::error(&format!(
            "{source_desc} carries no {} — its line was never declared.",
            meta::FILE
        ));
        ui::hint("re-fetch this checkout: `agit fetch` (or `agit clone` again)");
        return Ok(None);
    };
    if base_snap.is_file_line() {
        ui::error(&format!(
            "{source_desc} is a point on the file line (no session there)."
        ));
        ui::hint("the file line is for memory inheritance only: `agit new -b <name> --from <ref>`");
        return Ok(None);
    }

    // Identity recast: replace the session in meta. The source tree is otherwise inherited, but
    // the branch-local seal must not cross the fork boundary: forking is the sanctioned way to
    // continue from a sealed line.
    let log = storage::materialize_at(repo.root(), base_sha, meta::LOG_FILE)?;
    let view = storage::materialize_at(repo.root(), base_sha, meta::VIEW_FILE)?;
    let log = if base_snap.layout == meta::LayoutVersion::V0 {
        storage::make_view_reachable(&log, &view)?
    } else {
        log
    };
    let mut snap = base_snap;
    snap.session = mint_claim(base_sha, new_name);
    snap.runtime_instances = Vec::new(); // instances belong to the source branch, not inherited
    // This commit establishes a new branch identity; it is not another user turn even when the
    // source tip's last commit was one. Backends use `kind` to build the turn timeline.
    snap.kind = meta::Kind::File;
    snap.milestone = Some(format!("fork from {source_desc}"));
    snap.layout = meta::LayoutVersion::CURRENT;
    let snap_text = meta::to_text(&snap)?;

    // A historic fork point may still be v0. The fork's tip is a freshly written commit, so it
    // must be upgraded to the current layout while keeping the source VIEW's full context and the
    // original session annotation carried by each envelope.
    let tree = super::plumbing::session_snapshot_tree(repo, base_sha, &log, &view, &snap_text)?;
    let tree = super::plumbing::tree_apply(repo, &tree, &[(super::branch::SEAL_FILE, None)])?;
    let commit = super::plumbing::commit_tree(
        repo,
        &tree,
        &[base_sha],
        &format!("agit: fork {new_name} from {source_desc}"),
    )?;
    // CAS: the branch must not exist (checked above; this is the real guarantee).
    super::plumbing::update_ref_cas(repo, &format!("refs/heads/{new_name}"), &commit, None)?;
    Ok(Some(commit))
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let Some(base) = resolve_base(&args.source, &cwd)? else {
        return Ok(ExitCode::Ref);
    };
    let Some(commit) = fork_branch(&base, &args.source, &args.branch)? else {
        return Ok(ExitCode::Policy);
    };
    ui::success(&format!(
        "forked {} into {} ({} @ {} — new session in place)",
        args.source,
        args.branch,
        base.slug,
        &commit[..9.min(commit.len())]
    ));

    if !args.resume {
        // The hint must be pasteable unchanged. A fork can happen in a repo this directory is
        // not bound to (the kind `run` read-only clones on its own), where a bare branch name
        // resolves against the bound repo and reports "no such branch". `switch` only knows
        // branches in the bound repo, so it goes unmentioned when that repo is not this one.
        let full = format!("{}@{}", base.slug, args.branch);
        let bound_here = crate::domain::workspace::read(&cwd).is_some_and(|w| w.repo == base.slug);
        println!("{}", ui::dim("  next:"));
        println!("    agit resume {full:<40} bring this session up");
        if bound_here {
            println!(
                "    agit switch {:<40} pin this directory to it (optional)",
                args.branch
            );
        }
        return Ok(ExitCode::Ok);
    }

    let rargs = super::resume::Args {
        target: Some(args.branch.clone()),
        as_runtime: args.as_runtime.clone(),
        cwd: args.cwd.clone(),
        no_launch: args.no_launch,
        force: false,
    };
    match super::resume::resume_branch(&base.repo, &base.slug, &args.branch, &rargs)? {
        Some(res) => super::resume::finish_pub(res, args.no_launch),
        None => Ok(ExitCode::Precondition),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::{self, Envelope};

    #[test]
    fn forking_a_v0_history_point_writes_a_v1_tip_without_losing_context() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let claim = format!("{}{}", meta::ID_PREFIX, "a".repeat(40));
        let content = serde_json::json!({"type": "user", "message": "v0 context"});
        let envelope = Envelope {
            source: "codex".into(),
            session_id: claim.clone(),
            object_hash: transcript::object_hash(&content),
            content,
        };
        let line = storage::envelope_line(&envelope);
        let view_only_content = serde_json::json!({"type": "__merge_summary__"});
        let view_only = storage::envelope_line(&Envelope {
            source: "codex".into(),
            session_id: claim.clone(),
            object_hash: transcript::object_hash(&view_only_content),
            content: view_only_content,
        });
        let mut old_meta = meta::Meta::new(claim, "codex".into(), "/work".into());
        old_meta.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &old_meta).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), &line).unwrap();
        std::fs::write(
            repo.root().join(meta::LEGACY_VIEW_FILE),
            format!("{line}{view_only}"),
        )
        .unwrap();
        std::fs::write(
            repo.root().join(super::super::branch::SEAL_FILE),
            "sealed\n",
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 turn").unwrap();
        let old_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let base = ForkBase {
            repo,
            slug: "local/repo".into(),
            resolved: refs::Resolved {
                branch: Some("main".into()),
                sha: old_head,
                turn: None,
                event_index: None,
                range: None,
                path: None,
            },
        };

        let fork = fork_branch(&base, "main", "forked").unwrap().unwrap();
        let fork_meta = meta::read_at_ref(&base.repo, &fork).unwrap();
        assert!(
            base.repo
                .show_raw(&base.resolved.sha, super::super::branch::SEAL_FILE)
                .is_some()
        );
        assert!(
            base.repo
                .show_raw(&fork, super::super::branch::SEAL_FILE)
                .is_none()
        );
        assert_eq!(fork_meta.layout, meta::LayoutVersion::V1);
        assert_eq!(fork_meta.kind, meta::Kind::File);
        assert!(base.repo.show_raw(&fork, meta::LEGACY_LOG_FILE).is_none());
        assert_eq!(
            storage::materialize_at(base.repo.root(), &fork, meta::LOG_FILE).unwrap(),
            format!("{line}{view_only}")
        );
        assert_eq!(
            storage::materialize_at(base.repo.root(), &fork, meta::VIEW_FILE).unwrap(),
            format!("{line}{view_only}")
        );
    }

    #[test]
    fn fork_refuses_v0_user_tree_collision_with_v1_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        let claim = format!("{}{}", meta::ID_PREFIX, "c".repeat(40));
        let content = serde_json::json!({"type":"user","message":"context"});
        let line = storage::envelope_line(&Envelope {
            source: "codex".into(),
            session_id: claim.clone(),
            object_hash: transcript::object_hash(&content),
            content,
        });
        let mut old_meta = meta::Meta::new(claim, "codex".into(), "/work".into());
        old_meta.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &old_meta).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), &line).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), &line).unwrap();
        std::fs::write(repo.root().join(meta::VIEW_FILE), "user-owned root VIEW\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 collision").unwrap();
        let old_head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let base = ForkBase {
            repo,
            slug: "local/repo".into(),
            resolved: refs::Resolved {
                branch: Some("main".into()),
                sha: old_head.clone(),
                turn: None,
                event_index: None,
                range: None,
                path: None,
            },
        };

        let error = fork_branch(&base, "main", "forked").unwrap_err();
        assert!(error.to_string().contains("user-owned"), "{error:#}");
        assert!(!base.repo.has_ref("refs/heads/forked"));
        assert_eq!(
            base.repo
                .show_raw_result(&old_head, meta::VIEW_FILE)
                .unwrap()
                .as_deref(),
            Some("user-owned root VIEW\n")
        );
    }
}
