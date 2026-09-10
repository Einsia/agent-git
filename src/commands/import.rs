//! `agit import` — adopt an existing session and record the version that opens its history.
//!
//! # One command, not two
//!
//! "put this session under version control" is one intent. Split across two commands (`import`
//! writes the link, `commit -n <name>` records the version), the state in between — a link with
//! no version — means nothing to the user; nobody wants to stop there. So the name is given here
//! and the version is recorded here:
//!
//! ```text
//! agit import <session-id> --from <runtime> --into <owner>/photo@<branch>
//! ```
//!
//! The version half calls [`super::commit::record_at`] directly — the same code path as
//! `agit commit`, so the two produce byte-identical snapshots.
//!
//! # By session id only, never a bulk import
//!
//! Of 18858 sessions on this machine, 18745 are the residue of automated batch runs. Importing
//! everything makes an agent's "memory" meaningless — it is the few stretches of work you picked,
//! not whatever happens to be on disk.
//!
//! # Finding candidates still does not open a transcript
//!
//! "an operation that scales with the number of sessions must not parse a transcript" has not
//! loosened (see the module docs of [`crate::domain::meta`]): resolving an id and listing the
//! candidates under this directory both go through the runtime index and the store links. What is
//! loose is only what happens **after the pick** — parsing that one session once is the cost
//! recording a version owes anyway, and it does not scale with how many sessions are on disk.
//!
//! # It still does not copy the session
//!
//! The store holds links only (see [`crate::domain::link`]), so after an import the original
//! session keeps growing and the link keeps pointing at it. The second copy of the content lives
//! in the repo, produced by the version-recording step.
//!
//! # `--link-only`
//!
//! Recording a version needs an account name (the `<owner>/` of the repo path and the commit's
//! user.name/email all come from the credentials), so the default path requires being signed in.
//! `--link-only` writes an unclaimed link offline. Recording its opening version requires another
//! `agit import` with an explicit repository and session branch; an unclaimed link cannot borrow
//! repository ownership from the current account. The shared file line cannot receive session turns.

use super::CmdResult;
use crate::domain::link::{self, Link};
use crate::domain::repo::{self, Repo};
use crate::domain::store::Store;
use crate::infra::config;
#[cfg(windows)]
use crate::ui::quote_powershell_argument as powershell_selection_arg;
use crate::{ExitCode, adapter, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

mod lineage;

#[derive(ClapArgs)]
pub struct Args {
    /// Session id or prefix; omitted: list candidates for an explicit interactive choice.
    #[arg(value_name = "session")]
    pub session: Option<String>,

    /// Adopt into this agent — the first version lands under this name
    #[arg(short = 'n', long = "name", value_name = "agent")]
    pub name: Option<String>,

    /// Which runtime to look in (default: all of them)
    #[arg(long, value_name = "runtime")]
    pub from: Option<String>,

    /// Link only, no version yet. Works offline; import again with an explicit target to record
    #[arg(long)]
    pub link_only: bool,

    /// Destination target: `owner/repo@branch` (legacy `--repo owner/repo -b branch` accepted).
    #[arg(long = "into", alias = "repo", value_name = "owner/repo@branch")]
    pub repo: Option<String>,

    /// Branch to claim onto (session line). Required for a fresh claim — sessions
    /// must never land on `main` (the file line) by accident.
    #[arg(short = 'b', long, value_name = "branch")]
    pub branch: Option<String>,

    /// Lineage: hang behind an existing commit (when the transcript extends its prefix).
    #[arg(long, value_name = "ref")]
    pub onto: Option<String>,

    /// Inspect verified local lineage candidates without adopting or changing the native session.
    #[arg(long, long_help = "Inspect verified local lineage candidates without adopting or changing the native session. Inspecting an existing repository requires Git with NUL-framed worktree output (normally Git 2.36 or newer); unsupported output is reported as git_worktree_format without falling back to ambiguous paths.", requires_all = ["session", "from", "repo"], conflicts_with_all = ["onto", "link_only", "privacy", "name", "independent"])]
    pub propose_lineage: bool,

    /// Explicitly import without selecting a prior session history as the base.
    #[arg(long, conflicts_with_all = ["onto", "link_only", "propose_lineage"])]
    pub independent: bool,

    /// Adopt a privacy-scrubbed COPY instead of the live transcript: secrets →
    /// [redacted:<rule>], home dir / username / hostname / public IPs get stable
    /// pseudonyms. The copy is a new session id and does not follow the original
    /// as it grows (re-run to refresh). claude-code only; other runtimes:
    /// `agit export <ref> --redact`.
    #[arg(long)]
    pub privacy: bool,
}

/// The session that was found. All three fields come from the runtime index; **no transcript is
/// opened**.
struct Found {
    runtime: &'static str,
    session_id: String,
    cwd: Option<String>,
}

/// The result of looking for a session.
enum Pick {
    One(Found),
    /// The reason is already printed; exit with this code.
    Explained(ExitCode),
}

pub fn run(args: Args) -> CmdResult {
    run_with_output(args, false)
}

/// Target selection precedes storage migration and adoption.
pub fn needs_readonly_startup(args: &Args) -> bool {
    args.propose_lineage || args.session.is_none() || !args.link_only
}

pub fn run_with_output(args: Args, json: bool) -> CmdResult {
    if args.propose_lineage {
        return lineage::preview(&args, json);
    }
    let deferred_startup = needs_readonly_startup(&args);
    if args.privacy && !args.link_only && args.onto.is_none() && !args.independent {
        ui::error(
            "--privacy requires an explicit --onto or --independent choice before creating a copy",
        );
        return Ok(ExitCode::Usage);
    }
    let mut args = args;
    // A bare import is a request to choose, not a request to guess. The TUI fills in the same
    // explicit session and destination arguments the command accepts, then leaves the alternate
    // screen before this function reaches any precondition or write below. Explicit lineage
    // decisions bypass this picker.
    if wants_tui(&args) {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                let cwd = std::env::current_dir()?;
                let Some(picked) = crate::tui::screens::adopt::pick(&cwd)? else {
                    return Ok(ExitCode::Ok);
                };
                args.session = Some(picked.session_id);
                args.from = Some(picked.runtime);
                args.link_only = picked.link_only;
                if let Some((slug, branch)) = picked.destination {
                    args.repo = Some(format!("{slug}@{branch}"));
                }
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }

    if args.session.is_none() {
        let store = Store::at(std::env::current_dir()?.join(config::store_root()?));
        match pick_here_with_preview(&store, &args, false)? {
            Pick::One(found) => {
                args.session = Some(found.session_id);
                args.from = Some(found.runtime.into());
            }
            Pick::Explained(code) => return Ok(code),
        }
    }

    let mut accepted = None;
    let mut same_claim = None;
    if !args.link_only && args.onto.is_none() && !args.independent {
        match lineage::choose(&mut args, json)? {
            lineage::Decision::Stop(code) => return Ok(code),
            lineage::Decision::SameClaim(selected) => same_claim = Some(*selected),
            lineage::Decision::Apply(selected) => accepted = Some(*selected),
        }
    }

    // ── 1. Ask the preconditions first, then touch the disk ──
    //
    // Recording a version needs an account name, and import records one by default. Finding out
    // that sign-in fails only after the link is written leaves the user with a half-made thing —
    // adopted but with no version — which is exactly the state this command must never leave
    // behind.
    let owner = if args.link_only {
        None
    } else {
        match super::commit::owner_for_recording(false)? {
            Some(o) => Some(o),
            None => {
                ui::hint(
                    "adopt without versioning (works offline): agit import <session-id> --link-only",
                );
                return Ok(ExitCode::Usage);
            }
        }
    };

    if let Some(n) = &args.name {
        repo::valid_name(n)?;
    }

    let store = match &accepted {
        Some(selected) => selected.store(),
        None => Store::at(config::store_root()?),
    };

    // ── 2. Find that session ──
    let picked = if let Some(selected) = &accepted {
        Pick::One(selected.found())
    } else {
        match &args.session {
            Some(sel) if sel == "@" => {
                ui::error(
                    "import requires a native session id; `@` does not infer the current runtime session.",
                );
                ui::hint(
                    "use `agit import <session-id> --from <runtime> --into <owner>/<repo>@<branch>`, or choose a session in the interactive import picker",
                );
                return Ok(ExitCode::Usage);
            }
            Some(sel) => by_selector(sel, args.from.as_deref())?,
            None => pick_here(&store, &args)?,
        }
    };
    let found = match picked {
        Pick::One(f) => f,
        Pick::Explained(code) => return Ok(code),
    };

    // ── 3. The name ──
    //
    // An already-adopted session reuses the agent it is managed under; otherwise the name must be
    // given explicitly. **Never guess**: a name chosen automatically silently decides which
    // lineage this memory lands on, and that kind of mistake is not noticed right away.
    let mut existing = if args.privacy {
        None
    } else if let Some(selected) = &accepted {
        selected.initial_link()
    } else {
        same_claim
            .clone()
            .or_else(|| link::get(&store, found.runtime, &found.session_id))
    };
    let destination = args
        .repo
        .as_deref()
        .map(crate::commands::target::parse)
        .transpose()?;
    let mut creation_notice = None;
    if let Some(dest) = &destination {
        let Some(repo) = dest.repo.as_deref() else {
            ui::error("import destination must name a repository: `<owner>/<repo>@<branch>`.");
            return Ok(ExitCode::Usage);
        };
        if !repo.contains('/') {
            ui::error(
                "import destination must use `<owner>/<repo>` (a bare repo name is not a destination).",
            );
            return Ok(ExitCode::Usage);
        }
        if dest.tail != crate::domain::refs::Tail::None {
            ui::error("import target accepts a branch, not a historic selector.");
            return Ok(ExitCode::Usage);
        }
        if dest.base.as_deref() == Some("@") {
            ui::error("import target must name a branch explicitly, or omit `@` and use `-b`.");
            return Ok(ExitCode::Usage);
        }
        if dest.base.is_some() && args.branch.is_some() {
            ui::error("a branch in `--into <owner/repo@branch>` cannot be combined with `-b`.");
            return Ok(ExitCode::Usage);
        }
        let (dest_owner, dest_name) = super::parse_slug(repo)?;
        if let Err(e) = super::canonical_owner(&dest_owner) {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
        let me = owner.as_deref().unwrap_or_default();
        // When the destination is not your own name, ask the hub's write-permission gate; being
        // unable to ask is an error, not "no permission".
        match super::writability(me, &dest_owner, &dest_name)? {
            super::Writability::Mine | super::Writability::Granted => {}
            super::Writability::Creatable => {
                creation_notice = Some(format!(
                    "  {repo} isn’t on the hub yet — the first push creates it under the {dest_owner} organization"
                ));
            }
            super::Writability::ReadOnly => {
                ui::error(&format!(
                    "cannot import into `{repo}`: the hub does not let you push to it."
                ));
                ui::hint(
                    "organization repos take pushes from the org owner and from team members granted on that repo — ask an org owner, or import into your own namespace",
                );
                return Ok(ExitCode::Policy);
            }
            super::Writability::Missing => {
                ui::error(&format!(
                    "cannot import into `{repo}`: the hub has no such repo that you can see, and only an owner of `{dest_owner}` could create one there."
                ));
                ui::hint(
                    "check the name with `agit repo list --remote`, or ask an org owner to create it",
                );
                return Ok(ExitCode::Ref);
            }
        }
    }
    if !args.link_only
        && existing
            .as_ref()
            .is_some_and(|link| link.branch.is_some() && recorded_owner(link).is_none())
        && destination
            .as_ref()
            .is_none_or(|target| target.base.is_none() && args.branch.is_none())
    {
        ui::error("this session claim has no recorded owner; its namespace cannot be inferred.");
        ui::hint(
            "re-adopt with an explicit `--into <owner>/<repo>@<branch>` and confirm that destination",
        );
        return Ok(ExitCode::Usage);
    }
    let agent = destination
        .as_ref()
        .and_then(|t| t.repo.as_deref())
        .and_then(|r| super::parse_slug(r).ok().map(|(_, n)| n))
        .or_else(|| args.name.clone())
        .or_else(|| existing.as_ref().and_then(|l| l.agent.clone()));

    if agent.is_none() && !args.link_only {
        ui::error("versioning needs a destination agent named first.");
        ui::hint(&format!(
            "agit import {} --from {} --into <owner>/{}@<branch>",
            ui::session::shell_arg(&found.session_id),
            found.runtime,
            suggested_name(found.cwd.as_deref())
        ));
        ui::hint("adopt without versioning (works offline): add --link-only");
        return Ok(ExitCode::Usage);
    }

    let prepared = if args.link_only {
        None
    } else {
        let author = owner.as_deref().unwrap();
        let namespace = destination
            .as_ref()
            .and_then(|target| target.repo.as_deref())
            .and_then(|slug| super::parse_slug(slug).ok().map(|(owner, _)| owner))
            .unwrap_or_else(|| author.to_owned());
        let selected_link = existing
            .clone()
            .unwrap_or_else(|| Link::new(found.runtime, &found.session_id, None));
        match prepare_target(
            &selected_link,
            agent.as_deref().unwrap(),
            &namespace,
            &args,
            destination.as_ref(),
            accepted.as_ref(),
        )? {
            TargetSelection::Ready(target) => Some(target),
            TargetSelection::Refused(code) => return Ok(code),
        }
    };

    if deferred_startup && accepted.is_none() {
        if let Err(error) = super::migration::migrate_startup() {
            ui::error(&format!("local storage preparation failed: {error:#}"));
            return Ok(ExitCode::Precondition);
        }
        if let Some(before) = same_claim.as_ref() {
            let store = Store::at(std::env::current_dir()?.join(config::store_root()?));
            let current = link::get(&store, &before.source, &before.session_id);
            if !current.as_ref().is_some_and(|current| {
                current.is_active()
                    && current.owner == before.owner
                    && current.agent == before.agent
                    && current.branch == before.branch
            }) {
                ui::error("the session claim changed before its existing import could continue");
                return Ok(ExitCode::Policy);
            }
            existing = current;
        }
    }

    if let Some(target) = &prepared
        && accepted.is_none()
    {
        target.verify()?;
    }

    // ── --privacy: swap in a redacted copy; no byte of the original enters history ──
    let found = if args.privacy {
        match scrub_copy(&found)? {
            Some(f) => f,
            None => return Ok(ExitCode::Usage),
        }
    } else {
        found
    };

    if let Some(target) = &prepared
        && accepted.is_none()
    {
        target.verify()?;
    }

    if let Some(notice) = creation_notice {
        println!("{}", ui::dim(&notice));
    }

    // ── 4. Adoption: write the link ──
    let lk = match &accepted {
        Some(selected) => selected.link(),
        None => attach(&store, &found, existing)?,
    };

    if args.link_only {
        println!(
            "\n{}",
            ui::dim(&format!(
                "  `agit import {} --from {} --into <owner/repo>@<branch>` chooses lineage and records the first version after sign-in",
                ui::session::shell_arg(&lk.session_id),
                ui::session::shell_arg(&lk.source)
            ))
        );
        return Ok(ExitCode::Ok);
    }

    // ── 5. Record the opening version (turn-by-turn settlement, the `agit commit` path) ──
    let agent = agent.unwrap();
    let owner = owner.unwrap();
    let prepared = prepared.unwrap();
    let namespace = prepared.namespace.clone();
    let mut lk = lk;
    let landing = match place_on_branch(
        &mut lk,
        &store,
        &agent,
        &owner,
        &prepared,
        accepted.as_ref(),
    )? {
        Placed::Ready(l) => *l,
        Placed::Refused(code) => return Ok(code),
    };
    println!();
    // Settlement that did not succeed = this import did not happen: put the ref that was created
    // and the checkout that was switched back the way they were.
    let outcome =
        super::commit::record_at(&store, lk, &agent, &namespace, &owner, landing.repo_dir());
    if !matches!(outcome, Ok(ExitCode::Ok)) {
        landing.rollback();
    }
    outcome
}

/// Only the actual zero-argument form opens the picker. A flag changes the operation and must not
/// disappear into a screen that cannot represent it.
fn wants_tui(args: &Args) -> bool {
    args.session.is_none()
        && args.name.is_none()
        && args.from.is_none()
        && !args.link_only
        && args.repo.is_none()
        && args.branch.is_none()
        && args.onto.is_none()
        && !args.propose_lineage
        && !args.independent
        && !args.privacy
}

/// Where this import lands, and how to put it back on failure.
pub(super) struct Landing {
    repo_dir: PathBuf,
    branch: String,
    /// This import created the branch (a failure deletes it).
    created: bool,
    /// The commit it pointed at when created: deleting the ref uses it as the expected OID, and a
    /// branch someone else advanced is left alone.
    created_oid: Option<String>,
    /// Where HEAD pointed before the branch was created (a failure switches back).
    prev_checkout: Option<String>,
    /// The link as it sat on disk before the claim was rerouted (a failure writes it back —
    /// restoring the ref without the link lets one refused import lose the previous destination
    /// and the materialization baseline for good). None when there was no previous link: the new
    /// link then points at a rolled-back branch, and the next command's context resolution
    /// refuses it on its own.
    store: Store,
    prev_link: Option<Link>,
    /// The bytes of the link file after this claim was persisted. Restoring is a CAS: `prev_link`
    /// goes back only while the disk still equals them byte for byte — between the snapshot and
    /// the rollback a concurrent settlement (the Stop hook) may have advanced the watermark, and
    /// an unconditional write back would rewind it to the old snapshot.
    claim_source: String,
    claim_session: String,
    claimed_path: PathBuf,
    claimed_bytes: Vec<u8>,
    /// A selected application restores the complete original image, including its absence.
    previous_image: Option<Option<Vec<u8>>>,
}

impl Landing {
    pub(super) fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }

    pub(super) fn branch(&self) -> &str {
        &self.branch
    }

    /// Put the ref and the checkout back the way they were before the import.
    ///
    /// Without this, an import refused by "already claimed" leaves a branch ref pointing at
    /// **someone else's commit** and moves the repo checkout onto it; the `agit push` that
    /// follows (which only knows `current_branch`) then publishes that ghost branch to the hub.
    /// So "did not succeed" must mean "nothing happened" — switch back first, then delete the
    /// ref; in the other order git refuses to delete the current branch.
    pub(super) fn rollback(&self) {
        let Some(repo) = Repo::open(&self.repo_dir) else {
            return;
        };
        if let Some(prev) = &self.prev_checkout
            && repo.current_branch().as_deref() != Some(prev.as_str())
        {
            let target = format!("refs/heads/{prev}");
            if let Err(error) = super::plumbing::ensure_safe_checkout(&repo, &target)
                .and_then(|()| repo.switch(prev))
            {
                ui::warning(&format!(
                    "could not restore the previous checkout `{prev}` safely: {error:#}"
                ));
                return;
            }
        }
        if let Some(previous) = &self.previous_image {
            match link::lock(&self.store, &self.claim_source, &self.claim_session) {
                Ok(_guard) => {
                    if std::fs::read(&self.claimed_path)
                        .is_ok_and(|current| current == self.claimed_bytes)
                    {
                        let restored = match previous {
                            Some(bytes) => std::fs::write(&self.claimed_path, bytes),
                            None => std::fs::remove_file(&self.claimed_path),
                        };
                        if let Err(error) = restored {
                            ui::warning(&format!(
                                "could not restore the previous import claim: {error}"
                            ));
                        }
                    } else {
                        ui::warning(
                            "the session claim advanced after this import; preserving its current state",
                        );
                    }
                }
                Err(error) => ui::warning(&format!(
                    "could not lock the import claim for rollback: {error}"
                )),
            }
        } else if let Some(prev) = &self.prev_link {
            // The same lock as the claim: no other write may slip in between the comparison
            // and the restore. When the lock cannot be taken, warn and restore nothing — a link
            // left pointing at the new destination is better than a blind write.
            match link::lock(&self.store, &prev.source, &prev.session_id) {
                Err(_) => ui::warning(
                    "could not lock the session link for rollback — leaving it in place",
                ),
                Ok(_guard) => {
                    let untouched = std::fs::read(&self.claimed_path)
                        .is_ok_and(|now| now == self.claimed_bytes);
                    if !untouched {
                        ui::warning(
                            "the session link moved while this import was rolling back — leaving it in place",
                        );
                    } else if let Err(error) = link::write(&self.store, prev) {
                        ui::warning(&format!(
                            "could not restore the session link to its previous claim: {error:#}"
                        ));
                    }
                }
            }
        }
        if self.created {
            let head_ref = format!("refs/heads/{}", self.branch);
            // Deleting the ref carries an expected OID: a branch someone else advanced is not
            // deleted; that history is theirs now. The OID is produced together with `created`
            // (a resolve failure after the branch is created fails the import before it lands),
            // so a missing one here can only be a construction error, and it is not deleted
            // either.
            let Some(oid) = &self.created_oid else {
                ui::warning(&format!(
                    "no expected OID for branch `{}` — leaving it in place",
                    self.branch
                ));
                return;
            };
            match repo.git(&["update-ref", "-d", &head_ref, oid]) {
                Ok(_) => println!(
                    "{}",
                    ui::dim(&format!(
                        "  rolled back: branch `{}` was not created after all",
                        self.branch
                    ))
                ),
                Err(_) => ui::warning(&format!(
                    "branch `{}` moved since this import created it — leaving it in place",
                    self.branch
                )),
            }
        }
    }
}

/// The result of [`place_on_branch`].
pub(super) enum Placed {
    Ready(Box<Landing>),
    /// The reason is already printed; exit with this code.
    Refused(ExitCode),
}

/// A legacy destination is selected without creating a link, a privacy copy or a repository.
struct PreparedTarget {
    namespace: String,
    repo_dir: PathBuf,
    branch: String,
    onto_commit: Option<String>,
    repository: Option<(lineage::PathIdentity, lineage::PathIdentity)>,
}

impl PreparedTarget {
    fn verify(&self) -> crate::Result<()> {
        lineage::verify_git_routing()?;
        if let Some((root, common)) = &self.repository {
            root.verify()?;
            common.verify()?;
            let repo = Repo::open(&self.repo_dir)
                .ok_or_else(|| anyhow::anyhow!("the selected import repository disappeared"))?
                .local_objects_only();
            let common_dir = repo.common_dir_with_policy(repo::ReadPolicy::LocalOnly)?;
            common.verify_at(&common_dir)?;
            match std::fs::metadata(common_dir.join("info/grafts")) {
                Ok(metadata) => anyhow::ensure!(
                    metadata.len() == 0,
                    "remove Git graft overlays before applying an import target"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let (status, toplevel, _) = repo.git_status_local(&["rev-parse", "--show-toplevel"])?;
            anyhow::ensure!(
                status == Some(0),
                "the selected import checkout is unreadable"
            );
            root.verify_at(Path::new(&toplevel))?;
            if let Some(oid) = &self.onto_commit {
                let (status, current, _) = repo.git_status_local(&[
                    "rev-parse",
                    "--verify",
                    &format!("{oid}^{{commit}}"),
                ])?;
                anyhow::ensure!(
                    status == Some(0) && current == *oid,
                    "the selected import base is no longer a readable commit"
                );
            }
        } else {
            anyhow::ensure!(
                std::fs::symlink_metadata(&self.repo_dir)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "the import destination appeared after it was selected"
            );
        }
        Ok(())
    }
}

enum TargetSelection {
    Ready(Box<PreparedTarget>),
    Refused(ExitCode),
}

fn prepare_target(
    lk: &Link,
    agent: &str,
    owner: &str,
    args: &Args,
    destination: Option<&crate::commands::target::Target>,
    accepted: Option<&lineage::Accepted>,
) -> crate::Result<TargetSelection> {
    let (repo_dir, namespace) = if let Some(selected) = accepted {
        (selected.repo_dir().to_owned(), owner.to_owned())
    } else if destination.is_some() {
        (config::repo_dir(owner, agent)?, owner.to_owned())
    } else {
        match super::clone::checkouts_named(owner, agent)?.as_slice() {
            [] => (config::repo_dir(owner, agent)?, owner.to_owned()),
            [only] => (only.path.clone(), only.owner.clone()),
            many => {
                let candidates = many
                    .iter()
                    .map(|checkout| format!("  {}", checkout.slug()))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(crate::domain::refs::Ambiguous(format!(
                    "`{agent}` names multiple local repos — choose --into <owner/repo@branch>:\n{candidates}"
                )).into());
            }
        }
    };
    let repo = Repo::at(&repo_dir).local_objects_only();
    let repository = if accepted.is_some() {
        None
    } else {
        lineage::verify_git_routing()?;
        if Repo::open(&repo_dir).is_some() {
            Some((
                lineage::PathIdentity::capture(&repo_dir, true)?,
                lineage::PathIdentity::capture(
                    &repo.common_dir_with_policy(repo::ReadPolicy::LocalOnly)?,
                    true,
                )?,
            ))
        } else {
            anyhow::ensure!(
                std::fs::symlink_metadata(&repo_dir)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "the destination is not an absent or readable repository"
            );
            None
        }
    };
    let onto_commit = if accepted.is_some() {
        args.onto.clone()
    } else if let Some(onto) = &args.onto {
        use crate::domain::refs::{Base, RepoSel, Tail};
        let mut spec = crate::domain::refs::parse(onto)?;
        if matches!(
            spec.tail,
            Tail::Event { .. } | Tail::Range { .. } | Tail::Path(_)
        ) {
            ui::error("--onto requires a whole commit, not an event, turn range, or file path");
            return Ok(TargetSelection::Refused(ExitCode::Usage));
        }
        let slug = format!("{namespace}/{agent}");
        let same_repo = match &spec.repo {
            RepoSel::Context => true,
            RepoSel::Slug(owner, name) => owner == &namespace && name == agent,
            RepoSel::Local(name) if name == agent => {
                match super::clone::checkouts_named(&namespace, name)?.as_slice() {
                    [only] => only.owner == namespace && only.path == repo_dir,
                    [] => false,
                    many => {
                        let candidates = many
                            .iter()
                            .map(|checkout| format!("  {}", checkout.slug()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Err(crate::domain::refs::Ambiguous(format!(
                            "--onto `{onto}` names multiple local repos — qualify it as {slug}@<ref>:\n{candidates}"
                        )).into());
                    }
                }
            }
            RepoSel::Local(_) => false,
        };
        if !same_repo {
            ui::error(&format!(
                "--onto `{onto}` must refer to the selected destination repository `{slug}`"
            ));
            return Ok(TargetSelection::Refused(ExitCode::Usage));
        }
        if spec.base == Base::At {
            let context = match super::context::at_context() {
                Ok(context) => context,
                Err(error) => {
                    ui::error(&format!("--onto `{onto}` failed to resolve: {error:#}"));
                    return Ok(TargetSelection::Refused(ExitCode::Ref));
                }
            };
            if context.repo != slug {
                ui::error(&format!(
                    "--onto `{onto}` selects session repository `{}`, not the destination `{slug}`",
                    context.repo
                ));
                return Ok(TargetSelection::Refused(ExitCode::Usage));
            }
            spec.base = Base::SessionBranch(context.branch);
        }
        if repository.is_none() {
            ui::error(&format!(
                "--onto `{onto}` requires an existing destination repository"
            ));
            return Ok(TargetSelection::Refused(ExitCode::Ref));
        }
        match crate::domain::refs::resolve(&repo, &spec) {
            Ok(resolved) => Some(resolved.sha),
            Err(error) if error.is::<crate::domain::refs::Ambiguous>() => {
                return Err(error.context(format!("cannot select --onto `{onto}`")));
            }
            Err(error) => {
                ui::error(&format!("--onto `{onto}` failed to resolve: {error:#}"));
                return Ok(TargetSelection::Refused(ExitCode::Ref));
            }
        }
    } else {
        None
    };
    let cur = if accepted.is_some() {
        None
    } else {
        repo.current_branch()
    };
    let cur_is_session = cur
        .as_deref()
        .and_then(|branch| crate::domain::meta::read_at_ref(&repo, &format!("refs/heads/{branch}")))
        .is_some_and(|meta| meta.is_session_line());
    let target_branch = destination.and_then(|target| match target.base.as_deref() {
        Some("@") | None => None,
        Some(branch) => Some(branch.to_owned()),
    });
    let branch = match target_branch.as_ref().or(args.branch.as_ref()) {
        Some(branch) => branch.clone(),
        None if onto_commit.is_none() && cur_is_session => cur.unwrap(),
        None => {
            let suggested = format!("{}-{}", agent, link::short(&lk.session_id));
            ui::error("claiming a fresh session line needs -b <branch>.");
            ui::hint(&format!(
                "e.g. `agit import {} --from {} --into {namespace}/{agent}@{suggested}`",
                ui::session::shell_arg(&lk.session_id),
                ui::session::shell_arg(&lk.source)
            ));
            return Ok(TargetSelection::Refused(ExitCode::Usage));
        }
    };
    // Raw ref syntax and branch-only rules must pass before any local writes; raw validation
    // also prevents branch shorthand from being expanded against the caller's checkout.
    let validator = Repo::at(std::env::current_dir()?);
    let (status, _, _) =
        validator.git_status_local(&["check-ref-format", &format!("refs/heads/{branch}")])?;
    if status != Some(0) {
        ui::error("the destination session branch is not a valid Git ref");
        return Ok(TargetSelection::Refused(ExitCode::Usage));
    }
    let (status, _, _) = validator.git_status_local(&["check-ref-format", "--branch", &branch])?;
    if status != Some(0) {
        ui::error("the destination session branch is not a valid Git ref for a branch");
        return Ok(TargetSelection::Refused(ExitCode::Usage));
    }
    if (accepted.is_some() || !repo.has_ref(&format!("refs/heads/{branch}")))
        && let Err(error) = repo::valid_branch_name(&branch)
    {
        ui::error(&format!("{error:#}"));
        return Ok(TargetSelection::Refused(ExitCode::Usage));
    }
    let selected = PreparedTarget {
        namespace,
        repo_dir,
        branch,
        onto_commit,
        repository,
    };
    if accepted.is_none() {
        selected.verify()?;
    }
    Ok(TargetSelection::Ready(Box::new(selected)))
}

/// Placement consumes the selected identity; mutable names are not resolved again after adoption.
fn place_on_branch(
    lk: &mut Link,
    store: &Store,
    agent: &str,
    author: &str,
    selected: &PreparedTarget,
    accepted: Option<&lineage::Accepted>,
) -> crate::Result<Placed> {
    place_resolved_branch(
        lk,
        store,
        agent,
        &selected.namespace,
        author,
        selected.repo_dir.clone(),
        Repo::at(&selected.repo_dir),
        selected.branch.clone(),
        selected.onto_commit.clone(),
        accepted,
        Some(selected),
    )
}

/// Prepare the branch selected by the legacy `commit <session-id> -n <name> -b <branch>` form.
///
/// The repo path is already resolved by `commit`, preserving its legacy checkout-selection and
/// namespace rules. Branch creation and link claiming then use the same path as normal import.
pub(super) fn place_legacy_commit_branch(
    lk: &mut Link,
    store: &Store,
    agent: &str,
    owner: &str,
    author: &str,
    repo_dir: &Path,
    branch: String,
) -> crate::Result<Placed> {
    if branch == "main" {
        ui::error("cannot settle session turns onto `main` — it is the shared file line");
        ui::hint("choose a session branch with `-b <branch>`; sessions must never land on main");
        return Ok(Placed::Refused(ExitCode::Precondition));
    }
    let repo = Repo::open_or_init(repo_dir)?;
    place_resolved_branch(
        lk,
        store,
        agent,
        owner,
        author,
        repo_dir.to_path_buf(),
        repo,
        branch,
        None,
        None,
        None,
    )
}

/// Validate, claim, and create a selected session branch.
#[allow(clippy::too_many_arguments)]
fn place_resolved_branch(
    lk: &mut Link,
    store: &Store,
    agent: &str,
    owner: &str,
    author: &str,
    repo_dir: PathBuf,
    repo: Repo,
    branch: String,
    onto_commit: Option<String>,
    accepted: Option<&lineage::Accepted>,
    prepared: Option<&PreparedTarget>,
) -> crate::Result<Placed> {
    // Only the name of a branch about to be **created** goes through the prefix check: an
    // existing branch is a fact on the ground, and stopping it only leaves a line that already
    // exists unable to settle from then on.
    if (accepted.is_some() || !repo.has_ref(&format!("refs/heads/{branch}")))
        && let Err(e) = repo::valid_branch_name(&branch)
    {
        ui::error(&format!("{e:#}"));
        return Ok(Placed::Refused(ExitCode::Usage));
    }

    // The same session already hangs on another branch: changing the branch reroutes every
    // settlement from here on, which is not something a re-run of import does silently. Ask; when
    // asking is impossible (no tty and no `-y`), refuse.
    if let Some(prev) = claimed_elsewhere(lk, owner, agent, &branch) {
        let next = format!("{owner}/{agent}@{branch}");
        ui::warning(&format!(
            "session {} is already claimed on {prev}; importing into {next} would re-route its future settlements",
            link::short(&lk.session_id)
        ));
        if std::env::var_os("AGIT_YES").is_none() {
            match ui::prompt::confirm(&format!("re-claim it from `{prev}` onto `{next}`?"), false)?
            {
                Some(true) => {}
                Some(false) => {
                    println!("cancelled.");
                    return Ok(Placed::Refused(ExitCode::Policy));
                }
                None => {
                    ui::error("refusing to move the claim without confirmation");
                    ui::hint(&format!(
                        "inspect the recorded identity, then pass `-y` only if you intend to claim `{next}`"
                    ));
                    return Ok(Placed::Refused(ExitCode::Interactive));
                }
            }
        }
    }

    birth_session_branch(
        lk,
        store,
        agent,
        owner,
        author,
        repo_dir,
        repo,
        branch,
        onto_commit,
        accepted,
        prepared,
    )
}

/// Create and claim a session branch after the destination and reroute checks have passed.
///
/// Every entry point uses this path so a fresh repository always gets its `main` file line before
/// the session branch is born from it.
#[allow(clippy::too_many_arguments)]
fn birth_session_branch(
    lk: &mut Link,
    store: &Store,
    agent: &str,
    owner: &str,
    author: &str,
    repo_dir: PathBuf,
    repo: Repo,
    branch: String,
    onto_commit: Option<String>,
    accepted: Option<&lineage::Accepted>,
    prepared: Option<&PreparedTarget>,
) -> crate::Result<Placed> {
    // Import and materialization both create active branch claims. Serialize their branch/ref and
    // link updates under the same key so a concurrent `run --no-launch` cannot observe an empty
    // destination and install a second writer while this claim is being placed.
    let _branch_guard = link::lock_branch(store, &format!("{owner}/{agent}"), &branch)?;
    let _claim_guard = link::lock(store, &lk.source, &lk.session_id)?;
    if let Some(selected) = accepted
        && let Err(error) =
            selected.verify(store, &repo, owner, agent, &branch, onto_commit.as_deref())
    {
        ui::error(&format!(
            "the import choice is stale: {error:#}; inspect and choose again"
        ));
        return Ok(Placed::Refused(ExitCode::Policy));
    }
    if accepted.is_none()
        && let Some(selected) = prepared
        && let Err(error) = selected.verify()
    {
        ui::error(&format!(
            "the import target changed before placement: {error:#}"
        ));
        return Ok(Placed::Refused(ExitCode::Policy));
    }
    let prev_link = if let Some(selected) = accepted {
        selected.initial_link()
    } else {
        link::get(store, &lk.source, &lk.session_id)
    };
    if accepted.is_none() {
        let Some(current) = prev_link.as_ref() else {
            ui::error(
                "the attached session link is missing or unreadable; inspect its metadata before retrying the import",
            );
            return Ok(Placed::Refused(ExitCode::Policy));
        };
        if current.owner != lk.owner || current.agent != lk.agent || current.branch != lk.branch {
            ui::error("the session claim changed while import was waiting for its branch lock");
            ui::hint("inspect the current destination with `agit status`, then retry the import");
            return Ok(Placed::Refused(ExitCode::Policy));
        }
        let discovered_cwd = lk.cwd.clone();
        *lk = current.clone();
        if lk.cwd.is_none() {
            lk.cwd = discovered_cwd;
        }
    }
    let repo = if accepted.is_some() || prepared.is_some() {
        Repo::open_or_init(&repo_dir)?
    } else {
        repo
    };

    if !lk.is_active() && repo.has_ref(&format!("refs/heads/{branch}")) {
        ui::error(&format!(
            "session {} was superseded by {} and cannot reclaim existing branch {owner}/{agent}@{branch}.",
            link::short(&lk.session_id),
            lk.superseded_by
                .as_deref()
                .unwrap_or("a newer runtime session")
        ));
        ui::hint(&format!(
            "preserve its later work with `agit import {} --from {} --into {owner}/{agent}@<new-branch>`",
            ui::session::shell_arg(&lk.session_id),
            ui::session::shell_arg(&lk.source)
        ));
        return Ok(Placed::Refused(ExitCode::Policy));
    }

    if let Some(base) = onto_commit.as_deref()
        && repo.has_ref(&format!("refs/heads/{branch}"))
        && !existing_onto_is_lineage(&repo, &branch, base)?
    {
        ui::error(&format!(
            "--onto {base} is not on the first-parent history of existing branch {owner}/{agent}@{branch}."
        ));
        ui::hint(
            "choose a new destination branch to attach at that commit; the existing branch is unchanged",
        );
        return Ok(Placed::Refused(ExitCode::Policy));
    }

    // A repo with no `main` collapses the whole chain, at every link: a server-side bare repo's
    // HEAD dangles at a non-existent `refs/heads/main` → `clone` checks out no local branch and
    // warns that the remote HEAD points at a ref that does not exist → `resume` reports no branch
    // and `run` mistakes a writable branch head for a historic commit and forks by force →
    // `push` cannot read the workspace meta. `main` is also where shared memory/skills live, and
    // only a session branch grown off its head can snapshot those shared files into its tree at
    // creation.
    if repo.commit_count() == 0 {
        create_main_file_line(&repo, author, lk)?;
    }

    let prev_checkout = repo.current_branch();
    let head_ref = format!("refs/heads/{branch}");
    let mut created = false;
    let mut created_oid: Option<String> = None;
    if !repo.has_ref(&head_ref) {
        let frozen = |base: &str| -> crate::Result<String> {
            let oid = repo
                .git(&["rev-parse", "--verify", &format!("{base}^{{commit}}")])?
                .trim()
                .to_string();
            super::migration::migrate_frozen_tip(&repo, &oid)
        };
        match &onto_commit {
            Some(base) => {
                let oid = frozen(base)?;
                repo.git(&["branch", &branch, &oid])?;
                println!(
                    "{}",
                    ui::dim(&format!(
                        "  lineage: {branch} hangs after {}",
                        &oid[..9.min(oid.len())]
                    ))
                );
                created_oid = Some(oid);
            }
            None => {
                if let Some(base) = birth_base(&repo) {
                    let oid = frozen(&base)?;
                    repo.git(&["branch", &branch, &oid])?;
                    created_oid = Some(oid);
                }
            }
        }
        created = repo.has_ref(&head_ref);
        if let Some(published) = declare_session_line(&repo, &branch, lk)? {
            created_oid = Some(published);
        }
    }
    if !created {
        created_oid = None;
    }
    // The rerouted claim is about to be persisted; the failure path must be able to put the link
    // back (a rollback that restores only the ref and the checkout loses the baseline and the
    // destination for good). Snapshot, claim, and read-back of the expected bytes all happen
    // under one link lock (see `link::lock`): a watermark advance from the Stop hook cannot slip
    // in between, so the bytes read back are necessarily the ones this claim wrote.
    // The materialization baseline asserts "this prefix is already history **on the line it was
    // materialized onto**". Only two cases keep it: a re-run onto the same destination (with the
    // first turn unsettled, settlement is legitimately a no-op, and clearing it falls back to the
    // native continuity comparison, whose materialized content carries recast ids that are never
    // a prefix of the LOG); and a branch this import creates with an explicit `--onto` (it really
    // does carry the history the baseline covers). Every other reroute drops it: rerouted onto a
    // branch with no history, keeping it makes the settlement region the empty string forever and
    // the whole history silently settles as zero turns; rerouted onto a non-empty branch someone
    // else has claimed, keeping it makes materialized settlement look only at the tail after the
    // baseline and bypass the native continuity check, so later turns are written into someone
    // else's history. Once dropped, the continuity / claim checks refuse the combinations that
    // cannot be written.
    //
    // A materialization baseline belongs to a recorded namespace. Missing ownership cannot
    // prove that the destination carries its history, even when the repo and branch names match.
    let rerouted = lk.branch.as_deref() != Some(branch.as_str())
        || lk.agent.as_deref() != Some(agent)
        || recorded_owner(lk) != Some(owner);
    if rerouted && !(created && onto_commit.is_some()) {
        lk.baseline_bytes = None;
        lk.baseline_hash = None;
        lk.materialized_from = None;
    }
    if rerouted {
        // A superseded transcript can be recovered only onto another line. The new claim is
        // active there; retaining its old successor would make the recovery impossible to settle.
        lk.superseded_by = None;
    }
    persist_branch_claim(store, lk, owner, agent, &branch)?;
    let claimed_path = link::link_path(store, &lk.source, &lk.session_id);
    let claimed_bytes = std::fs::read(&claimed_path).unwrap_or_default();
    Ok(Placed::Ready(Box::new(Landing {
        repo_dir,
        branch,
        created,
        created_oid,
        prev_checkout,
        store: store.clone(),
        prev_link,
        claim_source: lk.source.clone(),
        claim_session: lk.session_id.clone(),
        claimed_path,
        claimed_bytes,
        previous_image: accepted.map(|selected| selected.previous_image()),
    })))
}

/// Repeating an attachment may reuse its descendant, but a merged side branch is not its base.
fn existing_onto_is_lineage(repo: &Repo, branch: &str, base: &str) -> crate::Result<bool> {
    let mut found = false;
    let base = base.trim().as_bytes();
    repo.git_stream_split(
        &[
            "rev-list",
            "--first-parent",
            &format!("refs/heads/{branch}"),
            "--",
        ],
        b'\n',
        |oid| {
            found |= oid == base;
            Ok(())
        },
    )?;
    Ok(found)
}

fn recorded_owner(lk: &Link) -> Option<&str> {
    lk.owner.as_deref().filter(|owner| !owner.is_empty())
}

/// A recorded branch claim may be reused without confirmation only at its complete identity.
/// Unknown ownership is displayed as unknown rather than borrowed from the current account.
fn claimed_elsewhere(lk: &Link, owner: &str, agent: &str, branch: &str) -> Option<String> {
    let prev_branch = lk.branch.as_deref()?;
    let same = recorded_owner(lk) == Some(owner)
        && lk.agent.as_deref() == Some(agent)
        && prev_branch == branch;
    (!same).then(|| {
        format!(
            "{}/{}@{prev_branch}",
            recorded_owner(lk).unwrap_or("<unknown-owner>"),
            lk.agent.as_deref().unwrap_or("<unknown-repo>")
        )
    })
}

/// Persist the routing fields as soon as import claims a branch.
///
/// The first turn may still be in flight, so settlement can legitimately be a
/// no-op. The next `agit commit` must still be able to find this link by agent
/// and branch after that turn finishes.
fn persist_branch_claim(
    store: &Store,
    lk: &mut Link,
    owner: &str,
    agent: &str,
    branch: &str,
) -> crate::Result<()> {
    lk.agent = Some(agent.to_string());
    lk.owner = Some(owner.to_string());
    lk.branch = Some(branch.to_string());
    link::write(store, lk)?;
    Ok(())
}

/// Where a new session branch grows from.
///
/// **The `main` file line wins.** Growing off "the repo's current checkout" is the back half of
/// the A2 chain: the checkout may be parked on someone else's session branch, so the new branch
/// inherits that other session's transcript byte for byte and the first settlement hits "already
/// claimed by another session" — that sentence compares against the repo's current branch instead
/// of the target branch, and a repo then holds one session and no more.
///
/// A legacy repo with no `main` can only grow off the current head: shared files are still
/// inherited, and [`declare_session_line`] clears the session body that came along with them.
pub(super) fn birth_base(repo: &Repo) -> Option<String> {
    if repo.has_ref("refs/heads/main") {
        return Some("main".into());
    }
    (repo.commit_count() > 0).then(|| "HEAD".to_string())
}

/// The `main` file line of a repo created here: the scaffold plus the current project's memory /
/// skills assets.
///
/// Asset discovery and the confirmation discipline both reuse what `init --seed` does (confirm
/// item by item, take nothing when non-interactive) — personal memory can hold private things,
/// and import is not a back route around that gate.
pub(super) fn create_main_file_line(repo: &Repo, owner: &str, lk: &Link) -> crate::Result<()> {
    // An old git's init fallback can leave HEAD under another name; the file line is only ever
    // called `main`.
    if repo.current_branch().as_deref() != Some("main") {
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
    }
    println!(
        "{}",
        ui::dim("  creating the main file line (shared memory / skills live there)")
    );
    super::init::scaffold(repo.root())?;
    let project = lk
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    if let Some(p) = &project {
        super::init::seed_into(repo.root(), p)?;
    }
    // The author fields come from the credentials, the same as on the settlement path.
    repo.git(&["config", "user.name", owner])?;
    let email =
        crate::infra::credentials::current_email().unwrap_or_else(|| format!("{owner}@agit.local"));
    repo.git(&["config", "user.email", &email])?;
    repo.add_all()?;
    repo.commit("agit: init (main file line)")?;
    Ok(())
}

/// Make a freshly created branch **declare** that it is a session line.
///
/// A new branch usually grows off the head of `main` (the file line), so it inherits that
/// `session/meta.json` byte for byte — the one that says `line: file`. Left as is, the first
/// settlement is refused as "stuffing a conversation onto the file line" with exit code 4: the W1
/// deadlock.
///
/// When the starting point is already a session line, **do nothing**: that is the lineage
/// inheritance `--onto` means (identity is inherited, not claimed again).
///
/// The identity is still empty at this moment — `session_hash` needs transcript bytes, and those
/// only exist once the first turn settles. The shape lands first and the identity is claimed
/// after; the two happen at different moments by construction. Returns the tip OID it actually
/// published (None when no ref moved) — the expected OID for the rollback's ref deletion must
/// come from the publisher itself, not from sampling again afterwards.
pub(super) fn declare_session_line(
    repo: &Repo,
    branch: &str,
    lk: &Link,
) -> crate::Result<Option<String>> {
    use crate::domain::meta::{self, Meta};
    let head_ref = format!("refs/heads/{branch}");
    let Some(head) = repo
        .git_opt(&["rev-parse", &head_ref])
        .map(|s| s.trim().to_string())
    else {
        // Empty repo: no commit to hang on yet, and the first turn commit writes the meta itself.
        return Ok(None);
    };
    if meta::read_at_ref(repo, &head).is_some_and(|m| m.is_session_line()) {
        return Ok(None);
    }
    let born = Meta::new_session_line(lk.source.clone(), lk.cwd.clone().unwrap_or_default());
    let born_text = meta::to_text(&born)?;
    let tree = super::new::fresh_session_tree(repo, &head, &born_text)?;
    let commit = super::plumbing::commit_tree(
        repo,
        &tree,
        &[&head],
        &format!("agit: claim session line {branch}"),
    )?;
    super::plumbing::update_ref_cas(repo, &head_ref, &commit, Some(&head))?;
    Ok(Some(commit))
}

/// Find a session by id or prefix. **Does not open the transcript file.**
fn by_selector(selector: &str, from: Option<&str>) -> crate::Result<Pick> {
    let runtimes: Vec<&'static str> = match from {
        Some(r) => vec![adapter::normalize(r)?],
        None => adapter::RUNTIMES.to_vec(),
    };

    let mut found: Vec<Found> = vec![];
    for rt in &runtimes {
        let ad = adapter::get(rt)?;
        // Resolve by full id first (Codex queries the `threads` table, Claude Code globs one
        // directory level; neither opens a transcript).
        if let Some(path) = ad.resolve(selector, None) {
            let cwd = cwd_of(ad.id(), selector, &path);
            found.push(Found {
                runtime: ad.id(),
                session_id: selector.to_string(),
                cwd,
            });
            continue;
        }
        // No hit: look through the list by prefix. This one costs more, and runs only when the
        // user gave a prefix.
        for sr in ad.all_sessions().unwrap_or_default() {
            if sr.id.starts_with(selector) {
                found.push(Found {
                    runtime: ad.id(),
                    session_id: sr.id,
                    cwd: sr.cwd,
                });
            }
        }
    }

    match found.len() {
        0 => {
            ui::error(&format!("no session named `{selector}`."));
            ui::hint(
                "`agit import -n <name>` without a session argument lists this repo’s candidates",
            );
            Ok(Pick::Explained(ExitCode::Ref))
        }
        1 => Ok(Pick::One(found.into_iter().next().unwrap())),
        n => {
            // An ambiguous prefix must error — importing the wrong session mixes unrelated
            // things into the agent's memory.
            ui::error(&format!("`{selector}` matches {n} sessions:"));
            for f in found.iter().take(8) {
                eprintln!("  {:12} {}", f.runtime, f.session_id);
            }
            ui::hint("give a longer prefix or select its runtime with `--from <runtime>`");
            Ok(Pick::Explained(ExitCode::Interactive))
        }
    }
}

/// What `--privacy` does: read the original → redact → write a byte copy under a **new session
/// identity**, so the rest of the import flow runs it through as an ordinary session.
///
/// # Why only claude-code
///
/// Claude Code indexes sessions as "`<uuid>.jsonl` in the project directory", so dropping one
/// file there is enough to be found; Codex indexes from the SQLite `threads` table, and injecting
/// a fake thread row lies to the runtime's own index (compaction and account accounting both take
/// it as true). Other runtimes take a file from `agit export <ref> --redact` first.
///
/// # The copy does not follow the original
///
/// The original session keeps growing; the copy stops at the moment of redaction — following the
/// original would turn the privacy gate into a one-time action, and secrets appended later would
/// slip into an already published lineage. Run `import --privacy` again to update it.
fn scrub_copy(found: &Found) -> crate::Result<Option<Found>> {
    if found.runtime != "claude-code" {
        ui::error(&format!(
            "--privacy currently supports claude-code sessions (this one is {}).",
            found.runtime
        ));
        ui::hint("for other runtimes: `agit export <ref> --format jsonl --redact -o <file>`");
        return Ok(None);
    }
    let ad = adapter::get("claude-code")?;
    let Some(path) = ad.resolve(&found.session_id, None) else {
        ui::error(&format!(
            "can't locate the transcript file for {}.",
            link::short(&found.session_id)
        ));
        return Ok(None);
    };
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;

    let rep = crate::domain::redact::Redactor::try_this_machine()?.scrub(&raw);

    // A new identity. The old id inside the copy is replaced along with it — a copy that claims
    // to be another session fools both resume and dedupe.
    let new_id = uuid::Uuid::new_v4().to_string();
    let text = rep.text.replace(&found.session_id, &new_id);

    let cwd = found
        .cwd
        .as_deref()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("can't determine a working directory for the scrubbed copy")
        })?;
    let home = config::user_home().ok_or_else(|| {
        anyhow::anyhow!("the user home is not set — can't place the scrubbed copy")
    })?;
    let dir = home
        .join(".claude")
        .join("projects")
        .join(crate::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&dir)?;
    let out = dir.join(format!("{new_id}.jsonl"));
    std::fs::write(&out, &text)?;

    ui::success(&format!(
        "scrubbed copy: {} ({} secrets, {} path/host hits, {} public IPs)",
        link::short(&new_id),
        rep.secrets,
        rep.paths,
        rep.ips
    ));
    // `ui::dim` only colors and **returns**; it does not print (unlike `ui::success` /
    // `ui::hint`, which do). A bare call here means this hint never reaches any `agit import`
    // output.
    println!(
        "{}",
        ui::dim(
            "  the copy is frozen at this moment; the original session keeps growing without it"
        )
    );
    println!();

    Ok(Some(Found {
        runtime: "claude-code",
        session_id: new_id,
        cwd: Some(cwd.to_string_lossy().into_owned()),
    }))
}

/// With no session argument: pick one of the sessions that ran in this repo and are not adopted
/// yet.
///
/// Candidates come from the runtime index (Codex queries the `threads` table, Claude Code reads
/// the directory), with **no transcript opened**.
fn pick_here(store: &Store, args: &Args) -> crate::Result<Pick> {
    pick_here_with_preview(store, args, true)
}

fn pick_here_with_preview(store: &Store, args: &Args, legacy_preview: bool) -> crate::Result<Pick> {
    let Some(repo) = config::repo_root().or_else(|| std::env::current_dir().ok()) else {
        ui::error("can’t determine the current directory.");
        ui::hint(
            "be explicit: agit import <session-id> --from <runtime> --into <owner/repo>@<branch>",
        );
        return Ok(Pick::Explained(ExitCode::Usage));
    };

    let links = link::list(store);
    let known: std::collections::HashSet<_> = links
        .iter()
        .map(|link| (link.source.as_str(), link.session_id.as_str()))
        .collect();

    let sp = ui::spinner("looking for sessions under this directory…");
    let mut cands = Vec::new();
    let selected_runtime = args.from.as_deref().map(adapter::normalize).transpose()?;
    for rt in adapter::RUNTIMES {
        if selected_runtime.is_some_and(|selected| selected != *rt) {
            continue;
        }
        let Ok(ad) = adapter::get(rt) else { continue };
        for sr in ad.sessions_for(&repo).unwrap_or_default() {
            if !known.contains(&(ad.id(), sr.id.as_str())) {
                cands.push((ad.id(), sr.path, sr.id, sr.gist));
            }
        }
    }
    sp.finish_and_clear();

    let here = repo.to_string_lossy().to_string();

    if cands.is_empty() {
        println!(
            "{}",
            ui::dim(&format!(
                "no unadopted sessions under {}.",
                ui::tilde(&repo)
            ))
        );
        ui::hint(
            "session ran in another directory? give the id directly: agit import <session-id> --from <runtime> --into <owner/repo>@<branch>",
        );
        return Ok(Pick::Explained(ExitCode::Ok));
    }

    // Before identity selection, an indexed gist is advisory; absent previews do not open native caches.
    let signals = crate::tui::Signals::from_process();
    let interactive = signals.interactive
        && (legacy_preview
            || (signals.off.is_none()
                && signals.agent_session.is_none()
                && std::env::var_os("CI").is_none()));
    let labels: Vec<String> = cands
        .iter()
        .map(|(rt, p, id, indexed_gist)| {
            let gist = if legacy_preview {
                gist_for(rt, id, p)
            } else {
                indexed_gist
                    .as_deref()
                    .map(|gist| ui::truncate(gist, 48))
                    .unwrap_or_else(|| "preview deferred until selection".into())
            };
            let identity = if interactive {
                link::short(id)
            } else {
                id.clone()
            };
            format!("{rt:12} {identity}  \"{gist}\"")
        })
        .collect();

    if !interactive {
        ui::error("a session must be selected explicitly; no interactive terminal is available.");
        for label in &labels {
            eprintln!("  {label}");
        }
        ui::hint(&format!("be explicit: {}", selection_command(args)));
        return Ok(Pick::Explained(ExitCode::Interactive));
    }

    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

    match ui::prompt::select("which session to adopt?", &refs)? {
        Some(i) => {
            let (rt, _, id, _) = &cands[i];
            Ok(Pick::One(Found {
                runtime: rt,
                session_id: id.clone(),
                cwd: Some(here),
            }))
        }
        None => {
            // Nothing to ask with when non-interactive — list them and let the user be
            // explicit; never guess.
            ui::error(
                "a session must be selected explicitly; no interactive terminal is available.",
            );
            for l in labels.iter().take(12) {
                println!("  {l}");
            }
            ui::hint(
                "be explicit: agit import <session-id> --from <runtime> --into <owner/repo>@<branch>",
            );
            Ok(Pick::Explained(ExitCode::Usage))
        }
    }
}

fn selection_command(args: &Args) -> String {
    let mut command = String::from("agit import <session-id>");
    for (flag, value) in [
        ("--into", args.repo.as_deref()),
        ("-n", args.name.as_deref()),
        ("-b", args.branch.as_deref()),
        ("--from", args.from.as_deref()),
        ("--onto", args.onto.as_deref()),
    ] {
        if let Some(value) = value {
            command.push_str(&format!(" {flag} {}", selection_arg(value)));
        }
    }
    if args.from.is_none() {
        command.push_str(" --from <runtime>");
    }
    if args.link_only {
        command.push_str(" --link-only");
    } else if args.repo.is_none()
        && (args.name.is_none() || (args.onto.is_none() && !args.independent))
    {
        command.push_str(if args.branch.is_some() {
            " --into <owner/repo>"
        } else {
            " --into <owner/repo>@<branch>"
        });
    } else if args.branch.is_none() && args.repo.as_deref().is_none_or(|repo| !repo.contains('@')) {
        command.push_str(" -b <branch>");
    }
    if args.privacy {
        command.push_str(" --privacy");
    }
    if args.independent {
        command.push_str(" --independent");
    }
    command
}

pub(super) fn selection_arg(value: &str) -> String {
    #[cfg(windows)]
    {
        powershell_selection_arg(value)
    }
    #[cfg(not(windows))]
    {
        if value.is_empty() || (value.starts_with('<') && value.ends_with('>')) {
            format!("'{}'", value.replace('\'', "'\\''"))
        } else {
            ui::session::shell_arg(value)
        }
    }
}

/// Adopt a session: write the link, nothing else.
///
/// **Does not read the transcript.** The version-recording step reads it; this does not repeat
/// that work.
///
/// cwd comes from the index (Codex queries the `threads` table) or from the result of
/// `sessions_for`, neither of which opens a file. When it is missing it stays empty, and
/// recording a version fills it in from the transcript.
///
/// # A repeated import keeps the agent it is already managed under
///
/// A bare `Link::new` overwrites the `agent` on an already-adopted link back to None. Once
/// written, that agent may already have been used by several commits, and erasing it makes the
/// next commit ask for the name again.
fn attach(store: &Store, found: &Found, existing: Option<Link>) -> crate::Result<Link> {
    let _guard = link::lock(store, found.runtime, &found.session_id)?;
    let path = link::link_path(store, found.runtime, &found.session_id);
    let current = match std::fs::symlink_metadata(&path) {
        Ok(_) => Some(link::read(&path).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot read the existing session link at {}",
                path.display()
            )
        })?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    // Destination selection can wait on remote permissions. Its link snapshot cannot replace a
    // claim, supersession marker or watermark another writer publishes while that request waits.
    if current.as_ref().map(Link::to_json).transpose()?
        != existing.as_ref().map(Link::to_json).transpose()?
    {
        anyhow::bail!(
            "the session link changed during destination selection; inspect its current claim with `agit status`, then retry the import"
        );
    }
    let s = ui::theme::symbols();
    let was_tracked = current.is_some();

    let mut lk = current.unwrap_or_else(|| Link::new(found.runtime, &found.session_id, None));
    if lk.cwd.is_none() {
        lk.cwd = found.cwd.clone();
    }
    // An explicit import is the user's answer to the naming prompt, including `--link-only`.
    // Leaving the dismissal set would make the same session disappear again if its claim is
    // temporarily incomplete.
    lk.naming_ignored = false;
    link::write(store, &lk)?;

    if was_tracked {
        println!(
            "{} {} {} was already adopted",
            ui::dim(s.idle),
            found.runtime,
            ui::bold(&link::short(&found.session_id))
        );
    } else {
        println!(
            "{} adopted {} {}",
            ui::ok(s.check),
            found.runtime,
            ui::bold(&link::short(&found.session_id))
        );
    }

    let mut kv: Vec<(&str, String)> = vec![];
    match &lk.cwd {
        Some(c) => kv.push(("working dir", ui::tilde(Path::new(c)))),
        None => kv.push((
            "working dir",
            ui::dim("unknown (filled in when a version is recorded)").to_string(),
        )),
    }
    kv.push((
        "link",
        ui::tilde(&link::link_path(store, &lk.source, &lk.session_id)),
    ));
    print!("{}", ui::table::key_values(&kv));
    Ok(lk)
}

/// The suggested agent name for the hint.
///
/// **It goes into the hint only; it is never used.** The difference is who decides: a name chosen
/// automatically silently decides which lineage this memory lands on, while a name in a hint
/// takes effect only once the user types it out.
fn suggested_name(cwd: Option<&str>) -> String {
    cwd.and_then(|c| {
        Path::new(c)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
    })
    .map(|n| {
        n.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
    })
    .filter(|n| repo::valid_name(n).is_ok())
    .unwrap_or_else(|| "<agent-name>".into())
}

/// The session's cwd.
///
/// Codex takes it from the `threads` table, with no file opened. Claude Code returns None — it
/// has no equivalent index, and recording a version fills it in from the transcript anyway.
///
/// **Cursor must get it here.** Its transcript carries no cwd field at all, so the "fill it in by
/// parsing the transcript" route does not exist, and a cwd not recorded at import time is lost
/// for good. The cost is opening one file ([`adapter::Adapter::parse_at`] infers it from the slug
/// in the path plus the absolute paths in the body), and this is the **one** session the user
/// picked deliberately — affordable.
fn cwd_of(runtime: &str, session_id: &str, path: &Path) -> Option<String> {
    match runtime {
        "codex" => adapter::codex_index::thread_by_id(session_id).and_then(|t| t.cwd),
        "cursor" => adapter::get(runtime).ok()?.parse_at(path).ok()?.cwd,
        _ => None,
    }
}

/// The opening prompt for the interactive list.
///
/// Codex takes it from the index; Claude Code can only read the file (the only exception, see the
/// comment at the call site).
pub(crate) fn gist_for(runtime: &str, session_id: &str, path: &Path) -> String {
    if runtime == "codex"
        && let Some(m) =
            adapter::codex_index::thread_by_id(session_id).and_then(|t| t.first_user_message)
    {
        let one = m.split_whitespace().collect::<Vec<_>>().join(" ");
        return ui::truncate(&one, 48);
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| adapter::get(runtime).ok()?.parse(&t).ok())
        .and_then(|ir| ir.gist(48))
        .unwrap_or_else(|| "(content unreadable)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::{self, Meta};
    use clap::Parser;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        a: super::Args,
    }

    /// The interface represents the bare command exactly. An option-bearing invocation stays on
    /// the command path so no requested operation is hidden or dropped by the picker.
    #[test]
    fn only_the_zero_argument_form_enters_the_import_picker() {
        assert!(wants_tui(&W::try_parse_from(["x"]).unwrap().a));
        for argv in [
            vec!["x", "SESSION"],
            vec!["x", "--from", "codex"],
            vec!["x", "--link-only"],
            vec!["x", "--privacy"],
            vec!["x", "--into", "nana/payments@work"],
        ] {
            let args = W::try_parse_from(argv).unwrap().a;
            assert!(!wants_tui(&args));
        }
    }

    #[cfg(unix)]
    #[test]
    fn selection_arguments_round_trip_literal_values_through_the_shell() {
        let values = [
            "",
            "<work>",
            "<work'literal>",
            "@branch",
            "work;literal'branch",
        ];
        let arguments = values.map(selection_arg).join(" ");
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("printf '%s\\0' {arguments}")])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let expected: Vec<u8> = values
            .iter()
            .flat_map(|value| value.bytes().chain(std::iter::once(0)))
            .collect();
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    /// `main` (the file line) plus someone else's session branch, HEAD parked on the latter —
    /// the situation where "the repo's current checkout" is not the target branch.
    fn repo_with_a_foreign_session() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("alice/photo")).unwrap();
        super::super::init::scaffold(r.root()).unwrap();
        r.add_all().unwrap();
        r.commit("agit: init (main file line)").unwrap();

        r.git(&["branch", "theirs"]).unwrap();
        r.switch("theirs").unwrap();
        meta::ensure_session_dir(r.root()).unwrap();
        std::fs::write(r.root().join(meta::LOG_FILE), "{\"_raw\":\"theirs\"}\n").unwrap();
        meta::write(
            r.root(),
            &Meta::new_session_line("codex".into(), "/other".into()),
        )
        .unwrap();
        r.add_all().unwrap();
        r.commit("agit: claim session line theirs").unwrap();
        (d, r)
    }

    /// A new session branch is born off the **`main` file line**, not off the repo's current
    /// checkout.
    ///
    /// Growing off the current checkout carries another session's transcript along, and the first
    /// settlement hits "already claimed by another session" — a repo then holds one session and
    /// no more.
    #[test]
    fn a_new_session_branch_is_born_off_the_file_line_not_the_current_checkout() {
        let (_d, r) = repo_with_a_foreign_session();
        assert_eq!(r.current_branch().as_deref(), Some("theirs"));
        assert_eq!(birth_base(&r).as_deref(), Some("main"));

        r.git(&["branch", "mine", &birth_base(&r).unwrap()])
            .unwrap();
        // A branch grown off main carries the shared files and not one byte of the other
        // transcript.
        assert!(r.show("refs/heads/mine", "AGENTS.md").is_some());
        assert!(r.show("refs/heads/mine", meta::LOG_FILE).is_none());
        assert!(
            meta::is_file_line_at(&r, "refs/heads/mine"),
            "the base is the file line"
        );
    }

    /// A legacy repo with no `main` still yields a base (the current head) instead of failing.
    #[test]
    fn a_legacy_repo_without_main_still_has_a_base() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("legacy")).unwrap();
        assert_eq!(birth_base(&r), None, "an empty repo has no base");
        std::fs::write(r.root().join("x"), "1").unwrap();
        r.add_all().unwrap();
        r.commit("one").unwrap();
        r.git(&["branch", "-m", "main", "solo"]).unwrap();
        assert_eq!(birth_base(&r).as_deref(), Some("HEAD"));
    }

    /// A failed import leaves no ghost branch and no moved checkout.
    ///
    /// The shape this pins: a refused import leaves a branch ref pointing at someone else's
    /// commit and the repo checkout moved onto it, and the `agit push` that follows (which only
    /// knows `current_branch`) publishes that ghost branch to the hub.
    #[test]
    fn a_refused_import_leaves_no_ghost_branch_and_no_moved_checkout() {
        let (_d, r) = repo_with_a_foreign_session();
        // Import half-done: the branch is created and the checkout has moved onto it.
        r.git(&["branch", "ghost", "main"]).unwrap();
        r.switch("ghost").unwrap();
        let oid = r
            .git(&["rev-parse", "refs/heads/ghost"])
            .unwrap()
            .trim()
            .to_string();
        let landing = Landing {
            claim_source: "codex".into(),
            claim_session: "AB".into(),
            previous_image: None,
            repo_dir: r.root().to_path_buf(),
            branch: "ghost".into(),
            created: true,
            created_oid: Some(oid),
            prev_checkout: Some("theirs".into()),
            store: Store::at(r.root().join("store")),
            prev_link: None,
            claimed_path: PathBuf::new(),
            claimed_bytes: vec![],
        };

        landing.rollback();

        assert!(!r.has_ref("refs/heads/ghost"), "the ghost ref must be gone");
        assert_eq!(
            r.current_branch().as_deref(),
            Some("theirs"),
            "the checkout must be restored"
        );
        assert!(r.has_ref("refs/heads/main"), "no other branch is touched");
    }

    /// Deleting the branch this import created carries an expected OID: a branch someone else
    /// advanced is left in place.
    #[test]
    fn rollback_leaves_a_branch_someone_else_advanced() {
        let (_d, r) = repo_with_a_foreign_session();
        r.git(&["branch", "line", "main"]).unwrap();
        let oid = r
            .git(&["rev-parse", "refs/heads/line"])
            .unwrap()
            .trim()
            .to_string();
        let landing = Landing {
            claim_source: "codex".into(),
            claim_session: "AB".into(),
            previous_image: None,
            repo_dir: r.root().to_path_buf(),
            branch: "line".into(),
            created: true,
            created_oid: Some(oid),
            prev_checkout: None,
            store: Store::at(r.root().join("store")),
            prev_link: None,
            claimed_path: PathBuf::new(),
            claimed_bytes: vec![],
        };
        // Someone else lands a new commit on this branch.
        r.git(&["switch", "line"]).unwrap();
        std::fs::write(r.root().join("f"), "x").unwrap();
        r.git(&["add", "."]).unwrap();
        r.git(&["commit", "-m", "advanced"]).unwrap();
        r.git(&["switch", "main"]).unwrap();

        landing.rollback();
        assert!(r.has_ref("refs/heads/line"), "an advanced branch is kept");
    }

    /// The rollback's link restore is a CAS: when the link was advanced between the snapshot and
    /// the rollback, the old snapshot must not go back.
    #[test]
    fn rollback_leaves_a_link_someone_else_advanced() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::at(d.path().join("store"));
        let mut prev = Link::new("codex", "AB", None);
        prev.branch = Some("old".into());
        let mut claimed = prev.clone();
        claimed.branch = Some("new".into());
        let claimed_path = link::write(&store, &claimed).unwrap();
        let claimed_bytes = std::fs::read(&claimed_path).unwrap();
        // A concurrent settlement advanced the link: the disk no longer holds what this claim
        // wrote.
        let mut advanced = claimed.clone();
        advanced.baseline_bytes = Some(999);
        link::write(&store, &advanced).unwrap();
        let (_rd, r) = repo_with_a_foreign_session();
        let landing = Landing {
            claim_source: "codex".into(),
            claim_session: "AB".into(),
            previous_image: None,
            repo_dir: r.root().to_path_buf(),
            branch: "x".into(),
            created: false,
            created_oid: None,
            prev_checkout: None,
            store: store.clone(),
            prev_link: Some(prev),
            claimed_path,
            claimed_bytes,
        };
        landing.rollback();
        let now = link::get(&store, "codex", "AB").unwrap();
        assert_eq!(now.baseline_bytes, Some(999), "watermark is not rewound");
        assert_eq!(now.branch.as_deref(), Some("new"));
    }

    /// A failed selected import removes only its own claim, or restores the exact prior image.
    #[test]
    fn selected_rollback_retains_raw_unknown_fields_and_preserves_concurrent_claims() {
        for prior in [
            None,
            Some(b"{\"extension\":true,\"agent\":\"prior\"}\n".to_vec()),
        ] {
            for advanced in [false, true] {
                let (directory, repo) = repo_with_a_foreign_session();
                let store = Store::at(directory.path().join("store"));
                let claimed = Link::new("codex", "AB", None);
                let claimed_path = link::write(&store, &claimed).unwrap();
                let claimed_bytes = std::fs::read(&claimed_path).unwrap();
                let landing = Landing {
                    repo_dir: repo.root().to_owned(),
                    branch: "unused".into(),
                    created: false,
                    created_oid: None,
                    prev_checkout: None,
                    store: store.clone(),
                    prev_link: None,
                    claim_source: "codex".into(),
                    claim_session: "AB".into(),
                    claimed_path: claimed_path.clone(),
                    claimed_bytes,
                    previous_image: Some(prior.clone()),
                };
                if advanced {
                    std::fs::write(&claimed_path, b"concurrent image").unwrap();
                }
                landing.rollback();
                if advanced {
                    assert_eq!(std::fs::read(&claimed_path).unwrap(), b"concurrent image");
                } else if let Some(prior) = &prior {
                    assert_eq!(std::fs::read(&claimed_path).unwrap(), *prior);
                } else {
                    assert!(!claimed_path.exists());
                }
            }
        }
    }

    /// An in-flight first turn makes settlement a no-op. The claim must already
    /// be durable so the next argument-free commit can recover the live transcript.
    #[test]
    fn an_in_flight_import_persists_its_branch_claim() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::at(d.path().join("store"));
        let mut lk = Link::new("codex", "AB", Some(Path::new("/repo/one")));

        persist_branch_claim(&store, &mut lk, "alice", "photo", "work").unwrap();

        let saved = link::get(&store, "codex", "AB").unwrap();
        assert_eq!(saved.agent.as_deref(), Some("photo"));
        assert_eq!(saved.branch.as_deref(), Some("work"));
    }

    /// Only a complete matching identity can reuse its claim without confirmation.
    #[test]
    fn a_claim_is_elsewhere_when_any_identity_component_differs() {
        let mut lk = Link::new("codex", "AB", Some(Path::new("/repo/one")));
        assert_eq!(claimed_elsewhere(&lk, "alice", "photo", "work"), None);
        lk.branch = Some("work".into());
        assert_eq!(
            claimed_elsewhere(&lk, "alice", "photo", "work").as_deref(),
            Some("<unknown-owner>/<unknown-repo>@work")
        );
        lk.agent = Some("photo".into());
        assert_eq!(
            claimed_elsewhere(&lk, "alice", "photo", "work").as_deref(),
            Some("<unknown-owner>/photo@work")
        );
        lk.owner = Some(String::new());
        assert_eq!(
            claimed_elsewhere(&lk, "alice", "photo", "work").as_deref(),
            Some("<unknown-owner>/photo@work")
        );
        lk.owner = Some("alice".into());
        assert_eq!(claimed_elsewhere(&lk, "alice", "photo", "work"), None);
        for (owner, agent, branch) in [
            ("bob", "photo", "work"),
            ("alice", "notes", "work"),
            ("alice", "photo", "other"),
        ] {
            assert_eq!(
                claimed_elsewhere(&lk, owner, agent, branch).as_deref(),
                Some("alice/photo@work")
            );
        }
    }

    #[test]
    fn explicit_import_clears_a_naming_dismissal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        let mut existing = Link::new("codex", "AB", Some(Path::new("/repo/one")));
        existing.naming_ignored = true;
        link::write(&store, &existing).unwrap();
        let found = Found {
            runtime: "codex",
            session_id: "AB".into(),
            cwd: Some("/repo/one".into()),
        };

        let attached = attach(&store, &found, Some(existing)).unwrap();

        assert!(!attached.naming_ignored);
        assert!(!link::get(&store, "codex", "AB").unwrap().naming_ignored);
    }

    /// Destination selection must not rewind a claim or its settlement state after a delayed lookup.
    #[test]
    fn attach_preserves_a_link_changed_during_destination_selection() {
        for previously_present in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::at(dir.path().join("store"));
            let mut old = Link::new("codex", "AB", Some(Path::new("/repo/one")));
            old.owner = Some("alice".into());
            old.agent = Some("photo".into());
            old.branch = Some("work".into());
            let expected = previously_present.then(|| old.clone());
            let mut current = old;
            current.owner = Some("organization".into());
            current.branch = Some("continued".into());
            current.superseded_by = Some("replacement".into());
            current.baseline_bytes = Some(17);
            current.baseline_hash = Some("new-baseline".into());
            let path = link::write(&store, &current).unwrap();
            let before = std::fs::read(&path).unwrap();
            let found = Found {
                runtime: "codex",
                session_id: "AB".into(),
                cwd: Some("/repo/one".into()),
            };
            let error = attach(&store, &found, expected).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("changed during destination selection")
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    /// Unreadable existing metadata is evidence to preserve, never a fresh adoption slot.
    #[test]
    fn attach_preserves_an_unreadable_existing_link() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        let path = link::write(&store, &Link::new("codex", "AB", None)).unwrap();
        std::fs::write(&path, "{incomplete").unwrap();
        let found = Found {
            runtime: "codex",
            session_id: "AB".into(),
            cwd: None,
        };
        assert!(attach(&store, &found, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{incomplete");
    }

    /// Placement cannot recreate a lost attachment or overwrite unreadable supersession evidence.
    #[test]
    fn branch_placement_preserves_a_missing_or_unreadable_attached_link() {
        const CHILD: &str = "AGIT_TEST_PLACEMENT_FINAL_LINK_CHILD";
        const COMPLETE: &str = "placement final-link controls completed";
        if std::env::var_os(CHILD).is_none() {
            let isolated = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "commands::import::tests::branch_placement_preserves_a_missing_or_unreadable_attached_link",
                    "--nocapture",
                ])
                .env_clear();
            for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            let output = command
                .env(CHILD, "1")
                .env("HOME", isolated.path())
                .env("USERPROFILE", isolated.path())
                .env("AGIT_HOME", isolated.path().join("agit"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains(COMPLETE));
            return;
        }
        for malformed in [false, true] {
            let (directory, repo) = repo_with_a_foreign_session();
            let store = Store::at(directory.path().join("store"));
            let found = Found {
                runtime: "codex",
                session_id: "AB".into(),
                cwd: None,
            };
            let mut lk = attach(&store, &found, None).unwrap();
            let path = link::link_path(&store, found.runtime, &found.session_id);
            let evidence = b"{\"superseded_by\":\"replacement\",";
            if malformed {
                std::fs::write(&path, evidence).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            let before_refs = repo.git(&["show-ref"]).unwrap();
            let before_checkout = repo.current_branch();
            let repo_dir = repo.root().to_path_buf();
            let result = birth_session_branch(
                &mut lk,
                &store,
                "photo",
                "alice",
                "alice",
                repo_dir.clone(),
                repo,
                "recovery".into(),
                None,
                None,
                None,
            );
            assert!(matches!(result.unwrap(), Placed::Refused(ExitCode::Policy)));
            if malformed {
                assert_eq!(std::fs::read(&path).unwrap(), evidence);
            } else {
                assert!(!path.exists());
            }
            let repo = Repo::open(&repo_dir).unwrap();
            assert_eq!(repo.git(&["show-ref"]).unwrap(), before_refs);
            assert_eq!(repo.current_branch(), before_checkout);
            assert_eq!(lk.owner, None);
            assert_eq!(lk.agent, None);
            assert_eq!(lk.branch, None);
        }
        println!("{COMPLETE}");
    }

    /// A selected checkout and commit survive name drift; locked placement still rejects lost evidence.
    #[test]
    fn prepared_target_is_consumed_and_revalidated_under_the_claim_lock() {
        const CHILD: &str = "AGIT_TEST_IMPORT_TARGET_CHILD";
        const COMPLETE: &str = "prepared target controls completed";
        if std::env::var_os(CHILD).is_none() {
            let isolated = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact", "commands::import::tests::prepared_target_is_consumed_and_revalidated_under_the_claim_lock", "--nocapture",
            ]).env_clear();
            for key in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            let output = command
                .env(CHILD, "1")
                .env("HOME", isolated.path())
                .env("USERPROFILE", isolated.path())
                .env("AGIT_HOME", isolated.path().join("agit"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", isolated.path().join("absent-config"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains(COMPLETE));
            return;
        }
        fn repository(owner: &str, name: &str) -> Repo {
            let repo = Repo::init(&config::repo_dir(owner, name).unwrap()).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            meta::write(repo.root(), &Meta::new_file_line()).unwrap();
            repo.add_all().unwrap();
            repo.commit("synthetic file line").unwrap();
            repo
        }
        let store = Store::open_or_init().unwrap();
        for shape in ["stable", "object", "claim", "repository", "grafts"] {
            let repo = repository("alice", shape);
            let oid = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
            repo.git(&["branch", "base", &oid]).unwrap();
            let found = Found {
                runtime: "codex",
                session_id: shape.into(),
                cwd: None,
            };
            let mut lk = Link::new(found.runtime, &found.session_id, None);
            let args = W::parse_from([
                "x", shape, "--from", "codex", "-n", shape, "-b", "selected", "--onto", "base",
            ])
            .a;
            let TargetSelection::Ready(selected) =
                prepare_target(&lk, shape, "alice", &args, None, None).unwrap()
            else {
                panic!("the explicit base must be selectable");
            };
            let repo_dir = selected.repo_dir.clone();
            assert_eq!(selected.onto_commit.as_deref(), Some(oid.as_str()));
            if shape == "stable" {
                let other = repository("bob", shape);
                let other_refs = other.git(&["show-ref"]).unwrap();
                let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
                let advanced = repo
                    .git(&["commit-tree", &tree, "-p", &oid, "-m", "advanced name"])
                    .unwrap();
                repo.git(&["update-ref", "refs/heads/base", &advanced])
                    .unwrap();
                selected.verify().unwrap();
                lk = attach(&store, &found, None).unwrap();
                let placed =
                    place_on_branch(&mut lk, &store, shape, "alice", &selected, None).unwrap();
                assert!(matches!(placed, Placed::Ready(_)));
                assert_eq!(repo.git(&["rev-parse", "selected^"]).unwrap().trim(), oid);
                assert_eq!(other.git(&["show-ref"]).unwrap(), other_refs);
                assert_eq!(
                    (lk.owner.as_deref(), lk.agent.as_deref()),
                    (Some("alice"), Some(shape))
                );
                continue;
            }
            lk = attach(&store, &found, None).unwrap();
            let guard = link::lock(&store, found.runtime, &found.session_id).unwrap();
            let worker_store = store.clone();
            let worker = std::thread::spawn(move || {
                place_on_branch(&mut lk, &worker_store, shape, "alice", &selected, None)
            });
            use fs2::FileExt as _;
            use sha2::Digest as _;
            let mut digest = sha2::Sha256::new();
            digest.update(format!("alice/{shape}").as_bytes());
            digest.update([0]);
            digest.update(b"selected");
            let lock_path = store
                .root()
                .join(".locks/branches")
                .join(format!("{}.lock", hex::encode(digest.finalize())));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if let Ok(file) = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                {
                    match file.try_lock_exclusive() {
                        Ok(()) => fs2::FileExt::unlock(&file).unwrap(),
                        Err(error)
                            if error.raw_os_error()
                                == fs2::lock_contended_error().raw_os_error() =>
                        {
                            break;
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
                assert!(
                    !worker.is_finished(),
                    "placement completed without the held claim lock"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "placement did not reach its branch lock"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let path = link::link_path(&store, found.runtime, &found.session_id);
            match shape {
                "object" => {
                    std::fs::remove_file(
                        repo_dir
                            .join(".git/objects")
                            .join(&oid[..2])
                            .join(&oid[2..]),
                    )
                    .unwrap();
                }
                "claim" => {
                    let mut current = link::get(&store, found.runtime, &found.session_id).unwrap();
                    current.owner = Some("bob".into());
                    current.agent = Some("other".into());
                    current.branch = Some("continued".into());
                    link::write(&store, &current).unwrap();
                }
                "repository" => {
                    match std::fs::rename(&repo_dir, repo_dir.with_extension("retained")) {
                        Ok(()) => {}
                        #[cfg(windows)]
                        Err(error) if error.raw_os_error() == Some(5) => {
                            // Open child handles can fence a Windows directory rename. Replacing
                            // its leaf Git directory still changes the selected repository identity.
                            assert!(repo_dir.is_dir());
                            assert!(!repo_dir.with_extension("retained").exists());
                            std::fs::rename(
                                repo_dir.join(".git"),
                                repo_dir.with_extension("retained-git"),
                            )
                            .unwrap();
                        }
                        Err(error) => panic!("cannot replace the selected repository: {error}"),
                    }
                    repository("alice", shape);
                }
                "grafts" => {
                    std::fs::write(repo_dir.join(".git/info/grafts"), format!("{oid}\n")).unwrap();
                }
                _ => unreachable!(),
            }
            let before_link = std::fs::read(&path).unwrap();
            let current_repo = Repo::at(&repo_dir);
            let git_image = || {
                walkdir::WalkDir::new(repo_dir.join(".git"))
                    .into_iter()
                    .map(|entry| entry.unwrap())
                    .filter(|entry| entry.file_type().is_file())
                    .map(|entry| {
                        (
                            entry.path().to_owned(),
                            std::fs::read(entry.path()).unwrap(),
                        )
                    })
                    .collect::<std::collections::BTreeMap<_, _>>()
            };
            let before_git = git_image();
            drop(guard);
            let outcome = worker.join().unwrap().unwrap();
            assert!(matches!(outcome, Placed::Refused(ExitCode::Policy)));
            assert_eq!(std::fs::read(path).unwrap(), before_link);
            assert_eq!(git_image(), before_git);
            assert!(!current_repo.has_ref("refs/heads/selected"));
        }
        println!("{COMPLETE}");
    }

    #[test]
    fn no_all_flag_exists() {
        // "by session id only, never import everything" is a deliberate product decision: of
        // 18858 sessions on this machine, 18745 are the residue of automated batch runs, and
        // importing all of them makes an agent's memory meaningless.
        for flag in ["--all", "-a"] {
            assert!(
                W::try_parse_from(["x", flag]).is_err(),
                "`{flag}` must not exist"
            );
        }
    }

    /// The name is the point of this command, yet it stays an optional argument — an
    /// already-adopted session reuses the agent it is already managed under.
    #[test]
    fn the_name_is_a_flag_not_a_second_positional() {
        // A second positional argument fights the session id: which does `agit import photo` mean?
        assert!(W::try_parse_from(["x", "AB", "photo"]).is_err());
        assert_eq!(
            W::parse_from(["x", "AB", "-n", "photo"]).a.name.as_deref(),
            Some("photo")
        );
        assert!(W::parse_from(["x", "AB"]).a.name.is_none());
    }

    /// The offline route stays, and it is opt-in.
    #[test]
    fn link_only_is_opt_in() {
        assert!(
            !W::parse_from(["x", "AB"]).a.link_only,
            "the default records a version"
        );
        assert!(W::parse_from(["x", "AB", "--link-only"]).a.link_only);
    }

    /// An explicit base must describe the destination's attachment line, not merely a merged input.
    #[test]
    fn an_existing_destination_checks_the_explicit_onto_base() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        super::super::init::scaffold(repo.root()).unwrap();
        repo.add_all().unwrap();
        repo.commit("main file line").unwrap();
        let base = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["branch", "work", &base]).unwrap();
        repo.git(&["branch", "side", &base]).unwrap();
        let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let first = repo
            .git(&["commit-tree", &tree, "-p", &base, "-m", "work"])
            .unwrap();
        let side = repo
            .git(&["commit-tree", &tree, "-p", &base, "-m", "side"])
            .unwrap();
        let merged = repo
            .git(&[
                "commit-tree",
                &tree,
                "-p",
                &first,
                "-p",
                &side,
                "-m",
                "merged",
            ])
            .unwrap();
        repo.git(&["update-ref", "refs/heads/work", &merged])
            .unwrap();
        repo.git(&["update-ref", "refs/heads/side", &side]).unwrap();

        assert!(existing_onto_is_lineage(&repo, "work", &merged).unwrap());
        assert!(existing_onto_is_lineage(&repo, "work", &first).unwrap());
        assert!(existing_onto_is_lineage(&repo, "work", &base).unwrap());
        assert!(!existing_onto_is_lineage(&repo, "work", &side).unwrap());
        let unrelated = repo
            .git(&["commit-tree", &tree, "-m", "independent"])
            .unwrap();
        assert!(!existing_onto_is_lineage(&repo, "work", &unrelated).unwrap());

        let store = Store::at(d.path().join("store"));
        let mut lk = Link::new("codex", "synthetic-session", None);
        lk.owner = Some("alice".into());
        lk.agent = Some("repo".into());
        lk.branch = Some("work".into());
        let link_path = link::write(&store, &lk).unwrap();
        let link_before = std::fs::read(&link_path).unwrap();
        let refs_before = repo.git(&["show-ref"]).unwrap();
        let checkout_before = repo.current_branch();
        let placed = birth_session_branch(
            &mut lk,
            &store,
            "repo",
            "alice",
            "alice",
            repo.root().to_path_buf(),
            Repo::at(repo.root()),
            "work".into(),
            Some(side),
            None,
            None,
        )
        .unwrap();
        assert!(matches!(placed, Placed::Refused(ExitCode::Policy)));
        assert_eq!(std::fs::read(link_path).unwrap(), link_before);
        assert_eq!(repo.git(&["show-ref"]).unwrap(), refs_before);
        assert_eq!(repo.current_branch(), checkout_before);
    }

    /// A matching tip cannot hide a later failure to read its first-parent ancestry.
    #[test]
    fn existing_onto_requires_a_successful_walk_after_a_matching_tip() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("repo")).unwrap();
        super::super::init::scaffold(repo.root()).unwrap();
        repo.add_all().unwrap();
        repo.commit("root").unwrap();
        let root = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let middle = repo
            .git(&["commit-tree", &tree, "-p", &root, "-m", "middle"])
            .unwrap();
        let tip = repo
            .git(&["commit-tree", &tree, "-p", &middle, "-m", "tip"])
            .unwrap();
        repo.git(&["update-ref", "refs/heads/work", &tip]).unwrap();
        std::fs::remove_file(
            repo.root()
                .join(".git/objects")
                .join(&root[..2])
                .join(&root[2..]),
        )
        .unwrap();

        let mut saw_tip = false;
        let walk = repo.git_stream_split(
            &["rev-list", "--first-parent", "refs/heads/work", "--"],
            b'\n',
            |oid| {
                saw_tip |= oid == tip.as_bytes();
                Ok(())
            },
        );
        assert!(saw_tip);
        assert!(walk.is_err());
        assert!(existing_onto_is_lineage(&repo, "work", &tip).is_err());
    }

    #[test]
    fn importing_onto_legacy_history_publishes_a_migrated_tip_with_exact_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("store"));
        let repo_dir = directory.path().join("repos/alice/history");
        let repo = Repo::init(&repo_dir).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        super::super::init::scaffold(repo.root()).unwrap();
        repo.add_all().unwrap();
        repo.commit("main file line").unwrap();
        let main = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let mut legacy = Meta::new(
            "agit-1111111111111111111111111111111111111111".into(),
            "codex".into(),
            "/project".into(),
        );
        legacy.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &legacy).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), []).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), []).unwrap();
        repo.add_all().unwrap();
        repo.commit("legacy session").unwrap();
        let frozen = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["reset", "--hard", &main]).unwrap();
        let mut lk = Link::new("codex", "inflight", None);
        link::write(&store, &lk).unwrap();
        let placed = birth_session_branch(
            &mut lk,
            &store,
            "history",
            "alice",
            "alice",
            repo_dir.clone(),
            repo,
            "replay".into(),
            Some(frozen.clone()),
            None,
            None,
        )
        .unwrap();
        let Placed::Ready(landing) = placed else {
            panic!("the unclaimed legacy lineage must be importable");
        };
        let repo = Repo::open(&repo_dir).unwrap();
        let published = repo.git(&["rev-parse", "refs/heads/replay"]).unwrap();
        assert_eq!(landing.created_oid.as_deref(), Some(published.as_str()));
        assert_eq!(
            meta::read_at_ref(&repo, &published).unwrap().layout,
            meta::LayoutVersion::V1
        );
        assert_eq!(repo.git(&["rev-parse", "replay^1"]).unwrap(), frozen);
        assert_eq!(
            meta::read_at_ref(&repo, &frozen).unwrap().layout,
            meta::LayoutVersion::V0
        );
        landing.rollback();
        assert!(!repo.has_ref("refs/heads/replay"));
        assert_eq!(repo.git(&["rev-parse", "main"]).unwrap(), main);
    }

    /// Explicit placement of an unclaimed link creates the shared file line before its session branch.
    #[test]
    fn link_only_followup_births_main_before_the_session_branch() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::at(d.path().join("store"));
        let repo_dir = d.path().join("agents/alice/photo");
        let mut lk = Link::new("codex", "AB", None);
        link::write(&store, &lk).unwrap();

        let placed = place_legacy_commit_branch(
            &mut lk,
            &store,
            "photo",
            "alice",
            "alice",
            &repo_dir,
            "fix-auth".into(),
        )
        .unwrap();
        assert!(matches!(placed, Placed::Ready(_)));

        let repo = Repo::open(&repo_dir).unwrap();
        assert!(meta::is_file_line_at(&repo, "refs/heads/main"));
        assert!(
            meta::read_at_ref(&repo, "refs/heads/fix-auth")
                .is_some_and(|snapshot| snapshot.is_session_line())
        );
        repo.git(&["merge-base", "--is-ancestor", "main", "fix-auth"])
            .unwrap();
        assert!(repo.show("refs/heads/fix-auth", "AGENTS.md").is_some());

        let saved = link::get(&store, "codex", "AB").unwrap();
        assert_eq!(saved.owner.as_deref(), Some("alice"));
        assert_eq!(saved.agent.as_deref(), Some("photo"));
        assert_eq!(saved.branch.as_deref(), Some("fix-auth"));
    }

    #[test]
    fn at_selects_the_current_runtime_session() {
        assert_eq!(W::parse_from(["x", "@"]).a.session.as_deref(), Some("@"));
    }

    /// The suggested name only reaches a hint, so it must always be something typeable as is.
    #[test]
    fn the_suggested_name_is_always_pasteable() {
        use super::suggested_name;
        assert_eq!(
            suggested_name(Some("/Users/nana/Projects/OpenPad")),
            "OpenPad"
        );
        // Illegal characters become hyphens.
        assert_eq!(suggested_name(Some("/tmp/my project!")), "my-project-");
        // An `agit-` prefix is unambiguous in a repo name, so the directory name is used as is.
        assert_eq!(suggested_name(Some("/tmp/agit-photo")), "agit-photo");
        // Unavailable, or still invalid after cleaning: fall back to the placeholder.
        assert_eq!(suggested_name(None), "<agent-name>");
        assert_eq!(suggested_name(Some("/")), "<agent-name>");
    }
}
