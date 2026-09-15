use super::*;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

enum Startup {
    Ready,
    Exited(ExitStatus),
    TimedOut,
}

pub(super) fn detach(hub: &str) -> CmdResult {
    // Each launch owns its log, so competing starters cannot truncate a live daemon's output.
    let directory = crate::rc::rc_dir()?;
    #[cfg(not(windows))]
    let log = tempfile::Builder::new()
        .prefix("agitd-")
        .suffix(".log")
        .tempfile_in(&directory)?;
    #[cfg(windows)]
    let log = crate::infra::windows_security::private_tempfile(&directory, "agitd-", ".log")?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(["rc", "start"])
        .env("AGIT_HUB_URL", hub)
        .stdin(std::process::Stdio::null())
        .stdout(log.as_file().try_clone()?)
        .stderr(log.as_file().try_clone()?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    // Persist before spawning: no later failure may unlink the running child's diagnostics.
    let (_, log_path) = log.keep()?;
    println!("  log       {}", log_path.display());
    let mut child = command.spawn()?;
    println!(
        "  process   started (pid {}); connecting to Hub",
        child.id()
    );
    let outcome = wait_for_ready(&mut child, hub, STARTUP_TIMEOUT, |budget| {
        control::ask_with_timeout(&control::Request::Status, budget)
            .ok()
            .and_then(|reply| match reply {
                control::Reply::Status(status) => Some(status),
                _ => None,
            })
    })?;
    let code = match outcome {
        Startup::Ready => {
            println!("  {} Hub registered; remote control ready", ui::ok("✓"));
            println!(
                "  workspaces {}",
                crate::rc::navigation::workspaces_url(hub)
            );
            ExitCode::Ok
        }
        Startup::Exited(status) => {
            ui::error(&format!("daemon exited before Hub readiness ({status})"));
            ui::hint(&format!(
                "check {} for the startup error",
                log_path.display()
            ));
            ExitCode::Precondition
        }
        Startup::TimedOut => {
            ui::error("Hub readiness was not confirmed before the startup deadline");
            ui::hint(&format!(
                "the daemon process has not exited; it may still be connecting or busy. Check {}",
                log_path.display()
            ));
            ExitCode::Precondition
        }
    };
    ui::hint("`agit rc status` to check it, `agit rc stop` to stop it");
    Ok(code)
}

fn wait_for_ready(
    child: &mut Child,
    hub: &str,
    within: Duration,
    mut probe: impl FnMut(Duration) -> Option<control::Status>,
) -> crate::Result<Startup> {
    let authority = crate::infra::hub_authority::HubAuthority::parse(hub)?;
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Startup::Exited(status));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(Startup::TimedOut);
        }
        // The transport bounds connect, write and read separately. Reserve a budget for each.
        let budget = (remaining / 3).min(POLL_INTERVAL);
        if let Some(status) = probe(budget)
            && status.pid == child.id()
            && status.online
            && authority.matches(&status.hub)
            && Instant::now() < deadline
        {
            // A different daemon or a child that already exited cannot establish readiness.
            return Ok(match child.try_wait()? {
                Some(status) => Startup::Exited(status),
                None => Startup::Ready,
            });
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_child() {
        if std::env::var_os("AGIT_RC_STARTUP_TEST_CHILD").is_some() {
            use std::io::Read;
            let _ = std::io::stdin().read_exact(&mut [0]);
        }
    }

    struct WaitingChild(Child);

    impl WaitingChild {
        fn new() -> Self {
            Self(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "commands::rc::startup::tests::startup_child"])
                    .env("AGIT_RC_STARTUP_TEST_CHILD", "1")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }
    }

    impl Drop for WaitingChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn readiness_requires_the_spawned_child_registered_to_this_hub() {
        let mut child = WaitingChild::new();
        let expected = control::Status {
            pid: child.0.id(),
            hub: "http://hub.test".into(),
            online: true,
            ..Default::default()
        };
        let mut replies = [
            None,
            Some(control::Status {
                online: false,
                ..expected.clone()
            }),
            Some(control::Status {
                pid: expected.pid.wrapping_add(1),
                ..expected.clone()
            }),
            Some(control::Status {
                hub: "http://another.test".into(),
                ..expected.clone()
            }),
            Some(expected),
        ]
        .into_iter();
        let result = wait_for_ready(
            &mut child.0,
            "http://hub.test",
            Duration::from_secs(5),
            |_| {
                replies
                    .next()
                    .expect("readiness must finish on the matching reply")
            },
        )
        .unwrap();
        assert!(matches!(result, Startup::Ready));
        assert!(replies.next().is_none());
    }

    #[test]
    fn an_unready_live_child_times_out_without_being_stopped() {
        let mut child = WaitingChild::new();
        let within = Duration::from_millis(20);
        let result = wait_for_ready(&mut child.0, "http://hub.test", within, |budget| {
            assert!(budget <= within / 3);
            None
        })
        .unwrap();
        assert!(matches!(result, Startup::TimedOut));
        assert!(child.0.try_wait().unwrap().is_none());
    }

    #[test]
    fn a_ready_reply_after_the_deadline_is_not_success() {
        let mut child = WaitingChild::new();
        let pid = child.0.id();
        let within = Duration::from_millis(10);
        let result = wait_for_ready(&mut child.0, "http://hub.test", within, |_| {
            std::thread::sleep(within);
            Some(control::Status {
                pid,
                hub: "http://hub.test".into(),
                online: true,
                ..Default::default()
            })
        })
        .unwrap();
        assert!(matches!(result, Startup::TimedOut));
    }

    #[test]
    fn an_exited_child_fails_without_waiting_for_the_startup_deadline() {
        let mut child = WaitingChild::new();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let result = wait_for_ready(&mut child.0, "http://hub.test", STARTUP_TIMEOUT, |_| {
            panic!("an exited child cannot be ready")
        })
        .unwrap();
        assert!(matches!(result, Startup::Exited(_)));
    }
}
