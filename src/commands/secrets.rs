//! `agit secrets` — manage local policy and reversible recovery records.
//!
//! A secret never travels through argv: interactive input does not echo, and a non-interactive
//! run must say `--stdin` explicitly. This command returns only the opaque id and the label the
//! user chose; there is no show / decrypt / export entry point at all.

use super::CmdResult;
use crate::domain::privacy::{management::Command, service};
use crate::domain::repo::Repo;
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
    /// Disable a global block rule by opaque id or exact label
    Remove {
        id_or_name: String,
        /// Confirm disabling the rule without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Report recovery storage and cached encryption availability
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Review recovered candidates and repository policy without revealing values
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
        global: bool,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Restore default protection for an allowed heuristic candidate
    Unallow {
        record_id: String,
        #[arg(long)]
        global: bool,
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
    let mut command = Command {
        action: String::new(),
        global: true,
        id: None,
        name: None,
        secret: None,
    };
    let mut repository = None;
    let mut json_output = false;
    match args.command {
        Action::Add {
            name,
            stdin,
            allow_short,
        } => {
            let secret = read_new_secret(stdin)?;
            crate::input_argument(crate::domain::secret_filter::validate_registration(
                &name,
                &secret,
                allow_short,
            ))?;
            command.action = "add".into();
            command.name = Some(name);
            command.secret = Some(secret.to_string());
        }
        Action::List { json } => {
            command.action = "list".into();
            json_output = json;
        }
        Action::Status { json } => {
            command.action = "status".into();
            json_output = json;
        }
        Action::Remove { id_or_name, yes } => {
            if !yes {
                match ui::prompt::confirm(
                    &format!("Disable registered rule `{id_or_name}`?"),
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
            command.action = "remove".into();
            command.id = Some(id_or_name);
        }
        Action::Review { repo, json } => {
            command.action = "review".into();
            command.global = false;
            repository = Some(repository_path(repo)?);
            json_output = json;
        }
        Action::Allow {
            record_id,
            repo,
            global,
        } => {
            command.action = "allow".into();
            command.id = Some(record_id);
            command.global = global;
            if !global {
                repository = Some(repository_path(repo)?);
            }
        }
        Action::Unallow {
            record_id,
            repo,
            global,
        } => {
            command.action = "unallow".into();
            command.id = Some(record_id);
            command.global = global;
            if !global {
                repository = Some(repository_path(repo)?);
            }
        }
        Action::Block { command: block } => {
            command.global = false;
            match block {
                BlockAction::Add {
                    name,
                    stdin,
                    allow_short,
                    repo,
                } => {
                    let secret = read_new_secret(stdin)?;
                    crate::input_argument(crate::domain::secret_filter::validate_registration(
                        &name,
                        &secret,
                        allow_short,
                    ))?;
                    command.action = "add".into();
                    command.name = Some(name);
                    command.secret = Some(secret.to_string());
                    repository = Some(repository_path(repo)?);
                }
                BlockAction::Remove { record_id, repo } => {
                    command.action = "remove".into();
                    command.id = Some(record_id);
                    repository = Some(repository_path(repo)?);
                }
            }
        }
    }
    let result = service::manage(repository.as_deref(), command)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if let Some(records) = result.as_array() {
        if records.is_empty() {
            println!("no matching privacy records");
        }
        for record in records {
            println!(
                "{}\t{}\tblocked={}\tallowed={}",
                record["id"].as_str().unwrap_or(""),
                record["name"].as_str().unwrap_or(""),
                record["explicit_block"],
                record["allowed"]
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(ExitCode::Ok)
}

fn read_new_secret(stdin: bool) -> crate::Result<Zeroizing<String>> {
    if stdin {
        return read_secret_stdin();
    }
    let Some(first) = ui::prompt::password("Secret")? else {
        return Err(super::InteractionRequired(
            "an interactive terminal is required; automation must use `--stdin`".into(),
        )
        .into());
    };
    let Some(second) = ui::prompt::password("Secret again")? else {
        return Err(super::InteractionRequired("could not read the confirmation".into()).into());
    };
    if first != second {
        return crate::input_argument(Err(anyhow::anyhow!(
            "the two secret values did not match; nothing was saved"
        )));
    }
    Ok(Zeroizing::new(first))
}

fn repository_path(repo: Option<PathBuf>) -> crate::Result<PathBuf> {
    let path = repo.map(Ok).unwrap_or_else(std::env::current_dir)?;
    let repo = Repo::open(&path).ok_or_else(|| {
        anyhow::anyhow!("not an AgentGit repository checkout; pass --repo <path>")
    })?;
    Ok(repo.root().to_owned())
}

fn read_secret_stdin() -> crate::Result<Zeroizing<String>> {
    let mut value = String::new();
    std::io::stdin()
        .take(8 * 1024 * 1024 + 1)
        .read_to_string(&mut value)?;
    anyhow::ensure!(
        value.len() <= 8 * 1024 * 1024,
        "secret input exceeds its byte budget"
    );
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
