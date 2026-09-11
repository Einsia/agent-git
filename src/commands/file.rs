//! Ordinary files live in the selected branch's worktree and use its Git index.
//! Runtime settlement owns conversation paths; only an explicit file commit consumes this index.

use super::CmdResult;
use crate::domain::{mergetx, meta, repo::Repo};
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::Context as _;
use clap::Args as ClapArgs;
use std::path::{Component, Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// Select an existing branch explicitly; otherwise use AGIT_SESSION.
    #[arg(long, global = true, value_name = "owner/repo@branch")]
    pub into: Option<String>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Print the selected branch's file worktree directory.
    Cwd,
    /// Copy and stage files; external files default to artifacts/<name>.
    Add {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Destination relative to the file worktree (requires one source).
        #[arg(long, value_name = "path")]
        to: Option<String>,
    },
    /// Show staged and unstaged file changes.
    Status,
    /// Show unstaged changes, or the contents prepared for the next file commit.
    Diff {
        #[arg(long)]
        staged: bool,
        paths: Vec<String>,
    },
    /// Commit only staged files without settling any conversation turns.
    Commit {
        #[arg(short = 'm', long)]
        message: String,
    },
    /// List files at a committed version (defaults to this branch's HEAD).
    List {
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Copy a committed file to a local destination.
    Get {
        path: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Remove files from the worktree and stage their deletion.
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Stop tracking while keeping the local copy.
        #[arg(long)]
        cached: bool,
    },
    /// Move a file within the worktree and stage its rename.
    Mv { source: String, destination: String },
    /// Unstage selected files while keeping their working copies.
    Restore {
        #[arg(long, required = true)]
        staged: bool,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Print a Hub permalink containing the commit hash and file path.
    Link {
        path: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
}

struct Target {
    repo: Repo,
    slug: String,
    branch: String,
}

fn target(into: Option<&str>) -> crate::Result<Target> {
    let (slug, branch) = if let Some(raw) = into {
        let t = super::target::branch_only(raw)?;
        let slug = t.repo.context("--into requires owner/repo@branch")?;
        (slug, t.base.context("--into requires a branch")?)
    } else {
        let context = super::context::resolve(&std::env::current_dir()?)?;
        (super::context::qualify(&context.repo), context.branch)
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let primary = Repo::open(config::repo_dir(&owner, &name)?)
        .with_context(|| format!("{slug} does not exist locally"))?;
    let repo = super::worktree::checkout(&primary, &branch)?;
    super::plumbing::recover_interrupted_checkout(&repo)?;
    Ok(Target { repo, slug, branch })
}

fn ordinary(path: &str) -> crate::Result<()> {
    let root = path.split('/').next().unwrap_or_default();
    anyhow::ensure!(
        !path.is_empty()
            && !path.contains(['\0', '\n', '\r', '\\', ':'])
            && path.split('/').all(|p| !p.is_empty()
                && !p.ends_with(['.', ' '])
                && !p.eq_ignore_ascii_case(".git"))
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && !meta::is_storage_path(path)
            && !["session", "events", "log", "view"]
                .iter()
                .any(|reserved| root.eq_ignore_ascii_case(reserved)),
        "{path:?} is not an ordinary relative file path"
    );
    Ok(())
}

fn safe_parents(path: &Path) -> crate::Result<()> {
    for parent in path.ancestors() {
        match std::fs::symlink_metadata(parent) {
            Ok(m) => anyhow::ensure!(
                !m.file_type().is_symlink(),
                "{} is a symbolic link",
                parent.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn relative(path: &Path) -> crate::Result<String> {
    let text = path
        .to_str()
        .context("file paths must be valid UTF-8")?
        .replace(std::path::MAIN_SEPARATOR, "/");
    ordinary(&text)?;
    Ok(text)
}

fn plan_copy(
    source: &Path,
    destination: &str,
    repo: &Repo,
    files: &mut Vec<(PathBuf, String)>,
) -> crate::Result<()> {
    ordinary(destination)?;
    safe_parents(&repo.root().join(destination))?;
    let metadata = std::fs::symlink_metadata(source)?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "{} is a symbolic link",
        source.display()
    );
    if metadata.is_dir() {
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("file paths must be valid UTF-8"))?;
            plan_copy(&entry.path(), &format!("{destination}/{name}"), repo, files)?;
        }
    } else {
        anyhow::ensure!(
            metadata.is_file(),
            "{} is not a regular file",
            source.display()
        );
        files.push((source.to_path_buf(), destination.to_owned()));
    }
    Ok(())
}

fn add(repo: &Repo, sources: &[PathBuf], destination: Option<&str>) -> crate::Result<()> {
    anyhow::ensure!(
        destination.is_none() || sources.len() == 1,
        "--to requires exactly one source"
    );
    let cwd = std::env::current_dir()?.canonicalize()?;
    let root = repo.root().canonicalize()?;
    let mut files = Vec::new();
    let mut stage = std::collections::BTreeSet::new();
    for source in sources {
        let source = if source.is_absolute() {
            source.clone()
        } else {
            cwd.join(source)
        };
        // Check the leaf before canonicalizing so a link is never silently followed.
        safe_parents(&source)?;
        if destination.is_none() && source.starts_with(&root) {
            let source: PathBuf = source
                .strip_prefix(&root)?
                .components()
                .filter(|part| !matches!(part, Component::CurDir))
                .collect();
            let selection = source
                .to_str()
                .context("file paths must be valid UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            let selection = selection.trim_end_matches('/');
            let selection = if selection == "." { "" } else { selection };
            if !selection.is_empty() {
                ordinary(selection)?;
            }
            let candidates = repo.git_bytes_result(&[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])?;
            let before = stage.len();
            for candidate in candidates.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                let path = std::str::from_utf8(candidate)?;
                if ordinary(path).is_ok()
                    && (selection.is_empty()
                        || path == selection
                        || path.starts_with(&format!("{selection}/")))
                {
                    safe_parents(&root.join(path))?;
                    stage.insert(path.to_owned());
                }
            }
            anyhow::ensure!(
                stage.len() > before || selection.is_empty(),
                "no files match {selection}"
            );
            continue;
        }
        let source = source.canonicalize()?;
        let dest = match destination {
            Some(path) => path.to_owned(),
            None => match source.strip_prefix(&root) {
                Ok(path) => relative(path)?,
                Err(_) => format!(
                    "artifacts/{}",
                    source
                        .file_name()
                        .and_then(|s| s.to_str())
                        .context("the source needs a file name")?
                ),
            },
        };
        plan_copy(&source, &dest, repo, &mut files)?;
    }
    let mut destinations = std::collections::HashSet::new();
    for (_, dest) in &files {
        anyhow::ensure!(
            destinations.insert(dest),
            "multiple sources target {dest}; use separate --to paths"
        );
        stage.insert(dest.clone());
    }
    for (source, dest) in &files {
        let target = root.join(dest);
        if source != &target {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(source, &target)?;
        }
    }
    if !stage.is_empty() {
        let mut argv = vec!["--literal-pathspecs", "add", "--"];
        argv.extend(stage.iter().map(String::as_str));
        repo.git(&argv)?;
    }
    ui::success(&format!(
        "staged {} file(s); run `agit file commit -m <message>` to record them",
        stage.len()
    ));
    Ok(())
}

fn commit_ref(repo: &Repo, reference: Option<&str>) -> crate::Result<String> {
    let reference = reference.unwrap_or("HEAD");
    anyhow::ensure!(
        !reference.starts_with('-') && !reference.contains(['\0', '\n', '\r']),
        "invalid file ref"
    );
    Ok(repo
        .git(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ])?
        .trim()
        .to_owned())
}

fn committed_file(
    repo: &Repo,
    reference: Option<&str>,
    path: &str,
) -> crate::Result<(String, Vec<u8>)> {
    let commit = file_commit_ref(repo, reference, path)?;
    let spec = format!("{commit}:{path}");
    Ok((commit, repo.git_bytes_result(&["cat-file", "blob", &spec])?))
}

fn file_commit_ref(repo: &Repo, reference: Option<&str>, path: &str) -> crate::Result<String> {
    ordinary(path)?;
    let commit = commit_ref(repo, reference)?;
    let spec = format!("{commit}:{path}");
    anyhow::ensure!(
        repo.git(&["cat-file", "-t", &spec])?.trim() == "blob",
        "{path} is not a file"
    );
    Ok(commit)
}

fn encoded(text: &str) -> String {
    let mut output = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            output.push(byte as char);
        } else {
            use std::fmt::Write as _;
            write!(output, "%{byte:02X}").expect("write to string");
        }
    }
    output
}

fn permalink_hub(repo: &Repo, owner: &str, name: &str) -> crate::Result<String> {
    let origin = repo
        .remote_url()
        .context("this repository has no origin; publish it before generating a file permalink")?;
    let origin = crate::hub::identity::normalize_hub(&origin)?;
    crate::infra::hub_authority::HubAuthority::parse(&origin)?;
    let suffix = format!("/{}/{}", encoded(owner), encoded(name));
    let hub = origin
        .strip_suffix(&format!("{suffix}.git"))
        .or_else(|| origin.strip_suffix(&suffix))
        .context(
            "origin does not identify the selected repository; refusing to guess a Hub link",
        )?;
    let (_, route) = hub.split_once("://").context("invalid origin Hub URL")?;
    anyhow::ensure!(
        route
            .split('/')
            .skip(1)
            .all(|part| { !part.is_empty() && part != "." && part != ".." && !part.contains('%') }),
        "origin has an ambiguous Hub route; refusing to generate a file permalink"
    );
    crate::hub::identity::expected_for_transport(repo, hub)?;
    Ok(hub.to_owned())
}

pub fn run(args: Args) -> CmdResult {
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        anyhow::ensure!(
            std::env::var_os(variable).is_none(),
            "unset {variable} before managing branch files"
        );
    }
    let target = target(args.into.as_deref())?;
    let repo = &target.repo;
    let mutates = matches!(
        &args.cmd,
        Cmd::Add { .. }
            | Cmd::Commit { .. }
            | Cmd::Rm { .. }
            | Cmd::Mv { .. }
            | Cmd::Restore { .. }
    );
    let _branch_guard = if mutates {
        let store = crate::domain::store::Store::open_or_init()?;
        Some(crate::domain::link::lock_branch(
            &store,
            &target.slug,
            &target.branch,
        )?)
    } else {
        None
    };
    if mutates {
        anyhow::ensure!(
            !super::branch::is_sealed(repo, &target.branch),
            "{} is sealed",
            target.branch
        );
        anyhow::ensure!(
            mergetx::locking(repo.root(), &target.branch).is_none(),
            "{} is locked by a merge transaction",
            target.branch
        );
    }
    match args.cmd {
        Cmd::Cwd => println!("{}", repo.root().display()),
        Cmd::Add { paths, to } => add(repo, &paths, to.as_deref())?,
        Cmd::Status => {
            let output = repo.git(&["status", "--short", "--untracked-files=all"])?;
            if !output.is_empty() {
                println!("{output}");
            }
        }
        Cmd::Diff { staged, paths } => {
            for path in &paths {
                ordinary(path)?;
            }
            let mut argv = vec![
                "--literal-pathspecs",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
            ];
            if staged {
                argv.push("--cached");
            }
            argv.push("--");
            argv.extend(paths.iter().map(String::as_str));
            use std::io::Write as _;
            std::io::stdout().write_all(&repo.git_bytes_result(&argv)?)?;
        }
        Cmd::Commit { message } => {
            return super::commit::commit_staged_files(
                repo,
                &target.slug,
                &target.branch,
                &message,
            );
        }
        Cmd::List { git_ref } => {
            let commit = commit_ref(repo, git_ref.as_deref())?;
            let bytes = repo.git_bytes_result(&["ls-tree", "-rz", "--name-only", &commit])?;
            for path in bytes.split(|byte| *byte == 0).filter(|p| !p.is_empty()) {
                let path = std::str::from_utf8(path)?;
                if ordinary(path).is_ok() {
                    println!("{path}");
                }
            }
        }
        Cmd::Get {
            path,
            output,
            git_ref,
        } => {
            let (_, bytes) = committed_file(repo, git_ref.as_deref(), &path)?;
            let output = if output.is_absolute() {
                output
            } else {
                std::env::current_dir()?.canonicalize()?.join(output)
            };
            safe_parents(&output)?;
            std::fs::write(&output, bytes)?;
            println!("{}", output.display());
        }
        Cmd::Rm { paths, cached } => {
            for path in &paths {
                ordinary(path)?;
                safe_parents(&repo.root().join(path))?;
            }
            let mut argv = vec!["--literal-pathspecs", "rm", "-r"];
            if cached {
                argv.push("--cached");
            }
            argv.push("--");
            argv.extend(paths.iter().map(String::as_str));
            print!("{}", repo.git(&argv)?);
        }
        Cmd::Mv {
            source,
            destination,
        } => {
            ordinary(&source)?;
            ordinary(&destination)?;
            safe_parents(&repo.root().join(&source))?;
            safe_parents(&repo.root().join(&destination))?;
            repo.git(&["--literal-pathspecs", "mv", "--", &source, &destination])?;
        }
        Cmd::Restore { paths, .. } => {
            for path in &paths {
                ordinary(path)?;
            }
            let mut argv = vec!["--literal-pathspecs", "restore", "--staged", "--"];
            argv.extend(paths.iter().map(String::as_str));
            repo.git(&argv)?;
        }
        Cmd::Link { path, git_ref } => {
            let commit = file_commit_ref(repo, git_ref.as_deref(), &path)?;
            let metadata = meta::read_at_ref_result(repo, &commit)?
                .context("the version has no AgentGit metadata")?;
            let (owner, name) = super::parse_slug(&target.slug)?;
            let path = path.split('/').map(encoded).collect::<Vec<_>>().join("/");
            let base = format!(
                "{}/@{}/{}",
                permalink_hub(repo, &owner, &name)?,
                encoded(&owner),
                encoded(&name)
            );
            if metadata.is_file_line() {
                println!("{base}?tab=files&ref={commit}&file={path}");
            } else {
                println!(
                    "{base}/s/{}?ref={commit}&tab=files&file={path}",
                    encoded(&metadata.session)
                );
            }
        }
    }
    Ok(ExitCode::Ok)
}
