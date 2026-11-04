//! Values an earlier agit replaced with placeholders although they were no credentials (digests,
//! data URIs, links, ordinary lines) are put back into a branch's files from this device's
//! dictionary, and protected again under the current policy, so a real credential stays a
//! placeholder, in the current spelling. The repair is one forward file commit: history keeps its
//! placeholders.

use super::CmdResult;
use crate::domain::privacy::{projector, service};
use crate::domain::{mergetx, meta, repo::Repo};
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::Context as _;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Put back values an earlier agit replaced in a branch's files; real credentials stay replaced
    Placeholders {
        /// `owner/repo` for every file line, or `owner/repo@branch`
        target: String,
        /// Show what would change without committing
        #[arg(long)]
        dry_run: bool,
    },
}

/// The message of the commit a repair lands as.
pub(crate) const MESSAGE: &str = "agit: restore values an earlier agit replaced";

const PLACEHOLDER: &str = "{{AGIT_SECRET_V";

/// One file the repair changes.
struct Repaired {
    path: String,
    content: String,
    restored: usize,
}

/// A placeholder whose original this device's dictionary does not hold.
struct Unresolved {
    path: String,
    line: usize,
}

pub fn run(args: Args) -> CmdResult {
    let Cmd::Placeholders { target, dry_run } = args.cmd;
    let parsed = super::target::parse(&target)?;
    let slug = parsed
        .repo
        .context("name the repository: `owner/repo` or `owner/repo@branch`")?;
    let (owner, name) = super::parse_slug(&slug)?;
    let primary = Repo::open(config::repo_dir(&owner, &name)?)
        .with_context(|| format!("{slug} does not exist locally"))?;
    let branches = match parsed.base {
        Some(branch) => vec![branch],
        None => primary
            .local_branches()
            .into_iter()
            .filter(|branch| meta::is_file_line_at(&primary, branch))
            .collect(),
    };
    let mut unresolved_any = false;
    let mut repaired_any = false;
    let store = crate::domain::store::Store::open_or_init()?;
    for branch in &branches {
        // Everything for this branch reads one tip, under its lock: a commit landing between the
        // plan and the repair would otherwise be overwritten with the contents planned from before it.
        let _guard = crate::domain::link::lock_branch(&store, &slug, branch)?;
        let tip = primary
            .git(&["rev-parse", "--verify", &format!("{branch}^{{commit}}")])?
            .trim()
            .to_owned();
        let (repaired, unresolved) = plan(&primary, &tip)?;
        unresolved_any |= !unresolved.is_empty();
        if repaired.is_empty() && unresolved.is_empty() {
            continue;
        }
        let restored: usize = repaired.iter().map(|file| file.restored).sum();
        let files = repaired.len();
        if dry_run {
            ui::info(format_args!(
                "{branch}: would restore {restored} value(s) in {files} file(s)"
            ));
        } else if !repaired.is_empty() {
            commit(&primary, &slug, branch, &tip, &repaired)?;
            repaired_any = true;
            ui::success(&format!(
                "{branch}: restored {restored} value(s) in {files} file(s)"
            ));
        }
        for file in &repaired {
            ui::info(format_args!("  {}", file.path));
        }
        for left in &unresolved {
            ui::warning(&format!(
                "{branch}:{}:{}: a placeholder whose value this device's dictionary does not hold",
                left.path, left.line
            ));
        }
    }
    if unresolved_any {
        ui::hint(
            "sign in on this device so its dictionary syncs, or run the repair on the device that published these files",
        );
    }
    if repaired_any {
        // What the current check doesn't treat as a credential is back in the files, and the older
        // check may have replaced a key of a form the current one doesn't know.
        ui::hint(&format!(
            "look through the files listed above, then `agit push {slug}` publishes them; everything the current check doesn't treat as a credential is back in them"
        ));
    } else if !unresolved_any && !dry_run {
        ui::info("nothing to repair: no file holds a placeholder");
    }
    Ok(if unresolved_any {
        ExitCode::Precondition
    } else {
        ExitCode::Ok
    })
}

/// The text files at `rev` that hold a placeholder. Names are read NUL-separated: git quotes and
/// escapes a name that isn't plain ASCII otherwise, and that spelling names no file.
pub(crate) fn affected(repo: &Repo, rev: &str) -> Vec<String> {
    // `git grep` exits non-zero when nothing matches.
    let found = repo
        .git_bytes_result(&["grep", "-z", "-I", "-l", "-F", "-e", PLACEHOLDER, rev, "--"])
        .unwrap_or_default();
    let prefix = format!("{rev}:");
    found
        .split(|byte| *byte == 0)
        .filter_map(|name| std::str::from_utf8(name).ok())
        .filter_map(|name| name.strip_prefix(&prefix))
        .filter(|path| *path != meta::FILE)
        .map(str::to_owned)
        .collect()
}

/// The one line `agit push` shows for a branch whose files still hold placeholders.
pub(crate) fn hint(repo: &Repo, slug: &str, branch: &str) -> Option<String> {
    let files = affected(repo, branch).len();
    (files > 0).then(|| {
        format!(
            "{files} file(s) on {branch} still hold placeholders from an older agit; `agit repair placeholders {slug}@{branch}` puts back the values that are not credentials"
        )
    })
}

/// Each affected file at `tip` restored from the dictionary and protected again, and the
/// placeholders whose originals this device does not hold.
fn plan(repo: &Repo, tip: &str) -> crate::Result<(Vec<Repaired>, Vec<Unresolved>)> {
    let mut repaired = Vec::new();
    let mut unresolved = Vec::new();
    for path in affected(repo, tip) {
        let bytes = repo.git_bytes_result(&["show", &format!("{tip}:{path}")])?;
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let (hydrated, staged) = if is_json(&text) {
            hydrate_json(repo, &text)
        } else {
            hydrate_text(repo, &text).map(|hydrated| (hydrated.clone(), hydrated))
        }
        .with_context(|| {
            format!("the privacy check for {path} could not run; retrying usually works")
        })?;
        for (start, _, _) in projector::tokens(&hydrated) {
            unresolved.push(Unresolved {
                path: path.clone(),
                line: hydrated[..start].matches('\n').count() + 1,
            });
        }
        // Every file is protected as text: the record mode keeps a transcript's protocol fields
        // such as `id` as they are, and a file's own `id` can hold a credential. A placeholder
        // carries no quote or backslash, so a JSON string stays valid around it.
        let protected =
            service::protect_for_commit(Some(repo.root()), &staged, projector::Mode::ProtectText)?;
        if protected != text {
            let restored = projector::tokens(&text)
                .count()
                .saturating_sub(projector::tokens(&protected).count());
            repaired.push(Repaired {
                path,
                content: protected,
                restored,
            });
        }
    }
    Ok((repaired, unresolved))
}

/// `text` with its placeholders replaced by what this device's dictionary holds.
fn hydrate_text(repo: &Repo, text: &str) -> crate::Result<String> {
    let hydrated = service::transform(Some(repo.root()), text, projector::Mode::HydrateText);
    anyhow::ensure!(
        hydrated.status != projector::Status::Skipped && hydrated.unprocessed == 0,
        "the privacy check did not run"
    );
    Ok(hydrated.content)
}

/// Whether `text` is one JSON document, or JSON on every line. A placeholder in such a file stands
/// inside a string.
fn is_json(text: &str) -> bool {
    let valid = |text: &str| serde_json::from_str::<serde_json::Value>(text).is_ok();
    valid(text) || text.lines().all(valid)
}

/// [`hydrate_text`] for a JSON file. Each string that holds a placeholder is decoded, restored and
/// written back as JSON, so a quote, a backslash or a newline in a value leaves the file valid;
/// the rest of the file keeps its bytes.
///
/// Answers the restored file and the one to protect, where each such string was protected whole,
/// as it reads decoded, before it was written back. A rule matches a value as it stands, with the
/// rest of its string around it, so protecting only the escaped file, or each value on its own,
/// would write back a value the person blocked.
fn hydrate_json(repo: &Repo, text: &str) -> crate::Result<(String, String)> {
    let mut hydrated = String::with_capacity(text.len());
    let mut staged = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in string_literals(text) {
        let literal = &text[start..end];
        if projector::tokens(literal).next().is_none() {
            continue;
        }
        let value: String = serde_json::from_str(literal)?;
        let restored = hydrate_text(repo, &value)?;
        let protected = service::protect_for_commit(
            Some(repo.root()),
            &restored,
            projector::Mode::ProtectText,
        )?;
        hydrated.push_str(&text[at..start]);
        staged.push_str(&text[at..start]);
        hydrated.push_str(&serde_json::to_string(&restored)?);
        staged.push_str(&serde_json::to_string(&protected)?);
        at = end;
    }
    hydrated.push_str(&text[at..]);
    staged.push_str(&text[at..]);
    Ok((hydrated, staged))
}

/// The byte ranges of the string literals in `text`, a JSON document or JSON lines, quotes
/// included.
fn string_literals(text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut open = None;
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        match open {
            None if byte == b'"' => open = Some(index),
            None => {}
            Some(_) if escaped => escaped = false,
            Some(_) if byte == b'\\' => escaped = true,
            Some(start) if byte == b'"' => {
                found.push((start, index + 1));
                open = None;
            }
            Some(_) => {}
        }
    }
    found
}

/// Land `repaired` on `branch` as one file commit in its worktree, on top of `tip`, the commit the
/// plan read. The caller holds the branch's lock.
fn commit(
    primary: &Repo,
    slug: &str,
    branch: &str,
    tip: &str,
    repaired: &[Repaired],
) -> crate::Result<()> {
    let repo = super::worktree::checkout(primary, branch)?;
    super::plumbing::recover_interrupted_checkout(&repo)?;
    anyhow::ensure!(
        repo.git(&["rev-parse", "HEAD"])?.trim() == tip,
        "{branch} moved while it was being repaired; repair it again"
    );
    anyhow::ensure!(
        !super::branch::is_sealed(&repo, branch),
        "{branch} is sealed"
    );
    anyhow::ensure!(
        mergetx::locking(repo.root(), branch).is_none(),
        "{branch} is locked by a merge transaction"
    );
    // Anything else in the worktree would land in the same commit.
    anyhow::ensure!(
        repo.git(&["status", "--porcelain", "--untracked-files=no"])?
            .trim()
            .is_empty(),
        "{branch}'s file worktree has uncommitted changes; commit or discard them, then repair again"
    );
    for file in repaired {
        std::fs::write(repo.root().join(&file.path), &file.content)?;
    }
    let mut add = vec!["add", "--"];
    add.extend(repaired.iter().map(|file| file.path.as_str()));
    repo.git(&add)?;
    super::commit::commit_staged_files(&repo, slug, branch, MESSAGE)?;
    Ok(())
}
