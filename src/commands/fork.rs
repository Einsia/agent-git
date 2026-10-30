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
use crate::domain::session_privacy::{ForkContext, ForkOrigin, SessionPrivacy};
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
    /// The ref name the source names, with `@` and an omitted ref already replaced by the
    /// branch they stand for. `None` when the source names no ref at all.
    pub name: Option<String>,
}

impl ForkBase {
    /// The Hub's fork locator: `<owner/repo>@<name>#<turn>` when the fork point has a turn
    /// ordinal, otherwise the exact commit. A branch name without a turn would follow the
    /// branch as it moves, so it never stands alone.
    pub fn locator(&self, turn: Option<u32>) -> String {
        match (&self.name, self.resolved.turn.or(turn)) {
            (Some(name), Some(turn)) => format!("{}@{name}#{turn}", self.slug),
            _ => format!("{}@{}", self.slug, self.resolved.sha),
        }
    }
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
    let name = ref_name(&repo, &spec);
    match refs::resolve(&repo, &spec) {
        Ok(resolved) => Ok(Some(ForkBase {
            repo,
            slug,
            resolved,
            name,
        })),
        Err(e) if e.is::<refs::Ambiguous>() => {
            Err(e.context(format!("failed to resolve `{source}`")))
        }
        Err(e) => {
            ui::error(&format!("failed to resolve `{source}`: {e:#}"));
            Ok(None)
        }
    }
}

/// The ref name a resolved-`@` spec names, as [`ForkBase::name`] holds it.
pub fn ref_name(repo: &Repo, spec: &RefSpec) -> Option<String> {
    match &spec.base {
        refs::Base::Name(name) | refs::Base::SessionBranch(name) => Some(name.clone()),
        refs::Base::Default => repo.current_branch(),
        refs::Base::At => None,
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
        refs::RepoSel::Local(name) => match super::clone::unique_checkout(name)? {
            Some(checkout) => Ok(Some((Repo::at(&checkout.path), checkout.slug()))),
            None => {
                ui::error(&format!("no local repo named `{name}` — write owner/repo."));
                Ok(None)
            }
        },
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

    let source_ref = base.locator(base_snap.turn);
    let mut origin = ForkOrigin {
        commit: base_sha.clone(),
        context: ForkContext::Original,
        snapshot: None,
    };
    let mut snap = base_snap;
    let original = match super::privacy::unlock::envelope_bytes(repo, base_sha)? {
        Some(bytes) => {
            let envelope = crate::domain::privacy_envelope::PrivacyEnvelope::parse(&bytes)?;
            let (snapshot, layer) = super::privacy::unlock::open_session_original(
                repo, base_sha, &bytes, &envelope, true,
            )?;
            origin.snapshot = Some(snapshot);
            if layer.is_none() {
                origin.context = ForkContext::Sanitized;
            }
            layer
        }
        None => None,
    };
    let (log, view) = if let Some(layer) = original {
        layer.import_protection(repo)?;
        snap = serde_json::from_value(layer.metadata.clone())?;
        snap.cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        layer.session_bytes()?
    } else {
        (
            zeroize::Zeroizing::new(storage::materialize_at(
                repo.root(),
                base_sha,
                meta::LOG_FILE,
            )?),
            zeroize::Zeroizing::new(storage::materialize_at(
                repo.root(),
                base_sha,
                meta::VIEW_FILE,
            )?),
        )
    };
    let log = if snap.layout == meta::LayoutVersion::V0 {
        zeroize::Zeroizing::new(storage::make_view_reachable(&log, &view)?)
    } else {
        log
    };
    if origin.snapshot.is_some() {
        ui::info(match origin.context {
            ForkContext::Original => "Fork context: authorized originals.",
            ForkContext::Sanitized => "Fork context: sanitized snapshot.",
        });
    }
    snap.committer_account = crate::infra::credentials::current_account()?;
    snap.session_privacy = Some(SessionPrivacy::new(snap.committer_account.clone())?);
    snap.fork_origin = Some(origin);
    snap.source_ref = Some(source_ref);
    snap.source_commit = Some(base_sha.clone());
    snap.session = mint_claim(base_sha, new_name);
    snap.runtime_instances = Vec::new(); // instances belong to the source branch, not inherited
    snap.baseline_bytes = None;
    snap.cwd_state = None;
    snap.cwd_is_agent_repository = false;
    // This commit establishes a new branch identity; it is not another user turn even when the
    // source tip's last commit was one. Backends use `kind` to build the turn timeline.
    snap.kind = meta::Kind::File;
    // The source is named only by `source_ref`, which a publication keeps or withholds by rule.
    snap.milestone = Some("fork".into());
    snap.layout = meta::LayoutVersion::CURRENT;
    let snap_text = meta::to_text(&snap)?;

    // A historic fork point may still be v0. The fork's tip is a freshly written commit, so it
    // must be upgraded to the current layout while keeping the source VIEW's full context and the
    // original session annotation carried by each envelope.
    let tree = super::plumbing::session_snapshot_tree(repo, base_sha, &log, &view, &snap_text)?;
    let tree = super::plumbing::tree_apply(
        repo,
        &tree,
        &[
            (super::branch::SEAL_FILE, None),
            ("privacy/envelope.json", None),
        ],
    )?;
    let commit =
        super::plumbing::commit_tree(repo, &tree, &[base_sha], &format!("agit: fork {new_name}"))?;
    // CAS: the branch must not exist (checked above; this is the real guarantee).
    super::plumbing::update_ref_cas(repo, &format!("refs/heads/{new_name}"), &commit, None)?;
    Ok(Some(commit))
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let spec = crate::input_argument(refs::parse(&args.source))?;
    let Some(base) = resolve_base(&args.source, &cwd)? else {
        return Ok(ExitCode::Ref);
    };
    let Some(commit) = fork_branch(&base, &args.source, &args.branch)? else {
        return Ok(ExitCode::Policy);
    };
    let target = if matches!(spec.tail, refs::Tail::None) {
        base.resolved.branch.as_ref().unwrap_or(&base.resolved.sha)
    } else {
        &base.resolved.sha
    };
    super::echo::emit(
        "fork",
        &[super::echo::Selection::new(
            format!("{}@{target}", base.slug),
            super::echo::Source::for_spec(&spec),
        )],
    );
    ui::success(&format!(
        "forked {} into {} ({} @ {} — new session in place)",
        args.source,
        args.branch,
        base.slug,
        &commit[..9.min(commit.len())]
    ));

    if !args.resume {
        // A fully qualified hint remains valid independently of the caller's environment.
        let full = format!("{}@{}", base.slug, args.branch);
        ui::info(ui::dim("  next:"));
        ui::info(format_args!(
            "    agit resume {full:<40} bring this session up"
        ));
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
    fn shared_fork_resolution_does_not_classify_stored_sources_as_arguments() {
        let error = resolve_base("alice/payments@refund-fix#0", std::path::Path::new("."))
            .err()
            .expect("invalid reference syntax must fail before repository resolution");
        assert_eq!(
            crate::commands::terminal_error_code(&error, ExitCode::Failure),
            ExitCode::Failure
        );
    }

    /// The fork tip is written in the current layout with the source's full context, and names its
    /// source only through `source_ref`: a message or milestone that spelled out the reference
    /// would carry a private source into every publication that withholds `source_ref`.
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
            name: Some("main".into()),
        };

        let fork = fork_branch(&base, "local/repo@main", "forked")
            .unwrap()
            .unwrap();
        let fork_meta = meta::read_at_ref(&base.repo, &fork).unwrap();
        assert_eq!(fork_meta.milestone.as_deref(), Some("fork"));
        assert_eq!(
            base.repo.git(&["log", "-1", "--format=%B", &fork]).unwrap(),
            "agit: fork forked"
        );
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
            name: Some("main".into()),
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
