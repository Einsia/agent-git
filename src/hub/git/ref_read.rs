//! Read-only probes retain a classified failure without exposing native diagnostic secrets.

use super::*;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(super) struct Failure {
    message: String,
    pub transient: bool,
}

impl Failure {
    fn new(message: impl Into<String>, transient: bool) -> Self {
        Self {
            message: message.into(),
            transient,
        }
    }

    fn diagnostic(stderr: &str) -> Self {
        let status = Outcome {
            code: 1,
            stderr: stderr.into(),
        }
        .http_status();
        if let Some(status) = status {
            return Self::new(
                format!("Git destination returned HTTP {status}"),
                matches!(status, 408 | 429 | 500 | 502 | 503 | 504),
            );
        }
        if looks_like_auth_failure(stderr) {
            return Self::new("Git destination rejected authentication", false);
        }
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("repository") && lower.contains("not found") {
            return Self::new("Git repository was not found or is inaccessible", false);
        }
        let transient = [
            "timed out",
            "timeout",
            "connection reset",
            "connection refused",
            "failed to connect",
            "couldn't connect",
            "could not resolve host",
            "could not resolve proxy",
            "empty reply from server",
            "remote end hung up",
            "connection was closed",
            "recv failure",
            "send failure",
            "early eof",
            "http/2 stream",
            "http2 stream",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
        Self::new(
            if transient {
                "Git destination connection failed temporarily"
            } else {
                "Git destination read failed (check repository access and transport configuration)"
            },
            transient,
        )
    }
}

pub(super) fn capture(
    dir: &Path,
    args: &[&str],
    transport: Option<&TransportIdentity>,
) -> std::result::Result<String, Failure> {
    let once = || -> std::result::Result<(bool, String, String), Failure> {
        let mut cmd = match transport {
            Some(transport) => transport.command(Some(dir)),
            None => {
                let mut command = transport_command();
                command.arg("-C").arg(dir);
                command
            }
        };
        cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
        if let Some(transport) = transport {
            for (key, value) in transport
                .environment_in(transport.execution.as_ref(), args.first() == Some(&"lfs"))
                .map_err(|_| {
                    Failure::new(
                        "Git transport account or configuration is unavailable or changed",
                        false,
                    )
                })?
            {
                cmd.env(key, value);
            }
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| Failure::new("cannot start Git transport", false))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::new("cannot capture Git transport output", false));
        };
        let stdout = drain(stdout);
        let stderr = drain(stderr);
        let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Failure::new(
                        "Git destination read timed out or could not be observed",
                        result.is_ok(),
                    ));
                }
            }
        };
        let stdout = stdout
            .recv_timeout(PROBE_TIMEOUT)
            .map_err(|_| Failure::new("Git destination output timed out", true))?;
        let stderr = stderr
            .recv_timeout(PROBE_TIMEOUT)
            .map_err(|_| Failure::new("Git destination diagnostics timed out", true))?;
        Ok((
            status.success(),
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        ))
    };
    let (ok, stdout, stderr) = once()?;
    if ok {
        return Ok(stdout);
    }
    if looks_like_auth_failure(&stderr)
        && let Some(transport) = transport
    {
        let refreshed = transport.refresh().map_err(|_| {
            Failure::new(
                "Git authentication could not be refreshed for the original account",
                false,
            )
        })?;
        if refreshed {
            let (ok, stdout, stderr) = once()?;
            return if ok {
                Ok(stdout)
            } else {
                Err(Failure::diagnostic(&stderr))
            };
        }
    }
    Err(Failure::diagnostic(&stderr))
}
