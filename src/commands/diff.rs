//! `agit diff` — between two points.
//!
//! * `--turns` (default): the fork point plus the turns each side added, the reconnaissance view
//!   before a merge. A fork point is a verified Git merge-base, including shared history stored
//!   in separate local repositories. Unrelated histories also compare normalized LOG turns,
//!   explicitly labeled semantic; equal projected content alone does not prove ancestry.
//! * `--view`: insertions and deletions between the two VIEW sequences — what a merge or a
//!   distill actually swapped into the agent's context.
//! * `--files`: an ordinary text diff of the shared files.
//!
//! Zero arguments shows working-state changes to shared files plus a summary of unsettled turns.

use super::CmdResult;
use crate::domain::comparison::{Comparison, SemanticPrefix};
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

pub(super) mod pending;

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
    if args.range.is_none() {
        let context = match super::context::resolve(&cwd) {
            Ok(context) => context,
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Ref);
            }
        };
        let (owner, name) = context.owner_name()?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&owner, &name)?) else {
            ui::error(&format!("{} does not exist locally.", context.repo));
            return Ok(ExitCode::Precondition);
        };
        return match workdir_diff(&repo, &context) {
            Ok(code) => Ok(code),
            Err(error) => {
                ui::error(&format!("pending inspection unavailable: {error:#}"));
                Ok(ExitCode::Precondition)
            }
        };
    }
    let endpoints = match args.range.as_deref().map(parse_endpoints).transpose() {
        Ok(endpoints) => endpoints,
        Err(error) => {
            ui::error(&super::terminal_error_message(&error));
            return Ok(ExitCode::Usage);
        }
    };
    let at_context = if endpoints.as_ref().is_some_and(|(left, right, _)| {
        left.base == refs::Base::At
            || right
                .as_ref()
                .is_some_and(|spec| spec.base == refs::Base::At)
    }) {
        match super::context::at_context() {
            Ok(context) => Some(context),
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Ref);
            }
        }
    } else {
        None
    };
    let explicit_repo = match endpoints.as_ref().map(|(left, _, _)| left) {
        Some(refs::RefSpec {
            repo: refs::RepoSel::Slug(owner, name),
            ..
        }) => Some(format!("{owner}/{name}")),
        Some(refs::RefSpec {
            repo: refs::RepoSel::Context,
            base: refs::Base::At,
            ..
        }) => at_context.as_ref().map(|context| context.repo.clone()),
        Some(refs::RefSpec {
            repo: refs::RepoSel::Local(_),
            ..
        }) => {
            ui::error("name the repository as owner/repo@ref");
            return Ok(ExitCode::Usage);
        }
        _ => None,
    };
    let slug = match explicit_repo {
        Some(slug) => slug,
        None => match super::context::resolve(&cwd) {
            Ok(context) => context.repo,
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Ref);
            }
        },
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let Some(repo) = Repo::open(crate::infra::config::repo_dir(&owner, &name)?) else {
        ui::error(&format!("{slug} does not exist locally."));
        return Ok(ExitCode::Precondition);
    };
    let (left_spec, right_spec, three_dot) = endpoints.expect("working-state mode handled above");
    let left_source = super::echo::Source::for_spec(&left_spec);
    let left_repo_explicit = matches!(left_spec.repo, refs::RepoSel::Slug(_, _));
    let right_repo = match right_spec.as_ref() {
        Some(spec) => match right_repository(&repo, spec, at_context.as_ref()) {
            Ok(repo) => repo,
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Precondition);
            }
        },
        None => repo.clone(),
    };
    let Some(a_sha) = (match resolve_spec(&repo, &left_spec, at_context.as_ref()) {
        Ok(sha) => sha,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    }) else {
        return Ok(ExitCode::Ref);
    };
    let b_sha = match right_spec.as_ref() {
        Some(spec) => match resolve_spec(&right_repo, spec, at_context.as_ref()) {
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

    let comparison = Comparison::new(&repo, &right_repo)?;
    let graph = comparison.repository();

    // Every output mode uses the same selected endpoints. Only a verified common ancestor
    // earns the fork-point label; unrelated histories retain their explicit left endpoint.
    let (base, real_fork) = if three_dot {
        match comparison.merge_base(&a_sha, &b_sha) {
            Ok(Some(base)) => (base, true),
            Ok(None) => {
                ui::warning("no common Git ancestor; comparing the explicit endpoints");
                (a_sha.clone(), false)
            }
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Precondition);
            }
        }
    } else {
        (a_sha.clone(), false)
    };

    if args.files {
        // Shared files = everything but the session itself. Excluding the whole `session/`
        // rather than each file name by name drops one way to fail silently: "the layout
        // changed and the exclude list did not follow".
        let out = graph.git(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
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

    let (right_slug, right_source) = match right_spec.as_ref() {
        Some(spec) => {
            let selected_slug = match (&spec.repo, &spec.base, at_context.as_ref()) {
                (refs::RepoSel::Slug(owner, name), _, _) => format!("{owner}/{name}"),
                (refs::RepoSel::Context, refs::Base::At, Some(context)) => context.repo.clone(),
                _ => slug.clone(),
            };
            let source = if left_repo_explicit
                && spec.repo == refs::RepoSel::Context
                && spec.base != refs::Base::At
            {
                super::echo::Source::Explicit
            } else {
                super::echo::Source::for_spec(spec)
            };
            (selected_slug, source)
        }
        None => (
            slug.clone(),
            if left_repo_explicit {
                super::echo::Source::Explicit
            } else {
                super::echo::Source::Environment
            },
        ),
    };
    let selections = [
        super::echo::Selection::new(format!("{slug}@{a_sha}"), left_source).role("left"),
        super::echo::Selection::new(format!("{right_slug}@{b_sha}"), right_source).role("right"),
    ];

    if args.view {
        let va = view_at(graph, &base)?;
        let vb = view_at(graph, &b_sha)?;
        super::echo::emit("diff", &selections);
        view_diff(&va, &vb);
        return Ok(ExitCode::Ok);
    }

    // Default: the --turns reconnaissance.
    let semantic = if real_fork {
        None
    } else if three_dot || matches!(comparison.merge_base(&a_sha, &b_sha), Ok(None)) {
        Some(SemanticPrefix::read(graph, &a_sha, &b_sha)?)
    } else {
        // Explicit endpoint comparison does not require a unique or complete ancestry graph.
        None
    };
    turns_report(graph, &base, &a_sha, &b_sha, real_fork, &selections)?;
    if let Some(semantic) = semantic {
        ui::semantic_prefix::print(&semantic, "A", "B");
    }
    Ok(ExitCode::Ok)
}

fn parse_endpoints(raw: &str) -> crate::Result<(refs::RefSpec, Option<refs::RefSpec>, bool)> {
    let (left, right, three_dot) = split_range(raw);
    let left = super::target::parse_spec(&left)?;
    let right = right.as_deref().map(parse_right).transpose()?;
    whole_point(&left)?;
    if let Some(right) = &right {
        whole_point(right)?;
    }
    Ok((left, right, three_dot))
}

fn parse_right(raw: &str) -> crate::Result<refs::RefSpec> {
    if raw
        .split_once('@')
        .is_some_and(|(repo, _)| repo.contains('/'))
    {
        super::target::parse_spec(raw)
    } else {
        crate::commands::target::parse_local(raw)
    }
}

fn whole_point(spec: &refs::RefSpec) -> crate::Result<()> {
    match spec.tail {
        refs::Tail::None | refs::Tail::Tilde(_) | refs::Tail::Turn(_) => Ok(()),
        _ => anyhow::bail!(
            "diff endpoints must name whole commits; event, turn-range, and path selectors are not supported"
        ),
    }
}

fn right_repository(
    left: &Repo,
    spec: &refs::RefSpec,
    at_context: Option<&super::context::Context>,
) -> crate::Result<Repo> {
    let slug = match &spec.repo {
        refs::RepoSel::Slug(owner, name) => Some(format!("{owner}/{name}")),
        refs::RepoSel::Context if spec.base == refs::Base::At => Some(
            at_context
                .ok_or_else(|| anyhow::anyhow!("missing current-session identity"))?
                .repo
                .clone(),
        ),
        refs::RepoSel::Context => None,
        refs::RepoSel::Local(_) => anyhow::bail!("name the repository as owner/repo@ref"),
    };
    match slug {
        None => Ok(left.clone()),
        Some(slug) => {
            let (owner, name) = super::parse_slug(&slug)?;
            Repo::open(crate::infra::config::repo_dir(&owner, &name)?)
                .ok_or_else(|| anyhow::anyhow!("{slug} does not exist locally."))
        }
    }
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

#[cfg(test)]
fn resolve_local_in(repo: &Repo, name: &str) -> crate::Result<Option<String>> {
    let spec = crate::commands::target::parse_local(name)?;
    resolve_spec(repo, &spec, None)
}

fn resolve_spec(
    repo: &Repo,
    spec: &refs::RefSpec,
    at_context: Option<&super::context::Context>,
) -> crate::Result<Option<String>> {
    let mut spec = spec.clone();
    if spec.base == refs::Base::At {
        let context =
            at_context.ok_or_else(|| anyhow::anyhow!("missing current-session identity"))?;
        let (owner, name) = super::parse_slug(&context.repo)?;
        let current = Repo::at(crate::infra::config::repo_dir(&owner, &name)?);
        if current.common_dir()?.canonicalize()? != repo.common_dir()?.canonicalize()? {
            anyhow::bail!(
                "`@` belongs to {}; name an explicit branch in the selected repository",
                context.repo
            );
        }
        spec.base = refs::Base::SessionBranch(context.branch.clone());
    }
    match refs::resolve(repo, &spec) {
        Ok(r) => Ok(Some(r.sha)),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(None)
        }
    }
}

fn turns_report(
    repo: &Repo,
    base: &str,
    a: &str,
    b: &str,
    three_dot: bool,
    selections: &[super::echo::Selection],
) -> crate::Result<()> {
    // The label follows the semantics: only the left end of `...` is the fork point, the left
    // end of `..` is `a` itself. Printing `fork point` in the two-point view is a lie — that
    // value is not a computed fork point, and the A side then counts zero new turns, which
    // reads as the tool having got it wrong.
    let label = if three_dot {
        "fork point"
    } else {
        "base      "
    };
    let prior: std::collections::HashSet<String> = repo
        .git(&["rev-list", base])?
        .lines()
        .map(str::to_owned)
        .collect();
    let mut sides = Vec::new();
    for (label, head) in [("A", a), ("B", b)] {
        let chain = refs::Chain::read(repo, head)?;
        let additions: Vec<String> = chain
            .turns()
            .into_iter()
            .filter(|(_, sha)| !prior.contains(*sha))
            .map(|(_, sha)| sha.to_owned())
            .collect();
        let mut previews = Vec::new();
        for sha in additions.iter().rev().take(10) {
            previews.push(repo.git(&["show", "-s", "--format=%h %s", sha])?);
        }
        sides.push((label, additions.len(), previews));
    }
    super::echo::emit("diff", selections);
    println!("{label}  {}", &base[..9.min(base.len())]);
    for (label, count, previews) in sides {
        println!("{label} side    +{count} turns");
        for preview in previews {
            println!("  {preview}");
        }
    }
    Ok(())
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
fn workdir_diff(repo: &Repo, context: &super::context::Context) -> CmdResult {
    super::migration::check_readonly_repo_startup_with(repo, pending::check_readonly_repository)?;
    let out = repo.git(&[
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
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
        println!("{out}");
    }
    match pending::inspect(repo, &context.repo, &context.branch) {
        Ok(summary) => {
            println!("pending {}@{}: {summary}", context.repo, context.branch);
            Ok(ExitCode::Ok)
        }
        Err(error) => {
            println!(
                "pending {}@{}: unavailable ({error:#})",
                context.repo, context.branch
            );
            Ok(ExitCode::Precondition)
        }
    }
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
