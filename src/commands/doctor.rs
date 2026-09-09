//! `agit doctor` — diagnostics.
//!
//! # It must answer one concrete question
//!
//! **"Is the version I wrote down still the one I wrote down?"**
//!
//! Version = commit SHA gives that question a far stronger answer than a per-turn hash can:
//! comparing a "recorded per-turn hash" against a "recomputed per-turn hash" leaves out thinking
//! blocks and encrypted reasoning, so an edit to those bytes is undetectable. A version ID is a
//! git commit SHA (a content address covering the whole parent → tree → blobs tree):
//!
//! 1. **The metadata is readable**: every commit carries `session/meta.json`; failing to read it
//!    is a partial checkout.
//! 2. **The VIEW is self-consistent**: every event the VIEW references is reachable in the log,
//!    and merge markers pair up and close.
//! 3. **The transcript is still append-only**: the live transcript in the runtime directory must
//!    have the committed bytes as its prefix. This one needs a local copy of what was committed:
//!    a staging area deleted once push succeeds leaves nothing to compare against.
//!
//! Every conclusion points straight at the next action.

use super::CmdResult;
use super::skill_bundle;
use crate::domain::link;
use crate::domain::meta;
use crate::domain::repo::{self, Repo};
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::infra::config;
use crate::infra::credentials;
use crate::{ExitCode, adapter, ui};
use anyhow::Context as _;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// Restrict repository and adopted-session checks to this local owner/repo
    #[arg(long, value_name = "OWNER/REPO", value_parser = parse_repo)]
    pub repo: Option<String>,

    /// Also check backend connectivity
    #[arg(long)]
    pub check_backend: bool,
}

fn parse_repo(value: &str) -> Result<String, String> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err("expected a complete owner/repo, without a branch or selector".into());
    };
    for component in [owner, name] {
        if component != component.trim() {
            return Err("owner/repo must not contain whitespace".into());
        }
        repo::valid_name(component).map_err(|error| error.to_string())?;
    }
    Ok(value.to_owned())
}

enum Check {
    Ok(String),
    Warn(String),
    Err(String),
}

pub fn run(args: Args) -> CmdResult {
    let selected = if let Some(slug) = args.repo.as_deref() {
        parse_repo(slug).map_err(anyhow::Error::msg)?;
        let (owner, name) = super::parse_slug(slug)?;
        let path = config::repo_dir(&owner, &name)?;
        let Some(repo) = Repo::open(&path) else {
            ui::error(&format!("local repository {slug} is unavailable"));
            return Ok(ExitCode::Ref);
        };
        if let Err(error) = super::migration::check_readonly_repo_startup(&repo) {
            ui::error(&format!("local storage inspection failed: {error:#}"));
            return Ok(ExitCode::Precondition);
        }
        Some((owner, name, path))
    } else {
        None
    };
    let s = ui::theme::symbols();
    let mut checks: Vec<(String, Check)> = vec![];
    let mut fatal = false;

    // ── Runtimes ──
    for ad in adapter::all() {
        checks.push(runtime_row(ad.as_ref()));
    }

    // ── Skill installation ──
    // The Skill is not part of the session store; it has its own per-runtime home and version. A
    // missing one is a warning, since a user may have installed only one runtime — but an
    // installation that exists must be complete and at the current version.
    checks.extend(skill_installation_checks());

    // ── git ──
    match std::process::Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => checks.push((
            "git".into(),
            Check::Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        )),
        _ => {
            fatal = true;
            checks.push((
                "git".into(),
                Check::Err("unavailable — agit depends on git".into()),
            ));
        }
    }

    // ── store ──
    //
    // The counts come from the link files; no transcript is opened. The link list is hoisted to
    // the outer scope because the live-transcript comparison pairs against it too.
    let store = Store::open()?;
    let links: Vec<link::Link> = store
        .as_ref()
        .map(link::list)
        .unwrap_or_default()
        .into_iter()
        .filter(|claim| {
            selected.as_ref().is_none_or(|(owner, name, _)| {
                claim.owner.as_deref() == Some(owner.as_str())
                    && claim.agent.as_deref() == Some(name.as_str())
            })
        })
        .collect();
    match &store {
        Some(_) => {
            let committed = links.iter().filter(|l| l.agent.is_some()).count();
            checks.push((
                "local store".into(),
                Check::Ok(format!(
                    "{} adopted sessions, {} versioned",
                    links.len(),
                    committed
                )),
            ));
        }
        // Not an error: having adopted no session yet is the normal state of a fresh install.
        None => checks.push((
            "local store".into(),
            Check::Warn(
                "no sessions adopted yet (adopt one with `agit import <session-id> --from <runtime> --into <owner/repo>@<branch>`)"
                    .into(),
            ),
        )),
    }

    // ── Local repos ──
    let agents = match &selected {
        Some(repo) => vec![repo.clone()],
        None => super::clone::list_local()?,
    };
    let unpushed: Vec<&String> = agents
        .iter()
        .filter(|(_, _, p)| {
            let r = crate::domain::repo::Repo::at(p);
            !matches!(r.ahead_behind(), Some((0, _)))
        })
        .map(|(_, n, _)| n)
        .collect();
    if !agents.is_empty() {
        checks.push((
            "agent repos".into(),
            if unpushed.is_empty() {
                Check::Ok(format!("{}, all published", agents.len()))
            } else {
                Check::Warn(format!(
                    "{} of them, {} with unpublished commits: {}",
                    agents.len(),
                    unpushed.len(),
                    unpushed
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            },
        ));
    }

    // ── Sign-in ──
    checks.push((
        "sign-in".into(),
        match credentials::current_user() {
            Some(u) => Check::Ok(format!("{u} @ {}", config::hub_url())),
            // Recording a version needs a sign-in (the commit author and the repo path both
            // carry the account name), and `import` records an initial version by default;
            // `--link-only` / log / show / doctor do not.
            None => Check::Warn(
                "not signed in — import / commit / push all need agit login first (except agit import --link-only)"
                    .into(),
            ),
        },
    ));

    // ── Secret keystore ──
    // Probed the way a commit uses it. A store that opens but refuses writes, or a vault whose
    // key the configured store does not hold, fails the first commit that finds a secret — on
    // a machine with no desktop session, that is the first sign anything is wrong.
    checks.push(("secret keystore".into(), keystore_row()));

    // ── Backend ──
    if args.check_backend {
        let client = crate::hub::Client::from_env();
        checks.push((
            "backend".into(),
            match client.health() {
                Ok(h) => Check::Ok(format!(
                    "{} @ {} (version {})",
                    h.status,
                    client.base(),
                    h.version.unwrap_or_else(|| "unknown".into())
                )),
                Err(e) => Check::Warn(first_line(&format!("{e:#}"))),
            },
        ));
    }

    // ── Print ──
    println!("{}", ui::bold("agit doctor"));
    if let Some((owner, name, _)) = &selected {
        println!("Repository scope: {owner}/{name}");
    }
    println!();
    for (label, c) in &checks {
        let (mark, text) = match c {
            Check::Ok(t) => (ui::ok(s.check), t.clone()),
            Check::Warn(t) => (ui::warn_text(s.warn), ui::warn_text(t)),
            Check::Err(t) => (ui::err_text(s.cross), ui::err_text(t)),
        };
        println!("  [{mark}] {label:16} {text}");
    }

    // ── Session metadata integrity ──
    ui::section("session metadata integrity");
    if agents.is_empty() {
        println!(
            "  {}",
            ui::dim("nothing committed yet — no metadata to check")
        );
    } else {
        let sp = ui::spinner(&format!(
            "checking session metadata of {} repos…",
            agents.len()
        ));
        // Old-layout repos collapse into one warning instead of scrolling by one at a time:
        // the tag, view and continuity checks all say the same thing about them, which carries
        // no information.
        let mut old_layout: Vec<(String, PathBuf)> = vec![];
        let mut new_agents: Vec<&(String, String, PathBuf)> = vec![];
        for a in &agents {
            let (o, n, p) = a;
            if is_old_layout(p) {
                old_layout.push((format!("{o}/{n}"), p.clone()));
            } else {
                new_agents.push(a);
            }
        }

        let mut findings: Vec<String> = vec![];
        let mut ok = 0usize;
        for (o, n, p) in &new_agents {
            let slug = format!("{o}/{n}");
            let repo = crate::domain::repo::Repo::at(p);
            // Every branch's metadata must be readable: the main checkout sits on main, so a
            // broken session branch is exposed by no checkout at all — this is the only place
            // that looks at it.
            for error in session_roots_checked(p).1 {
                findings.push(format!("{slug}: {error}"));
            }
            match meta::resolve(p) {
                Ok(_) => {
                    // One of doctor's hard checks. A version ID is a commit SHA, which is
                    // itself the content address of the whole parent→tree→events→VIEW tree, so
                    // no per-commit machine tag has to be verified: HEAD only has to exist.
                    // VIEW self-consistency is what `check_view` below answers.
                    match repo.git_opt(&["rev-parse", "HEAD"]) {
                        Some(_) => {
                            ok += 1;
                        }
                        None => findings.push(format!("the repo of {slug} has no commits")),
                    }
                }
                Err(e) => findings.push(format!("{slug} {}", first_line(&format!("{e:#}")))),
            }
        }

        // ── View comparison: every event the VIEW references must be reachable in the log ──
        let mut view_ok = 0usize;
        let mut view_errs: Vec<String> = vec![];
        for (o, n, p) in &new_agents {
            for (branch, root) in session_roots(p) {
                let slug = format!("{o}/{n}@{branch}");
                match check_view_root(&root) {
                    Ok(None) => {}
                    Ok(Some(ViewNote::Ok)) => view_ok += 1,
                    Ok(Some(ViewNote::MissingView)) => view_errs.push(format!(
                        "{slug}: {} present but {} missing",
                        meta::LOG_FILE,
                        meta::VIEW_FILE
                    )),
                    Ok(Some(ViewNote::Unreachable { count })) => view_errs.push(format!(
                        "{slug}: VIEW references {count} events the log doesn’t have (VIEW consistency broken)",
                    )),
                    Ok(Some(ViewNote::UnbalancedMarkers { count })) => view_errs.push(format!(
                        "{slug}: merge markers don’t pair up ({count} unclosed)",
                    )),
                    Err(error) => view_errs.push(format!(
                        "{slug}: storage is unreadable: {}",
                        first_line(&format!("{error:#}"))
                    )),
                }
            }
        }
        sp.finish_and_clear();

        if findings.is_empty() {
            println!(
                "  {} all {ok} repos’ session metadata is consistent",
                ui::ok(s.check)
            );
        } else {
            println!(
                "  {} {} consistent, {} problems",
                ui::warn_text(s.warn),
                ok,
                findings.len()
            );
            for f in findings.iter().take(8) {
                println!("    {}", ui::err_text(f));
            }
            if findings.len() > 8 {
                println!("    {}", ui::dim(&format!("… {} more", findings.len() - 8)));
            }
            ui::hint("the committed copy is still in the agent repo — nothing was lost");
        }

        match view_errs.len() {
            0 if view_ok == 0 => println!(
                "  {}",
                ui::dim("no views to compare yet (no repo has a session log)")
            ),
            0 => println!(
                "  {} the VIEW of {view_ok} repos all reference reachable events",
                ui::ok(s.check)
            ),
            e => {
                println!(
                    "  {} view check: {e} problems ({view_ok} ok)",
                    ui::err_text(s.cross)
                );
                for f in view_errs.iter().take(8) {
                    println!("    {}", ui::err_text(f));
                }
                if view_errs.len() > 8 {
                    println!(
                        "    {}",
                        ui::dim(&format!("… and {} more", view_errs.len() - 8))
                    );
                }
                ui::hint(
                    "doctor only reports: the next version (`agit commit`) rebuilds the VIEW wholesale",
                );
            }
        }

        if !old_layout.is_empty() {
            let names: Vec<&str> = old_layout.iter().map(|(slug, _)| slug.as_str()).collect();
            println!(
                "  {} found {} old-layout repos (session files outside session/): {}",
                ui::warn_text(s.warn),
                old_layout.len(),
                names.join(", ")
            );
            for (slug, p) in old_layout.iter().take(8) {
                println!("    {}", ui::dim(&format!("{slug}  {}", ui::tilde(p))));
            }
            ui::hint(
                "the layout is not migrated in place: remove these directories, re-`agit import` and record a first version",
            );
        }
    }

    print_live_comparisons(&links);

    // ── Environment summary ──
    ui::section("environment");
    print!(
        "{}",
        ui::table::key_values(&[
            ("agit version", env!("CARGO_PKG_VERSION").to_string()),
            (
                "system",
                format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH)
            ),
            ("AGIT_HOME", ui::tilde(&config::agit_home()?)),
            ("hub", config::hub_url()),
        ])
    );

    if !args.check_backend {
        ui::hint("add --check-backend to probe hub connectivity");
    }

    Ok(if fatal {
        ExitCode::Failure
    } else {
        ExitCode::Ok
    })
}

/// One keystore row: which store, whether it answers, and what stops working when it does not.
fn keystore_row() -> Check {
    use crate::domain::secret_filter::KeystoreHealth;
    use crate::infra::config::SecretKeystore;
    let label = |keystore: Option<SecretKeystore>, dir: Option<&Path>| match (keystore, dir) {
        (Some(SecretKeystore::File), Some(dir)) => format!("file keystore {}", ui::tilde(dir)),
        (Some(SecretKeystore::File), None) => "file keystore".to_string(),
        (Some(SecretKeystore::Os), _) => "OS credential store".to_string(),
        (None, _) => format!("`{}`", SecretKeystore::KEY),
    };
    match crate::domain::secret_filter::keystore_health() {
        KeystoreHealth::Ok {
            keystore,
            dir,
            vault,
        } => Check::Ok(format!(
            "{} — {vault}",
            label(Some(keystore), dir.as_deref())
        )),
        KeystoreHealth::Problem { keystore, dir, why } => Check::Warn(format!(
            "{}: {why} — `agit secrets add` and any commit that finds a secret fail",
            label(keystore, dir.as_deref())
        )),
    }
}

/// One runtime diagnostic row: capability tier + format family + next step.
///
/// The capability label carries more than the path (desktop-apps.md §4.5): Cursor's `cursor` is
/// on PATH, but it is a VS Code-style launcher that cannot resume a session — an [OK] row
/// carrying a path reads as "`--as cursor` works", when it is rejected before any work starts.
/// So a row that is not `Resumable` does not probe PATH; it states the capability and "what
/// then".
fn runtime_row(ad: &dyn adapter::Adapter) -> (String, Check) {
    let label = format!("runtime {}", ad.id());
    let cap = ad.capability();
    let info = format!("{} · format {}", cap.label(), ad.format());
    match cap {
        adapter::Capability::Resumable => match adapter::which(ad.cli()) {
            Some(p) => (label, Check::Ok(format!("{info} · {}", ui::tilde(&p)))),
            None => (
                label,
                Check::Warn(format!("{info} · `{}` is not on PATH", ad.cli())),
            ),
        },
        adapter::Capability::ImportOnly | adapter::Capability::ExportOnly => {
            (label, Check::Ok(format!("{info} · {}", cap.next_hint())))
        }
    }
}

fn skill_installation_checks() -> Vec<(String, Check)> {
    let mut checks = Vec::new();
    for runtime in ["claude-code", "codex", "opencode", "cursor"] {
        if let Some(path) = super::setup::skill_path(runtime) {
            checks.push((format!("skill {runtime}"), check_skill_dir(&path, runtime)));
        }
        if let Some(path) = super::setup::legacy_inline_skill_path(runtime)
            && legacy_inline_skill_exists(&path, runtime)
        {
            checks.push((
                format!("skill {runtime} legacy"),
                Check::Warn(format!(
                    "the legacy inline manual is still at {}; run `agit setup --runtime {runtime} --skill`",
                    path.display()
                )),
            ));
        }
    }
    checks
}

fn legacy_inline_skill_exists(path: &Path, runtime: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let (begin, end) = if runtime == "cursor" {
        (
            skill_bundle::CURSOR_BEGIN_MARKER,
            skill_bundle::CURSOR_END_MARKER,
        )
    } else {
        (skill_bundle::BEGIN_MARKER, skill_bundle::END_MARKER)
    };
    let Some(start) = text.find(begin) else {
        return false;
    };
    let body_start = start + begin.len();
    let Some(relative_end) = text[body_start..].find(end) else {
        return false;
    };
    text[body_start..body_start + relative_end].contains("<!-- agit:skill-version:")
}

fn check_skill_dir(dir: &Path, runtime: &str) -> Check {
    let entrypoint = dir.join("SKILL.md");
    let version_file = dir.join(skill_bundle::VERSION_FILE);
    let refs = dir.join(skill_bundle::REFERENCES_DIR);
    let mut issues = Vec::new();

    match std::fs::read_to_string(&entrypoint) {
        Ok(body) if body == skill_bundle::entrypoint() => {}
        Ok(_) => issues.push("the entrypoint manual is out of date".to_string()),
        Err(_) => issues.push("the entrypoint manual is missing".to_string()),
    }
    match std::fs::read_to_string(&version_file) {
        Ok(version) if version.trim() == skill_bundle::version() => {}
        Ok(version) => issues.push(format!(
            "version {}, current {}",
            version.trim(),
            skill_bundle::version()
        )),
        Err(_) => issues.push("the version file is missing".to_string()),
    }

    let expected: std::collections::BTreeSet<&str> = skill_bundle::SUBSKILLS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    for (name, expected_body) in skill_bundle::SUBSKILLS {
        match std::fs::read_to_string(refs.join(format!("{name}.md"))) {
            Ok(body) if body == *expected_body => {}
            Ok(_) => issues.push(format!("sub-skill {name} is out of date")),
            Err(_) => issues.push(format!("sub-skill {name} is missing")),
        }
    }
    if let Ok(entries) = std::fs::read_dir(&refs) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) == Some("md")
                && path
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .is_some_and(|name| !expected.contains(name))
            {
                issues.push(format!(
                    "obsolete sub-skill {} is present",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }

    if issues.is_empty() {
        Check::Ok(format!(
            "v{} · {} command references · {}",
            skill_bundle::version(),
            skill_bundle::SUBSKILLS.len(),
            dir.display()
        ))
    } else {
        Check::Warn(format!(
            "{}; run `agit setup --runtime {runtime} --skill`",
            issues.join("; ")
        ))
    }
}

/// An old-layout repo: its session files do not live under `session/`.
///
/// Two generations of old layout each leave one signature: one spreads transcripts across
/// `sessions/<runtime>/<id>.jsonl`, the other drops the three session files at the repo root
/// (`snapshot.json` / `transcript.jsonl`). Both count as "not recognized" — there is no
/// migration code; doctor only names them and says what to do about them.
fn is_old_layout(repo_root: &Path) -> bool {
    repo_root.join("sessions").is_dir()
        || repo_root.join("snapshot.json").is_file()
        || repo_root.join("transcript.jsonl").is_file()
}

/// The committed transcript envelope text: the worktree copy, falling back to the HEAD blob
/// when that file is gone (deleted by hand).
/// Where the content of one session branch is read from.
///
/// A branch with a worktree is read from the worktree (which sees bytes written mid-settlement);
/// one without is read by ref — a branch's content does not depend on being checked out.
#[derive(Debug, Clone)]
enum SessionRoot {
    Worktree(PathBuf),
    Ref { repo: PathBuf, branch: String },
}

impl SessionRoot {
    fn meta(&self) -> crate::Result<Option<meta::Meta>> {
        match self {
            SessionRoot::Worktree(root) => {
                if !meta::path_in(root).exists() {
                    return Ok(None);
                }
                meta::resolve(root).map(Some)
            }
            SessionRoot::Ref { repo, branch } => meta::read_at_ref_result(
                &crate::domain::repo::Repo::at(repo),
                &format!("refs/heads/{branch}"),
            ),
        }
    }
}

/// Where to read every session branch with a claimed identity in one repo; with none, this
/// falls back to the checkout root.
///
/// A branch whose metadata cannot be read (git read failure, bad JSON, a violated invariant) is
/// not "absent": it goes into the second return value and the caller records it as a finding —
/// such a branch is exactly what doctor exists to diagnose.
fn session_roots_checked(repo_root: &Path) -> (Vec<(String, SessionRoot)>, Vec<String>) {
    let repo = crate::domain::repo::Repo::at(repo_root);
    let worktrees = repo.worktrees().unwrap_or_default();
    let mut errors = Vec::new();
    let mut out: Vec<(String, SessionRoot)> = repo
        .local_branches()
        .into_iter()
        .filter(
            |branch| match meta::read_at_ref_result(&repo, &format!("refs/heads/{branch}")) {
                Ok(Some(m)) => m.is_session_line() && !m.session.is_empty(),
                Ok(None) => false,
                Err(error) => {
                    errors.push(format!(
                        "branch `{branch}`: {}",
                        first_line(&format!("{error:#}"))
                    ));
                    false
                }
            },
        )
        .map(|branch| {
            let root = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(branch.as_str()))
                .map(|w| SessionRoot::Worktree(w.path.clone()))
                .unwrap_or_else(|| SessionRoot::Ref {
                    repo: repo_root.to_path_buf(),
                    branch: branch.clone(),
                });
            (branch, root)
        })
        .collect();
    if out.is_empty() && meta::path_in(repo_root).exists() {
        out.push((
            "HEAD".into(),
            SessionRoot::Worktree(repo_root.to_path_buf()),
        ));
    }
    (out, errors)
}

fn session_roots(repo_root: &Path) -> Vec<(String, SessionRoot)> {
    session_roots_checked(repo_root).0
}

/// A claim is compared only with its recorded owner, repository, and local branch.
fn compare_claim(lk: &link::Link) -> crate::Result<(ContinuityNote, Option<String>)> {
    let owner = lk.owner.as_deref().context("claim has no recorded owner")?;
    let agent = lk
        .agent
        .as_deref()
        .context("claim has no recorded repository")?;
    let branch = lk
        .branch
        .as_deref()
        .context("claim has no recorded branch")?;
    let slug = format!("{owner}/{agent}");
    parse_repo(&slug)
        .map_err(anyhow::Error::msg)
        .context("claim has invalid repository identity")?;
    let repo = crate::domain::repo::Repo::open(config::repo_dir(owner, agent)?)
        .context("claimed repository is not available locally")?;
    if repo
        .git_status(&["check-ref-format", &format!("refs/heads/{branch}")])?
        .0
        != Some(0)
    {
        anyhow::bail!("claim has an invalid local branch name");
    }
    let head = repo
        .git(&[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch}^{{commit}}"),
        ])
        .context("claimed local branch cannot be read")?;
    let snapshot = meta::read_at_ref_result(&repo, &head)?
        .context("claimed branch has no committed session metadata")?;
    if !snapshot.is_session_line() || snapshot.session.is_empty() {
        anyhow::bail!("claimed branch is not a recorded session line");
    }
    let live = lk.read_bytes().context("live transcript cannot be read")?;
    if lk.baseline_bytes.is_some() || lk.baseline_hash.is_some() || lk.materialized_from.is_some() {
        let lineage = match lk.materialized_from.as_deref() {
            Some(tip) if tip == head => None,
            Some(_) => Some("materialized tip differs from the current branch tip".to_owned()),
            None => Some("materialized branch-tip evidence is unavailable".to_owned()),
        };
        return Ok((check_materialized(lk, &live)?, lineage));
    }
    let live = std::str::from_utf8(&live).context("live transcript is not valid UTF-8")?;
    let log = repo
        .show_result(&head, meta::LOG_FILE)?
        .context("claimed branch has no committed LOG")?;
    let committed = committed_content(&log)?;
    let (committed, live) = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?
        .hydrate_pair_readonly(&committed, live)
        .context("repository secret reconstruction is unavailable")?;
    if committed.unresolved != 0 || live.unresolved != 0 {
        anyhow::bail!("repository secret mappings needed for comparison are unavailable");
    }
    Ok((check_continuity(&committed.text, &live.text)?, None))
}

fn committed_content(log: &str) -> crate::Result<String> {
    let mut raw = String::new();
    for line in log.split_inclusive('\n') {
        let envelope = crate::domain::storage::parse_envelope_line(line)?;
        raw.push_str(&serde_json::to_string(&envelope.content)?);
        raw.push('\n');
    }
    Ok(raw)
}

fn check_materialized(lk: &link::Link, live: &[u8]) -> crate::Result<ContinuityNote> {
    use sha2::Digest as _;
    let baseline = usize::try_from(
        lk.baseline_bytes
            .context("materialized byte baseline is missing")?,
    )?;
    let expected = lk
        .baseline_hash
        .as_deref()
        .context("materialized baseline digest is missing")?;
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("materialized baseline digest is invalid");
    }
    if live.len() < baseline {
        return Ok(ContinuityNote::Truncated);
    }
    let actual = hex::encode(sha2::Sha256::digest(&live[..baseline]));
    if !actual.eq_ignore_ascii_case(expected) {
        return Ok(ContinuityNote::Rewritten);
    }
    Ok(ContinuityNote::Clean {
        appended: live.len() - baseline,
    })
}

fn print_live_comparisons(links: &[link::Link]) {
    ui::section("live transcript comparison");
    let mut checked = 0;
    let mut appended = 0;
    let mut truncated = 0;
    let mut rewritten = 0;
    let mut unavailable = 0;
    for lk in links.iter().filter(|lk| {
        lk.is_active()
            && (lk.agent.is_some()
                || lk.owner.is_some()
                || lk.branch.is_some()
                || lk.baseline_bytes.is_some()
                || lk.baseline_hash.is_some()
                || lk.materialized_from.is_some())
    }) {
        let identity = format!(
            "{}/{}@{} ({} {})",
            lk.owner.as_deref().unwrap_or("<unknown-owner>"),
            lk.agent.as_deref().unwrap_or("<unknown-repo>"),
            lk.branch.as_deref().unwrap_or("<unknown-branch>"),
            lk.source,
            link::short(&lk.session_id)
        );
        match compare_claim(lk) {
            Ok((note, lineage)) => {
                checked += 1;
                let (status, detail) = match note {
                    ContinuityNote::Clean { appended: 0 } => {
                        ("clean", "recorded content is unchanged")
                    }
                    ContinuityNote::Clean { .. } => {
                        appended += 1;
                        ("appended", "new content follows the recorded content")
                    }
                    ContinuityNote::Truncated => {
                        truncated += 1;
                        (
                            "truncated",
                            "live content is shorter than the recorded content",
                        )
                    }
                    ContinuityNote::Rewritten => {
                        rewritten += 1;
                        (
                            "rewritten",
                            "live content differs inside the recorded content",
                        )
                    }
                };
                println!("  {identity}: {status} — {detail}");
                if let Some(lineage) = lineage {
                    println!("    {}", ui::warn_text(&lineage));
                }
            }
            Err(error) => {
                unavailable += 1;
                println!(
                    "  {identity}: unavailable — {}",
                    first_line(&format!("{error:#}"))
                );
            }
        }
    }
    if checked == 0 && unavailable == 0 {
        println!("  {}", ui::dim("no active claimed transcripts to compare"));
    } else {
        println!(
            "  checked {checked} live transcripts: {appended} appended, {truncated} truncated, {rewritten} rewritten; {unavailable} unavailable"
        );
    }
    if truncated > 0 || rewritten > 0 {
        ui::hint(
            "inspect the explicit recorded branch and native transcript before choosing an import or fork; doctor changes neither",
        );
    }
}

/// A malformed carrier is unavailable evidence, rather than an empty matching history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuityNote {
    Clean { appended: usize },
    Truncated,
    Rewritten,
}

fn raw_hashes(text: &str) -> crate::Result<Vec<String>> {
    let mut current = Vec::new();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|_| {
            let kind = if line.ends_with('\n') {
                "malformed"
            } else {
                "unfinished"
            };
            anyhow::anyhow!("live transcript has a {kind} record at line {}", index + 1)
        })?;
        current.push(transcript::object_hash(&value));
    }
    Ok(current)
}

fn check_continuity(stored: &str, live: &str) -> crate::Result<ContinuityNote> {
    let stored = raw_hashes(stored)?;
    let current = raw_hashes(live)?;
    if stored.iter().zip(&current).any(|(a, b)| a != b) {
        return Ok(ContinuityNote::Rewritten);
    }
    if current.len() < stored.len() {
        return Ok(ContinuityNote::Truncated);
    }
    Ok(ContinuityNote::Clean {
        appended: current.len() - stored.len(),
    })
}

/// The view-comparison verdict for one repo. No log = nothing to compare against (None).
///
/// The PRD's "doctor" section: VIEW self-consistency = every referenced event is reachable +
/// merge markers pair up and close. After a cherry-pick / revert / merge the VIEW is a
/// **surgical product** rather than an ordered suffix, so what is checked here is "the reachable
/// set + marker pairing", not a subsequence.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ViewNote {
    Ok,
    /// The log is there and the VIEW is not.
    MissingView,
    /// How many events the VIEW references are absent from the log (and are not synthetic
    /// markers).
    Unreachable {
        count: usize,
    },
    /// How many merge / cherry-pick __start__ / __end__ markers are unpaired.
    UnbalancedMarkers {
        count: usize,
    },
}

#[cfg(test)]
fn check_view(repo_root: &Path) -> crate::Result<Option<ViewNote>> {
    check_view_root(&SessionRoot::Worktree(repo_root.to_path_buf()))
}

fn check_view_root(root: &SessionRoot) -> crate::Result<Option<ViewNote>> {
    let Some(snapshot) = root.meta()? else {
        return Ok(None);
    };
    if snapshot.is_file_line() || snapshot.session.is_empty() {
        return Ok(None);
    }
    let stored_view = match snapshot.layout {
        meta::LayoutVersion::V0 => meta::LEGACY_VIEW_FILE,
        meta::LayoutVersion::V1 => meta::VIEW_FILE,
    };
    let (t, v) = match root {
        SessionRoot::Worktree(repo_root) => {
            let t = crate::domain::storage::materialize_worktree(repo_root, meta::LOG_FILE)
                .context("LOG is unreadable")?;
            match std::fs::symlink_metadata(repo_root.join(stored_view)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Some(ViewNote::MissingView));
                }
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            let v = crate::domain::storage::materialize_worktree(repo_root, meta::VIEW_FILE)
                .context("VIEW is unreadable")?;
            (t, v)
        }
        SessionRoot::Ref { repo, branch } => {
            let refname = format!("refs/heads/{branch}");
            let t = crate::domain::storage::materialize_at(repo, &refname, meta::LOG_FILE)
                .context("LOG is unreadable")?;
            let git = crate::domain::repo::Repo::at(repo);
            if git.show_raw(&refname, stored_view).is_none() {
                return Ok(Some(ViewNote::MissingView));
            }
            let v = crate::domain::storage::materialize_at(repo, &refname, meta::VIEW_FILE)
                .context("VIEW is unreadable")?;
            (t, v)
        }
    };
    let reachable: std::collections::HashSet<String> = t
        .split_inclusive('\n')
        .filter_map(|line| crate::domain::storage::event_id(line).ok())
        .collect();
    let mut unreachable = 0usize;
    for line in v.split_inclusive('\n') {
        let Ok(id) = crate::domain::storage::event_id(line) else {
            unreachable += 1;
            continue;
        };
        if !reachable.contains(&id) {
            unreachable += 1;
        }
    }
    if unreachable > 0 {
        return Ok(Some(ViewNote::Unreachable { count: unreachable }));
    }
    let unbalanced = crate::domain::storage::unbalanced_view_markers(&v)?;
    if unbalanced != 0 {
        return Ok(Some(ViewNote::UnbalancedMarkers { count: unbalanced }));
    }
    Ok(Some(ViewNote::Ok))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    /// The main checkout sits on a healthy main while one session branch has broken metadata.
    /// This pins that the branch surfaces as a finding instead of vanishing from the
    /// enumeration.
    #[test]
    fn a_corrupt_branch_metadata_is_a_finding_not_an_absence() {
        use crate::domain::meta::{self, Meta};
        use crate::domain::repo::Repo;
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(r.root(), &Meta::new_file_line()).unwrap();
        r.add_all().unwrap();
        r.commit("init").unwrap();
        r.git(&["checkout", "--quiet", "-b", "good", "main"])
            .unwrap();
        let mut snap = Meta::new_session_line("codex".into(), "/w".into());
        snap.session = format!("{}{}", meta::ID_PREFIX, "e".repeat(meta::ID_HEX_LEN));
        meta::write(r.root(), &snap).unwrap();
        r.add_all().unwrap();
        r.commit("good").unwrap();
        r.git(&["checkout", "--quiet", "-b", "broken", "main"])
            .unwrap();
        std::fs::write(meta::path_in(r.root()), "{not json").unwrap();
        r.add_all().unwrap();
        r.commit("broken").unwrap();
        r.git(&["checkout", "--quiet", "main"]).unwrap();

        let (roots, errors) = super::session_roots_checked(r.root());
        assert_eq!(
            roots.iter().map(|(b, _)| b.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("broken"), "{errors:?}");
    }

    use super::{
        Check, ContinuityNote, ViewNote, check_continuity, check_skill_dir, check_view,
        is_old_layout, runtime_row,
    };
    use crate::adapter::{self, Capability};
    use crate::domain::meta::{self, LayoutVersion, Meta};
    use crate::domain::transcript;
    use std::path::{Path, PathBuf};

    const SRC: &str = "codex";
    const SID: &str = "agit-0123456789abcdef0123456789abcdef01234567";

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let has_current_storage = files
            .iter()
            .any(|(name, _)| matches!(*name, meta::LOG_FILE | meta::VIEW_FILE));
        if has_current_storage {
            let mut snapshot = Meta::new(SID.into(), SRC.into(), "/work".into());
            snapshot.layout = LayoutVersion::V0;
            meta::write(d.path(), &snapshot).unwrap();
        }
        for (name, text) in files {
            let name = match *name {
                meta::LOG_FILE => meta::LEGACY_LOG_FILE,
                meta::VIEW_FILE => meta::LEGACY_VIEW_FILE,
                other => other,
            };
            let p = d.path().join(name);
            // The session files live under `session/`, and `fs::write` creates no intermediate
            // directories.
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, text).unwrap();
        }
        d
    }

    #[test]
    fn backend_check_is_opt_in() {
        // doctor runs to completion offline — a network request by default hangs it
        // inexplicably with no connectivity.
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            a: super::Args,
        }
        let w = W::parse_from(["x"]);
        assert!(!w.a.check_backend);
    }

    #[test]
    fn skill_dir_check_detects_current_and_stale_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("references/commands")).unwrap();
        std::fs::write(
            dir.path().join("SKILL.md"),
            crate::commands::skill_bundle::entrypoint(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(crate::commands::skill_bundle::VERSION_FILE),
            format!("{}\n", crate::commands::skill_bundle::version()),
        )
        .unwrap();
        for (name, body) in crate::commands::skill_bundle::SUBSKILLS {
            std::fs::write(
                dir.path()
                    .join(crate::commands::skill_bundle::REFERENCES_DIR)
                    .join(format!("{name}.md")),
                body,
            )
            .unwrap();
        }
        assert!(matches!(check_skill_dir(dir.path(), "codex"), Check::Ok(_)));
        std::fs::write(
            dir.path()
                .join(crate::commands::skill_bundle::REFERENCES_DIR)
                .join("status.md"),
            "old",
        )
        .unwrap();
        assert!(matches!(
            check_skill_dir(dir.path(), "codex"),
            Check::Warn(_)
        ));
    }

    // ── Runtime rows: capability + format + next step ──

    /// An adapter that exists only in tests: filling in the trait is enough, since the row text
    /// comes entirely from capability / format / cli. `cli` uses a name that can never be on
    /// PATH, so the "Resumable but the binary is missing" branch deterministically hits Warn.
    struct Stub {
        cap: Capability,
    }

    impl adapter::Adapter for Stub {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn cli(&self) -> &'static str {
            "agit-test-stub-definitely-not-on-path"
        }
        fn capability(&self) -> Capability {
            self.cap
        }
        fn format(&self) -> &'static str {
            "stub-fmt"
        }
        fn sessions_for(&self, _: &Path) -> crate::Result<Vec<adapter::SessionRef>> {
            Ok(vec![])
        }
        fn resolve(&self, _: &str, _: Option<&Path>) -> Option<PathBuf> {
            None
        }
        fn all_sessions(&self) -> crate::Result<Vec<adapter::SessionRef>> {
            Ok(vec![])
        }
        fn parse(&self, _: &str) -> crate::Result<adapter::Session> {
            unimplemented!("a doctor row parses no content")
        }
        fn render(&self, _: &adapter::Session, _: &str, _: &Path) -> crate::Result<String> {
            unimplemented!("a doctor row renders nothing")
        }
        fn mint_id(&self) -> String {
            unimplemented!("a doctor row mints no id")
        }
        fn install(&self, _: &str, _: &str, _: &Path) -> crate::Result<adapter::Installed> {
            unimplemented!("a doctor row installs nothing")
        }
    }

    fn row_text(check: &Check) -> &str {
        match check {
            Check::Ok(t) | Check::Warn(t) | Check::Err(t) => t,
        }
    }

    /// Resumable while the CLI is not on PATH: a Warn whose row still states capability and
    /// format.
    #[test]
    fn a_resumable_row_reports_capability_and_format() {
        let (label, check) = runtime_row(&Stub {
            cap: Capability::Resumable,
        });
        assert_eq!(label, "runtime stub");
        assert!(
            matches!(check, Check::Warn(_)),
            "a missing CLI must be a Warn"
        );
        let t = row_text(&check);
        assert!(t.contains(Capability::Resumable.label()), "{t}");
        assert!(t.contains("format stub-fmt"), "{t}");
    }

    /// A read-only target: PATH is not probed whether or not the CLI is there (the `cursor`
    /// launcher being on PATH does not mean a session can be resumed); the row reports
    /// capability + format + next step.
    #[test]
    fn an_import_only_row_says_readonly_without_probing_path() {
        let (_, check) = runtime_row(&Stub {
            cap: Capability::ImportOnly,
        });
        assert!(
            matches!(check, Check::Ok(_)),
            "read-only is a normal state, not a warning"
        );
        let t = row_text(&check);
        assert!(t.contains(Capability::ImportOnly.label()), "{t}");
        assert!(t.contains("importable"), "{t}");
    }

    /// An export-only target (the Claude Desktop tier): the row discloses that the handoff goes
    /// to the app and the outcome cannot be observed.
    #[test]
    fn an_export_only_row_discloses_the_handoff() {
        let (_, check) = runtime_row(&Stub {
            cap: Capability::ExportOnly,
        });
        assert!(matches!(check, Check::Ok(_)));
        let t = row_text(&check);
        assert!(t.contains(Capability::ExportOnly.label()), "{t}");
        assert!(t.contains("cannot observe"), "{t}");
    }

    /// Every registered adapter's row carries its own capability label — the label and the
    /// §4.3 declaration table have one source.
    #[test]
    fn every_registered_adapter_row_shows_its_capability() {
        for ad in adapter::all() {
            let (label, check) = runtime_row(ad.as_ref());
            assert_eq!(label, format!("runtime {}", ad.id()));
            assert!(
                row_text(&check).contains(ad.capability().label()),
                "row for {} lacks the capability label",
                ad.id()
            );
        }
    }

    // ── Live-transcript comparison ──

    /// A session that kept growing after its latest version: the verdict is Clean, and the
    /// number of added lines must come out — the doctor row reporting growth reads that
    /// number.
    #[test]
    fn append_after_last_version_is_clean_with_a_line_count() {
        let v1 = "{\"a\":1}\n{\"b\":2}\n";
        let stored = v1;
        let live = format!("{v1}{{\"c\":3}}\n");
        assert_eq!(
            check_continuity(stored, &live).unwrap(),
            ContinuityNote::Clean { appended: 1 }
        );
    }

    /// A session untouched in place reports no new content, and no warning.
    #[test]
    fn an_untouched_session_is_clean_with_zero_new_lines() {
        let live = "{\"a\":1}\n{\"b\":2}\n";
        let stored = live;
        assert_eq!(
            check_continuity(stored, live).unwrap(),
            ContinuityNote::Clean { appended: 0 }
        );
    }

    /// A rewrite in the middle (not a pure append) must be judged a divergence — it is the one
    /// answer to "is the version I wrote down still the one I wrote down" that has to speak
    /// harshly.
    #[test]
    fn a_rewritten_history_is_flagged_as_diverged() {
        let stored = "{\"a\":1}\n{\"b\":2}\n";
        let live = "{\"a\":1}\n{\"b\":999}\n";
        assert_eq!(
            check_continuity(stored, live).unwrap(),
            ContinuityNote::Rewritten
        );
    }

    // ── View comparison ──

    /// A VIEW that is an ordered suffix of the log: the normal state.
    #[test]
    fn a_view_that_is_a_proper_suffix_passes() {
        let t = transcript::wrap_lines("{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n", SRC, SID);
        let v = transcript::wrap_lines("{\"b\":2}\n{\"c\":3}\n", SRC, SID);
        let d = repo_with(&[(meta::LOG_FILE, &t), (meta::VIEW_FILE, &v)]);
        assert_eq!(check_view(d.path()).unwrap(), Some(ViewNote::Ok));
    }

    /// Reordered while every reference stays reachable: this is **legal** (a VIEW after
    /// revert/cherry-pick is a surgical product, and no ordered suffix is required).
    #[test]
    fn a_reordered_view_with_reachable_refs_passes() {
        let t = transcript::wrap_lines("{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n", SRC, SID);
        let v = transcript::wrap_lines("{\"c\":3}\n{\"b\":2}\n", SRC, SID);
        let d = repo_with(&[(meta::LOG_FILE, &t), (meta::VIEW_FILE, &v)]);
        assert_eq!(check_view(d.path()).unwrap(), Some(ViewNote::Ok));
    }

    /// A VIEW referencing an event the log does not have → unreachable, self-consistency
    /// broken.
    #[test]
    fn an_unreachable_view_ref_is_an_error() {
        let t = transcript::wrap_lines("{\"a\":1}\n", SRC, SID);
        let v = transcript::wrap_lines("{\"a\":1}\n{\"b\":2}\n", SRC, SID);
        let d = repo_with(&[(meta::LOG_FILE, &t), (meta::VIEW_FILE, &v)]);
        assert_eq!(
            check_view(d.path()).unwrap(),
            Some(ViewNote::Unreachable { count: 1 })
        );
    }

    /// Merge markers must pair up and close.
    #[test]
    fn unbalanced_merge_markers_are_flagged() {
        use crate::commands::merge::marker_envelope;
        let base = transcript::wrap_lines("{\"a\":1}\n", SRC, SID);
        let start = marker_envelope("__merge_start__", SRC, SID, "b#1");
        let end = marker_envelope("__merge_end__", SRC, SID, "b#1");
        let t = format!("{base}{start}");
        let v = t.clone();
        let d = repo_with(&[(meta::LOG_FILE, &t), (meta::VIEW_FILE, &v)]);
        assert_eq!(
            check_view(d.path()).unwrap(),
            Some(ViewNote::UnbalancedMarkers { count: 1 })
        );
        // Paired up, it passes.
        let t2 = format!("{base}{start}{end}");
        let v2 = t2.clone();
        let d2 = repo_with(&[(meta::LOG_FILE, &t2), (meta::VIEW_FILE, &v2)]);
        assert_eq!(check_view(d2.path()).unwrap(), Some(ViewNote::Ok));
    }

    /// The VIEW is a required file: a log present with no VIEW is an error.
    #[test]
    fn a_missing_view_is_an_error_not_a_skip() {
        let t = transcript::wrap_lines("{\"a\":1}\n{\"b\":2}\n", SRC, SID);
        let d = repo_with(&[(meta::LOG_FILE, &t)]);
        assert_eq!(check_view(d.path()).unwrap(), Some(ViewNote::MissingView));
        // The counter-case is pinned too: a repo without even a log has no view to compare
        // against, which is not an error.
        let empty = repo_with(&[]);
        assert_eq!(check_view(empty.path()).unwrap(), None);
    }

    #[test]
    fn corrupt_storage_is_reported_instead_of_disappearing_from_doctor() {
        let valid = transcript::wrap_lines("{\"a\":1}\n", SRC, SID);
        let d = repo_with(&[
            (meta::LOG_FILE, "{not-an-envelope}\n"),
            (meta::VIEW_FILE, &valid),
        ]);
        let error = check_view(d.path()).unwrap_err();
        assert!(error.to_string().contains("LOG"), "{error:#}");
        assert!(super::committed_content("{not-an-envelope}\n").is_err());
    }

    // ── Old-layout detection (what the aggregate warning rests on) ──

    /// Both generations of old layout are recognized: a `sessions/` directory still present, or
    /// the session files spread across the repo root. There is no migration code, so recognizing
    /// them is the whole job — doctor reports, the user starts over.
    #[test]
    fn pre_session_dir_repos_are_detected_as_old_layout() {
        let with_sessions = repo_with(&[("sessions/codex/AB.jsonl", "{}\n")]);
        assert!(
            is_old_layout(with_sessions.path()),
            "a surviving sessions/ directory is the old layout"
        );

        let root_snapshot = repo_with(&[(
            "snapshot.json",
            &format!("{{\"session\":\"{SID}\",\"runtime\":\"codex\",\"cwd\":\"/r\"}}\n"),
        )]);
        assert!(
            is_old_layout(root_snapshot.path()),
            "a snapshot.json at the repo root is an old layout"
        );

        let root_transcript = repo_with(&[("transcript.jsonl", "{}\n")]);
        assert!(is_old_layout(root_transcript.path()));

        // Neither the current layout nor an empty repo with no commits is one.
        let new_layout = repo_with(&[(
            meta::FILE,
            &format!(
                "{{\"line\":\"session\",\"session\":\"{SID}\",\"runtime\":\"codex\",\"cwd\":\"/r\"}}\n"
            ),
        )]);
        assert!(!is_old_layout(new_layout.path()));
        let bare = repo_with(&[]);
        assert!(!is_old_layout(bare.path()));
    }
}
