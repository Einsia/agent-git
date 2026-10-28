//! Recovery text retains the selected repository and survives native shell argument parsing.

use agit::domain::meta;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Hub {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<usize>>,
}

impl Hub {
    fn new(source: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = serde_json::json!({
            "agent_id":"aaaaaaaa-0000-4000-8000-000000000090",
            "owner":"alice", "name":"notes", "visibility":"public", "clone_url":source,
        })
        .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut requests = 0;
            while !stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("metadata accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 8192, "metadata request too large");
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                if request.starts_with("GET /api/cli/version ") {
                    write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                assert!(request.starts_with("GET /api/agents/alice/notes HTTP/1.1\r\n"));
                requests += 1;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        Self {
            url,
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
    work: PathBuf,
    source: PathBuf,
    hub: Hub,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let work = root.path().join("work");
        let source = root.path().join("source");
        fs::create_dir_all(&work).unwrap();
        Self {
            hub: Hub::new(&source),
            root,
            home,
            work,
            source,
        }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, session: Option<&str>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for key in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "PATHEXT",
            "TEMP",
            "TMP",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .current_dir(&self.work)
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub.url)
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Recovery fixture")
            .env("GIT_AUTHOR_EMAIL", "recovery@example.invalid")
            .env("GIT_COMMITTER_NAME", "Recovery fixture")
            .env("GIT_COMMITTER_EMAIL", "recovery@example.invalid")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .stdin(Stdio::null());
        if let Some(session) = session {
            command.env("AGIT_SESSION", session);
        }
        command
    }

    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let output = self
            .command("git", None)
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn shell(&self, text: &str, session: Option<&str>) -> Output {
        #[cfg(windows)]
        let mut command = {
            let mut command = self.command("powershell.exe", session);
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("{text}; exit $LASTEXITCODE"),
            ]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = self.command("sh", session);
            command.args(["-c", text]);
            command
        };
        let mut paths = vec![
            Path::new(env!("CARGO_BIN_EXE_agit"))
                .parent()
                .unwrap()
                .to_path_buf(),
        ];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap()
    }

    fn file_checkout(&self) -> PathBuf {
        fs::create_dir_all(&self.source).unwrap();
        self.git(&self.source, &["init", "--quiet", "--initial-branch=main"]);
        self.git(
            &self.source,
            &["config", "receive.denyCurrentBranch", "updateInstead"],
        );
        meta::write(&self.source, &meta::Meta::new_file_line()).unwrap();
        fs::write(self.source.join("page.md"), "Title\n\nMiddle\n\nEnd\n").unwrap();
        self.git(&self.source, &["add", "-A"]);
        self.git(&self.source, &["commit", "-qm", "Base files"]);
        let selected = self.home.join("repos/alice/notes");
        fs::create_dir_all(selected.parent().unwrap()).unwrap();
        let output = self
            .command("git", None)
            .args(["clone", "--quiet"])
            .arg(&self.source)
            .arg(&selected)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        selected
    }

    fn agit(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_agit"), None)
            .args(args)
            .output()
            .unwrap()
    }
}

/// File pulls preserve both parents and reconcile ordinary content without a native runtime.
#[test]
fn file_pull_reconciles_disjoint_changes_and_can_publish_the_result() {
    let lab = Lab::new();
    let local = lab.file_checkout();
    fs::write(local.join("local.md"), "Local page\n").unwrap();
    fs::write(local.join("page.md"), "Local title\n\nMiddle\n\nEnd\n").unwrap();
    lab.git(&local, &["add", "-A"]);
    lab.git(&local, &["commit", "-qm", "Local changes"]);
    let left = lab.git(&local, &["rev-parse", "HEAD"]);
    fs::write(lab.source.join("remote.md"), "Remote page\n").unwrap();
    fs::write(
        lab.source.join("page.md"),
        "Title\n\nMiddle\n\nRemote end\n",
    )
    .unwrap();
    lab.git(&lab.source, &["add", "-A"]);
    lab.git(&lab.source, &["commit", "-qm", "Remote changes"]);
    let right = lab.git(&lab.source, &["rev-parse", "HEAD"]);
    let output = lab.agit(&["pull", "alice/notes@main"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("after the next resume"),
        "{output:?}"
    );
    assert_eq!(lab.git(&local, &["rev-parse", "HEAD^1"]), left);
    assert_eq!(lab.git(&local, &["rev-parse", "HEAD^2"]), right);
    assert_eq!(
        fs::read_to_string(local.join("page.md")).unwrap(),
        "Local title\n\nMiddle\n\nRemote end\n"
    );
    for name in ["local.md", "remote.md"] {
        assert!(local.join(name).is_file());
    }
    assert!(meta::read(&local).unwrap().is_file_line());
    assert!(!local.join(".git/AGIT_MERGE_TX").exists());
    assert!(!lab.home.join("store/codex").exists());
    lab.git(&local, &["push", "origin", "main"]);
    assert_eq!(
        lab.git(&local, &["rev-parse", "HEAD"]),
        lab.git(&lab.source, &["rev-parse", "HEAD"])
    );
}

/// Conflict recovery is a persistent manual transaction; refusing dirty input cannot consume it.
#[test]
fn file_pull_preserves_dirty_input_and_conflicts_can_be_completed_or_aborted() {
    let lab = Lab::new();
    let local = lab.file_checkout();
    fs::write(local.join("page.md"), "Local text\n").unwrap();
    lab.git(&local, &["add", "-A"]);
    lab.git(&local, &["commit", "-qm", "Local text"]);
    let left = lab.git(&local, &["rev-parse", "HEAD"]);
    lab.git(&lab.source, &["rm", "page.md"]);
    lab.git(&lab.source, &["commit", "-qm", "Remote deletion"]);
    fs::write(local.join("draft.md"), "Uncommitted draft\n").unwrap();
    lab.git(&local, &["add", "draft.md"]);
    let index = fs::read(local.join(".git/index")).unwrap();
    let refused = lab.agit(&["pull", "alice/notes@main"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("uncommitted or staged"),
        "{refused:?}"
    );
    assert_eq!(fs::read(local.join(".git/index")).unwrap(), index);
    assert!(!local.join(".git/AGIT_MERGE_TX").exists());
    lab.git(&local, &["reset", "--", "draft.md"]);
    fs::remove_file(local.join("draft.md")).unwrap();
    let conflicted = lab.agit(&["pull", "alice/notes@main"]);
    assert!(!conflicted.status.success(), "{conflicted:?}");
    let text = String::from_utf8_lossy(&conflicted.stderr);
    assert!(
        text.contains("--continue --resolved page.md"),
        "{conflicted:?}"
    );
    assert_eq!(lab.git(&local, &["rev-parse", "HEAD"]), left);
    let tx = agit::domain::mergetx::read(&local).unwrap().unwrap();
    assert_eq!(tx.mode, Some(agit::domain::mergetx::Mode::Manual));
    assert_eq!(tx.exploration, None);
    let aborted = lab.agit(&["merge", "--into", "alice/notes@main", "--abort"]);
    assert!(aborted.status.success(), "{aborted:?}");
    assert_eq!(lab.git(&local, &["rev-parse", "HEAD"]), left);
    assert_eq!(
        fs::read_to_string(local.join("page.md")).unwrap(),
        "Local text\n"
    );
    let conflicted = lab.agit(&["pull", "alice/notes@main"]);
    assert!(!conflicted.status.success(), "{conflicted:?}");
    fs::write(local.join("page.md"), "Resolved text\n").unwrap();
    let completed = lab.agit(&[
        "merge",
        "--into",
        "alice/notes@main",
        "--continue",
        "--resolved",
        "page.md",
    ]);
    assert!(completed.status.success(), "{completed:?}");
    assert_eq!(
        fs::read_to_string(local.join("page.md")).unwrap(),
        "Resolved text\n"
    );
    assert!(agit::domain::mergetx::read(&local).unwrap().is_none());
}

#[test]
fn divergence_hints_keep_the_explicit_repo_without_or_with_foreign_process_identity() {
    for branch in ["topic", "topic;literal'quoted‘’‚‛"] {
        for session in [None, Some("bob/other@topic")] {
            let mut lab = Lab::new();
            fs::create_dir_all(&lab.source).unwrap();
            lab.git(&lab.source, &["init", "--quiet", "--initial-branch=main"]);
            let snapshot = meta::Meta::new(
                "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                "codex".into(),
                lab.work.to_string_lossy().into_owned(),
            );
            meta::write(&lab.source, &snapshot).unwrap();
            fs::write(lab.source.join(meta::LOG_FILE), "").unwrap();
            fs::write(lab.source.join(meta::VIEW_FILE), "").unwrap();
            fs::write(lab.source.join("AGENTS.md"), "Shared context.\n").unwrap();
            lab.git(&lab.source, &["add", "-A"]);
            lab.git(&lab.source, &["commit", "-qm", "shared base"]);
            let base = lab.git(&lab.source, &["rev-parse", "HEAD"]);
            lab.git(&lab.source, &["checkout", "-qb", branch]);
            lab.git(
                &lab.source,
                &["commit", "--allow-empty", "-qm", "remote advance"],
            );
            let remote = lab.git(&lab.source, &["rev-parse", "HEAD"]);
            let selected = lab.home.join("repos/alice/notes");
            fs::create_dir_all(selected.parent().unwrap()).unwrap();
            let output = lab
                .command("git", None)
                .args(["clone", "--quiet"])
                .arg(&lab.source)
                .arg(&selected)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            lab.git(&selected, &["reset", "--hard", &base]);
            lab.git(
                &selected,
                &["commit", "--allow-empty", "-qm", "local advance"],
            );
            let local = lab.git(&selected, &["rev-parse", "HEAD"]);
            let foreign = lab.home.join("repos/bob/other");
            fs::create_dir_all(&foreign).unwrap();
            lab.git(&foreign, &["init", "--quiet", "--initial-branch=topic"]);
            fs::write(foreign.join("untouched"), "foreign identity\n").unwrap();
            lab.git(&foreign, &["add", "-A"]);
            lab.git(&foreign, &["commit", "-qm", "foreign base"]);
            let foreign_refs = lab.git(&foreign, &["show-ref"]);
            let output = lab
                .command(env!("CARGO_BIN_EXE_agit"), session)
                .args(["pull", "alice/notes", "-b", branch])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            let recovery: Vec<_> = diagnostic
                .lines()
                .filter(|line| line.contains("reconcile:") || line.contains("keep both:"))
                .map(|line| line.split('`').nth(1).unwrap())
                .collect();
            assert_eq!(recovery.len(), 2, "{diagnostic}");
            let merge = lab.shell(&format!("{} --dry-run", recovery[0]), session);
            assert!(merge.status.success(), "{}: {merge:?}", recovery[0]);
            let report = String::from_utf8_lossy(&merge.stdout);
            assert!(report.contains(&base[..9]), "{report}");
            let fork = recovery[1].strip_suffix(" --resume").unwrap();
            let fork_output = lab.shell(fork, session);
            assert!(fork_output.status.success(), "{fork}: {fork_output:?}");
            assert_eq!(
                lab.git(
                    &selected,
                    &["rev-parse", &format!("refs/heads/{branch}-remote^")]
                ),
                remote
            );
            assert_eq!(
                lab.git(&selected, &["rev-parse", &format!("refs/heads/{branch}")]),
                local
            );
            assert_eq!(lab.git(&foreign, &["show-ref"]), foreign_refs);
            assert!(!lab.home.join("store/codex").exists());
            lab.hub.stop.store(true, Ordering::Release);
            assert_eq!(lab.hub.worker.take().unwrap().join().unwrap(), 1);
        }
    }
}

/// A store whose loose objects a hard stop left empty is named rather than passed through: doctor
/// names the damaged branches and the repair, a pull refuses to commit on top of the damaged
/// history, and the repair refetches what the Hub has, names the commit only this machine had and
/// sets that branch aside. A repair that ran a plain fetch would leave the published object
/// unreadable, since this store claims the commits that reach it. A repair that stops early (here
/// the set-aside is declined) has already removed the empty files, so only its record lets doctor
/// still find the branch that cannot read; and a readable tip is kept under a ref named by that
/// tip before its branch goes, so when that ref cannot be written the branch stays. The damage is
/// settled only when no other ref reaches it: a tag on the set-aside history keeps the record and
/// is named.
#[test]
fn damaged_objects_are_named_refused_and_restored_from_the_hub() {
    let lab = Lab::new();
    let local = lab.file_checkout();
    fs::write(local.join("published.md"), "On the Hub\n").unwrap();
    lab.git(&local, &["add", "published.md"]);
    lab.git(&local, &["commit", "-qm", "Published page"]);
    lab.git(&local, &["push", "-q", "origin", "main"]);
    lab.git(&local, &["switch", "-qc", "draft"]);
    fs::write(local.join("draft.md"), "Only here\n").unwrap();
    lab.git(&local, &["add", "draft.md"]);
    lab.git(&local, &["commit", "-qm", "Draft only on this machine"]);
    lab.git(&local, &["switch", "-q", "main"]);
    fs::write(local.join("note.md"), "Local note\n").unwrap();
    lab.git(&local, &["add", "note.md"]);
    lab.git(&local, &["commit", "-qm", "Local note"]);
    let published = lab.git(&local, &["rev-parse", "main:published.md"]);
    let draft = lab.git(&local, &["rev-parse", "draft:draft.md"]);
    for object in [&published, &draft] {
        empty_object(&local, object);
    }

    let doctor = lab.agit(&["doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        report.contains("2 damaged Git objects")
            && report.contains("branches that reach them: draft, main")
            && report.contains("agit doctor --repair-objects alice/notes"),
        "{doctor:?}"
    );

    fs::write(lab.source.join("remote.md"), "Remote page\n").unwrap();
    lab.git(&lab.source, &["add", "remote.md"]);
    lab.git(&lab.source, &["commit", "-qm", "Remote page"]);
    let before = lab.git(&local, &["rev-parse", "main"]);
    let pull = lab.agit(&["pull", "alice/notes@main"]);
    assert!(!pull.status.success(), "{pull:?}");
    assert!(
        String::from_utf8_lossy(&pull.stderr).contains("agit doctor --repair-objects alice/notes"),
        "{pull:?}"
    );
    assert_eq!(lab.git(&local, &["rev-parse", "main"]), before);

    let draft_tip = lab.git(&local, &["rev-parse", "draft"]);
    let declined = lab.agit(&["doctor", "--repair-objects", "alice/notes"]);
    assert!(!declined.status.success(), "{declined:?}");
    assert!(
        String::from_utf8_lossy(&declined.stdout).contains("Draft only on this machine"),
        "{declined:?}"
    );
    assert_eq!(
        lab.git(&local, &["cat-file", "-p", &published]),
        "On the Hub"
    );
    let doctor = lab.agit(&["doctor"]);
    assert!(
        String::from_utf8_lossy(&doctor.stdout).contains("branches that reach them: draft;"),
        "{doctor:?}"
    );

    let colliding = format!("refs/agit/damaged/draft-{}", &draft_tip[..12]);
    let main_tip = lab.git(&local, &["rev-parse", "main"]);
    lab.git(&local, &["update-ref", &colliding, &main_tip]);
    let blocked = lab.agit(&["--yes", "doctor", "--repair-objects", "alice/notes"]);
    assert!(!blocked.status.success(), "{blocked:?}");
    assert_eq!(
        lab.git(&local, &["rev-parse", "refs/heads/draft"]),
        draft_tip
    );
    lab.git(&local, &["update-ref", "-d", &colliding]);

    lab.git(&local, &["tag", "keep-draft", &draft_tip]);
    let held = lab.agit(&["--yes", "doctor", "--repair-objects", "alice/notes"]);
    assert!(!held.status.success(), "{held:?}");
    assert!(
        String::from_utf8_lossy(&held.stderr).contains("refs/tags/keep-draft"),
        "{held:?}"
    );
    assert_eq!(
        lab.git(
            &local,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/agit/damaged/"
            ]
        ),
        format!("refs/agit/damaged/draft-{} {draft_tip}", &draft_tip[..12])
    );
    let doctor = lab.agit(&["doctor"]);
    assert!(
        String::from_utf8_lossy(&doctor.stdout).contains("damaged Git object"),
        "{doctor:?}"
    );
    lab.git(&local, &["tag", "-d", "keep-draft"]);
    let repair = lab.agit(&["--yes", "doctor", "--repair-objects", "alice/notes"]);
    assert!(repair.status.success(), "{repair:?}");
    let doctor = lab.agit(&["doctor"]);
    assert!(
        !String::from_utf8_lossy(&doctor.stdout).contains("damaged Git object"),
        "{doctor:?}"
    );
    let pull = lab.agit(&["pull", "alice/notes@main"]);
    assert!(pull.status.success(), "{pull:?}");
}

/// Leave `oid`'s loose file in place with no content, as a crash before the data reached the disk
/// does.
fn empty_object(repo: &Path, oid: &str) {
    let file = repo.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    assert!(file.is_file(), "{oid} is not a loose object");
    let mut permissions = fs::metadata(&file).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&file, permissions).unwrap();
    fs::File::create(&file).unwrap();
}
