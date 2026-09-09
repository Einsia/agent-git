//! JSON recovery is typed advisory data, while explicit version 1 keeps its closed envelope.

#[cfg(unix)]
mod unix {
    use serde_json::Value;
    use std::path::PathBuf;
    use std::process::{Command, Output};
    use std::{collections::BTreeMap, fs};

    const HUB: &str = "http://127.0.0.1:1";
    const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

    struct Lab {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        store: PathBuf,
        work: PathBuf,
    }

    impl Lab {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let store = tmp.path().join("store");
            let work = tmp.path().join("work <literal> 'quoted'\nline");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&work).unwrap();
            Self {
                _tmp: tmp,
                home,
                store,
                work: work.canonicalize().unwrap(),
            }
        }

        fn command(&self, args: &[&str]) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
            command
                .args(args)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.home)
                .env("AGIT_HOME", &self.store)
                .env("AGIT_HUB_URL", HUB)
                .env("AGIT_SECRETS_KEYSTORE", "file")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("CI", "1")
                .env("NO_COLOR", "1")
                .env("AGIT_FIX_TEST_SECRET", "SYNTHETIC-DO-NOT-SERIALIZE")
                .current_dir(&self.work);
            command
        }

        fn json(&self, args: &[&str]) -> (Output, Value) {
            let output = self.command(args).output().unwrap();
            assert!(output.stderr.is_empty(), "{output:?}");
            let value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("{error}: {output:?}"));
            (output, value)
        }

        fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            walkdir::WalkDir::new(self._tmp.path())
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| (entry.path().to_path_buf(), fs::read(entry.path()).unwrap()))
                .collect()
        }

        fn seed_session(&self, branch: &str) {
            agit::infra::credentials::save_at(
                &self.store.join("credentials").join(format!(
                    "{}.json",
                    agit::infra::config::hub_host_key(HUB).unwrap()
                )),
                &agit::infra::credentials::HubCredential {
                    username: "me".into(),
                    email: None,
                    hub: Some(HUB.into()),
                    access_token: "SYNTHETIC".into(),
                    refresh_token: "SYNTHETIC".into(),
                    access_expires_at: "2099-01-01T00:00:00Z".into(),
                    refresh_expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .unwrap();
            let directory = self.home.join(".codex/sessions/2026/09/08");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("rollout-2026-09-08T00-00-00-{SID}.jsonl")), [
                serde_json::json!({"type":"session_meta","payload":{"id":SID,"cwd":self.work}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"SYNTHETIC-FIX-RECOVERY"}]}}),
            ].map(|value| format!("{value}\n")).concat()).unwrap();
            for args in [
                vec!["init", "qa", "--no-bind"],
                vec![
                    "import",
                    SID,
                    "--from",
                    "codex",
                    "--into",
                    branch,
                    "--independent",
                ],
            ] {
                let output = self.command(&args).output().unwrap();
                assert!(output.status.success(), "{output:?}");
            }
        }
    }

    #[test]
    fn default_v2_registers_login_and_explicit_v1_preserves_its_envelope() {
        let lab = Lab::new();
        let (output, v2) = lab.json(&["--json", "commit"]);
        assert_eq!(output.status.code(), Some(5));
        assert_eq!(v2["schema_version"], 2);
        assert_eq!(
            v2["fix"][0]["argv"],
            serde_json::json!(["agit", "login", "--hub", HUB])
        );
        assert_eq!(v2["fix"][0]["cwd"], lab.work.to_str().unwrap());
        assert_eq!(
            v2["fix"][0]["env"],
            serde_json::json!({"AGIT_HOME":lab.store,"AGIT_HUB_URL":HUB})
        );
        assert_eq!(v2["fix"][0]["requires_interaction"], true);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SYNTHETIC-DO-NOT-SERIALIZE"));
        assert!(!lab.store.join("credentials").exists());
        let (legacy, v1) = lab.json(&["--json", "--json-version", "1", "commit"]);
        assert_eq!(legacy.status.code(), Some(5));
        assert_eq!(v1["schema_version"], 1);
        assert!(v1.get("fix").is_none());
        let mut same = v2;
        same["schema_version"] = 1.into();
        same.as_object_mut().unwrap().remove("fix");
        assert_eq!(same, v1);
        assert_eq!(v1.as_object().unwrap().len(), 7);
        assert!(
            String::from_utf8_lossy(&legacy.stdout)
                .starts_with("{\n  \"schema\": \"cli-output\",\n  \"schema_version\": 1,")
        );
    }

    #[test]
    fn a_known_missing_repository_has_a_complete_clone_action_without_running_it() {
        let lab = Lab::new();
        let (output, value) = lab.json(&["--json", "resume", "alice/research@work", "--no-launch"]);
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(
            value["fix"][0]["argv"],
            serde_json::json!(["agit", "clone", "--no-bind", "--", "alice/research"])
        );
        assert!(!lab.store.join("repos/alice/research").exists());
        assert_eq!(value["fix"][0]["requires_interaction"], false);
    }

    #[test]
    fn a_complete_preparation_action_preserves_literals_and_can_be_run_explicitly() {
        let lab = Lab::new();
        let target = "me/qa@<work>'\"‘’‚‛;literal";
        lab.seed_session(target);
        let before = lab.files();
        let (output, value) = lab.json(&["--json", "resume", target, "--as", "codex"]);
        assert_eq!(output.status.code(), Some(8));
        assert_eq!(
            lab.files(),
            before,
            "reporting must not execute the preparation"
        );
        assert_eq!(
            value["fix"][0]["argv"],
            serde_json::json!(["agit", "resume", "--no-launch", "--as=codex", "--", target])
        );
        let action = &value["fix"][0];
        let args: Vec<_> = action["argv"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|value| value.as_str().unwrap())
            .collect();
        let mut retry = lab.command(&args);
        retry.current_dir(action["cwd"].as_str().unwrap());
        for (key, value) in action["env"].as_object().unwrap() {
            retry.env(key, value.as_str().unwrap());
        }
        let result = retry.output().unwrap();
        assert!(result.status.success(), "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stdout).contains(SID),
            "{result:?}"
        );
    }

    #[test]
    fn version_selection_covers_parse_and_incompatible_errors_without_guessing_targets() {
        let lab = Lab::new();
        for (version, has_fix) in [("1", false), ("2", true)] {
            for tail in [vec!["--not-an-option"], vec!["resume"], vec![]] {
                let mut args = vec!["--json", "--json-version", version];
                args.extend(tail);
                let (output, value) = lab.json(&args);
                assert!(!output.status.success());
                assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
                assert_eq!(value.get("fix").is_some(), has_fix);
                if has_fix {
                    assert_eq!(value["fix"], serde_json::json!([]));
                }
            }
        }
        let (_, shorthand) = lab.json(&["--json", "resume", "qa@work"]);
        assert_eq!(shorthand["fix"], serde_json::json!([]));
        let output = lab
            .command(&["status", "--json-version", "1"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }

    #[test]
    fn preparation_keeps_explicit_confirmation_and_hyphen_leading_values() {
        let lab = Lab::new();
        let (_, value) = lab.json(&[
            "--json",
            "--yes",
            "--quiet",
            "--no-color",
            "--no-tui",
            "resume",
            "me/qa@work",
            "--cwd=-C",
        ]);
        assert_eq!(
            value["fix"][0]["argv"],
            serde_json::json!([
                "agit",
                "resume",
                "--no-launch",
                "--yes",
                "--quiet",
                "--no-color",
                "--no-tui",
                "--cwd=-C",
                "--",
                "me/qa@work"
            ])
        );
        let argv: Vec<&str> = value["fix"][0]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        use clap::Parser as _;
        let parsed = agit::commands::Cli::try_parse_from(argv).unwrap();
        assert!(parsed.yes && parsed.quiet && parsed.no_color && parsed.no_tui);
        let agit::commands::Commands::Resume(args) = parsed.command.unwrap() else {
            panic!("expected resume");
        };
        assert_eq!(args.cwd.as_deref(), Some(std::path::Path::new("-C")));
        assert_eq!(args.target.as_deref(), Some("me/qa@work"));
        assert!(args.no_launch);
        assert_eq!(value["fix"][0]["requires_interaction"], false);
    }

    #[test]
    fn global_version_selection_accepts_both_sides_of_the_subcommand() {
        let lab = Lab::new();
        for version in ["1", "2"] {
            for command in [
                vec!["status"],
                vec!["resume", "me/qa@work"],
                vec!["scan"],
                vec!["view"],
            ] {
                for (before, after) in [
                    (vec!["--json-version", version], vec!["--json"]),
                    (vec!["--json"], vec!["--json-version", version]),
                    (vec!["--json-version", version, "--json"], vec![]),
                    (vec![], vec!["--json", "--json-version", version]),
                ] {
                    let args = [before, command.clone(), after].concat();
                    let (_, value) = lab.json(&args);
                    assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
                    assert_ne!(value["result"]["kind"], "parse_error", "{args:?}: {value}");
                    assert_eq!(value.get("fix").is_some(), version == "2");
                }
            }
        }
    }

    #[test]
    fn version_without_json_refuses_before_startup_or_dispatch() {
        let lab = Lab::new();
        let before = lab.files();
        for args in [
            vec!["--json-version", "1"],
            vec!["--json-version", "1", "init", "qa"],
            vec!["status", "--json-version", "2"],
        ] {
            let output = lab.command(&args).output().unwrap();
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("--json-version requires --json"),
                "{output:?}"
            );
            assert_eq!(lab.files(), before);
        }
    }

    #[test]
    fn preparation_does_not_promise_to_bypass_unconfirmed_resume_questions() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let lab = Lab::new();
        for (confirmation, expected) in [
            (None, true),
            (Some(OsString::from("")), false),
            (Some(OsString::from("1")), false),
            (Some(OsString::from_vec(vec![0xff])), true),
        ] {
            let mut command =
                lab.command(&["--json", "resume", "me/qa@work", "--as=claude", "--force"]);
            if let Some(value) = confirmation {
                command.env("AGIT_YES", value);
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            let action = &value["fix"][0];
            assert_eq!(action["requires_interaction"], expected, "{value}");
            assert_eq!(
                action["argv"],
                serde_json::json!([
                    "agit",
                    "resume",
                    "--no-launch",
                    "--as=claude",
                    "--force",
                    "--",
                    "me/qa@work"
                ])
            );
            assert!(action["env"].get("AGIT_YES").is_none());
        }
    }

    #[test]
    fn unsupported_hub_scheme_spellings_do_not_create_recovery_actions() {
        let lab = Lab::new();
        let schema: Value =
            serde_json::from_str(include_str!("../docs/cli-json-schema-v2.json")).unwrap();
        let pattern = schema["$defs"]["fix_command"]["properties"]["env"]["properties"]["AGIT_HUB_URL"]["pattern"].as_str().unwrap();
        let allowed = regex::Regex::new(pattern).unwrap();
        for (args, code) in [
            (vec!["--json", "resume", "me/qa@work"], 8),
            (vec!["--json", "commit"], 5),
        ] {
            for (hub, count) in [
                ("http://example.invalid", 1),
                ("https://example.invalid/path@name", 1),
                ("HTTP://example.invalid", 0),
                ("hTTps://example.invalid/path@name", 0),
                ("http://example.invalid:65536", 0),
                ("http://example.invalid:bad", 0),
                ("http://example.invalid:", 0),
                ("http://example.invalid/path\\name", 0),
                ("http://[not-an-ip]", 0),
                ("http://[::1]:65535/path@name", 1),
                ("http://example.invalid/path%5Cname", 1),
            ] {
                let output = lab
                    .command(&args)
                    .env("AGIT_HUB_URL", hub)
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(code), "{output:?}");
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                let actions = value["fix"].as_array().unwrap();
                assert_eq!(actions.len(), count, "{hub}: {value}");
                for action in actions {
                    let emitted = action["env"]["AGIT_HUB_URL"].as_str().unwrap();
                    assert_eq!(emitted, hub);
                    assert!(
                        allowed.is_match(emitted),
                        "the schema rejected emitted routing: {hub}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_global_directory_keeps_relative_store_routing_and_runtime_options() {
        let lab = Lab::new();
        let selected = lab.work.join("selected\n<work>");
        fs::create_dir_all(selected.join("relative-store")).unwrap();
        fs::write(
            selected.join("relative-store/config.json"),
            r#"{"hub.url":"https://example.invalid/prefix"}"#,
        )
        .unwrap();
        let output = lab
            .command(&[
                "--json",
                "-C",
                selected.to_str().unwrap(),
                "resume",
                "me/qa@work",
                "--cwd",
                "runtime path\nwith \\",
                "--force",
            ])
            .env_remove("AGIT_HUB_URL")
            .env("AGIT_HOME", "relative-store")
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let action = &value["fix"][0];
        assert_eq!(action["cwd"], selected.to_str().unwrap());
        assert_eq!(
            action["env"]["AGIT_HOME"],
            selected.join("relative-store").to_str().unwrap()
        );
        assert_eq!(
            action["env"]["AGIT_HUB_URL"],
            "https://example.invalid/prefix"
        );
        assert_eq!(
            action["argv"],
            serde_json::json!([
                "agit",
                "resume",
                "--no-launch",
                "--cwd=runtime path\nwith \\",
                "--force",
                "--",
                "me/qa@work"
            ])
        );
    }
}
