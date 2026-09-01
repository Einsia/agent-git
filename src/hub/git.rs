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
//! ```text
//! GIT_CONFIG_COUNT=3
//! GIT_CONFIG_KEY_0=http.extraHeader
//! GIT_CONFIG_VALUE_0=
//! GIT_CONFIG_KEY_1=http.extraHeader
//! GIT_CONFIG_VALUE_1=Authorization: Bearer <access_token>
//! GIT_CONFIG_KEY_2=http.extraHeader
//! GIT_CONFIG_VALUE_2=X-AgentGit-Expected-Agent-Id: <agent_id>
//! ```
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
use crate::infra::credentials;
use anyhow::Context;
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

fn looks_like_auth_failure(stderr: &str) -> bool {
    AUTH_MARKERS.iter().any(|m| stderr.contains(m))
}

/// Environment variables that inject the authentication header.
///
/// Respects a `GIT_CONFIG_COUNT` the caller already set: writing 1 outright silently drops the
/// other entries the user configured through the same mechanism (their KEY_1 is still in the
/// environment, but git no longer reads it).
fn transport_env(token: Option<&str>, expected_agent_id: &str) -> Vec<(String, String)> {
    let existing: usize = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    transport_env_after(existing, token, expected_agent_id)
}

fn transport_env_after(
    existing: usize,
    token: Option<&str>,
    expected_agent_id: &str,
) -> Vec<(String, String)> {
    // One empty value clears every http.extraHeader accumulated from system/global/local
    // config and from the caller's existing environment. Otherwise, when the user has already
    // configured a header of the same name, appending ours makes the server receive two
    // Expected-Agent-Id headers; the hub has to reject that ambiguous request.
    let mut values = vec![String::new()];
    if let Some(token) = token {
        values.push(format!("Authorization: Bearer {token}"));
    }
    values.push(format!(
        "{}: {expected_agent_id}",
        super::identity::EXPECTED_AGENT_ID_HEADER
    ));

    let mut out = vec![(
        "GIT_CONFIG_COUNT".into(),
        (existing + values.len()).to_string(),
    )];
    for (offset, value) in values.into_iter().enumerate() {
        let i = existing + offset;
        out.push((format!("GIT_CONFIG_KEY_{i}"), "http.extraHeader".into()));
        out.push((format!("GIT_CONFIG_VALUE_{i}"), value));
    }
    out
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
    // Exchange first when the expiry is already known locally, saving a round trip certain
    // to 401.
    if credentials::current().is_some_and(|c| c.access_expired()) {
        refresh();
    }

    let (code, stderr) = spawn(dir, args, &identity.agent_id)?;
    if code == 0 || !looks_like_auth_failure(&stderr) {
        return Ok(Outcome { code, stderr });
    }

    // Authentication failed: exchange the token once and try again. When the exchange fails,
    // hand this result back — the caller's hint (`agit login`) is more useful than reporting it
    // again ourselves.
    if !refresh() {
        return Ok(Outcome { code, stderr });
    }
    let (code, stderr) = spawn(dir, args, &identity.agent_id)?;
    Ok(Outcome { code, stderr })
}

/// Ask a remote: **which branch tips do you have right now**.
///
/// `None` means "no answer" (offline, no permission, unreachable address, remote does not
/// exist) — the caller must take it as "unknown", not as "nothing there". The direction is the
/// same either way (both force a full scan), but reading one failure as "the far side already
/// has it" leaves content unscanned, and that is the failure this gate must not have.
///
/// # Why this is needed
///
/// The local `refs/remotes/origin/*` describes **the remote of the last fetch/push**, not the
/// destination of this push. Switching hubs, or a remote deleted and recreated, leaves it
/// unchanged. So "what this push will send" can only be asked of the destination itself. This
/// one round trip is read-only and changes no state, and what comes back is exactly the
/// advertisement `git push` negotiates against.
///
/// `--heads` only: what is wanted here is "how far the far side's history has come", and the
/// commit a tag points at is already reachable from a branch. It also avoids a real shape — an
/// agent makes one tag per version, so a repo of a thousand turns has a thousand tag refs, and
/// fetching them all only moves a list that takes no part in the verdict across the network.
pub fn ls_remote_heads(dir: &Path, url: &str) -> Option<Vec<String>> {
    let repo = Repo::at(dir);
    let identity =
        super::identity::require_current_expected(&repo, &crate::infra::config::hub_url()).ok()?;
    let out = capture(
        dir,
        &["ls-remote", "--heads", url],
        Some(&identity.agent_id),
    )?;
    Some(head_oids(&out))
}

fn head_oids(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|oid| !oid.is_empty())
        .map(str::to_string)
        .collect()
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
/// in this product every session line is one `refs/heads/*` ([`ls_remote_heads`]'s own doc is
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
fn capture(dir: &Path, args: &[&str], expected_agent_id: Option<&str>) -> Option<String> {
    let once = |token: Option<String>| -> Option<(bool, String, String)> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir);
        cmd.args(args);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if let Some(expected_agent_id) = expected_agent_id {
            for (k, v) in transport_env(token.as_deref(), expected_agent_id) {
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
    let (ok, stdout, stderr) = once(credentials::current().map(|c| c.access_token))?;
    if ok {
        return Some(stdout);
    }
    if !looks_like_auth_failure(&stderr) || !refresh() {
        return None;
    }
    let (ok, stdout, _) = once(credentials::current().map(|c| c.access_token))?;
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

/// Clone a repo on the hub, and pin the same remote identity into the new checkout as soon as it
/// succeeds.
pub fn clone(
    url: &str,
    dest: &Path,
    identity: &super::identity::RemoteIdentity,
) -> Result<Outcome> {
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

fn spawn(dir: Option<&Path>, args: &[&str], expected_agent_id: &str) -> Result<(i32, String)> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    let full = with_progress(args, std::io::IsTerminal::is_terminal(&std::io::stderr()));
    cmd.args(&full);
    // Give git no chance to ask for a password interactively: with no usable token it must fail
    // immediately and let us see the authentication marker, instead of hanging on input in a
    // non-interactive environment.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let token = credentials::current().map(|c| c.access_token);
    for (k, v) in transport_env(token.as_deref(), expected_agent_id) {
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

/// Exchange the refresh token for a new access token.
fn refresh() -> bool {
    super::Client::from_env().refresh_access()
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
    fn transport_env_resets_inherited_headers_then_adds_one_identity() {
        let e = transport_env_after(2, Some("tok123"), "00000000-0000-0000-0000-000000000001");
        let get = |k: &str| {
            e.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("GIT_CONFIG_COUNT"), "5");
        assert_eq!(get("GIT_CONFIG_KEY_2"), "http.extraHeader");
        assert_eq!(get("GIT_CONFIG_VALUE_2"), "");
        assert_eq!(get("GIT_CONFIG_VALUE_3"), "Authorization: Bearer tok123");
        assert_eq!(
            get("GIT_CONFIG_VALUE_4"),
            "X-AgentGit-Expected-Agent-Id: 00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(
            e.iter()
                .filter(|(_, v)| v.starts_with("X-AgentGit-Expected-Agent-Id:"))
                .count(),
            1
        );
    }

    #[test]
    fn token_never_appears_in_a_url_or_argv() {
        // The whole reason this module exists: the token lives only in an environment variable.
        let e = transport_env_after(0, Some("s3cret"), "00000000-0000-0000-0000-000000000001");
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
    /// refs" is the normal case for this product ([`ls_remote_heads`]'s own doc discusses it).
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
