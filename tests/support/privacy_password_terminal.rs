use std::{
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

pub fn terminal_password(
    home: &Path,
    runtime_home: &Path,
    workspace: &Path,
    hub: &str,
    args: &[&str],
    password: &str,
) -> (portable_pty::ExitStatus, String) {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    command.args(args);
    command.env_clear();
    for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("HOME", runtime_home);
    command.env("USERPROFILE", runtime_home);
    command.env("AGIT_HOME", home);
    command.env("AGIT_HUB_URL", hub);
    command.env("AGIT_USE_SYSTEM_GIT", "1");
    command.env("AGIT_SECRETS_KEYSTORE", "file");
    command.env("AGIT_TUI", "0");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", home.join("absent-gitconfig"));
    command.env("NO_COLOR", "1");
    command.env("CI", "1");
    command.env("TERM", "xterm-256color");
    command.cwd(workspace);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    #[cfg(unix)]
    let terminal_fd = pair.master.as_raw_fd().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _master = pair.master;
        let mut buffer = [0; 4096];
        while let Ok(size) = reader.read(&mut buffer) {
            if size == 0 || sender.send(buffer[..size].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut captured = Vec::new();
    let mut sent = false;
    loop {
        if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(10)) {
            captured.extend_from_slice(&bytes);
        }
        let output = String::from_utf8_lossy(&captured);
        #[cfg(unix)]
        let ready = {
            let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
            unsafe {
                libc::tcgetattr(terminal_fd, attributes.as_mut_ptr()) == 0
                    && attributes.assume_init().c_lflag & libc::ECHO == 0
            }
        };
        #[cfg(not(unix))]
        let ready = true;
        if !sent && ready && output.contains("Repository viewing password:") {
            writer.write_all(password.as_bytes()).unwrap();
            writer.write_all(b"\r").unwrap();
            writer.flush().unwrap();
            sent = true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            while let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(20)) {
                captured.extend_from_slice(&bytes);
            }
            drop(writer);
            worker.join().unwrap();
            return (status, String::from_utf8_lossy(&captured).into_owned());
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("repository password command timed out: {output}");
        }
    }
}
