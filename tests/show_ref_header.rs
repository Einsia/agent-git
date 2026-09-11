use agit::domain::{meta, repo::Repo, storage, transcript};
use agit::hub::identity::{self, RemoteIdentity};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};

#[path = "support/publication_process.rs"]
mod publication_process;

const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HUB: &str = "https://published.example.test/mount";

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: Repo,
    sha: String,
    view: Vec<Value>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let work = root.path().join("workspace");
        std::fs::create_dir_all(&work).unwrap();
        let repo = Repo::init(&home.join("repos/alice/headers")).unwrap();
        std::fs::write(
            home.join("config.json"),
            if cfg!(windows) {
                "{\"secrets.keystore\":\"os\"}\n"
            } else {
                "{\"secrets.keystore\":\"file\"}\n"
            },
        )
        .unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        std::fs::write(repo.root().join("README.md"), "VERBATIM-FILE\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("shared file line").unwrap();
        repo.git(&["switch", "-q", "-c", "selected"]).unwrap();
        let view = vec![
            json!({"type":"user", "message":{"role":"user", "content":"FROZEN-PROMPT"}}),
            json!({"type":"system", "subtype":"vendor-observation", "private_evidence":"RAW-ONLY"}),
        ];
        let selected = transcript::wrap_lines(
            &view
                .iter()
                .map(|value| format!("{value}\n"))
                .collect::<String>(),
            "claude-code",
            SESSION,
        );
        let hidden = transcript::wrap_lines(
            &json!({"type":"user", "message":{"role":"user", "content":"HIDDEN-PROMPT"}})
                .to_string(),
            "claude-code",
            SESSION,
        );
        storage::write_snapshot(repo.root(), &(selected.clone() + &hidden), &selected).unwrap();
        let mut snapshot = meta::Meta::new(
            SESSION.into(),
            "claude-code".into(),
            "/selected/project".into(),
        );
        snapshot.code = Some("selected-code-anchor".into());
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("selected version").unwrap();
        let sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["tag", "saved"]).unwrap();
        repo.git(&["switch", "-q", "main"]).unwrap();
        repo.git(&[
            "remote",
            "add",
            "origin",
            &format!("{HUB}/alice/headers.git"),
        ])
        .unwrap();
        identity::pin(
            &repo,
            &RemoteIdentity::new(HUB, "aaaaaaaa-0000-4000-8000-000000000001").unwrap(),
        )
        .unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/selected", &sha])
            .unwrap();
        Self {
            _root: root,
            home,
            work,
            repo,
            sha,
            view,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self._root.path())
            .env("USERPROFILE", self._root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", "https://different.example.test")
            .env("AGIT_SESSION", "alice/headers@selected")
            .env(
                "GIT_CONFIG_GLOBAL",
                self._root.path().join("absent-gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("AGIT_TUI", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
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

    fn success(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}

#[test]
fn selected_repository_read_failure_stays_unclassified_when_git_can_fetch() {
    fn state(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                (
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    entry
                        .file_type()
                        .is_file()
                        .then(|| std::fs::read(entry.path()).unwrap()),
                )
            })
            .collect()
    }

    let fixture = Fixture::new();
    fixture.success(&["show", SESSION, "--agent", "alice/headers", "--raw"]);
    let blob = fixture
        .repo
        .git(&["rev-parse", &format!("{}:{}", fixture.sha, meta::VIEW_FILE)])
        .unwrap();
    let path = fixture
        .repo
        .root()
        .join(".git/objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    let bytes = std::fs::read(&path).unwrap();
    let remote = fixture._root.path().join("empty-promisor.git");
    fixture
        .repo
        .git(&["init", "--bare", remote.to_str().unwrap()])
        .unwrap();
    fixture
        .repo
        .git(&["remote", "set-url", "origin", remote.to_str().unwrap()])
        .unwrap();
    fixture
        .repo
        .git(&["config", "extensions.partialClone", "origin"])
        .unwrap();
    fixture
        .repo
        .git(&["config", "remote.origin.promisor", "true"])
        .unwrap();
    fixture
        .repo
        .git(&["config", "remote.origin.partialclonefilter", "blob:none"])
        .unwrap();
    std::fs::write(fixture.repo.root().join(".git/FETCH_HEAD"), b"").unwrap();
    #[cfg(windows)]
    {
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    std::fs::remove_file(&path).unwrap();
    let before = state(fixture._root.path());
    let refs = fixture.repo.git(&["show-ref"]).unwrap();

    for (flags, mode, version) in [
        (vec![], "human", None),
        (vec!["--quiet"], "quiet", None),
        (vec!["--json", "--json-version", "1"], "json1", Some(1)),
        (vec!["--json", "--json-version", "2"], "json2", Some(2)),
    ] {
        for raw in [false, true] {
            let mut args = flags.clone();
            args.extend(["show", SESSION, "--agent", "alice/headers"]);
            if raw {
                args.push("--raw");
            }
            let mut command = fixture.command(&args);
            command
                .env_remove("GIT_NO_LAZY_FETCH")
                .env("GIT_ALLOW_PROTOCOL", "file")
                .env("LC_ALL", "C")
                .env("LANGUAGE", "C");
            let output = publication_process::output(
                command,
                mode,
                "missing-promisor-view",
                std::time::Instant::now() + publication_process::MODE_LIMIT,
            )
            .unwrap();
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            let diagnostics = if let Some(version) = version {
                assert!(output.stderr.is_empty(), "{output:?}");
                let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(envelope["schema_version"], version);
                assert_eq!(envelope["command"], "show");
                assert_eq!(envelope["exit_code"], 1);
                assert_eq!(envelope["ok"], false);
                assert_eq!(envelope["result"]["format"], "empty");
                envelope["diagnostics"].to_string()
            } else {
                assert!(output.stdout.is_empty(), "{output:?}");
                String::from_utf8(output.stderr).unwrap()
            };
            assert!(
                diagnostics.contains("cannot read selected session content"),
                "{diagnostics}"
            );
            assert!(
                diagnostics.contains("upload-pack") && diagnostics.contains(&blob),
                "{diagnostics}"
            );
            assert!(
                !diagnostics.contains("FROZEN-PROMPT") && !diagnostics.contains("HIDDEN-PROMPT")
            );
            assert_eq!(state(fixture._root.path()), before);
            assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
            assert!(!path.exists());
        }
    }
    std::fs::write(path, bytes).unwrap();
    let before = state(fixture._root.path());
    let restored = fixture.success(&["show", SESSION, "--agent", "alice/headers", "--raw"]);
    assert!(restored.contains("FROZEN-PROMPT"));
    assert!(!restored.contains("HIDDEN-PROMPT"));
    assert_eq!(state(fixture._root.path()), before);
}

#[test]
fn explicit_agent_absence_is_a_reference_failure_without_changing_saved_evidence() {
    fn state(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                (
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    entry
                        .file_type()
                        .is_file()
                        .then(|| std::fs::read(entry.path()).unwrap()),
                )
            })
            .collect()
    }

    let fixture = Fixture::new();
    fixture.success(&["show", "alice/headers@selected", "--raw"]);
    let before = state(fixture._root.path());
    let refs = fixture
        .repo
        .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
        .unwrap();
    for (flags, version) in [
        (vec![], None),
        (vec!["--quiet"], None),
        (vec!["--json", "--json-version", "1"], Some(1)),
        (vec!["--json", "--json-version", "2"], Some(2)),
    ] {
        for (agent, code, diagnostic) in [
            ("alice/missing", 3, "nothing local named alice/missing"),
            ("alice/headers/extra", 2, "use the <owner>/<agent> form"),
        ] {
            let mut args = flags.clone();
            args.extend(["show", SESSION, "--agent", agent]);
            let output = fixture.run(&args);
            assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
            let diagnostics = if let Some(version) = version {
                assert!(output.stderr.is_empty(), "{output:?}");
                let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(envelope["command"], "show");
                assert_eq!(envelope["schema_version"], version);
                assert_eq!(envelope["exit_code"], code);
                assert_eq!(envelope["ok"], false);
                assert_eq!(envelope["result"]["format"], "empty");
                if version == 1 {
                    assert!(envelope.get("fix").is_none());
                } else {
                    assert_eq!(envelope["fix"], json!([]));
                }
                envelope["diagnostics"].to_string()
            } else {
                assert!(output.stdout.is_empty(), "{output:?}");
                String::from_utf8(output.stderr).unwrap()
            };
            assert!(diagnostics.contains(diagnostic), "{diagnostics}");
            assert_eq!(state(fixture._root.path()), before);
        }

        let mut args = flags;
        args.extend(["show", SESSION, "--agent", "alice/headers", "--raw"]);
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let values = if let Some(version) = version {
            let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(envelope["schema_version"], version);
            assert_eq!(envelope["ok"], true);
            assert_eq!(envelope["result"]["format"], "json_lines");
            envelope["result"]["values"].as_array().unwrap().clone()
        } else {
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        };
        assert_eq!(values, fixture.view);
        assert_eq!(fixture.repo.current_branch().as_deref(), Some("main"));
        assert_eq!(
            fixture
                .repo
                .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
                .unwrap(),
            refs
        );
        assert_eq!(state(fixture._root.path()), before);
    }
}

#[test]
fn qualified_points_report_the_selected_metadata_and_loss() {
    let fixture = Fixture::new();
    let version = meta::id_from_sha(&fixture.sha);
    for selector in ["selected", "saved", &fixture.sha] {
        let target = format!("alice/headers@{selector}");
        let text = fixture.success(&["show", &target]);
        let notice = text.lines().next().unwrap();
        assert!(notice.starts_with("target: alice/headers@"), "{text}");
        assert!(notice.ends_with("(via explicit arguments)"), "{text}");
        assert_eq!(text.matches("target: ").count(), 1, "{text}");
        for expected in [
            SESSION,
            "claude-code",
            &version,
            "repository VIEW",
            "selected-code-anchor",
            "/selected/project",
            "vendor-proprietary events",
            "FROZEN-PROMPT",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("HIDDEN-PROMPT"), "{text}");
        assert!(text.find("session").unwrap() < text.find("FROZEN-PROMPT").unwrap());
        assert!(
            text.contains(&format!(
                "{HUB}/@alice/headers/s/{SESSION}?ref={}",
                fixture.sha
            )),
            "{text}"
        );
        assert!(!text.contains("different.example.test"), "{text}");
    }
    assert_eq!(fixture.repo.current_branch().as_deref(), Some("main"));
    assert!(
        fixture
            .repo
            .git(&["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn raw_and_json_keep_native_values_separate_from_metadata() {
    let fixture = Fixture::new();
    let target = "alice/headers@selected";
    let raw = fixture.success(&["show", target, "--raw"]);
    let values: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(values, fixture.view);
    assert!(!raw.contains("repository VIEW"));
    assert!(!raw.contains("web:"));
    for (options, version) in [
        (vec!["--json"], 2),
        (vec!["--json", "--json-version", "1"], 1),
        (vec!["--json", "--json-version", "2"], 2),
    ] {
        for raw in [false, true] {
            let mut args = options.clone();
            args.extend(["show", target]);
            if raw {
                args.push("--raw");
            }
            let text = fixture.success(&args);
            let report: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(report["schema"], "cli-output");
            assert_eq!(report["schema_version"], version);
            assert_eq!(report["ok"], true);
            assert_eq!(report["command"], "show");
            assert_eq!(report["diagnostics"]["stderr"], json!([]));
            assert_eq!(
                report.as_object().unwrap().len(),
                if version == 1 { 7 } else { 8 }
            );
            if version == 1 {
                assert!(report.get("fix").is_none());
            } else {
                assert_eq!(report["fix"], json!([]));
            }
            if raw {
                assert_eq!(report["result"]["format"], "json_lines");
                assert_eq!(report["result"]["values"], json!(fixture.view));
            } else {
                assert_eq!(report["result"]["format"], "text");
                assert!(
                    report["result"]["lines"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|line| line.as_str().unwrap().contains(SESSION))
                );
            }
            assert!(report.get("session").is_none());
        }
    }
    assert_eq!(
        fixture.success(&["show", "alice/headers@selected:README.md"]),
        "VERBATIM-FILE\n"
    );
    assert_eq!(
        fixture.success(&["show", "alice/headers@selected:README.md", "--raw"]),
        "VERBATIM-FILE\n"
    );
    let quiet = fixture.success(&["--quiet", "show", target]);
    assert!(quiet.contains("FROZEN-PROMPT"));
    assert!(!quiet.contains("target: "), "{quiet}");
    let log = fixture.success(&["show", target, "--log-only"]);
    assert!(log.contains("repository LOG") && log.contains("HIDDEN-PROMPT"));
}

#[test]
fn historic_headers_and_links_do_not_follow_a_newer_branch_tip() {
    let fixture = Fixture::new();
    fixture.repo.git(&["switch", "-q", "selected"]).unwrap();
    let replacement = "agit-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let raw = json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"NEWER-CONTENT"}]}});
    let envelope = transcript::wrap_lines(&raw.to_string(), "codex", replacement);
    storage::write_snapshot(fixture.repo.root(), &envelope, &envelope).unwrap();
    meta::write(
        fixture.repo.root(),
        &meta::Meta::new(replacement.into(), "codex".into(), "/newer/project".into()),
    )
    .unwrap();
    fixture.repo.add_all().unwrap();
    fixture.repo.commit("newer selected branch").unwrap();
    let newer = fixture.repo.git(&["rev-parse", "HEAD"]).unwrap();
    fixture
        .repo
        .git(&["update-ref", "refs/remotes/origin/selected", &newer])
        .unwrap();
    fixture.repo.git(&["switch", "-q", "main"]).unwrap();
    for selector in ["saved", &fixture.sha] {
        let text = fixture.success(&["show", &format!("alice/headers@{selector}")]);
        assert!(
            text.contains(SESSION) && text.contains("FROZEN-PROMPT"),
            "{text}"
        );
        assert!(
            !text.contains(replacement) && !text.contains("NEWER-CONTENT"),
            "{text}"
        );
        assert!(text.contains(&format!("?ref={}", fixture.sha)), "{text}");
        assert!(!text.contains(&format!("?ref={newer}")), "{text}");
    }
    let text = fixture.success(&["show", "alice/headers@selected"]);
    assert!(
        text.contains(replacement) && text.contains("codex") && text.contains("NEWER-CONTENT"),
        "{text}"
    );
}

#[test]
fn optional_web_links_require_consistent_cached_publication_evidence() {
    let fixture = Fixture::new();
    let target = "alice/headers@selected";
    fixture
        .repo
        .git(&["update-ref", "-d", "refs/remotes/origin/selected"])
        .unwrap();
    let local = fixture.success(&["show", target]);
    assert!(
        local.contains(SESSION) && !local.contains("web:"),
        "{local}"
    );
    fixture
        .repo
        .git(&["update-ref", "refs/remotes/origin/selected", &fixture.sha])
        .unwrap();
    for remote in [
        "https://other.example.test/alice/headers.git",
        "https://published.example.test/another-mount/alice/headers.git",
        "https://user:secret@published.example.test/mount/alice/headers.git",
        "https://published.example.test/mount/alice/%2Fheaders.git",
        "https://published.example.test/mount/../headers.git",
    ] {
        fixture
            .repo
            .git(&["remote", "set-url", "origin", remote])
            .unwrap();
        let text = fixture.success(&["show", target]);
        assert!(
            !text.contains("web:") && !text.contains("user:secret"),
            "{text}"
        );
    }
    fixture
        .repo
        .git(&[
            "remote",
            "set-url",
            "origin",
            &format!("{HUB}/alice/headers.git"),
        ])
        .unwrap();
    fixture
        .repo
        .git(&["config", "agit.remoteIdentity", "invalid"])
        .unwrap();
    let text = fixture.success(&["show", target]);
    assert!(
        text.contains("FROZEN-PROMPT") && !text.contains("web:"),
        "{text}"
    );
    fixture
        .repo
        .git(&["config", "--unset", "agit.remoteIdentity"])
        .unwrap();
    let text = fixture.success(&["show", target]);
    assert!(!text.contains("web:"), "{text}");
}

#[test]
#[cfg(unix)]
fn commit_time_ignores_optional_git_signature_display() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let commit = fixture
        .repo
        .git(&["cat-file", "commit", &fixture.sha])
        .unwrap();
    let (headers, message) = commit.split_once("\n\n").unwrap();
    let signed = format!(
        "{headers}\ngpgsig -----BEGIN PGP SIGNATURE-----\n synthetic-signature\n -----END PGP SIGNATURE-----\n\n{message}\n"
    );
    let object = fixture._root.path().join("signed-commit");
    std::fs::write(&object, signed).unwrap();
    let sha = fixture
        .repo
        .git(&[
            "hash-object",
            "-w",
            "-t",
            "commit",
            object.to_str().unwrap(),
        ])
        .unwrap();
    fixture
        .repo
        .git(&["update-ref", "refs/heads/signed", &sha])
        .unwrap();
    let verifier = fixture._root.path().join("synthetic-verifier");
    std::fs::write(
        &verifier,
        "#!/bin/sh\nprintf 'synthetic verifier output\\n' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&verifier, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture
        .repo
        .git(&["config", "gpg.program", verifier.to_str().unwrap()])
        .unwrap();
    fixture
        .repo
        .git(&["config", "log.showSignature", "true"])
        .unwrap();
    let text = fixture.success(&["show", "alice/headers@signed"]);
    assert!(
        text.contains(&meta::id_from_sha(&sha)) && text.contains("FROZEN-PROMPT"),
        "{text}"
    );
    assert!(!text.contains("synthetic verifier"), "{text}");
}

#[test]
fn published_birth_points_without_a_session_identity_have_no_web_link() {
    let fixture = Fixture::new();
    fixture
        .repo
        .git(&["switch", "-q", "-c", "birth", "main"])
        .unwrap();
    let mut snapshot = meta::Meta::new_session_line("claude-code".into(), "/birth/project".into());
    meta::write(fixture.repo.root(), &snapshot).unwrap();
    storage::write_snapshot(fixture.repo.root(), "", "").unwrap();
    fixture.repo.add_all().unwrap();
    fixture.repo.commit("unclaimed birth point").unwrap();
    let birth = fixture.repo.git(&["rev-parse", "HEAD"]).unwrap();
    snapshot.session = SESSION.into();
    meta::write(fixture.repo.root(), &snapshot).unwrap();
    fixture.repo.add_all().unwrap();
    fixture.repo.commit("claimed descendant").unwrap();
    let published = fixture.repo.git(&["rev-parse", "HEAD"]).unwrap();
    fixture
        .repo
        .git(&["update-ref", "refs/remotes/origin/birth", &published])
        .unwrap();
    fixture.repo.git(&["switch", "-q", "main"]).unwrap();
    let text = fixture.success(&["show", &format!("alice/headers@{birth}")]);
    assert!(
        text.contains("claude-code") && text.contains(&meta::id_from_sha(&birth)),
        "{text}"
    );
    assert!(
        !text.contains("web:") && !text.contains("/s/?ref="),
        "{text}"
    );
}
