//! Publish targets require explicit repository and branch selection before any outbound request.

use agit::domain::{link, meta, repo::Repo, store::Store};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[test]
fn singleton_publish_state_cannot_replace_missing_or_rejected_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("agit");
    let repo = Repo::init(&home.join("repos/me/qa")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("synthetic shared history").unwrap();
    repo.git(&["branch", "-m", "work"]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let credential = agit::infra::credentials::HubCredential {
        username: "me".into(),
        email: None,
        hub: Some(hub.clone()),
        access_token: "synthetic-token".into(),
        access_expires_at: "2099-01-01T00:00:00Z".into(),
        refresh_token: "synthetic-refresh".into(),
        refresh_expires_at: "2099-01-01T00:00:00Z".into(),
    };
    agit::infra::credentials::save_at(
        &home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&hub).unwrap()
        )),
        &credential,
    )
    .unwrap();
    let mut stale = link::Link::new("claude-code", "stale-runtime", None);
    stale.owner = Some("me".into());
    stale.agent = Some("qa".into());
    stale.branch = Some("other".into());
    link::write(&Store::at(home.join("store")), &stale).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let finished = stop.clone();
    let seen = requests.clone();
    let server = std::thread::spawn(move || {
        while !finished.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut buffer = [0; 4096];
                    let _ = stream.read(&mut buffer);
                    seen.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"error":"synthetic route refused","kind":"not_found"}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => panic!("synthetic Hub accept: {error}"),
            }
        }
    });
    let invoke = |args: &[&str], identity: Option<&str>, native: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(temporary.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", temporary.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub)
            .env("CI", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if let Some(value) = identity {
            command.env("AGIT_SESSION", value);
        }
        if native {
            command.env("CLAUDE_CODE_SESSION_ID", "stale-runtime");
        }
        command.output().unwrap()
    };
    let mut refused = vec![];
    for (identity, native) in [
        (None, false),
        (Some("malformed"), false),
        (Some("me/qa@work"), true),
    ] {
        for args in [
            &["push"][..],
            &["push", "--all"],
            &["push", "-b", "work"],
            &["push", "me/qa"],
        ] {
            refused.push(invoke(args, identity, native));
        }
    }
    let unpublished = requests.load(Ordering::SeqCst);
    let allowed = [
        invoke(&["push", "me/qa@work", "--dry-run"], None, false),
        invoke(&["push", "me/qa", "-b", "work", "--dry-run"], None, false),
        invoke(&["push", "me/qa", "--all", "--dry-run"], None, false),
        invoke(&["push", "--dry-run"], Some("me/qa@work"), false),
        invoke(
            &["push", "me/qa@work", "--dry-run"],
            Some("me/qa@work"),
            true,
        ),
    ];
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();
    assert_eq!(
        unpublished, 0,
        "missing or rejected identity must not reach the Hub"
    );
    for output in refused {
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("explicit"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for output in allowed {
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
