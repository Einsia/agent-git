#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

#[test]
fn doctor_repository_scope_filters_complete_claim_identity_before_native_reads() {
    use std::os::unix::fs::PermissionsExt as _;

    let lab = Lab::new(false);
    for slug in ["bob/qa", "alice/other"] {
        let destination = lab.store.join("repos").join(slug);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        lab.git(
            &lab.work,
            &[
                "clone",
                "--no-hardlinks",
                "--branch",
                "work",
                lab.repo().to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
    }
    let original: Value = serde_json::from_slice(&fs::read(&lab.link).unwrap()).unwrap();
    for (index, owner, agent, superseded) in [
        (2, Some("bob"), "qa", false),
        (3, None, "qa", false),
        (4, Some(""), "qa", false),
        (5, Some("alice"), "other", false),
        (6, Some("alice"), "qa", true),
    ] {
        let mut claim = original.clone();
        claim["owner"] = owner.map(Value::from).unwrap_or(Value::Null);
        claim["agent"] = Value::from(agent);
        if superseded {
            claim["superseded_by"] = Value::from(format!("claude-code/{SID}"));
        }
        fs::write(
            lab.link
                .parent()
                .unwrap()
                .join(format!("aaaaaaaa-0000-4000-8000-{index:012}.json")),
            serde_json::to_vec(&claim).unwrap(),
        )
        .unwrap();
        if matches!(index, 2 | 5) {
            let live = lab
                .live
                .parent()
                .unwrap()
                .join(format!("aaaaaaaa-0000-4000-8000-{index:012}.jsonl"));
            fs::write(&live, b"synthetic unreadable transcript\n").unwrap();
            fs::set_permissions(live, fs::Permissions::from_mode(0o0)).unwrap();
        }
    }
    let before = fs::read(&lab.link).unwrap();
    for args in [
        vec!["doctor", "--repo", "alice/qa"],
        vec!["--json", "doctor", "--repo", "alice/qa"],
    ] {
        let output = lab.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("alice/qa@work"), "{text}");
        assert!(text.contains("checked 1 live transcripts"), "{text}");
        for excluded in ["bob/qa", "alice/other", "unknown-owner", ": unavailable"] {
            assert!(!text.contains(excluded), "{text}");
        }
        assert_eq!(fs::read(&lab.link).unwrap(), before);
    }
    let global = lab.run(&["doctor"]);
    assert!(
        global.contains("bob/qa@work") && global.contains("alice/other@work"),
        "{global}"
    );
    assert!(global.contains(": unavailable"), "{global}");

    fs::write(&lab.live, b"{\"type\":").unwrap();
    let selected = lab.run(&["doctor", "--repo", "alice/qa"]);
    assert!(
        selected.contains("alice/qa@work") && selected.contains(": unavailable"),
        "{selected}"
    );
    assert!(!selected.contains("bob/qa"), "{selected}");
}

struct Lab {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    live: PathBuf,
    link: PathBuf,
}

impl Lab {
    fn new(protected: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = dir.path().join("agit");
        let work = dir.path().join("work");
        let project = home.join(".claude/projects/fixture");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(store.join("credentials")).unwrap();
        fs::write(
            store.join("credentials/127.0.0.1_1.json"),
            json!({
                "username": "alice", "email": null,
                "hub": "http://127.0.0.1:1", "access_token": "synthetic",
                "access_expires_at": "2099-01-01T00:00:00Z",
                "refresh_token": "synthetic", "refresh_expires_at": "2099-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        let lab = Self {
            live: project.join(format!("{SID}.jsonl")),
            link: store.join(format!("store/claude-code/{SID}.json")),
            _dir: dir,
            home,
            store,
            work,
        };
        lab.run(&["config", "secrets.keystore", "file"]);
        if protected {
            let mut child = lab
                .command()
                .args(["secrets", "add", "fixture", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"synthetic prompt")
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        lab.run(&["init", "qa"]);
        fs::write(&lab.live, format!("{}{}", lab.turn(1), lab.turn(2))).unwrap();
        lab.run(&["import", SID, "--into", "alice/qa@work", "--independent"]);
        lab
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        self.configure(&mut cmd);
        cmd
    }

    fn configure(&self, cmd: &mut Command) {
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("CI", "1")
            .env("AGIT_YES", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work);
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        self.configure(&mut cmd);
        let out = cmd.arg("-C").arg(repo).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn repo(&self) -> PathBuf {
        self.store.join("repos/alice/qa")
    }

    fn turn(&self, n: usize) -> String {
        [
            json!({"type":"user", "sessionId":SID, "cwd":self.work,
                "uuid":format!("u{n}"), "message":{"role":"user", "content":format!("synthetic prompt {n}")}}),
            json!({"type":"assistant", "sessionId":SID, "cwd":self.work,
                "uuid":format!("a{n}"), "message":{"role":"assistant", "content":[{"type":"text", "text":format!("synthetic answer {n}")}]}}),
        ].iter().map(|v| format!("{v}\n")).collect()
    }

    fn claim(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.link).unwrap()).unwrap()
    }
    fn write_claim(&self, claim: &Value) {
        fs::write(&self.link, claim.to_string()).unwrap();
    }

    fn comparison(&self) -> String {
        let out = self.run(&["doctor"]);
        out.split_once("live transcript comparison")
            .unwrap()
            .1
            .split("environment")
            .next()
            .unwrap()
            .to_string()
    }

    fn assert_status(&self, status: &str) -> String {
        let out = self.comparison();
        assert!(out.contains(&format!("): {status} —")), "{out}");
        out
    }
}

#[test]
fn doctor_refuses_malformed_claim_repository_identity_before_resolving_paths() {
    let lab = Lab::new(false);
    let original = lab.claim();
    let outside = lab.store.join("outside");
    lab.git(
        &lab.work,
        &[
            "clone",
            "--no-hardlinks",
            "--branch",
            "work",
            lab.repo().to_str().unwrap(),
            outside.to_str().unwrap(),
        ],
    );
    lab.assert_status("clean");
    let outside_refs = lab.git(&outside, &["show-ref"]);
    for (owner, agent) in [
        ("..", "outside"),
        ("alice", "../outside"),
        (" alice", "qa"),
        ("alice ", "qa"),
        ("alice", "qa "),
        ("alice/nested", "qa"),
        ("alice", "nested/qa"),
        ("alice\\nested", "qa"),
        ("alice", "nested\\qa"),
        ("", "qa"),
        ("alice", ""),
    ] {
        let mut claim = original.clone();
        claim["owner"] = Value::from(owner);
        claim["agent"] = Value::from(agent);
        lab.write_claim(&claim);
        let bytes = fs::read(&lab.link).unwrap();
        let result = lab.assert_status("unavailable");
        assert!(
            result.contains("claim has invalid repository identity"),
            "{owner:?}/{agent:?}: {result}"
        );
        assert_eq!(fs::read(&lab.link).unwrap(), bytes);
    }
    assert_eq!(lab.git(&outside, &["show-ref"]), outside_refs);
}

#[test]
fn doctor_compares_exact_owner_and_committed_branch_without_changing_claims() {
    let lab = Lab::new(false);
    let bob = lab.store.join("repos/bob/qa");
    fs::create_dir_all(bob.parent().unwrap()).unwrap();
    lab.git(
        &lab.work,
        &[
            "clone",
            "--quiet",
            lab.repo().to_str().unwrap(),
            bob.to_str().unwrap(),
        ],
    );
    lab.git(&bob, &["update-ref", "refs/heads/work", "refs/heads/main"]);
    let session_worktree = PathBuf::from(
        lab.git(&lab.repo(), &["worktree", "list", "--porcelain"])
            .split("\n\n")
            .find(|block| block.contains("branch refs/heads/work"))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .strip_prefix("worktree ")
            .unwrap(),
    );
    fs::write(
        session_worktree.join("LOG"),
        "dirty and invalid working content\n",
    )
    .unwrap();
    let refs = lab.git(&lab.repo(), &["show-ref"]);
    let claim = fs::read(&lab.link).unwrap();
    let out = lab.assert_status("clean");
    assert!(out.contains("alice/qa@work"), "{out}");
    assert!(!out.contains("bob/qa"), "{out}");
    assert!(out.contains("checked 1 live transcripts"), "{out}");
    assert_eq!(lab.git(&lab.repo(), &["show-ref"]), refs);
    assert_eq!(fs::read(&lab.link).unwrap(), claim);
    assert_eq!(
        fs::read_to_string(session_worktree.join("LOG")).unwrap(),
        "dirty and invalid working content\n"
    );
}

#[test]
fn doctor_reports_missing_claim_evidence_without_falling_back_to_other_refs() {
    let lab = Lab::new(false);
    let original = lab.claim();
    for field in ["owner", "agent", "branch"] {
        let mut claim = original.clone();
        claim.as_object_mut().unwrap().remove(field);
        lab.write_claim(&claim);
        let out = lab.assert_status("unavailable");
        assert!(out.contains("0 live transcripts"), "{out}");
    }
    let mut claim = original.clone();
    claim["branch"] = json!("absent");
    lab.git(&lab.repo(), &["tag", "absent", "work"]);
    lab.write_claim(&claim);
    assert!(
        lab.assert_status("unavailable")
            .contains("claimed local branch cannot be read")
    );
    lab.write_claim(&original);
    fs::rename(&lab.live, lab.live.with_extension("saved")).unwrap();
    assert!(
        lab.assert_status("unavailable")
            .contains("live transcript cannot be read")
    );
    fs::rename(lab.live.with_extension("saved"), &lab.live).unwrap();
    fs::rename(lab.store.join("repos"), lab.store.join("repos-saved")).unwrap();
    assert!(
        lab.assert_status("unavailable")
            .contains("claimed repository is not available locally")
    );
}

#[test]
fn doctor_distinguishes_native_growth_truncation_rewrite_and_incomplete_evidence() {
    let lab = Lab::new(false);
    let original = fs::read_to_string(&lab.live).unwrap();
    lab.assert_status("clean");
    fs::write(&lab.live, format!("{original}{}", lab.turn(3))).unwrap();
    lab.assert_status("appended");
    fs::write(&lab.live, lab.turn(1)).unwrap();
    lab.assert_status("truncated");
    fs::write(
        &lab.live,
        original.replace("synthetic prompt 1", "different prompt"),
    )
    .unwrap();
    lab.assert_status("rewritten");
    fs::write(&lab.live, format!("{original}{{\"unfinished\":")).unwrap();
    assert!(
        lab.assert_status("unavailable")
            .contains("unfinished record")
    );
    fs::write(&lab.live, format!("{original}invalid record\n")).unwrap();
    assert!(
        lab.assert_status("unavailable")
            .contains("malformed record")
    );
}

#[test]
fn doctor_checks_real_cross_runtime_materialization_against_its_own_baseline() {
    let lab = Lab::new(false);
    lab.run(&["resume", "alice/qa@work", "--no-launch", "--as", "codex"]);
    let codex_link = fs::read_dir(lab.store.join("store/codex"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let original_claim: Value = serde_json::from_slice(&fs::read(&codex_link).unwrap()).unwrap();
    let id = codex_link.file_stem().unwrap().to_str().unwrap();
    let live = walkdir::WalkDir::new(lab.home.join(".codex/sessions"))
        .into_iter()
        .map(Result::unwrap)
        .find(|e| e.file_type().is_file() && e.file_name().to_string_lossy().contains(id))
        .unwrap()
        .into_path();
    let original = fs::read(&live).unwrap();
    let out = lab.assert_status("clean");
    assert!(out.contains("codex"), "{out}");
    assert!(!out.contains("claude-code"), "{out}");
    assert!(out.contains("checked 1 live transcripts"), "{out}");
    let mut appended = original.clone();
    appended.extend_from_slice(b"{\"appended\":true}\n");
    fs::write(&live, appended).unwrap();
    lab.assert_status("appended");
    fs::write(&live, &original[..original.len() - 1]).unwrap();
    lab.assert_status("truncated");
    let mut rewritten = original.clone();
    rewritten[0] ^= 1;
    fs::write(&live, rewritten).unwrap();
    lab.assert_status("rewritten");
    fs::write(&live, &original).unwrap();
    let mut claim = original_claim.clone();
    claim.as_object_mut().unwrap().remove("baseline_hash");
    fs::write(&codex_link, claim.to_string()).unwrap();
    assert!(
        lab.assert_status("unavailable")
            .contains("baseline digest is missing")
    );
    let mut claim = original_claim.clone();
    claim.as_object_mut().unwrap().remove("materialized_from");
    fs::write(&codex_link, claim.to_string()).unwrap();
    assert!(
        lab.assert_status("clean")
            .contains("branch-tip evidence is unavailable")
    );
    fs::write(&codex_link, original_claim.to_string()).unwrap();
    let next = lab.git(&lab.repo(), &["rev-parse", "refs/heads/main"]);
    let mut claim = original_claim.clone();
    claim["materialized_from"] = json!(next);
    fs::write(&codex_link, claim.to_string()).unwrap();
    assert!(
        lab.assert_status("clean")
            .contains("tip differs from the current branch tip")
    );
}

#[test]
fn doctor_hydrates_existing_secrets_without_mutating_the_dictionary() {
    let lab = Lab::new(true);
    let dictionary = lab.repo().join(".git/agit/secret-dictionary/vault.json");
    let lock = dictionary.parent().unwrap().join("vault.lock");
    let before = fs::read(&dictionary).unwrap();
    let modified = fs::metadata(&dictionary).unwrap().modified().unwrap();
    let claim = fs::read(&lab.link).unwrap();
    fs::remove_file(&lock).unwrap();
    lab.assert_status("clean");
    assert_eq!(fs::read(&dictionary).unwrap(), before);
    assert_eq!(
        fs::metadata(&dictionary).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(fs::read(&lab.link).unwrap(), claim);
    assert!(!lock.exists());
    fs::rename(lab.store.join("keystore"), lab.store.join("keystore-saved")).unwrap();
    assert!(
        lab.assert_status("unavailable")
            .contains("repository secret reconstruction is unavailable")
    );
    assert_eq!(fs::read(&dictionary).unwrap(), before);
    assert!(!lock.exists());
}

#[test]
fn doctor_handles_later_secret_mappings_and_missing_reconstruction_evidence() {
    let lab = Lab::new(false);
    let original = fs::read(&lab.live).unwrap();
    let mut child = lab
        .command()
        .args(["secrets", "add", "later-rule", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic prompt")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let later = "bbbbbbbb-0000-4000-8000-000000000002";
    let later_path = lab.live.parent().unwrap().join(format!("{later}.jsonl"));
    fs::write(&later_path, lab.turn(1).replace(SID, later)).unwrap();
    lab.run(&[
        "import",
        later,
        "--into",
        "alice/qa@after-mapping",
        "--independent",
    ]);
    assert_eq!(fs::read(&lab.live).unwrap(), original);
    let dictionary = lab.repo().join(".git/agit/secret-dictionary/vault.json");
    let before = fs::read(&dictionary).unwrap();
    let modified = fs::metadata(&dictionary).unwrap().modified().unwrap();
    let out = lab.comparison();
    for branch in ["work", "after-mapping"] {
        let row = out
            .lines()
            .find(|line| line.contains(&format!("alice/qa@{branch} (")))
            .unwrap();
        assert!(row.contains("): clean —"), "{out}");
    }
    assert_eq!(fs::read(&dictionary).unwrap(), before);
    assert_eq!(
        fs::metadata(&dictionary).unwrap().modified().unwrap(),
        modified
    );
    let backup = dictionary.with_extension("saved");
    fs::rename(&dictionary, &backup).unwrap();
    let out = lab.comparison();
    let old = out
        .lines()
        .find(|line| line.contains("alice/qa@work ("))
        .unwrap();
    assert!(old.contains("): clean —"), "{out}");
    let protected = out
        .lines()
        .find(|line| line.contains("alice/qa@after-mapping ("))
        .unwrap();
    assert!(protected.contains("): unavailable —"), "{out}");
    assert!(
        protected.contains("secret mappings needed for comparison are unavailable"),
        "{out}"
    );
    assert!(!dictionary.exists());
    fs::rename(&backup, &dictionary).unwrap();
    fs::write(
        &lab.live,
        String::from_utf8(original)
            .unwrap()
            .replace("synthetic prompt", "different prompt"),
    )
    .unwrap();
    lab.assert_status("rewritten");
}
