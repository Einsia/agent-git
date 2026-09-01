//! Subcommand implementations.
//!
//! # One file per subcommand
//!
//! To find out how `agit push` works, open `commands/push.rs` and nothing else — argument
//! definitions, execution logic, output format and tests all live there. No jumping between a few
//! large files.
//!
//! Every file has the same structure:
//!
//! ```ignore
//! pub struct Args { ... }     // clap arguments
//! pub fn run(args) -> ...     // the single entry point
//! fn <private helper>         // serves this command only
//! mod tests                   // tests for this command
//! ```
//!
//! # What the command layer does not do
//!
//! No business logic (that is `domain/`), no reading environment variables directly (that is
//! `infra::config`), no hand-rolled terminal escapes (that is `ui/`). A command file over 200
//! lines usually means logic that belongs further down.

// Authentication
pub mod config;
pub mod login;
pub mod logout;
pub mod whoami;

// Repositories
pub mod clone;
pub mod init;
pub mod repo;
pub mod run;

// Adoption and status
pub mod branch;
pub mod context;
pub mod import;
pub mod json;
pub mod status;
pub mod switch;
pub mod target;
pub mod upgrade;

// plumbing (the low-level git surgery fork/merge/seal share)
pub mod plumbing;
pub mod rc;
pub mod worktree;

// Recording
pub mod commit;
pub mod memory;
pub mod tag;

// Inspection
pub mod diff;
pub mod log;
pub mod show;
pub mod view;

// Forking and continuing
pub mod fork;
pub mod new;
pub mod resume;

// Merging
pub mod cherry_pick;
pub mod merge;
pub mod revert;

// Remote
pub mod fetch;
pub mod pull;
pub mod push;

// Discovery and sharing
pub mod pr;
pub mod search;
pub mod share;

// Export and governance
pub mod doctor;
pub mod export;
/// runtime hook entry point (hidden).
pub mod hooks;
/// stdio MCP server (hidden).
pub mod mcp;
pub mod migration;
pub mod scan;
#[path = "secrets.rs"]
pub mod secret_vault;
pub mod setup;
pub(crate) mod skill_bundle;

use crate::domain::secrets;
use crate::{ExitCode, Result};

pub type CmdResult = Result<ExitCode>;

/// Require sign-in, otherwise give an actionable next step.
///
/// Several commands need this precondition, so the wording lives in one place.
pub fn require_login() -> Result<crate::hub::Client> {
    let c = crate::hub::Client::from_env();
    if !c.has_token() {
        anyhow::bail!(
            "not logged in to {} yet.\n  \
             Run `agit login` first.",
            c.base()
        );
    }
    Ok(c)
}

/// Parse the `<owner>/<name>` form.
pub fn parse_slug(s: &str) -> Result<(String, String)> {
    let s = s.trim();
    match s.split_once('/') {
        Some((o, n)) if !o.is_empty() && !n.is_empty() && !n.contains('/') => {
            Ok((o.to_string(), n.to_string()))
        }
        _ => anyhow::bail!("use the <owner>/<agent> form, e.g. einsia/payments (got: {s})"),
    }
}

/// Whether I may write to `<owner>/<name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writability {
    /// My own namespace.
    Mine,
    /// Someone else's namespace, but the hub's write gate allows it (an org owner, or a member
    /// granted access to this agent).
    Granted,
    /// The agent is on the hub and you can see it, but the write gate does not allow it.
    ReadOnly,
    /// The hub has no such agent (or it is private and you cannot see it), and `owner` is an org
    /// you own: a first push creates it under that org.
    Creatable,
    /// The hub has no such agent (or you cannot see it), and you are not entitled to create a
    /// repo under `owner` either.
    Missing,
}

/// My own name is writable outright; any other name is a question for the hub, never inferred
/// from an org member list.
///
/// The two questions are asked separately, because the hub's write gate answers "exists but not
/// writable" and "does not exist" with the same 404: first ask whether the agent exists at all
/// with the readable `GET /api/agents/{owner}/{name}`, and if it does, probe the write gate (org
/// owners and team members granted access to this agent all sit inside that one decision); if it
/// does not, ask whether I am an owner of that org — only an owner can create an agent under an
/// org.
///
/// Network and auth errors propagate unchanged: conflating a timeout with "no permission" makes
/// `import` land a version locally and makes `push` offer to copy the repo — both decide for the
/// user on a question that has no answer.
pub fn writability(me: &str, owner: &str, name: &str) -> Result<Writability> {
    if owner == me {
        return Ok(Writability::Mine);
    }
    let client = crate::hub::Client::from_env();
    if !client.has_token() {
        anyhow::bail!(
            "{owner}/{name} is not your namespace — sign in (`agit login`) so the hub can say whether you may write to it"
        );
    }
    match client.get_agent(owner, name) {
        Ok(agent) => Ok(match client.push_access(owner, name, &agent.agent_id)? {
            crate::hub::PushAccess::Writable => Writability::Granted,
            crate::hub::PushAccess::ReadOnly | crate::hub::PushAccess::Missing => {
                Writability::ReadOnly
            }
        }),
        Err(e) if is_not_found(&e) => match client.get_org(owner) {
            Ok(org) if org.role == "owner" => Ok(Writability::Creatable),
            Ok(_) => Ok(Writability::Missing),
            Err(e) if is_not_found(&e) => Ok(Writability::Missing),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::hub::client::ApiError>()
        .is_some_and(|api| api.status == 404)
}

/// Account names and org names are lowercase on the hub. A different spelling lands in another
/// local directory and queries another remote path, so it is refused at the entry point rather
/// than guessed at.
pub fn canonical_owner(owner: &str) -> Result<()> {
    if owner.chars().any(|c| c.is_ascii_uppercase()) {
        anyhow::bail!(
            "owner names are lowercase on the hub — write `{}`",
            owner.to_ascii_lowercase()
        );
    }
    Ok(())
}

/// What to do about one secret hit — **the remedies that branch on the carrier**.
///
/// The test for branching is [`secrets::Source`]: the same advice on a different carrier is not
/// "less convenient", it is **impossible to follow**. When the hit is inside a commit / tag
/// object, that line is not in the working tree and there is no line to write
/// `agit:allow-secret` on.
///
/// This enum exists for two reasons: to make the branching testable, and to make `agit push` and
/// `agit scan` take the **same** decision. Written once on each side they drift apart — scan
/// branching on `source` while push unconditionally promises an inline annotation, and push is
/// the gate that actually stops people.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretRemedy {
    /// Write `agit:allow-secret` on that line. Only a hit in a working-tree file can reach it.
    InlineAnnotation,
    /// Locate the blob object and rewrite the commits carrying it.
    RewriteBlob,
    /// Rewrite the commit object.
    RewriteCommit,
    /// Re-create the annotated tag.
    RetagObject,
}

/// Which remedies this batch of hits earns. A pure function; the return order is the print order.
///
/// The three object remedies come last: they are the only actions that work on a hit inside an
/// object, and the most expensive.
pub fn secret_remedies(sources: impl IntoIterator<Item = secrets::Source>) -> Vec<SecretRemedy> {
    let (mut file, mut blob, mut commit, mut tag) = (false, false, false, false);
    for s in sources {
        match s {
            secrets::Source::File => file = true,
            secrets::Source::BlobObject => blob = true,
            secrets::Source::CommitObject => commit = true,
            secrets::Source::TagObject => tag = true,
        }
    }
    let mut out = Vec::new();
    if file {
        out.push(SecretRemedy::InlineAnnotation);
    }
    if blob {
        out.push(SecretRemedy::RewriteBlob);
    }
    if commit {
        out.push(SecretRemedy::RewriteCommit);
    }
    if tag {
        out.push(SecretRemedy::RetagObject);
    }
    out
}

/// Print the remedies [`secret_remedies`] selected.
///
/// **`push` and `scan` both call this one.** A copy is where the two start to drift apart: same
/// repo, `agit scan --secrets` branching by carrier and getting the advice right while
/// `agit push` promises, for a hit inside an object, an inline annotation the user has nowhere to
/// write.
pub fn hint_secret_remedies(sources: impl IntoIterator<Item = secrets::Source>) {
    for r in secret_remedies(sources) {
        crate::ui::hint(&remedy_hint(r));
    }
}

/// The rule that matched takes part in the remedy too: an explicitly registered rule accepts
/// neither an allowlist nor an inline annotation.
///
/// The object-rewrite remedies hold for both kinds of rule; only a built-in heuristic hit in the
/// working tree can be allowed through with `agit:allow-secret`. Keeping that decision in one
/// place is what stops push and scan from each promising a way out a registered rule ignores.
pub fn hint_secret_hit_remedies<'a>(hits: impl IntoIterator<Item = &'a secrets::Hit>) {
    let mut registered = false;
    let mut sources = Vec::new();
    for hit in hits {
        if hit.rule == "registered-secret" {
            registered = true;
            if hit.source != secrets::Source::File {
                sources.push(hit.source);
            }
        } else {
            sources.push(hit.source);
        }
    }
    if registered {
        crate::ui::hint(
            "· registered-secret rules are explicit and ignore allowlists: remove the value from the content, or review local rule labels with `agit secrets list` and unregister one only if it is no longer a secret",
        );
    }
    hint_secret_remedies(sources);
}

/// The body of each remedy.
///
/// Split out from printing so the **text itself** can be asserted on. All of the value here is in
/// "follow it and it is fixed", and parts of the body are not wording: drop the step that saves
/// the original target from the tag remedy and it goes from "executable" to **executable and
/// harmful** (see the assertion in `mod tests`).
fn remedy_hint(r: SecretRemedy) -> String {
    match r {
        SecretRemedy::InlineAnnotation => format!(
            "· inline annotation (working-tree files only): put `{}` on that line",
            secrets::INLINE_PRAGMA
        ),
        // The blob remedy **cannot** fall back to "add an inline annotation": the line that hit
        // need not exist in the working tree (the file may have been deleted in a later commit,
        // or the whole tree may have been swapped for a clean replacement object through
        // `refs/replace/*`), and a user facing a working tree that does not contain that string
        // has nowhere to start. So this one gives the two things that can actually be done:
        // **locate** it by oid (read the content, find which commits carry it), then **rewrite**
        // those commits.
        //
        // The last sentence is not wording either: deleting the file in a new commit is the most
        // natural reaction and it fixes nothing — the old blob stays reachable and still ships
        // with this push. Left unsaid, the user deletes it, is stopped by the same gate again,
        // and concludes the gate is broken.
        SecretRemedy::RewriteBlob => "· some hits are inside blob objects — file content that lives in history, not necessarily in your working tree (the file may have been deleted in a later commit), so there is no line to annotate. the report locates it as `blob object <oid>/<path>`: read it with `git cat-file blob <oid>` (the reported line number counts from there) and find the commits carrying it with `git log --all --find-object=<oid>`, then rewrite those: `git commit --amend` if only the tip has it, `git rebase -i <sha>~1` for anything older, or `git filter-repo --strip-blobs-with-ids <file-of-oids>` to drop it across the whole history. deleting the file in a new commit does not help — the old blob stays reachable and still ships".to_string(),
        SecretRemedy::RewriteCommit => "· some hits are inside commit objects — those lines are not in your working tree, so neither the inline annotation nor `agit revert` reaches them. locate one with `git cat-file commit <sha>` (the reported line number counts from there), then rewrite the commit: `git commit --amend` for the tip, `git rebase -i` for anything older".to_string(),
        // Re-creating it **must** carry the original target: `git tag -a <name>` with no target
        // tags HEAD, so following the advice to change a tag's message silently moves a released
        // version off the commit it pointed at and onto HEAD. The command that reads the target
        // must also run **before** `git tag -d` — once the tag is deleted, that name no longer
        // resolves to the commit it named.
        // `%(object)` and **not** `rev-parse '<name>^{}'`: the latter peels **recursively** down
        // to a commit. When the outer tag points straight at another annotated tag (a nested tag;
        // git warns about it but allows it), `^{}` gives the final commit, and rebuilding from
        // that silently swaps the outer tag's `object` from the inner tag OID to the commit OID —
        // the tag topology is changed. `%(object)` gives the **direct** target, peeling nothing.
        SecretRemedy::RetagObject => "· some hits are inside annotated tag objects. locate one with `git cat-file tag <sha>` (the reported line number counts from there), then re-create the tag on whatever it already points at. save that target first, before the delete: `target=$(git for-each-ref --format='%(object)' 'refs/tags/<name>')` — then `git tag -d <name> && git tag -a <name> $target`. two things are not optional: the target (`git tag -a <name>` with nothing after it tags HEAD, silently moving a released version onto whatever you have checked out), and using `%(object)` rather than `<name>^{}` (the latter peels recursively, so a tag pointing at another tag would be rebuilt pointing at the commit instead). no history rewrite needed, the tag is a separate object".to_string(),
    }
}

/// Say "this part I did not scan" out loud.
///
/// **`push` and `scan` both call this one**, for the reason given in [`hint_secret_remedies`].
///
/// # Why it must be as loud as "a secret was found"
///
/// What was never scanned and what is clean look identical in an empty hit list. Silently
/// returning "clean scan" is fail open — a gate allowing the input it cannot reach, and saying
/// nothing about it. So this prints both the reason and the next step the user can actually take;
/// the verdict is the caller's, by the same rule (stop, unless `AGIT_ALLOW_SECRETS`).
pub fn report_unscanned(u: &secrets::Unscanned) {
    if let Some((bytes, budget)) = u.over_budget {
        // All three words are deliberate.
        //
        // **surface** rather than history: the overrun can happen on the working-tree pass (no
        // commit needed at all), and "history" sends the reader digging through history that has
        // nothing to do with it.
        //
        // **at least**: that number is a lower bound — at the moment of the overrun what was left
        // was never counted, see [`secrets::Unscanned::over_budget`]. Reporting it as the total
        // invents a number nobody ever counted.
        //
        // **not fully**: the part read before the overrun really was scanned. "NOT scanned" would
        // make the user think that half went unchecked too, and rerun to wait for an answer that
        // will not change.
        crate::ui::error(&format!(
            "this publish surface is larger than the local scan budget (at least {} vs {}) — it was NOT fully scanned.",
            mib(bytes),
            mib(budget)
        ));
        // Both of these have to be **true**. The scan surface is "every object reachable from
        // the local branches" minus "what the destination already has", so `-b` does not shrink
        // it (`-b` picks which refs to push, not which objects to scan) — "push one branch at a
        // time" is advice that goes nowhere. The two real levers are: delete local branches you
        // never intend to publish (shrink the minuend), and do not hoard — publish a little every
        // time and the destination side keeps growing (shrink the difference).
        crate::ui::hint(
            "· the surface is everything reachable from your local branches minus what the destination already has — deleting local branches you never publish shrinks it",
        );
        crate::ui::hint(
            "· publishing regularly keeps each run small; a first publish of a very long history is the expensive case",
        );
        // Both levers above act on the history half only. The working-tree pass spends the same
        // budget, and the shape that blows it there (a pile of uncommitted logs / build output)
        // is immune to both: deleting every branch does not shrink it by one byte. Without this
        // line the user deletes branches as advised, reruns, and sees exactly the same number.
        crate::ui::hint(
            "· the budget also covers your working tree — untracked logs or build output can eat it on their own; move them out of the repo and scan again",
        );
        crate::ui::hint(
            "· the server scans on its own when content becomes readable by others, so going public later may still be refused",
        );
    }
    if !u.oversized.is_empty() {
        crate::ui::error(&format!(
            "{} object(s) exceed the per-object read limit and were NOT scanned.",
            u.oversized.len()
        ));
        for (oid, n) in u.oversized.iter().take(5) {
            crate::ui::hint(&format!("· {oid} ({})", mib(*n)));
        }
        // The hub **refuses** the same thing rather than skipping it (`blob_bytes`, with the
        // same default), so it is stopped here too — said early, the user can still deal with the
        // content before it enters history.
        crate::ui::hint(
            "· an object this large is refused by the hub too — inspect it with `git cat-file -p <oid>` and rewrite the history that carries it",
        );
    }
    // The working-tree ledger is reported **separately**. The handle is a path, not an oid, and
    // the next action is entirely different: the file has most likely never entered history (in
    // practice an uncommitted `artifact.log`), so moving or deleting it is the whole fix and no
    // history rewrite is needed. Reported together, the user walks into the dead end of
    // `git cat-file -p <path>` and the sentence that works has nowhere to go.
    if !u.oversized_files.is_empty() {
        crate::ui::error(&format!(
            "{} working-tree file(s) exceed the per-file read limit and were NOT scanned.",
            u.oversized_files.len()
        ));
        for (path, n) in u.oversized_files.iter().take(5) {
            crate::ui::hint(&format!("· {path} ({})", mib(*n)));
        }
        crate::ui::hint(
            "· these are files as they sit in your working tree — move them out of the repo (or delete them) and scan again; nothing to rewrite unless they are also committed",
        );
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Ask the destination once: **where this repo's next push goes, and what is already there**.
///
/// **`push` and `scan` both call this one.** The two local gates must reach the same verdict, the
/// verdict depends on the scan surface, and the scan surface depends on this answer; computed
/// separately the verdicts drift apart sooner or later, and the direction of the drift is always
/// one of them under-scanning.
///
/// # The test: `origin` is this destination, and the far side says so itself
///
/// None of the three steps can be dropped:
///
/// 1. **`origin` must point at the hub this push goes to.** With `AGIT_HUB_URL=… agit push`
///    switching hubs ([`crate::hub`] recommends exactly that usage), `origin` still points at the
///    hub before it — asking that one is asking the wrong party, and it answers "I have all of
///    these" with full confidence.
/// 2. **The agent on `origin` must be the one being published now** ([`lands_on`]: owner and name
///    both have to match). A read-only checkout's `origin` points, by repo contract, at the
///    **original author**'s copy, while `agit push` always publishes under **your** name
///    (`ensure_remote(client, me, …)` after `promote_if_read_only`, see
///    [`crate::commands::push`]); the name half stops **another** agent under the same account.
///    Missing either half, history objects the far side already owns and this destination does
///    not are subtracted wholesale by `--not`: `agit scan` / `agit push --dry-run` report clean,
///    and only the server's full rescan on a real push refuses — handing back that verdict early
///    is the one thing the two local gates are for, and they hand back the opposite one.
/// 3. **Only the refs the far side reports itself count.** Local `refs/remotes/origin/*` records
///    the result of the preceding fetch/push and does not change by one character once the remote
///    is deleted and rebuilt (`agit repo delete` defaults "delete the local copy too?" to false,
///    so the local tracking refs stay).
///
/// A step that cannot be answered gives [`secrets::Destination::Unknown`] — scan the full
/// history. That direction is the right one: scanning twice is only slow, scanning too little
/// ships the secret.
///
/// # Why the test lives **here** and not in `push`
///
/// Because "may the destination shrink the scan surface" is one question, not two. Written into
/// [`crate::commands::push`], the `agit scan` path does not have it — and these two gates must
/// reach the same verdict. `--dry-run` is a third site: it returns **before** the read-only
/// promotion, so the `origin` it sees is never the real destination. At the one place that
/// produces the narrowed surface, none of the three has to remember to run the test itself.
///
/// That "one" has a test watching it, see `only_one_place_can_narrow_the_scan_surface`.
///
/// # What `agent` is
///
/// The name of the agent being published to, supplied by the caller: `agit push` takes it from
/// the checkout it selected (`checkout.name`, the same name `ensure_remote` creates or fetches),
/// `agit scan` from the slug it resolved. It is the third segment of the destination identity —
/// it **must not** be derived here from `origin`, which is handing the suspect URL its own alibi.
pub fn publish_destination(repo: &crate::domain::repo::Repo, agent: &str) -> secrets::Destination {
    let Some(url) = repo.remote_url() else {
        return secrets::Destination::Unknown; // No origin yet: this push sends everything.
    };
    let base = crate::infra::config::hub_url();
    let me = crate::infra::credentials::current_user();
    if !lands_on(&url, &base, me.as_deref(), agent) {
        return secrets::Destination::Unknown; // origin does not describe this destination.
    }
    let Some(owner) = me.as_deref() else {
        return secrets::Destination::Unknown;
    };
    let client = crate::hub::Client::from_env();
    if crate::hub::identity::verify_slug(repo, &client, owner, agent).is_err() {
        return secrets::Destination::Unknown; // slug reused, legacy checkout, or no answer now.
    }
    let Some(tips) = crate::hub::git::ls_remote_heads(repo.root(), &url) else {
        return secrets::Destination::Unknown; // Unanswerable (offline, no access, agent gone).
    };
    secrets::Destination::advertised(repo, tips).unwrap_or(secrets::Destination::Unknown)
}

/// Whether this push really lands on `remote_url`.
///
/// The destination identity is the `(hub, owner, name)` **triple**; all three segments are
/// necessary, and the only safe direction to be wrong in is "no". Saying yes means asking the far
/// side "what do you already have" and subtracting that from the scan surface — the cost of
/// asking the wrong party is a whole stretch of history silently escaping the scan.
///
/// Each of the three catches a shape that **exists normally in the product**:
///
/// * **hub**: `AGIT_HUB_URL=… agit push` switches hubs ([`crate::hub`] recommends that usage) and
///   `origin` still points at the one before.
/// * **owner**: the read-only checkout `agit clone alice/photo` leaves behind, whose `origin`
///   points at the original author.
/// * **name**: another agent under the same account. Skipping this half on the grounds that
///   pointing at the wrong agent is a misconfiguration caught by the re-check after `agit push`
///   writes does not hold — that re-check is step 6b of [`crate::commands::push`], and
///   `agit scan` and `agit push --dry-run` **both return before it**. Two of the three gates
///   cannot reach that backstop at all, so the cost of the misconfiguration is not borne by
///   whoever misconfigured it; it is those two silently reporting a false clean.
fn lands_on(remote_url: &str, hub: &str, me: Option<&str>, agent: &str) -> bool {
    // Not signed in means there is no "my namespace" — the destination is unanswerable, so scan
    // everything.
    let Some(me) = me else { return false };
    same_hub(remote_url, hub)
        && remote_slug(remote_url) == Some((me.to_string(), agent.to_string()))
}

/// The `(owner, name)` inside a hub address: the last two segments of `<hub>/<owner>/<name>.git`.
///
/// Clone / push addresses are assembled by the server (`<public_url>/<owner>/<name>.git`) and the
/// hub itself may sit under a subpath, so this counts the **last** two segments, not the first
/// two. Unrecognized gives `None`, and the caller falls back to the full history.
///
/// Both segments are parsed together rather than split into two functions: they come out of one
/// split of one URL, and two functions means the same parsing written twice — with no symptom at
/// all on the day they drift, since `lands_on` would simply compare one segment fewer.
fn remote_slug(url: &str) -> Option<(String, String)> {
    let after_scheme = url.split_once("://")?.1;
    let path = after_scheme.split(['?', '#']).next()?;
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    segs.next()?; // host[:port]
    let segs: Vec<&str> = segs.collect();
    // Without at least the `<owner>/<name>` segments this address is not an agent address at all.
    if segs.len() < 2 {
        return None;
    }
    let owner = segs[segs.len() - 2].to_string();
    // `.git` is the suffix the server appends when it assembles the address, not part of the
    // name. When it is absent, do not strip anyway (`strip_suffix` leaves the string unchanged
    // when it does not match), so an agent genuinely named `x.git` is not parsed as `x`.
    let name = segs[segs.len() - 1];
    let name = name.strip_suffix(".git").unwrap_or(name).to_string();
    (!owner.is_empty() && !name.is_empty()).then_some((owner, name))
}

/// Whether this remote address sits on the hub this run uses.
///
/// It compares the authority after the scheme (host + port), not the whole URL: clone addresses
/// are assembled by the server (`<public_url>/<owner>/<name>.git`), so the path part is supposed
/// to differ.
///
/// When in doubt, return false — the two errors differ by an order of magnitude: scanning the
/// full history again is only slow, while treating another hub's tracking refs as this
/// destination drops a whole stretch of history outright.
fn same_hub(remote_url: &str, hub: &str) -> bool {
    let authority = |u: &str| -> Option<String> {
        let rest = u.split_once("://")?.1;
        let host = rest.split(['/', '?', '#']).next()?;
        // The address may have been configured by hand with credentials; in `user:pass@host`
        // only the host half counts.
        Some(host.rsplit('@').next()?.to_ascii_lowercase())
    };
    match (authority(remote_url), authority(hub)) {
        (Some(a), Some(b)) => !a.is_empty() && a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    /// A push probe has exactly three answers; any other status code is not an answer. An
    /// implementation reading a 5xx as "no permission" offers to copy the repo whenever the hub
    /// wobbles.
    #[test]
    fn a_push_probe_has_exactly_three_answers() {
        use crate::hub::PushAccess;
        assert_eq!(PushAccess::from_status(200), Some(PushAccess::Writable));
        assert_eq!(PushAccess::from_status(403), Some(PushAccess::ReadOnly));
        assert_eq!(PushAccess::from_status(404), Some(PushAccess::Missing));
        assert_eq!(PushAccess::from_status(500), None);
        assert_eq!(PushAccess::from_status(302), None);
    }

    /// A non-lowercase owner is refused at the entry point: names on the hub are lowercase, and
    /// allowing one only makes the local directory and the remote path use different spellings.
    #[test]
    fn owners_must_be_written_in_their_canonical_lowercase() {
        assert!(super::canonical_owner("einsia").is_ok());
        assert!(super::canonical_owner("a-b_1").is_ok());
        assert!(super::canonical_owner("Einsia").is_err());
    }

    use super::{SecretRemedy, secret_remedies};
    use crate::domain::secrets;
    use crate::domain::secrets::Source;

    /// Once the hub changes, the old `origin` is no longer the destination.
    ///
    /// `AGIT_HUB_URL=… agit push` is the usage the product docs recommend, and this test is the
    /// only thing separating it from "the tracking refs describe the remote before it".
    #[test]
    fn a_remote_on_another_hub_is_not_this_push_target() {
        assert!(super::same_hub(
            "https://hub.corp.com/me/photo.git",
            "https://hub.corp.com"
        ));
        assert!(super::same_hub(
            "http://127.0.0.1:8177/me/photo.git",
            "http://127.0.0.1:8177/"
        ));
        // The credentials segment takes no part in the comparison; the port does.
        assert!(super::same_hub(
            "https://x:tok@hub.corp.com/me/p.git",
            "https://hub.corp.com"
        ));
        assert!(!super::same_hub(
            "http://127.0.0.1:8177/me/photo.git",
            "http://127.0.0.1:9999"
        ));
        assert!(!super::same_hub(
            "https://hub.corp.com/me/photo.git",
            "https://hub.example.org"
        ));
        // An unrecognized address (ssh shorthand, a local path) is never the same hub: better to
        // scan everything.
        assert!(!super::same_hub(
            "git@github.com:me/p.git",
            "https://github.com"
        ));
        assert!(!super::same_hub("/tmp/hub.git", "https://hub.corp.com"));
    }

    /// A read-only checkout's `origin` is **not** this push's destination.
    ///
    /// The checkout `agit clone alice/photo` (without `--mine`) leaves behind has an `origin`
    /// pointing, by repo contract, at alice's copy; `agit push` always publishes under your name
    /// (after promotion `ensure_remote` takes `me`). A mismatched owner is asking the wrong
    /// party, and the scan surface must not shrink by it.
    #[test]
    fn a_read_only_checkout_is_not_this_push_target() {
        const HUB: &str = "https://hub.corp.com";
        // Your own copy, with the name matching too: this is the destination, and shrinking the
        // scan surface is right. A promoted copy has the same shape (`origin` points at your
        // copy, `upstream` is the one pointing at alice), so it still shrinks — otherwise every
        // push would rescan the full history for nothing.
        assert!(super::lands_on(
            &format!("{HUB}/me/photo.git"),
            HUB,
            Some("me"),
            "photo"
        ));
        // Read-only checkout: `origin` points at the original author.
        assert!(!super::lands_on(
            &format!("{HUB}/alice/photo.git"),
            HUB,
            Some("me"),
            "photo"
        ));
        // Another agent under the same account: a matching owner is not enough, see
        // `another_agent_on_the_same_account_is_not_this_push_target`.
        assert!(!super::lands_on(
            &format!("{HUB}/me/other.git"),
            HUB,
            Some("me"),
            "photo"
        ));
        // Not signed in: no "my namespace", so the destination is unanswerable.
        assert!(!super::lands_on(
            &format!("{HUB}/me/photo.git"),
            HUB,
            None,
            "photo"
        ));
        // Hub changed: matching owner and name is not enough, all three must hold.
        assert!(!super::lands_on(
            &format!("{HUB}/me/photo.git"),
            "https://other.corp.com",
            Some("me"),
            "photo"
        ));

        // With the hub under a subpath, `(owner, name)` is still the **last** two segments.
        assert_eq!(
            super::remote_slug("https://hub.corp.com/git/alice/photo.git"),
            Some(("alice".into(), "photo".into()))
        );
        // `.git` is the suffix the server appends; it is not part of the name, and the address
        // parses without it.
        assert_eq!(
            super::remote_slug("https://hub.corp.com/alice/photo"),
            Some(("alice".into(), "photo".into()))
        );
        // An unrecognized address is always None, and the caller falls back to the full history.
        assert_eq!(super::remote_slug("https://hub.corp.com/photo.git"), None);
        assert_eq!(super::remote_slug("git@github.com:me/p.git"), None);
    }

    /// In a read-only checkout, `agit scan` / `agit push --dry-run` must scan the **full
    /// history**.
    ///
    /// # What this pins
    ///
    /// In a read-only checkout `agit push` **promotes** first: it creates an empty agent under
    /// your namespace and pushes the whole history to it. What goes out is therefore **all** of
    /// the history, and not one object may be subtracted.
    ///
    /// The scan asks "what does the current `origin` already have", and a read-only checkout's
    /// `origin` points at the original author's copy — which has **everything**. So the whole
    /// history is subtracted by `--not`, the scan surface collapses to the empty set, both local
    /// gates report clean, and the secret is refused only by the server's full rescan after a real
    /// push. Handing back that verdict early is the one thing the two local gates are for, and
    /// they hand back exactly the opposite one.
    ///
    /// `--dry-run` least of all can escape it: it returns **before** the read-only promotion, so
    /// the `origin` it sees is necessarily the original author's.
    #[test]
    fn a_read_only_checkout_scans_the_full_history() {
        const AWS: &str = "AKIA4X7QZ2M5RT6VW3JH";
        const HUB: &str = "http://127.0.0.1:8177";

        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);

        // The secret is **only in history**: written, committed, then deleted and committed
        // again. The working-tree pass therefore cannot see it; the only pass that can is the
        // reachable-object pass — the one `--not` subtracts from.
        std::fs::write(d.path().join("config.env"), format!("AWS_KEY={AWS}\n")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "settings"]);
        std::fs::remove_file(d.path().join("config.env")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop settings"]);
        let tip = run(&["rev-parse", "HEAD"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let verdict = |dest| {
            secrets::scan_agent_repo(&repo, &secrets::ScanPlan::to(dest))
                .expect("valid repo")
                .hits
        };

        // The real-push side: after promotion the destination is the **empty** copy under your
        // name, so the whole history goes out.
        let real_push = verdict(secrets::Destination::Unknown);
        assert!(
            !real_push.is_empty(),
            "precondition: this history must contain a secret, or the test proves nothing"
        );
        // Precondition: with the source repo's tip as `--not` the whole history really is
        // subtracted away — that is the mechanism of the bug.
        assert!(
            verdict(
                secrets::Destination::advertised(&repo, vec![tip.clone()]).expect("valid repo")
            )
            .is_empty(),
            "precondition: the source repo's tip must empty the scan surface, or the test cannot reach the bug"
        );

        // The scan surface the two local gates get in a read-only checkout — this is how
        // `publish_destination` computes it.
        let read_only_origin = format!("{HUB}/alice/photo.git");
        let dest = if super::lands_on(&read_only_origin, HUB, Some("me"), "photo") {
            secrets::Destination::advertised(&repo, vec![tip]).expect("valid repo")
        } else {
            secrets::Destination::Unknown
        };
        assert_eq!(
            verdict(dest),
            real_push,
            "in a read-only checkout the verdict of `agit scan` / `agit push --dry-run` must match a real push (both refuse): \
             origin points at the original author, not at this destination"
        );
    }

    /// The `origin` of **another** agent under the same account is not this publish's
    /// destination.
    ///
    /// # What this pins
    ///
    /// Comparing hub and owner alone, and deliberately not the name half, on the grounds that an
    /// `origin` pointing at another agent of your own is a misconfiguration rather than a shape to
    /// defend against, does not cost whoever misconfigured it — it makes **the gate fail
    /// silently**: the current checkout publishes to `me/target` while `origin` points at
    /// `…/me/other.git`, and as soon as `other` already owns this history the tips `ls-remote`
    /// reports are subtracted wholesale by `--not`, collapsing the scan surface to the empty set.
    ///
    /// The backstop re-check cannot reach here: a real push notices after `ensure_remote` that
    /// the destination changed and rescans everything (see step 6b of
    /// [`crate::commands::push`]), while `agit scan` and `agit push --dry-run` **both return
    /// before that**. So the two local gates report clean and the real push is refused by the
    /// server — handing back that verdict early is the one thing the two gates are for, and they
    /// hand back exactly the opposite one.
    ///
    /// Hence the destination identity is the `(hub, owner, name)` triple: three gates, one
    /// verdict, and one place where the test has to be right.
    #[test]
    fn another_agent_on_the_same_account_is_not_this_push_target() {
        const AWS: &str = "AKIA4X7QZ2M5RT6VW3JH";
        const HUB: &str = "http://127.0.0.1:8177";

        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);

        // The secret is **only in history**: the working-tree pass cannot see it, the only pass
        // that can is the reachable-object pass — the one `--not` subtracts from.
        std::fs::write(d.path().join("config.env"), format!("AWS_KEY={AWS}\n")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "settings"]);
        std::fs::remove_file(d.path().join("config.env")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop settings"]);
        let tip = run(&["rev-parse", "HEAD"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let verdict = |dest| {
            secrets::scan_agent_repo(&repo, &secrets::ScanPlan::to(dest))
                .expect("valid repo")
                .hits
        };

        // The real-push side: this publishes to `me/target`, and that copy does not have this
        // history yet.
        let real_push = verdict(secrets::Destination::Unknown);
        assert!(
            !real_push.is_empty(),
            "precondition: this history must contain a secret, or the test proves nothing"
        );
        // Precondition: with `other`'s tip as `--not` the whole history really is subtracted
        // away — that is the mechanism of the bug. `other` is another agent under the same
        // account, and "already owns this history" is everyday in this product (the same session
        // was forked out of it).
        assert!(
            verdict(
                secrets::Destination::advertised(&repo, vec![tip.clone()]).expect("valid repo")
            )
            .is_empty(),
            "precondition: those tips must empty the scan surface, or the test cannot reach the bug"
        );

        // The scan surface the two local gates get — this is how `publish_destination` computes
        // it, except that "ask the far side for its tips" is fed in locally here.
        let stale_origin = format!("{HUB}/me/other.git");
        let dest = if super::lands_on(&stale_origin, HUB, Some("me"), "target") {
            secrets::Destination::advertised(&repo, vec![tip]).expect("valid repo")
        } else {
            secrets::Destination::Unknown
        };
        assert_eq!(
            verdict(dest),
            real_push,
            "`origin` points at **another** agent under the same account (me/other) while this publishes to me/target: \
             history the far side already has must not be subtracted from the scan surface, or `agit scan` / `--dry-run` report clean while the real push is refused"
        );
    }

    /// **Exactly one** place may narrow the scan surface — [`super::publish_destination`].
    ///
    /// # What this pins
    ///
    /// Not "it is written correctly now" — **the next person**. This gate is broken open the same
    /// way every time: the boundary is added only in the layer being looked at (`push` gets it,
    /// `scan` does not, or the other way round), and the path that was missed has no symptom at
    /// all — it quietly scans one stretch of history less, then prints "clean scan".
    ///
    /// "The destination was computed wrong" and "the destination was never asked" differ by an
    /// order of magnitude: the latter only scans more, the former moves a whole stretch of
    /// secret-bearing history out of the scan surface. So producing a **narrowed** scan surface
    /// must have a single entry point, and then the test has one place to be right in.
    ///
    /// Scanning source for the same reason as `every_git_subprocess_disables_replace_resolution`:
    /// the contract it really states ("only this function may construct `Advertised`") cannot be
    /// stated in the type system, since anyone can build an enum variant. The cost is that it
    /// matches **text** — so it recognizes this one spelling only, and a re-spelling turns it red
    /// rather than green, which is the right direction.
    #[test]
    fn only_one_place_can_narrow_the_scan_surface() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sites: Vec<String> = vec![];
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            // The module that defines it of course names its own variant (`advertised` /
            // `narrows` / `revs` all live there). This counts **everywhere else**.
            if rel.starts_with("domain/secrets/") {
                continue;
            }
            let src = std::fs::read_to_string(path).unwrap();
            // Only the half before `#[cfg(test)]`: tests set up destinations of their own, which
            // is the scenario under test, and it also keeps this file's own literals from
            // counting themselves.
            let prod = src
                .split_once("\n#[cfg(test)]")
                .map_or(src.as_str(), |(p, _)| p);
            let n = prod.matches("Destination::advertised(").count()
                + prod.matches("Destination::Advertised(").count();
            for _ in 0..n {
                sites.push(rel.clone());
            }
        }
        assert_eq!(
            sites,
            vec!["commands/mod.rs".to_string()],
            "the places producing a scan surface narrowed by the destination must be exactly one (`publish_destination`), got {sites:?}\n\
             a second entry point means the test \"is `origin` this destination\" has to be right in both places, and the one\n\
             that is missed has no symptom at all — it only scans one stretch of history less, then prints \"clean scan\""
        );

        // "In this file" is not enough, it has to be in that function: the test and what it
        // produces must sit next to each other; one layer between them and it is back to
        // "remember to run the test in both places".
        let me = include_str!("mod.rs");
        let body = me
            .split_once("pub fn publish_destination")
            .expect("this file defines publish_destination")
            .1
            .split_once("\n}\n")
            .expect("the function body has a closing brace")
            .0;
        assert!(
            body.contains("Destination::advertised(") && body.contains("lands_on("),
            "the single production site must sit inside `publish_destination`, with the test (`lands_on`) right beside it"
        );
    }

    #[test]
    fn slug_parsing() {
        assert_eq!(
            super::parse_slug("einsia/payments").unwrap(),
            ("einsia".into(), "payments".into())
        );
        for bad in ["payments", "/x", "x/", "a/b/c", ""] {
            assert!(super::parse_slug(bad).is_err(), "`{bad}` must be rejected");
        }
    }

    /// When every hit is inside an object, an inline annotation **must not** be promised.
    ///
    /// That line is not in the working tree and the user has nowhere to write it. A gate printing
    /// it unconditionally while `agit scan` branches by carrier gives two different remedies for
    /// the same repo, one of which goes nowhere.
    ///
    /// The blob one needs the most watching: its payload **looks** most like a working-tree file
    /// (the report carries a path), while the file at that path may have been deleted long ago, or
    /// the whole tree swapped for a clean replacement object through `refs/replace/*` — someone
    /// following the path into the working tree does not find that string there at all.
    #[test]
    fn hits_that_are_all_inside_objects_get_no_inline_annotation_advice() {
        let commits_only = secret_remedies([Source::CommitObject, Source::CommitObject]);
        assert_eq!(
            commits_only,
            vec![SecretRemedy::RewriteCommit],
            "a hit inside a commit object has one way out only: rewriting history"
        );

        let blobs_only = secret_remedies([Source::BlobObject, Source::BlobObject]);
        assert_eq!(
            blobs_only,
            vec![SecretRemedy::RewriteBlob],
            "the line inside a blob object need not be in the working tree, so no inline annotation is promised"
        );

        // Mixed in, that one is still given — it is executable for the working-tree hits among
        // them.
        let mixed = secret_remedies([
            Source::CommitObject,
            Source::File,
            Source::TagObject,
            Source::BlobObject,
        ]);
        assert_eq!(
            mixed,
            vec![
                SecretRemedy::InlineAnnotation,
                SecretRemedy::RewriteBlob,
                SecretRemedy::RewriteCommit,
                SecretRemedy::RetagObject,
            ]
        );
    }

    /// The blob advice must be **executable**: locate by oid, rewrite by commit.
    ///
    /// # Why these parts are pinned
    ///
    /// "There is a blob hit" on its own offers no way out. The user holds an oid and a path, and
    /// exactly two steps are available: read that object out / find which commits carry it
    /// (`git cat-file blob`, `git log --all --find-object=`), then rewrite those commits. Without
    /// the locating half the advice becomes "work out where it is yourself"; without the rewriting
    /// half the most natural reaction is to **delete the file in a new commit** — and the old blob
    /// stays reachable and still ships with this push, so the same gate stops them again and they
    /// conclude it is broken. That sentence has to stay in the body too.
    #[test]
    fn the_blob_advice_is_executable_from_an_oid() {
        let advice = super::remedy_hint(SecretRemedy::RewriteBlob);
        assert!(
            advice.contains("git cat-file blob <oid>"),
            "the advice says how to read that object out: {advice}"
        );
        assert!(
            advice.contains("git log --all --find-object=<oid>"),
            "the advice says how to find which commits carry it: {advice}"
        );
        assert!(
            advice.contains("git commit --amend") && advice.contains("git rebase -i"),
            "locating is not enough; the advice names the rewrite action too: {advice}"
        );
        assert!(
            advice.contains("deleting the file in a new commit does not help"),
            "the advice must say that deleting the file in a new commit does not help, or the same gate stops the user again: {advice}"
        );
        assert!(
            !advice.contains(crate::domain::secrets::INLINE_PRAGMA),
            "the line of a blob hit need not be in the working tree, so no inline annotation belongs in this advice: {advice}"
        );
    }

    /// The tag advice must spell out **how to read the original target**, and pass it to
    /// `git tag -a`.
    ///
    /// `git tag -a <name>` with no target tags **HEAD**. Advice without the target is therefore
    /// harmful: following it to change a tag's message moves that tag off the commit it pointed at
    /// and onto HEAD — a released version silently relocated. That is worse than advice that
    /// cannot be followed, because it **can be followed and does harm**.
    ///
    /// The read itself has a trap: `git rev-parse '<name>^{}'` peels **recursively** down to a
    /// commit. When the outer tag points straight at another annotated tag, rebuilding it that way
    /// silently turns the outer `object` from the inner tag OID into the commit OID — the tag
    /// topology is changed. `%(object)` gives the **direct** target.
    ///
    /// So this assertion watches the three parts of the body (use the non-peeling read, pass what
    /// it read back in, say why `^{}` is not used), and the next person simplifying the wording
    /// does not delete them along with it.
    #[test]
    fn the_retag_advice_says_how_to_keep_the_tag_where_it_is() {
        let advice = super::remedy_hint(SecretRemedy::RetagObject);
        // The read has to be in the copy, and it has to be the **non-peeling** one.
        assert!(
            advice.contains("%(object)"),
            "the advice reads the direct target with `%(object)`: {advice}"
        );
        assert!(
            advice.contains("git tag -a <name> $target"),
            "the target it read is passed back in: {advice}"
        );
        // `^{}` may appear only as the **counter-example** (why it is not used), never inside
        // the command.
        assert!(
            !advice.contains("$(git rev-parse '<name>^{}')"),
            "`^{{}}` peels recursively down to a commit, so a nested tag would be rebuilt pointing at the commit: {advice}"
        );
        assert!(
            advice.contains("peels recursively"),
            "the advice says why `^{{}}` is not used, or the next person swaps it back in: {advice}"
        );
    }
}

// ────────────────────── CLI definition (shared by main and setup --completions) ──────────────────────

use clap::{Parser, Subcommand};

/// The top-level command line.
///
/// Defined **here** and not in main.rs: `setup --completions` needs the same definition to
/// generate completion scripts, and main.rs is only a shell.
#[derive(Parser)]
#[command(
    name = "agit",
    version = crate::infra::config::BUILD_VERSION,
    about = "git for agent sessions",
    long_about = "agit is git for agent sessions: a repo holds one agent's sessions plus shared \
                  memory/skills, a branch is one session, a commit is one user turn.\n\n\
                  Six verbs get you through the day: run, import, commit, push, resume, merge.",
    propagate_version = true
)]
pub struct Cli {
    /// No subcommand = `agit resume` — **but only when someone is sitting at a terminal**.
    ///
    /// Every other shape (a pipe, CI, inside an agent session) still prints help, which is
    /// `arg_required_else_help`'s own behavior. The arbitration is in main.rs, because only there
    /// is it known whether to print help or open the TUI.
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Disable colored output (auto-off in pipes and CI; this forces it).
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Emit one machine-readable JSON document for the command.
    #[arg(long, global = true)]
    pub json: bool,

    /// Skip confirmations (equivalent to answering “yes” to every prompt).
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// Quiet mode.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Act as if run in this directory.
    #[arg(short = 'C', long, global = true, value_name = "dir")]
    pub directory: Option<std::path::PathBuf>,

    /// Open the full-screen interface even inside an agent session (needs a terminal).
    #[arg(long, global = true, conflicts_with = "no_tui")]
    pub tui: bool,

    /// Never open the full-screen interface; print the usual text.
    #[arg(long = "no-tui", global = true)]
    pub no_tui: bool,
}

/// The command set. Adding a command touches four places: this enum, the mod declaration, the
/// main.rs dispatch, and the module file.
#[derive(Subcommand)]
pub enum Commands {
    // ── Authentication ──────────────────────────────────────────────
    /// Sign in to the hub (interactive on a TTY; CI uses --with-token reading a PAT from stdin)
    Login(login::Args),
    /// Sign out (revokes the server session, deletes local credentials; the store and repos are untouched)
    Logout(logout::Args),
    /// Current identity: hub, account, email, credential expiry (offline by default)
    Whoami(whoami::Args),
    /// Global config: hub.url / runtime.default / push.visibility / commit.auto
    Config(config::Args),

    // ── Repositories ────────────────────────────────────────────────
    /// Create an agent repo: the main file line + scaffolding (memory/ skills/ AGENTS.md), bound to this directory
    Init(init::Args),
    /// Clone a repo locally (fetch only — running is run/resume's job)
    Clone(clone::Args),
    /// Run any frozen ref: fetch + arbitrate (fork if needed) + materialize + launch
    Run(run::Args),
    /// Repo administration: create/list/info/visibility/collab/rename/delete/path
    Repo(repo::Args),

    // ── Adoption and status ─────────────────────────────────────────
    /// Adopt a running session; import settles immediately (cursor picker; --link-only works offline)
    Import(import::Args),
    /// Who am I, adopted sessions, sync state (instant, offline)
    Status(status::Args),
    /// Pin the current directory to a branch (context only)
    Switch(switch::Args),
    /// Branch listing and hygiene: rename / rm / seal (branches are born only via import/fork/new/run)
    Branch(branch::Args),

    // ── Recording ───────────────────────────────────────────────────
    /// Settle: new content since the last settlement becomes one turn commit per user turn
    Commit(commit::Args),
    /// Name milestones / release versions (pushed tags are immutable)
    Tag(tag::Args),
    /// Memory between the runtime directory, this session branch and main: status / diff / distill / sync
    Memory(memory::Args),
    /// Carry memory files from this session branch into main (alias of `memory distill`)
    Distill(memory::DistillArgs),

    // ── Inspection ──────────────────────────────────────────────────
    /// Turn-by-turn history: #n ordinals, short shas, kinds, messages, code anchors, tags
    Log(log::Args),
    /// Render the VIEW as a readable conversation (what the agent sees on resume)
    Show(show::Args),
    /// Between two points: new turns (default) / VIEW diff / shared-file diff
    Diff(diff::Args),
    /// plumbing: print a point's VIEW composition (the merge agent's recon command)
    View(view::Args),

    // ── Forking and continuing ──────────────────────────────────────
    /// Fork a new session off any ref — the only form of “check out an old state”
    Fork(fork::Args),
    /// Start a fresh session (empty VIEW, shared files inherited from main)
    New(new::Args),
    /// The strict entry point for continuing a session
    Resume(resume::Args),

    // ── Merging ─────────────────────────────────────────────────────
    /// Merge driven by a merge agent (--manual drives the same plumbing by hand)
    Merge(merge::Args),
    /// Agent-free picking: lift turns/events from other branches into the target VIEW
    CherryPick(cherry_pick::Args),
    /// Drop events from the VIEW (the evidence log never moves)
    Revert(revert::Args),

    // ── Remote ──────────────────────────────────────────────────────
    /// Publish to the hub (secret scan first; visibility set once at first push; no --force)
    Push(push::Args),
    /// fetch: objects and remote refs only — local branches never move
    Fetch(fetch::Args),
    /// fetch + fast-forward only; a real fork offers merge / fork as the two exits
    Pull(pull::Args),

    // ── Discovery and sharing ───────────────────────────────────────
    /// Search the corpus you can read for “has anyone done this” (MCP is the primary surface)
    Search(search::Args),
    /// Mint a one-shot read-only link for people without agit
    Share(share::Args),
    /// Pull requests: create / list / show / fetch / merge
    Pr(pr::Args),

    // ── Export and diagnostics ──────────────────────────────────────
    /// Export a branch or a range: jsonl / ir / markdown / claude-code / codex
    Export(export::Args),
    /// Pre-flight scan before publishing: --secrets (default) / --sensitive (local review agent)
    Scan(scan::Args),
    /// Register low-entropy literal secrets in the device-local encrypted vault
    Secrets(secret_vault::Args),
    /// Install all integrations at once: hooks + skill + mcp + AGENTS.md (idempotent)
    Setup(setup::Args),
    /// Upgrade to the latest version the hub announces (--check only reports)
    Upgrade(upgrade::Args),
    /// Integrity check: VIEW self-consistency + append-only transcripts (--check-backend goes online)
    Doctor(doctor::Args),

    // ── Hidden: runtime and MCP, not shown in help ───
    /// (hidden) runtime hook entry point
    #[command(hide = true)]
    Hooks(hooks::Args),
    /// (hidden) stdio MCP server
    #[command(hide = true)]
    Mcp(mcp::Args),

    /// Remote control: pair this machine and drive its sessions from the web.
    Rc(rc::Args),
}

/// Stable command identifier used in machine-readable output.
pub fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Login(_) => "login",
        Commands::Logout(_) => "logout",
        Commands::Whoami(_) => "whoami",
        Commands::Config(_) => "config",
        Commands::Init(_) => "init",
        Commands::Clone(_) => "clone",
        Commands::Run(_) => "run",
        Commands::Repo(_) => "repo",
        Commands::Import(_) => "import",
        Commands::Status(_) => "status",
        Commands::Switch(_) => "switch",
        Commands::Branch(_) => "branch",
        Commands::Commit(_) => "commit",
        Commands::Tag(_) => "tag",
        Commands::Memory(_) => "memory",
        Commands::Distill(_) => "distill",
        Commands::Log(_) => "log",
        Commands::Show(_) => "show",
        Commands::Diff(_) => "diff",
        Commands::View(_) => "view",
        Commands::Fork(_) => "fork",
        Commands::New(_) => "new",
        Commands::Resume(_) => "resume",
        Commands::Merge(_) => "merge",
        Commands::CherryPick(_) => "cherry-pick",
        Commands::Revert(_) => "revert",
        Commands::Push(_) => "push",
        Commands::Fetch(_) => "fetch",
        Commands::Pull(_) => "pull",
        Commands::Search(_) => "search",
        Commands::Share(_) => "share",
        Commands::Pr(_) => "pr",
        Commands::Export(_) => "export",
        Commands::Scan(_) => "scan",
        Commands::Secrets(_) => "secrets",
        Commands::Setup(_) => "setup",
        Commands::Upgrade(_) => "upgrade",
        Commands::Doctor(_) => "doctor",
        Commands::Hooks(_) => "hooks",
        Commands::Mcp(_) => "mcp",
        Commands::Rc(_) => "rc",
    }
}

/// `scan` and `view` predate the global switch and keep a local spelling for
/// compatibility.  Treat either spelling as a request for the same envelope.
pub fn json_requested(cli_json: bool, command: &Commands) -> bool {
    // These two hidden commands speak a line-oriented protocol on stdio.  An
    // outer envelope would corrupt the stream (and make the next request
    // impossible to parse), so their protocol remains authoritative even when
    // a caller happens to pass the global flag.
    if matches!(command, Commands::Hooks(_) | Commands::Mcp(_)) {
        return false;
    }
    cli_json
        || matches!(command, Commands::Scan(args) if args.json)
        || matches!(command, Commands::View(args) if args.json)
}

/// The command-definition entry point `setup --completions` needs.
pub fn cli_def() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}

#[cfg(test)]
mod json_cli_tests {
    use super::*;

    /// Parse one test command line and take out its subcommand.
    ///
    /// `Cli::command` is an `Option` (a bare `agit` falls through to the TUI/help path, see
    /// `main.rs`), while every command line in this group carries a subcommand — failing to get
    /// one means the case itself is written wrong.
    fn command_of(argv: Vec<&str>) -> Commands {
        Cli::try_parse_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"))
            .command
            .unwrap_or_else(|| panic!("{argv:?} names no subcommand"))
    }

    #[test]
    fn global_json_is_accepted_before_and_after_a_command() {
        for argv in [
            vec!["agit", "--json", "status"],
            vec!["agit", "status", "--json"],
        ] {
            let json = Cli::try_parse_from(&argv)
                .expect("--json should parse")
                .json;
            assert!(json_requested(json, &command_of(argv)));
        }
    }

    #[test]
    fn protocol_commands_keep_their_stdio_contract() {
        for argv in [vec!["agit", "hooks"], vec!["agit", "mcp"]] {
            assert!(!json_requested(true, &command_of(argv)));
        }
    }

    #[test]
    fn json_rejects_lifecycles_that_cannot_return_one_document() {
        for (argv, expected) in [
            (vec!["agit", "--json", "login"], "interactive login flow"),
            (
                vec!["agit", "--json", "rc", "start"],
                "foreground RC daemon",
            ),
            (vec!["agit", "--json", "show", "--tui"], "show --tui"),
            (vec!["agit", "--json", "resume", "main"], "launched runtime"),
        ] {
            let reason = json::incompatible(&command_of(argv)).unwrap_or("");
            assert!(reason.contains(expected), "{reason}");
        }
    }

    #[test]
    fn json_allows_preparation_only_lifecycles() {
        for argv in [
            vec!["agit", "--json", "rc", "start", "--detach"],
            vec!["agit", "--json", "run", "main", "--no-launch"],
            vec!["agit", "--json", "new", "--no-launch"],
            vec![
                "agit",
                "--json",
                "fork",
                "main",
                "-b",
                "copy",
                "--resume",
                "--no-launch",
            ],
            vec!["agit", "--json", "merge", "source", "--manual"],
            vec!["agit", "--json", "merge", "source", "--dry-run"],
        ] {
            assert!(
                json::incompatible(&command_of(argv)).is_none(),
                "unexpected rejection"
            );
        }
    }
}
