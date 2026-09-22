//! Owner-authenticated local RPC and the byte-preserving SSH bridge.
//!
//! This listener is never enabled by a Hub daemon. Peer credentials establish
//! authority; wire caller claims cannot widen it. Disconnecting a viewer leaves
//! supervised processes and their durable start receipts in the daemon.

#[cfg(unix)]
use anyhow::Context;
use anyhow::ensure;
use clap::{Args as ClapArgs, Subcommand};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(windows)]
#[path = "local_windows.rs"]
mod windows;
#[cfg(windows)]
use windows::bridge;
#[cfg(windows)]
pub(super) fn wait_ready() -> crate::Result<()> {
    windows::wait_ready_sync()
}
#[cfg(windows)]
pub(super) use windows::{Listener, Stream, authenticate_client, listen};
#[cfg(unix)]
pub(super) type Listener = tokio::net::UnixListener;
#[cfg(unix)]
pub(super) type Stream = tokio::net::UnixStream;

#[cfg(unix)]
pub(super) fn authenticate_client(socket: &Stream) -> std::io::Result<()> {
    if socket.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Local RPC client belongs to another user",
        ));
    }
    Ok(())
}

pub use super::endpoint::{MAX_FRAME, WORKSPACE};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Read Hub catalog data using this device's existing Agit credentials.
    Catalog,
    /// Start the independent local owner daemon.
    Start {
        #[arg(long)]
        detach: bool,
    },
    /// Bridge stdin/stdout to local RPC; suitable for a persistent SSH channel.
    Bridge {
        #[arg(long)]
        ensure: bool,
        /// Require an additional RPC feature before forwarding any client bytes.
        #[arg(long = "require-feature")]
        required_features: Vec<String>,
        /// Require this bridge's exact build, including same-version replacements.
        #[arg(long)]
        require_current_build: bool,
    },
    /// Inspect the local daemon.
    Status,
    /// Stop the local daemon and its supervised sessions.
    Stop,
    /// Replace the running local daemon only when no user work would be interrupted.
    Restart {
        #[arg(long, required = true)]
        if_idle: bool,
    },
    /// Recover Unix control socket state without starting a daemon.
    Recover {
        /// Confirm all daemons and starters in this local namespace have exited, including in containers.
        #[arg(long, required = true)]
        confirm_stopped: bool,
    },
    #[command(hide = true)]
    AfterUpgrade {
        #[arg(long)]
        target: String,
    },
}

pub fn run(args: Args) -> crate::commands::CmdResult {
    super::select_local_authority();
    match args.action {
        Action::Catalog => {
            use std::io::Read;
            let mut input = String::new();
            std::io::stdin().take(65537).read_to_string(&mut input)?;
            ensure!(input.len() <= 65536, "Catalog request is too large");
            let request = serde_json::from_str(&input)?;
            let client = crate::hub::Client::from_env();
            let result = match client.catalog_read(request) {
                Ok(value) => serde_json::json!({"ok":true,"value":value}),
                Err(error) => serde_json::json!({"ok":false,"error":error.to_string()}),
            };
            println!("{}", serde_json::to_string(&result)?);
        }
        Action::Start { detach } => {
            if detach {
                ensure_daemon()?;
            } else {
                start_foreground()?;
            }
        }
        Action::Bridge {
            ensure,
            required_features,
            require_current_build,
        } => {
            super::lifecycle::attach(ensure, &required_features, require_current_build)?;
            bridge()?;
        }
        Action::Restart { if_idle: _ } => {
            let outcome = super::lifecycle::restart_if_idle()?;
            super::lifecycle::report(&outcome)?;
            println!("{}", serde_json::to_string(&outcome)?);
            if matches!(outcome, super::lifecycle::Outcome::Deferred { .. }) {
                return Ok(crate::ExitCode::Precondition);
            }
        }
        Action::Recover { confirm_stopped: _ } => {
            let removed = super::lifecycle::recover_stopped()?;
            println!(
                "{}",
                serde_json::json!({"status": "recovered", "socket_removed": removed})
            );
        }
        Action::AfterUpgrade { target } => {
            let outcome = super::lifecycle::after_upgrade(serde_json::from_str(&target)?)?;
            println!("{}", serde_json::to_string(&outcome)?);
        }
        Action::Status => {
            use super::control::{self, Presence};
            let reply = control::ask(&control::Request::Status).map_err(|error| {
                match control::presence() {
                    Presence::Absent => anyhow::anyhow!("no local daemon is running"),
                    Presence::Running(pid) => anyhow::anyhow!(
                        "local daemon (pid {pid}) did not answer the status request: {error}"
                    ),
                    Presence::Unclear(why) => anyhow::anyhow!(
                        "cannot establish local daemon state: {why}; status request failed: {error}"
                    ),
                }
            })?;
            println!("{}", serde_json::to_string(&reply)?);
        }
        Action::Stop => println!(
            "{}",
            serde_json::to_string(&super::lifecycle::stop_and_wait()?)?
        ),
    }
    Ok(crate::ExitCode::Ok)
}

fn rpc_path() -> crate::Result<PathBuf> {
    Ok(super::control::socket_path()?.with_extension("rpc"))
}

pub fn start_foreground() -> crate::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(super::daemon::Daemon::run(super::daemon::Options {
        local_owner: true,
        hub: "local-owner".into(),
    }))
}

pub fn ensure_daemon() -> crate::Result<()> {
    super::lifecycle::attach(true, &[], false)
}

#[cfg(unix)]
pub(super) fn wait_ready() -> crate::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(socket) =
            super::control::connect_within(&rpc_path()?, std::time::Duration::from_secs(1))
        {
            authenticate_server(socket, unsafe { libc::geteuid() })?;
            return Ok(());
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "agitd did not become ready; inspect agitd-*.log in {}",
            super::rc_dir()?.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

pub(super) fn spawn_daemon() -> crate::Result<()> {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    let log = tempfile::Builder::new()
        .prefix("agitd-")
        .suffix(".log")
        .tempfile_in(super::rc_dir()?)?;
    let (log, path) = log.keep()?;
    eprintln!("agitd: startup diagnostics: {}", path.display());
    #[cfg(windows)]
    {
        windows::spawn_daemon(log)
    }
    #[cfg(unix)]
    {
        let mut command = crate::infra::background::command(std::env::current_exe()?);
        command
            .args(["rc", "local", "start"])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // Detachment is process ownership, not a promise made by the SSH channel.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[cfg(unix)]
fn bridge() -> crate::Result<()> {
    let socket = super::control::connect_within(&rpc_path()?, std::time::Duration::from_secs(5))
        .context("local daemon disappeared before bridge attachment; retry the connection")?;
    let mut socket = authenticate_server(socket, unsafe { libc::geteuid() })?;
    let mut input = socket.try_clone()?;
    // The output owner exits on socket closure even while stdin has no data.
    std::thread::spawn(move || {
        let _ = copy_flushed(&mut std::io::stdin().lock(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    copy_flushed(&mut socket, &mut std::io::stdout().lock())?;
    Ok(())
}

#[cfg(unix)]
fn authenticate_server(
    socket: std::os::unix::net::UnixStream,
    owner: libc::uid_t,
) -> crate::Result<std::os::unix::net::UnixStream> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let _guard = runtime.enter();
    socket.set_nonblocking(true)?;
    let socket = tokio::net::UnixStream::from_std(socket)?;
    ensure!(
        socket.peer_cred()?.uid() == owner,
        "Local RPC server belongs to another user"
    );
    let socket = socket.into_std()?;
    socket.set_nonblocking(false)?;
    Ok(socket)
}

#[cfg(unix)]
fn copy_flushed(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let mut bytes = [0; 32768];
    loop {
        let count = match reader.read(&mut bytes) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..count])?;
        // Interactive protocol bytes must be visible before the next input arrives.
        writer.flush()?;
    }
}

#[cfg(unix)]
pub fn listen() -> crate::Result<UnixListener> {
    // The control listener already holds exclusive daemon ownership.
    let path = rpc_path()?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            metadata.file_type().is_socket(),
            "local RPC path is not a socket"
        );
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn bridge_authenticates_server_before_forwarding_bytes() {
        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let owner = unsafe { libc::geteuid() };
        let error = authenticate_server(client, owner.wrapping_add(1)).unwrap_err();
        assert!(error.to_string().contains("another user"));
        assert_eq!(server.read(&mut [0; 1]).unwrap(), 0);

        let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(authenticate_server(client, owner).is_ok());
    }
}
