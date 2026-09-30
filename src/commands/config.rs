//! Configuration at user scope and explicit repository scope.
//! Repository overrides remain local to this device; unset values inherit user preferences.

use super::CmdResult;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

/// The full set of valid keys. Adding a key means editing here; an unknown key is always rejected
/// and this table printed.
pub const KEYS: [(&str, &str); 8] = [
    (
        "hub.url",
        "default hub address (AGIT_HUB_URL takes priority)",
    ),
    (
        "runtime.default",
        "default runtime: claude-code / codex / opencode",
    ),
    (
        "push.visibility",
        "first-publish visibility: ask | private | public (non-interactive ask = private)",
    ),
    (
        "push.auto",
        "automatically publish settled turns: true | false (default false)",
    ),
    (
        "privacy.encryption",
        "encryption default for new Hub repositories: true | false (default false; existing repository modes are fixed)",
    ),
    ("commit.auto", "hooks auto-settlement switch: true | false"),
    (
        "memory.track",
        "collect the runtime’s project memory onto session branches: session | off",
    ),
    (
        config::SecretKeystore::KEY,
        "where the global registration key lives: os (system credential store) | file (private file under AGIT_HOME/keystore; AGIT_SECRETS_KEYSTORE takes priority)",
    ),
];

#[derive(ClapArgs)]
pub struct Args {
    /// Key name.
    pub key: Option<String>,
    /// Value. Omit to read.
    pub value: Option<String>,
    /// Delete this key.
    #[arg(long, conflicts_with = "value")]
    pub unset: bool,
    /// List everything.
    #[arg(long, conflicts_with_all = ["key", "unset"])]
    pub list: bool,
    /// Configure a local Agent repository instead of user preferences.
    #[arg(long, value_name = "OWNER/REPO", conflicts_with = "global")]
    pub repo: Option<String>,
    /// Configure user preferences, inherited by repositories without an override.
    #[arg(long)]
    pub global: bool,
}

/// Where the effective value shown by the editor comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    Environment,
    Stored,
    Default,
    Unset,
}

/// One config row, keeping the effective and persisted values separate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Entry {
    pub key: &'static str,
    pub description: &'static str,
    pub effective: Option<String>,
    pub stored: Option<String>,
    pub source: Source,
    pub environment_name: Option<&'static str>,
    pub environment: Option<String>,
}

impl Entry {
    fn json(&self) -> crate::Result<serde_json::Value> {
        let mut value = serde_json::to_value(self)?;
        if self.key == "privacy.encryption" {
            for field in ["effective", "stored"] {
                if let Some(text) = value[field].as_str() {
                    validate(self.key, text)?;
                    value[field] = serde_json::Value::Bool(text == "true");
                }
            }
            value["scope"] = "creation_default".into();
        }
        Ok(value)
    }
}

/// The entry point other commands read config through; that `AGIT_HUB_URL` takes priority is a
/// rule of the `config` module.
pub fn get(key: &str) -> Option<String> {
    config::get_global(key).ok().flatten()
}

pub fn run(args: Args) -> CmdResult {
    if let Some(slug) = args.repo.as_deref() {
        return run_repo(&args, slug);
    }
    if wants_tui(&args) {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                crate::tui::screens::configuration::edit()?;
                return Ok(ExitCode::Ok);
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }

    if args.list || (super::json::requested() && args.key.is_none() && !args.unset) {
        let entries = collect()?;
        if super::json::requested() {
            let entries = entries
                .iter()
                .map(Entry::json)
                .collect::<crate::Result<Vec<_>>>()?;
            println!(
                "{}",
                serde_json::json!({"schema_version": 1, "operation": "list", "settings": entries})
            );
            return Ok(ExitCode::Ok);
        }
        for entry in entries {
            let mark = match entry.source {
                Source::Stored => String::new(),
                Source::Environment => format!(
                    " (environment: {})",
                    entry.environment_name.unwrap_or_default()
                ),
                Source::Default => " (default)".to_owned(),
                Source::Unset => String::new(),
            };
            println!(
                "{} = {}{}\n    {}",
                entry.key,
                entry.effective.as_deref().unwrap_or("(unset)"),
                mark,
                entry.description
            );
        }
        return Ok(ExitCode::Ok);
    }

    let Some(key) = args.key else {
        ui::error("missing key name.");
        ui::hint("`agit config --list` shows every legal key");
        return Ok(ExitCode::Usage);
    };

    if !KEYS.iter().any(|(k, _)| *k == key) {
        ui::error(&format!(
            "unknown config key `{key}`. The config surface is deliberately these {} keys.",
            KEYS.len()
        ));
        for (k, _) in KEYS {
            ui::hint(&format!("  {k}"));
        }
        return Ok(ExitCode::Usage);
    }

    if args.unset {
        config::set_global(&key, None)?;
        if super::json::requested() {
            return structured_entry("unset", &key);
        }
        ui::success(&format!("deleted {key}"));
        return Ok(ExitCode::Ok);
    }

    match args.value {
        None => {
            if super::json::requested() {
                return structured_entry("get", &key);
            }
            let v = if key == "privacy.encryption" {
                Some(config::encryption_default()?.to_string())
            } else {
                config::get_global(&key)?
            };
            match v {
                Some(v) => println!("{v}"),
                None => {
                    println!("(unset)");
                    ui::hint(&format!("set it: `agit config {key} <value>`"));
                }
            }
        }
        Some(v) => {
            crate::input_argument(validate(&key, &v).map_err(|e| {
                ui::error(&format!("{e:#}"));
                e
            }))?;
            config::set_global(&key, Some(&v))?;
            if super::json::requested() {
                return structured_entry("set", &key);
            }
            ui::success(&format!("{key} = {v}"));
        }
    }
    Ok(ExitCode::Ok)
}

fn run_repo(args: &Args, slug: &str) -> CmdResult {
    use crate::domain::{refs, repo::Repo};
    let spec = crate::input_argument(refs::parse(slug))?;
    let (refs::RepoSel::Slug(owner, name), refs::Base::Default, refs::Tail::None) =
        (spec.repo, spec.base, spec.tail)
    else {
        return crate::input_argument(Err(anyhow::anyhow!(
            "--repo requires an explicit owner/repo without a branch or turn selector"
        )));
    };
    crate::input_argument(crate::domain::repo::valid_name(&owner))?;
    crate::input_argument(crate::domain::repo::valid_name(&name))?;
    if args
        .key
        .as_deref()
        .is_some_and(|key| !matches!(key, "push.auto" | "privacy.encryption"))
    {
        return crate::input_argument(Err(anyhow::anyhow!(
            "only push.auto supports a repository override; privacy.encryption is read-only at repository scope"
        )));
    }
    if args.key.as_deref() == Some("privacy.encryption") && (args.unset || args.value.is_some()) {
        return crate::input_argument(Err(anyhow::anyhow!(
            "repository encryption mode is fixed at creation; create a different repository with --encryption=true or --encryption=false. Use `agit config --global privacy.encryption <true|false>` to change the creation default"
        )));
    }
    if args.unset && args.key.is_none() {
        return crate::input_argument(Err(anyhow::anyhow!("--unset requires a key")));
    }
    if let Some(value) = args.value.as_deref() {
        crate::input_argument(validate("push.auto", value))?;
    }
    let repo = Repo::open(config::repo_dir(&owner, &name)?).ok_or_else(|| {
        anyhow::anyhow!("{owner}/{name} is not a local Agent repository; clone it first")
    })?;
    if args.key.as_deref() == Some("privacy.encryption") {
        let entry = repository_encryption_entry(&repo, &owner, &name)?;
        if super::json::requested() {
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"operation":"get","repository":format!("{owner}/{name}"),"setting":entry})
            );
        } else {
            show_repository_encryption(&entry);
        }
        return Ok(ExitCode::Ok);
    }
    let operation = if args.unset {
        repo.set_auto_push(None)?;
        "unset"
    } else if let Some(value) = args.value.as_deref() {
        repo.set_auto_push(Some(value == "true"))?;
        "set"
    } else if args.list || args.key.is_none() {
        "list"
    } else {
        "get"
    };
    let stored = repo.auto_push_override()?;
    let effective = repo.auto_push_enabled()?;
    let entry = serde_json::json!({
        "key": "push.auto", "description": "Automatically publish settled turns",
        "effective": effective.to_string(), "stored": stored.map(|value| value.to_string()),
        "source": if stored.is_some() { "repository" } else { "inherited" },
    });
    let encryption = (operation == "list")
        .then(|| repository_encryption_entry(&repo, &owner, &name))
        .transpose()?;
    if super::json::requested() {
        let mut response = serde_json::json!({"schema_version": 1, "operation": operation, "repository": format!("{owner}/{name}")});
        if operation == "list" {
            response["settings"] = serde_json::json!([entry, encryption]);
        } else {
            response["setting"] = entry;
        }
        println!("{response}");
    } else if operation == "get" {
        println!("{effective}");
    } else {
        println!(
            "push.auto = {effective} ({})",
            if stored.is_some() {
                "repository"
            } else {
                "inherited from user preferences"
            }
        );
    }
    if !super::json::requested()
        && let Some(entry) = encryption
    {
        show_repository_encryption(&entry);
    }
    if effective && matches!(operation, "set" | "unset") && !super::json::requested() {
        ui::hint(&format!(
            "run `agit push {owner}/{name}@<branch>` to preview and confirm the policy for automatic publication"
        ));
    }
    Ok(ExitCode::Ok)
}

fn repository_encryption_entry(
    repo: &crate::domain::repo::Repo,
    owner: &str,
    name: &str,
) -> crate::Result<serde_json::Value> {
    use crate::hub::{Client, identity};
    let publication = crate::rc::local_repository::publication::Destination::load(repo)?;
    let (owner, name) = match publication {
        Some(destination) => super::parse_slug(&destination.repository)?,
        None => (owner.to_owned(), name.to_owned()),
    };
    if identity::read(repo)?.is_some() || repo.remote_url().is_some() {
        let (identity, enabled) =
            identity::repository_mode(repo, &Client::from_env(), &owner, &name)?;
        Ok(serde_json::json!({
            "key":"privacy.encryption", "effective":enabled, "stored":enabled,
            "source":"hub", "scope":"repository_mode", "fixed":true,
            "agent_id":identity.agent_id, "hub":identity.hub,
            "publication_repository":format!("{owner}/{name}"),
        }))
    } else {
        let stored = repo.creation_encryption()?;
        Ok(serde_json::json!({
            "key":"privacy.encryption", "effective":repo.encryption_for_creation(None)?,
            "stored":stored, "source":if stored.is_some() { "local_intent" } else { "inherited" },
            "scope":"creation_intent", "fixed":false,
        }))
    }
}

fn show_repository_encryption(entry: &serde_json::Value) {
    println!(
        "privacy.encryption = {} ({})",
        entry["effective"],
        if entry["fixed"] == true {
            "Hub; fixed at creation"
        } else {
            "local creation intent; no Hub mode established"
        }
    );
    if let Some(repository) = entry["publication_repository"].as_str() {
        println!("publication repository: {repository}");
    }
}

/// The automatic-push setting a repository receives when it is created.
pub(super) struct RepoAutoPush {
    /// A per-repository override; `None` inherits the user preference.
    pub(super) value: Option<bool>,
    /// Nobody could be asked, so creation names the inherited preference and its override.
    unasked: bool,
}

impl RepoAutoPush {
    /// A choice made on the command line or in a screen; creation records it without comment.
    pub(super) fn explicit(value: Option<bool>) -> Self {
        Self {
            value,
            unasked: false,
        }
    }

    /// Record the choice on the repository just created as `slug`.
    ///
    /// The line about an unasked question is printed here rather than where the question was
    /// skipped: a refusal between the two creates nothing, and only the final owner names the
    /// repository that exists.
    pub(super) fn apply(&self, repo: &crate::domain::repo::Repo, slug: &str) -> crate::Result<()> {
        if let Some(value) = self.value {
            return repo.set_auto_push(Some(value));
        }
        if self.unasked {
            // An unreadable user preference must not fail a creation that never asked about it;
            // settlement reports that preference when it next consults it.
            let inherited = match config::auto_push_default() {
                Ok(true) => " (on)",
                Ok(false) => " (off)",
                Err(_) => "",
            };
            ui::progress(format_args!(
                "{slug}: automatic push inherits the user preference{inherited}; set it for this repository with `agit config --repo {slug} push.auto <true|false>`"
            ));
        }
        Ok(())
    }
}

/// Ask only at an interactive repository creation boundary; unattended commands inherit.
///
/// Callers ask before taking branch or link locks: a question that waits for an answer while
/// holding them stalls every other writer of that session until someone replies. The answer is
/// applied with [`RepoAutoPush::apply`] once the repository is actually created.
pub(super) fn choose_repo_auto_push() -> crate::Result<RepoAutoPush> {
    if !ui::prompt::may_prompt() {
        return Ok(RepoAutoPush {
            value: None,
            unasked: true,
        });
    }
    let inherited = config::auto_push_default()?;
    let label = format!(
        "Inherit user preference ({})",
        if inherited { "on" } else { "off" }
    );
    let value = match ui::prompt::select(
        "Automatically push settled turns from this repository?",
        &[&label, "On", "Off"],
    )? {
        Some(1) => Some(true),
        Some(2) => Some(false),
        _ => None,
    };
    Ok(RepoAutoPush::explicit(value))
}

fn structured_entry(operation: &str, key: &str) -> CmdResult {
    let entry = collect()?
        .into_iter()
        .find(|entry| entry.key == key)
        .ok_or_else(|| anyhow::anyhow!("unknown config key `{key}`"))?;
    println!(
        "{}",
        serde_json::json!({"schema_version": 1, "operation": operation, "setting": entry.json()?})
    );
    Ok(ExitCode::Ok)
}

/// Read every row once for the editor without conflating an environment override with a stored
/// value.
pub(crate) fn collect() -> crate::Result<Vec<Entry>> {
    let stored = config::list_global()?;
    Ok(KEYS
        .into_iter()
        .map(|(key, description)| {
            entry_for(
                key,
                description,
                stored.get(key).cloned(),
                config::global_env_name(key),
                config::global_env_override(key).map(|(_, value)| value),
            )
        })
        .collect())
}

fn entry_for(
    key: &'static str,
    description: &'static str,
    persisted: Option<String>,
    environment_name: Option<&'static str>,
    environment: Option<String>,
) -> Entry {
    let (effective, source) = if let Some(value) = &environment {
        (Some(value.clone()), Source::Environment)
    } else if let Some(value) = &persisted {
        (Some(value.clone()), Source::Stored)
    } else if let Some(value) = default_value(key) {
        (Some(value.to_string()), Source::Default)
    } else {
        (None, Source::Unset)
    };
    Entry {
        key,
        description,
        effective,
        stored: persisted,
        source,
        environment_name,
        environment,
    }
}

/// Apply one editor action through the same validation and storage path as the CLI.
pub(crate) fn apply(key: &str, value: Option<&str>) -> crate::Result<()> {
    if !KEYS.iter().any(|(known, _)| *known == key) {
        return crate::input_argument(Err(anyhow::anyhow!("unknown config key `{key}`")));
    }
    if let Some(value) = value {
        crate::input_argument(validate(key, value))?;
    }
    config::set_global(key, value)
}

fn default_value(key: &str) -> Option<&'static str> {
    match key {
        "hub.url" => Some(config::DEFAULT_HUB_URL),
        "push.visibility" => Some("ask"),
        "commit.auto" => Some("true"),
        "push.auto" => Some("false"),
        "privacy.encryption" => Some("false"),
        "memory.track" => Some("session"),
        config::SecretKeystore::KEY => Some(config::SecretKeystore::Os.as_str()),
        _ => None,
    }
}

fn wants_tui(args: &Args) -> bool {
    args.key.is_none() && args.value.is_none() && !args.unset && !args.list
}

/// Value-domain check. Failing it is a usage error, not a runtime failure.
pub(crate) fn validate(key: &str, v: &str) -> crate::Result<()> {
    let ok = match key {
        "push.visibility" => matches!(v, "ask" | "private" | "public"),
        "commit.auto" | "push.auto" | "privacy.encryption" => matches!(v, "true" | "false"),
        "memory.track" => matches!(v, "session" | "off"),
        "runtime.default" => crate::adapter::normalize(v).is_ok(),
        "hub.url" => v.starts_with("http://") || v.starts_with("https://"),
        "secrets.keystore" => config::SecretKeystore::parse(v).is_some(),
        _ => false,
    };
    if !ok {
        anyhow::bail!("`{key}` doesn’t take `{v}` (see the value domains in `agit config --list`)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        args: super::Args,
    }

    #[test]
    fn only_the_zero_argument_form_enters_the_config_editor() {
        assert!(wants_tui(&W::try_parse_from(["x"]).unwrap().args));
        for argv in [
            vec!["x", "hub.url"],
            vec!["x", "hub.url", "https://example.test"],
            vec!["x", "--list"],
            vec!["x", "--unset", "hub.url"],
        ] {
            assert!(!wants_tui(&W::try_parse_from(argv).unwrap().args));
        }
    }

    #[test]
    fn validate_known_keys() {
        assert!(validate("push.visibility", "ask").is_ok());
        assert!(validate("push.visibility", "public").is_ok());
        assert!(validate("push.visibility", "secret").is_err());
        assert!(validate("commit.auto", "true").is_ok());
        assert!(validate("commit.auto", "yes").is_err());
        assert!(validate("privacy.encryption", "true").is_ok());
        assert!(validate("privacy.encryption", "false").is_ok());
        assert!(validate("privacy.encryption", "yes").is_err());
        assert!(validate("memory.track", "off").is_ok());
        assert!(validate("memory.track", "session").is_ok());
        assert!(validate("memory.track", "maybe").is_err());
        assert!(validate("hub.url", "https://h.example").is_ok());
        assert!(validate("hub.url", "h.example").is_err());
        assert!(validate("secrets.keystore", "os").is_ok());
        assert!(validate("secrets.keystore", "file").is_ok());
        assert!(validate("secrets.keystore", "keychain").is_err());
    }

    #[test]
    fn secret_keystore_projects_environment_stored_and_default_sources() {
        let description = KEYS
            .iter()
            .find(|(key, _)| *key == config::SecretKeystore::KEY)
            .unwrap()
            .1;
        let environment = entry_for(
            config::SecretKeystore::KEY,
            description,
            Some("os".into()),
            Some(config::SecretKeystore::ENV),
            Some("file".into()),
        );
        assert_eq!(environment.effective.as_deref(), Some("file"));
        assert_eq!(environment.stored.as_deref(), Some("os"));
        assert_eq!(environment.source, Source::Environment);
        assert_eq!(
            environment.environment_name,
            Some(config::SecretKeystore::ENV)
        );

        let stored = entry_for(
            config::SecretKeystore::KEY,
            description,
            Some("file".into()),
            Some(config::SecretKeystore::ENV),
            None,
        );
        assert_eq!(stored.effective.as_deref(), Some("file"));
        assert_eq!(stored.source, Source::Stored);
        assert_eq!(stored.environment_name, Some(config::SecretKeystore::ENV));

        let default = entry_for(
            config::SecretKeystore::KEY,
            description,
            None,
            Some(config::SecretKeystore::ENV),
            None,
        );
        assert_eq!(default.effective.as_deref(), Some("os"));
        assert_eq!(default.source, Source::Default);
        assert_eq!(default.environment_name, Some(config::SecretKeystore::ENV));
    }
}
