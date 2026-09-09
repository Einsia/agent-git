use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use agit::domain::{link, store::Store};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs;
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
            if cfg!(windows) {
                "{\"secrets.keystore\":\"os\"}\n"
            } else {
                "{\"secrets.keystore\":\"file\"}\n"
            },
        )
        .unwrap();
        fs::write(home.join("layout-v1.complete"), b"1\n").unwrap();
        Self {
            _root: root,
            home,
            work,
        }
    }

    fn repo(&self) -> Repo {
        let repo = Repo::init(&self.home.join("repos/alice/fixture")).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("declare file line").unwrap();
        repo
    }

    fn doctor_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .arg("doctor")
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self._root.path())
            .env("USERPROFILE", self._root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env("CI", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self._root.path().join("absent-gitconfig"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .current_dir(&self.work);
        #[cfg(windows)]
        command.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn doctor(&self, args: &[&str]) -> Output {
        let output = self.doctor_command(args).output().unwrap();
        if args.contains(&"--json") {
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let version = args
                .windows(2)
                .find(|pair| pair[0] == "--json-version")
                .map(|pair| pair[1].parse::<u64>().unwrap())
                .unwrap_or(2);
            assert_eq!(document["schema"], "cli-output");
            assert_eq!(document["schema_version"], version);
            assert_eq!(document["command"], "doctor");
            assert_eq!(document["exit_code"], output.status.code().unwrap());
            assert_eq!(document["ok"], output.status.success());
            assert_eq!(document.get("fix").is_some(), version == 2);
        }
        output
    }

    fn healthy_claim(&self, repo: &Repo) {
        repo.git(&["checkout", "-b", "work", "main"]).unwrap();
        meta::write(
            repo.root(),
            &Meta::new(
                format!("agit-{}", "a".repeat(40)),
                "claude-code".into(),
                self.work.to_string_lossy().into_owned(),
            ),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("record synthetic session").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["checkout", "main"]).unwrap();
        let native = self
            ._root
            .path()
            .join(".claude/projects")
            .join(agit::adapter::claude_code::slug_for(&self.work));
        fs::create_dir_all(&native).unwrap();
        let content = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Synthetic baseline\"}}\n";
        fs::write(native.join("healthy.jsonl"), content).unwrap();
        let mut claim = link::Link::new("claude-code", "healthy", Some(&self.work));
        claim.owner = Some("alice".into());
        claim.agent = Some("fixture".into());
        claim.branch = Some("work".into());
        claim.baseline_bytes = Some(content.len() as u64);
        claim.baseline_hash = Some(hex::encode(Sha256::digest(content)));
        claim.materialized_from = Some(head);
        link::write(&Store::at(self.home.join("store")), &claim).unwrap();
    }
}

fn report(output: &Output) -> String {
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(lines) = json["result"]["lines"].as_array()
    {
        return lines
            .iter()
            .map(|line| line.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
    }
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
        .filter(|entry| !entry.path().ends_with("secret-filter/vault.lock"))
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn assert_unchanged(root: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) {
    let after = files(root);
    let changed: std::collections::BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(changed.is_empty(), "changed fixture paths: {changed:?}");
}

/// A deleted declaration cannot hide behind a healthy checkout or an undeclared branch.
#[test]
fn doctor_reports_missing_metadata_only_on_declared_session_lines() {
    let lab = Lab::new();
    let repo = lab.repo();
    repo.git(&["checkout", "-b", "damaged", "main"]).unwrap();
    meta::write(
        repo.root(),
        &Meta::new_session_line("codex".into(), lab.work.to_string_lossy().into_owned()),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("declare session line").unwrap();
    fs::remove_file(meta::path_in(repo.root())).unwrap();
    repo.add_all().unwrap();
    repo.commit("remove session metadata").unwrap();

    repo.git(&["checkout", "-b", "legacy", "main"]).unwrap();
    fs::remove_file(meta::path_in(repo.root())).unwrap();
    repo.add_all().unwrap();
    repo.commit("undeclared legacy branch").unwrap();
    repo.git(&["checkout", "main"]).unwrap();

    let before = files(&lab.home);
    let output = lab.doctor(&["--repo", "alice/fixture"]);
    assert!(output.status.success(), "{output:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(
            "branch `damaged`: session/meta.json is missing from a declared session line"
        ),
        "{text}"
    );
    assert!(!text.contains("branch `legacy`"), "{text}");
    assert!(!text.contains("branch `main`"), "{text}");
    assert!(!text.contains("session metadata is consistent"), "{text}");
    assert_unchanged(&lab.home, &before);
}

/// Missing historical objects stay unavailable even when a promisor remote permits fetching.
#[test]
fn doctor_does_not_fetch_missing_promisor_metadata() {
    let lab = Lab::new();
    let source = Repo::init(&lab._root.path().join("source")).unwrap();
    meta::write(source.root(), &Meta::new_file_line()).unwrap();
    source.add_all().unwrap();
    source.commit("declare file line").unwrap();
    source.git(&["checkout", "-b", "damaged"]).unwrap();
    meta::write(
        source.root(),
        &Meta::new_session_line("codex".into(), lab.work.to_string_lossy().into_owned()),
    )
    .unwrap();
    fs::write(
        source.root().join("unrelated"),
        b"unrelated historical blob",
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("declare session line").unwrap();
    let metadata = source
        .git(&["rev-parse", &format!("HEAD:{}", meta::FILE)])
        .unwrap();
    let unrelated = source.git(&["rev-parse", "HEAD:unrelated"]).unwrap();
    fs::remove_file(meta::path_in(source.root())).unwrap();
    source.add_all().unwrap();
    source.commit("remove session metadata").unwrap();
    source.git(&["checkout", "main"]).unwrap();
    source
        .git(&["config", "uploadpack.allowFilter", "true"])
        .unwrap();

    let destination = lab.home.join("repos/alice/fixture");
    let cloned = Command::new("git")
        .args(["clone", "--filter=blob:none", "--no-checkout", "--no-local"])
        .arg(source.root())
        .arg(&destination)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_NO_LAZY_FETCH", "0")
        .env(
            "GIT_CONFIG_GLOBAL",
            lab._root.path().join("absent-gitconfig"),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(cloned.status.success(), "{cloned:?}");
    let repo = Repo::at(&destination);
    repo.git(&["checkout", "main"]).unwrap();
    repo.git(&["branch", "damaged", "origin/damaged"]).unwrap();
    let missing = Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["cat-file", "-e", &metadata])
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .unwrap();
    assert!(
        !missing.status.success(),
        "the fixture must omit the metadata object"
    );
    let before = files(&lab.home);
    let trace = lab._root.path().join("doctor.trace");
    let output = lab
        .doctor_command(&["--repo", "alice/fixture"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_NO_LAZY_FETCH", "0")
        .env("GIT_TRACE", &trace)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = report(&output);
    assert!(
        text.contains("branch `damaged`: metadata history is unavailable"),
        "{text}"
    );
    assert_unchanged(&lab.home, &before);
    let trace = fs::read_to_string(trace).unwrap();
    assert!(!trace.contains("upload-pack"), "{trace}");
    assert!(!trace.contains("fetch origin"), "{trace}");

    let control_trace = lab._root.path().join("fetch.trace");
    let fetched = Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["cat-file", "blob", &metadata])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_NO_LAZY_FETCH", "0")
        .env("GIT_TRACE", &control_trace)
        .output()
        .unwrap();
    assert!(fetched.status.success(), "{fetched:?}");
    assert!(
        fs::read_to_string(control_trace)
            .unwrap()
            .contains("upload-pack")
    );
    let missing = Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["cat-file", "-e", &unrelated])
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .unwrap();
    assert!(
        !missing.status.success(),
        "the unrelated blob must remain absent"
    );
    let before = files(&lab.home);
    let output = lab
        .doctor_command(&["--repo", "alice/fixture"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_NO_LAZY_FETCH", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = report(&output);
    assert!(
        text.contains("session/meta.json is missing from a declared session line"),
        "{text}"
    );
    assert!(!text.contains("metadata history is unavailable"), "{text}");
    assert_unchanged(&lab.home, &before);
}

/// Corrupt records retain diagnostic scope without revealing their private fields.
#[test]
fn doctor_reports_corrupt_links_without_misattribution_or_private_values() {
    let lab = Lab::new();
    let repo = lab.repo();
    lab.healthy_claim(&repo);
    let links = lab.home.join("store/codex");
    fs::create_dir_all(&links).unwrap();
    for (file, owner, agent) in [
        ("matching.json", "alice", "fixture"),
        ("unrelated.json", "bob", "other"),
    ] {
        fs::write(
            links.join(file),
            serde_json::to_vec(&serde_json::json!({
                "owner": owner,
                "agent": agent,
                "baseline_bytes": "PRIVATE-LINK-SENTINEL",
                "cwd": "/PRIVATE-LINK-SENTINEL"
            }))
            .unwrap(),
        )
        .unwrap();
    }
    fs::write(links.join("unknown.json"), "{PRIVATE-LINK-SENTINEL").unwrap();
    let before = files(lab._root.path());
    let mut formats = vec![vec!["--repo", "alice/fixture"]];
    if cfg!(any(unix, all(windows, target_env = "msvc"))) {
        formats.push(vec!["--json", "--repo", "alice/fixture"]);
        for version in ["1", "2"] {
            formats.push(vec![
                "--json",
                "--json-version",
                version,
                "--repo",
                "alice/fixture",
            ]);
        }
    }
    for args in formats {
        let output = lab.doctor(&args);
        assert!(output.status.success(), "{output:?}");
        let text = report(&output);
        assert!(
            text.contains("matching.json\": invalid link data"),
            "{text}"
        );
        assert!(text.contains("unknown.json\": invalid link data"), "{text}");
        assert!(text.contains("repository scope unavailable"), "{text}");
        assert!(!text.contains("unrelated.json"), "{text}");
        assert!(!text.contains("PRIVATE-LINK-SENTINEL"), "{text}");
        assert!(text.contains("alice/fixture@work"), "{text}");
        assert!(text.contains(": clean"), "{text}");
        assert!(text.contains("checked 1 live transcripts"), "{text}");
        assert_unchanged(lab._root.path(), &before);
    }
    let global = lab.doctor(&[]);
    assert!(global.status.success(), "{global:?}");
    let text = report(&global);
    assert!(
        text.contains("unrelated.json\": invalid link data"),
        "{text}"
    );
    assert!(!text.contains("PRIVATE-LINK-SENTINEL"), "{text}");
    assert_unchanged(lab._root.path(), &before);
}

/// Filesystem failures remain visible, and filenames cannot inject terminal lines.
#[cfg(unix)]
#[test]
fn doctor_reports_unreadable_link_files_and_directories_with_escaped_paths() {
    use std::os::unix::fs::PermissionsExt as _;

    let lab = Lab::new();
    lab.repo();
    let links = lab.home.join("store/codex");
    fs::create_dir_all(&links).unwrap();
    let path = links.join("blocked\nlink.json");
    fs::write(&path, b"{\"cwd\":\"PRIVATE-UNREADABLE-SENTINEL\"}").unwrap();
    let original = fs::metadata(&path).unwrap().permissions();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    let denied = fs::read(&path).is_err();
    let output = lab.doctor(&["--repo", "alice/fixture"]);
    fs::set_permissions(&path, original).unwrap();
    assert!(output.status.success(), "{output:?}");
    if denied {
        let text = report(&output);
        assert!(
            text.contains("blocked\\nlink.json\": unreadable link file"),
            "{text}"
        );
        assert!(text.contains("repository scope unavailable"), "{text}");
        assert!(!text.contains("blocked\nlink.json"), "{text}");
        assert!(!text.contains("PRIVATE-UNREADABLE-SENTINEL"), "{text}");
    }
    let original = fs::metadata(&links).unwrap().permissions();
    let before = files(lab._root.path());
    fs::set_permissions(&links, fs::Permissions::from_mode(0o0)).unwrap();
    let denied = fs::read_dir(&links).is_err();
    let output = lab.doctor(&["--repo", "alice/fixture"]);
    fs::set_permissions(&links, original).unwrap();
    assert!(output.status.success(), "{output:?}");
    if denied {
        let text = report(&output);
        assert!(
            text.contains("codex\": unreadable link directory"),
            "{text}"
        );
        assert!(!text.contains("PRIVATE-UNREADABLE-SENTINEL"), "{text}");
    }
    assert_unchanged(lab._root.path(), &before);

    let store = lab.home.join("store");
    let original = fs::metadata(&store).unwrap().permissions();
    fs::set_permissions(&store, fs::Permissions::from_mode(0o0)).unwrap();
    let denied = fs::read_dir(&store).is_err();
    let output = lab.doctor(&["--repo", "alice/fixture"]);
    fs::set_permissions(&store, original).unwrap();
    assert!(output.status.success(), "{output:?}");
    if denied {
        let text = report(&output);
        assert!(
            text.contains("store\": unreadable link directory"),
            "{text}"
        );
        assert!(text.contains("repository scope unavailable"), "{text}");
        assert!(!text.contains("no sessions adopted yet"), "{text}");
    }
    assert_unchanged(lab._root.path(), &before);
}

/// An adoption slot cannot disappear from diagnostics by becoming a directory or symlink.
#[test]
fn doctor_reports_nonregular_adoption_slots_without_following_them() {
    let lab = Lab::new();
    lab.repo();
    let links = lab.home.join("store/codex");
    let directory = links.join("directory.json");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("private"), b"PRIVATE-NONREGULAR-SENTINEL").unwrap();
    #[cfg(unix)]
    let target = {
        let target = lab._root.path().join("private-target.json");
        fs::write(
            &target,
            br#"{"owner":"bob","agent":"other","baseline_bytes":"PRIVATE-NONREGULAR-SENTINEL"}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&target, links.join("symlink.json")).unwrap();
        target
    };
    let before = files(lab._root.path());
    let output = lab.doctor(&["--repo", "alice/fixture"]);
    assert!(output.status.success(), "{output:?}");
    let text = report(&output);
    assert!(
        text.contains("directory.json\": invalid link path"),
        "{text}"
    );
    assert!(text.contains("repository scope unavailable"), "{text}");
    assert!(!text.contains("PRIVATE-NONREGULAR-SENTINEL"), "{text}");
    #[cfg(unix)]
    {
        assert!(text.contains("symlink.json\": invalid link path"), "{text}");
        assert_eq!(fs::read_link(links.join("symlink.json")).unwrap(), target);
    }
    assert!(directory.is_dir());
    assert!(link::list(&Store::at(lab.home.join("store"))).is_empty());
    assert_unchanged(lab._root.path(), &before);
}

/// Missing storage is fresh state; an existing replacement is unavailable adoption evidence.
#[test]
fn doctor_distinguishes_missing_store_from_invalid_store_and_runtime_containers() {
    #[cfg(unix)]
    let shapes = [
        "absent",
        "root-file",
        "runtime-file",
        "root-symlink",
        "dangling-root",
        "runtime-symlink",
        "runtime-dangling",
    ];
    #[cfg(not(unix))]
    let shapes = ["absent", "root-file", "runtime-file"];
    for shape in shapes {
        let lab = Lab::new();
        lab.repo();
        let store = lab.home.join("store");
        let runtime = store.join("codex");
        let linked: Option<(PathBuf, PathBuf)> = match shape {
            "absent" => None,
            "root-file" => {
                fs::write(&store, b"PRIVATE-CONTAINER-SENTINEL").unwrap();
                None
            }
            "runtime-file" => {
                fs::create_dir_all(&store).unwrap();
                fs::write(&runtime, b"PRIVATE-CONTAINER-SENTINEL").unwrap();
                None
            }
            #[cfg(unix)]
            "root-symlink" | "dangling-root" | "runtime-symlink" | "runtime-dangling" => {
                let target = lab._root.path().join("private-container");
                if !shape.contains("dangling") {
                    fs::create_dir_all(&target).unwrap();
                    fs::write(target.join("private.json"), b"PRIVATE-CONTAINER-SENTINEL").unwrap();
                }
                let path = if shape.starts_with("runtime-") {
                    fs::create_dir_all(&store).unwrap();
                    runtime.clone()
                } else {
                    store.clone()
                };
                std::os::unix::fs::symlink(&target, &path).unwrap();
                Some((path, target))
            }
            _ => unreachable!(),
        };
        let before = files(lab._root.path());
        let output = lab.doctor(&["--repo", "alice/fixture"]);
        assert!(output.status.success(), "{shape}: {output:?}");
        let text = report(&output);
        if shape == "absent" {
            assert!(text.contains("no sessions adopted yet"), "{text}");
            assert!(!text.contains("invalid link path"), "{text}");
            assert!(!store.exists());
        } else {
            let expected = if shape.starts_with("runtime-") {
                "codex"
            } else {
                "store"
            };
            assert!(
                text.contains(&format!("{expected}\": invalid link path")),
                "{shape}: {text}"
            );
            assert!(text.contains("repository scope unavailable"), "{text}");
            assert!(!text.contains("no sessions adopted yet"), "{text}");
        }
        assert!(!text.contains("PRIVATE-CONTAINER-SENTINEL"), "{text}");
        if let Some((path, target)) = linked {
            assert_eq!(fs::read_link(path).unwrap(), target);
        }
        assert_unchanged(lab._root.path(), &before);
    }
}
