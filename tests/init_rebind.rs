//! When the directory is already bound to another repo, `agit init` refuses before it touches
//! the disk: no half-built repository may be left behind for the next attempt to hit
//! "already exists". `--rebind` is the only way to rebind explicitly.

use std::path::Path;
use std::{fs, process::Command};

fn bind(home: &Path, work: &Path, repo: &str) {
    use sha2::{Digest, Sha256};
    let canonical = work.canonicalize().unwrap();
    let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
    let dir = home.join("workspaces");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({ "dir": canonical, "repo": repo })).unwrap(),
    )
    .unwrap();
}

fn agit(home: &Path, work: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .current_dir(work)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env_remove("AGIT_SESSION")
        .output()
        .unwrap()
}

#[test]
fn init_refuses_before_creating_anything_when_the_directory_is_bound_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    bind(&home, &work, "me/other");

    let out = agit(&home, &work, &["init", "qa"]);
    assert!(!out.status.success(), "init must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("me/other") && stderr.contains("--rebind"),
        "{stderr}"
    );
    assert!(
        !home.join("repos/local/qa").exists(),
        "a refused init leaves no repository behind"
    );

    let out = agit(&home, &work, &["init", "qa", "--rebind"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(home.join("repos/local/qa/.git").exists());
    let ws = fs::read_dir(home.join("workspaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let text = fs::read_to_string(ws.path()).unwrap();
    assert!(text.contains("local/qa"), "{text}");
}

#[test]
fn automatic_init_requires_login_before_creation_and_explicit_off_stays_local() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let out = agit(&home, &work, &["init", "automatic", "--auto-push"]);
    assert!(!out.status.success());
    assert!(!home.join("repos/local/automatic").exists());
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        output.contains("Sign in to enable automatic pushing"),
        "{output}"
    );
    assert!(
        agit(&home, &work, &["config", "push.auto", "true"])
            .status
            .success()
    );
    let out = agit(&home, &work, &["init", "manual", "--auto-push=false"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo = agit::domain::repo::Repo::open(home.join("repos/local/manual")).unwrap();
    assert_eq!(repo.auto_push_override().unwrap(), Some(false));
}

/// A bare `--auto-push` takes no separated value, so `--auto-push false` would name the repository
/// `false` and turn automatic pushing on. The parse is refused before anything is created, and the
/// error shows the `=` form; accepting it falls through to sign-in or creates `local/false`.
#[test]
fn a_separated_auto_push_value_is_refused_instead_of_naming_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let out = agit(&home, &work, &["init", "--auto-push", "false", "--no-bind"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--auto-push=false"), "{stderr}");
    assert!(!home.join("repos/local/false").exists());
}

/// Run agit under a pseudo-terminal that nobody types into, as an agent's tool shell does, with a
/// runtime session variable present. Panics when the command is still waiting at the deadline.
#[cfg(unix)]
fn agit_under_agent_terminal(home: &Path, work: &Path, args: &[&str]) -> (bool, String) {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read as _;
    use std::time::{Duration, Instant};

    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    builder.env_clear();
    let path = std::env::var("PATH").unwrap_or_default();
    let store = home.join("agit");
    for (key, value) in [
        ("PATH", path.as_str()),
        ("HOME", home.to_str().unwrap()),
        ("AGIT_HOME", store.to_str().unwrap()),
        ("AGIT_HUB_URL", "http://127.0.0.1:1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("TERM", "xterm"),
        ("CODEBUDDY_SESSION_ID", "synthetic-agent-session"),
    ] {
        builder.env(key, value);
    }
    builder.cwd(work);
    builder.args(args);
    let mut child = pair.slave.spawn_command(builder).unwrap();
    drop(pair.slave);
    // Held open for the whole run: the terminal stays attached and nothing is ever typed.
    let _keyboard = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sender, chunks) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = String::new();
    let status = loop {
        while let Ok(chunk) = chunks.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("agit {args:?} waited for input under an agent terminal: {output}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    while let Ok(chunk) = chunks.recv_timeout(Duration::from_millis(200)) {
        output.push_str(&String::from_utf8_lossy(&chunk));
    }
    (status.success(), output)
}

/// An agent's tool shell can give every standard stream a pseudo-terminal that nobody types into.
/// With a runtime session variable present, `agit init <name>` must stop neither at the
/// automatic-push question — it inherits the user preference and says so — nor, when automatic
/// pushing is requested while signed out, at the sign-in menu, which it leaves for the human
/// without creating the repository. An implementation that asks whenever stdin and stdout are
/// terminals blocks until the deadline and fails.
#[cfg(unix)]
#[test]
fn init_under_an_agent_pseudo_terminal_takes_the_default_without_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let store = home.join("agit");

    let (success, output) = agit_under_agent_terminal(&home, &work, &["init", "demo", "--no-bind"]);
    assert!(success, "{output}");
    assert!(
        output.contains("automatic push inherits the user preference (off)"),
        "{output}"
    );
    assert!(store.join("repos/local/demo/.git").exists(), "{output}");

    let (success, output) = agit_under_agent_terminal(
        &home,
        &work,
        &["init", "published", "--auto-push=true", "--no-bind"],
    );
    assert!(!success, "{output}");
    assert!(
        output.contains("Sign in to enable automatic pushing"),
        "{output}"
    );
    assert!(!store.join("repos/local/published").exists(), "{output}");
}
