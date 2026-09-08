use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Lab {
    _root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let work = root.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(
            home.join("config.json"),
            b"{\"secrets.keystore\":\"file\"}\n",
        )
        .unwrap();
        Self {
            _root: root,
            home,
            work,
        }
    }

    fn repo(&self, slug: &str) -> Repo {
        let repo = Repo::init(&self.home.join("repos").join(slug)).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("synthetic local history").unwrap();
        repo
    }

    fn doctor(&self, hub: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self._root.path())
            .env("USERPROFILE", self._root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", hub)
            .env("AGIT_SESSION", "bob/other@unrelated")
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work)
            .output()
            .unwrap()
    }

    fn pending(&self, name: &str, contents: &str) -> PathBuf {
        let directory = self.home.join("layout-v1-recovery");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("pending-{name}"));
        fs::write(&path, contents).unwrap();
        path
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        // The global keystore diagnostic retains its lock independently of repository scope.
        .filter(|entry| !entry.path().ends_with("secret-filter/vault.lock"))
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn assert_unchanged(root: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) {
    let after = files(root);
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(changed.is_empty(), "inspection changed files: {changed:?}");
}

fn formats() -> Vec<Vec<&'static str>> {
    let mut args = vec![vec!["doctor", "--repo", "alice/chosen"]];
    if cfg!(unix) {
        args.push(vec!["--json", "doctor", "--repo", "alice/chosen"]);
    }
    args
}

#[test]
fn scoped_doctor_skips_unrelated_repositories_recovery_and_automatic_network() {
    let lab = Lab::new();
    lab.repo("alice/chosen");
    let other = lab.repo("bob/other");
    fs::write(other.root().join(meta::FILE), b"broken metadata").unwrap();
    lab.pending("other", "bob/other\n");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let before = files(&lab.home);
    for args in formats() {
        let output = lab.doctor(&hub, &args);
        assert!(output.status.success(), "{}", text(&output));
        assert!(!text(&output).contains("bob/other"), "{}", text(&output));
        assert_unchanged(&lab.home, &before);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "scoped inspection contacted the Hub without an explicit backend check"
        );
    }
}

#[test]
fn scoped_doctor_contacts_the_backend_only_when_explicitly_requested() {
    let lab = Lab::new();
    lab.repo("alice/chosen");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "backend check did not arrive"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("cannot accept backend check: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0 && request.len() < 16 * 1024);
            request.extend_from_slice(&buffer[..read]);
        }
        assert!(request.starts_with(b"GET /api/health HTTP/1.1\r\n"));
        let body = r#"{"status":"ok","version":"synthetic"}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    let before = files(&lab.home);
    let output = lab.doctor(
        &hub,
        &[
            "-C",
            lab.work.to_str().unwrap(),
            "doctor",
            "--repo",
            "alice/chosen",
            "--check-backend",
        ],
    );
    let request = server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("version synthetic"),
        "{}",
        text(&output)
    );
    assert!(
        !String::from_utf8(request)
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization:")
    );
    assert_unchanged(&lab.home, &before);
}

#[test]
fn scoped_doctor_refuses_missing_and_non_repository_selectors_before_storage_work() {
    let lab = Lab::new();
    let before = files(&lab.home);
    for args in formats() {
        let output = lab.doctor("http://127.0.0.1:1", &args);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output));
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["exit_code"], 3);
        }
        assert_unchanged(&lab.home, &before);
    }
    for selector in [
        "chosen",
        "@",
        "alice/chosen@work",
        "alice/chosen#1",
        "alice/chosen~1",
        "../chosen",
        "alice/..",
        "alice/chosen/extra",
        "alice\\other/chosen",
        "alice/ chosen",
    ] {
        let output = lab.doctor("http://127.0.0.1:1", &["doctor", "--repo", selector]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{selector}: {}",
            text(&output)
        );
        assert_unchanged(&lab.home, &before);
    }
}

#[test]
fn scoped_doctor_refuses_related_and_unattributable_recovery_without_consuming_it() {
    let lab = Lab::new();
    let repo = lab.repo("alice/chosen");
    fs::create_dir_all(lab.home.join("repos/bob/not-a-repository")).unwrap();
    for contents in [
        "alice/chosen\n",
        "not a repository path\n",
        "../outside\n",
        "alice/missing\n",
        "bob/not-a-repository\n",
        "alice/chosen",
    ] {
        let pending = lab.pending("selected", contents);
        let before = files(&lab.home);
        for args in formats() {
            let output = lab.doctor("http://127.0.0.1:1", &args);
            assert_eq!(output.status.code(), Some(4), "{}", text(&output));
            assert_unchanged(&lab.home, &before);
        }
        fs::remove_file(pending).unwrap();
    }
    fs::write(
        repo.git_path("agit-checkout-transaction.json").unwrap(),
        b"pending checkout",
    )
    .unwrap();
    let before = files(&lab.home);
    let output = lab.doctor("http://127.0.0.1:1", &formats()[0]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert_unchanged(&lab.home, &before);
}

#[test]
fn scoped_doctor_checks_recovery_in_registered_worktrees() {
    let lab = Lab::new();
    let repo = lab.repo("alice/chosen");
    let linked = lab._root.path().join("linked");
    repo.git(&["worktree", "add", "-b", "work", linked.to_str().unwrap()])
        .unwrap();
    fs::write(
        Repo::at(&linked)
            .git_path("agit-checkout-transaction.json")
            .unwrap(),
        b"pending linked checkout",
    )
    .unwrap();
    let before = files(lab._root.path());
    let output = lab.doctor("http://127.0.0.1:1", &formats()[0]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert_unchanged(lab._root.path(), &before);
}

#[test]
fn scoped_doctor_recognizes_recovery_through_a_git_directory_alias() {
    let lab = Lab::new();
    let repo = lab.repo("alice/chosen");
    let alias = lab.home.join("repos/bob/alias");
    fs::create_dir_all(&alias).unwrap();
    fs::write(
        alias.join(".git"),
        format!("gitdir: {}\n", repo.root().join(".git").display()),
    )
    .unwrap();
    lab.pending("git-alias", "bob/alias\n");
    let before = files(&lab.home);
    let output = lab.doctor("http://127.0.0.1:1", &formats()[0]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert_unchanged(&lab.home, &before);
}

#[test]
fn scoped_doctor_detects_legacy_checkout_recovery_without_a_journal() {
    let lab = Lab::new();
    let repo = lab.repo("alice/chosen");
    let mut snapshot = Meta::new_file_line();
    snapshot.layout = meta::LayoutVersion::V0;
    meta::write(repo.root(), &snapshot).unwrap();
    repo.add_all().unwrap();
    repo.commit("legacy layout").unwrap();
    let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
    snapshot.layout = meta::LayoutVersion::V1;
    meta::write(repo.root(), &snapshot).unwrap();
    repo.add_all().unwrap();
    repo.commit(meta::STORAGE_MIGRATION_MESSAGE).unwrap();
    repo.git(&["update-ref", "refs/agit/layout-v0/main", &old])
        .unwrap();
    repo.git(&["checkout", &old, "--", meta::FILE]).unwrap();
    let before = files(&lab.home);
    let output = lab.doctor("http://127.0.0.1:1", &formats()[0]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert_unchanged(&lab.home, &before);
}

#[cfg(unix)]
#[test]
fn scoped_doctor_preserves_alias_recovery_and_ignores_unreadable_other_owner() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let lab = Lab::new();
    let repo = lab.repo("alice/chosen");
    let other_owner = lab.home.join("repos/unreadable");
    fs::create_dir_all(&other_owner).unwrap();
    fs::set_permissions(&other_owner, fs::Permissions::from_mode(0o0)).unwrap();
    let output = lab.doctor("http://127.0.0.1:1", &formats()[0]);
    fs::set_permissions(&other_owner, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let alias = lab.home.join("repos/aliases/visible");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    symlink(repo.root(), &alias).unwrap();
    lab.pending("alias", "aliases/visible\n");
    let before = files(&lab.home);
    let output = lab.doctor("http://127.0.0.1:1", &formats()[0]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert_unchanged(&lab.home, &before);
}
