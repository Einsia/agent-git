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
//! agit import <session-id> -n photo
//! ```
//!
//! The version half calls [`super::commit::record`] directly — the same code path as
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
//! `--link-only` keeps the purely offline route: write the link only, and `agit commit` later to
//! record a version. Marking a session down on a plane must not need the network.

use super::CmdResult;
use crate::domain::link::{self, Link};
use crate::domain::repo::{self, Repo};
use crate::domain::store::Store;
use crate::infra::config;
use crate::{ExitCode, adapter, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// Session id (or prefix); `@` means the current runtime session; omitted: lists this repo’s candidates to pick from
    #[arg(value_name = "session")]
    pub session: Option<String>,

    /// Adopt into this agent — the first version lands under this name
    #[arg(short = 'n', long = "name", value_name = "agent")]
    pub name: Option<String>,

    /// Which runtime to look in (default: all of them)
    #[arg(long, value_name = "runtime")]
    pub from: Option<String>,

    /// Link only, no version yet. Works offline; `agit commit` later
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

    let store = Store::open_or_init()?;

    // ── 2. Find that session ──
    let picked = match &args.session {
        Some(sel) if sel == "@" => {
            let Some(current) = crate::infra::runtime_session::current() else {
                ui::error("`@` requires an active runtime session in this process.");
                ui::hint(
                    "run this command from inside Codex/Claude Code/OpenCode, or give an explicit session id",
                );
                return Ok(ExitCode::Precondition);
            };
            if let Some(requested) = args.from.as_deref().map(adapter::normalize).transpose()?
                && requested != current.runtime
            {
                ui::error(&format!(
                    "`@` resolved to the current {} session, not `{requested}`.",
                    crate::ui::session::runtime_label(current.runtime)
                ));
                return Ok(ExitCode::Usage);
            }
            Pick::One(Found {
                runtime: current.runtime,
                session_id: current.session_id,
                cwd: current.cwd,
            })
        }
        Some(sel) => by_selector(sel, args.from.as_deref())?,
        None => pick_here(&store)?,
    };
    let found = match picked {
        Pick::One(f) => f,
        Pick::Explained(code) => return Ok(code),
    };

    // ── 2.5 --privacy: swap in a redacted copy; no byte of the original enters history ──
    let found = if args.privacy {
        match scrub_copy(&found)? {
            Some(f) => f,
            None => return Ok(ExitCode::Usage),
        }
    } else {
        found
    };

    // ── 3. The name ──
    //
    // An already-adopted session reuses the agent it is managed under; otherwise the name must be
    // given explicitly. **Never guess**: a name chosen automatically silently decides which
    // lineage this memory lands on, and that kind of mistake is not noticed right away.
    let existing = link::get(&store, found.runtime, &found.session_id);
    let destination = args
        .repo
        .as_deref()
        .map(crate::commands::target::parse)
        .transpose()?;
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
                println!(
                    "{}",
                    ui::dim(&format!(
                        "  {repo} isn’t on the hub yet — the first push creates it under the {dest_owner} organization"
                    ))
                );
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
    let agent = destination
        .as_ref()
        .and_then(|t| t.repo.as_deref())
        .and_then(|r| super::parse_slug(r).ok().map(|(_, n)| n))
        .or_else(|| args.name.clone())
        .or_else(|| existing.as_ref().and_then(|l| l.agent.clone()));

    if agent.is_none() && !args.link_only {
        ui::error("versioning needs a destination agent named first.");
        ui::hint(&format!(
            "agit import {} -n {}",
            link::short(&found.session_id),
            suggested_name(found.cwd.as_deref())
        ));
        ui::hint("adopt without versioning (works offline): add --link-only");
        return Ok(ExitCode::Usage);
    }

    // ── 4. Adoption: write the link ──
    let lk = attach(&store, &found, existing)?;

    if args.link_only {
        println!(
            "\n{}",
            ui::dim(&format!(
                "  `agit commit {} -n <name>` records the first version",
                link::short(&lk.session_id)
            ))
        );
        return Ok(ExitCode::Ok);
    }

    // ── 5. Record the opening version (turn-by-turn settlement, the `agit commit` path) ──
    let agent = agent.unwrap();
    let owner = owner.unwrap();
    // The destination decides which namespace the repo lands in (an org repo is `<org>/<name>`);
    // the author identity is always the signed-in account. The two coincide only in your own repo.
    let namespace = destination
        .as_ref()
        .and_then(|t| t.repo.as_deref())
        .and_then(|r| super::parse_slug(r).ok().map(|(o, _)| o))
        .unwrap_or_else(|| owner.clone());
    let mut lk = lk;
    let landing = match place_on_branch(
        &mut lk,
        &store,
        &agent,
        &namespace,
        &owner,
        &args,
        destination.as_ref(),
    )? {
        Placed::Ready(l) => *l,
        Placed::Refused(code) => return Ok(code),
    };
    println!();
    // Settlement that did not succeed = this import did not happen: put the ref that was created
    // and the checkout that was switched back the way they were.
    let outcome = super::commit::record(&store, lk, &agent, &namespace, &owner);
    if !matches!(outcome, Ok(ExitCode::Ok)) {
        landing.rollback();
    }
    outcome
}

/// Where this import lands, and how to put it back on failure.
struct Landing {
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
    claimed_path: PathBuf,
    claimed_bytes: Vec<u8>,
}

impl Landing {
    /// Put the ref and the checkout back the way they were before the import.
    ///
    /// Without this, an import refused by "already claimed" leaves a branch ref pointing at
    /// **someone else's commit** and moves the repo checkout onto it; the `agit push` that
    /// follows (which only knows `current_branch`) then publishes that ghost branch to the hub.
    /// So "did not succeed" must mean "nothing happened" — switch back first, then delete the
    /// ref; in the other order git refuses to delete the current branch.
    fn rollback(&self) {
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
        if let Some(prev) = &self.prev_link {
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
enum Placed {
    Ready(Box<Landing>),
    /// The reason is already printed; exit with this code.
    Refused(ExitCode),
}

/// Decide which branch this import lands on, and create it (PRD: import claiming an existing
/// transcript creates a branch; `main` is the file line, and a session never lands on it).
///
/// Every check runs before any write: anything uncertain is asked before a ref or a checkout is
/// touched, and the failure that remains (settlement itself refused) is cleaned up by
/// [`Landing::rollback`].
fn place_on_branch(
    lk: &mut Link,
    store: &Store,
    agent: &str,
    owner: &str,
    author: &str,
    args: &Args,
    destination: Option<&crate::commands::target::Target>,
) -> crate::Result<Placed> {
    // An explicitly named destination is that directory; no searching by name for a same-named
    // checkout in another namespace. When a personal repo and an org repo share a name, guessing
    // picks the other one.
    let repo_dir = if destination.is_some() {
        crate::infra::config::repo_dir(owner, agent)?
    } else {
        super::clone::checkout_for_recording(owner, agent)?
    };
    let repo = Repo::open_or_init(&repo_dir)?;

    // --onto: the lineage attachment point. The point must exist; the new branch grows off it
    // (identity is inherited, not claimed again).
    let onto_commit = if let Some(o) = &args.onto {
        let spec = crate::domain::refs::parse(o)?;
        let spec = match crate::commands::context::substitute_at(spec) {
            Ok(spec) => spec,
            Err(e) => {
                ui::error(&format!("--onto `{o}` failed to resolve: {e:#}"));
                return Ok(Placed::Refused(ExitCode::Ref));
            }
        };
        let resolved = match crate::domain::refs::resolve(&repo, &spec) {
            Ok(r) => r,
            Err(e) => {
                ui::error(&format!("--onto `{o}` failed to resolve: {e:#}"));
                return Ok(Placed::Refused(ExitCode::Ref));
            }
        };
        Some(resolved.sha)
    } else {
        None
    };

    // The target branch name.
    let cur = repo.current_branch();
    let cur_is_session = cur
        .as_deref()
        .and_then(|b| crate::domain::meta::read_at_ref(&repo, &format!("refs/heads/{b}")))
        .is_some_and(|m| m.is_session_line());
    let target_branch = destination.and_then(|t| match t.base.as_deref() {
        Some("@") | None => None,
        Some(b) => Some(b.to_string()),
    });
    let branch = match target_branch.as_ref().or(args.branch.as_ref()) {
        Some(b) => b.clone(),
        None => {
            if onto_commit.is_none() && cur_is_session {
                cur.clone().unwrap()
            } else {
                // A fresh claim requires an explicit -b: a guessed name is forgotten by
                // tomorrow, and it is what goes into the sharing link. The `main` file line is
                // even less something to pick on the user's behalf.
                let suggested = format!("{}-{}", agent, crate::domain::link::short(&lk.session_id));
                ui::error("claiming a fresh session line needs -b <branch>.");
                ui::hint(&format!(
                    "e.g. `agit import {} -n {agent} -b {suggested}`",
                    link::short(&lk.session_id)
                ));
                return Ok(Placed::Refused(ExitCode::Usage));
            }
        }
    };

    // Only the name of a branch about to be **created** goes through the prefix check: an
    // existing branch is a fact on the ground, and stopping it only leaves a line that already
    // exists unable to settle from then on.
    if !repo.has_ref(&format!("refs/heads/{branch}"))
        && let Err(e) = repo::valid_branch_name(&branch)
    {
        ui::error(&format!("{e:#}"));
        return Ok(Placed::Refused(ExitCode::Usage));
    }

    // The same session already hangs on another branch: changing the branch reroutes every
    // settlement from here on, which is not something a re-run of import does silently. Ask; when
    // asking is impossible (no tty and no `-y`), refuse.
    if let Some(prev) = claimed_elsewhere(lk, agent, &branch) {
        ui::warning(&format!(
            "session {} is already claimed on {prev}; `-n {agent} -b {branch}` would re-route its future settlements",
            link::short(&lk.session_id)
        ));
        if std::env::var_os("AGIT_YES").is_none() {
            match ui::prompt::confirm(&format!("re-claim it onto `{branch}`?"), false)? {
                Some(true) => {}
                Some(false) => {
                    println!("cancelled.");
                    return Ok(Placed::Refused(ExitCode::Policy));
                }
                None => {
                    ui::error("refusing to move the claim without confirmation");
                    ui::hint(&format!(
                        "re-run against `{prev}` to settle where it already lives, or pass `-y` to move it"
                    ));
                    return Ok(Placed::Refused(ExitCode::Interactive));
                }
            }
        }
    }

    // Only once the name is decided does the disk get touched: a repo created here gets its
    // `main` file line first.
    //
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
            Ok(repo
                .git(&["rev-parse", "--verify", &format!("{base}^{{commit}}")])?
                .trim()
                .to_string())
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
    let _claim_guard = link::lock(store, &lk.source, &lk.session_id)?;
    let prev_link = link::get(store, &lk.source, &lk.session_id);

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
    // An old link with no owner is not "no destination": that is the legacy link form whose
    // namespace comes from the signed-in account, and the comparison fills it in the same way —
    // from the `author` snapshot taken at the start of the command, not by reading the
    // credentials again here (a sign-in identity swapped mid-import would make the comparison
    // drift).
    let prev_owner = lk.owner.as_deref().unwrap_or(author);
    let rerouted = lk.branch.as_deref().is_some_and(|b| b != branch)
        || lk.agent.as_deref().is_some_and(|a| a != agent)
        || prev_owner != owner;
    if rerouted && !(created && onto_commit.is_some()) {
        lk.baseline_bytes = None;
        lk.baseline_hash = None;
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
        claimed_path,
        claimed_bytes,
    })))
}

/// Returns the destination (`agent@branch`) when the link already hangs on a **different** one;
/// `None` for the same destination or for no claim yet. A destination is the (agent, branch)
/// pair: the same branch name under a different agent is a reroute too.
fn claimed_elsewhere(lk: &Link, agent: &str, branch: &str) -> Option<String> {
    let prev_branch = lk.branch.as_deref()?;
    let prev_agent = lk.agent.as_deref().unwrap_or(agent);
    (prev_agent != agent || prev_branch != branch).then(|| format!("{prev_agent}@{prev_branch}"))
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
            Ok(Pick::Explained(ExitCode::Failure))
        }
        1 => Ok(Pick::One(found.into_iter().next().unwrap())),
        n => {
            // An ambiguous prefix must error — importing the wrong session mixes unrelated
            // things into the agent's memory.
            ui::error(&format!("`{selector}` matches {n} sessions:"));
            for f in found.iter().take(8) {
                println!("  {:12} {}", f.runtime, link::short(&f.session_id));
            }
            ui::hint("give a longer prefix");
            Ok(Pick::Explained(ExitCode::Usage))
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
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("$HOME is not set — can't place the scrubbed copy"))?;
    let dir = std::path::PathBuf::from(home)
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
fn pick_here(store: &Store) -> crate::Result<Pick> {
    let Some(repo) = config::repo_root().or_else(|| std::env::current_dir().ok()) else {
        ui::error("can’t determine the current directory.");
        ui::hint("be explicit: agit import <session-id> -n <name>");
        return Ok(Pick::Explained(ExitCode::Usage));
    };

    let known: std::collections::HashSet<String> = link::list(store)
        .into_iter()
        .map(|l| l.session_id)
        .collect();

    let sp = ui::spinner("looking for sessions under this directory…");
    let mut cands: Vec<(&'static str, std::path::PathBuf, String)> = vec![];
    for rt in adapter::RUNTIMES {
        let Ok(ad) = adapter::get(rt) else { continue };
        for sr in ad.sessions_for(&repo).unwrap_or_default() {
            if !known.contains(&sr.id) {
                cands.push((ad.id(), sr.path, sr.id));
            }
        }
    }
    sp.finish_and_clear();

    let here = repo.to_string_lossy().to_string();

    // Exactly one candidate is not a question. Asking a question with a single answer wastes the
    // user's time — and to make that one recognizable, the question would have to read a file
    // (see the comment below).
    if let [(rt, _, id)] = cands.as_slice() {
        return Ok(Pick::One(Found {
            runtime: rt,
            session_id: id.clone(),
            cwd: Some(here),
        }));
    }

    if cands.is_empty() {
        println!(
            "{}",
            ui::dim(&format!(
                "no unadopted sessions under {}.",
                ui::tilde(&repo)
            ))
        );
        ui::hint(
            "session ran in another directory? give the id directly: agit import <session-id> -n <name>",
        );
        return Ok(Pick::Explained(ExitCode::Ok));
    }

    // The list shows the opening prompt so the user can tell which session is which.
    //
    // On the Codex side `first_user_message` comes straight from the `threads` table, with no
    // file opened. Claude Code has no equivalent index, so the file has to be read — the **only**
    // exception to "listing must not parse transcripts", because without the prompt a column of
    // uuids means nothing to the user in an interactive list. Two bounds hold it down: only the
    // unadopted candidates under the current directory, and only when the list is really shown
    // (the single-candidate fast path above skips even this one).
    let labels: Vec<String> = cands
        .iter()
        .map(|(rt, p, id)| {
            let gist = gist_for(rt, id, p);
            format!("{rt:12} {}  \"{gist}\"", link::short(id))
        })
        .collect();
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

    match ui::prompt::select("which session to adopt?", &refs)? {
        Some(i) => {
            let (rt, _, id) = &cands[i];
            Ok(Pick::One(Found {
                runtime: rt,
                session_id: id.clone(),
                cwd: Some(here),
            }))
        }
        None => {
            // Nothing to ask with when non-interactive — list them and let the user be
            // explicit; never guess.
            ui::error("multiple candidates, nothing interactive to ask with.");
            for l in labels.iter().take(12) {
                println!("  {l}");
            }
            ui::hint("be explicit: agit import <session-id> -n <name>");
            Ok(Pick::Explained(ExitCode::Usage))
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
    let s = ui::theme::symbols();
    let was_tracked = existing.is_some();

    let mut lk = existing.unwrap_or_else(|| Link::new(found.runtime, &found.session_id, None));
    if lk.cwd.is_none() {
        lk.cwd = found.cwd.clone();
    }
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
fn gist_for(runtime: &str, session_id: &str, path: &Path) -> String {
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

    /// A re-run onto the same `agent@branch` is idle; a different branch *or* a different
    /// agent counts as moving the claim. An implementation that compared only the branch
    /// would let `-n other-agent -b work` re-route the session without a question, and
    /// one that flagged every re-run would make the documented "settle later with
    /// `agit commit`" flow ask a question it cannot answer.
    #[test]
    fn a_claim_is_elsewhere_when_agent_or_branch_differs() {
        let mut lk = Link::new("codex", "AB", Some(Path::new("/repo/one")));
        assert_eq!(claimed_elsewhere(&lk, "photo", "work"), None);
        lk.branch = Some("work".into());
        assert_eq!(claimed_elsewhere(&lk, "photo", "work"), None);
        assert_eq!(
            claimed_elsewhere(&lk, "photo", "other").as_deref(),
            Some("photo@work")
        );
        lk.agent = Some("photo".into());
        assert_eq!(claimed_elsewhere(&lk, "photo", "work"), None);
        assert_eq!(
            claimed_elsewhere(&lk, "notes", "work").as_deref(),
            Some("photo@work")
        );
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
