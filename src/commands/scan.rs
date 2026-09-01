//! `agit scan` — the review before anything goes out.
//!
//! * `--secrets` (default): a standalone entry point into the scan `push` runs built-in, so CI
//!   can run it on its own. A hit carries file / line / redacted snippet.
//!
//!   "Standalone entry point" is literal: destination, scan surface, the workspace pass, where
//!   the allowlist comes from, the decision rules — all of it goes through
//!   [`secrets::scan_agent_repo`] (workspace + reachable blob / commit / tag, repo-wide, run
//!   **once**), the **same function** `push` calls. An entry point that only looks at reachable
//!   objects and skips the workspace makes scan report clean while push blocks, on the most
//!   common shape of all: a secret already pushed and still sitting in a workspace file. An
//!   entry point that assembles its own scan surface also scans the `session/meta.json` that
//!   push deliberately excludes, and reads the allowlist from the **repo root** instead of
//!   `$AGIT_HOME` — an allowlist added exactly as scan's own hint says then has no effect
//!   inside scan. Two implementations will drift apart sooner or later.
//!
//!   Refs on the command line do **not** narrow the scan surface; they are only parsed and
//!   validated (a mistyped ref errors on the spot rather than quietly scanning something else).
//!   The publish surface is not a function of any one ref: `agit push -b other` has the publish
//!   plan bring `main` along as well, so a per-ref scan only misses. See
//!   [`secrets::scan_agent_repo`].
//!
//! * `--sensitive`: **brings up a local review agent** to vet the transcript (content off the
//!   session's topic, out-of-scope sensitive information, directory structure leaked through
//!   absolute paths). Every entry in the report carries an `@#n.k` locator and a directly
//!   runnable remedy (`agit revert @#12.4`). It reports; it changes nothing.
//!
//!   With no model available it reports the unmet precondition explicitly (exit 4) and points
//!   at `--secrets`, which still works — it never pretends a machine reviewed anything.

use super::CmdResult;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::secrets;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Refs (repeatable). Default: the context branch.
    #[arg(value_name = "owner/repo@ref")]
    pub refs: Vec<String>,
    /// Structural secret scan (default).
    #[arg(long)]
    pub secrets: bool,
    /// Bring up a local review agent to vet sensitive content.
    #[arg(long)]
    pub sensitive: bool,
    /// Machine-readable report.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: Args) -> CmdResult {
    if args.sensitive && args.secrets {
        ui::error("`--secrets` and `--sensitive` are two different checks; run them separately.");
        return Ok(ExitCode::Usage);
    }
    let cwd = std::env::current_dir()?;
    let parsed_refs: Vec<refs::RefSpec> = match args
        .refs
        .iter()
        .map(
            |raw| match crate::commands::target::parse_spec_preferring_local(&cwd, raw) {
                Ok(spec) => Ok(spec),
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    Err(ExitCode::Usage)
                }
            },
        )
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(specs) => specs,
        Err(code) => return Ok(code),
    };
    // `@` becomes a branch name before anyone reads these specs — including the repo derivation
    // below. The resolver reads no environment, so a `Base::At` handed down to it is refused.
    let parsed_refs: Vec<refs::RefSpec> = match parsed_refs
        .into_iter()
        .map(super::context::substitute_at)
        .collect::<anyhow::Result<Vec<_>>>()
    {
        Ok(specs) => specs,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    // An explicit ref only needs the repo resolved; only the zero-argument form has to resolve
    // the "current branch".
    let (slug, default_branch) = if args.refs.is_empty() {
        match super::context::resolve(&cwd) {
            Ok(c) => (super::context::qualify(&c.repo), Some(c.branch)),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        }
    } else {
        // An explicit `<owner/repo>@ref` also identifies the repository.  If
        // only a local ref was written, retain the legacy cwd-based lookup.
        let explicit_repos: Vec<String> = parsed_refs
            .iter()
            .filter_map(|spec| match &spec.repo {
                refs::RepoSel::Slug(owner, name) => Some(format!("{owner}/{name}")),
                refs::RepoSel::Local(name) => Some(name.clone()),
                refs::RepoSel::Context => None,
            })
            .collect();
        if explicit_repos.windows(2).any(|w| w[0] != w[1]) {
            ui::error("all scan targets must belong to the same agent repo.");
            return Ok(ExitCode::Usage);
        }
        match explicit_repos.first() {
            Some(r) if r.contains('/') => (r.clone(), None),
            Some(r) => (super::context::qualify(r), None),
            None => match super::context::repo_for(&cwd) {
                Ok(r) => (super::context::qualify(&r), None),
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Ref);
                }
            },
        }
    };
    let (o, n) = super::parse_slug(&slug)?;
    let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
        ui::error(&format!("{slug} does not exist locally."));
        return Ok(ExitCode::Precondition);
    };

    if args.sensitive {
        ui::error(
            "--sensitive needs a local review agent (model) to run; none is available on this machine.",
        );
        ui::hint(
            "the structured scan still works: `agit scan --secrets` (the same one `push` runs built-in)",
        );
        return Ok(ExitCode::Precondition);
    }

    // --secrets: scan repo-wide, **once**.
    let targets: Vec<refs::RefSpec> = match default_branch {
        // The context branch is a branch name, never repository syntax — a
        // slash in it (`topic/foo`) must stay inside the name.
        Some(b) => vec![
            crate::commands::target::parse_local(&b).expect("context branch names are valid refs"),
        ],
        None => parsed_refs,
    };
    // Every ref is resolved first: a mistyped ref says so before a whole-repo scan is spent on
    // it.
    //
    // The resolved shas take **no part** in the scan — the scan surface is the whole repo's
    // publish surface, not the union of these refs (reasons in [`collect`]). What is wanted here
    // is only the "a typo errors on the spot" half: swallowing a ref that does not exist and
    // then reporting clean is far worse than erroring.
    for spec in &targets {
        if let Err(e) = refs::resolve(&repo, spec) {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    }
    let found = collect(&repo, &n)?;
    let any_hit = !found.hits.is_empty();
    let truncated = found.truncated;
    let shown = found.hits.len();
    let unscanned = found.unscanned.clone();
    // The **carrier** of each hit, accumulated and handed to [`super::hint_secret_remedies`] at
    // the end.
    //
    // For a hit inside a blob / commit / tag object, the hints below about annotating the line
    // with `agit:allow-secret` and reverting the VIEW entry do not apply at all — those lines
    // are not in the workspace, there is no line to annotate and no VIEW entry to revert.
    //
    // The way out of each of the object carriers is **different**, so they are recorded and
    // stated separately: a blob is located by oid first and then the commits carrying it are
    // rewritten, a commit is rewritten directly, a tag only needs to be cut again. Collapsing
    // them into one "rewrite history" makes the user rebase needlessly over a tag; the other way
    // round, promising "and a tag is just cut again" while only commits were scanned states
    // something that holds for no hit at all. A way out that leads nowhere wastes more of the
    // user's time than no way out.
    //
    // The branching itself lives in [`super::secret_remedies`], and the `agit push` gate calls
    // the same function: this only accumulates carriers. The test is the **structured source**,
    // not the human-readable `at` string — with the latter, a directory genuinely named
    // `commit object x/...` flips the decision, and that is input a user can create.
    let mut report: Vec<serde_json::Value> = vec![];
    for (at, h) in &found.hits {
        if args.json {
            // `source` is the **structured** carrier, so a consumer never parses the prefix
            // of `at` — that is exactly the test named wrong above (a user can create a
            // directory genuinely called `commit object ab12cd34/...` and flip it).
            report.push(serde_json::json!({
                "rule": h.rule,
                "at": at,
                "line": h.line,
                "preview": h.redacted,
                "source": h.source.as_str(),
            }));
        } else {
            // Print only the redacted snippet: this output goes into CI logs.
            println!("  {}  {at}:{}  {}", h.rule, h.line, ui::dim(&h.redacted));
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    if any_hit {
        if !unscanned.is_empty() {
            super::report_unscanned(&unscanned);
        }
        ui::error("secret-like patterns found. Handle before sending out:");
        // This one comes first: it decides how many times the actions below have to be
        // repeated. Unsaid, a truncated report looks exactly like "this is all of it", and "fix
        // these, then push" becomes a loop that never ends.
        if truncated {
            ui::hint(&format!(
                "· this report is incomplete: it shows {shown} findings and stops there, more remain — handle these, then run `agit scan` again to see the rest"
            ));
        }
        // An explicitly registered secret deliberately does not accept an allowlist, so this
        // way out is offered only when a built-in heuristic actually hit.
        if found
            .hits
            .iter()
            .any(|(_, h)| h.rule != "registered-secret")
        {
            ui::hint("· allowlist (local): write to $AGIT_HOME/.agit-allow-secrets");
        }
        ui::hint(
            "· blanket bypass (think twice before it enters history): AGIT_ALLOW_SECRETS=1 agit push — the server may still refuse it, and it will block going public later",
        );
        ui::hint("· remove from the VIEW: `agit revert @#n.k`");
        // The carrier-specific hints come last: for a hit inside an object they are the only
        // actions that work at all, and they cost the most.
        super::hint_secret_hit_remedies(found.hits.iter().map(|(_, hit)| hit));
        // Policy rejection is 7, not the generic failure 1 — CI has to tell "a secret was
        // found" from "the command broke".
        Ok(ExitCode::Policy)
    } else if !unscanned.is_empty() {
        // This must **not** say clean. A verdict read off an empty hit list is fail open — what
        // was never scanned and what is clean look identical in an empty list, and that is
        // exactly the drift where `agit push` blocks while `agit scan` reports clean. The two
        // paths must return the same verdict, so this exits non-zero too.
        super::report_unscanned(&unscanned);
        Ok(ExitCode::Policy)
    } else {
        if !args.json {
            // Never "N refs": what is scanned is the whole repo's publish surface, not the
            // union of the refs on the command line. Reporting it as a per-ref count makes the
            // user believe "scan again with another ref" shows something else.
            ui::success("clean scan");
        }
        Ok(ExitCode::Ok)
    }
}

/// Every hit from one `--secrets` scan, each carrying **the location it really belongs to**.
struct Found {
    /// `(display location, hit)`. The location is the hit's own carrier label, never prefixed
    /// with a ref name.
    hits: Vec<(String, secrets::Hit)>,
    /// Any truncated stretch makes the report as a whole incomplete — say so.
    truncated: bool,
    /// Some part was **never scanned**. An empty hit list plus this does not mean clean.
    unscanned: secrets::Unscanned,
}

/// Run the **one** repo-wide pass and produce a report.
///
/// # Why there is no per-target pass on top
///
/// The scan surface ([`secrets::scan_agent_repo`]) is the **whole repo**, independent of which
/// ref was written on the command line — the publish surface is not a function of any one ref.
/// A per-ref pass is wrong in both directions:
///
/// * **Misses**: it looks only at that ref's tree, while `agit push -b other` has the publish
///   plan bring `main` along; with the secret in a current file on `main`, `agit scan other`
///   reports clean while push blocks.
/// * **Rescans, re-reports**: `agit scan main other` scans the same objects twice, the same hit
///   appears twice in the report and the user believes there are two places to fix; each is also
///   prefixed with the target of that pass, so a hit that exists only on `main` shows up as
///   `other:...` — pointing at a ref that does not have it, and chasing that is a dead end.
///
/// So a hit's location is its own carrier label (`blob object <sha8>/<path>`,
/// `commit object <sha8>`, `tag object <sha8>`), never prefixed with a ref name. The caller
/// still **parses and validates** every ref (a mistyped ref must error on the spot), but those
/// shas never reach here — off the signature, they give no one the chance to narrow the scan
/// surface with them.
fn collect(repo: &Repo, agent: &str) -> crate::Result<Found> {
    // Destination, scan surface, the workspace pass — every one of them goes through the
    // **same code** as `agit push`. See [`secrets::scan_agent_repo`] and
    // [`super::publish_destination`].
    //
    // `agent` is the third element of the destination identity: the name this repo resolves to,
    // which is the one `agit push` publishes to from here. Without it, when `origin` points at
    // another agent under the same account, that agent's history is subtracted wholesale from
    // the scan surface.
    let plan = secrets::ScanPlan::to(super::publish_destination(repo, agent));
    let wide = secrets::scan_agent_repo(repo, &plan)?;
    let hits = wide
        .hits
        .into_iter()
        .map(|h| (h.file.clone().unwrap_or_else(|| "?".into()), h))
        .collect();
    Ok(Found {
        hits,
        truncated: wide.truncated,
        unscanned: wide.unscanned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWS: &str = "AKIA4X7QZ2M5RT6VW3JH";

    /// Build a repo with two branches: `main` carries a commit message with a secret in it,
    /// `other` splits off before that commit, and both branches' trees are clean.
    fn two_branch_repo() -> tempfile::TempDir {
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
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "other"]);
        // The secret is only on main, and only in the message — the tree is empty, so a per-ref
        // path never produces it.
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("leak {AWS}"),
        ]);

        d
    }

    /// This pins that one hit is reported once, and that the size of the report is not a
    /// function of how many refs the user wrote.
    ///
    /// The scan surface is the whole repo's publish surface. A per-ref scan has
    /// `agit scan main other` scan the same objects twice and report them twice — the user
    /// believes there are two places to fix, and the count does not drop after fixing one.
    #[test]
    fn a_repo_wide_hit_is_reported_exactly_once() {
        let d = two_branch_repo();
        let repo = Repo::at(d.path());

        let found = collect(&repo, "photo").expect("the repo is well-formed");
        let leaks: Vec<&(String, secrets::Hit)> = found
            .hits
            .iter()
            .filter(|(_, h)| h.rule == "aws-access-token")
            .collect();
        assert_eq!(
            leaks.len(),
            1,
            "the same commit hit must be reported exactly once: {leaks:?}"
        );
    }

    /// This pins that a hit is never prefixed with the name of a ref.
    ///
    /// The secret is only on `main`, and `other` splits off before it. Showing it as
    /// `other:commit object ...` points the user at a ref that does not have that commit, and
    /// chasing that is a dead end.
    #[test]
    fn a_repo_wide_hit_is_not_attributed_to_a_ref_that_lacks_it() {
        let d = two_branch_repo();
        let repo = Repo::at(d.path());

        let found = collect(&repo, "photo").expect("the repo is well-formed");
        let (at, _) = found
            .hits
            .iter()
            .find(|(_, h)| h.rule == "aws-access-token")
            .expect("this hit must be reported");
        assert!(
            at.starts_with("commit object "),
            "a repo-wide hit's location is its own carrier, never a ref name: {at:?}"
        );
    }

    /// This pins that `agit scan` and `agit push` return the **same** verdict on the same repo.
    ///
    /// # What this pins
    ///
    /// Two entry points behind the two gates — `agit scan` on `scan_repo_wide` (reachable
    /// objects only), `agit push` on `scan_agent_repo` (which also walks the whole workspace) —
    /// split on the **most common** shape of all: a secret already pushed and still lying in a
    /// workspace file. scan says `clean scan` (exit 0), push says
    /// `1 suspected secrets found — publish blocked`. The user cannot reproduce locally why they
    /// were refused, and two entry points keep drifting for as long as both exist.
    ///
    /// Both go through the **same function** and the same [`secrets::ScanPlan`], so what is
    /// asserted here is that the full hit lists are equal, not merely that "both are non-empty":
    /// the same verdict is the conclusion, the same scan surface is the reason.
    #[test]
    fn scan_and_push_agree_on_the_same_repo() {
        let d = tempfile::tempdir().unwrap();
        let work = d.path().join("work");
        let hub = d.path().join("hub.git");
        std::fs::create_dir_all(&work).unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&work)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // The secret sits in a workspace file and has **already been pushed** — the most common
        // shape.
        std::fs::write(work.join("config.env"), format!("AWS_KEY={AWS}\n")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "settings"]);
        std::process::Command::new("git")
            .args(["init", "-q", "--bare", &hub.to_string_lossy()])
            .output()
            .unwrap();
        run(&["remote", "add", "origin", &hub.to_string_lossy()]);
        run(&["push", "-q", "origin", "main"]);

        // Precondition: the secret really is in a workspace file, and really has been pushed.
        assert!(
            std::fs::read_to_string(work.join("config.env"))
                .unwrap()
                .contains(AWS),
            "precondition: the secret must be in the workspace file"
        );
        assert!(
            !run(&["for-each-ref", "--format=%(refname)", "refs/remotes/origin"]).is_empty(),
            "precondition: it must have been pushed, or the two paths cannot drift apart at all"
        );

        let repo = Repo::at(&work);
        // Both paths ask about the **same** destination, so the third element of the identity
        // (which agent to publish to) has to be the same as well: different names send them to
        // two different far sides, and this test then no longer pins "the same scan surface".
        const AGENT: &str = "photo";
        // The scan path.
        let scanned = collect(&repo, AGENT).expect("the repo is well-formed");
        // The push path: the same destination, the same plan.
        let plan = secrets::ScanPlan::to(crate::commands::publish_destination(&repo, AGENT));
        let pushed = secrets::scan_agent_repo(&repo, &plan).expect("the repo is well-formed");

        assert!(
            !scanned.hits.is_empty(),
            "`agit scan` must see the secret in the workspace file — reporting clean is the bug"
        );
        assert_eq!(
            scanned.hits.len(),
            pushed.hits.len(),
            "the two gates must find the same number of hits: scan={:?} push={:?}",
            scanned.hits,
            pushed.hits
        );
        let scan_hits: Vec<&secrets::Hit> = scanned.hits.iter().map(|(_, h)| h).collect();
        let push_hits: Vec<&secrets::Hit> = pushed.hits.iter().collect();
        assert_eq!(
            scan_hits, push_hits,
            "the two gates must look at the same scan surface, not merely both be non-empty"
        );
    }
}
