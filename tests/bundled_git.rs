#![cfg(feature = "bundled-git")]

use agit::domain::{meta, repo::Repo};
use std::fs;
use std::process::Command;

#[test]
fn copied_cli_records_and_restores_lfs_without_system_git_or_profile_changes() {
    let lab = tempfile::tempdir().unwrap();
    let root = agit::infra::git_runtime::path_for_git(lab.path().canonicalize().unwrap());
    let home = root.join("profile");
    let store = home.join(".agit");
    let workspace = root.join("workspace");
    let install = root.join("installation with spaces");
    for path in [&home, &workspace, &install] {
        fs::create_dir_all(path).unwrap();
    }
    let primary = Repo::init(&store.join("repos/me/files")).unwrap();
    primary
        .set_remote("https://files.test/me/files.git")
        .unwrap();
    meta::write(primary.root(), &meta::Meta::new_file_line()).unwrap();
    agit::domain::storage::ensure_attributes(primary.root()).unwrap();
    fs::write(primary.root().join("README.md"), "# Files\n").unwrap();
    primary.add_all().unwrap();
    primary.commit("Initialize file line").unwrap();
    let profile = home.join(".gitconfig");
    let profile_bytes = b"[user]\nname = Profile identity\nemail = profile@example.invalid\n[commit]\ngpgsign = true\n";
    fs::write(&profile, profile_bytes).unwrap();
    let binary = install.join(format!("agit{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_agit"), &binary).unwrap();
    let call = |args: &[&str]| {
        let mut command = Command::new(&binary);
        command
            .args(["--quiet", "--no-tui"])
            .args(args)
            .current_dir(&workspace)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("AGIT_HOME", &store)
            .env("AGIT_SESSION", "me/files@main")
            .env("AGIT_HUB_URL", "http://127.0.0.1:9")
            .env("AGIT_TELEMETRY_DISABLED", "1")
            .env("GIT_CONFIG_GLOBAL", &profile)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_EXEC_PATH", root.join("wrong-git-exec-path"))
            .env("PATH", root.join("no-system-executables"))
            .env_remove("AGIT_USE_SYSTEM_GIT");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    libc::umask(0o002);
                    Ok(())
                });
            }
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let payload = b"A binary artifact\0with preserved bytes\n";
    let source = workspace.join("sample.mp4");
    fs::write(&source, payload).unwrap();
    call(&["file", "add", "--lfs", source.to_str().unwrap()]);
    call(&["file", "commit", "-m", "Record artifact with bundled Git"]);
    let restored = workspace.join("restored.mp4");
    call(&[
        "file",
        "get",
        "artifacts/sample.mp4",
        "--output",
        restored.to_str().unwrap(),
    ]);
    assert_eq!(fs::read(restored).unwrap(), payload);
    assert_eq!(fs::read(&profile).unwrap(), profile_bytes);
    assert!(!install.join("git").exists());
    let runtime_count = fs::read_dir(store.join("git-runtime"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
        .count();
    assert_eq!(
        runtime_count, 1,
        "CLI invocations must reuse the same complete runtime"
    );
}
