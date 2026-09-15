//! Dropping a transport owner terminates and reaps its descendant process tree.

use std::process::Stdio;
use tokio::process::{Child, Command};

pub struct Process {
    pub child: Child,
    #[cfg(unix)]
    group: i32,
    #[cfg(windows)]
    job: crate::windows_job::Job,
}

impl Process {
    pub fn spawn(command: &mut Command) -> crate::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        let job = {
            let job = crate::windows_job::Job::new()?;
            crate::windows_job::Job::configure(command);
            job
        };
        let child = command.spawn()?;
        #[cfg(windows)]
        job.attach_and_resume(&child)?;
        Ok(Self {
            #[cfg(unix)]
            group: child
                .id()
                .ok_or_else(|| anyhow::anyhow!("tunnel process has no PID"))?
                as i32,
            child,
            #[cfg(windows)]
            job,
        })
    }

    pub fn terminate(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = self.job.terminate();
        let _ = self.child.start_kill();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}
