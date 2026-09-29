use std::{
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

struct Child(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for Child {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        if let Some(pid) = self.0.process_id() {
            // The terminal runtime handles graceful signals; teardown cannot await user input.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}

/// Interactive restoration reads model metadata that print-mode continuation can skip.
pub fn assert_loads(config: &Path, workspace: &Path, id: &str) {
    let home = config.parent().unwrap();
    let physical = workspace.canonicalize().unwrap();
    std::fs::write(
        config.join(".claude.json"),
        serde_json::json!({
            "hasCompletedOnboarding":true,
            "theme":"dark",
            "customApiKeyResponses":{"approved":["synthetic-key"],"rejected":[]},
            "projects":{physical.to_string_lossy().as_ref():{"hasTrustDialogAccepted":true}},
        })
        .to_string(),
    )
    .unwrap();
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 40,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(
        std::env::var_os("AGIT_TEST_CLAUDE").unwrap_or_else(|| "claude".into()),
    );
    command.env_clear();
    command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.env("HOME", home);
    command.env("CLAUDE_CONFIG_DIR", config);
    command.env("ANTHROPIC_API_KEY", "synthetic-key");
    command.env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1");
    command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    command.env("TERM", "xterm-256color");
    command.cwd(workspace);
    command.args([
        "--bare",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--tools",
        "",
        "--disable-slash-commands",
        "--resume",
        id,
    ]);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let mut child = Child(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _master = pair.master;
        let mut buffer = [0; 8192];
        while let Ok(size) = reader.read(&mut buffer) {
            if size == 0 || sender.send(buffer[..size].to_vec()).is_err() {
                break;
            }
        }
    });
    let controls = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut captured = Vec::new();
    let mut expanded = false;
    loop {
        if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(20)) {
            captured.extend_from_slice(&bytes);
        }
        let raw = String::from_utf8_lossy(&captured);
        let plain = controls.replace_all(&raw, " ");
        assert!(
            !plain.contains("Failed to resume session") && !plain.contains("No conversation found"),
            "{plain}"
        );
        if !expanded && plain.contains("ORCHID-COPPER-927") {
            writer.write_all(b"\x0f").unwrap();
            writer.flush().unwrap();
            expanded = true;
        }
        if plain.contains("ORCHID-COPPER-927")
            && plain.contains("/source/private.txt")
            && plain.contains("glm-5.3")
            && plain.contains("historical reasoning")
        {
            assert!(child.0.try_wait().unwrap().is_none(), "{plain}");
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "Claude exited before showing the recovered history: {plain}"
        );
        assert!(
            Instant::now() < deadline,
            "Claude did not show the recovered history: {plain}"
        );
    }
}
