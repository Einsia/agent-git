//! `agit repo rename` moves a repository's linked worktrees into the new name's place, and stops
//! before the Hub hears of the rename when one of them can't move.

#[path = "support/lab_env.rs"]
mod lab_env;
#[path = "support/worktrees.rs"]
mod worktrees;

use agit::domain::repo::Repo;
use agit::hub::identity::{self, RemoteIdentity};
use lab_env::LabEnv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const AGENT: &str = "cccccccc-0000-4000-8000-000000000003";

/// A Hub that takes a rename and declines everything else a signed-in command may ask, and
/// records each rename it was asked for.
struct Hub {
    url: String,
    renames: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let renames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, stopped) = (renames.clone(), stop.clone());
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("local hub accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (status, reply) = if request.starts_with("PATCH /api/agents/me/old ") {
                    seen.lock().unwrap().push(String::from_utf8(body).unwrap());
                    ("200 OK", "{}")
                } else {
                    ("404 Not Found", "")
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        Self {
            url,
            renames,
            stop,
            server: Some(server),
        }
    }

    fn renames(&self) -> Vec<String> {
        self.renames.lock().unwrap().clone()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// me/old, signed in and pinned to the Hub, with a session branch's worktree in its place that
/// holds a staged edit.
fn lab(hub: &Hub) -> (tempfile::TempDir, std::path::PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("agit");
    let credential = agit::infra::credentials::HubCredential {
        account_id: None,
        username: "me".into(),
        email: None,
        hub: Some(hub.url.clone()),
        access_token: "synthetic-token".into(),
        access_expires_at: "2099-01-01T00:00:00Z".into(),
        refresh_token: "synthetic-refresh".into(),
        refresh_expires_at: "2099-01-01T00:00:00Z".into(),
    };
    agit::infra::credentials::save_at(
        &home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&hub.url).unwrap()
        )),
        &credential,
    )
    .unwrap();
    let repo = Repo::init(&home.join("repos/me/old")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    std::fs::write(repo.root().join("README.md"), "history").unwrap();
    repo.add_all().unwrap();
    repo.commit("history").unwrap();
    identity::pin(&repo, &RemoteIdentity::new(&hub.url, AGENT).unwrap()).unwrap();
    let topic = home.join("worktrees/me/old/topic");
    std::fs::create_dir_all(topic.parent().unwrap()).unwrap();
    repo.git(&["worktree", "add", "-b", "topic", topic.to_str().unwrap()])
        .unwrap();
    std::fs::write(topic.join("staged.txt"), "staged before the rename").unwrap();
    Repo::at(&topic).git(&["add", "staged.txt"]).unwrap();
    (temporary, home)
}

fn rename(temporary: &Path, home: &Path, hub: &Hub) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["repo", "rename", "me/old", "new"])
        .env_clear_local()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", temporary)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", &hub.url)
        .env("CI", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(temporary)
        .output()
        .unwrap()
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A rename moves the worktree into the new name's place, where it opens with its staged edit,
/// and every worktree then names the repository its `.git` points to. A rename that moved only
/// the main checkout leaves the worktree under the old name, pointing at a checkout that is gone.
#[test]
fn a_rename_moves_its_worktrees_into_the_new_place() {
    let hub = Hub::start();
    let (temporary, home) = lab(&hub);
    let output = rename(temporary.path(), &home, &hub);
    let said = said(&output);
    assert!(output.status.success(), "{said}");
    assert_eq!(hub.renames().len(), 1, "{said}");
    assert!(said.contains("and its worktrees: "), "{said}");
    let topic = home.join("worktrees/me/new/topic");
    assert_eq!(
        Repo::at(&topic).git(&["status", "--porcelain"]).unwrap(),
        "A  staged.txt"
    );
    assert!(!home.join("worktrees/me/old").exists());
    worktrees::assert_each_names_its_repository(&home);
}

/// A worktree that can't move stops the rename before the Hub hears of it, with nothing changed
/// on this computer either. Renaming first and moving after would leave the Hub and this
/// computer disagreeing about the name.
#[test]
fn a_worktree_that_cannot_move_stops_the_rename_before_the_hub_hears_of_it() {
    let hub = Hub::start();
    let (temporary, home) = lab(&hub);
    std::fs::create_dir_all(home.join("worktrees/me/new/topic")).unwrap();
    let output = rename(temporary.path(), &home, &hub);
    let said = said(&output);
    assert_eq!(output.status.code(), Some(4), "{said}");
    assert!(
        said.contains("can't move me/old's worktree ")
            && said.contains(
                "worktrees/me/new/topic: something is already there; nothing was renamed"
            ),
        "{said}"
    );
    assert!(hub.renames().is_empty(), "{said}");
    assert!(home.join("repos/me/old/.git").exists());
    assert_eq!(
        Repo::at(home.join("worktrees/me/old/topic"))
            .git(&["status", "--porcelain"])
            .unwrap(),
        "A  staged.txt"
    );
}
