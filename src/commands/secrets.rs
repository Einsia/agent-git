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
    /// Allow a repository value in local protection and scans (old keys still hydrate)
    Allow {
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        record_id: Option<String>,
        /// Read an exact value from stdin without putting it in process arguments.
        #[arg(long)]
        stdin: bool,
        /// Explain why this exact value is suitable for the repository's audience.
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Restore default protection for an allowed repository value
    Unallow {
        record_id: String,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
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
    match args.command {
        Action::Add {
            name,
            stdin,
            allow_short,
        } => {
            let store = VaultStore::open_default()?;
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
            crate::input_argument(crate::domain::secret_filter::validate_registration(
                &name,
                &secret,
                allow_short,
            ))?;
            let added = store.add(&name, secret, allow_short)?;
            reload_daemon()?;
            ui::success_result(&format!("registered {} ({})", added.name, added.id));
            Ok(ExitCode::Ok)
        }
        Action::List { json } => {
            let store = VaultStore::open_default()?;
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
            let store = VaultStore::open_default()?;
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
            let store = VaultStore::open_default()?;
            let status = store.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if status.initialized {
                ui::success_result(&format!(
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
        Action::Allow {
            record_id,
            stdin,
            reason,
            repo,
            json,
        } => {
            crate::input_argument(crate::domain::secret_filter::validate_declaration_reason(
                reason.as_deref(),
            ))?;
            let repo = repository_at(repo)?;
            let dictionary = RepositoryDictionary::open(repo.root())?;
            let changed = if stdin {
                let value = read_secret_stdin()?;
                crate::input_argument(crate::domain::secret_filter::validate_declaration_value(
                    &value,
                ))?;
                dictionary.allow_value(value, reason.as_deref())?
            } else {
                dictionary.allow_with_reason(
                    record_id
                        .as_deref()
                        .expect("clap requires record ID or stdin"),
                    reason.as_deref(),
                )?
            };
            report_local_decision(&repo, changed, json)
        }
        Action::Unallow {
            record_id,
            repo,
            json,
        } => {
            let repo = repository_at(repo)?;
            let changed = RepositoryDictionary::open(repo.root())?.unallow(&record_id)?;
            report_local_decision(&repo, changed, json)
        }
        Action::Block { command } => match command {
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
                let added = repository_dictionary(repo)?.block_add(&name, secret, allow_short)?;
                ui::success_result(&format!(
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

fn repository_dictionary(repo: Option<PathBuf>) -> crate::Result<RepositoryDictionary> {
    RepositoryDictionary::open(repository_at(repo)?.root())
}

fn repository_at(repo: Option<PathBuf>) -> crate::Result<Repo> {
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
    Ok(repo)
}

fn report_local_decision(
    repo: &Repo,
    mut record: RepositoryRecordSummary,
    json: bool,
) -> CmdResult {
    let mut sync = synchronize_selected(repo);
    match RepositoryDictionary::open(repo.root()).and_then(|d| d.review()) {
        Ok(records) => match records.into_iter().find(|saved| saved.id == record.id) {
            Some(saved) => record = saved,
            None => {
                sync = Err(anyhow::anyhow!(
                    "the saved declaration is no longer available"
                ))
            }
        },
        Err(error) => sync = Err(error),
    }
    let (status, kind, message, code) = match &sync {
        Ok(()) => (
            "synced",
            "acknowledged",
            "repository decision acknowledged by the Hub".to_owned(),
            ExitCode::Ok,
        ),
        Err(error) => (
            if error.is::<crate::domain::secret_filter::PolicySyncConflict>() {
                "conflict"
            } else {
                "pending"
            },
            sync_error_kind(error),
            super::terminal_error_message(error),
            sync_error_code(error),
        ),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "local_applied": true, "record": record,
                "synchronization": {"status": status, "kind": kind, "message": message},
            }))?
        );
    } else {
        ui::success_result(&format!(
            "local decision saved for {} ({}; {})",
            record.id, record.local_state, status
        ));
    }
    if sync.is_err() {
        ui::error(&format!(
            "local decision saved; Hub synchronization did not complete: {message}"
        ));
    }
    Ok(code)
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Hub exact-declaration capability is unavailable; the endpoint may be unsupported or write access denied"
)]
struct PolicyUnavailable;

fn sync_error_kind(error: &anyhow::Error) -> &'static str {
    if error.is::<crate::domain::secret_filter::PolicySyncConflict>() {
        "conflict"
    } else if error.is::<PolicyUnavailable>() {
        "unsupported"
    } else {
        "synchronization_failed"
    }
}

fn sync_error_code(error: &anyhow::Error) -> ExitCode {
    super::terminal_error_code(error, ExitCode::Precondition)
}

struct HubPolicy<'a> {
    client: &'a crate::hub::Client,
    owner: &'a str,
    name: &'a str,
    agent_id: &'a str,
}

fn policy_response<T>(result: crate::Result<T>) -> crate::Result<T> {
    result.map_err(|error| {
        if let Some(api) = error.downcast_ref::<crate::hub::client::ApiError>() {
            if api.kind == "secret_policy_stale" {
                return crate::domain::secret_filter::PolicySyncConflict.into();
            }
            if api.status == 404 || api.kind == "secret_policy_unsupported" {
                return PolicyUnavailable.into();
            }
        }
        error
    })
}

impl crate::domain::secret_filter::RepositoryPolicyTransport for HubPolicy<'_> {
    fn snapshot(&self) -> crate::Result<crate::domain::secrets::repository_policy::PolicySnapshot> {
        policy_response(super::remote_request(self.client.secret_allowances(
            self.owner,
            self.name,
            self.agent_id,
        )))
    }
    fn change(
        &self,
        change: &crate::domain::secrets::repository_policy::PolicyChange,
    ) -> crate::Result<crate::domain::secrets::repository_policy::PolicyChangeResponse> {
        policy_response(super::remote_request(
            self.client
                .change_secret_allowance(self.owner, self.name, change),
        ))
    }
}

fn selected_route(repo: &Repo, hub: &str) -> crate::Result<(String, String)> {
    let common = std::fs::canonicalize(crate::domain::repo::common_git_dir(repo.root()))?;
    for (owner, name, path) in super::clone::list_local()? {
        if std::fs::canonicalize(crate::domain::repo::common_git_dir(&path))
            .ok()
            .as_ref()
            == Some(&common)
        {
            return Ok((owner, name));
        }
    }
    if let Some(remote) = repo.remote_url()
        && let Some((owner, name)) = super::remote_slug(&remote)
        && super::lands_on(&remote, hub, Some(&owner), &name)
    {
        return Ok((owner, name));
    }
    anyhow::bail!(
        "the repository has no selected Hub target; declarations remain pending until its first push"
    )
}

fn synchronize_selected(repo: &Repo) -> crate::Result<()> {
    let client = crate::hub::Client::from_env();
    if let Some(identity) = crate::hub::identity::read(repo)? {
        anyhow::ensure!(
            identity.hub == crate::hub::identity::normalize_hub(client.base())?,
            "the selected Hub differs from the retained repository declaration target"
        );
        RepositoryDictionary::open(repo.root())?.bind_declarations(
            &crate::domain::secret_filter::DeclarationTarget {
                hub: identity.hub,
                repository_id: identity.agent_id,
            },
        )?;
    }
    if !client.has_token() {
        return Err(super::LoginRequired::new(client.base()).into());
    }
    let (owner, name) = selected_route(repo, client.base())?;
    let remote = super::remote_request(client.get_agent(&owner, &name))?;
    let identity = crate::hub::identity::RemoteIdentity::new(client.base(), &remote.agent_id)?;
    synchronize_target(repo, &client, &owner, &name, &identity, false)
}

pub(crate) fn synchronize_target(
    repo: &Repo,
    client: &crate::hub::Client,
    owner: &str,
    name: &str,
    identity: &crate::hub::identity::RemoteIdentity,
    dry_run: bool,
) -> crate::Result<()> {
    anyhow::ensure!(
        identity.hub == crate::hub::identity::normalize_hub(client.base())?,
        "repository declarations belong to another Hub"
    );
    crate::hub::identity::verify_transport_target(repo, identity)?;
    // The dictionary binds declarations to their immutable target; an ordinary transport's
    // current API response remains authoritative over an unrelated cached checkout identity.
    RepositoryDictionary::open(repo.root())?.synchronize_policy(
        &crate::domain::secret_filter::DeclarationTarget {
            hub: identity.hub.clone(),
            repository_id: identity.agent_id.clone(),
        },
        &HubPolicy {
            client,
            owner,
            name,
            agent_id: &identity.agent_id,
        },
        dry_run,
    )
}

pub(crate) fn has_policy_state(repo: &Repo) -> crate::Result<bool> {
    let dictionary = RepositoryDictionary::open(repo.root())?;
    Ok(dictionary.exists() && dictionary.has_policy_state()?)
}

/// A missing first-publication destination is materialized by push only after local checks.
pub(crate) fn synchronize_before_push(
    repo: &Repo,
    client: &crate::hub::Client,
    owner: &str,
    name: &str,
    dry_run: bool,
    allow_secrets: bool,
) -> crate::Result<Option<ExitCode>> {
    let has_policy_state = match has_policy_state(repo) {
        Ok(state) => state,
        Err(error) => {
            ui::error(&format!(
                "cannot inspect repository declaration state: {}",
                super::terminal_error_message(&error)
            ));
            return Ok(Some(ExitCode::Precondition));
        }
    };
    let expected = crate::hub::identity::expected_for_transport(repo, client.base())?;
    let result = match super::remote_request(client.get_agent(owner, name)) {
        Ok(remote) => crate::hub::identity::RemoteIdentity::new(client.base(), &remote.agent_id)
            .and_then(|identity| synchronize_target(repo, client, owner, name, &identity, dry_run)),
        Err(error)
            if error
                .downcast_ref::<crate::hub::client::ApiError>()
                .is_some_and(|api| api.status == 404) =>
        {
            if expected.is_some() {
                return Err(
                    error.context("the RC remote is unavailable; refusing to create a replacement")
                );
            }
            if has_policy_state {
                report_pending_declarations(repo)?;
                ui::info(
                    "Declarations require an immutable destination; synchronization is planned after first-publication repository creation.",
                );
            }
            return Ok(None);
        }
        Err(error) => Err(error),
    };
    report_policy_discovery(result, has_policy_state, allow_secrets)
}

/// A prepared copy installs policy for its validated destination without resolving its name again.
pub(crate) fn synchronize_push_target(
    repo: &Repo,
    client: &crate::hub::Client,
    owner: &str,
    name: &str,
    identity: &crate::hub::identity::RemoteIdentity,
    allow_secrets: bool,
) -> crate::Result<Option<ExitCode>> {
    let has_policy_state = has_policy_state(repo)?;
    let result = synchronize_target(repo, client, owner, name, identity, false);
    report_policy_discovery(result, has_policy_state, allow_secrets)
}

fn report_policy_discovery(
    result: crate::Result<()>,
    has_policy_state: bool,
    allow_secrets: bool,
) -> crate::Result<Option<ExitCode>> {
    // An older Hub can publish with the normal scanner when no local policy depends on it.
    if !has_policy_state {
        match result {
            Err(error) if error.is::<PolicyUnavailable>() => return Ok(None),
            Err(error) if !allow_secrets => return Err(error),
            result => return report_sync_for_push(result, allow_secrets),
        }
    }
    report_sync_for_push(result, allow_secrets)
}

pub(crate) fn report_sync_for_push(
    result: crate::Result<()>,
    allow_secrets: bool,
) -> crate::Result<Option<ExitCode>> {
    match result {
        Ok(()) => Ok(None),
        Err(error) if allow_secrets => {
            ui::warning(&format!(
                "repository declaration synchronization did not complete: {}; --allow-secrets applies only to this publication",
                super::terminal_error_message(&error)
            ));
            Ok(None)
        }
        Err(error) => {
            ui::error(&format!(
                "repository declaration synchronization failed: {}",
                super::terminal_error_message(&error)
            ));
            Ok(Some(sync_error_code(&error)))
        }
    }
}

pub(crate) fn pending_declarations(repo: &Repo) -> crate::Result<Vec<RepositoryRecordSummary>> {
    let dictionary = RepositoryDictionary::open(repo.root())?;
    if !dictionary.exists() {
        return Ok(vec![]);
    }
    Ok(dictionary
        .review()?
        .into_iter()
        .filter(|record| record.pending_operation.is_some())
        .collect())
}

pub(crate) fn report_pending_declarations(repo: &Repo) -> crate::Result<bool> {
    let records = pending_declarations(repo)?;
    if records.is_empty() {
        return Ok(false);
    }
    ui::warning(&format!(
        "{} repository declaration(s) pending synchronization; local scan decisions are not confirmed server policy. Inspect `agit secrets review --repo <path> --json`.",
        records.len()
    ));
    Ok(true)
}

fn print_repository_record(record: &RepositoryRecordSummary) {
    println!(
        "{}\t{}\torigins={}\theuristic={:?}\texplicit={}\tactive={}\tlocal={}\tsync={}",
        record.id,
        record.name,
        record.origins.join(","),
        record.heuristic_disposition,
        record.explicit_block,
        record.effective_protect,
        record.local_state,
        record.sync_status,
    );
}

fn read_secret_stdin() -> crate::Result<Zeroizing<String>> {
    read_secret_input(std::io::stdin().lock())
}

fn read_secret_input(input: impl std::io::Read) -> crate::Result<Zeroizing<String>> {
    let limit = crate::domain::secret_filter::MAX_REPOSITORY_SECRET_BYTES;
    let mut bytes = Zeroizing::new(Vec::new());
    input.take((limit + 3) as u64).read_to_end(&mut bytes)?;
    crate::input_argument(check_input_size(bytes.len(), limit + 2))?;
    let mut value = Zeroizing::new(
        crate::input_argument(
            std::str::from_utf8(&bytes)
                .map_err(|_| anyhow::anyhow!("secret input must be valid UTF-8")),
        )?
        .to_owned(),
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
    crate::input_argument(check_input_size(value.len(), limit))?;
    Ok(value)
}

fn check_input_size(size: usize, limit: usize) -> crate::Result<()> {
    anyhow::ensure!(
        size <= limit,
        "secret input exceeds the supported size limit"
    );
    Ok(())
}

fn print_record(record: &RecordSummary) {
    println!("{}\t{}", record.id, record.name);
}

/// A running daemon must switch to the new matcher before this command returns; one that is not
/// running loads it on its next start.
fn reload_daemon() -> crate::Result<()> {
    crate::rc::select_local_authority();
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

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    #[test]
    fn exact_stdin_is_bounded_and_removes_only_one_line_ending() {
        for (input, expected) in [
            (" value \r\n", " value "),
            ("value\n\n", "value\n"),
            ("value\r", "value\r"),
            ("value", "value"),
        ] {
            assert_eq!(
                super::read_secret_input(input.as_bytes()).unwrap().as_str(),
                expected
            );
        }
        let limit = crate::domain::secret_filter::MAX_REPOSITORY_SECRET_BYTES;
        assert_eq!(
            super::read_secret_input(format!("{}\r\n", "x".repeat(limit)).as_bytes())
                .unwrap()
                .len(),
            limit
        );
        let mut input = std::io::Cursor::new(vec![b'x'; limit * 2]);
        assert!(super::read_secret_input(&mut input).is_err());
        assert_eq!(input.position(), (limit + 3) as u64);
        assert!(super::read_secret_input([255].as_slice()).is_err());
    }

    #[test]
    fn exact_allow_requires_one_input_source() {
        for args in [
            vec!["agit", "secrets", "allow"],
            vec!["agit", "secrets", "allow", "sec_example", "--stdin"],
        ] {
            assert!(crate::commands::Cli::try_parse_from(args).is_err());
        }
        assert!(
            crate::commands::Cli::try_parse_from([
                "agit",
                "secrets",
                "allow",
                "--stdin",
                "--reason",
                "Public fixture",
                "--json"
            ])
            .is_ok()
        );
    }

    #[test]
    fn repository_management_does_not_require_global_keystore_configuration() {
        const CHILD: &str = "AGIT_TEST_REPOSITORY_LOCAL_KEYS";
        let Some(root) = std::env::var_os(CHILD) else {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::secret_vault::tests::repository_management_does_not_require_global_keystore_configuration",
                    "--nocapture",
                ])
                .env(CHILD, dir.path())
                .env("AGIT_HOME", dir.path().join("home"))
                .env("AGIT_SECRETS_KEYSTORE", "invalid-global-setting")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed;"));
            return;
        };
        let path = std::path::PathBuf::from(root).join("repository");
        crate::domain::repo::Repo::init(&path).unwrap();
        let dictionary = super::repository_dictionary(Some(path.clone())).unwrap();
        dictionary
            .protect_jsonl(
                "{\"text\":\"fixture-registered-value\"}\n",
                &crate::domain::secret_filter::Matcher::for_test(&[(
                    "fixture",
                    "fixture-registered-value",
                )]),
            )
            .unwrap();
        assert_eq!(
            super::run(super::Args {
                command: super::Action::Review {
                    repo: Some(path),
                    json: true
                },
            })
            .unwrap(),
            crate::ExitCode::Ok
        );
        assert!(
            super::run(super::Args {
                command: super::Action::Status { json: true }
            })
            .is_err()
        );
    }

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
