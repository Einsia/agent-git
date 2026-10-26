//! Cloning a repository that exists on the Hub with no refs yields a usable main file line.
//!
//! The first file commit must land on main from the checkout clone prints, with no session
//! metadata. A clone that leaves Git's unborn default branch makes the commit read as a session
//! settlement; one that leaves HEAD detached sends it to a separate worktree that lacks the page.

use agit::domain::{meta, repo::Repo};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Answers the repository lookup with a local empty source; every other request is a 404.
struct Hub {
    base: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn new(source: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let lookup = serde_json::json!({
            "agent_id": "aaaaaaaa-0000-4000-8000-000000000127",
            "owner": "alice",
            "name": "wiki",
            "visibility": "private",
            "clone_url": source,
        })
        .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("hub fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    headers.push(byte[0]);
                }
                if headers.starts_with(b"GET /api/agents/alice/wiki HTTP/1.1\r\n") {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{lookup}",
                        lookup.len()
                    );
                } else {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            }
        });
        Self {
            base,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    hub: Hub,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.git");
        for dir in ["work", "templates", "tmp"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let lab = Self {
            home: root.path().join("agit"),
            hub: Hub::new(&source),
            root,
        };
        lab.git(
            lab.root.path(),
            &["init", "--quiet", "--bare", "source.git"],
        );
        let credential = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "alice".into(),
            email: None,
            hub: Some(lab.hub.base.clone()),
            access_token: "synthetic-access".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "synthetic-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(&lab.hub.base).unwrap();
        agit::infra::credentials::save_at(
            &lab.home.join("credentials").join(format!("{key}.json")),
            &credential,
        )
        .unwrap();
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        let root = self.root.path();
        cmd.env("HOME", root)
            .env("USERPROFILE", root)
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub.base)
            .env("TMP", root.join("tmp"))
            .env("TEMP", root.join("tmp"))
            .env("TMPDIR", root.join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("empty-config"))
            .env("GIT_TEMPLATE_DIR", root.join("templates"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(root.join("work"))
            .stdin(Stdio::null());
        cmd
    }

    fn agit(&self, args: &[&str]) -> Output {
        let out = self
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "agit {args:?}: {out:?}");
        out
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
}

#[test]
fn first_file_commit_lands_on_main_from_the_checkout_an_empty_clone_prints() {
    let lab = Lab::new();
    lab.agit(&["clone", "alice/wiki", "--no-bind", "--auto-push=false"]);
    let checkout = lab.home.join("repos/alice/wiki");
    assert_eq!(
        lab.git(&checkout, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert!(meta::is_file_line_at(
        &Repo::at(&checkout),
        "refs/heads/main"
    ));

    std::fs::write(checkout.join("README.md"), "# Team wiki\n").unwrap();
    lab.agit(&[
        "commit",
        "alice/wiki@main",
        "-m",
        "wiki: first page",
        "--",
        "README.md",
    ]);
    assert_eq!(
        lab.git(&checkout, &["log", "--format=%s", "refs/heads/main"]),
        "wiki: first page\nagit: init (main file line)"
    );
    assert_eq!(
        lab.git(&checkout, &["show", "refs/heads/main:README.md"]),
        "# Team wiki"
    );
    assert_eq!(
        lab.git(&checkout, &["ls-tree", "--name-only", "refs/heads/main~1"]),
        ".gitattributes\nsession",
        "the line carries its form and none of init's scaffolding"
    );
}
