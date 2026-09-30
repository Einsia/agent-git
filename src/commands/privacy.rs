//! The privacy command manages the device-local publication allowlist.
//!
//! The policy file is outside the tracked AgentGit tree. The file action prints its location for
//! direct edits, while the other policy actions validate and save the same JSON through the CLI.
//! Preview evaluates only paths named by the caller; it never walks a directory implicitly.

use super::CmdResult;
use crate::domain::privacy::{ExternalRoot, PreviewReport, PrivacyPolicy, ReplacementRule};
use crate::domain::privacy_paths::PathAliasStore;
use crate::domain::repo::Repo;
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand};
use std::path::{Path, PathBuf};

mod keys;
pub(crate) mod publication_key;
pub(crate) mod sources;
mod unlock;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Action,
}

#[derive(Subcommand)]
pub enum Action {
    /// Initialize the repository viewing password (administrator access required).
    Init {
        /// Return browser setup or publishing readiness without prompting for a password.
        #[arg(long)]
        browser: bool,
        #[arg(value_name = "OWNER/REPO")]
        repository: String,
        /// Create a missing repository with public visibility after confirmation.
        #[arg(long, conflicts_with = "private")]
        public: bool,
        /// Create a missing repository privately (the default).
        #[arg(long)]
        private: bool,
        /// Creation-time encryption selection; password setup requires true. Without this flag or
        /// a stored preference, a repository this command creates is encrypted.
        #[arg(long, require_equals = true, value_name = "true|false")]
        encryption: Option<bool>,
    },
    /// Change the repository viewing password while retaining its current key.
    ChangePassword {
        #[arg(value_name = "OWNER/REPO")]
        repository: String,
    },
    /// Generate a new viewing key for future publications; retain historical keys.
    RotateKey {
        #[arg(value_name = "OWNER/REPO")]
        repository: String,
    },
    /// Recover an encrypted session using its repository password or an explicitly saved key.
    Unlock {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
        /// Rebind recovery to this directory; otherwise use the configured local workspace.
        #[arg(long, value_name = "DIRECTORY")]
        workspace: Option<PathBuf>,
        /// Save the viewing key in the OS credential store for this many hours (1-720).
        #[arg(long, value_name = "HOURS", value_parser = clap::value_parser!(u16).range(1..=720), conflicts_with = "use_saved_key")]
        remember_for: Option<u16>,
        /// Reuse an unexpired OS key after checking repository read access and its recipient.
        #[arg(long)]
        use_saved_key: bool,
    },
    /// Remove a saved unlock key for the configured Hub, without contacting the server.
    ForgetUnlock {
        #[arg(value_name = "OWNER/REPO")]
        repository: String,
    },
    /// Inspect or change the device-local policy.
    Policy {
        #[command(subcommand)]
        command: PolicyAction,
    },
    /// Preview explicitly selected file and memory candidates.
    Preview {
        /// owner/repo; omitted uses the explicit AGIT_SESSION target.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        /// Branch-specific restrictions to apply.
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
        /// Local files to evaluate. A directory is not recursively walked.
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
        /// Relative memory paths to evaluate.
        #[arg(long = "memory", value_name = "PATH")]
        memory: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum PolicyAction {
    /// Print the policy and its non-reversible digest.
    Show {
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Print the local JSON file path without opening or creating it.
    File {
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Set the authorized workspace root.
    SetWorkspace {
        path: PathBuf,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Remove the workspace root. File candidates then require an external root.
    ClearWorkspace {
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add a repository-level include pattern.
    Include {
        pattern: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add a repository-level exclude pattern.
    Exclude {
        pattern: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add a memory allowlist pattern.
    AllowMemory {
        pattern: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Remove a memory allowlist pattern.
    DenyMemory {
        pattern: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add or replace an explicitly authorized external root.
    AddExternal {
        label: String,
        path: PathBuf,
        /// Patterns relative to the external root. Repeat for several classes.
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Patterns relative to the external root that remain excluded.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Remove an external root by label.
    RemoveExternal {
        label: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add a branch-only exclusion pattern.
    RestrictBranch {
        branch: String,
        pattern: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add a branch-only memory exclusion pattern.
    RestrictMemory {
        branch: String,
        pattern: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Add a content replacement for the later projection stage.
    Replace {
        pattern: String,
        replacement: String,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        #[arg(long)]
        regex: bool,
    },
}

pub fn run(args: Args) -> CmdResult {
    match args.command {
        Action::Init {
            repository,
            public,
            encryption,
            browser,
            ..
        } => keys::run(
            &repository,
            if browser {
                keys::Operation::BrowserInitialize
            } else {
                keys::Operation::Initialize
            },
            public,
            encryption,
        ),
        Action::ChangePassword { repository } => {
            keys::run(&repository, keys::Operation::Rewrap, false, None)
        }
        Action::RotateKey { repository } => {
            keys::run(&repository, keys::Operation::Rotate, false, None)
        }
        Action::Unlock {
            target,
            workspace,
            remember_for,
            use_saved_key,
        } => unlock::run(&target, workspace.as_deref(), remember_for, use_saved_key),
        Action::ForgetUnlock { repository } => unlock::forget(&repository),
        Action::Policy { command } => run_policy(command),
        Action::Preview {
            repo,
            branch,
            paths,
            memory,
        } => preview(repo.as_deref(), branch.as_deref(), paths, memory),
    }
}

fn run_policy(command: PolicyAction) -> CmdResult {
    let repo_arg = match &command {
        PolicyAction::Show { repo }
        | PolicyAction::File { repo }
        | PolicyAction::SetWorkspace { repo, .. }
        | PolicyAction::ClearWorkspace { repo }
        | PolicyAction::Include { repo, .. }
        | PolicyAction::Exclude { repo, .. }
        | PolicyAction::AllowMemory { repo, .. }
        | PolicyAction::DenyMemory { repo, .. }
        | PolicyAction::AddExternal { repo, .. }
        | PolicyAction::RemoveExternal { repo, .. }
        | PolicyAction::RestrictBranch { repo, .. }
        | PolicyAction::RestrictMemory { repo, .. }
        | PolicyAction::Replace { repo, .. } => repo.as_deref(),
    };
    let (repo, slug) = resolve_repo(repo_arg)?;
    match command {
        PolicyAction::Show { .. } => show(&repo, &slug),
        PolicyAction::File { .. } => file(&repo),
        PolicyAction::SetWorkspace { path, .. } => mutate(
            &repo,
            |policy| {
                policy.workspace = Some(absolute_directory(&path)?);
                Ok(())
            },
            "workspace root updated",
        ),
        PolicyAction::ClearWorkspace { .. } => mutate(
            &repo,
            |policy| {
                policy.workspace = None;
                Ok(())
            },
            "workspace root cleared",
        ),
        PolicyAction::Include { pattern, .. } => mutate(
            &repo,
            |policy| add_pattern(&mut policy.include, pattern),
            "include pattern added",
        ),
        PolicyAction::Exclude { pattern, .. } => mutate(
            &repo,
            |policy| add_pattern(&mut policy.exclude, pattern),
            "exclude pattern added",
        ),
        PolicyAction::AllowMemory { pattern, .. } => mutate(
            &repo,
            |policy| add_pattern(&mut policy.memory_allow, pattern),
            "memory allowlist pattern added",
        ),
        PolicyAction::DenyMemory { pattern, .. } => mutate(
            &repo,
            |policy| {
                policy.memory_allow.retain(|item| item != &pattern);
                Ok(())
            },
            "memory allowlist pattern removed",
        ),
        PolicyAction::AddExternal {
            label,
            path,
            include,
            exclude,
            ..
        } => mutate(
            &repo,
            |policy| {
                let root = ExternalRoot {
                    label,
                    path: absolute_directory(&path)?,
                    include,
                    exclude,
                };
                policy
                    .external_roots
                    .retain(|item| item.label != root.label);
                policy.external_roots.push(root);
                Ok(())
            },
            "external root updated",
        ),
        PolicyAction::RemoveExternal { label, .. } => mutate(
            &repo,
            |policy| {
                policy.external_roots.retain(|item| item.label != label);
                Ok(())
            },
            "external root removed",
        ),
        PolicyAction::RestrictBranch {
            branch, pattern, ..
        } => mutate(
            &repo,
            |policy| {
                policy
                    .branches
                    .entry(branch)
                    .or_default()
                    .exclude
                    .push(pattern);
                Ok(())
            },
            "branch exclusion added",
        ),
        PolicyAction::RestrictMemory {
            branch, pattern, ..
        } => mutate(
            &repo,
            |policy| {
                policy
                    .branches
                    .entry(branch)
                    .or_default()
                    .memory_exclude
                    .push(pattern);
                Ok(())
            },
            "branch memory exclusion added",
        ),
        PolicyAction::Replace {
            pattern,
            replacement,
            regex,
            ..
        } => mutate(
            &repo,
            |policy| {
                policy.replacements.push(ReplacementRule {
                    pattern,
                    replacement,
                    regex,
                });
                Ok(())
            },
            "replacement rule added",
        ),
    }
    .map(|_| ExitCode::Ok)
}

fn show(repo: &Repo, slug: &str) -> Result<()> {
    let policy = PrivacyPolicy::load(repo)?;
    let digest = policy.digest()?;
    if super::json::requested() {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "operation": "privacy_policy_show",
                "repository": slug,
                "policy": policy,
                "policy_digest": digest,
                "file": PrivacyPolicy::path(repo)?,
            })
        );
    } else {
        println!("repository: {slug}");
        println!("policy digest: {digest}");
        println!("policy file: {}", PrivacyPolicy::path(repo)?.display());
        println!("{}", serde_json::to_string_pretty(&policy)?);
    }
    Ok(())
}

fn file(repo: &Repo) -> Result<()> {
    println!("{}", PrivacyPolicy::path(repo)?.display());
    Ok(())
}

fn preview(
    slug: Option<&str>,
    branch: Option<&str>,
    paths: Vec<PathBuf>,
    memory: Vec<String>,
) -> CmdResult {
    ensure!(
        !paths.is_empty() || !memory.is_empty(),
        "privacy preview needs at least one file path or --memory path"
    );
    let (repo, repository) = resolve_repo(slug)?;
    let policy = PrivacyPolicy::load(&repo)?;
    let references = paths.iter().map(PathBuf::as_path).collect::<Vec<_>>();
    let memory = memory.iter().map(String::as_str).collect::<Vec<_>>();
    let report = PathAliasStore::transact(&repo, |aliases| {
        aliases.preview(&policy, branch, references, &memory)
    })?;
    emit_preview(&repository, report)?;
    Ok(ExitCode::Ok)
}

fn emit_preview(repository: &str, report: PreviewReport) -> Result<()> {
    if super::json::requested() {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "operation": "privacy_preview",
                "repository": repository,
                "report": report,
            })
        );
        return Ok(());
    }
    println!("repository: {repository}");
    println!("policy digest: {}", report.policy_digest);
    if let Some(branch) = report.branch.as_deref() {
        println!("branch: {branch}");
    }
    println!(
        "allowed: {}  excluded: {}  review: {}",
        report.allowed, report.excluded, report.review
    );
    for candidate in report.candidates {
        let logical = candidate.logical_path.unwrap_or_else(|| "(hidden)".into());
        let reason = candidate.reason.unwrap_or_default();
        let suffix = if reason.is_empty() {
            String::new()
        } else {
            format!(" ({reason})")
        };
        println!(
            "{:?}: {} -> {} [rule: {}]{}",
            candidate.action, candidate.input, logical, candidate.rule, suffix
        );
    }
    Ok(())
}

fn mutate(
    repo: &Repo,
    change: impl FnOnce(&mut PrivacyPolicy) -> Result<()>,
    message: &str,
) -> Result<()> {
    let mut policy = PrivacyPolicy::load_local(repo)?;
    change(&mut policy)?;
    // Validate mandatory sources before saving, then report the effective policy digest.
    policy.mandatory = crate::domain::privacy::mandatory::load()?;
    policy.save(repo)?;
    if super::json::requested() {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "operation": "privacy_policy_update",
                "message": message,
                "policy_digest": policy.digest()?,
                "file": PrivacyPolicy::path(repo)?,
            })
        );
    } else {
        ui::success(message);
        println!("policy file: {}", PrivacyPolicy::path(repo)?.display());
        println!("policy digest: {}", policy.digest()?);
    }
    Ok(())
}

fn add_pattern(patterns: &mut Vec<String>, pattern: String) -> Result<()> {
    ensure!(!pattern.is_empty(), "privacy patterns must not be empty");
    if !patterns.contains(&pattern) {
        patterns.push(pattern);
    }
    Ok(())
}

fn absolute_directory(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = absolute
        .canonicalize()
        .with_context(|| format!("cannot resolve directory {}", absolute.display()))?;
    ensure!(
        canonical.is_dir(),
        "privacy root is not a directory: {}",
        canonical.display()
    );
    Ok(canonical)
}

fn resolve_repo(slug: Option<&str>) -> Result<(Repo, String)> {
    let slug = match slug {
        Some(slug) => slug.to_owned(),
        None => crate::commands::context::resolve(&std::env::current_dir()?)?.repo,
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let path = config::repo_dir(&owner, &name)?;
    let repo = Repo::open(path)
        .with_context(|| format!("{slug} is not a local Agent repository; clone it first"))?;
    Ok((repo, format!("{owner}/{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn repository_password_commands_require_explicit_repository_and_no_password_argument() {
        for operation in ["init", "change-password", "rotate-key"] {
            assert!(Wrapper::try_parse_from(["privacy", operation, "alice/app"]).is_ok());
            assert!(Wrapper::try_parse_from(["privacy", operation]).is_err());
            assert!(
                Wrapper::try_parse_from([
                    "privacy",
                    operation,
                    "alice/app",
                    "--password",
                    "secret"
                ])
                .is_err()
            );
        }
        assert!(matches!(
            Wrapper::try_parse_from(["privacy", "init", "alice/app", "--browser"])
                .unwrap()
                .args
                .command,
            Action::Init {
                browser: true,
                public: false,
                ..
            }
        ));
        for operation in ["change-password", "rotate-key"] {
            assert!(
                Wrapper::try_parse_from(["privacy", operation, "alice/app", "--browser"]).is_err()
            );
        }
        assert!(
            Wrapper::try_parse_from(["privacy", "init", "alice/app", "--public", "--private"])
                .is_err()
        );
        assert!(matches!(
            Wrapper::try_parse_from(["privacy", "init", "alice/app"])
                .unwrap()
                .args
                .command,
            Action::Init { public: false, .. }
        ));
    }

    #[test]
    fn unlock_storage_requires_explicit_lifetime_or_reuse() {
        let parsed = Wrapper::try_parse_from(["privacy", "unlock", "me/app@work"]).unwrap();
        assert!(matches!(
            parsed.args.command,
            Action::Unlock {
                remember_for: None,
                use_saved_key: false,
                ..
            }
        ));
        let parsed =
            Wrapper::try_parse_from(["privacy", "unlock", "me/app@work", "--remember-for", "24"])
                .unwrap();
        assert!(matches!(
            parsed.args.command,
            Action::Unlock {
                remember_for: Some(24),
                ..
            }
        ));
        let parsed =
            Wrapper::try_parse_from(["privacy", "unlock", "me/app@work", "--use-saved-key"])
                .unwrap();
        assert!(matches!(
            parsed.args.command,
            Action::Unlock {
                use_saved_key: true,
                ..
            }
        ));
        for args in [
            vec!["--remember-for", "0"],
            vec!["--remember-for", "721"],
            vec!["--remember-for", "24", "--use-saved-key"],
        ] {
            assert!(
                Wrapper::try_parse_from(
                    ["privacy", "unlock", "me/app@work"].into_iter().chain(args)
                )
                .is_err()
            );
        }
        assert!(Wrapper::try_parse_from(["privacy", "forget-unlock", "me/app"]).is_ok());
        assert!(
            Wrapper::try_parse_from([
                "privacy",
                "forget-unlock",
                "me/app",
                "--account",
                "account-1"
            ])
            .is_err()
        );
        assert!(
            Wrapper::try_parse_from(["privacy", "unlock", "me/app@work", "--no-browser"]).is_err()
        );
    }

    #[test]
    fn policy_and_preview_forms_parse() {
        let parsed =
            Wrapper::try_parse_from(["privacy", "policy", "include", "src/**", "--repo", "me/app"])
                .unwrap();
        assert!(matches!(
            parsed.args.command,
            Action::Policy {
                command: PolicyAction::Include { .. }
            }
        ));
        let parsed = Wrapper::try_parse_from([
            "privacy",
            "preview",
            "src/main.rs",
            "--memory",
            "team.md",
            "--repo",
            "me/app",
        ])
        .unwrap();
        assert!(matches!(parsed.args.command, Action::Preview { .. }));
    }

    #[test]
    fn relative_directories_are_resolved_and_files_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("workspace");
        std::fs::create_dir_all(&directory).unwrap();
        assert_eq!(
            absolute_directory(&directory).unwrap(),
            directory.canonicalize().unwrap()
        );
        let file = directory.join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(absolute_directory(&file).is_err());
    }
}
