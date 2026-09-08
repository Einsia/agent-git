//! Path and environment-variable resolution.
//!
//! Every "where does this live" question has its one answer here. No other module reads
//! environment variables directly — so defaults and empty-value handling exist in one place.
//!
//! # The repo-to-agent mapping lives on the server
//!
//! **No binding file is written into the code repo.** "Which agent should this repo pick up" is
//! answered by the server (with no argument, `agit clone` has the hub look up "which agents have
//! worked in this repo"). So there is no `.agit.toml` path resolution here — that file does not
//! exist.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// The explicit home wins; native Windows shells may provide only their user profile.
pub fn user_home() -> Option<PathBuf> {
    user_home_from(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
        cfg!(windows),
    )
}

fn user_home_from(
    home: Option<&std::ffi::OsStr>,
    profile: Option<&std::ffi::OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    home.filter(|path| !path.is_empty())
        .or_else(|| {
            windows
                .then_some(profile)
                .flatten()
                .filter(|path| !path.is_empty())
        })
        .map(PathBuf::from)
}

/// The agit home directory: an explicit `$AGIT_HOME` or `.agit` under the user home.
///
/// An empty string is rejected: used as-is, `AGIT_HOME=""` resolves to the **relative** path
/// `.agit`, and a hidden store grows under every working directory.
pub fn agit_home() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("AGIT_HOME") {
        let h = h.trim();
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    let home =
        user_home().context("no user home or $AGIT_HOME is set — cannot locate the agit home")?;
    Ok(home.join(".agit"))
}

/// The local store: `$AGIT_HOME/store/`.
///
/// **The MVP has one local store**, not a directory per agent. Naming is deferred to `push` —
/// before publishing, a session belongs to no agent and has no name to use as a directory name.
/// The relation "these sessions belong to agent X" is established on the server at publish time.
pub fn store_root() -> Result<PathBuf> {
    Ok(agit_home()?.join("store"))
}

/// Where repos live.
///
/// What your own `commit` produces and what `agit clone` copies down both live here. Both carry
/// an explicit identity (`<owner>/<name>`), so they are split into owner/name directories.
pub fn repos_dir() -> Result<PathBuf> {
    Ok(agit_home()?.join("repos"))
}

/// The local path of one repo.
pub fn repo_dir(owner: &str, name: &str) -> Result<PathBuf> {
    Ok(repos_dir()?.join(owner).join(name))
}

/// Where the linked worktrees of session branches live.
///
/// Outside `repos/`: a worktree nested inside the main checkout is, to that checkout, a pile of
/// untracked files; and the scans over the two-level `repos/<owner>/<name>` structure (startup
/// migration, `repo list`) must not take it for another repo.
pub fn worktrees_dir() -> Result<PathBuf> {
    Ok(agit_home()?.join("worktrees"))
}

/// The worktree path of one branch: `<worktrees>/<owner>/<name>/<branch>`.
pub fn worktree_dir(owner: &str, name: &str, branch: &str) -> Result<PathBuf> {
    Ok(worktrees_dir()?.join(owner).join(name).join(branch))
}

/// The credentials directory: one file per hub host.
///
/// A directory rather than one file, because "a hub's credentials are readable only by that
/// hub": with one file, every read or write pulls every hub's token into memory, and everyday
/// actions — syncing across machines, sending the wrong file, a backup tool packing things up —
/// carry the other hubs' credentials along with it.
pub fn credentials_dir() -> Result<PathBuf> {
    Ok(agit_home()?.join("credentials"))
}

/// The credentials file of one hub: `credentials/<hub-host>.json`.
pub fn credentials_path(hub: &str) -> Result<PathBuf> {
    Ok(credentials_dir()?.join(format!("{}.json", hub_host_key(hub)?)))
}

/// Credential filenames depend on a checked authority, not lossy URL spelling.
pub fn hub_host_key(hub: &str) -> Result<String> {
    Ok(super::hub_authority::HubAuthority::parse(hub)?.storage_key())
}

/// The compatibility filename can identify a candidate only when its metadata agrees.
pub fn legacy_hub_host_key(hub: &str) -> String {
    let s = hub.trim().trim_end_matches('/');
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    // Strip the path: `http://h:8177/api` and `http://h:8177` are the same hub.
    let authority = s.split('/').next().unwrap_or(s);
    let key: String = authority
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if key.is_empty() {
        "unknown".into()
    } else {
        key
    }
}

/// Case-preserving filesystems may retain a spelling different from a bound record's address.
pub fn legacy_hub_record_matches(path: &std::path::Path, hub: &str) -> bool {
    hub_record_key_matches(path, &legacy_hub_host_key(hub))
}

/// Filename aliases are admitted only when the filesystem resolves the expected name to the same file.
pub(crate) fn hub_record_key_matches(path: &std::path::Path, key: &str) -> bool {
    let expected_name = format!("{key}.json");
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name == expected_name {
        return true;
    }
    if !name.eq_ignore_ascii_case(&expected_name) {
        return false;
    }
    let expected_path = path.with_file_name(expected_name);
    std::fs::symlink_metadata(&expected_path).is_ok_and(|metadata| metadata.is_file())
        && same_file::is_same_file(path, &expected_path).unwrap_or(false)
}

/// The global config file: a few keys, no repo-level config (the PRD's `agit config` section).
pub fn global_config_path() -> Result<PathBuf> {
    Ok(agit_home()?.join("config.json"))
}

/// The private directory of this device's low-entropy secret filter.
pub fn secret_filter_dir() -> Result<PathBuf> {
    Ok(agit_home()?.join("secret-filter"))
}

/// The encrypted vault. The KEK is not here; the configured keystore holds it.
pub fn secret_filter_vault_path() -> Result<PathBuf> {
    Ok(secret_filter_dir()?.join("vault.json"))
}

/// The directory of the opt-in file keystore: one `<vault-id>.key` per vault, 0600 files in a
/// 0700 directory.
///
/// A sibling of `secret-filter/`, never inside it: the vault directory is what a backup or a
/// copy of the filter carries along, and the key must not travel with it by default.
pub fn keystore_dir() -> Result<PathBuf> {
    Ok(agit_home()?.join("keystore"))
}

/// Where the secret-filter KEK lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKeystore {
    /// The operating-system credential store: macOS Keychain, Windows Credential Manager, the
    /// Secret Service on other Unix systems.
    Os,
    /// A private file under [`keystore_dir`]. Chosen explicitly, for a machine with no desktop
    /// session (SSH, CI) where no Secret Service answers; its protection is the file mode.
    File,
}

impl SecretKeystore {
    pub const KEY: &str = "secrets.keystore";
    pub const ENV: &str = "AGIT_SECRETS_KEYSTORE";

    pub fn parse(v: &str) -> Option<Self> {
        match v.trim() {
            "os" => Some(Self::Os),
            "file" => Some(Self::File),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Os => "os",
            Self::File => "file",
        }
    }
}

/// The configured keystore: `$AGIT_SECRETS_KEYSTORE` > `secrets.keystore` > `os`.
///
/// A value outside the domain is an error, not the default: a misspelt setting that silently
/// selected the OS store would create the key there, and every later run under the intended
/// setting would find its vault without a key.
pub fn secret_keystore() -> Result<SecretKeystore> {
    match get_global(SecretKeystore::KEY)? {
        None => Ok(SecretKeystore::Os),
        Some(v) => SecretKeystore::parse(&v).with_context(|| {
            format!(
                "`{}` takes `os` or `file`, not `{v}` (from ${} or `agit config {}`)",
                SecretKeystore::KEY,
                SecretKeystore::ENV,
                SecretKeystore::KEY
            )
        }),
    }
}

/// The config keys an environment variable overrides, and the variable.
const ENV_OVERRIDES: [(&str, &str); 2] = [
    ("hub.url", "AGIT_HUB_URL"),
    (SecretKeystore::KEY, SecretKeystore::ENV),
];

/// Read one global config key.
///
/// Order: **the environment variable** (`hub.url` ← `AGIT_HUB_URL`, `secrets.keystore` ←
/// `AGIT_SECRETS_KEYSTORE`) > the config file > None (the caller supplies the default).
pub fn get_global(key: &str) -> Result<Option<String>> {
    if let Some((_, value)) = global_env_override(key) {
        return Ok(Some(value));
    }
    let path = global_config_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&text).with_context(|| format!("{} is malformed", path.display()))?;
    Ok(map.get(key).cloned())
}

/// The active environment override for a global key, including the variable that supplied it.
///
/// Configuration surfaces use this alongside the file value so an override does not masquerade
/// as persisted state.
pub(crate) fn global_env_override(key: &str) -> Option<(&'static str, String)> {
    let var = global_env_name(key)?;
    let value = std::env::var(var).ok()?;
    let value = value.trim();
    // A hub address is an origin; a trailing slash is not part of the identity.
    let value = if key == "hub.url" {
        value.trim_end_matches('/')
    } else {
        value
    };
    (!value.is_empty()).then(|| (var, value.to_string()))
}

/// The environment variable that can override a global key, whether or not it is currently set.
pub(crate) fn global_env_name(key: &str) -> Option<&'static str> {
    ENV_OVERRIDES
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, var)| *var)
}

/// Write or delete one global config key (a `value` of None deletes).
///
/// commands::config gates which keys are legal; this only reads and writes.
pub fn set_global(key: &str, value: Option<&str>) -> Result<()> {
    let path = global_config_path()?;
    let mut map: std::collections::BTreeMap<String, String> = match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => {
            serde_json::from_str(&t).with_context(|| format!("{} is malformed", path.display()))?
        }
        _ => Default::default(),
    };
    match value {
        Some(v) => {
            map.insert(key.to_string(), v.to_string());
        }
        None => {
            map.remove(key);
        }
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&map)?)?;
    Ok(())
}

/// List every global config key and value.
pub fn list_global() -> Result<std::collections::BTreeMap<String, String>> {
    let path = global_config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => {
            Ok(serde_json::from_str(&t)
                .with_context(|| format!("{} is malformed", path.display()))?)
        }
        _ => Ok(Default::default()),
    }
}

/// The root of the current code repo.
///
/// Returns None instead of erroring: many commands work outside a repo (`agit log`,
/// `agit clone owner/x`). A command that needs a repo decides for itself.
pub fn repo_root() -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        return None;
    }
    Some(PathBuf::from(p))
}

/// The origin URL of the current repo, which lets the server look up "which agents have worked
/// in this repo".
pub fn repo_origin() -> Option<String> {
    let root = repo_root()?;
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(&root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let u = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if u.is_empty() { None } else { Some(u) }
}

/// The hub address.
///
/// `$AGIT_HUB_URL` → the default public hub.
///
/// A loopback default looks safer — "sending session transcripts to the public internet with no
/// environment variable configured is unacceptable" — but that worry rests on a false premise:
/// nothing is sent out after installing. `import` and `commit` are purely local actions; only an
/// explicit `agit push` uploads, and push requires a sign-in and requires naming an agent. What
/// carries a transcript out the door is that one explicit command, not this constant.
///
/// The cost is that a self-hosted instance must set `AGIT_HUB_URL` explicitly — but those people
/// have to set it anyway, and requiring every ordinary user to export an environment variable
/// before anything works puts the cost on the wrong people.
pub fn hub_url() -> String {
    hub_url_at(agit_home().ok().as_deref())
}

/// Resolve routing for a known store without changing the process working directory.
pub(crate) fn hub_url_at(home: Option<&std::path::Path>) -> String {
    if let Some(v) = hub_url_env_override() {
        return v;
    }
    if let Some(home) = home
        && let Ok(Some(v)) = get_global_file_at(&home.join("config.json"), "hub.url")
    {
        return v;
    }
    DEFAULT_HUB_URL.to_string()
}

/// The non-empty, normalized `AGIT_HUB_URL` override, when one is active.
///
/// Configuration UIs need the source as well as the effective value: a stored value hidden by
/// the environment must not look as though editing the file will change the running process.
pub(crate) fn hub_url_env_override() -> Option<String> {
    global_env_override("hub.url").map(|(_, value)| value)
}

/// Reads the config file only (no environment lookup), for hub_url to use (avoids recursion).
fn get_global_file_at(path: &std::path::Path, key: &str) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&text).with_context(|| format!("{} is malformed", path.display()))?;
    Ok(map.get(key).cloned())
}

/// Built-in hub address.
///
/// Release builds use the public AgentGit endpoint. Deployment and acceptance-test builds can
/// pin a different immutable default without relying on the caller's shell environment:
///
/// ```text
/// AGIT_DEFAULT_HUB_URL=https://staging.agent-git.com cargo build --release
/// ```
///
/// Runtime `AGIT_HUB_URL` and `hub.url` still take precedence over this value.
pub const DEFAULT_HUB_URL: &str = match option_env!("AGIT_DEFAULT_HUB_URL") {
    Some(url) => url,
    None => "https://agent-git.com",
};

/// CLI release channel embedded by the build pipeline.
///
/// GitHub release builds intentionally use the default (`prod`). GitLab's internal builds set
/// this to `dev` or `staging`; those binaries must never replace themselves with the public npm
/// release because doing so would silently change both the tested commit and its built-in hub.
pub const RELEASE_CHANNEL: &str = match option_env!("AGIT_RELEASE_CHANNEL") {
    Some(channel) => channel,
    None => "prod",
};

/// User-visible build version. Internal CI includes the channel and source commit here, while a
/// normal source or GitHub release build keeps Cargo's package version.
pub const BUILD_VERSION: &str = match option_env!("AGIT_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub fn is_production_release() -> bool {
    RELEASE_CHANNEL == "prod"
}

/// Whether the secret scan is allowed through.
///
/// Accepts `1`/`true`/`yes`. Every time it takes effect it is printed explicitly — unlike git's
/// `--no-verify`, which is silent — because "a secret entered shared history" is irreversible.
pub fn allow_secrets() -> bool {
    std::env::var("AGIT_ALLOW_SECRETS")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
}

/// The lock to take before a test changes a process-level environment variable such as
/// `AGIT_HOME`.
///
/// Environment variables are shared by the whole test process, and cargo runs tests in parallel
/// by default: two tests that each `set_var` and each restore it interleave, and one reads the
/// other's directory. A test that changes the environment takes this lock first, so it
/// serializes only against tests of its own kind and leaves every other test parallel.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {

    use super::*;

    /// A pure-function form, so no process-global environment variable is touched (parallel
    /// tests overwrite each other).
    fn resolve_home(agit_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        if let Some(h) = agit_home.map(str::trim).filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(h));
        }
        home.map(|h| PathBuf::from(h).join(".agit"))
    }

    #[test]
    fn blank_agit_home_falls_back_never_relative() {
        assert_eq!(
            resolve_home(Some("/x/store"), Some("/home/dev")),
            Some("/x/store".into())
        );
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                resolve_home(Some(blank), Some("/home/dev")),
                Some("/home/dev/.agit".into()),
                "a blank value must fall back and must not become the relative path .agit"
            );
        }
    }

    #[test]
    fn hub_url_strips_trailing_slashes() {
        fn norm(s: &str) -> String {
            s.trim().trim_end_matches('/').to_string()
        }
        assert_eq!(norm("http://h:8177/"), "http://h:8177");
        assert_eq!(norm("http://h:8177///"), "http://h:8177");
    }

    #[test]
    fn default_backend_is_https() {
        // The default points at the public hub, so it must be TLS: signing in sends a password,
        // the CLI sends a token, the server sends whole transcripts back — none of it is
        // acceptable in the clear.
        assert!(
            DEFAULT_HUB_URL.starts_with("https://"),
            "the default backend must use TLS: {DEFAULT_HUB_URL}"
        );
    }

    #[test]
    fn env_var_overrides_the_default() {
        // Self-hosted instances live on this one; changing the default must not lose it.
        fn resolve(env: Option<&str>) -> String {
            env.map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_HUB_URL.to_string())
        }
        assert_eq!(
            resolve(Some("http://127.0.0.1:8177")),
            "http://127.0.0.1:8177"
        );
        assert_eq!(
            resolve(Some("  https://hub.corp.com/  ")),
            "https://hub.corp.com"
        );
        // An empty string falls back; it must not become an empty address.
        assert_eq!(resolve(Some("   ")), DEFAULT_HUB_URL);
        assert_eq!(resolve(None), DEFAULT_HUB_URL);
    }

    #[test]
    fn native_windows_home_falls_back_to_the_profile_without_changing_unix_resolution() {
        use std::ffi::OsStr;
        let explicit = OsStr::new("explicit-home");
        let profile = OsStr::new("profile-home");
        assert_eq!(
            user_home_from(None, Some(profile), true),
            Some(PathBuf::from(profile))
        );
        assert_eq!(
            user_home_from(Some(OsStr::new("")), Some(profile), true),
            Some(PathBuf::from(profile))
        );
        assert_eq!(
            user_home_from(Some(explicit), Some(profile), true),
            Some(PathBuf::from(explicit))
        );
        assert_eq!(user_home_from(None, Some(profile), false), None);
        assert_eq!(user_home_from(None, Some(OsStr::new("")), true), None);
    }

    #[test]
    fn hub_host_key_is_a_filename_and_ignores_the_scheme() {
        assert_eq!(
            hub_host_key("https://agent-git.com").unwrap(),
            hub_host_key("HTTP://AGENT-GIT.COM/").unwrap()
        );
        assert_ne!(
            hub_host_key("http://127.0.0.1:8177").unwrap(),
            hub_host_key("http://127.0.0.1:8178").unwrap()
        );
        assert_eq!(
            hub_host_key("https://hub.corp.com/api/").unwrap(),
            hub_host_key("https://hub.corp.com").unwrap()
        );
        assert!(hub_host_key("").is_err());
        assert_eq!(legacy_hub_host_key("HTTP://host:8177"), "HTTP_");
        assert_eq!(legacy_hub_host_key("http://host:8177"), "host_8177");
    }

    #[test]
    fn secret_keystore_values_are_exact() {
        assert_eq!(SecretKeystore::parse("os"), Some(SecretKeystore::Os));
        assert_eq!(SecretKeystore::parse(" file\n"), Some(SecretKeystore::File));
        // Anything else is refused rather than read as the default: a typo that silently
        // selected the OS store would put the key where the intended setting never looks.
        for bad in ["", "File", "keychain", "file/", "auto"] {
            assert_eq!(SecretKeystore::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn allow_secrets_only_accepts_explicit_truthy() {
        fn parse(v: &str) -> bool {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        }
        assert!(parse("1") && parse("true") && parse("YES"));
        // A value that "looks like off" must never be taken as on.
        assert!(!parse("0") && !parse("false") && !parse("") && !parse("no"));
    }
}
