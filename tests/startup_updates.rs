//! Startup updates must leave command data intact and require a human before installation.

use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NOTICE: &str = "agit 999.0.0 is available";

struct Lab {
    root: tempfile::TempDir,
    store: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("agit");
        fs::create_dir(&store).unwrap();
        Self { root, store }
    }

    fn cache(&self, latest: &str) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        fs::write(
            self.store.join("cli-update.json"),
            json!({"checked_at": now.as_secs(), "latest": latest}).to_string(),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str], hub: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", hub)
            .env("AGIT_TELEMETRY_DEFER", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(self.root.path());
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command
    }
}

fn assert_notice(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches(NOTICE).count(), 1, "{output:?}");
    assert!(!stderr.contains("Update agit now?"), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(NOTICE));
}

#[test]
fn redirected_and_json_commands_report_updates_outside_command_data() {
    if !agit::infra::config::is_production_release() {
        return;
    }
    let lab = Lab::new();
    lab.cache("999.0.0");
    let hub = "http://127.0.0.1:1";
    let plain = lab.command(&["config", "hub.url"], hub).output().unwrap();
    assert_notice(&plain);
    assert_eq!(String::from_utf8(plain.stdout).unwrap().trim(), hub);
    for version in ["1", "2"] {
        let output = lab
            .command(
                &["config", "hub.url", "--json", "--json-version", version],
                hub,
            )
            .output()
            .unwrap();
        assert_notice(&output);
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(envelope["diagnostics"]["stderr"], json!([]));
    }
    let ci = lab
        .command(&["config", "hub.url"], hub)
        .env("CI", "1")
        .output()
        .unwrap();
    assert_notice(&ci);
    let quiet = lab
        .command(&["--quiet", "config", "hub.url", "--json"], hub)
        .output()
        .unwrap();
    assert!(quiet.status.success());
    assert!(quiet.stderr.is_empty());
    assert!(serde_json::from_slice::<Value>(&quiet.stdout).unwrap()["ok"] == true);
}

struct Hub {
    base: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn new(status: u16, tarball: Option<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let base = base.clone();
            let requests = requests.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") {
                        assert!(request.len() < 65536);
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let path = request.split_whitespace().nth(1).unwrap();
                    let body = if path == "/api/cli/version" {
                        requests.fetch_add(1, Ordering::SeqCst);
                        json!({
                            "version": "999.0.0", "tag": "v999.0.0", "url": base,
                            "repo": "fixture/agit", "stale": false,
                            "npm_package": "@fixture/agit"
                        })
                        .to_string()
                        .into_bytes()
                    } else if path == "/fixture.tgz" {
                        tarball.as_ref().unwrap().clone()
                    } else {
                        use base64::Engine as _;
                        use sha2::Digest as _;
                        let digest = sha2::Sha512::digest(tarball.as_ref().unwrap());
                        let hash = base64::engine::general_purpose::STANDARD.encode(digest);
                        json!({"dist": {
                            "tarball": format!("{base}/fixture.tgz"),
                            "integrity": format!("sha512-{hash}")
                        }})
                        .to_string()
                        .into_bytes()
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                }
            })
        };
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn noninteractive_checks_cache_success_and_failure_without_changing_the_result() {
    if !agit::infra::config::is_production_release() {
        return;
    }
    for status in [200, 503] {
        let lab = Lab::new();
        let hub = Hub::new(status, None);
        for _ in 0..2 {
            let output = lab
                .command(&["config", "hub.url", "--json"], &hub.base)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["ok"],
                true
            );
            if status == 200 {
                assert_notice(&output);
            } else {
                assert!(output.stderr.is_empty(), "{output:?}");
            }
        }
        assert_eq!(hub.requests.load(Ordering::SeqCst), 1);
    }
}

#[cfg(unix)]
mod terminal {
    use super::*;
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::time::Instant;

    struct Terminal {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        writer: Box<dyn Write + Send>,
        chunks: std::sync::mpsc::Receiver<Vec<u8>>,
        output: String,
    }

    impl Terminal {
        fn start(
            lab: &Lab,
            executable: &std::path::Path,
            args: &[&str],
            env: &[(&str, &str)],
        ) -> Self {
            let pair = native_pty_system().openpty(PtySize::default()).unwrap();
            let mut builder = CommandBuilder::new(executable);
            builder.args(args);
            builder.env_clear();
            builder.env("PATH", std::env::var_os("PATH").unwrap_or_default());
            builder.env("HOME", lab.root.path());
            builder.env("AGIT_HOME", &lab.store);
            builder.env("AGIT_HUB_URL", "http://127.0.0.1:1");
            builder.env("AGIT_TELEMETRY_DEFER", "1");
            builder.env("NO_COLOR", "1");
            builder.env("TERM", "xterm");
            builder.env("GIT_CONFIG_NOSYSTEM", "1");
            builder.cwd(lab.root.path());
            for (key, value) in env {
                builder.env(key, value);
            }
            let mut reader = pair.master.try_clone_reader().unwrap();
            let writer = pair.master.take_writer().unwrap();
            let child = pair.slave.spawn_command(builder).unwrap();
            drop(pair.slave);
            let (sender, chunks) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _master = pair.master;
                let mut buffer = [0; 4096];
                while let Ok(count) = reader.read(&mut buffer) {
                    if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
            });
            Self {
                child,
                writer,
                chunks,
                output: String::new(),
            }
        }

        fn read_until(&mut self, mut done: impl FnMut(&mut Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                while let Ok(chunk) = self.chunks.try_recv() {
                    self.output.push_str(&String::from_utf8_lossy(&chunk));
                    assert!(self.output.len() < 1024 * 1024);
                }
                if done(self) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "terminal timed out: {}",
                    self.output
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn finish(&mut self) {
            self.read_until(|terminal| {
                terminal.child.try_wait().unwrap().is_some_and(|status| {
                    assert!(status.success(), "{}", terminal.output);
                    true
                })
            });
            while let Ok(chunk) = self.chunks.recv_timeout(Duration::from_millis(100)) {
                self.output.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
    }

    impl Drop for Terminal {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn upgrade_fixture(lab: &Lab, replacement: &[u8]) -> (Hub, PathBuf) {
        lab.cache("999.0.0");
        let package = lab.root.path().join("package/bin");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("agit"), replacement).unwrap();
        let tgz = lab.root.path().join("fixture.tgz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&tgz)
                .arg("-C")
                .arg(lab.root.path())
                .arg("package")
                .status()
                .unwrap()
                .success()
        );
        let hub = Hub::new(200, Some(fs::read(&tgz).unwrap()));
        let executable = lab.root.path().join("agit-bin");
        fs::copy(env!("CARGO_BIN_EXE_agit"), &executable).unwrap();
        (hub, executable)
    }

    #[test]
    fn human_terminal_waits_and_declining_or_enter_continues_the_command() {
        if !agit::infra::config::is_production_release() {
            return;
        }
        for answer in [b"n".as_slice(), b"\r".as_slice()] {
            let lab = Lab::new();
            lab.cache("999.0.0");
            let mut terminal = Terminal::start(
                &lab,
                env!("CARGO_BIN_EXE_agit").as_ref(),
                &["config", "hub.url"],
                &[],
            );
            terminal.read_until(|terminal| terminal.output.contains("Update agit now?"));
            assert!(terminal.child.try_wait().unwrap().is_none());
            assert!(!terminal.output.contains("http://127.0.0.1:1"));
            assert!(
                terminal.output.contains("y/n") || terminal.output.contains("y/N"),
                "{}",
                terminal.output
            );
            terminal.writer.write_all(answer).unwrap();
            terminal.writer.flush().unwrap();
            terminal.finish();
            assert!(terminal.output.contains("http://127.0.0.1:1"));
            assert!(!terminal.output.contains("upgraded to"));
        }
    }

    #[test]
    fn unattended_terminal_modes_never_wait_or_install() {
        if !agit::infra::config::is_production_release() {
            return;
        }
        for (extra, env) in [
            (vec!["--json"], vec![]),
            (vec!["--json", "--tui"], vec![]),
            (vec!["--yes"], vec![]),
            (vec!["--no-tui"], vec![]),
            (vec![], vec![("CI", "1")]),
            (vec![], vec![("AGIT_SESSION", "fixture/repo@branch")]),
            (vec![], vec![("CODEX_SESSION_ID", "fixture")]),
        ] {
            let lab = Lab::new();
            lab.cache("999.0.0");
            let mut args = vec!["config", "hub.url"];
            args.extend(extra);
            let mut terminal =
                Terminal::start(&lab, env!("CARGO_BIN_EXE_agit").as_ref(), &args, &env);
            terminal.finish();
            assert!(terminal.output.contains(NOTICE), "{}", terminal.output);
            assert!(
                !terminal.output.contains("Update agit now?"),
                "{}",
                terminal.output
            );
            assert!(!terminal.output.contains("upgraded to"));
        }
    }

    #[test]
    fn accepting_restarts_the_copied_cli_with_original_arguments_and_directory() {
        if !agit::infra::config::is_production_release() {
            return;
        }
        let lab = Lab::new();
        let replacement = "#!/bin/sh\nif [ \"$1\" = --no-tui ]; then\n  printf '%s\\n' \"$@\" > \"$AGIT_HOME/refreshed\"\nelse\n  printf '%s\\n' \"$@\" > \"$AGIT_HOME/continued\"\n  pwd > \"$AGIT_HOME/continued-cwd\"\n  printf '%s\\n' \"$AGIT_HUB_URL\"\nfi\n";
        let (hub, executable) = upgrade_fixture(&lab, replacement.as_bytes());
        fs::create_dir(lab.root.path().join("nested directory")).unwrap();
        let args = ["-C", "nested directory", "config", "hub.url"];
        let mut terminal = Terminal::start(
            &lab,
            &executable,
            &args,
            &[
                ("AGIT_HUB_URL", &hub.base),
                ("AGIT_NPM_REGISTRY", &hub.base),
            ],
        );
        terminal.read_until(|terminal| terminal.output.contains("Update agit now?"));
        assert!(terminal.child.try_wait().unwrap().is_none());
        assert!(!lab.store.join("refreshed").exists());
        terminal.writer.write_all(b"y").unwrap();
        terminal.writer.flush().unwrap();
        terminal.finish();
        assert!(
            terminal.output.contains("upgraded to 999.0.0"),
            "{}",
            terminal.output
        );
        assert!(terminal.output.contains(&hub.base));
        assert_eq!(fs::read_to_string(&executable).unwrap(), replacement);
        assert_eq!(
            fs::read_to_string(lab.store.join("refreshed")).unwrap(),
            "--no-tui\n--quiet\nsetup\n--skill\n--installed-only\n"
        );
        assert_eq!(
            fs::read_to_string(lab.store.join("continued")).unwrap(),
            format!("{}\n", args.join("\n"))
        );
        assert_eq!(
            PathBuf::from(
                fs::read_to_string(lab.store.join("continued-cwd"))
                    .unwrap()
                    .trim()
            )
            .canonicalize()
            .unwrap(),
            lab.root.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn restarted_setup_and_daemon_use_the_installed_executable_and_skill_bundle() {
        if !agit::infra::config::is_production_release() {
            return;
        }
        let build = tempfile::tempdir().unwrap();
        let replacement = build.path().join("replacement");
        assert!(
            Command::new("rustc")
                .arg("--edition=2024")
                .arg("-Dwarnings")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/upgraded_cli.rs"
                ))
                .arg("-o")
                .arg(&replacement)
                .status()
                .unwrap()
                .success()
        );
        let bytes = fs::read(replacement).unwrap();
        for fail_refresh in [false, true] {
            for args in [
                vec!["setup", "--skill"],
                vec!["setup", "--hooks"],
                vec!["rc", "start", "--detach"],
            ] {
                let lab = Lab::new();
                let (hub, executable) = upgrade_fixture(&lab, &bytes);
                let mut env = vec![
                    ("AGIT_HUB_URL", hub.base.as_str()),
                    ("AGIT_NPM_REGISTRY", hub.base.as_str()),
                ];
                if fail_refresh {
                    env.push(("FIXTURE_REFRESH_FAILURE", "1"));
                }
                let mut terminal = Terminal::start(&lab, &executable, &args, &env);
                terminal.read_until(|terminal| terminal.output.contains("Update agit now?"));
                terminal.writer.write_all(b"y").unwrap();
                terminal.writer.flush().unwrap();
                terminal.finish();
                assert_eq!(
                    fs::read_to_string(lab.store.join("continued")).unwrap(),
                    args.join("\n")
                );
                let installed = executable.canonicalize().unwrap();
                for file in [
                    Some("continued-exe"),
                    if args[0] == "rc" {
                        Some("daemon-exe")
                    } else if args[1] == "--hooks" {
                        Some("hook-exe")
                    } else {
                        None
                    },
                ]
                .into_iter()
                .flatten()
                {
                    assert_eq!(
                        PathBuf::from(fs::read_to_string(lab.store.join(file)).unwrap())
                            .canonicalize()
                            .unwrap(),
                        installed
                    );
                }
                assert_eq!(
                    fs::read_to_string(lab.root.path().join(".codex/skills/agit/SKILL.md"))
                        .unwrap(),
                    "new fixture skill\n"
                );
            }
        }
    }

    #[test]
    fn accepted_updates_preserve_one_telemetry_invocation_and_its_prompt() {
        if !agit::infra::config::is_production_release() {
            return;
        }
        for installed in [true, false] {
            let lab = Lab::new();
            let (hub, executable) = if installed {
                upgrade_fixture(&lab, b"#!/bin/sh\nexec \"$FIXTURE_REAL_CLI\" \"$@\"\n")
            } else {
                lab.cache("999.0.0");
                (
                    Hub::new(503, None),
                    PathBuf::from(env!("CARGO_BIN_EXE_agit")),
                )
            };
            fs::create_dir_all(lab.store.join("telemetry")).unwrap();
            fs::write(
                lab.store.join("telemetry/preferences.json"),
                json!({"preference":"enabled", "generation":1, "device_id":uuid::Uuid::new_v4()})
                    .to_string(),
            )
            .unwrap();
            let mut terminal = Terminal::start(
                &lab,
                &executable,
                &["config", "hub.url"],
                &[
                    ("AGIT_HUB_URL", &hub.base),
                    ("AGIT_NPM_REGISTRY", &hub.base),
                    ("AGIT_TELEMETRY_HOST", "http://127.0.0.1:9"),
                    ("AGIT_TELEMETRY_KEY", "synthetic_project"),
                    ("AGIT_TELEMETRY_DEFER", "0"),
                    ("FIXTURE_REAL_CLI", env!("CARGO_BIN_EXE_agit")),
                ],
            );
            terminal.read_until(|terminal| terminal.output.contains("Update agit now?"));
            terminal.writer.write_all(b"y").unwrap();
            terminal.writer.flush().unwrap();
            terminal.finish();
            assert_eq!(
                terminal.output.contains("upgraded to 999.0.0"),
                installed,
                "{}",
                terminal.output
            );
            let queue: Value =
                serde_json::from_slice(&fs::read(lab.store.join("telemetry/queue.json")).unwrap())
                    .unwrap();
            let events: Vec<_> = queue["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| &entry["event"])
                .filter(|event| {
                    event["properties"]["command"] == "config"
                        && matches!(
                            event["event"].as_str(),
                            Some("cli_command_started" | "cli_command_finished")
                        )
                })
                .collect();
            assert_eq!(events.len(), 2, "{events:?}");
            assert_eq!(events[0]["event"], "cli_command_started");
            assert_eq!(events[1]["event"], "cli_command_finished");
            assert_eq!(
                events[0]["properties"]["invocation_id"],
                events[1]["properties"]["invocation_id"]
            );
            assert_eq!(events[1]["properties"]["prompt_shown"], true);
            assert_eq!(events[1]["properties"]["exit_code"], 0);
        }
    }

    #[test]
    fn failed_upgrade_still_runs_the_requested_command() {
        if !agit::infra::config::is_production_release() {
            return;
        }
        let lab = Lab::new();
        lab.cache("999.0.0");
        let hub = Hub::new(503, None);
        let mut terminal = Terminal::start(
            &lab,
            env!("CARGO_BIN_EXE_agit").as_ref(),
            &["config", "hub.url"],
            &[("AGIT_HUB_URL", &hub.base)],
        );
        terminal.read_until(|terminal| terminal.output.contains("Update agit now?"));
        terminal.writer.write_all(b"y").unwrap();
        terminal.writer.flush().unwrap();
        terminal.finish();
        assert!(terminal.output.contains("continuing the requested command"));
        assert!(
            terminal.output.trim_end().ends_with(&hub.base),
            "{}",
            terminal.output
        );
        assert_eq!(hub.requests.load(Ordering::SeqCst), 1);
    }
}
