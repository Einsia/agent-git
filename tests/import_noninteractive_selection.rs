//! A discovered native transcript is a candidate, not permission to adopt it.

use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};

const HUB: &str = "http://127.0.0.1:1";

struct Lab {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    sources: Vec<(String, PathBuf)>,
}

#[cfg(windows)]
fn ordinary_windows_path(path: &std::path::Path) -> PathBuf {
    let path = path.to_str().unwrap();
    if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{share}"))
    } else {
        PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
    }
}

fn powershell_retry_script(tail: &str) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop';
$fixture = ConvertFrom-Json -InputObject ([Environment]::GetEnvironmentVariable('AGIT_IMPORT_TEST_ENV', 'Process'));
foreach ($name in [Environment]::GetEnvironmentVariables('Process').Keys) {{
    [Environment]::SetEnvironmentVariable($name, $null, 'Process');
}}
foreach ($property in $fixture.PSObject.Properties) {{
    [Environment]::SetEnvironmentVariable($property.Name, [string]$property.Value, 'Process');
}}
if ([Environment]::GetEnvironmentVariable('AGIT_IMPORT_TEST_HOST_ONLY', 'Process') -or [Environment]::GetEnvironmentVariable('AGIT_SESSION', 'Process')) {{
    throw 'the native retry inherited host-only context';
}}
[Console]::Out.WriteLine('SYNTHETIC-RETRY-HOST-READY');
& $env:AGIT_IMPORT_TEST_BINARY import {tail};
exit $LASTEXITCODE"#
    )
}

impl Lab {
    fn new(count: usize) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let store = tmp.path().join("agit");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        #[cfg(windows)]
        let work = ordinary_windows_path(&work);
        let project = home
            .join(".claude/projects")
            .join(agit::adapter::claude_code::slug_for(&work));
        fs::create_dir_all(&project).unwrap();
        let sources = (0..count)
            .map(|i| {
                let id = format!("aaaaaaaa-0000-4000-8000-{i:012}");
                let path = project.join(format!("{id}.jsonl"));
                let content = [
                    serde_json::json!({"type":"user", "sessionId":id, "cwd":work,
                        "uuid":format!("{id}-user"), "message":{"role":"user", "content":format!("SYNTHETIC-CANDIDATE-{i}")}}),
                    serde_json::json!({"type":"assistant", "sessionId":id, "cwd":work,
                        "uuid":format!("{id}-assistant"), "message":{"role":"assistant", "content":"SYNTHETIC-ANSWER"}}),
                ]
                .map(|record| format!("{record}\n"))
                .concat();
                fs::write(&path, content).unwrap();
                (id, path)
            })
            .collect();
        agit::infra::credentials::save_at(
            &store.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(HUB).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(HUB.into()),
                access_token: "synthetic".into(),
                refresh_token: "synthetic".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let lab = Self {
            _tmp: tmp,
            home,
            store,
            work,
            sources,
        };
        let init = lab.command().args(["init", "qa"]).output().unwrap();
        assert!(init.status.success(), "{init:?}");
        lab
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", HUB)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .current_dir(&self.work);
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", &self.home);
        }
        command
    }

    fn state(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for root in [self.store.join("repos"), self.store.join("store")] {
            if root.exists() {
                for entry in walkdir::WalkDir::new(&root) {
                    let entry = entry.unwrap();
                    if entry.file_type().is_file() {
                        files.insert(entry.path().to_owned(), fs::read(entry.path()).unwrap());
                    }
                }
            }
        }
        for (_, path) in &self.sources {
            files.insert(path.clone(), fs::read(path).unwrap());
        }
        files
    }

    fn retry(&self, template: &str, id: &str, runtime: &str) -> std::process::Output {
        let tail = template
            .replace("<session-id>", id)
            .replace("<runtime>", runtime);
        let command = self.command();
        let mut environment: Vec<_> = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|v| (key.to_owned(), v.to_owned())))
            .collect();
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_agit"));
        #[cfg(windows)]
        let binary = ordinary_windows_path(&binary);
        environment.push(("AGIT_IMPORT_TEST_BINARY".into(), binary.into_os_string()));
        let mut shell = if cfg!(windows) {
            let mut shell = Command::new("powershell");
            shell
                .stdin(std::process::Stdio::piped())
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(powershell_retry_script(&tail));
            let fixture: BTreeMap<_, _> = environment
                .iter()
                .map(|(key, value)| (key.to_str().unwrap(), value.to_str().unwrap()))
                .collect();
            // The shell initializes in its host context; only the fixture environment reaches
            // the native child, so an existing session identity cannot select another target.
            shell
                .env(
                    "AGIT_IMPORT_TEST_ENV",
                    serde_json::to_string(&fixture).unwrap(),
                )
                .env("AGIT_IMPORT_TEST_HOST_ONLY", "SYNTHETIC-HOST-CONTEXT")
                .env("AGIT_SESSION", "SYNTHETIC-HOST-SESSION");
            shell
        } else {
            let mut shell = Command::new("sh");
            shell
                .arg("-c")
                .arg(format!("exec \"$1\" import {tail}"))
                .arg("import-retry")
                .arg(env!("CARGO_BIN_EXE_agit"));
            shell.env_clear().envs(environment);
            shell
        };
        let output = shell.current_dir(&self.work).output().unwrap();
        if cfg!(windows) {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("SYNTHETIC-RETRY-HOST-READY"),
                "PowerShell did not reach the isolated native retry: {output:?}"
            );
        }
        output
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = fs::read(self.store.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

#[test]
fn fallback_candidates_share_activity_order_across_runtimes_without_adoption() {
    let mut lab = Lab::new(2);
    let codex_id = "cccccccc-0000-4000-8000-000000000001";
    let codex = lab.home.join(".codex/sessions/rollout-recency.jsonl");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(
        &codex,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "session_meta", "payload": {"id": codex_id, "cwd": lab.work}
            })
        ),
    )
    .unwrap();
    for (path, seconds) in [
        (&lab.sources[0].1, 300),
        (&codex, 200),
        (&lab.sources[1].1, 100),
    ] {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + seconds),
            )
            .unwrap();
    }
    lab.sources.push((codex_id.into(), codex));
    let expected = [&lab.sources[0].0, &lab.sources[2].0, &lab.sources[1].0];
    for adopted in [false, true] {
        if adopted {
            for (index, (id, _)) in lab.sources.iter().enumerate() {
                let runtime = if index == 2 { "codex" } else { "claude-code" };
                let mut link = agit::domain::link::Link::new(runtime, id, Some(&lab.work));
                link.owner = Some("me".into());
                link.agent = Some("qa".into());
                link.branch = Some(format!("line-{index}"));
                agit::domain::link::write(
                    &agit::domain::store::Store::at(lab.store.join("store")),
                    &link,
                )
                .unwrap();
            }
        }
        let before = lab.state();
        for flags in [
            vec![],
            vec!["--quiet"],
            vec!["--json", "--json-version", "1"],
            vec!["--json", "--json-version", "2"],
        ] {
            let output = lab
                .command()
                .env("CODEX_HOME", lab.home.join(".codex"))
                .args(&flags)
                .args(if adopted {
                    vec!["resume", "--no-launch"]
                } else {
                    vec!["import", "--link-only"]
                })
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            let text = if flags.contains(&"--json") {
                assert!(output.stderr.is_empty(), "{output:?}");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["exit_code"], 8);
                value["diagnostics"]["stderr"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["message"].as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                assert!(output.stdout.is_empty(), "{output:?}");
                String::from_utf8(output.stderr).unwrap()
            };
            let positions: Vec<_> = if adopted {
                ["line-0", "line-2", "line-1"]
                    .iter()
                    .map(|id| {
                        text.find(id).unwrap_or_else(|| {
                            panic!("missing candidate {id:?}; flags={flags:?}: {text}")
                        })
                    })
                    .collect()
            } else {
                expected
                    .iter()
                    .map(|id| {
                        text.find(id.as_str()).unwrap_or_else(|| {
                            panic!("missing candidate {id:?}; flags={flags:?}: {text}")
                        })
                    })
                    .collect()
            };
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
            assert_eq!(lab.state(), before);
        }
    }
}

#[test]
fn all_noninteractive_candidates_are_actionable_without_adoption() {
    for count in [1, 2, 17] {
        let lab = Lab::new(count);
        let before = lab.state();
        let output = lab
            .command()
            .args(["import", "--into", "me/qa@work"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(8), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        for (id, _) in &lab.sources {
            assert!(stderr.contains(id), "candidate {id} missing: {stderr}");
        }
        assert!(stderr.contains("agit import <session-id> --into me/qa@work"));
        assert_eq!(lab.state(), before);
    }
}

#[test]
fn an_ambiguous_explicit_prefix_requires_selection_without_stdout_or_adoption() {
    let lab = Lab::new(2);
    let before = lab.state();
    for quiet in [false, true] {
        for mode in [
            vec!["--link-only"],
            vec!["--into", "me/qa@work", "--independent"],
        ] {
            let mut command = lab.command();
            if quiet {
                command.arg("--quiet");
            }
            let output = command
                .args(["import", "aaaaaaaa", "--from", "claude-code"])
                .args(mode)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.contains("matches 2 sessions"), "{stderr}");
            for (id, _) in &lab.sources {
                assert!(stderr.contains(id), "candidate {id} missing: {stderr}");
            }
            assert!(stderr.contains("give a longer prefix"), "{stderr}");
            assert_eq!(lab.state(), before);
        }
    }
}

#[test]
fn a_missing_explicit_native_id_is_a_reference_error_without_adoption() {
    for count in [0, 2] {
        let lab = Lab::new(count);
        let before = lab.state();
        for quiet in [false, true] {
            for mode in [
                vec!["--link-only"],
                vec!["--into", "me/qa@work", "--independent"],
            ] {
                let mut command = lab.command();
                if quiet {
                    command.arg("--quiet");
                }
                let output = command
                    .args(["import", "missing-native-session", "--from", "claude-code"])
                    .args(mode)
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(3), "{output:?}");
                assert!(output.stdout.is_empty(), "{output:?}");
                let stderr = String::from_utf8(output.stderr).unwrap();
                assert!(
                    stderr.contains("no session named `missing-native-session`"),
                    "{stderr}"
                );
                assert_eq!(lab.state(), before);
            }
        }
    }
}

#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn a_missing_explicit_native_id_has_the_same_reference_error_in_json() {
    for count in [0, 2] {
        let lab = Lab::new(count);
        let before = lab.state();
        for version in ["1", "2"] {
            for mode in [
                vec!["--link-only"],
                vec!["--into", "me/qa@work", "--independent"],
            ] {
                let output = lab
                    .command()
                    .args([
                        "--json",
                        "--json-version",
                        version,
                        "import",
                        "missing-native-session",
                        "--from",
                        "claude-code",
                    ])
                    .args(mode)
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(3), "{output:?}");
                assert!(output.stderr.is_empty(), "{output:?}");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
                assert_eq!(value["exit_code"], 3);
                assert_eq!(value["ok"], false);
                assert_eq!(
                    value["result"],
                    serde_json::json!({"format":"empty", "kind":"import"})
                );
                assert_eq!(value.get("fix").is_some(), version == "2");
                assert!(
                    value["diagnostics"]["stderr"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["level"] == "error"
                            && row["message"]
                                .as_str()
                                .unwrap()
                                .contains("no session named `missing-native-session`")),
                    "{value}"
                );
                assert_eq!(lab.state(), before);
            }
        }
    }
}

#[test]
fn ambiguous_prefix_diagnostics_keep_the_candidate_limit() {
    let lab = Lab::new(9);
    let before = lab.state();
    let output = lab
        .command()
        .args(["import", "aaaaaaaa", "--from", "claude-code", "--link-only"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("matches 9 sessions"), "{stderr}");
    let listed = lab
        .sources
        .iter()
        .filter(|(id, _)| stderr.contains(id))
        .count();
    assert_eq!(listed, 8, "{stderr}");
    assert_eq!(lab.state(), before);
}

#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn ambiguous_prefix_json_keeps_candidates_in_diagnostics_and_requires_selection() {
    let lab = Lab::new(2);
    let before = lab.state();
    for version in ["1", "2"] {
        for mode in [
            vec!["--link-only"],
            vec!["--into", "me/qa@work", "--independent"],
        ] {
            let output = lab
                .command()
                .args([
                    "--json",
                    "--json-version",
                    version,
                    "import",
                    "aaaaaaaa",
                    "--from",
                    "claude-code",
                ])
                .args(mode)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
            assert_eq!(value["exit_code"], 8);
            assert_eq!(value["ok"], false);
            assert_eq!(
                value["result"],
                serde_json::json!({"format":"empty", "kind":"import"})
            );
            assert_eq!(value.get("fix").is_some(), version == "2");
            let diagnostics = value["diagnostics"]["stderr"].as_array().unwrap();
            for (id, _) in &lab.sources {
                assert!(
                    diagnostics
                        .iter()
                        .any(|row| row["message"].as_str().unwrap().contains(id)),
                    "{value}"
                );
            }
            assert!(
                diagnostics.iter().any(|row| row["level"] == "error"
                    && row["message"]
                        .as_str()
                        .unwrap()
                        .contains("matches 2 sessions")),
                "{value}"
            );
            assert_eq!(lab.state(), before);
        }
    }
}

#[test]
fn a_longer_or_exact_native_id_selects_only_the_named_source() {
    for exact in [false, true] {
        let mut lab = Lab::new(2);
        let selected = "aaaaaaaa-0000-4000-8000-111111111111";
        let (old_id, old_path) = &lab.sources[1];
        let text = fs::read_to_string(old_path)
            .unwrap()
            .replace(old_id, selected);
        let path = old_path.with_file_name(format!("{selected}.jsonl"));
        fs::write(&path, text).unwrap();
        fs::remove_file(old_path).unwrap();
        lab.sources[1] = (selected.into(), path);
        let before = lab.state();
        let selector = if exact { selected } else { &selected[..25] };
        let output = lab
            .command()
            .args(["import", selector, "--from", "claude-code", "--link-only"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let link_path = lab
            .store
            .join("store/claude-code")
            .join(format!("{selected}.json"));
        let link = agit::domain::link::get(
            &agit::domain::store::Store::at(lab.store.join("store")),
            "claude-code",
            selected,
        )
        .unwrap();
        assert_eq!(link.session_id, selected);
        assert_eq!(link.source, "claude-code");
        let mut after = lab.state();
        assert!(after.remove(&link_path).is_some());
        assert_eq!(
            after.remove(&link_path.with_extension("json.lock")),
            Some(Vec::new())
        );
        assert_eq!(after, before);
    }
}

#[test]
fn no_candidates_is_an_empty_result() {
    let lab = Lab::new(0);
    let before = lab.state();
    let output = lab
        .command()
        .args(["import", "--into", "me/qa@work"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("no unadopted sessions")
    );
    assert_eq!(lab.state(), before);
}

#[test]
fn an_explicit_native_id_still_selects_only_that_transcript() {
    let lab = Lab::new(2);
    let output = lab
        .command()
        .args([
            "import",
            &lab.sources[1].0,
            "--into",
            "me/qa@work",
            "--independent",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let linked = lab.store.join("store/claude-code");
    assert!(!linked.join(format!("{}.json", lab.sources[0].0)).exists());
    assert!(linked.join(format!("{}.json", lab.sources[1].0)).exists());
    let output = lab
        .command()
        .args(["log", "me/qa@work", "--oneline"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("SYNTHETIC-CANDIDATE-1"), "{stdout}");
    assert!(!stdout.contains("SYNTHETIC-CANDIDATE-0"), "{stdout}");
}

#[test]
fn a_link_only_retry_preserves_its_mode_without_inventing_a_target() {
    let lab = Lab::new(1);
    let before = lab.state();
    let output = lab
        .command()
        .args(["import", "--link-only"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("agit import <session-id> --from <runtime> --link-only"),
        "{stderr}"
    );
    assert!(!stderr.contains("--into"), "{stderr}");
    assert_eq!(lab.state(), before);
}

#[test]
fn retry_templates_preserve_known_options_and_request_only_missing_target_parts() {
    let lab = Lab::new(1);
    let before = lab.state();
    for (args, expected) in [
        (
            vec!["import", "-b", "work"],
            "agit import <session-id> -b work --from <runtime> --into <owner/repo>",
        ),
        (
            vec!["import", "--repo", "me/qa"],
            "agit import <session-id> --into me/qa --from <runtime> -b <branch>",
        ),
        (
            vec!["import", "-n", "qa"],
            "agit import <session-id> -n qa --from <runtime> --into <owner/repo>@<branch>",
        ),
        (
            vec![
                "import",
                "-n",
                "qa",
                "-b",
                "work",
                "--from",
                "cc",
                "--onto",
                "me/qa@base",
                "--privacy",
            ],
            "agit import <session-id> -n qa -b work --from cc --onto me/qa@base --privacy",
        ),
    ] {
        let output = lab.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(8), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.lines().last().unwrap().ends_with(expected),
            "{stderr}"
        );
    }
    assert_eq!(lab.state(), before);
}

#[test]
fn candidates_respect_the_runtime_preserved_in_the_retry_template() {
    let lab = Lab::new(1);
    let before = lab.state();
    let output = lab
        .command()
        .args(["import", "--into", "me/qa@work", "--from", "codex"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("no unadopted sessions")
    );
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains(&lab.sources[0].0)
    );

    let output = lab
        .command()
        .args(["import", "--into", "me/qa@work", "--from", "cc"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&lab.sources[0].0), "{stderr}");
    assert!(stderr.contains("--into me/qa@work --from cc"), "{stderr}");

    let output = lab
        .command()
        .args(["import", "--into", "me/qa@work", "--from", "not-a-runtime"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(lab.state(), before);
}

#[test]
fn an_existing_link_excludes_only_its_own_runtime_identity() {
    let lab = Lab::new(2);
    for (runtime, id) in [
        ("codex", &lab.sources[0].0),
        ("claude-code", &lab.sources[1].0),
    ] {
        let path = lab
            .store
            .join("store")
            .join(runtime)
            .join(format!("{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "{}\n").unwrap();
    }
    let before = lab.state();
    let output = lab
        .command()
        .args(["import", "--into", "me/qa@work", "--from", "cc"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&lab.sources[0].0), "{stderr}");
    assert!(!stderr.contains(&lab.sources[1].0), "{stderr}");
    assert_eq!(lab.state(), before);
}

#[test]
fn filling_the_runtime_and_id_from_the_candidate_table_selects_only_that_source() {
    for runtime in ["claude-code", "codex"] {
        let mut lab = Lab::new(1);
        let id = lab.sources[0].0.clone();
        let directory = lab.home.join(".codex/sessions/2026/09/08");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("rollout-2026-09-08T00-00-00-{id}.jsonl"));
        fs::write(&path, [
            serde_json::json!({"type":"session_meta", "payload":{"id":id,"cwd":lab.work}}),
            serde_json::json!({"type":"response_item", "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"SYNTHETIC-CODEX"}]}}),
        ].map(|record| format!("{record}\n")).concat()).unwrap();
        lab.sources.push((id.clone(), path));
        let before = lab.state();
        let output = lab
            .command()
            .args(["import", "--link-only"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(8), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("claude-code") && stderr.contains("codex"),
            "{stderr}"
        );
        assert_eq!(stderr.matches(&id).count(), 2, "{stderr}");
        assert_eq!(lab.state(), before);
        let template = stderr
            .lines()
            .find_map(|line| line.split_once("agit import ").map(|(_, rest)| rest))
            .unwrap();
        assert!(template.contains("--from <runtime>"), "{template}");
        let output = lab.retry(template, &id, runtime);
        assert!(output.status.success(), "{output:?}");
        for candidate_runtime in ["claude-code", "codex"] {
            assert_eq!(
                lab.store
                    .join("store")
                    .join(candidate_runtime)
                    .join(format!("{id}.json"))
                    .exists(),
                candidate_runtime == runtime,
                "retry did not select {runtime}: {output:?}"
            );
        }
    }
}

#[test]
fn the_retry_template_preserves_shell_sensitive_known_arguments() {
    for (branch, separate_branch) in [
        ("work;literal'\u{2018}\u{2019}\u{201a}\u{201b}branch", false),
        ("@branch", true),
    ] {
        let lab = Lab::new(2);
        let destination = format!("me/qa@{branch}");
        let mut command = lab.command();
        if separate_branch {
            command.args(["import", "--into", "me/qa", "-b", branch]);
        } else {
            command.args(["import", "--into", &destination]);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(8), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let template = stderr
            .lines()
            .find_map(|line| line.split_once("agit import ").map(|(_, rest)| rest))
            .unwrap();
        let output = lab.retry(
            &format!("{template} --independent"),
            &lab.sources[1].0,
            "claude-code",
        );
        assert!(output.status.success(), "{output:?}");
        let link: serde_json::Value = serde_json::from_slice(
            &fs::read(
                lab.store
                    .join("store/claude-code")
                    .join(format!("{}.json", lab.sources[1].0)),
            )
            .unwrap_or_else(|error| panic!("retry produced no readable link: {error}; {output:?}")),
        )
        .unwrap();
        assert_eq!(link["branch"], branch);
        assert!(
            !lab.store
                .join("store/claude-code")
                .join(format!("{}.json", lab.sources[0].0))
                .exists()
        );
    }
}

#[test]
fn an_explicit_independent_decision_survives_the_no_id_retry() {
    let lab = Lab::new(2);
    let before = lab.state();
    let output = lab
        .command()
        .args(["import", "--into", "me/qa@work", "--independent"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let template = stderr
        .lines()
        .find_map(|line| line.split_once("agit import ").map(|(_, rest)| rest))
        .unwrap();
    assert!(template.ends_with("--independent"), "{template}");
    assert_eq!(lab.state(), before);
    let retry = lab.retry(template, &lab.sources[1].0, "claude-code");
    assert!(retry.status.success(), "{retry:?}");
    let directory = lab.store.join("store/claude-code");
    assert!(
        directory
            .join(format!("{}.json", lab.sources[1].0))
            .exists()
    );
    assert!(
        !directory
            .join(format!("{}.json", lab.sources[0].0))
            .exists()
    );
}

fn target_selection_state(lab: &Lab) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    for root in [
        lab.store.join("repos"),
        lab.store.join("store"),
        lab.store.join("secret-filter"),
        lab.home.join(".claude"),
    ] {
        if root.exists() {
            for entry in walkdir::WalkDir::new(root) {
                let entry = entry.unwrap();
                let bytes = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                result.insert(entry.path().to_owned(), bytes);
            }
        }
    }
    result
}

fn target_git(lab: &Lab, repo: &std::path::Path, args: &[&str]) -> String {
    let environment = lab.command();
    let mut command = Command::new("git");
    command.env_clear();
    for (key, value) in environment.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let output = command
        .args(["-c", "commit.gpgsign=false", "-C"])
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn competing_checkout(lab: &Lab) -> PathBuf {
    let path = lab.store.join("repos/other/qa");
    fs::create_dir_all(path.join("session")).unwrap();
    target_git(lab, &path, &["init", "-q", "--initial-branch=main"]);
    fs::write(
        path.join("session/meta.json"),
        "{\"layout\":\"v1\",\"line\":\"file\",\"kind\":\"file\"}\n",
    )
    .unwrap();
    target_git(lab, &path, &["add", "."]);
    target_git(
        lab,
        &path,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.test",
            "commit",
            "-qm",
            "synthetic competitor",
        ],
    );
    path
}

/// Selection failures cannot adopt a link, create a privacy copy or touch either checkout.
#[test]
fn legacy_import_target_refusals_precede_adoption_and_privacy_writes() {
    let lab = Lab::new(1);
    let repo = lab.store.join("repos/me/qa");
    competing_checkout(&lab);
    target_git(&lab, &repo, &["branch", "collision", "main"]);
    target_git(&lab, &repo, &["tag", "collision", "main"]);
    let cases = [
        (
            vec!["-n", "qa", "-b", "selected", "--independent"],
            8,
            vec!["me/qa", "other/qa"],
        ),
        (
            vec!["--into", "me/qa@selected", "--onto", "collision"],
            8,
            vec!["branch collision", "tag collision"],
        ),
        (
            vec!["--into", "me/qa@selected", "--onto", "absent"],
            3,
            vec!["not a branch, tag, or commit prefix"],
        ),
        (
            vec!["--into", "me/absent@selected", "--onto", "main"],
            3,
            vec!["requires an existing destination repository"],
        ),
        (vec!["--independent"], 2, vec!["needs a destination agent"]),
    ];
    for (arguments, code, candidates) in cases {
        for privacy in [false, true] {
            for flags in [
                vec![],
                vec!["--quiet"],
                vec!["--json", "--json-version", "1"],
                vec!["--json", "--json-version", "2"],
            ] {
                let before = target_selection_state(&lab);
                let mut command = lab.command();
                command
                    .args(&flags)
                    .args(["import", &lab.sources[0].0, "--from", "claude-code"])
                    .args(&arguments);
                if privacy {
                    command.arg("--privacy");
                }
                let output = command.stdin(std::process::Stdio::null()).output().unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(code),
                    "{arguments:?}: {output:?}"
                );
                assert_eq!(
                    target_selection_state(&lab),
                    before,
                    "target refusal changed source or destination data"
                );
                let diagnostic = if flags.contains(&"--json") {
                    assert!(output.stderr.is_empty(), "{output:?}");
                    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(value["ok"], false);
                    assert_eq!(value["exit_code"], code);
                    assert_eq!(
                        value["schema_version"],
                        flags.last().unwrap().parse::<u64>().unwrap()
                    );
                    assert_eq!(value["result"]["format"], "empty");
                    value["diagnostics"]["stderr"].to_string()
                } else {
                    assert!(output.stdout.is_empty(), "{output:?}");
                    String::from_utf8(output.stderr).unwrap()
                };
                for candidate in &candidates {
                    assert!(
                        diagnostic.contains(candidate),
                        "missing {candidate}: {diagnostic}"
                    );
                }
            }
        }
    }
}

/// Invalid branch names cannot create a native copy, adoption claim or destination repository.
#[test]
fn invalid_import_branch_names_precede_all_local_writes() {
    let lab = Lab::new(1);
    for name in ["qa", "absent"] {
        for branch in ["bad.lock", "foo//bar", "HEAD", "-topic", "@{-1}"] {
            for privacy in [false, true] {
                for flags in [
                    vec![],
                    vec!["--quiet"],
                    vec!["--json", "--json-version", "1"],
                    vec!["--json", "--json-version", "2"],
                ] {
                    let before = target_selection_state(&lab);
                    let mut command = lab.command();
                    command
                        .args(&flags)
                        .args([
                            "import",
                            &lab.sources[0].0,
                            "--from",
                            "claude-code",
                            "-n",
                            name,
                            "--independent",
                        ])
                        .arg(format!("--branch={branch}"));
                    if privacy {
                        command.arg("--privacy");
                    }
                    let output = command.stdin(std::process::Stdio::null()).output().unwrap();
                    assert_eq!(output.status.code(), Some(2), "{name}@{branch}: {output:?}");
                    assert_eq!(
                        target_selection_state(&lab),
                        before,
                        "invalid branch selection changed source or destination data"
                    );
                    let diagnostic = if flags.contains(&"--json") {
                        assert!(output.stderr.is_empty(), "{output:?}");
                        let value: serde_json::Value =
                            serde_json::from_slice(&output.stdout).unwrap();
                        assert_eq!(value["ok"], false);
                        assert_eq!(value["exit_code"], 2);
                        assert_eq!(
                            value["schema_version"],
                            flags.last().unwrap().parse::<u64>().unwrap()
                        );
                        assert_eq!(value["result"]["format"], "empty");
                        value["diagnostics"]["stderr"].to_string()
                    } else {
                        assert!(output.stdout.is_empty(), "{output:?}");
                        String::from_utf8(output.stderr).unwrap()
                    };
                    assert!(
                        diagnostic.contains("not a valid Git ref"),
                        "{name}@{branch}: {diagnostic}"
                    );
                }
            }
        }
    }
}

/// Hierarchical branch names remain available even when the destination does not exist yet.
#[test]
fn import_branch_preflight_accepts_hierarchical_names_for_new_repositories() {
    let lab = Lab::new(1);
    let source = fs::read(&lab.sources[0].1).unwrap();
    let output = lab
        .command()
        .args([
            "import",
            &lab.sources[0].0,
            "--from",
            "claude-code",
            "-n",
            "absent",
            "-b",
            "topic/valid",
            "--independent",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let store = agit::domain::store::Store::at(lab.store.join("store"));
    let link = agit::domain::link::get(&store, "claude-code", &lab.sources[0].0).unwrap();
    assert_eq!(
        (
            link.owner.as_deref(),
            link.agent.as_deref(),
            link.branch.as_deref()
        ),
        (Some("me"), Some("absent"), Some("topic/valid"))
    );
    target_git(
        &lab,
        &lab.store.join("repos/me/absent"),
        &["show-ref", "--verify", "refs/heads/topic/valid"],
    );
    assert_eq!(fs::read(&lab.sources[0].1).unwrap(), source);
}

/// An explicit owner disambiguates a name without borrowing another checkout's history.
#[test]
fn legacy_import_consumes_the_explicit_checkout_with_a_competing_name() {
    let lab = Lab::new(1);
    let competitor = competing_checkout(&lab);
    let competitor_refs = target_git(&lab, &competitor, &["show-ref"]);
    let source = fs::read(&lab.sources[0].1).unwrap();
    let output = lab
        .command()
        .args([
            "import",
            &lab.sources[0].0,
            "--from",
            "claude-code",
            "--into",
            "me/qa@selected",
            "--independent",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let store = agit::domain::store::Store::at(lab.store.join("store"));
    let link = agit::domain::link::get(&store, "claude-code", &lab.sources[0].0).unwrap();
    assert_eq!(
        (
            link.owner.as_deref(),
            link.agent.as_deref(),
            link.branch.as_deref()
        ),
        (Some("me"), Some("qa"), Some("selected"))
    );
    target_git(
        &lab,
        &lab.store.join("repos/me/qa"),
        &["show-ref", "--verify", "refs/heads/selected"],
    );
    assert_eq!(
        target_git(&lab, &competitor, &["show-ref"]),
        competitor_refs
    );
    assert_eq!(fs::read(&lab.sources[0].1).unwrap(), source);
}

/// Repository qualifiers and partial selectors cannot be discarded during base selection.
#[test]
fn legacy_import_onto_scope_refusals_preserve_all_source_and_destination_data() {
    let lab = Lab::new(1);
    competing_checkout(&lab);
    let cases = [
        ("other/qa@main", None, 2, "selected destination repository"),
        ("other@main", None, 2, "selected destination repository"),
        ("@", Some("other/qa@main"), 2, "session repository"),
        ("me/qa@@", Some("other/qa@main"), 2, "session repository"),
        ("qa@main", None, 8, "names multiple local repos"),
        ("main#1.1", None, 2, "requires a whole commit"),
        ("main#1..#1", None, 2, "requires a whole commit"),
        ("main:AGENTS.md", None, 2, "requires a whole commit"),
    ];
    for (onto, session, code, diagnostic) in cases {
        for privacy in [false, true] {
            for flags in [
                vec![],
                vec!["--quiet"],
                vec!["--json", "--json-version", "1"],
                vec!["--json", "--json-version", "2"],
            ] {
                let before = target_selection_state(&lab);
                let mut command = lab.command();
                command.args(&flags).args([
                    "import",
                    &lab.sources[0].0,
                    "--from",
                    "claude-code",
                    "--into",
                    "me/qa@selected",
                    "--onto",
                    onto,
                ]);
                if let Some(session) = session {
                    command.env("AGIT_SESSION", session);
                }
                if privacy {
                    command.arg("--privacy");
                }
                let output = command.stdin(std::process::Stdio::null()).output().unwrap();
                assert_eq!(output.status.code(), Some(code), "{onto}: {output:?}");
                assert_eq!(target_selection_state(&lab), before);
                let text = if flags.contains(&"--json") {
                    assert!(output.stderr.is_empty(), "{output:?}");
                    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(value["ok"], false);
                    assert_eq!(value["exit_code"], code);
                    assert_eq!(value["result"]["format"], "empty");
                    value["diagnostics"]["stderr"].to_string()
                } else {
                    assert!(output.stdout.is_empty(), "{output:?}");
                    String::from_utf8(output.stderr).unwrap()
                };
                assert!(text.contains(diagnostic), "{onto}: {text}");
                if code == 8 {
                    assert!(
                        text.contains("me/qa") && text.contains("other/qa"),
                        "{text}"
                    );
                }
            }
        }
    }
}

/// Same-repository qualifiers and historic whole commits retain their selected lineage.
#[test]
fn legacy_import_onto_accepts_qualified_session_and_historic_commit_targets() {
    let lab = Lab::new(1);
    let repo = lab.store.join("repos/me/qa");
    let source = fs::read(&lab.sources[0].1).unwrap();
    let seed = lab
        .command()
        .args([
            "import",
            &lab.sources[0].0,
            "--from",
            "claude-code",
            "--into",
            "me/qa@seed",
            "--independent",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(seed.status.success(), "{seed:?}");
    let main = target_git(&lab, &repo, &["rev-parse", "main"]);
    let seed = target_git(&lab, &repo, &["rev-parse", "seed"]);
    let cases = [
        ("main", None, main.as_str()),
        ("me/qa@main", None, main.as_str()),
        ("qa@main", None, main.as_str()),
        ("@", Some("me/qa@main"), main.as_str()),
        ("seed~0", None, seed.as_str()),
        ("seed#1", None, seed.as_str()),
        ("me/qa@seed#1", None, seed.as_str()),
    ];
    for (i, (onto, session, expected)) in cases.into_iter().enumerate() {
        let branch = format!("positive-{i}");
        let mut command = lab.command();
        command.args([
            "-y",
            "import",
            &lab.sources[0].0,
            "--from",
            "claude-code",
            "--into",
            &format!("me/qa@{branch}"),
            "--onto",
            onto,
        ]);
        if let Some(session) = session {
            command.env("AGIT_SESSION", session);
        }
        let output = command.stdin(std::process::Stdio::null()).output().unwrap();
        assert!(output.status.success(), "{onto}: {output:?}");
        let history = target_git(&lab, &repo, &["rev-list", "--first-parent", &branch]);
        assert!(
            history.lines().any(|oid| oid == expected),
            "{onto}: {history}"
        );
        let store = agit::domain::store::Store::at(lab.store.join("store"));
        let link = agit::domain::link::get(&store, "claude-code", &lab.sources[0].0).unwrap();
        assert_eq!(
            (
                link.owner.as_deref(),
                link.agent.as_deref(),
                link.branch.as_deref()
            ),
            (Some("me"), Some("qa"), Some(branch.as_str()))
        );
        assert_eq!(fs::read(&lab.sources[0].1).unwrap(), source);
    }
}
