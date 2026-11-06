#[path = "support/lab_env.rs"]
mod lab_env;

use agit::domain::repo::Repo;
use lab_env::LabEnv;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn config(root: &Path, version: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .env_clear_local()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AGIT_HOME", root.join("agit"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-gitconfig"))
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env("NO_COLOR", "1")
        .current_dir(root)
        .stdin(Stdio::null());
    for name in ["SYSTEMROOT", "WINDIR", "COMSPEC"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(version) = version {
        command.args(["--json", "--json-version", version]);
    }
    command.arg("config").args(args).output().unwrap()
}

fn refused(root: &Path, version: Option<&str>, args: &[&str], code: i32) {
    let output = config(root, version, args);
    assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
    if let Some(version) = version {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(value["command"], "config");
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], false);
    } else {
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
}

fn repository(root: &Path) -> (Repo, PathBuf) {
    let path = root.join("agit/repos/alice/demo");
    std::fs::create_dir_all(&path).unwrap();
    let repo = Repo::init(&path).unwrap();
    repo.set_auto_push(Some(false)).unwrap();
    assert!(config(root, None, &["push.auto", "false"]).status.success());
    (repo, path)
}

#[test]
fn repository_input_refusals_keep_usage_codes_and_leave_preferences_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let (_repo, path) = repository(root.path());
    let local = std::fs::read(path.join(".git/config")).unwrap();
    let global = std::fs::read(root.path().join("agit/config.json")).unwrap();
    let cases: &[&[&str]] = &[
        &["--repo", "alice/demo", "push.auto", "not-a-bool"],
        &["--repo", "alice/demo", "unknown"],
        &["--repo", "alice/demo", "push.visibility", "public"],
        &["--repo", "alice/demo", "--unset"],
        &["--repo", "demo", "push.auto", "true"],
        &["--repo", "alice/demo@main", "push.auto", "true"],
        &["--repo", "alice/demo#1", "push.auto", "true"],
        &["--repo", "alice/demo#invalid", "push.auto", "true"],
        &["--repo", "alice/.demo", "push.auto", "true"],
        &["--repo", "al.ice/demo", "push.auto", "true"],
        &["--repo", "alice/missing", "push.auto", "not-a-bool"],
    ];
    for version in [None, Some("1"), Some("2")] {
        for args in cases {
            refused(root.path(), version, args, 2);
            assert_eq!(std::fs::read(path.join(".git/config")).unwrap(), local);
            assert_eq!(
                std::fs::read(root.path().join("agit/config.json")).unwrap(),
                global
            );
        }
    }
}

#[test]
fn repository_io_and_stored_value_failures_keep_runtime_codes() {
    let root = tempfile::tempdir().unwrap();
    let (repo, path) = repository(root.path());
    let local = std::fs::read(path.join(".git/config")).unwrap();
    let lock = path.join(".git/config.lock");
    std::fs::write(&lock, "synthetic lock").unwrap();
    // An off recorded before the checkout existed goes only once the checkout's own setting is
    // written; a write that fails leaves it, so the destination doesn't fall back to inheriting.
    let record = root
        .path()
        .join("agit/preferences/automatic-upload-off.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let off = serde_json::json!({"version": 1, "off": [format!(
        "{}/alice/demo",
        agit::infra::config::hub_host_key("http://127.0.0.1:1").unwrap()
    )]})
    .to_string();
    std::fs::write(&record, &off).unwrap();
    for version in [None, Some("1"), Some("2")] {
        refused(
            root.path(),
            version,
            &["--repo", "alice/demo", "push.auto", "true"],
            1,
        );
        assert_eq!(std::fs::read(path.join(".git/config")).unwrap(), local);
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "synthetic lock");
        assert_eq!(std::fs::read_to_string(&record).unwrap(), off);
    }
    std::fs::remove_file(lock).unwrap();
    std::fs::remove_file(&record).unwrap();
    repo.git(&["config", "--local", "agit.autoPush", "invalid"])
        .unwrap();
    let invalid = std::fs::read(path.join(".git/config")).unwrap();
    for version in [None, Some("1"), Some("2")] {
        refused(
            root.path(),
            version,
            &["--repo", "alice/demo", "push.auto"],
            1,
        );
        assert_eq!(std::fs::read(path.join(".git/config")).unwrap(), invalid);
    }
    // A repository without a checkout keeps its off in a record of its own; a record that can't be
    // read fails reads and writes alike and is left as it was.
    let record = root
        .path()
        .join("agit/preferences/automatic-upload-off.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, "not a record").unwrap();
    for version in [None, Some("1"), Some("2")] {
        for args in [
            &["--repo", "alice/missing", "push.auto"][..],
            &["--repo", "alice/missing", "push.auto", "false"],
            &["--repo", "alice/missing", "--unset", "push.auto"],
        ] {
            refused(root.path(), version, args, 1);
            assert_eq!(std::fs::read(&record).unwrap(), b"not a record");
        }
    }
}
