//! `agit secrets` — manage the device-local vault of low-entropy literal secrets.
//!
//! A secret never travels through argv: interactive input does not echo, and a non-interactive
//! run must say `--stdin` explicitly. This command returns only the opaque id and the label the
//! user chose; there is no show / decrypt / export entry point at all.

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::domain::secret_filter::{
    RecordSummary, RepositoryDictionary, RepositoryRecordSummary, VaultStore,
};
use crate::{ExitCode, ui};
use clap::{Args as ClapArgs, Subcommand};
use std::io::Read as _;
use std::path::PathBuf;
use zeroize::Zeroizing;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Register a literal secret (hidden prompt; use --stdin for automation)
    Add {
        /// Non-secret label shown by list/status.
        name: String,
        /// Read one secret from stdin instead of prompting.
        #[arg(long)]
        stdin: bool,
        /// Permit a 4-7 byte rule (high false-positive and enumeration risk). comment-rule-allow: clap help text; the length range is this command's contract with the user
        #[arg(long)]
        allow_short: bool,
    },
    /// List opaque ids and user-provided labels; never decrypt values to stdout
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove one record by opaque id or exact label
    Remove {
        id_or_name: String,
        /// Confirm the irreversible deletion without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Verify that the vault and every encrypted record can be authenticated
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Review repository-local candidate policy without revealing values
    Review {
        /// AgentGit repository checkout (defaults to the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Allow a heuristic candidate in future projections (old keys still hydrate)
    Allow {
        record_id: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Restore default protection for an allowed heuristic candidate
    Unallow {
        record_id: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Manage exact repository-local block rules
    Block {
        #[command(subcommand)]
        command: BlockAction,
    },
}

#[derive(Subcommand)]
enum BlockAction {
    /// Add an exact literal rule (hidden prompt; use --stdin for automation)
    Add {
        name: String,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        allow_short: bool,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Clear the explicit block bit; heuristic policy may still protect it
    Remove {
        record_id: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

pub fn run(args: Args) -> CmdResult {
    let store = VaultStore::open_default()?;
    match args.command {
        Action::Add {
            name,
            stdin,
            allow_short,
        } => {
            let secret = if stdin {
                read_secret_stdin()?
            } else {
                let Some(first) = ui::prompt::password("Secret")? else {
                    ui::error("an interactive terminal is required; automation must use `--stdin`");
                    return Ok(ExitCode::Interactive);
                };
                let Some(second) = ui::prompt::password("Secret again")? else {
                    ui::error("could not read the confirmation");
                    return Ok(ExitCode::Interactive);
                };
                if first != second {
                    ui::error("the two secret values did not match; nothing was saved");
                    return Ok(ExitCode::Usage);
                }
                Zeroizing::new(first)
            };
            let added = store.add(&name, secret, allow_short)?;
            reload_daemon()?;
            ui::success(&format!("registered {} ({})", added.name, added.id));
            Ok(ExitCode::Ok)
        }
        Action::List { json } => {
            let records = store.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else if records.is_empty() {
                println!("no registered secrets");
            } else {
                for record in records {
                    print_record(&record);
                }
            }
            Ok(ExitCode::Ok)
        }
        Action::Remove { id_or_name, yes } => {
            if !yes {
                match ui::prompt::confirm(
                    &format!("Permanently remove registered secret `{id_or_name}`?"),
                    false,
                )? {
                    Some(true) => {}
                    Some(false) => return Ok(ExitCode::Ok),
                    None => {
                        ui::error("non-interactive removal requires `--yes`");
                        return Ok(ExitCode::Interactive);
                    }
                }
            }
            let removed = store.remove(&id_or_name)?;
            reload_daemon()?;
            ui::success(&format!("removed {} ({})", removed.name, removed.id));
            Ok(ExitCode::Ok)
        }
        Action::Status { json } => {
            let status = store.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if status.initialized {
                ui::success(&format!(
                    "secret-filter vault is healthy ({} rules, generation {})",
                    status.rules, status.generation
                ));
            } else {
                println!("secret-filter vault is not initialized (0 rules)");
            }
            Ok(ExitCode::Ok)
        }
        Action::Review { repo, json } => {
            let dictionary = repository_dictionary(repo)?;
            let records = dictionary.review()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else if records.is_empty() {
                println!("no repository-local secret records");
            } else {
                for record in &records {
                    print_repository_record(record);
                }
            }
            Ok(ExitCode::Ok)
        }
        Action::Allow { record_id, repo } => {
            let changed = repository_dictionary(repo)?.allow(&record_id)?;
            ui::success(&format!(
                "allowed heuristic record {} (old placeholders remain hydratable)",
                changed.id
            ));
            Ok(ExitCode::Ok)
        }
        Action::Unallow { record_id, repo } => {
            let changed = repository_dictionary(repo)?.unallow(&record_id)?;
            ui::success(&format!("restored protection for {}", changed.id));
            Ok(ExitCode::Ok)
        }
        Action::Block { command } => match command {
            BlockAction::Add {
                name,
                stdin,
                allow_short,
                repo,
            } => {
                let secret = read_new_secret(stdin)?;
                let added = repository_dictionary(repo)?.block_add(&name, secret, allow_short)?;
                ui::success(&format!(
                    "repository block rule {} ({})",
                    added.name, added.id
                ));
                Ok(ExitCode::Ok)
            }
            BlockAction::Remove { record_id, repo } => {
                let changed = repository_dictionary(repo)?.block_remove(&record_id)?;
                ui::success(&format!("cleared explicit block for {}", changed.id));
                Ok(ExitCode::Ok)
            }
        },
    }
}

fn read_new_secret(stdin: bool) -> crate::Result<Zeroizing<String>> {
    if stdin {
        return read_secret_stdin();
    }
    let Some(first) = ui::prompt::password("Secret")? else {
        anyhow::bail!("an interactive terminal is required; automation must use `--stdin`");
    };
    let Some(second) = ui::prompt::password("Secret again")? else {
        anyhow::bail!("could not read the confirmation");
    };
    if first != second {
        anyhow::bail!("the two secret values did not match; nothing was saved");
    }
    Ok(Zeroizing::new(first))
}

fn repository_dictionary(repo: Option<PathBuf>) -> crate::Result<RepositoryDictionary> {
    let path = match repo {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let Some(repo) = Repo::open(&path) else {
        anyhow::bail!(
            "{} is not an AgentGit repository checkout; pass --repo <path>",
            path.display()
        );
    };
    RepositoryDictionary::open(repo.root())
}

fn print_repository_record(record: &RepositoryRecordSummary) {
    println!(
        "{}\t{}\torigins={}\theuristic={:?}\texplicit={}\tactive={}",
        record.id,
        record.name,
        record.origins.join(","),
        record.heuristic_disposition,
        record.explicit_block,
        record.effective_protect
    );
}

fn read_secret_stdin() -> crate::Result<Zeroizing<String>> {
    let mut value = String::new();
    std::io::stdin().read_to_string(&mut value)?;
    // A pipe is most often `printf ...` or a one-line file. Strip one terminating newline only;
    // any other surrounding whitespace is a real part of the secret and must not be trimmed the
    // way a token argument is.
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(Zeroizing::new(value))
}

fn print_record(record: &RecordSummary) {
    println!("{}\t{}", record.id, record.name);
}

/// A running daemon must switch to the new matcher before this command returns; one that is not
/// running loads it on its next start.
#[cfg(unix)]
fn reload_daemon() -> crate::Result<()> {
    use crate::rc::control::{Presence, Reply, Request};
    match crate::rc::control::presence() {
        Presence::Absent => Ok(()),
        Presence::Running(_) => match crate::rc::control::ask(&Request::ReloadSecrets)? {
            Reply::SecretsReloaded { .. } => Ok(()),
            Reply::Error { message } => anyhow::bail!(
                "the vault was updated, but the running daemon kept its previous matcher: {message}"
            ),
            other => anyhow::bail!(
                "the vault was updated, but the daemon returned an unexpected reload reply: {other:?}"
            ),
        },
        Presence::Unclear(why) => anyhow::bail!(
            "the vault was updated, but daemon state is unclear and reload was not confirmed: {why}"
        ),
    }
}

#[cfg(not(unix))]
fn reload_daemon() -> crate::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    #[test]
    fn cli_accepts_management_commands_but_never_a_positional_secret() {
        for argv in [
            vec!["agit", "secrets", "add", "production", "--stdin"],
            vec![
                "agit",
                "secrets",
                "add",
                "short",
                "--stdin",
                "--allow-short",
            ],
            vec!["agit", "secrets", "list", "--json"],
            vec!["agit", "secrets", "remove", "sec_example", "--yes"],
            vec!["agit", "secrets", "status", "--json"],
            vec!["agit", "secrets", "review", "--json"],
            vec!["agit", "secrets", "allow", "sec_example"],
            vec!["agit", "secrets", "unallow", "sec_example"],
            vec!["agit", "secrets", "block", "add", "production", "--stdin"],
            vec!["agit", "secrets", "block", "remove", "sec_example"],
        ] {
            assert!(
                crate::commands::Cli::try_parse_from(&argv).is_ok(),
                "documented command must parse: {argv:?}"
            );
        }
        assert!(
            crate::commands::Cli::try_parse_from([
                "agit",
                "secrets",
                "add",
                "production",
                "must-not-enter-argv"
            ])
            .is_err()
        );
    }
}
