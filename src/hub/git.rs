//! Git subprocesses that talk to the hub.
//!
//! # Why here and not `domain::repo`
//!
//! `clone` / `fetch` / `push` are the only git operations that need **authentication**, and
//! authentication is the hub's business: where the token comes from and how it is exchanged
//! once it expires — both answers live next door to this module. `domain::repo` owns "what a
//! repo looks like", and those operations (`add` / `commit` / `tag` / `show`) are entirely
//! local and need no credentials.
//!
//! # The token travels in the environment, not into `.git/config` and not into argv
//!
//! Git's configuration environment scopes the authorization and identity headers to the
//! validated repository URL. An empty entry resets inherited headers at that scope. Redirects
//! are disabled there because Git can reuse the initial URL's headers for later protocol calls.
//!
//! Of the three routes, only this one is safe:
//!
//! * Writing it into the remote URL (`https://x:<token>@hub/...`) **persists** it into
//!   `.git/config`, and an access token is due for exchange within the hour; worse, it turns up
//!   in `git remote -v` output, in push error messages, and in any log that gets pasted.
//! * `git -c http.extraHeader=...` puts the token in argv, where any user on the same machine
//!   sees it with `ps`.
//! * An environment variable is visible only to this one subprocess and is gone once the process
//!   exits, leaving no credential on disk. The immutable `agent_id` itself persists separately as
//!   a repo-local pin; it is not a secret.
//!
//! [`redact_url`] stays regardless: a user may have configured a URL with credentials by hand,
//! and the credentials have to come off before we print it.
//!
//! # A 401 is retried once
//!
//! An access token is valid for only an hour, so "the token expired" is the normal case, not an
//! exception. The approach matches [`super::Client`]'s REST retry: check the locally recorded
//! expiry first and exchange the token when it has passed; a 401 after that exchange means the
//! refresh token is dead too, and retrying past it only fills the server's logs.
//!
//! Git's exit code cannot identify an authentication failure (they are all 128), so the test is
//! a keyword in stderr. stderr is forwarded to the user and kept at the same time — plain
//! `inherit` leaves nothing to judge on, and capturing all of it makes push's progress bar
//! disappear, so a large repo looks hung.

use crate::Result;
use crate::domain::repo::Repo;
use anyhow::Context;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// What an authentication failure looks like in git's stderr.
///
/// Covers three shapes: smart-http answering 401/403 directly, git translating that into
/// "Authentication failed", and git turning to the terminal for a username (which means it never
/// got usable credentials).
const AUTH_MARKERS: &[&str] = &[
    "error: 401",
    "error: 403",
    "HTTP 401",
    "HTTP 403",
    "returned error: 401",
    "returned error: 403",
    "Authentication failed",
    "could not read Username",
    "terminal prompts disabled",
];

pub(crate) fn looks_like_auth_failure(stderr: &str) -> bool {
    AUTH_MARKERS.iter().any(|m| stderr.contains(m))
}

/// Authentication classification reads Git's diagnostics, so transport subprocesses use a
/// stable diagnostic language without changing the caller's environment.
fn transport_command() -> Command {
    let mut command = Command::new("git");
    command.env("LC_ALL", "C").env("LANGUAGE", "C");
    command
}

/// Inherited Git parameters retain their bytes and precedence; transport guards follow them.
fn transport_env(
    token: Option<&str>,
    expected_agent_id: &str,
    urls: &[String],
) -> Vec<(String, OsString)> {
    let inherited = std::env::var_os("GIT_CONFIG_PARAMETERS");
    transport_env_after(inherited.as_deref(), token, expected_agent_id, urls)
}

fn quote_git_parameter(value: &str) -> String {
    let mut quoted = String::from("'");
    for character in value.chars() {
        if matches!(character, '\'' | '!') {
            quoted.push('\'');
            quoted.push('\\');
            quoted.push(character);
            quoted.push('\'');
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn transport_env_after(
    inherited: Option<&OsStr>,
    token: Option<&str>,
    expected_agent_id: &str,
    urls: &[String],
) -> Vec<(String, OsString)> {
    let mut settings = vec![("http.extraHeader".to_string(), String::new())];
    for url in urls {
        let key = format!("http.{url}.extraHeader");
        settings.push((key.clone(), String::new()));
        if let Some(token) = token {
            settings.push((key.clone(), format!("Authorization: Bearer {token}")));
        }
        settings.push((
            key,
            format!(
                "{}: {expected_agent_id}",
                super::identity::EXPECTED_AGENT_ID_HEADER
            ),
        ));
        settings.push((format!("http.{url}.followRedirects"), "false".into()));
    }
    let mut parameters = inherited.unwrap_or_default().to_os_string();
    for (key, value) in settings {
        if !parameters.is_empty() {
            parameters.push(" ");
        }
        parameters.push(quote_git_parameter(&key));
        parameters.push("=");
        parameters.push(quote_git_parameter(&value));
    }
    vec![("GIT_CONFIG_PARAMETERS".into(), parameters)]
}

/// Destination validation and credential selection stay fixed across Git retries.
struct TransportIdentity {
    client: Option<super::Client>,
    urls: Vec<String>,
    agent_id: String,
}

impl TransportIdentity {
    fn new(
        dir: Option<&Path>,
        args: &[&str],
        identity: &super::identity::RemoteIdentity,
    ) -> Result<Self> {
        let command_index = args
            .iter()
            .position(|arg| matches!(*arg, "clone" | "fetch" | "push" | "pull" | "ls-remote"))
            .context("Git transport command is missing")?;
        let command = args[command_index];
        let requested = args
            .iter()
            .skip(command_index + 1)
            .find(|arg| !arg.starts_with('-'))
            .copied()
            .context("Git transport remote is missing")?;
        let temporary;
        // Clone reads global Git configuration without adopting the enclosing repository's
        // local configuration. URL expansion must use the same configuration boundary.
        let directory = match dir {
            Some(directory) if command != "clone" => directory,
            _ => {
                temporary = tempfile::tempdir()?;
                temporary.path()
            }
        };
        let repo = Repo::at(directory);
        let named = matches!(command, "push" | "fetch" | "pull");
        let mut urls = Vec::new();
        let mut unauthenticated = false;
        let destinations = if named {
            named_destinations(&repo, requested, command == "push")?
        } else {
            explicit_destination(&repo, requested)?
        };
        for (original, effective) in destinations {
            let original_scope = require_transport_url(&original, identity)?;
            let effective_scope = require_transport_url(&effective, identity)?;
            if original_scope.is_some()
                && let Some(url) = effective_scope
            {
                if !urls.contains(&url) {
                    urls.push(url);
                }
            } else {
                unauthenticated = true;
            }
        }
        anyhow::ensure!(
            urls.is_empty() || !unauthenticated,
            "a Git remote cannot mix authenticated Hub URLs with other transports"
        );
        let client = (!urls.is_empty()).then(|| super::Client::for_stored_hub(&identity.hub));
        Ok(Self {
            client,
            urls,
            agent_id: identity.agent_id.clone(),
        })
    }

    fn token(&self) -> Result<Option<String>> {
        self.client
            .as_ref()
            .map(super::Client::checked_access_token)
            .transpose()
            .map(Option::flatten)
    }

    fn refresh(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(super::Client::refresh_access)
    }

    fn environment(&self) -> Result<Vec<(String, OsString)>> {
        Ok(transport_env(
            self.token()?.as_deref(),
            &self.agent_id,
            &self.urls,
        ))
    }
}

fn explicit_destination(repo: &Repo, requested: &str) -> Result<Vec<(String, String)>> {
    let effective = repo.git(&["ls-remote", "--get-url", "--", requested])?;
    let effective = effective.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(!effective.is_empty(), "Git transport URL is missing");
    Ok(vec![(requested.to_string(), effective.to_string())])
}

fn named_destinations(repo: &Repo, requested: &str, push: bool) -> Result<Vec<(String, String)>> {
    let mut args = vec!["remote", "get-url", "--all"];
    if push {
        args.push("--push");
    }
    args.push(requested);
    let Some(effective) = repo.git_opt(&args) else {
        return explicit_destination(repo, requested);
    };
    let read_urls = |field| {
        repo.git_opt(&[
            "config",
            "--get-all",
            &format!("remote.{requested}.{field}"),
        ])
    };
    let original = if push {
        read_urls("pushurl").or_else(|| read_urls("url"))
    } else {
        read_urls("url")
    }
    .context("Git transport remote changed while reading its URLs")?;
    let mut original: Vec<_> = original.lines().map(str::to_string).collect();
    let mut effective: Vec<_> = effective.lines().map(str::to_string).collect();
    if !push {
        original.truncate(1);
        effective.truncate(1);
    }
    anyhow::ensure!(
        !original.is_empty() && original.len() == effective.len(),
        "Git transport remote changed while reading its URLs"
    );
    Ok(original.into_iter().zip(effective).collect())
}

/// The result of one git subprocess.
///
/// Carries stderr and not just the exit code: git reports every **server-side** rejection with
/// exit code 128, and the reason is only in stderr. The reason decides what happens next — "the
/// remote has moved ahead" needs a fetch, "the content was rejected by a gate" needs the content
/// changed, and the two have nothing to do with each other. A caller that cannot see stderr can
/// only collapse every failure into one sentence — a 422 arriving as "the remote has moved ahead
/// / an authentication problem".
pub struct Outcome {
    pub code: i32,
    pub stderr: String,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// The HTTP status that appears in stderr (git prints it verbatim).
    ///
    /// This is the only server-side semantics a client can get: git does not surface the
    /// response body (`remote-curl` sets `CURLOPT_FAILONERROR`, and curl disconnects the moment
    /// it sees a 4xx).
    pub fn http_status(&self) -> Option<u16> {
        let re = regex::Regex::new(r"\b(?:HTTP|error:)\s*(4\d\d|5\d\d)\b").ok()?;
        re.captures(&self.stderr)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
    }
}

/// Run a git command that needs authentication.
///
/// The identity is read from the repo-local pin, so a caller has no chance to forget the fencing
/// id on some fetch/push.
pub fn run(repo: &Repo, args: &[&str]) -> Result<Outcome> {
    let identity =
        super::identity::require_current_expected(repo, &crate::infra::config::hub_url())?;
    run_for_identity(Some(repo.root()), args, &identity)
}

fn run_for_identity(
    dir: Option<&Path>,
    args: &[&str],
    identity: &super::identity::RemoteIdentity,
) -> Result<Outcome> {
    let transport = TransportIdentity::new(dir, args, identity)?;
    if transport
        .client
        .as_ref()
        .is_some_and(super::Client::access_expired)
    {
        transport.refresh();
    }

    let (code, stderr) = spawn(dir, args, &transport)?;
    if code == 0 || !looks_like_auth_failure(&stderr) {
        return Ok(Outcome { code, stderr });
    }

    // Authentication failed: exchange the token once and try again. When the exchange fails,
    // hand this result back — the caller's hint (`agit login`) is more useful than reporting it
    // again ourselves.
    if !transport.refresh() {
        return Ok(Outcome { code, stderr });
    }
    let (code, stderr) = spawn(dir, args, &transport)?;
    Ok(Outcome { code, stderr })
}

/// Advertised branches and unpeeled tags from an identity-fenced remote probe.
#[derive(Default)]
pub struct RemoteRefs {
    pub heads: Vec<String>,
    pub tags: std::collections::BTreeMap<String, String>,
}

/// An unavailable or malformed advertisement is unknown and cannot justify skipping content.
/// Branch tips narrow the secret scan; exact tag objects identify already published tags.
pub fn ls_remote_refs(dir: &Path, url: &str, include_tags: bool) -> Option<RemoteRefs> {
    let repo = Repo::at(dir);
    let identity =
        super::identity::require_current_expected(&repo, &crate::infra::config::hub_url()).ok()?;
    require_transport_url(url, &identity).ok()?;
    let out = capture(dir, &remote_ref_args(url, include_tags), Some(&identity))?;
    parse_remote_refs(&out)
}

fn remote_ref_args(url: &str, include_tags: bool) -> Vec<&str> {
    let mut args = vec!["ls-remote", "--refs", "--heads"];
    if include_tags {
        args.push("--tags");
    }
    args.push(url);
    args
}

fn parse_remote_refs(out: &str) -> Option<RemoteRefs> {
    let mut refs = RemoteRefs::default();
    for line in out.lines() {
        let (oid, name) = line.split_once('\t')?;
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if name.starts_with("refs/heads/") {
            refs.heads.push(oid.to_string());
        } else if name.starts_with("refs/tags/") && !name.ends_with("^{}") {
            if refs
                .tags
                .insert(name.to_string(), oid.to_string())
                .is_some()
            {
                return None;
            }
        } else {
            return None;
        }
    }
    Some(refs)
}

/// How long one read-only probe waits at most.
///
/// This cap is required, not a tuning knob. `GIT_TERMINAL_PROMPT=0` blocks an **interactive**
/// hang; it does not block a network one: when an address is blackholed, TCP connect waits for
/// the kernel's connection timeout, observed at **75 seconds**. And what calls this is
/// `agit scan` / `agit push --dry-run` — a local operation in the user's eyes, and a standalone
/// entry point in CI. Letting one local scan stall that long because the hub is unreachable is the
/// cost most easily overlooked when introducing this network round trip.
///
/// The direction of a timeout is safe: a failed probe → `Destination::Unknown` → **a full scan**.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run a git command that needs authentication and **take its stdout**. Any failure is `None`.
///
/// Separate from [`run`] because that one leaves stdout to the user (push's progress), while
/// here stdout is the answer itself. A 401 is likewise retried once; the reason is in the module
/// header.
///
/// Carries [`PROBE_TIMEOUT`]: a timeout counts as a failure, and the caller takes the "no
/// answer" path from it.
///
/// # The side waiting out the timeout **cannot** be the only side doing anything
///
/// `try_wait` only asks "has it exited"; it does not read the pipes. stdout and stderr are both
/// pipes, holding on the order of 64 KiB: with nobody draining them during the loop, git fills
/// one, blocks forever in `write()` and **never exits**, so `try_wait` answers `Ok(None)`
/// forever and the timeout is what finally cuts it down.
///
/// This is not rare. The output of `ls-remote --heads` grows linearly with the branch count, and
/// in this product every session line is one `refs/heads/*` ([`ls_remote_refs`]'s own doc is
/// discussing "a repo of a thousand turns has a thousand refs") — observed: 1201 branches →
/// 106 950 bytes → every probe stalls out [`PROBE_TIMEOUT`].
///
/// And once it stalls, more than this one probe is broken: the `Destination::Advertised` path is
/// dead in a repo like that, the scan surface falls back to full forever, so a repo with **not a
/// single secret** — long history and many branches, nothing else — first stalls out
/// [`PROBE_TIMEOUT`] and is then stopped by the full-surface budget, leaving the user no way
/// forward.
///
/// So each pipe gets its own reader thread ([`spawn`] in this file already forwards stderr the
/// same way), and the main thread only waits for exit and kills at the deadline, never blocking
/// on a pipe even once.
///
/// # The timeout path does **not** join
///
/// Git starts helper processes (`git-remote-https` when `ls-remote` goes over https), and a
/// helper **inherits** both pipe write ends. Killing git does not kill the helper: while the
/// helper is still stuck in connect the write ends stay open, the reader threads never see EOF,
/// and a join pins the main thread back onto a pipe — exactly the thing being eliminated here,
/// only somewhere else. So after a timeout the handles are dropped: the answer is not wanted any
/// more, and each thread finishes on its own when the write end closes (nothing leaks, no zombie
/// is left — the helper is git's child, not ours).
fn capture(
    dir: &Path,
    args: &[&str],
    identity: Option<&super::identity::RemoteIdentity>,
) -> Option<String> {
    let transport = identity
        .map(|identity| TransportIdentity::new(Some(dir), args, identity))
        .transpose()
        .ok()?;
    let once = || -> Option<(bool, String, String)> {
        let mut cmd = transport_command();
        cmd.arg("-C").arg(dir);
        cmd.args(args);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if let Some(transport) = &transport {
            for (k, v) in transport.environment().ok()? {
                cmd.env(k, v);
            }
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().ok()?;
        // The pipes go to reader threads **immediately**, with nothing between the spawn and
        // here: from this moment on, whatever git writes has someone taking it, so it never
        // fails to exit because it cannot write.
        let (Some(out_pipe), Some(err_pipe)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        let out_rx = drain(out_pipe);
        let err_rx = drain(err_pipe);

        let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        // Kill and then wait: without the wait a zombie is left, and
                        // `Child::drop` does not wait.
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        // git has exited → the write ends it held are closed → both reader threads see EOF and
        // deliver. Still bounded: should another process still hold a write end, this must not
        // become a second unbounded wait.
        let stdout = out_rx.recv_timeout(PROBE_TIMEOUT).ok()?;
        let stderr = err_rx.recv_timeout(PROBE_TIMEOUT).unwrap_or_default();
        Some((
            status.success(),
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        ))
    };
    let (ok, stdout, stderr) = once()?;
    if ok {
        return Some(stdout);
    }
    let transport = transport.as_ref()?;
    if !looks_like_auth_failure(&stderr) || !transport.refresh() {
        return None;
    }
    let (ok, stdout, _) = once()?;
    ok.then_some(stdout)
}

/// Start a thread that **drains** this pipe, and hand the whole byte run back over a channel.
///
/// Returns a [`Receiver`](std::sync::mpsc::Receiver) rather than a `JoinHandle` so the caller
/// can **give up waiting**: `join` has only the blocking form, and the timeout path must be able
/// to leave without waiting (the reason is in [`capture`]). Dropping the `Receiver` still lets
/// the thread finish — a failed `send` only ends it.
fn drain(mut pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // A read error is handled the same as end of input, "that is all there is": the verdict
        // itself is on the exit-code side.
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

fn checked_transport_path(url: &str) -> Result<()> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let path = rest
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or_default();
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| char::from(byte).to_digit(16));
            let low = bytes.next().and_then(|byte| char::from(byte).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                anyhow::bail!("the Git destination has an invalid encoded path");
            };
            let byte = (high * 16 + low) as u8;
            anyhow::ensure!(
                !matches!(byte, b'/' | b'\\'),
                "the Git destination contains an encoded path separator"
            );
            decoded.push(byte);
        } else {
            decoded.push(byte);
        }
    }
    anyhow::ensure!(
        !decoded.iter().any(u8::is_ascii_control)
            && !decoded
                .split(|byte| matches!(byte, b'/' | b'\\'))
                .any(|part| part == b"." || part == b".."),
        "the Git destination contains an unsafe path segment"
    );
    Ok(())
}

fn require_transport_url(
    url: &str,
    identity: &super::identity::RemoteIdentity,
) -> Result<Option<String>> {
    let scheme = url.split(':').next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Ok(None);
    }
    checked_transport_path(&identity.hub)?;
    checked_transport_path(url)?;
    let authority = crate::infra::hub_authority::HubAuthority::parse(&identity.hub)?;
    anyhow::ensure!(
        authority.matches(url),
        "the Git destination does not belong to the pinned Hub"
    );
    let hub = super::identity::normalize_hub(&identity.hub)?;
    let destination = super::identity::normalize_hub(url)?;
    anyhow::ensure!(
        destination.starts_with(&format!("{hub}/")),
        "the Git destination does not belong to the pinned Hub"
    );
    Ok(Some(destination))
}

/// Clone a repo on the hub, and pin the same remote identity into the new checkout as soon as it
/// succeeds.
pub fn clone(
    url: &str,
    dest: &Path,
    identity: &super::identity::RemoteIdentity,
) -> Result<Outcome> {
    require_transport_url(url, identity)?;
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).with_context(|| format!("cannot create {}", p.display()))?;
    }
    let dest_s = dest.to_string_lossy().to_string();
    let out = run_for_identity(None, &["clone", "--quiet", url, &dest_s], identity)?;
    if out.ok() {
        super::identity::pin(&Repo::at(dest), identity)?;
    }
    Ok(out)
}

/// Run once, forwarding stderr while keeping a copy.
/// Transfer subcommands get `--progress` (right after the subcommand; position matters to git).
fn with_progress<'a>(args: &[&'a str], tty: bool) -> Vec<&'a str> {
    let mut full: Vec<&'a str> = args.to_vec();
    if tty
        && let Some(pos) = full
            .iter()
            .position(|a| matches!(*a, "fetch" | "clone" | "push" | "pull"))
    {
        full.insert(pos + 1, "--progress");
    }
    full
}

fn spawn(
    dir: Option<&Path>,
    args: &[&str],
    transport: &TransportIdentity,
) -> Result<(i32, String)> {
    let mut cmd = transport_command();
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    let full = with_progress(args, std::io::IsTerminal::is_terminal(&std::io::stderr()));
    cmd.args(&full);
    // Give git no chance to ask for a password interactively: with no usable token it must fail
    // immediately and let us see the authentication marker, instead of hanging on input in a
    // non-interactive environment.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in transport.environment()? {
        cmd.env(k, v);
    }

    let mut child = cmd
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    let mut captured = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        // Read in byte chunks rather than by line: git's progress refreshes in place with `\r`,
        // so a line-based read waits for a newline and shows nothing of the progress until the
        // end.
        let mut chunk = [0u8; 4096];
        let mut sink = std::io::stderr();
        loop {
            match err.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = sink.write_all(&chunk[..n]);
                    let _ = sink.flush();
                    captured.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }

    let status = child.wait().context("failed to wait for git to exit")?;
    Ok((
        status.code().unwrap_or(1),
        String::from_utf8_lossy(&captured).into_owned(),
    ))
}

/// Strip the credentials out of a URL.
///
/// We never write a token into a URL ourselves, but a user may have configured one by hand; and
/// terminal output ends up in CI logs.
pub fn redact_url(url: &str) -> String {
    let Some(i) = url.find("://") else {
        return url.to_string();
    };
    let (scheme, rest) = url.split_at(i + 3);
    match rest.find('@') {
        Some(at) if !rest[..at].contains('/') => format!("{scheme}***@{}", &rest[at + 1..]),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod progress_tests {
    use super::with_progress;

    /// On a tty a transfer subcommand carries `--progress`, right after the subcommand; every
    /// other case is passed through unchanged.
    #[test]
    fn progress_follows_the_transfer_subcommand_only_on_a_tty() {
        assert_eq!(
            with_progress(&["fetch", "origin", "--tags"], true),
            vec!["fetch", "--progress", "origin", "--tags"]
        );
        assert_eq!(
            with_progress(&["fetch", "origin"], false),
            vec!["fetch", "origin"]
        );
        assert_eq!(
            with_progress(&["ls-remote", "origin"], true),
            vec!["ls-remote", "origin"]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_guards_follow_the_unmodified_inherited_parameters() {
        let inherited = OsStr::new("'fixture.keep'='unchanged' 'http.extraHeader'='inherited'");
        let environment = transport_env_after(
            Some(inherited),
            Some("synthetic-token"),
            "00000000-0000-0000-0000-000000000001",
            &["https://hub.example.test/alice/notes.git".into()],
        );
        assert_eq!(environment.len(), 1);
        assert_eq!(environment[0].0, "GIT_CONFIG_PARAMETERS");
        assert!(
            environment[0]
                .1
                .as_encoded_bytes()
                .starts_with(inherited.as_encoded_bytes())
        );
        assert!(
            environment[0].1.to_str().unwrap().ends_with(
                "'http.https://hub.example.test/alice/notes.git.followRedirects'='false'"
            )
        );
    }

    #[test]
    fn scan_probe_omits_tags_without_changing_advertised_branch_tips() {
        let work = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(work.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .output()
                .unwrap();
            assert!(output.status.success(), "local Git fixture must succeed");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "--allow-empty", "--no-gpg-sign", "-m", "base"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&[
            "-c",
            "tag.gpgSign=false",
            "tag",
            "-a",
            "release",
            "-m",
            "release",
        ]);
        let tag = git(&["rev-parse", "refs/tags/release"]);
        let url = work.path().to_str().unwrap();
        let scan = parse_remote_refs(&git(&remote_ref_args(url, false))).unwrap();
        let push = parse_remote_refs(&git(&remote_ref_args(url, true))).unwrap();
        assert_eq!(scan.heads, [head]);
        assert_eq!(scan.heads, push.heads);
        assert!(scan.tags.is_empty());
        assert_eq!(push.tags.get("refs/tags/release"), Some(&tag));
    }

    #[test]
    fn explicit_transport_urls_preserve_the_pinned_hub_route() {
        let identity = super::super::identity::RemoteIdentity::new(
            "https://hub.example.test:8177/AgentGit",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        assert!(
            require_transport_url(
                "HTTPS://HUB.EXAMPLE.TEST:8177/AgentGit/alice/notes.git",
                &identity
            )
            .is_ok()
        );
        for url in [
            "https://other.example.test:8177/AgentGit/alice/notes.git",
            "https://hub.example.test:8178/AgentGit/alice/notes.git",
            "https://hub.example.test/AgentGit/alice/notes.git",
            "http://hub.example.test:8177/AgentGit/alice/notes.git",
            "https://hub.example.test:8177/agentgit/alice/notes.git",
            "https://hub.example.test:8177/AgentGitElsewhere/alice/notes.git",
            "https://hub.example.test:8177/alice/notes.git",
            "https://user:secret@hub.example.test:8177/AgentGit/alice/notes.git",
            "https://hub.example.test:8177/AgentGit/alice/notes.git?private",
            "https://hub.example.test:8177/AgentGit/alice/notes.git#private",
            "https://hub.example.test:8177/AgentGit/../alice/notes.git",
            "https://hub.example.test:8177/AgentGit/./alice/notes.git",
            "https://hub.example.test:8177/AgentGit/%2e%2E/alice/notes.git",
            "https://hub.example.test:8177/AgentGit/.%2e/alice/notes.git",
            "https://hub.example.test:8177/AgentGit/%2e./alice/notes.git",
            "https://hub.example.test:8177/AgentGit/%2e%2e%2falice/notes.git",
            "https://hub.example.test:8177/AgentGit/alice%2fnotes.git",
            "https://hub.example.test:8177/AgentGit/%2f..%2falice/notes.git",
            "https://hub.example.test:8177/AgentGit/%5c..%5calice/notes.git",
            "https://hub.example.test:8177/AgentGit/%00/alice/notes.git",
            "https://hub.example.test:8177/AgentGit/%invalid/alice/notes.git",
        ] {
            assert!(require_transport_url(url, &identity).is_err());
        }
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("missing/clone");
        assert!(
            clone(
                "https://other.example.test/alice/notes.git",
                &destination,
                &identity
            )
            .is_err()
        );
        assert!(!destination.parent().unwrap().exists());
    }

    #[test]
    fn advertised_tags_keep_their_exact_objects_separate_from_branch_tips() {
        let commit = "a".repeat(40);
        let tag = "b".repeat(40);
        let refs = parse_remote_refs(&format!(
            "{commit}\trefs/heads/main\n{tag}\trefs/tags/release\n"
        ))
        .unwrap();
        assert_eq!(refs.heads, [commit]);
        assert_eq!(refs.tags.get("refs/tags/release"), Some(&tag));
        assert!(parse_remote_refs("").unwrap().tags.is_empty());
        for malformed in [
            "bad\trefs/tags/release\n".to_string(),
            format!("{tag}\trefs/tags/release^{{}}\n"),
            format!("{tag}\trefs/tags/release\n{tag}\trefs/tags/release\n"),
        ] {
            assert!(parse_remote_refs(&malformed).is_none());
        }
    }

    #[test]
    fn token_never_appears_in_a_url_or_argv() {
        // The whole reason this module exists: the token lives only in an environment variable.
        let e = transport_env_after(
            None,
            Some("s3cret"),
            "00000000-0000-0000-0000-000000000001",
            &["https://hub.example.test/alice/notes.git".into()],
        );
        assert!(
            e.iter().all(|(k, _)| k.starts_with("GIT_CONFIG_")),
            "only GIT_CONFIG_* entries are produced"
        );
        // Authentication does not change the URL.
        assert_eq!(
            redact_url("https://hub.corp.com/alice/photo.git"),
            "https://hub.corp.com/alice/photo.git"
        );
    }

    #[test]
    fn auth_markers_cover_the_shapes_git_actually_prints() {
        for line in [
            "fatal: unable to access 'http://h/a.git/': The requested URL returned error: 403",
            "remote: HTTP 401 Unauthorized",
            "fatal: Authentication failed for 'http://h/a.git/'",
            "fatal: could not read Username for 'http://h': terminal prompts disabled",
        ] {
            assert!(
                looks_like_auth_failure(line),
                "must read as an auth failure: {line}"
            );
        }
        // A non-fast-forward is not an authentication problem — misreading it wastes a refresh.
        for line in [
            "! [rejected] main -> main (fetch first)",
            "fatal: repository 'http://h/a.git/' not found",
            "error: failed to push some refs",
        ] {
            assert!(
                !looks_like_auth_failure(line),
                "must not read as an auth failure: {line}"
            );
        }
    }

    fn outcome(stderr: &str) -> Outcome {
        Outcome {
            code: 128,
            stderr: stderr.into(),
        }
    }

    /// The status code is the only server-side semantics a client gets; it must be recovered
    /// from git's own words.
    #[test]
    fn http_status_is_recovered_from_git_stderr() {
        // This is the line `agit push` actually runs into.
        assert_eq!(
            outcome("error: RPC failed; HTTP 422 curl 22 The requested URL returned error: 422")
                .http_status(),
            Some(422)
        );
        assert_eq!(
            outcome("remote: HTTP 401 Unauthorized").http_status(),
            Some(401)
        );
        assert_eq!(
            outcome(
                "fatal: unable to access 'http://h/a.git/': The requested URL returned error: 403"
            )
            .http_status(),
            Some(403)
        );
        // A purely local failure has no status code, and none is invented out of nowhere.
        assert_eq!(
            outcome("! [rejected] main -> main (fetch first)").http_status(),
            None
        );
        // A 2xx is not a failure reason; only 4xx/5xx count.
        assert_eq!(outcome("HTTP 200 OK").http_status(), None);
    }

    #[test]
    fn redact_hides_manually_configured_credentials() {
        assert_eq!(
            redact_url("https://user:tok@h/a.git"),
            "https://***@h/a.git"
        );
        assert_eq!(redact_url("http://h/a.git"), "http://h/a.git");
        // An `@` inside the path is not a credential separator.
        assert_eq!(redact_url("http://h/a/b@c.git"), "http://h/a/b@c.git");
        assert_eq!(
            redact_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }
}

#[cfg(test)]
mod probe_timeout_tests {
    use super::*;

    fn head_oids(out: &str) -> Vec<String> {
        out.lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|oid| !oid.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// A read-only probe must be bounded.
    ///
    /// `GIT_TERMINAL_PROMPT=0` blocks an interactive hang, not a network one: when an address is
    /// blackholed, TCP connect waits for the kernel's connection timeout, observed at
    /// **75 seconds**. And what calls this is `agit scan` / `agit push --dry-run` — a local
    /// operation in the user's eyes.
    ///
    /// This pins "bounded", not a particular number of seconds: the assertion sits far below the
    /// kernel's own timeout and still leaves enough margin not to go flaky on a slow machine.
    #[test]
    fn a_blackholed_remote_does_not_hang_the_probe() {
        let d = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .output()
            .unwrap();
        assert!(out.status.success());

        let t0 = std::time::Instant::now();
        // 10.255.255.1 is not routable: connect waits until our cap cuts it off.
        let got = capture(
            d.path(),
            &["ls-remote", "--heads", "https://10.255.255.1/blackhole.git"],
            None,
        );
        let took = t0.elapsed();

        assert!(
            got.is_none(),
            "no answer is \"unknown\", not \"nothing there\""
        );
        assert!(
            took < PROBE_TIMEOUT * 3,
            "an unbounded probe waits out the kernel timeout: took {took:?}, cap {PROBE_TIMEOUT:?}"
        );
    }

    /// Enough branches to fill the pipe, and the probe still has to come back with the
    /// **complete** answer.
    ///
    /// # This is not an extreme shape
    ///
    /// Every session line is one `refs/heads/*`, so "a repo of a thousand turns has a thousand
    /// refs" is the normal case for this product ([`ls_remote_refs`]'s own doc discusses it).
    /// The advertisement for 1200 branches is about 107 KB while a pipe holds on the order of
    /// 64 KiB — **enough of them and it fills every time**.
    ///
    /// # What it pins is "somebody is draining the pipe"
    ///
    /// With only `try_wait` in the timeout loop and nobody reading the pipes, git fills one,
    /// blocks forever in `write()` and never exits, `try_wait` answers `Ok(None)` forever, and
    /// the timeout cuts it down — the probe degrades to
    /// [`Destination::Unknown`](crate::domain::secrets::Destination), the scan surface goes back
    /// to full forever, and a repo with not a single secret, only many branches, first stalls
    /// out [`PROBE_TIMEOUT`] and is then stopped by the budget.
    ///
    /// So both halves are asserted: **the answer is complete** (the branch count adds up, not a
    /// truncated half), and **the time taken is far below the cap** (it did not come back
    /// because the timeout ended it).
    #[test]
    fn a_large_advertisement_still_fits_through_the_probe() {
        // Past 1200 the advertisement reliably exceeds the pipe buffer; this leaves margin.
        const BRANCHES: usize = 1300;

        let work = tempfile::tempdir().unwrap();
        let bare = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                // Identity and signing go through the environment: this test must not depend
                // on the global git config of whoever runs it.
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(work.path(), &["init", "-q"]);
        std::fs::write(work.path().join("a.txt"), "hello").unwrap();
        git(work.path(), &["add", "a.txt"]);
        git(
            work.path(),
            &["commit", "-q", "--no-gpg-sign", "-m", "base"],
        );
        let head = git(work.path(), &["rev-parse", "HEAD"]);

        let bare_s = bare.path().to_string_lossy().to_string();
        git(
            work.path(),
            &["clone", "-q", "--bare", ".", bare_s.as_str()],
        );
        // `update-ref --stdin` creates them all in one pass: one `git branch` each would be
        // 1300 processes. stdin comes from a **file**, not a pipe — this test must not contain
        // the deadlock it exists to catch.
        let script = work.path().join("refs.txt");
        let mut lines = String::new();
        for i in 0..BRANCHES {
            lines.push_str(&format!(
                "create refs/heads/session/agent-run-{i:06}-branch {head}\n"
            ));
        }
        std::fs::write(&script, &lines).unwrap();
        let out = std::process::Command::new("git")
            .args(["-C", bare_s.as_str(), "update-ref", "--stdin"])
            .stdin(std::process::Stdio::from(
                std::fs::File::open(&script).unwrap(),
            ))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "update-ref: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let t0 = std::time::Instant::now();
        let got = capture(work.path(), &["ls-remote", "--heads", &bare_s], None)
            .map(|out| head_oids(&out));
        let took = t0.elapsed();

        let got = got.expect("no answer means the probe blocked on its own pipe");
        assert!(
            got.len() >= BRANCHES,
            "the advertisement is truncated: got {}, the destination has at least {BRANCHES}",
            got.len()
        );
        assert!(
            took < PROBE_TIMEOUT / 2,
            "local probe took {took:?}, cap {PROBE_TIMEOUT:?} — that is the timeout, not an answer"
        );
    }
}

#[cfg(test)]
mod git_credential_lifecycle_tests {
    use super::{capture, run_for_identity};
    use crate::hub::identity::RemoteIdentity;
    use crate::infra::{config, credentials};
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const AGENT_ID: &str = "00000000-0000-0000-0000-000000000001";
    const FAKE_OID: &str = "1111111111111111111111111111111111111111";
    const GIT_PATH: &str = "/alice/example.git/info/refs?service=git-upload-pack";

    fn persistent_git_configuration() -> std::path::PathBuf {
        static CONFIGURATIONS: std::sync::OnceLock<std::sync::Mutex<Vec<tempfile::TempDir>>> =
            std::sync::OnceLock::new();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.gitconfig");
        std::fs::write(&path, b"").unwrap();
        // Other Git children can inherit this path without holding the fixture's environment lock.
        CONFIGURATIONS
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(directory);
        path
    }

    struct IsolatedHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        home: tempfile::TempDir,
        previous: Vec<(String, Option<OsString>)>,
    }

    impl IsolatedHome {
        fn new() -> Self {
            let lock = config::env_lock();
            let home = tempfile::tempdir().unwrap();
            let empty_config = persistent_git_configuration();
            let mut settings: Vec<(String, Option<OsString>)> = [
                "AGIT_HUB_URL",
                "GIT_CONFIG",
                "GIT_CONFIG_PARAMETERS",
                "GIT_DIR",
                "GIT_COMMON_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_NAMESPACE",
                "GIT_CEILING_DIRECTORIES",
                "GIT_ASKPASS",
                "SSH_ASKPASS",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ]
            .into_iter()
            .map(|name| (name.to_string(), None))
            .collect();
            settings.extend([
                ("AGIT_HOME".into(), Some(home.path().as_os_str().into())),
                ("GIT_CONFIG_COUNT".into(), Some("0".into())),
                ("GIT_CONFIG_NOSYSTEM".into(), Some("1".into())),
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    Some(empty_config.as_os_str().into()),
                ),
                (
                    "GIT_CONFIG_SYSTEM".into(),
                    Some(empty_config.as_os_str().into()),
                ),
                ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
                ("GCM_INTERACTIVE".into(), Some("never".into())),
                ("NO_PROXY".into(), Some("*".into())),
                ("no_proxy".into(), Some("*".into())),
            ]);
            settings.extend(std::env::vars_os().filter_map(|(name, _)| {
                name.to_str()
                    .filter(|name| name.starts_with("GIT_TRACE"))
                    .map(|name| (name.to_string(), None))
            }));
            let previous = settings
                .iter()
                .map(|(name, _)| (name.clone(), std::env::var_os(name)))
                .collect();
            for (name, value) in settings {
                // The shared environment lock outlives the Git children and server threads.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
            Self {
                _lock: lock,
                home,
                previous,
            }
        }

        fn workspace(&self) -> &Path {
            self.home.path()
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            for (name, value) in &self.previous {
                // Restoration remains inside the shared environment lock.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn inherited_git_configuration_outlives_the_fixture() {
        let inherited = {
            let _fixture = IsolatedHome::new();
            std::env::vars_os().collect::<Vec<_>>()
        };
        for name in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
            let (_, path) = inherited.iter().find(|(key, _)| key == name).unwrap();
            std::fs::read(path).expect(
                "an inherited Git configuration must remain readable after fixture teardown",
            );
        }
        let output = std::process::Command::new("git")
            .args(["config", "--global", "--list"])
            .env_clear()
            .envs(inherited)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "an inherited Git configuration must remain readable after fixture teardown: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[derive(Clone, Debug)]
    struct WireRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl WireRequest {
        fn header(&self, name: &str) -> Option<&str> {
            let values: Vec<_> = self
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
                .collect();
            assert!(values.len() <= 1, "the request header must be unambiguous");
            values.first().copied()
        }
    }

    struct Reply {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    }

    fn denied() -> Reply {
        Reply {
            status: 401,
            content_type: "application/json",
            headers: Vec::new(),
            body: br#"{"error":"expired","kind":"unauthorized"}"#.to_vec(),
        }
    }

    fn advertisement() -> Reply {
        let mut body = Vec::new();
        let mut packet = |line: &str| {
            body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
            body.extend_from_slice(line.as_bytes());
        };
        packet("# service=git-upload-pack\n");
        body.extend_from_slice(b"0000");
        let line = format!("{FAKE_OID} refs/heads/main\0symref=HEAD:refs/heads/main\n");
        body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
        body.extend_from_slice(line.as_bytes());
        body.extend_from_slice(b"0000");
        Reply {
            status: 200,
            content_type: "application/x-git-upload-pack-advertisement",
            headers: Vec::new(),
            body,
        }
    }

    fn refreshed() -> Reply {
        Reply {
            status: 200,
            content_type: "application/json",
            headers: Vec::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "access_token": "fake-alice-fresh-access",
                "access_expires_at": "2099-01-01T00:00:00Z",
                "refresh_token": "fake-alice-fresh-refresh",
                "refresh_expires_at": "2099-02-01T00:00:00Z",
            }))
            .unwrap(),
        }
    }

    fn read_request(stream: &mut TcpStream) -> std::io::Result<WireRequest> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut bytes = Vec::new();
        let header_end = loop {
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
            if bytes.len() >= 65536 {
                return Err(std::io::Error::other(
                    "fixture request headers exceed the limit",
                ));
            }
            let mut buffer = [0; 4096];
            let len = stream.read(&mut buffer)?;
            if len == 0 {
                return Err(std::io::Error::other(
                    "fixture request headers are incomplete",
                ));
            }
            bytes.extend_from_slice(&buffer[..len]);
        };
        let header_text = String::from_utf8_lossy(&bytes[..header_end]);
        let mut lines = header_text.lines();
        let mut first = lines
            .next()
            .ok_or_else(|| std::io::Error::other("fixture request line is absent"))?
            .split_whitespace();
        let method = first.next().unwrap_or_default().to_string();
        let path = first.next().unwrap_or_default().to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_string(), value.trim().to_string()))
            .collect();
        let body_len = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.parse::<usize>())
            .transpose()
            .map_err(|_| std::io::Error::other("fixture request length is invalid"))?
            .unwrap_or(0);
        if body_len > 65536 {
            return Err(std::io::Error::other(
                "fixture request body exceeds the limit",
            ));
        }
        while bytes.len() < header_end + body_len {
            let mut buffer = [0; 4096];
            let len = stream.read(&mut buffer)?;
            if len == 0 {
                return Err(std::io::Error::other("fixture request body is incomplete"));
            }
            bytes.extend_from_slice(&buffer[..len]);
        }
        Ok(WireRequest {
            method,
            path,
            headers,
            body: bytes[header_end..header_end + body_len].to_vec(),
        })
    }

    struct FakeHub {
        base: String,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<std::io::Result<Vec<WireRequest>>>>,
    }

    impl FakeHub {
        fn new(mut respond: impl FnMut(&WireRequest) -> Reply + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut requests = Vec::new();
                while !stopped.load(Ordering::SeqCst) && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false)?;
                            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                            let request = read_request(&mut stream)?;
                            let response = respond(&request);
                            requests.push(request);
                            let reason = if response.status == 200 {
                                "OK"
                            } else {
                                "Unauthorized"
                            };
                            write!(
                                stream,
                                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                                response.status,
                                reason,
                                response.content_type,
                                response.body.len()
                            )?;
                            for (name, value) in response.headers {
                                write!(stream, "{name}: {value}\r\n")?;
                            }
                            stream.write_all(b"\r\n")?;
                            stream.write_all(&response.body)?;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(requests)
            });
            Self {
                base,
                stop,
                thread: Some(thread),
            }
        }

        fn finish(mut self) -> Vec<WireRequest> {
            self.stop.store(true, Ordering::SeqCst);
            self.thread
                .take()
                .unwrap()
                .join()
                .expect("the fixture server must not panic")
                .expect("the fixture server must handle complete requests")
        }
    }

    impl Drop for FakeHub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn pair(hub: &str, username: &str) -> credentials::HubCredential {
        credentials::HubCredential {
            username: username.into(),
            email: None,
            hub: Some(hub.into()),
            access_token: format!("fake-{username}-access"),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: format!("fake-{username}-refresh"),
            refresh_expires_at: "2099-02-01T00:00:00Z".into(),
        }
    }

    #[derive(Clone, Copy)]
    enum EntryPoint {
        Run,
        Capture,
    }

    fn execute(entry: EntryPoint, dir: &Path, url: &str, identity: &RemoteIdentity) -> bool {
        let args = [
            "-c",
            "credential.helper=",
            "-c",
            "http.proxy=",
            "-c",
            "http.lowSpeedLimit=1",
            "-c",
            "http.lowSpeedTime=3",
            "-c",
            "protocol.version=0",
            "ls-remote",
            url,
        ];
        match entry {
            EntryPoint::Run => run_for_identity(Some(dir), &args, identity)
                .expect("the Git subprocess must start")
                .ok(),
            EntryPoint::Capture => {
                let output = capture(dir, &args, Some(identity));
                if let Some(output) = &output {
                    assert_eq!(output, &format!("{FAKE_OID}\trefs/heads/main\n"));
                }
                output.is_some()
            }
        }
    }

    fn assert_git_request(request: &WireRequest, token: Option<&str>, identity: bool) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, GIT_PATH);
        assert_eq!(
            request.header("Authorization"),
            token.map(|token| format!("Bearer {token}")).as_deref()
        );
        assert_eq!(
            request.header("X-AgentGit-Expected-Agent-Id"),
            identity.then_some(AGENT_ID)
        );
        assert!(request.body.is_empty());
    }

    fn hub_change_during_git_keeps_the_captured_identity(entry: EntryPoint) {
        let home = IsolatedHome::new();
        let other = FakeHub::new(|_| denied());
        let other_base = other.base.clone();
        let mut git_requests = 0;
        let hub = FakeHub::new(move |request| {
            if request.path == GIT_PATH {
                git_requests += 1;
                if git_requests == 1 {
                    config::set_global("hub.url", Some(&other_base)).unwrap();
                    return denied();
                }
                return advertisement();
            }
            assert_eq!(request.path, "/api/auth/refresh");
            refreshed()
        });
        config::set_global("hub.url", Some(&hub.base)).unwrap();
        credentials::save(&hub.base, &pair(&hub.base, "alice")).unwrap();
        credentials::save(&other.base, &pair(&other.base, "bob")).unwrap();
        let other_path = config::credentials_path(&other.base).unwrap();
        let other_before = std::fs::read(&other_path).unwrap();
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        let url = format!("{}/alice/example.git", hub.base);
        assert!(execute(entry, home.workspace(), &url, &identity));
        assert_eq!(config::hub_url(), other.base);
        let saved = credentials::load_checked(&hub.base).unwrap().unwrap();
        assert_eq!(saved.username, "alice");
        assert_eq!(saved.access_token, "fake-alice-fresh-access");
        assert_eq!(saved.refresh_token, "fake-alice-fresh-refresh");
        assert_eq!(std::fs::read(&other_path).unwrap(), other_before);
        let requests = hub.finish();
        assert_eq!(requests.len(), 3);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/auth/refresh");
        assert_eq!(requests[1].header("Authorization"), None);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            serde_json::json!({ "refresh_token": "fake-alice-refresh" })
        );
        assert_git_request(&requests[2], Some("fake-alice-fresh-access"), true);
        assert!(other.finish().is_empty());
    }

    fn account_change_during_git_refuses_refresh_and_retry(entry: EntryPoint) {
        let home = IsolatedHome::new();
        let saved_by_login = Arc::new(std::sync::Mutex::new(None));
        let login_snapshot = saved_by_login.clone();
        let hub = FakeHub::new(move |request| {
            assert_eq!(request.path, GIT_PATH);
            let base = format!("http://{}", request.header("host").unwrap());
            credentials::save(&base, &pair(&base, "bob")).unwrap();
            *login_snapshot.lock().unwrap() =
                Some(std::fs::read(config::credentials_path(&base).unwrap()).unwrap());
            denied()
        });
        config::set_global("hub.url", Some(&hub.base)).unwrap();
        credentials::save(&hub.base, &pair(&hub.base, "alice")).unwrap();
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        let url = format!("{}/alice/example.git", hub.base);
        assert!(!execute(entry, home.workspace(), &url, &identity));
        let saved = credentials::load_checked(&hub.base).unwrap().unwrap();
        assert_eq!(saved.username, "bob");
        assert_eq!(saved.access_token, "fake-bob-access");
        assert_eq!(saved.refresh_token, "fake-bob-refresh");
        assert_eq!(
            std::fs::read(config::credentials_path(&hub.base).unwrap()).unwrap(),
            saved_by_login.lock().unwrap().as_ref().unwrap().clone()
        );
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
    }

    fn sibling_refresh_during_git_keeps_the_account_and_skips_another_exchange(entry: EntryPoint) {
        let home = IsolatedHome::new();
        let mut git_requests = 0;
        let hub = FakeHub::new(move |request| {
            assert_eq!(request.path, GIT_PATH);
            git_requests += 1;
            if git_requests == 1 {
                let base = format!("http://{}", request.header("host").unwrap());
                let mut fresh = pair(&base, "alice");
                fresh.access_token = "fake-alice-sibling-access".into();
                fresh.refresh_token = "fake-alice-sibling-refresh".into();
                credentials::save(&base, &fresh).unwrap();
                return denied();
            }
            advertisement()
        });
        config::set_global("hub.url", Some(&hub.base)).unwrap();
        credentials::save(&hub.base, &pair(&hub.base, "alice")).unwrap();
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        let url = format!("{}/alice/example.git", hub.base);
        assert!(execute(entry, home.workspace(), &url, &identity));
        let saved = credentials::load_checked(&hub.base).unwrap().unwrap();
        assert_eq!(saved.username, "alice");
        assert_eq!(saved.access_token, "fake-alice-sibling-access");
        assert_eq!(saved.refresh_token, "fake-alice-sibling-refresh");
        let requests = hub.finish();
        assert_eq!(requests.len(), 2);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
        assert_git_request(&requests[1], Some("fake-alice-sibling-access"), true);
    }

    #[test]
    fn streaming_git_adopts_a_siblings_rotation_for_the_same_account() {
        sibling_refresh_during_git_keeps_the_account_and_skips_another_exchange(EntryPoint::Run);
    }

    #[test]
    fn captured_git_adopts_a_siblings_rotation_for_the_same_account() {
        sibling_refresh_during_git_keeps_the_account_and_skips_another_exchange(
            EntryPoint::Capture,
        );
    }

    #[test]
    fn streaming_git_keeps_its_hub_after_persistent_selection_changes() {
        hub_change_during_git_keeps_the_captured_identity(EntryPoint::Run);
    }

    #[test]
    fn captured_git_keeps_its_hub_after_persistent_selection_changes() {
        hub_change_during_git_keeps_the_captured_identity(EntryPoint::Capture);
    }

    #[test]
    fn streaming_git_cannot_retry_as_a_concurrent_login() {
        account_change_during_git_refuses_refresh_and_retry(EntryPoint::Run);
    }

    #[test]
    fn captured_git_cannot_retry_as_a_concurrent_login() {
        account_change_during_git_refuses_refresh_and_retry(EntryPoint::Capture);
    }

    #[test]
    fn capture_without_identity_ignores_corrupt_saved_credentials() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        config::set_global("hub.url", Some(&hub.base)).unwrap();
        let path = config::credentials_path(&hub.base).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let malformed = b"invalid fake credential record";
        std::fs::write(&path, malformed).unwrap();
        let url = format!("{}/alice/example.git", hub.base);
        let output = capture(
            home.workspace(),
            &[
                "-c",
                "credential.helper=",
                "-c",
                "http.proxy=",
                "-c",
                "http.lowSpeedLimit=1",
                "-c",
                "http.lowSpeedTime=3",
                "-c",
                "protocol.version=0",
                "ls-remote",
                &url,
            ],
            None,
        );
        assert_eq!(output, Some(format!("{FAKE_OID}\trefs/heads/main\n")));
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_git_request(&requests[0], None, false);
    }

    fn pinned_repo(home: &IsolatedHome, hub: &str) -> crate::domain::repo::Repo {
        let repo = crate::domain::repo::Repo::init(&home.workspace().join("repo")).unwrap();
        let identity = RemoteIdentity::new(hub, AGENT_ID).unwrap();
        crate::hub::identity::pin(&repo, &identity).unwrap();
        repo.set_remote(&format!("{hub}/alice/example.git"))
            .unwrap();
        config::set_global("hub.url", Some(hub)).unwrap();
        credentials::save(hub, &pair(hub, "alice")).unwrap();
        repo
    }

    #[test]
    fn explicit_foreign_http_destinations_never_start_a_transfer() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        let other = FakeHub::new(|_| advertisement());
        let repo = pinned_repo(&home, &hub.base);
        let url = format!("{}/alice/example.git", other.base);
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        assert!(super::ls_remote_refs(repo.root(), &url, false).is_none());
        let destination = home.workspace().join("missing/clone");
        assert!(super::clone(&url, &destination, &identity).is_err());
        assert!(!destination.parent().unwrap().exists());
        assert!(hub.finish().is_empty());
        assert!(other.finish().is_empty());
    }

    #[test]
    fn rewritten_http_and_push_urls_must_match_the_pinned_hub() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        let other = FakeHub::new(|_| advertisement());
        let repo = pinned_repo(&home, &hub.base);
        let url = format!("{}/alice/example.git", hub.base);
        let foreign = format!("{}/alice/example.git", other.base);
        repo.git(&["config", "remote.origin.pushurl", &foreign])
            .unwrap();
        assert!(super::run(&repo, &["push", "origin", "main"]).is_err());
        repo.git(&[
            "config",
            "--global",
            &format!("url.{}/.insteadOf", other.base),
            &format!("{}/", hub.base),
        ])
        .unwrap();
        assert!(super::ls_remote_refs(repo.root(), &url, false).is_none());
        assert!(super::run(&repo, &["fetch", "origin"]).is_err());
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        assert!(super::clone(&url, &home.workspace().join("clone"), &identity).is_err());
        assert!(hub.finish().is_empty());
        assert!(other.finish().is_empty());
    }

    #[test]
    fn repository_scoped_headers_override_inherited_headers_without_duplicates() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        let repo = pinned_repo(&home, &hub.base);
        let url = format!("{}/alice/example.git", hub.base);
        let key = format!("http.{url}.extraHeader");
        for value in [
            "Authorization: Bearer inherited",
            "X-AgentGit-Expected-Agent-Id: inherited",
        ] {
            repo.git(&["config", "--global", "--add", &key, value])
                .unwrap();
            repo.git(&["config", "--local", "--add", &key, value])
                .unwrap();
        }
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        assert!(execute(EntryPoint::Capture, repo.root(), &url, &identity));
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
    }

    #[test]
    fn repository_scoped_redirect_refusal_overrides_inherited_redirects() {
        let home = IsolatedHome::new();
        let other = FakeHub::new(|_| advertisement());
        let destination = format!(
            "{}/alice/example.git/info/refs?service=git-upload-pack",
            other.base
        );
        let hub = FakeHub::new(move |_| Reply {
            status: 302,
            content_type: "text/plain",
            body: Vec::new(),
            headers: vec![("Location".into(), destination.clone())],
        });
        let repo = pinned_repo(&home, &hub.base);
        let url = format!("{}/alice/example.git", hub.base);
        let key = format!("http.{url}.followRedirects");
        repo.git(&["config", "--global", &key, "true"]).unwrap();
        repo.git(&["config", "--local", &key, "true"]).unwrap();
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        assert!(!execute(EntryPoint::Capture, repo.root(), &url, &identity));
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
        assert!(other.finish().is_empty());
    }

    #[test]
    fn local_clones_do_not_load_or_refresh_hub_credentials() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| denied());
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        let path = config::credentials_path(&hub.base).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"invalid saved identity").unwrap();
        let source = crate::domain::repo::Repo::init(&home.workspace().join("source")).unwrap();
        let destination = home.workspace().join("clone");
        let result =
            super::clone(source.root().to_str().unwrap(), &destination, &identity).unwrap();
        assert!(result.ok());
        let cloned = crate::domain::repo::Repo::at(&destination);
        assert_eq!(crate::hub::identity::read(&cloned).unwrap(), Some(identity));
        assert_eq!(std::fs::read(path).unwrap(), b"invalid saved identity");
        assert!(hub.finish().is_empty());
    }

    fn inherited_parameters(settings: &[(&str, &str)]) -> OsString {
        settings
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

    fn configuration_values(
        home: &IsolatedHome,
        inherited: &std::ffi::OsStr,
        token: &str,
        url: &str,
        key: &str,
    ) -> std::process::Output {
        std::process::Command::new("git")
            .args(["config", "--null", "--get-all", key])
            .current_dir(home.workspace())
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "fixture.keep")
            .env("GIT_CONFIG_VALUE_0", "from-count")
            .env("GIT_CONFIG_KEY_1", "fixture.countonly")
            .env("GIT_CONFIG_VALUE_1", "count-only")
            .envs(super::transport_env_after(
                Some(inherited),
                Some(token),
                AGENT_ID,
                &[url.into()],
            ))
            .output()
            .unwrap()
    }

    #[test]
    fn git_parameters_preserve_count_and_roundtrip_quoted_keys_and_values() {
        let home = IsolatedHome::new();
        let url = "https://hub.example.test/mount=one's!/alice/example.git";
        let key = format!("http.{url}.extraHeader");
        let mut inherited = OsString::from("'fixture.keep=legacy=value' ");
        inherited.push(inherited_parameters(&[
            ("fixture.keep", "modern's ! value"),
            ("fixture.empty", ""),
        ]));
        for token in [
            "",
            "plain",
            "apostrophe'and!bang",
            "space tab\tline\nslash\\",
            r#"equals=quote"dollar$backtick`"#,
            "nonascii-ä-λ",
        ] {
            let output = configuration_values(&home, &inherited, token, url, &key);
            assert!(
                output.status.success(),
                "Git must parse each quoted key and value"
            );
            let expected = format!(
                "\0Authorization: Bearer {token}\0X-AgentGit-Expected-Agent-Id: {AGENT_ID}\0"
            );
            assert_eq!(output.stdout, expected.as_bytes());
            assert_eq!(
                configuration_values(&home, &inherited, token, url, "fixture.keep").stdout,
                b"from-count\0legacy=value\0modern's ! value\0"
            );
            assert_eq!(
                configuration_values(&home, &inherited, token, url, "fixture.countonly").stdout,
                b"count-only\0"
            );
            assert_eq!(
                configuration_values(&home, &inherited, token, url, "fixture.empty").stdout,
                b"\0"
            );
            let redirect = format!("http.{url}.followRedirects");
            assert_eq!(
                configuration_values(&home, &inherited, token, url, &redirect).stdout,
                b"false\0"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn inherited_git_parameter_bytes_are_not_lossily_reencoded() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let home = IsolatedHome::new();
        let inherited = OsString::from_vec(b"'fixture.raw'='opaque-\xff-\xfe' ".to_vec());
        let url = "https://hub.example.test/alice/example.git";
        let environment = super::transport_env_after(
            Some(&inherited),
            Some("synthetic"),
            AGENT_ID,
            &[url.into()],
        );
        assert!(
            environment[0]
                .1
                .as_bytes()
                .starts_with(inherited.as_bytes())
        );
        let output = configuration_values(&home, &inherited, "synthetic", url, "fixture.raw");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"opaque-\xff-\xfe\0");
    }

    #[test]
    fn inherited_parameters_cannot_reenable_same_origin_redirects_or_headers() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|request| {
            if request.path == GIT_PATH {
                Reply {
                    status: 302,
                    content_type: "text/plain",
                    body: Vec::new(),
                    headers: vec![(
                        "Location".into(),
                        "/outside/example.git/info/refs?service=git-upload-pack".into(),
                    )],
                }
            } else {
                advertisement()
            }
        });
        let repo = pinned_repo(&home, &hub.base);
        let url = format!("{}/alice/example.git", hub.base);
        let redirect = format!("http.{url}.followRedirects");
        let headers = format!("http.{url}.extraHeader");
        let mut parameters = OsString::from(format!("'{redirect}=true' "));
        parameters.push(inherited_parameters(&[
            (&headers, "X-Inherited: must-not-escape"),
            (&headers, "X-AgentGit-Expected-Agent-Id: inherited"),
            ("http.userAgent", "preserved inherited agent"),
        ]));
        // The fixture's environment lock and restoration cover the inherited parameter value.
        unsafe { std::env::set_var("GIT_CONFIG_PARAMETERS", parameters) };
        let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        assert!(!execute(EntryPoint::Capture, repo.root(), &url, &identity));
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
        assert_eq!(requests[0].header("X-Inherited"), None);
        assert_eq!(
            requests[0].header("User-Agent"),
            Some("preserved inherited agent")
        );
    }

    #[test]
    fn inherited_parameter_rewrites_and_quoted_mounts_keep_their_wire_target() {
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        let mount = format!("{}/mount=one's!", hub.base);
        let identity = RemoteIdentity::new(&mount, AGENT_ID).unwrap();
        credentials::save(&mount, &pair(&mount, "alice")).unwrap();
        let requested = format!("{mount}/alice/requested.git");
        let effective = format!("{mount}/alice/example.git");
        let rewrite = format!("url.{effective}.insteadOf");
        let headers = format!("http.{effective}.extraHeader");
        let redirect = format!("http.{effective}.followRedirects");
        let parameters = inherited_parameters(&[
            (&rewrite, &requested),
            (&headers, "X-Inherited: must-not-escape"),
            (&redirect, "true"),
            ("http.userAgent", "preserved inherited agent"),
        ]);
        // URL expansion and transport observe the same inherited non-security configuration.
        unsafe { std::env::set_var("GIT_CONFIG_PARAMETERS", parameters) };
        assert!(execute(
            EntryPoint::Capture,
            home.workspace(),
            &requested,
            &identity
        ));
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].path,
            "/mount=one's!/alice/example.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(
            requests[0].header("Authorization"),
            Some("Bearer fake-alice-access")
        );
        assert_eq!(
            requests[0].header("X-AgentGit-Expected-Agent-Id"),
            Some(AGENT_ID)
        );
        assert_eq!(requests[0].header("X-Inherited"), None);
        assert_eq!(
            requests[0].header("User-Agent"),
            Some("preserved inherited agent")
        );
    }
}
