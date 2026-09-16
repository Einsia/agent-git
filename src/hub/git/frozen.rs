//! Publication uses a private Git configuration and literal object sources after preparation.
//!
//! This binds transport, not review policy or an atomic transaction across requests. The object
//! store remains borrowed: pruning captured objects can make publication fail. Git, platform
//! trust stores and explicitly selected proxy or certificate files remain trusted dependencies.

use super::{PublicationAttempt, PublicationPhase, RemoteRefs, TransportIdentity};
use crate::domain::repo::publication::{FrozenRef, PublicationPlan};
use crate::domain::repo::{Repo, bounded_inspection_output, inspection_git_path_spelling};
use crate::hub::identity::RemoteIdentity;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONFIG_LIMIT: usize = 64 * 1024;
const REQUEST_UNITS: usize = 24 * 1024;
const LFS_REMOTE: &str = "agit-publication";

#[path = "captured_publication.rs"]
mod captured;
#[path = "frozen_http.rs"]
mod http;
#[path = "frozen_lfs_cache.rs"]
mod lfs_cache;
pub use captured::CapturedPublication;
#[path = "prepared_publication.rs"]
mod prepared;
pub use prepared::{
    BlockedContentInspection, BlockedInspection, CompleteContentInspection, CompleteInspection,
    ContentInspection, InspectionReport, PreparedPayloadAvailability, PreparedPublication,
    PublicationInspection,
};

/// A prepared destination and publication plan survive ambient configuration and ref changes.
/// Local preparation may create private scan locks and temporary state; it performs no network request.
/// Callers enforce review and confirmation before push.
pub struct FrozenPublication {
    transport: TransportIdentity,
    directory: tempfile::TempDir,
    url: String,
    identity: RemoteIdentity,
    http: std::result::Result<(String, ureq::Agent), http::Failure>,
    lfs_objects: std::result::Result<PathBuf, lfs_cache::Failure>,
    lfs_preferences: std::result::Result<BTreeMap<String, String>, http::Failure>,
    inspection_policy: std::result::Result<
        crate::domain::secrets::publication::CapturedPolicy,
        crate::domain::secrets::publication::InspectionFailure,
    >,
    plan: PublicationPlan,
    lfs_inventory: Vec<crate::domain::lfs::Pointer>,
    heads: Vec<Vec<FrozenRef>>,
    tags: Vec<Vec<FrozenRef>>,
}

impl FrozenPublication {
    /// The URL must be the canonical clone URL selected alongside the immutable remote identity.
    /// Named remotes are deliberately absent because they can select multiple push destinations.
    /// Supported HTTP preferences are captured once; unsupported HTTP preferences fail explicitly.
    /// Ambient routing settings do not participate in this context.
    pub fn prepare(
        repo: &Repo,
        plan: &PublicationPlan,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Result<Self> {
        let source = Source::new(repo)?;
        let (url, client) = Self::destination(&source, canonical_url, identity)?;
        Self::from_source(
            source,
            plan,
            url,
            identity.clone(),
            Some(client),
            "http:https",
        )
    }

    fn destination(
        source: &Source,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Result<(String, crate::hub::Client)> {
        Self::destination_with_client(
            source,
            canonical_url,
            identity,
            crate::hub::Client::for_stored_hub(&identity.hub),
        )
    }

    fn destination_with_client(
        source: &Source,
        canonical_url: &str,
        identity: &RemoteIdentity,
        client: crate::hub::Client,
    ) -> Result<(String, crate::hub::Client)> {
        let normalized = RemoteIdentity::new(&identity.hub, &identity.agent_id)?;
        ensure!(
            normalized == *identity,
            "frozen publication identity is not canonical"
        );
        let url = super::require_transport_url(canonical_url, identity)?
            .context("frozen publication requires a canonical HTTP Hub URL")?;
        ensure!(
            url == canonical_url,
            "frozen publication URL is not canonical"
        );
        ensure!(
            !url.chars().any(|character| character.is_control()
                || character.is_whitespace()
                || character == '\\'),
            "frozen publication URL contains non-canonical characters"
        );
        source.verify_identity(identity)?;
        ensure!(
            crate::hub::identity::normalize_hub(client.base())? == identity.hub,
            "publication client belongs to another Hub"
        );
        client.checked_access_token()?;
        Ok((url, client))
    }

    fn from_source(
        source: Source,
        plan: &PublicationPlan,
        url: String,
        identity: RemoteIdentity,
        client: Option<crate::hub::Client>,
        protocols: &str,
    ) -> Result<Self> {
        let content = captured::GitContent::capture(&source, plan)?;
        Self::from_captured(source, content, url, identity, client, protocols)
    }

    fn from_captured(
        source: Source,
        content: captured::GitContent,
        url: String,
        identity: RemoteIdentity,
        client: Option<crate::hub::Client>,
        protocols: &str,
    ) -> Result<Self> {
        let captured::GitContent {
            directory,
            format,
            lfs_objects,
            inspection_policy,
            plan,
            lfs_inventory,
        } = content;
        let mut preferences = source.http_preferences(&url)?;
        let lfs_url = format!("{}/info/lfs", url.trim_end_matches('/'));
        let batch_url = format!("{lfs_url}/objects/batch");
        let lfs_preferences = source
            .http_preferences(&batch_url)
            .map_err(|_| http::Failure::Configuration);
        let http = lfs_preferences
            .as_ref()
            .map_err(|error| *error)
            .and_then(|values| http::prepare(&batch_url, values, &source.environment))
            .map(|agent| (lfs_url, agent));
        insert_execution_constraints(&mut preferences, directory.path())?;
        let parameters = parameters(&preferences);
        validate_prepared_parameters(&parameters)?;
        let mut environment = source.process_environment();
        for name in ["SSL_CERT_FILE", "SSL_CERT_DIR"] {
            ensure!(
                !source.environment.contains_key(OsStr::new(name)),
                "frozen publication cannot preserve backend-specific {name}; configure Git http.sslCAInfo or http.sslCAPath explicitly"
            );
        }
        for (key, value) in [
            ("HOME", directory.path().join("home").into_os_string()),
            (
                "USERPROFILE",
                directory.path().join("home").into_os_string(),
            ),
            (
                "XDG_CONFIG_HOME",
                directory.path().join("home").into_os_string(),
            ),
            ("GIT_DIR", directory.path().as_os_str().to_owned()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            (
                "GIT_CONFIG_SYSTEM",
                directory.path().join("empty-config").into_os_string(),
            ),
            (
                "GIT_CONFIG_GLOBAL",
                directory.path().join("empty-config").into_os_string(),
            ),
            ("GIT_ALLOW_PROTOCOL", protocols.into()),
            ("GIT_NO_LAZY_FETCH", "1".into()),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
            ("GIT_ATTR_NOSYSTEM", "1".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
        ] {
            environment.insert(key.into(), value);
        }
        ensure!(
            !plan.heads().is_empty(),
            "frozen publication has no branch roots"
        );
        let oid_length = if format == "sha256" { 64 } else { 40 };
        let heads = batches(plan.heads(), &url, oid_length)?;
        let tags = batches(plan.tags(), &url, oid_length)?;
        let urls = client
            .as_ref()
            .map(|_| vec![url.clone()])
            .unwrap_or_default();
        let transport = TransportIdentity {
            client,
            urls,
            agent_id: Some(identity.agent_id.clone()),
            accept_secret_findings: false,
            lfs: None,
            execution: Some(Execution {
                root: directory.path().to_owned(),
                environment,
                parameters,
            }),
        };
        transport.environment()?;
        Ok(Self {
            transport,
            directory,
            url,
            identity,
            http,
            lfs_objects,
            lfs_preferences,
            inspection_policy,
            plan,
            lfs_inventory,
            heads,
            tags,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn identity(&self) -> &RemoteIdentity {
        &self.identity
    }

    /// The availability client is captured with this destination and account. It retains the
    /// ordinary API's WebPki baseline, explicit supported proxy/CA/header preferences and bounds.
    /// Unsupported availability preferences do not prevent Git-only publication preparation.
    /// This accessor performs no requests and does not select an LFS publication policy.
    pub fn prepared_lfs_http(&self) -> Result<(&str, &ureq::Agent)> {
        self.http
            .as_ref()
            .map(|(endpoint, agent)| (endpoint.as_str(), agent))
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// The original cache path is captured without creating or inspecting cached payloads.
    /// A missing cache is valid; errors remain deferred until local payloads are required.
    /// The path fixes lookup configuration, not file identity or bytes behind filesystem links.
    pub fn source_lfs_objects(&self) -> Result<&Path> {
        self.lfs_objects
            .as_deref()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// Advertisement and publication share the prepared endpoint, account and configuration.
    pub fn advertised_refs(&self) -> Option<RemoteRefs> {
        let output = super::capture_transport(
            self.directory.path(),
            &super::remote_ref_args(&self.url, true),
            Some(&self.transport),
        )?;
        super::parse_remote_refs(&output)
    }

    /// Every attempt retains its observations, including effects preceding a failed retry.
    /// Tags are attempted only after all branch batches are affirmatively acknowledged.
    pub(super) fn push_refs(&self) -> (PublicationPhase, Option<PublicationPhase>) {
        let heads = self.push_phase(&self.heads);
        let tags = heads.ok().then(|| self.push_phase(&self.tags));
        (heads, tags)
    }

    fn lfs_execution(&self, storage: &Path) -> Result<Execution> {
        let mut preferences = self
            .lfs_preferences
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .clone();
        let endpoint = &self
            .http
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .0;
        insert_execution_constraints(&mut preferences, self.directory.path())?;
        for (key, value) in [
            (format!("remote.{LFS_REMOTE}.url"), self.url.clone()),
            (format!("remote.{LFS_REMOTE}.pushurl"), self.url.clone()),
            ("lfs.storage".into(), path_text(storage)?),
        ] {
            preferences.insert(key, value);
        }
        let mut parameters = parameters(&preferences);
        super::constrain_lfs_parameters(&mut parameters, LFS_REMOTE, endpoint);
        validate_prepared_parameters(&parameters)?;
        let original = self
            .transport
            .execution
            .as_ref()
            .context("frozen publication execution is unavailable")?;
        Ok(Execution {
            root: original.root.clone(),
            environment: original.environment.clone(),
            parameters,
        })
    }

    fn push_phase(&self, batches: &[Vec<FrozenRef>]) -> PublicationPhase {
        let mut phase = PublicationPhase::default();
        for (index, batch) in batches.iter().enumerate() {
            let specs: Vec<_> = batch.iter().map(FrozenRef::refspec).collect();
            let mut args = vec![
                "push",
                "--porcelain",
                "--no-follow-tags",
                "--recurse-submodules=no",
                "--",
                &self.url,
            ];
            args.extend(specs.iter().map(String::as_str));
            let run =
                super::execute_transport(None, &args, &self.transport, super::OutputMode::Captured);
            let attempted = !run.attempts.is_empty();
            phase.attempts.extend(
                run.attempts
                    .into_iter()
                    .map(|attempt| PublicationAttempt::parse(index, batch, attempt)),
            );
            phase.error = run.error.map(|error| format!("{error:#}"));
            if phase.error.is_some() || !phase.attempts.last().is_some_and(PublicationAttempt::ok) {
                let start = if attempted { index + 1 } else { index };
                phase.unattempted = batches[start..].iter().flatten().cloned().collect();
                break;
            }
        }
        phase
    }
}

pub(super) struct Execution {
    root: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    pub(super) parameters: OsString,
}

impl Execution {
    #[cfg(all(test, feature = "cli"))]
    pub(super) fn fixture(
        root: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        parameters: OsString,
    ) -> Self {
        Self {
            root,
            environment,
            parameters,
        }
    }

    pub(super) fn validate_parameters(&self, parameters: &OsStr) -> Result<()> {
        ensure!(
            parameters
                .to_str()
                .context("frozen transport configuration is not valid UTF-8")?
                .encode_utf16()
                .count()
                <= 28 * 1024,
            "frozen transport configuration exceeds its environment limit"
        );
        Ok(())
    }

    pub(super) fn command(&self) -> Command {
        let mut command = super::transport_command();
        command
            .env_clear()
            .envs(&self.environment)
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .arg("--no-replace-objects")
            .current_dir(&self.root);
        crate::infra::git_runtime::configure(&mut command);
        command
    }
}

fn insert_execution_constraints(
    preferences: &mut BTreeMap<String, String>,
    directory: &Path,
) -> Result<()> {
    for (key, value) in [
        ("core.hooksPath", path_text(&directory.join("hooks"))?),
        ("core.fsmonitor", "false".into()),
        ("credential.helper", String::new()),
        ("credential.interactive", "false".into()),
        ("http.sslVerify", "true".into()),
        ("http.followRedirects", "false".into()),
        ("push.followTags", "false".into()),
        ("push.recurseSubmodules", "no".into()),
        ("push.negotiate", "false".into()),
        ("submodule.recurse", "false".into()),
    ] {
        preferences.insert(key.into(), value);
    }
    Ok(())
}

fn validate_prepared_parameters(parameters: &OsStr) -> Result<()> {
    ensure!(
        parameters.len() <= CONFIG_LIMIT,
        "frozen HTTP configuration exceeds its limit"
    );
    ensure!(
        parameters
            .to_str()
            .context("frozen HTTP configuration is not valid UTF-8")?
            .encode_utf16()
            .count()
            <= 16 * 1024,
        "frozen HTTP configuration exceeds its environment limit"
    );
    Ok(())
}

fn batches(refs: &[FrozenRef], url: &str, oid_length: usize) -> Result<Vec<Vec<FrozenRef>>> {
    let overhead = argument_units(url)
        .checked_add(256)
        .context("publication URL is too long")?;
    ensure!(
        overhead < REQUEST_UNITS,
        "publication URL exceeds the command limit"
    );
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut units = overhead;
    for reference in refs {
        ensure!(
            reference.oid().len() == oid_length,
            "publication object format differs from its source"
        );
        let spec = reference.refspec();
        let size = argument_units(&spec);
        ensure!(
            size + overhead <= REQUEST_UNITS,
            "publication ref exceeds the command limit"
        );
        if units + size > REQUEST_UNITS {
            batches.push(std::mem::take(&mut batch));
            units = overhead;
        }
        batch.push(reference.clone());
        units += size;
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    Ok(batches)
}

fn argument_units(value: &str) -> usize {
    // Quoting can expand backslashes and embedded quotes on Windows, even in valid Git refs.
    value
        .encode_utf16()
        .count()
        .saturating_mul(2)
        .saturating_add(3)
}

struct Source {
    root: PathBuf,
    gitdir: &'static str,
    environment: BTreeMap<OsString, OsString>,
}

impl Source {
    fn verify_identity(&self, observed: &RemoteIdentity) -> Result<()> {
        let Some(expected) = self
            .environment
            .get(OsStr::new(crate::hub::identity::EXPECTED_AGENT_ID_ENV))
        else {
            return Ok(());
        };
        let expected = expected
            .to_str()
            .context("expected remote identity is not valid UTF-8")?;
        let mut command = self.command();
        // The pin belongs to the selected source's local configuration, not injected parameters.
        for key in ["GIT_CONFIG", "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"] {
            command.env_remove(key);
        }
        command.args(["config", "--local", "--get", "agit.remoteIdentity"]);
        let output = bounded_inspection_output(command, CONFIG_LIMIT)?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "the publication source has no readable immutable remote identity; re-clone it before supervised publication"
        );
        let pinned: RemoteIdentity = serde_json::from_slice(&output.stdout)
            .context("the publication source remote identity is malformed")?;
        let pinned = RemoteIdentity::new(&pinned.hub, &pinned.agent_id)?;
        let pinned =
            crate::hub::identity::constrain_expected(pinned, &observed.hub, Some(expected))?;
        crate::hub::identity::verify_expected_target(Some(&pinned), observed)
    }

    fn new(repo: &Repo) -> Result<Self> {
        #[cfg(windows)]
        let environment = std::env::vars_os()
            .map(|(key, value)| {
                let key = key
                    .to_str()
                    .map(|key| OsString::from(key.to_ascii_uppercase()))
                    .unwrap_or(key);
                (key, value)
            })
            .collect();
        #[cfg(not(windows))]
        let environment = std::env::vars_os().collect();
        Self::at(repo, environment)
    }

    fn at(repo: &Repo, environment: BTreeMap<OsString, OsString>) -> Result<Self> {
        let root = inspection_git_path_spelling(
            repo.root()
                .canonicalize()
                .context("publication source is unavailable")?,
        );
        ensure!(root.is_dir(), "publication source is not a directory");
        let gitdir = if root.join(".git").exists() {
            ".git"
        } else {
            ensure!(
                root.join("HEAD").is_file() && root.join("objects").is_dir(),
                "publication source is not an exact Git repository"
            );
            "."
        };
        let source = Self {
            root,
            gitdir,
            environment,
        };
        if gitdir == "." {
            ensure!(
                source.text(&["rev-parse", "--is-bare-repository"])? == "true",
                "publication source is not a bare Git repository"
            );
        }
        Ok(source)
    }

    fn command(&self) -> Command {
        let mut command = super::transport_command();
        command.env_clear().envs(self.process_environment());
        for (key, value) in &self.environment {
            let Some(name) = key.to_str() else { continue };
            if matches!(
                name,
                "HOME"
                    | "USERPROFILE"
                    | "XDG_CONFIG_HOME"
                    | "GIT_CONFIG_NOSYSTEM"
                    | "GIT_CONFIG_SYSTEM"
                    | "GIT_CONFIG_GLOBAL"
                    | "GIT_CONFIG_COUNT"
                    | "GIT_CONFIG_PARAMETERS"
            ) || name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
            {
                command.env(key, value);
            }
        }
        command
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(&self.root)
            .args(["--git-dir", self.gitdir, "-c", "core.fsmonitor=false"]);
        crate::infra::git_runtime::configure(&mut command);
        command
    }

    fn output(&self, args: &[&str]) -> Result<Output> {
        let mut command = self.command();
        command.args(args);
        bounded_inspection_output(command, CONFIG_LIMIT)
    }

    fn text(&self, args: &[&str]) -> Result<String> {
        let output = self.output(args)?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "publication source inspection failed"
        );
        let text = std::str::from_utf8(&output.stdout)
            .context("publication source path is not valid UTF-8")?;
        Ok(text
            .strip_suffix('\n')
            .context("publication source inspection is incomplete")?
            .to_owned())
    }

    fn process_environment(&self) -> BTreeMap<OsString, OsString> {
        self.environment
            .iter()
            .filter(|(key, _)| {
                key.to_str().is_some_and(|name| {
                    matches!(
                        name,
                        "PATH"
                            | "SystemRoot"
                            | "SYSTEMROOT"
                            | "WINDIR"
                            | "ComSpec"
                            | "COMSPEC"
                            | "PATHEXT"
                            | "TEMP"
                            | "TMP"
                            | "TMPDIR"
                            | "HTTP_PROXY"
                            | "HTTPS_PROXY"
                            | "ALL_PROXY"
                            | "NO_PROXY"
                            | "http_proxy"
                            | "https_proxy"
                            | "all_proxy"
                            | "no_proxy"
                            | "CURL_SSL_BACKEND"
                    )
                })
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn http_preferences(&self, url: &str) -> Result<BTreeMap<String, String>> {
        let output = self.output(&[
            "config",
            "--includes",
            "--null",
            "--get-urlmatch",
            "http",
            url,
        ])?;
        ensure!(
            output.stderr.is_empty()
                && (output.status.success()
                    || (output.status.code() == Some(1) && output.stdout.is_empty())),
            "cannot capture frozen HTTP configuration"
        );
        let mut values = BTreeMap::new();
        ensure!(
            output.stdout.is_empty() || output.stdout.ends_with(&[0]),
            "HTTP configuration framing is incomplete"
        );
        for entry in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry =
                std::str::from_utf8(entry).context("HTTP configuration is not valid UTF-8")?;
            let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
            if matches!(key, "http.extraheader" | "http.followredirects") {
                continue;
            }
            ensure!(
                supported_http_key(key),
                "frozen publication does not support HTTP preference {key}"
            );
            values.insert(key.to_owned(), value.to_owned());
        }
        for (name, key) in [
            ("GIT_HTTP_USER_AGENT", "http.useragent"),
            ("GIT_HTTP_LOW_SPEED_LIMIT", "http.lowspeedlimit"),
            ("GIT_HTTP_LOW_SPEED_TIME", "http.lowspeedtime"),
            ("GIT_HTTP_MAX_REQUESTS", "http.maxrequests"),
            ("GIT_SSL_CAINFO", "http.sslcainfo"),
            ("GIT_SSL_CAPATH", "http.sslcapath"),
            ("GIT_SSL_CERT", "http.sslcert"),
            ("GIT_SSL_KEY", "http.sslkey"),
            ("GIT_SSL_CERT_TYPE", "http.sslcerttype"),
            ("GIT_SSL_KEY_TYPE", "http.sslkeytype"),
            ("GIT_SSL_VERSION", "http.sslversion"),
            ("GIT_SSL_CIPHER_LIST", "http.sslcipherlist"),
            ("GIT_PROXY_SSL_CAINFO", "http.proxysslcainfo"),
            ("GIT_PROXY_SSL_CERT", "http.proxysslcert"),
            ("GIT_PROXY_SSL_KEY", "http.proxysslkey"),
            ("GIT_HTTP_PROXY_AUTHMETHOD", "http.proxyauthmethod"),
        ] {
            if let Some(value) = self.environment.get(OsStr::new(name)) {
                values.insert(
                    key.into(),
                    value
                        .to_str()
                        .context("HTTP preference environment is not valid UTF-8")?
                        .into(),
                );
            }
        }
        for name in [
            "GIT_SSL_CERT_PASSWORD_PROTECTED",
            "GIT_PROXY_SSL_CERT_PASSWORD_PROTECTED",
        ] {
            ensure!(
                !self.environment.contains_key(OsStr::new(name)),
                "frozen publication cannot preserve interactive certificate preference {name}"
            );
        }
        if let Some(value) = self.environment.get(OsStr::new("GIT_SSL_NO_VERIFY")) {
            ensure!(
                !git_bool(
                    value
                        .to_str()
                        .context("invalid TLS verification preference")?
                )?,
                "frozen publication requires TLS certificate verification"
            );
        }
        for key in ["http.sslverify", "http.proxysslverify"] {
            if let Some(value) = values.get(key) {
                ensure!(
                    git_bool(value)?,
                    "frozen publication requires TLS certificate verification"
                );
            }
        }
        for (key, value) in &values {
            ensure!(
                !value.chars().any(char::is_control),
                "frozen publication HTTP preference {key} contains control characters"
            );
        }
        for (key, allowed) in [
            ("http.sslcerttype", ["PEM", "DER", "P12"].as_slice()),
            ("http.sslkeytype", ["PEM", "DER"].as_slice()),
        ] {
            if let Some(value) = values.get(key) {
                ensure!(
                    allowed
                        .iter()
                        .any(|allowed| value.eq_ignore_ascii_case(allowed)),
                    "frozen publication requires file-based certificate and key types; unsupported {key}"
                );
            }
        }
        for key in [
            "http.sslcainfo",
            "http.sslcapath",
            "http.sslcert",
            "http.sslkey",
            "http.proxysslcainfo",
            "http.proxysslcert",
            "http.proxysslkey",
        ] {
            if let Some(value) = values.get_mut(key) {
                ensure!(
                    !value.is_empty(),
                    "frozen publication needs an explicit certificate path for {key}"
                );
                *value = path_text(&self.preference_path(value)?)?;
            }
        }
        if let Some(value) = values.get_mut("http.pinnedpubkey")
            && !value.starts_with("sha256//")
        {
            *value = path_text(&self.preference_path(value)?)?;
        }
        Ok(values)
    }

    fn preference_path(&self, value: &str) -> Result<PathBuf> {
        if let Some(value) = value.strip_prefix("~/") {
            let home = self
                .environment
                .get(OsStr::new("HOME"))
                .or_else(|| self.environment.get(OsStr::new("USERPROFILE")))
                .context("certificate path needs a home directory")?;
            return absolute_path(
                &self.root,
                path_text(&PathBuf::from(home).join(value))?.as_str(),
            );
        }
        ensure!(
            !value.starts_with('~') && !value.starts_with("%(prefix)"),
            "frozen publication requires an explicit certificate path"
        );
        let lower = value.to_ascii_lowercase();
        ensure!(
            !lower.starts_with("pkcs11:")
                && ![
                    "currentuser/",
                    "currentuser\\",
                    "localmachine/",
                    "localmachine\\"
                ]
                .iter()
                .any(|prefix| lower.starts_with(prefix)),
            "frozen publication requires certificate files rather than engine or certificate-store selectors"
        );
        absolute_path(&self.root, value)
    }
}

fn supported_http_key(key: &str) -> bool {
    matches!(
        key,
        "http.proxy"
            | "http.proxyauthmethod"
            | "http.proxysslcert"
            | "http.proxysslkey"
            | "http.proxysslcainfo"
            | "http.proxysslverify"
            | "http.sslverify"
            | "http.sslcainfo"
            | "http.sslcapath"
            | "http.sslcert"
            | "http.sslkey"
            | "http.sslcerttype"
            | "http.sslkeytype"
            | "http.sslbackend"
            | "http.sslversion"
            | "http.sslcipherlist"
            | "http.pinnedpubkey"
            | "http.schannelusesslcainfo"
            | "http.schannelcheckrevoke"
            | "http.useragent"
            | "http.lowspeedlimit"
            | "http.lowspeedtime"
            | "http.version"
            | "http.maxrequests"
            | "http.minsessions"
            | "http.postbuffer"
            | "http.keepaliveidle"
            | "http.keepaliveinterval"
            | "http.keepalivecount"
            | "http.suppressconnectheaders"
    )
}

fn git_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" | "" => Ok(false),
        _ => anyhow::bail!("invalid TLS verification preference"),
    }
}

fn absolute_path(root: &Path, value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "publication path is empty or contains control characters"
    );
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn path_text(path: &Path) -> Result<String> {
    let path = inspection_git_path_spelling(path.to_owned());
    let text = path
        .to_str()
        .context("frozen publication paths must be valid UTF-8")?;
    ensure!(
        !text.chars().any(char::is_control),
        "publication path contains control characters"
    );
    Ok(text.to_owned())
}

fn parameters(values: &BTreeMap<String, String>) -> OsString {
    values
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                super::quote_git_parameter(key),
                super::quote_git_parameter(value)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

#[cfg(test)]
#[path = "frozen_tests.rs"]
mod tests;
