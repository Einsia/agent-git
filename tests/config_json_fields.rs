use std::process::Command;

fn run(root: &std::path::Path, args: &[&str], environment: &[(&str, &str)]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["--json", "config"])
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("AGIT_HOME", root.join("agit"))
        .env("CI", "1")
        .envs(environment.iter().copied())
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["result"]["format"], "json");
    document["result"]["value"].clone()
}

#[test]
fn config_json_distinguishes_persisted_values_from_effective_defaults() {
    let root = tempfile::tempdir().unwrap();
    let all = run(root.path(), &[], &[]);
    assert_eq!(all["operation"], "list");
    let automatic = all["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"] == "commit.auto")
        .unwrap();
    assert_eq!(automatic["effective"], "true");
    assert_eq!(automatic["source"], "default");
    assert!(automatic["stored"].is_null());
    assert!(!root.path().join("agit/config.json").exists());

    let changed = run(root.path(), &["commit.auto", "false"], &[]);
    assert_eq!(changed["operation"], "set");
    assert_eq!(changed["setting"]["stored"], "false");
    assert_eq!(changed["setting"]["effective"], "false");
    assert_eq!(changed["setting"]["source"], "stored");
    let read = run(root.path(), &["commit.auto"], &[]);
    assert_eq!(read["setting"], changed["setting"]);

    let unset = run(root.path(), &["--unset", "commit.auto"], &[]);
    assert_eq!(unset["operation"], "unset");
    assert_eq!(unset["setting"]["effective"], "true");
    assert!(unset["setting"]["stored"].is_null());
}

#[test]
fn overridden_settings_report_both_the_saved_request_and_active_environment() {
    let root = tempfile::tempdir().unwrap();
    let value = run(
        root.path(),
        &["secrets.keystore", "os"],
        &[("AGIT_SECRETS_KEYSTORE", "file")],
    );
    assert_eq!(value["setting"]["stored"], "os");
    assert_eq!(value["setting"]["effective"], "file");
    assert_eq!(value["setting"]["source"], "environment");
    assert_eq!(
        value["setting"]["environment_name"],
        "AGIT_SECRETS_KEYSTORE"
    );
}
