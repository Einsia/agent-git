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
        .args(["import", &lab.sources[1].0, "--into", "me/qa@work"])
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
            "agit import <session-id> -n qa --from <runtime> -b <branch>",
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
        let output = lab.retry(template, &lab.sources[1].0, "claude-code");
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
