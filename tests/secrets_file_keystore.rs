//! The secret vault on the file keystore, reached through the command entry point on a machine
//! that has no credential store at all. A CI runner and an SSH login both look like this, and
//! it is the one keystore a test can exercise on every Unix platform; off Unix the store is
//! refused, so the test has nothing to drive there.
#![cfg(unix)]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Lab {
    _tmp: tempfile::TempDir,
    agit_home: PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let agit_home = tmp.path().join("agit-home");
        Lab {
            _tmp: tmp,
            agit_home,
        }
    }

    fn agit(&self, keystore: &str, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        cmd.args(args)
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_SECRETS_KEYSTORE", keystore)
            .env("AGIT_HUB_URL", "http://127.0.0.1:9")
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match stdin {
            None => cmd.stdin(Stdio::null()).output().unwrap(),
            Some(input) => {
                let mut child = cmd.stdin(Stdio::piped()).spawn().unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(input.as_bytes())
                    .unwrap();
                child.wait_with_output().unwrap()
            }
        }
    }

    fn ok(&self, keystore: &str, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.agit(keystore, args, stdin);
        assert!(
            out.status.success(),
            "`agit {}` failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The vault status out of the CLI JSON envelope.
    fn status(&self, keystore: &str) -> serde_json::Value {
        let out = self.ok(keystore, &["secrets", "status", "--json"], None);
        let envelope: serde_json::Value = serde_json::from_str(&out).unwrap();
        envelope["result"]["value"].clone()
    }
}

#[test]
fn file_keystore_serves_the_vault_without_a_credential_store() {
    let lab = Lab::new();
    assert_eq!(lab.status("file")["initialized"], false);

    lab.ok(
        "file",
        &["secrets", "add", "router", "--stdin"],
        Some("acme-router-pass\n"),
    );
    let status = lab.status("file");
    assert_eq!(status["initialized"], true);
    assert_eq!(status["rules"], 1);
    let list = lab.ok("file", &["secrets", "list"], None);
    assert!(list.contains("router"), "{list}");
    assert!(!list.contains("acme-router-pass"), "{list}");

    // One key file, beside the vault directory and never inside it: a copy of the vault
    // directory must not carry its key along.
    let keystore = lab.agit_home.join("keystore");
    let key_files: Vec<PathBuf> = std::fs::read_dir(&keystore)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "key"))
        .collect();
    assert_eq!(key_files.len(), 1, "{key_files:?}");
    assert!(
        !lab.agit_home
            .join("secret-filter")
            .join("keystore")
            .exists()
    );
    let key = std::fs::read_to_string(&key_files[0]).unwrap();
    let vault = std::fs::read_to_string(lab.agit_home.join("secret-filter/vault.json")).unwrap();
    assert!(!vault.contains("acme-router-pass"));
    assert!(!vault.contains(key.trim()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&keystore), 0o700);
        assert_eq!(mode(&key_files[0]), 0o600);
    }

    // doctor names the store and unlocks the vault through it.
    let doctor = lab.ok("file", &["doctor"], None);
    let row = doctor
        .lines()
        .find(|l| l.contains("secret keystore"))
        .unwrap_or_else(|| panic!("no keystore row:\n{doctor}"));
    assert!(row.contains("file keystore"), "{row}");
    assert!(row.contains("vault unlocked, 1 rules"), "{row}");

    // The same vault under the OS setting has no key there: fail closed, never read as empty.
    let out = lab.agit("os", &["secrets", "status"], None);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("secrets.keystore"), "{stderr}");

    // A setting outside its domain is refused, not read as the default.
    let out = lab.agit("keychain", &["secrets", "status"], None);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`os` or `file`"), "{stderr}");
}
