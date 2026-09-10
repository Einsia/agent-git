//! Terminal categories distinguish absent references from malformed command syntax.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let store = root.path().join("agit");
        let work = root.path().join("work");
        for path in [&home, &work, &root.path().join("git-template")] {
            fs::create_dir_all(path).unwrap();
        }
        let lab = Self {
            root,
            home,
            store,
            work,
        };
        success(lab.run(&["init", "qa"]));
        // Startup receives an explicit identity; rejection snapshots begin after migration.
        success(
            lab.command(env!("CARGO_BIN_EXE_agit"))
                .env("AGIT_SESSION", "local/qa@main")
                .arg("tag")
                .output()
                .unwrap(),
        );
        assert_eq!(lab.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_TEMPLATE_DIR", self.root.path().join("git-template"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Terminal category fixture")
            .env("GIT_AUTHOR_EMAIL", "terminal-category@example.test")
            .env("GIT_COMMITTER_NAME", "Terminal category fixture")
            .env("GIT_COMMITTER_EMAIL", "terminal-category@example.test")
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

    fn run(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .output()
            .unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = success(
            self.command("git")
                .arg("-C")
                .arg(self.store.join("repos/local/qa"))
                .args(args)
                .output()
                .unwrap(),
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                let bytes = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    bytes,
                )
            })
            .collect()
    }
}

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    output
}

fn assert_output(output: &Output, mode: &str, code: i32, message: Option<&str>) {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{mode}: {output:?}");
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["schema"], "cli-output");
        assert_eq!(document["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(document["command"], "tag");
        assert_eq!(document["exit_code"], code);
        assert_eq!(document["ok"], code == 0);
        if version == "1" {
            assert!(document.get("fix").is_none());
        } else {
            assert_eq!(document["fix"], serde_json::json!([]));
        }
        if let Some(message) = message {
            assert_eq!(document["result"]["format"], "empty");
            assert!(
                document["diagnostics"]["stderr"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|diagnostic| diagnostic["level"] == "error"
                        && diagnostic["message"].as_str().unwrap().contains(message))
            );
        }
    } else if let Some(message) = message {
        assert!(output.stdout.is_empty(), "{mode}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    } else if mode == "quiet" {
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn missing_tag_targets_are_refs_in_every_output_mode_without_writing_identity() {
    for (mode, flags) in [
        ("human", vec![]),
        ("quiet", vec!["--quiet"]),
        ("json1", vec!["--json", "--json-version", "1"]),
        ("json2", vec!["--json", "--json-version", "2"]),
    ] {
        let lab = Lab::new();
        let head = lab.git(&["rev-parse", "HEAD"]);
        let refs = lab.git(&["show-ref"]);
        let before = lab.state();

        for (target, code, message) in [
            ("local/qa@absent", 3, "cannot resolve `absent`"),
            ("local/qa@main~oops", 2, "non-negative integer"),
        ] {
            let mut args = flags.clone();
            args.extend(["tag", "rejected", target]);
            assert_output(&lab.run(&args), mode, code, Some(message));
            assert_eq!(lab.git(&["show-ref"]), refs);
            assert_eq!(lab.git(&["rev-parse", "HEAD"]), head);
            assert_eq!(lab.state(), before, "{mode}: {target}");
        }

        let target = format!("local/qa@{head}");
        let mut args = flags;
        args.extend(["tag", "accepted", &target]);
        assert_output(&lab.run(&args), mode, 0, None);
        assert_eq!(lab.git(&["rev-parse", "refs/tags/accepted"]), head);
        assert_eq!(lab.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            lab.git(&["show-ref"]),
            format!("{refs}\n{head} refs/tags/accepted")
        );
        let mut after = lab.state();
        assert_eq!(
            after.remove(&PathBuf::from(
                "agit/repos/local/qa/.git/refs/tags/accepted"
            )),
            Some(Some(format!("{head}\n").into_bytes()))
        );
        assert_eq!(after, before, "{mode}: only the selected tag may be added");
    }
}
