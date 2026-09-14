use super::*;

#[test]
fn terminal_input_and_output_are_both_required() {
    for (input, output) in [(false, false), (false, true), (true, false)] {
        assert!(require_terminal(input, output).is_err());
    }
    require_terminal(true, true).unwrap();
}

#[test]
fn authority_is_removed_while_provider_and_terminal_inputs_survive() {
    let source = [
        ("ANTHROPIC_API_KEY", "PRIVATE_API_KEY"),
        ("CLAUDE_CONFIG_DIR", "/native-config"),
        ("AWS_PROFILE", "review-profile"),
        ("TERM", "xterm-256color"),
        ("AGIT_HOME", "/original-store"),
        ("AGIT_SESSION", "owner/repo@branch"),
        ("AGIT_MERGE_TX", "owner/repo@branch"),
        ("AGIT_MERGE_GENERATION", "generation"),
        ("AGIT_SETTLEMENT_NATIVE", "native"),
        ("AGIT_SETTLEMENT_ARCHIVE_ROLE", "role"),
        ("AGIT_RC", "1"),
        ("AGIT_RC_SUPERVISED_HOOK", "1"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_PERMISSION_MODE", "bypassPermissions"),
        ("CLAUDE_CODE_MANAGED_SETTINGS_PATH", "/injected-policy"),
        ("NODE_OPTIONS", "--require=/injected"),
        ("BASH_ENV", "/injected"),
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG", "/injected-query-config"),
        ("GIT_CONFIG_NOSYSTEM", "0"),
        ("GIT_CONFIG_SYSTEM", "/injected-system"),
        ("GIT_CONFIG_GLOBAL", "/injected-global"),
        ("GIT_CONFIG_KEY_0", "include.path"),
        ("GIT_CONFIG_VALUE_0", "/injected-include"),
        ("GIT_CONFIG_PARAMETERS", "'include.path=/injected-include'"),
        ("GIT_DIR", "/unrelated-repository"),
        ("GIT_WORK_TREE", "/unrelated-worktree"),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/unrelated-objects"),
        ("GIT_REPLACE_REF_BASE", "refs/replace/"),
        ("GIT_NO_REPLACE_OBJECTS", "0"),
        ("GIT_NO_LAZY_FETCH", "0"),
        ("GIT_ATTR_NOSYSTEM", "0"),
    ];
    let prepared = foreground_environment(
        source.map(|(key, value)| (key.into(), value.into())),
        Path::new("/prepared/agit-home"),
        Path::new("/prepared/empty-git-config"),
    );
    assert_eq!(prepared.len(), 12);
    let value = |name| {
        prepared
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    assert_eq!(
        value("ANTHROPIC_API_KEY"),
        Some(&OsString::from("PRIVATE_API_KEY"))
    );
    assert_eq!(value("TERM"), Some(&OsString::from("xterm-256color")));
    assert_eq!(
        value("AGIT_HOME"),
        Some(&OsString::from("/prepared/agit-home"))
    );
    for key in ["GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL"] {
        assert_eq!(
            value(key),
            Some(&OsString::from("/prepared/empty-git-config"))
        );
    }
    for (key, expected) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_NO_LAZY_FETCH", "1"),
        ("GIT_ATTR_NOSYSTEM", "1"),
    ] {
        assert_eq!(value(key), Some(&OsString::from(expected)));
    }
    for key in [
        "GIT_CONFIG",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_PARAMETERS",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_REPLACE_REF_BASE",
    ] {
        assert_eq!(value(key), None);
    }
}

#[test]
fn foreground_arguments_keep_tools_questions_and_literal_opening_directions() {
    let prompt = "--resume 'unrelated'\n$(touch SHOULD_NOT_EXIST); `false`";
    let report = Path::new("report with 'quote'.json");
    let args = foreground_arguments(prompt, "fixture-session", report).unwrap();
    let value = |flag| {
        let position = args.iter().position(|argument| argument == flag).unwrap();
        args[position + 1].as_str()
    };
    assert_eq!(value("--tools"), "Bash,Read,Write,AskUserQuestion");
    assert_eq!(value("--permission-mode"), "default");
    assert_eq!(value("--setting-sources"), "user");
    assert!(args.last().unwrap().ends_with(prompt));
    assert!(
        args.last()
            .unwrap()
            .starts_with("Review the prepared publication")
    );
    assert!(args.last().unwrap().contains("report with 'quote'.json"));
    for forbidden in [
        "--print",
        "-p",
        "--output-format",
        "--json-schema",
        "--resume",
        "--continue",
        "--permission-prompts",
        "--no-session-persistence",
        "--dangerously-skip-permissions",
    ] {
        assert!(!args.iter().any(|argument| argument == forbidden));
    }
    let plan = ForegroundReview {
        program: "/trusted/claude".into(),
        workspace: "/prepared".into(),
        environment: vec![("AGIT_HOME".into(), "/prepared/agit-home".into())],
        arguments: args.clone(),
        report: report.into(),
        session_id: "fixture-session".into(),
        git_config: "/prepared/empty-git-config".into(),
    };
    assert_eq!(plan.session_id(), "fixture-session");
    let command = plan.command();
    assert_eq!(command.get_program(), OsStr::new("/trusted/claude"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        args.iter().map(OsStr::new).collect::<Vec<_>>()
    );
    assert_eq!(command.get_current_dir(), Some(Path::new("/prepared")));
}

#[test]
fn invalid_opening_directions_are_rejected_without_echoing_input() {
    for prompt in [
        String::new(),
        " \n ".into(),
        "PRIVATE\0INPUT".into(),
        "x".repeat(MAX_OPENING_BYTES + 1),
    ] {
        assert_eq!(
            validate_opening(&prompt).unwrap_err().to_string(),
            "audit opening directions are empty, invalid, or exceed their byte limit"
        );
    }
}

#[test]
fn report_and_store_must_be_fresh_caller_owned_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    assert!(prepared_agent_home(&workspace).is_err());
    std::fs::create_dir(workspace.join("agit-home")).unwrap();
    assert_eq!(
        prepared_agent_home(&workspace).unwrap(),
        workspace.join("agit-home")
    );
    let report = workspace.join("audit-report.json");
    require_absent_report(&report).unwrap();
    std::fs::write(&report, b"old result").unwrap();
    assert!(require_absent_report(&report).is_err());
    assert_eq!(std::fs::read(report).unwrap(), b"old result");
}

#[test]
fn git_configuration_is_created_empty_and_rechecked_without_truncation() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let path = workspace.join(EMPTY_GIT_CONFIG);
    create_empty_git_config(&workspace).unwrap();
    assert!(std::fs::read(&path).unwrap().is_empty());
    verify_empty_git_config(&workspace, &path).unwrap();
    assert!(create_empty_git_config(&workspace).is_err());
    std::fs::write(&path, b"[include]\npath = /injected\n").unwrap();
    assert!(verify_empty_git_config(&workspace, &path).is_err());
    assert!(create_empty_git_config(&workspace).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"[include]\npath = /injected\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    let unrelated = tempfile::tempdir().unwrap();
    let unrelated_path = unrelated.path().join(EMPTY_GIT_CONFIG);
    std::fs::write(&unrelated_path, b"").unwrap();
    assert!(verify_empty_git_config(&workspace, &unrelated_path).is_err());
}

#[test]
fn preexisting_git_configuration_entries_are_never_adopted() {
    for contents in [b"".as_slice(), b"[include]\npath = /injected\n"] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let path = workspace.join(EMPTY_GIT_CONFIG);
        std::fs::write(&path, contents).unwrap();
        assert!(create_empty_git_config(&workspace).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), contents);
    }
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    std::fs::create_dir(workspace.join(EMPTY_GIT_CONFIG)).unwrap();
    assert!(create_empty_git_config(&workspace).is_err());
    assert!(workspace.join(EMPTY_GIT_CONFIG).is_dir());
}

#[cfg(unix)]
#[test]
fn git_configuration_symlinks_cannot_redirect_creation_or_launch() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let target = unrelated.path().join("external-config");
    let path = workspace.join(EMPTY_GIT_CONFIG);
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(create_empty_git_config(&workspace).is_err());
    assert!(!target.exists());
    std::fs::write(&target, b"").unwrap();
    assert!(create_empty_git_config(&workspace).is_err());
    assert!(verify_empty_git_config(&workspace, &path).is_err());
    assert!(std::fs::read(&target).unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn git_configuration_environment_uses_git_compatible_captured_paths() {
    let prepared = foreground_environment(
        [
            ("Git_Config_Global".into(), "/injected-global".into()),
            ("Git_Config_Count".into(), "4".into()),
            ("Agit_Session".into(), "unrelated@branch".into()),
        ],
        Path::new(r"\\?\C:\prepared\agit-home"),
        Path::new(r"\\?\C:\prepared\empty-git-config"),
    );
    for key in ["GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL"] {
        assert_eq!(
            prepared.iter().find(|(name, _)| name == key).unwrap().1,
            OsString::from(r"C:\prepared\empty-git-config")
        );
    }
    assert!(prepared.iter().all(|(key, _)| key != "AGIT_SESSION"));
    assert_eq!(
        prepared
            .iter()
            .filter(|(key, _)| key == "GIT_CONFIG_COUNT")
            .count(),
        1
    );
    assert_eq!(
        prepared
            .iter()
            .find(|(key, _)| key == "GIT_CONFIG_COUNT")
            .unwrap()
            .1,
        OsString::from("0")
    );
}

#[cfg(unix)]
#[test]
fn symlinked_stores_and_dangling_reports_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    std::os::unix::fs::symlink(outside.path(), workspace.join("agit-home")).unwrap();
    assert!(prepared_agent_home(&workspace).is_err());
    std::os::unix::fs::symlink(
        workspace.join("missing"),
        workspace.join("audit-report.json"),
    )
    .unwrap();
    assert!(require_absent_report(&workspace.join("audit-report.json")).is_err());
}

#[cfg(unix)]
#[test]
fn signalled_and_unsuccessful_exits_cannot_be_accepted() {
    use std::os::unix::process::ExitStatusExt;
    assert!(require_success(ExitStatus::from_raw(0)).is_ok());
    assert!(require_success(ExitStatus::from_raw(1 << 8)).is_err());
    assert!(require_success(ExitStatus::from_raw(libc::SIGINT)).is_err());
    assert!(require_success(ExitStatus::from_raw(libc::SIGTERM)).is_err());
}

#[cfg(windows)]
#[test]
fn nonzero_windows_exit_cannot_be_accepted() {
    use std::os::windows::process::ExitStatusExt;
    assert!(require_success(ExitStatus::from_raw(0)).is_ok());
    assert!(require_success(ExitStatus::from_raw(1)).is_err());
    assert!(require_success(ExitStatus::from_raw(0xc000013a)).is_err());
}
