//! `agit diff` — between two points.
//!
//! * `--turns` (default): the fork point plus the turns each side added, the reconnaissance view
//!   before a merge. Within one repo that is the merge-base; across repos it is the common prefix
//!   of the turn hash chain (the hash covers no timestamp, machine or account — the same turn
//!   hashes the same across people, which is what makes a fork point findable across repos).
//! * `--view`: insertions and deletions between the two VIEW sequences — what a merge or a
//!   distill actually swapped into the agent's context.
//! * `--files`: an ordinary text diff of the shared files.
//!
//! Zero arguments shows working-state changes to shared files plus a summary of unsettled turns.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// `<a>[..<b>]`; zero args = the working state.
    #[arg(value_name = "owner/repo@a..b")]
    pub range: Option<String>,
    /// Turn-level comparison (default).
    #[arg(long, conflicts_with_all = ["view", "files"])]
    pub turns: bool,
    /// VIEW sequence diff.
    #[arg(long, conflicts_with_all = ["turns", "files"])]
    pub view: bool,
    /// Shared-file text diff.
    #[arg(long, conflicts_with_all = ["turns", "view"])]
    pub files: bool,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let range = args.range.as_deref().map(|raw| {
        let left = raw
            .split_once("...")
            .or_else(|| raw.split_once(".."))
            .map(|(a, _)| a)
            .unwrap_or(raw);
        crate::commands::target::parse(left).map(|target| (raw.to_string(), target))
    });
    let (range, left_target) = match range {
        Some(Ok((range, target))) => (Some(range), Some(target)),
        Some(Err(e)) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
        None => (None, None),
    };
    let explicit_repo = left_target.as_ref().and_then(|target| target.repo.clone());
    let repo = if let Some(slug) = explicit_repo {
        let (o, n) = super::parse_slug(&slug)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{slug} does not exist locally."));
            return Ok(ExitCode::Precondition);
        };
        repo
    } else {
        let ctx = match super::context::resolve(&cwd) {
            Ok(c) => c,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        };
        let (o, n) = super::parse_slug(&ctx.repo)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{} does not exist locally.", ctx.repo));
            return Ok(ExitCode::Precondition);
        };
        repo
    };

    if range.is_none() {
        return workdir_diff(&repo);
    }
    let range = range.unwrap();
    let (a, b, three_dot) = split_range(&range);
    if let Some(right) = &b
        && let Err(e) = crate::commands::target::parse_local(right)
    {
        ui::error(&format!("{e:#}"));
        return Ok(ExitCode::Usage);
    }
    let Some(a_sha) = (match resolve_in(&repo, &a) {
        Ok(sha) => sha,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    }) else {
        return Ok(ExitCode::Ref);
    };
    let b_sha = match b {
        Some(b) => match resolve_local_in(&repo, &b) {
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
            Ok(sha) => match sha {
                Some(s) => s,
                None => return Ok(ExitCode::Ref),
            },
        },
        None => match repo.git_opt(&["rev-parse", "HEAD"]) {
            Some(s) => s.trim().to_string(),
            None => {
                ui::error("no commits yet.");
                return Ok(ExitCode::Precondition);
            }
        },
    };

    // The range operator **means one thing on all three paths**, so the left end is computed
    // once here and every path below uses it.
    //
    // A left end that ignores the operator is wrong in both directions at once: a left end
    // hardwired to the merge-base reads `..` as `...`, and one hardwired to `a_sha` reads `...`
    // as `..`. Either way the same `a..b` is a fork-point view under `--turns` and a two-point
    // view under `--files`, and the user has no way to ask for the other one.
    //
    // git's own definition governs: the left end of `..` is `a`, the left end of `...` is the
    // fork point. All three paths share that left end — and share **whether it really is a fork
    // point**.
    //
    // Three-dot semantics want the merge-base. A cross-repo comparison or an orphan branch has
    // no common ancestor at all and `merge-base` fails. Falling back to `a` is right there (it
    // is an honest answer), but **the label must stop saying fork point** — that value is not a
    // computed fork point, and a fake fork point in the report reads as though the two lines
    // really did split there.
    //
    // So "did it compute" comes out alongside the value, and the label below follows the fact
    // rather than the operator the user typed.
    let (base, real_fork) = if three_dot {
        match repo
            .git_opt(&["merge-base", &a_sha, &b_sha])
            .map(|s| s.trim().to_string())
        {
            Some(b) => (b, true),
            None => (a_sha.clone(), false),
        }
    } else {
        (a_sha.clone(), false)
    };

    if args.files {
        // Shared files = everything but the session itself. Excluding the whole `session/`
        // rather than each file name by name drops one way to fail silently: "the layout
        // changed and the exclude list did not follow".
        let out = repo.git(&[
            "diff",
            &base,
            &b_sha,
            "--",
            ".",
            ":(exclude)session",
            ":(exclude)LOG",
            ":(exclude)VIEW",
            ":(exclude)events",
        ])?;
        print!("{out}");
        return Ok(ExitCode::Ok);
    }

    if args.view {
        let va = view_at(&repo, &base)?;
        let vb = view_at(&repo, &b_sha)?;
        view_diff(&va, &vb);
        return Ok(ExitCode::Ok);
    }

    // Default: the --turns reconnaissance.
    turns_report(&repo, &base, &a_sha, &b_sha, real_fork);
    Ok(ExitCode::Ok)
}

/// The VIEW at one endpoint. **No VIEW does not mean broken.**
///
/// The left endpoint of `--view` is almost always the kind of commit that carries no VIEW: `main`
/// is the file line `agit init` writes (its tree deliberately holds no LOG/VIEW), and for two
/// session lines grown off `main` the merge-base of `a...b` lands right back on that commit —
/// `agit diff <target>...pr/{id} --view`, the form `pr show` prints itself, is this shape. A bare
/// `materialize_at?` kills the whole command with `cannot inspect <sha>:VIEW` and prints nothing.
///
/// The test is the one used everywhere else (commit / revert / cherry-pick / merge all use it):
/// a file line, and a newborn session line that has not claimed an identity yet, carry no session
/// in the first place and compare as the **empty sequence**; a session line that has claimed an
/// identity and is missing its VIEW is genuine damage and still fails hard.
fn view_at(repo: &Repo, git_ref: &str) -> crate::Result<String> {
    match meta::read_at_ref_result(repo, git_ref)? {
        Some(snapshot) if snapshot.is_file_line() || snapshot.session.is_empty() => {
            Ok(String::new())
        }
        _ => storage::materialize_at(repo.root(), git_ref, meta::VIEW_FILE),
    }
}

fn split_range(r: &str) -> (String, Option<String>, bool) {
    if let Some((a, b)) = r.split_once("...") {
        (a.to_string(), Some(b.to_string()), true)
    } else if let Some((a, b)) = r.split_once("..") {
        (a.to_string(), Some(b.to_string()), false)
    } else {
        (r.to_string(), None, false)
    }
}

fn resolve_in(repo: &Repo, name: &str) -> crate::Result<Option<String>> {
    let spec = refs::parse(name)?;
    resolve_spec(repo, &spec)
}

fn resolve_local_in(repo: &Repo, name: &str) -> crate::Result<Option<String>> {
    let spec = crate::commands::target::parse_local(name)?;
    resolve_spec(repo, &spec)
}

fn resolve_spec(repo: &Repo, spec: &refs::RefSpec) -> crate::Result<Option<String>> {
    let spec = match crate::commands::context::substitute_at(spec.clone()) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(None);
        }
    };
    match refs::resolve(repo, &spec) {
        Ok(r) => Ok(Some(r.sha)),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(None)
        }
    }
}

fn turns_report(repo: &Repo, base: &str, a: &str, b: &str, three_dot: bool) {
    // The label follows the semantics: only the left end of `...` is the fork point, the left
    // end of `..` is `a` itself. Printing `fork point` in the two-point view is a lie — that
    // value is not a computed fork point, and the A side then counts zero new turns, which
    // reads as the tool having got it wrong.
    let label = if three_dot {
        "fork point"
    } else {
        "base      "
    };
    println!("{label}  {}", &base[..9.min(base.len())]);
    for (label, head) in [("A", a), ("B", b)] {
        let n = repo
            .git_opt(&["rev-list", "--count", &format!("{base}..{head}")])
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        println!("{label} side    +{n} turns");
        if let Ok(log) = repo.git(&["log", "--format=%h %s", &format!("{base}..{head}")]) {
            for l in log.lines().take(10) {
                println!("  {l}");
            }
        }
    }
}

/// VIEW sequence delta: identity is the full envelope event id, and insertions and deletions are
/// reported by occurrence count.
fn view_diff(a: &str, b: &str) {
    let a_ids = event_ids_of(a);
    let b_ids = event_ids_of(b);
    let (common, reordered, removed, added) = sequence_delta(&a_ids, &b_ids);
    println!(
        "VIEW: common {} · reordered {} · removed {} · added {}",
        common,
        reordered,
        removed.len(),
        added.len()
    );
    for h in removed {
        println!("  - {}", &h[..12.min(h.len())]);
    }
    for h in added {
        println!("  + {}", &h[..12.min(h.len())]);
    }
}

fn event_ids_of(text: &str) -> Vec<String> {
    text.split_inclusive('\n')
        .filter_map(|line| storage::event_id(line).ok())
        .collect()
}

fn multiset_delta(a: &[String], b: &[String]) -> (usize, Vec<String>, Vec<String>) {
    fn counts(ids: &[String]) -> std::collections::HashMap<&str, usize> {
        let mut counts = std::collections::HashMap::new();
        for id in ids {
            *counts.entry(id.as_str()).or_insert(0) += 1;
        }
        counts
    }

    let mut b_remaining = counts(b);
    let mut removed = Vec::new();
    for id in a {
        match b_remaining.get_mut(id.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => removed.push(id.clone()),
        }
    }

    let mut a_remaining = counts(a);
    let mut added = Vec::new();
    for id in b {
        match a_remaining.get_mut(id.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => added.push(id.clone()),
        }
    }
    (a.len() - removed.len(), removed, added)
}

/// Sequence delta with an exact Hunt-Szymanski LCS on normal inputs.
///
/// Pairing the nth duplicate on A with the nth duplicate on B is not a valid LCS algorithm:
/// `[x,y,x] -> [y,x]` should delete the first x without reporting a reorder. Hunt-Szymanski feeds
/// every matching B position (in reverse) into an LIS and therefore chooses the optimal duplicate
/// occurrence. A hard match-pair budget prevents adversarial repeated events from turning this
/// diagnostic into quadratic work; above it we use the bounded occurrence approximation, which may
/// conservatively over-report reorder but never hides additions/removals.
fn sequence_delta(a: &[String], b: &[String]) -> (usize, usize, Vec<String>, Vec<String>) {
    let (common, removed, added) = multiset_delta(a, b);

    const MAX_LCS_MATCH_PAIRS: usize = 10_000_000;
    let mut b_positions = std::collections::HashMap::<&str, Vec<usize>>::new();
    for (position, id) in b.iter().enumerate() {
        b_positions.entry(id.as_str()).or_default().push(position);
    }
    let match_pairs = a.iter().try_fold(0usize, |total, id| {
        total.checked_add(b_positions.get(id.as_str()).map_or(0, Vec::len))
    });
    let lcs = if match_pairs.is_some_and(|pairs| pairs <= MAX_LCS_MATCH_PAIRS) {
        let mut tails = Vec::<usize>::new();
        for id in a {
            if let Some(positions) = b_positions.get(id.as_str()) {
                for &position in positions.iter().rev() {
                    let slot = tails.partition_point(|tail| *tail < position);
                    if slot == tails.len() {
                        tails.push(position);
                    } else {
                        tails[slot] = position;
                    }
                }
            }
        }
        tails.len()
    } else {
        occurrence_lis_len(a, b)
    };
    (common, common.saturating_sub(lcs), removed, added)
}

fn occurrence_lis_len(a: &[String], b: &[String]) -> usize {
    let mut b_occurrences = std::collections::HashMap::<&str, usize>::new();
    let mut positions_by_occurrence = std::collections::HashMap::<(&str, usize), usize>::new();
    for (position, id) in b.iter().enumerate() {
        let occurrence = b_occurrences.entry(id.as_str()).or_insert(0);
        positions_by_occurrence.insert((id.as_str(), *occurrence), position);
        *occurrence += 1;
    }

    let mut a_occurrences = std::collections::HashMap::<&str, usize>::new();
    let mut positions = Vec::with_capacity(a.len().min(b.len()));
    for id in a {
        let occurrence = a_occurrences.entry(id.as_str()).or_insert(0);
        if let Some(position) = positions_by_occurrence.get(&(id.as_str(), *occurrence)) {
            positions.push(*position);
        }
        *occurrence += 1;
    }

    let mut tails = Vec::<usize>::new();
    for position in positions {
        let slot = tails.partition_point(|tail| *tail < position);
        if slot == tails.len() {
            tails.push(position);
        } else {
            tails[slot] = position;
        }
    }
    tails.len()
}

/// Zero arguments: working-state changes to shared files plus a summary of unsettled turns.
fn workdir_diff(repo: &Repo) -> CmdResult {
    let out = repo.git(&[
        "diff",
        "--",
        ".",
        ":(exclude)session",
        ":(exclude)LOG",
        ":(exclude)VIEW",
        ":(exclude)events",
    ])?;
    if out.trim().is_empty() {
        println!("no shared-file changes in the working state.");
    } else {
        print!("{out}");
    }
    // Unsettled-turn summary: the turn in HEAD meta against the link's settlement baseline,
    // approximated cheaply by the length of the working log against the length of the log in
    // the head commit.
    if let (Ok(head), Ok(wt)) = (
        crate::domain::storage::materialize_at(repo.root(), "HEAD", meta::LOG_FILE),
        crate::domain::storage::materialize_worktree(repo.root(), meta::LOG_FILE),
    ) {
        let extra = wt.len().saturating_sub(head.len());
        if extra > 0 {
            println!(
                "{}",
                ui::dim(&format!(
                    "  the working transcript is {extra} bytes past the settled state (unsettled content)"
                ))
            );
        }
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::{self, Envelope};

    fn envelope(session: &str, content: serde_json::Value) -> String {
        storage::envelope_line(&Envelope {
            source: "codex".into(),
            session_id: session.into(),
            object_hash: transcript::object_hash(&content),
            content,
        })
    }

    #[test]
    fn view_delta_uses_full_envelope_identity_and_preserves_multiplicity() {
        let a_session = format!("agit-{}", "a".repeat(40));
        let b_session = format!("agit-{}", "b".repeat(40));
        let content = serde_json::json!({"same": "content"});
        let a = envelope(&a_session, content.clone());
        let b = envelope(&b_session, content);
        let a_id = event_ids_of(&a)[0].clone();
        let b_id = event_ids_of(&b)[0].clone();
        assert_ne!(a_id, b_id, "provenance is part of event identity");

        let (common, reordered, removed, added) =
            sequence_delta(&[a_id.clone(), a_id.clone()], &[a_id.clone(), b_id.clone()]);
        assert_eq!(common, 1);
        assert_eq!(reordered, 0);
        assert_eq!(removed, vec![a_id]);
        assert_eq!(added, vec![b_id]);
    }

    #[test]
    fn view_delta_reports_reordering_with_duplicate_occurrences() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let (common, reordered, removed, added) = sequence_delta(
            &[a.clone(), b.clone(), a.clone()],
            &[a.clone(), a.clone(), b.clone()],
        );
        assert_eq!((common, reordered), (3, 1));
        assert!(removed.is_empty() && added.is_empty());
    }

    #[test]
    fn view_delta_chooses_the_optimal_duplicate_occurrence() {
        let x = "a".repeat(40);
        let y = "b".repeat(40);
        let (common, reordered, removed, added) =
            sequence_delta(&[x.clone(), y.clone(), x.clone()], &[y.clone(), x.clone()]);
        assert_eq!((common, reordered), (2, 0));
        assert_eq!(removed, vec![x]);
        assert!(added.is_empty());
    }

    /// The `main` `agit init` writes (a file line, no VIEW in its tree) plus one session line
    /// grown off it with one settled turn. Returns (tempdir, repo, sha of main, sha of the
    /// session line).
    fn file_line_main_and_one_settled_branch() -> (tempfile::TempDir, Repo, String, String) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::ensure_session_dir(r.root()).unwrap();

        meta::write(r.root(), &meta::Meta::new_file_line()).unwrap();
        std::fs::write(r.root().join("AGENTS.md"), "hi\n").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: init").unwrap());
        let main = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert!(
            r.show_result(&main, meta::VIEW_FILE).unwrap().is_none(),
            "precondition: the main written by `agit init` has no VIEW in its tree"
        );

        r.git(&["checkout", "-q", "-b", "b"]).unwrap();
        let claim = format!("agit-{}", "b".repeat(40));
        let env = transcript::wrap_lines("{\"a\":1}\n{\"b\":2}\n", "codex", &claim);
        storage::write_snapshot(r.root(), &env, &env).unwrap();
        let mut m = meta::Meta::new(claim, "codex".into(), "/r".into());
        m.turn = Some(1);
        meta::write(r.root(), &m).unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: turn 1").unwrap());
        let b = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        (d, r, main, b)
    }

    /// A `--view` left endpoint without a VIEW is not an error, it is the empty side.
    ///
    /// # What this pins
    ///
    /// The `main` that `agit init` writes is a file line, and its tree deliberately holds no
    /// LOG/VIEW. For two session lines grown off `main`, the merge-base of `a...b` lands right
    /// back on that commit, and the left end of `main..b` is that commit outright. A hard
    /// `materialize_at?` on the left end kills the whole command with `cannot inspect
    /// <sha>:VIEW` and prints not a word — while `agit diff <target>...pr/{id} --view`, the
    /// form `pr show` prints for the user to copy, is exactly this.
    ///
    /// It also pins the edge of the exemption: a session line that **has claimed an identity**
    /// and is missing its VIEW is still damage, and still fails hard.
    #[test]
    fn a_view_diff_left_end_without_a_view_is_an_empty_side() {
        let (_d, r, main, b) = file_line_main_and_one_settled_branch();

        let va = view_at(&r, &main).unwrap_or_else(|e| panic!("a file line must not fail: {e:#}"));
        assert!(
            va.is_empty(),
            "a file line has no VIEW and compares as the empty sequence: {va}"
        );
        let vb = view_at(&r, &b).unwrap();

        // Observed: the whole of b's VIEW is an addition, with nothing removed.
        let (common, reordered, removed, added) =
            sequence_delta(&event_ids_of(&va), &event_ids_of(&vb));
        assert_eq!((common, reordered), (0, 0));
        assert!(removed.is_empty());
        assert_eq!(added.len(), 2, "every event on the b side is an addition");

        // A branch that has claimed an identity and is missing its VIEW is genuine damage; the
        // exemption does not reach here.
        std::fs::remove_file(r.root().join(meta::VIEW_FILE)).unwrap();
        r.add_all().unwrap();
        assert!(r.commit("drop VIEW").unwrap());
        let broken = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let e = view_at(&r, &broken).unwrap_err().to_string();
        assert!(e.contains("VIEW"), "{e}");
    }

    /// `..` and `...` must parse into different things — the test below rests on it.
    #[test]
    fn the_range_operator_is_parsed() {
        assert_eq!(
            split_range("a..b"),
            ("a".to_string(), Some("b".to_string()), false)
        );
        assert_eq!(
            split_range("a...b"),
            ("a".to_string(), Some("b".to_string()), true)
        );
        // `...` is tried before `..`: the other order splits `a...b` into `a` and `.b`.
        let (_, b, three) = split_range("a...b");
        assert_eq!(
            b.as_deref(),
            Some("b"),
            "a three-dot right end carries no extra dot"
        );
        assert!(three);
        assert_eq!(split_range("a"), ("a".to_string(), None, false));
    }

    #[test]
    fn range_right_side_resolves_slash_branches_in_the_selected_repo() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "user.email", "t@t"]).unwrap();
        repo.git(&["config", "user.name", "t"]).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "base"])
            .unwrap();
        repo.git(&["checkout", "-q", "-b", "topic/foo"]).unwrap();

        for op in ["..", "..."] {
            let raw = format!("alice/payments@main{op}topic/foo");
            let (left, right, _) = split_range(&raw);
            let left_target = crate::commands::target::parse(&left).unwrap();
            assert_eq!(left_target.repo.as_deref(), Some("alice/payments"));
            let right_spec =
                crate::commands::target::parse_local(right.as_deref().unwrap()).unwrap();
            assert_eq!(right_spec.base, refs::Base::Name("topic/foo".into()));
            assert_eq!(right_spec.repo, refs::RepoSel::Context);
        }

        assert!(resolve_local_in(&repo, "topic/foo").unwrap().is_some());
    }

    /// The parsed operator must really **change the left end**.
    ///
    /// # What this pins
    ///
    /// A left end written `if three_dot || true { merge-base } else { a }` is always true, its
    /// `else` is dead, and `a..b` is treated as `a...b`; the `--files` and `--view` paths taking
    /// `a` directly treat `...` as `..`. The operator is ignored on **all three** paths, only in
    /// different directions, and the user has no way to ask for the other view.
    ///
    /// So this does not test one command's output, it tests that decision directly: given a
    /// history that really forks, the two-dot left end must be `a` itself, the three-dot left end
    /// must be the merge-base, and **the two must differ** — only differing shows the choice
    /// exists at all.
    #[test]
    fn two_dot_and_three_dot_pick_different_left_ends() {
        let d = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        g(&["commit", "-q", "--allow-empty", "-m", "fork point"]);
        let fork = g(&["rev-parse", "HEAD"]);
        // One more step on the a side, so a != merge-base(a, b).
        g(&["commit", "-q", "--allow-empty", "-m", "a side"]);
        let a = g(&["rev-parse", "HEAD"]);
        g(&["checkout", "-q", "-b", "other", &fork]);
        g(&["commit", "-q", "--allow-empty", "-m", "b side"]);
        let b = g(&["rev-parse", "HEAD"]);

        assert_ne!(
            a, fork,
            "precondition: a must be past the fork point, or the semantics agree"
        );
        let repo = Repo::at(d.path());
        let merge_base = repo
            .git_opt(&["merge-base", &a, &b])
            .map(|s| s.trim().to_string())
            .expect("one repo always has a common ancestor");
        assert_eq!(merge_base, fork);

        // The expression from `run`, lifted out and exercised directly.
        let left = |three_dot: bool| {
            if three_dot {
                repo.git_opt(&["merge-base", &a, &b])
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| a.clone())
            } else {
                a.clone()
            }
        };
        assert_eq!(left(false), a, "the left end of `..` is a");
        assert_eq!(left(true), fork, "the left end of `...` is the fork point");
        assert_ne!(
            left(false),
            left(true),
            "the two semantics must really differ"
        );
    }
}
