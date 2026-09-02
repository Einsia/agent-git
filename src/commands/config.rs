//! `agit config` — reading and writing the small set of global config keys.
//!
//! Only these keys exist (PRD, the `agit config` section): `hub.url`, `runtime.default`,
//! `push.visibility`, `commit.auto`, `memory.track`, `secrets.keystore`. **There is no repo-level
//! config file** — the smaller the configuration surface, the easier "why does it behave this
//! way" is to answer.
//!
//! `ask` for `push.visibility` counts as `private` in a non-interactive environment (publishing
//! memory always errs conservative); that rule lives where the value is read (push), not in the
//! storage layer.

use super::CmdResult;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

/// The full set of valid keys. Adding a key means editing here; an unknown key is always rejected
/// and this table printed.
pub const KEYS: [(&str, &str); 6] = [
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
    ("commit.auto", "hooks auto-settlement switch: true | false"),
    (
        "memory.track",
        "collect the runtime’s project memory onto session branches: session | off",
    ),
    (
        config::SecretKeystore::KEY,
        "where the secret-filter key lives: os (system credential store) | file (private file under AGIT_HOME/keystore; AGIT_SECRETS_KEYSTORE takes priority)",
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
}

/// The entry point other commands read config through; that `AGIT_HUB_URL` takes priority is a
/// rule of the `config` module.
pub fn get(key: &str) -> Option<String> {
    config::get_global(key).ok().flatten()
}

pub fn run(args: Args) -> CmdResult {
    if args.list {
        let stored = config::list_global()?;
        for (key, desc) in KEYS {
            // The read goes through the full resolution chain (env var > file), so what the
            // user sees is the effective value.
            let effective = config::get_global(key)?.or(match key {
                "hub.url" => Some(config::DEFAULT_HUB_URL.to_string()),
                "push.visibility" => Some("ask".to_string()),
                "commit.auto" => Some("false".to_string()),
                "memory.track" => Some("session".to_string()),
                "secrets.keystore" => Some(config::SecretKeystore::Os.as_str().to_string()),
                _ => None,
            });
            let mark = if stored.contains_key(key) {
                ""
            } else {
                " (default)"
            };
            println!(
                "{} = {}{}\n    {}",
                key,
                effective.as_deref().unwrap_or("(unset)"),
                mark,
                desc
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
        ui::success(&format!("deleted {key}"));
        return Ok(ExitCode::Ok);
    }

    match args.value {
        None => {
            let v = config::get_global(&key)?;
            match v {
                Some(v) => println!("{v}"),
                None => {
                    println!("(unset)");
                    ui::hint(&format!("set it: `agit config {key} <value>`"));
                }
            }
        }
        Some(v) => {
            validate(&key, &v).map_err(|e| {
                ui::error(&format!("{e:#}"));
                e
            })?;
            config::set_global(&key, Some(&v))?;
            ui::success(&format!("{key} = {v}"));
        }
    }
    Ok(ExitCode::Ok)
}

/// Value-domain check. Failing it is a usage error, not a runtime failure.
fn validate(key: &str, v: &str) -> crate::Result<()> {
    let ok = match key {
        "push.visibility" => matches!(v, "ask" | "private" | "public"),
        "commit.auto" => matches!(v, "true" | "false"),
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

    #[test]
    fn validate_known_keys() {
        assert!(validate("push.visibility", "ask").is_ok());
        assert!(validate("push.visibility", "public").is_ok());
        assert!(validate("push.visibility", "secret").is_err());
        assert!(validate("commit.auto", "true").is_ok());
        assert!(validate("commit.auto", "yes").is_err());
        assert!(validate("memory.track", "off").is_ok());
        assert!(validate("memory.track", "session").is_ok());
        assert!(validate("memory.track", "maybe").is_err());
        assert!(validate("hub.url", "https://h.example").is_ok());
        assert!(validate("hub.url", "h.example").is_err());
        assert!(validate("secrets.keystore", "os").is_ok());
        assert!(validate("secrets.keystore", "file").is_ok());
        assert!(validate("secrets.keystore", "keychain").is_err());
    }
}
