//! Real receivers distinguish frozen publication from a configuration preflight.

use super::*;
use crate::publication_test_process as process;
use std::time::Instant;

const CHILD: &str = "AGIT_FROZEN_FILE_TEST_ROOT";
const COMPLETE: &str = "frozen file boundary verified";

fn isolated(name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CHILD) {
        return Some(path.into());
    }
    let home = tempfile::tempdir().unwrap();
    for path in ["tmp", "home", "templates", "agit"] {
        std::fs::create_dir(home.path().join(path)).unwrap();
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.env_clear();
    for key in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
        if let Some(value) = std::env::var_os(key) {
            child.env(key, value);
        }
    }
    child
        .args([
            "--exact",
            &format!("hub::git::frozen::tests::{name}"),
            "--nocapture",
        ])
        .env(CHILD, home.path())
        .env("HOME", home.path().join("home"))
        .env("USERPROFILE", home.path().join("home"))
        .env("AGIT_HOME", home.path().join("agit"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.path().join("empty-config"))
        .env("GIT_TEMPLATE_DIR", home.path().join("templates"))
        .env("GIT_AUTHOR_NAME", "Frozen publication fixture")
        .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
        .env("GIT_COMMITTER_NAME", "Frozen publication fixture")
        .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
        .env("TMP", home.path().join("tmp"))
        .env("TEMP", home.path().join("tmp"))
        .env("TMPDIR", home.path().join("tmp"))
        .current_dir(home.path());
    let output = process::output(
        child,
        name,
        "isolated fixture",
        Instant::now() + process::MODE_LIMIT,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "isolated frozen fixture: {output:?}"
    );
    assert!(String::from_utf8(output.stdout).unwrap().contains(COMPLETE));
    None
}

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_ALLOW_PROTOCOL", "file");
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_EXEC_PATH",
    ] {
        command.env_remove(key);
    }
    let output = process::output(
        command,
        "frozen fixture",
        "owned Git",
        Instant::now() + process::CHILD_LIMIT,
    )
    .unwrap();
    assert!(output.status.success(), "Git fixture {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn identity() -> RemoteIdentity {
    RemoteIdentity::new(
        "https://fixture.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap()
}

fn receiver(root: &Path, name: &str, format: &str) -> PathBuf {
    let path = root.join(name);
    git(
        root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            &format!("--object-format={format}"),
            path.to_str().unwrap(),
        ],
    );
    path
}

fn file_url(path: &Path) -> String {
    let path = path_text(path).unwrap().replace('\\', "/");
    let prefix = if path.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    let mut url = prefix.to_owned();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/:._-~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

fn source(root: &Path, name: &str, format: &str) -> (Repo, PublicationPlan) {
    let path = root.join(name);
    git(
        root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={format}"),
            path.to_str().unwrap(),
        ],
    );
    git(&path, &["config", "commit.gpgsign", "false"]);
    git(&path, &["config", "tag.gpgsign", "false"]);
    std::fs::write(path.join("payload.txt"), b"captured shared files\n").unwrap();
    git(&path, &["add", "payload.txt"]);
    git(&path, &["commit", "-qm", "shared files"]);
    git(&path, &["checkout", "-qb", "selected"]);
    std::fs::write(path.join("payload.txt"), b"captured session files\n").unwrap();
    git(&path, &["commit", "-qam", "selected session"]);
    git(&path, &["tag", "-a", "inner", "-m", "inner metadata"]);
    git(
        &path,
        &[
            "-c",
            "advice.nestedTag=false",
            "tag",
            "-a",
            "outer",
            "inner",
            "-m",
            "outer metadata",
        ],
    );
    git(&path, &["tag", "-d", "inner"]);
    let repo = Repo::at(path);
    let plan = PublicationPlan::freeze(&repo, &["selected".into()]).unwrap();
    (repo, plan)
}

fn refs(path: &Path) -> BTreeMap<String, String> {
    git(path, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .map(|line| {
            let (name, oid) = line.split_once(' ').unwrap();
            (name.to_owned(), oid.to_owned())
        })
        .collect()
}

fn expected(plan: &PublicationPlan) -> BTreeMap<String, String> {
    plan.heads()
        .iter()
        .chain(plan.tags())
        .map(|reference| (reference.name().to_owned(), reference.oid().to_owned()))
        .collect()
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn build(repo: &Repo, plan: &PublicationPlan, destination: &Path) -> FrozenPublication {
    try_build(repo, plan, destination).unwrap()
}

fn try_build(repo: &Repo, plan: &PublicationPlan, destination: &Path) -> Result<FrozenPublication> {
    FrozenPublication::from_source(
        Source::new(repo)?,
        plan,
        file_url(destination),
        identity(),
        None,
        "file",
    )
}

fn set_environment(key: &str, value: impl AsRef<OsStr>) {
    // This test body runs alone in a child; no other fixture can inherit its temporary inputs.
    unsafe {
        std::env::set_var(key, value);
    }
}

#[test]
fn availability_agent_ignores_live_proxy_changes_after_source_capture() {
    let Some(root) = isolated("availability_agent_ignores_live_proxy_changes_after_source_capture")
    else {
        return;
    };
    let (repo, plan) = source(&root, "source", "sha1");
    let url = "https://fixture.invalid/owner/repo.git";
    let snapshot = Source::new(&repo).unwrap();
    set_environment("ALL_PROXY", "http://live-proxy.invalid:8080");
    let direct =
        FrozenPublication::from_source(snapshot, &plan, url.into(), identity(), None, "http:https")
            .unwrap();
    assert!(
        direct
            .prepared_lfs_http()
            .unwrap()
            .1
            .config()
            .proxy()
            .is_none()
    );
    set_environment("ALL_PROXY", "http://captured-proxy.invalid:8081");
    set_environment("HTTPS_PROXY", "http://shadow-proxy.invalid:8083");
    git(repo.root(), &["config", "http.userAgent", "git-agent"]);
    let scoped_agent = format!("http.{url}/info/lfs.userAgent");
    git(repo.root(), &["config", &scoped_agent, "captured-agent"]);
    let captured = FrozenPublication::from_source(
        Source::new(&repo).unwrap(),
        &plan,
        url.into(),
        identity(),
        None,
        "http:https",
    )
    .unwrap();
    set_environment("ALL_PROXY", "http://changed-proxy.invalid:8082");
    set_environment("NO_PROXY", "*");
    git(repo.root(), &["config", "http.userAgent", "changed-agent"]);
    git(repo.root(), &["config", &scoped_agent, "changed-agent"]);
    let config = captured.prepared_lfs_http().unwrap().1.config();
    assert_eq!(config.proxy().unwrap().host(), "captured-proxy.invalid");
    assert_eq!(config.proxy().unwrap().port(), 8081);
    assert!(
        matches!(config.user_agent(), ureq::config::AutoHeaderValue::Provided(value) if value.as_str() == "captured-agent")
    );
    assert_eq!(captured.url(), url);
    assert_eq!(
        captured.prepared_lfs_http().unwrap().0,
        format!("{url}/info/lfs")
    );
    assert_eq!(captured.identity(), &identity());
    assert!(captured.transport.lfs.is_none());
    git(repo.root(), &["config", "http.sslCert", "client.pem"]);
    let unsupported = FrozenPublication::from_source(
        Source::new(&repo).unwrap(),
        &plan,
        url.into(),
        identity(),
        None,
        "http:https",
    )
    .unwrap();
    assert!(unsupported.prepared_lfs_http().is_err());
    git(repo.root(), &["config", "--unset", "http.sslCert"]);
    assert!(unsupported.prepared_lfs_http().is_err());
    let pem_path = root.join("malformed-ca.pem");
    std::fs::write(&pem_path, "-----BEGIN PRIVATE_PEM_SENTINEL\n").unwrap();
    let scoped_ca = format!("http.{url}/info/lfs.sslCAInfo");
    git(
        repo.root(),
        &["config", &scoped_ca, pem_path.to_str().unwrap()],
    );
    let malformed = FrozenPublication::from_source(
        Source::new(&repo).unwrap(),
        &plan,
        url.into(),
        identity(),
        None,
        "http:https",
    )
    .unwrap();
    git(repo.root(), &["config", "--unset", &scoped_ca]);
    std::fs::remove_file(&pem_path).unwrap();
    assert_eq!(
        format!("{:#}", malformed.prepared_lfs_http().unwrap_err()),
        "availability CA bundle must be a readable bounded PEM certificate file"
    );
    let secret_key = format!("http.{url}/info/lfs.privateConfigSentinel");
    git(
        repo.root(),
        &["config", &secret_key, "PRIVATE_CONFIG_VALUE"],
    );
    let invalid = FrozenPublication::from_source(
        Source::new(&repo).unwrap(),
        &plan,
        url.into(),
        identity(),
        None,
        "http:https",
    )
    .unwrap();
    git(repo.root(), &["config", "--unset", &secret_key]);
    let error = invalid.prepared_lfs_http().unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "cannot capture availability HTTP configuration"
    );
    assert!(!format!("{error:?}").contains("Sentinel"));
    assert!(!format!("{error:?}").contains("PRIVATE_CONFIG_VALUE"));
    println!("{COMPLETE}");
}

#[test]
fn source_refs_rewrites_and_injected_configuration_cannot_redirect_publication() {
    let name = "source_refs_rewrites_and_injected_configuration_cannot_redirect_publication";
    let Some(root) = isolated(name) else { return };
    let (repo, plan) = source(&root, "source with spaces", "sha1");
    let target = receiver(&root, "intended.git", "sha1");
    let wrong = receiver(&root, "wrong.git", "sha1");
    let target_url = file_url(&target);
    let wrong_url = file_url(&wrong);
    let include = root.join("included-config");
    std::fs::write(&include, b"[http]\nuserAgent = prepared-agent\n").unwrap();
    git(
        repo.root(),
        &["config", "include.path", include.to_str().unwrap()],
    );
    git(
        repo.root(),
        &["config", "extensions.worktreeConfig", "true"],
    );
    for suffix in ["insteadOf", "pushInsteadOf"] {
        git(
            repo.root(),
            &[
                "config",
                "--worktree",
                &format!("url.{wrong_url}.{suffix}"),
                &target_url,
            ],
        );
    }
    git(repo.root(), &["config", "remote.origin.url", &target_url]);
    git(
        repo.root(),
        &["config", "--add", "remote.origin.pushurl", &target_url],
    );
    git(
        repo.root(),
        &["config", "--add", "remote.origin.pushurl", &wrong_url],
    );
    git(repo.root(), &["config", "push.followTags", "true"]);
    git(repo.root(), &["config", "push.recurseSubmodules", "only"]);
    let hooks = root.join("unapproved-hooks");
    std::fs::create_dir(&hooks).unwrap();
    std::fs::write(hooks.join("pre-push"), b"#!/bin/sh\nexit 99\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            hooks.join("pre-push"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    git(
        repo.root(),
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    set_environment("GIT_CONFIG_COUNT", "1");
    set_environment("GIT_CONFIG_KEY_0", format!("url.{wrong_url}.pushInsteadOf"));
    set_environment("GIT_CONFIG_VALUE_0", &target_url);
    set_environment("GIT_DIR", &wrong);
    set_environment("GIT_OBJECT_DIRECTORY", wrong.join("objects"));
    let shadow = root.join("query-only-config");
    std::fs::write(&shadow, b"[http]\nuserAgent = query-only-agent\n").unwrap();
    set_environment("GIT_CONFIG", &shadow);
    let publication = build(&repo, &plan, &target);
    let private = publication.directory.path().to_owned();
    let base = plan
        .heads()
        .iter()
        .find(|reference| reference.name() == "refs/heads/main")
        .unwrap()
        .oid();
    for name in ["refs/heads/selected", "refs/tags/outer"] {
        git(repo.root(), &["update-ref", name, base]);
    }
    git(
        repo.root(),
        &["tag", "-a", "unapproved", "-m", "unapproved metadata", base],
    );
    std::fs::write(&include, b"[http]\nuserAgent = changed-agent\n").unwrap();
    for name in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        set_environment(name, &include);
    }
    set_environment("GIT_CONFIG_NOSYSTEM", "0");
    set_environment(
        "GIT_CONFIG_PARAMETERS",
        "'core.bare'='false' 'http.userAgent'='injected-agent'",
    );
    set_environment("GIT_EXEC_PATH", root.join("missing-executables"));
    let before = inventory(repo.root());
    let outcome = publication.push_refs();
    assert!(
        (outcome.0.ok() && outcome.1.as_ref().is_some_and(|phase| phase.ok())),
        "frozen publication failed: {outcome:?}"
    );
    assert_eq!(refs(&target), expected(&plan));
    assert!(refs(&wrong).is_empty());
    assert_eq!(inventory(repo.root()), before);
    let execution = publication.transport.execution.as_ref().unwrap();
    let mut probe = execution.command();
    probe.args(["config", "--get", "http.userAgent"]);
    probe.envs(publication.transport.environment().unwrap());
    let output = bounded_inspection_output(probe, 1024).unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"prepared-agent\n");
    drop(publication);
    assert!(
        !private.exists(),
        "private execution context must be cleaned up"
    );
    // The unisolated control follows the same source rewrite and reads the moved source ref.
    git(
        repo.root(),
        &[
            "push",
            "--no-verify",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &target_url,
            "selected",
        ],
    );
    assert_eq!(
        refs(&wrong),
        BTreeMap::from([("refs/heads/selected".into(), base.into())])
    );
    println!("{COMPLETE}");
}

#[test]
fn linked_bare_and_sha256_sources_use_the_captured_object_store() {
    let Some(root) = isolated("linked_bare_and_sha256_sources_use_the_captured_object_store")
    else {
        return;
    };
    for (name, format, bare) in [
        ("linked", "sha1", false),
        ("bare", "sha1", true),
        ("sha256", "sha256", true),
    ] {
        let (repo, plan) = source(&root, &format!("source-{name}"), format);
        let carrier = root.join(format!("carrier {name} café"));
        if bare {
            git(
                &root,
                &[
                    "clone",
                    "--quiet",
                    "--bare",
                    repo.root().to_str().unwrap(),
                    carrier.to_str().unwrap(),
                ],
            );
        } else {
            git(
                repo.root(),
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    carrier.to_str().unwrap(),
                    "selected",
                ],
            );
        }
        let receiver = receiver(&root, &format!("receiver-{name}.git"), format);
        let carrier = Repo::at(carrier);
        let common = prepare_policy_snapshot(&carrier);
        let common_before = cache_disk_snapshot(&common);
        let before = inventory(carrier.root());
        let publication = build(&carrier, &plan, &receiver);
        let (heads, tags) = publication.push_refs();
        assert!(heads.ok() && tags.as_ref().is_some_and(|phase| phase.ok()));
        assert_eq!(refs(&receiver), expected(&plan));
        assert_eq!(inventory(carrier.root()), before);
        assert_eq!(cache_disk_snapshot(&common), common_before);
    }
    println!("{COMPLETE}");
}

fn recorded_status(
    phase: &super::super::PublicationPhase,
    name: &str,
) -> super::super::PublicationStatus {
    phase
        .attempts
        .iter()
        .rev()
        .flat_map(|attempt| &attempt.refs)
        .find(|reference| reference.reference.name() == name)
        .unwrap()
        .status
}

#[test]
fn partial_branch_rejection_retains_accepted_refs_and_stops_before_tags() {
    let Some(root) =
        isolated("partial_branch_rejection_retains_accepted_refs_and_stops_before_tags")
    else {
        return;
    };
    let (repo, _) = source(&root, "source", "sha1");
    git(repo.root(), &["branch", "z-later", "selected"]);
    let plan = PublicationPlan::freeze(&repo, &["selected".into(), "z-later".into()]).unwrap();
    let base = git(repo.root(), &["rev-parse", "main"]);
    let tree = git(repo.root(), &["rev-parse", "main^{tree}"]);
    let diverged = git(
        repo.root(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &base,
            "-m",
            "independent destination history",
        ],
    );
    for split in [false, true] {
        let target = receiver(&root, &format!("receiver-{split}.git"), "sha1");
        git(
            repo.root(),
            &[
                "push",
                "--no-follow-tags",
                "--recurse-submodules=no",
                &file_url(&target),
                &format!("{diverged}:refs/heads/selected"),
            ],
        );
        let mut publication = build(&repo, &plan, &target);
        if split {
            // Separate requests expose a later failure without changing the selected refs.
            publication.heads = plan
                .heads()
                .iter()
                .cloned()
                .map(|reference| vec![reference])
                .collect();
        }
        let before = inventory(repo.root());
        let result = publication.push_refs();
        assert!(!(result.0.ok() && result.1.as_ref().is_some_and(|phase| phase.ok())));
        assert!(!result.0.ok());
        assert!(result.1.is_none());
        assert_eq!(result.0.attempts.len(), if split { 2 } else { 1 });
        if split {
            assert!(result.0.attempts[0].ok());
            assert_eq!(result.0.attempts[0].batch, 0);
            assert_eq!(result.0.attempts[1].batch, 1);
            assert_eq!(result.0.unattempted, vec![plan.heads()[2].clone()]);
        } else {
            assert!(result.0.unattempted.is_empty());
            assert_eq!(
                recorded_status(&result.0, "refs/heads/z-later"),
                super::super::PublicationStatus::Updated
            );
        }
        assert_eq!(
            recorded_status(&result.0, "refs/heads/main"),
            super::super::PublicationStatus::Updated
        );
        assert_eq!(
            recorded_status(&result.0, "refs/heads/selected"),
            super::super::PublicationStatus::Unconfirmed
        );
        let mut expected_remote = BTreeMap::from([
            ("refs/heads/main".into(), base.clone()),
            ("refs/heads/selected".into(), diverged.clone()),
        ]);
        if !split {
            expected_remote.insert(
                "refs/heads/z-later".into(),
                plan.heads()[2].oid().to_owned(),
            );
        }
        assert_eq!(refs(&target), expected_remote);
        assert_eq!(inventory(repo.root()), before);
    }
    println!("{COMPLETE}");
}

#[test]
fn tag_rejection_preserves_branch_success_and_quiet_keeps_machine_acknowledgments() {
    let Some(root) =
        isolated("tag_rejection_preserves_branch_success_and_quiet_keeps_machine_acknowledgments")
    else {
        return;
    };
    let (repo, _) = source(&root, "source", "sha1");
    git(repo.root(), &["tag", "new", "selected"]);
    let plan = PublicationPlan::freeze(&repo, &["selected".into()]).unwrap();
    let target = receiver(&root, "receiver.git", "sha1");
    let base = git(repo.root(), &["rev-parse", "main"]);
    git(
        repo.root(),
        &[
            "push",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &file_url(&target),
            &format!("{base}:refs/tags/outer"),
        ],
    );
    let publication = build(&repo, &plan, &target);
    let before = inventory(repo.root());
    let result = publication.push_refs();
    assert!(!(result.0.ok() && result.1.as_ref().is_some_and(|phase| phase.ok())));
    assert!(result.0.ok());
    let tags = result.1.as_ref().unwrap();
    assert!(!tags.ok());
    assert_eq!(
        recorded_status(tags, "refs/tags/new"),
        super::super::PublicationStatus::Updated
    );
    assert_eq!(
        recorded_status(tags, "refs/tags/outer"),
        super::super::PublicationStatus::Unconfirmed
    );
    let mut remote = expected(&plan);
    remote.insert("refs/tags/outer".into(), base);
    assert_eq!(refs(&target), remote);
    assert_eq!(inventory(repo.root()), before);

    let complete_target = receiver(&root, "complete.git", "sha1");
    let complete = build(&repo, &plan, &complete_target);
    let (heads, tags) = complete.push_refs();
    assert!(heads.ok() && tags.as_ref().is_some_and(|phase| phase.ok()));
    set_environment("AGIT_QUIET", "1");
    let repeated = complete.push_refs();
    assert!(
        repeated.0.ok() && repeated.1.as_ref().is_some_and(|phase| phase.ok()),
        "quiet up-to-date publication: {repeated:?}"
    );
    for phase in [&repeated.0, repeated.1.as_ref().unwrap()] {
        assert!(
            phase
                .attempts
                .iter()
                .flat_map(|attempt| &attempt.refs)
                .all(|reference| reference.status == super::super::PublicationStatus::UpToDate)
        );
    }
    assert_eq!(refs(&complete_target), expected(&plan));
    println!("{COMPLETE}");
}

#[test]
fn missing_duplicate_malformed_and_interrupted_records_cannot_complete_a_batch() {
    let Some(root) =
        isolated("missing_duplicate_malformed_and_interrupted_records_cannot_complete_a_batch")
    else {
        return;
    };
    let (_, plan) = source(&root, "source", "sha1");
    let refs = plan.heads();
    let valid: String = refs
        .iter()
        .map(|reference| format!("*\t{}\t[new branch]\n", reference.refspec()))
        .collect();
    let first = format!("*\t{}\t[new branch]\n", refs[0].refspec());
    let parse = |stdout: Vec<u8>, complete, code| {
        PublicationAttempt::parse(
            0,
            refs,
            super::super::ProcessOutput {
                outcome: super::super::Outcome {
                    code,
                    stderr: String::new(),
                },
                stdout,
                complete,
                error: None,
            },
        )
    };
    assert!(parse(valid.as_bytes().to_vec(), true, 0).ok());
    for bytes in [
        first.as_bytes().to_vec(),
        format!("{valid}{first}").into_bytes(),
        format!("{valid}malformed\n").into_bytes(),
        valid.trim_end().as_bytes().to_vec(),
        format!("{valid}!\tunknown:refs/heads/unselected\t[remote failure]\n").into_bytes(),
        [valid.as_bytes(), &[0xff, b'\n']].concat(),
    ] {
        let result = parse(bytes, true, 0);
        assert!(!result.complete);
        assert!(!result.ok());
    }
    let duplicate = parse(format!("{valid}{first}").into_bytes(), true, 0);
    assert_eq!(
        duplicate.refs[0].status,
        super::super::PublicationStatus::Unconfirmed
    );
    let interrupted = parse(first.into_bytes(), false, 1);
    assert_eq!(
        interrupted.refs[0].status,
        super::super::PublicationStatus::Updated
    );
    assert_eq!(
        interrupted.refs[1].status,
        super::super::PublicationStatus::Unconfirmed
    );
    assert!(!interrupted.ok());
    assert!(!parse(valid.into_bytes(), false, 0).ok());
    println!("{COMPLETE}");
}

#[test]
fn unsupported_tls_and_authentication_preferences_refuse_before_transport() {
    let Some(root) =
        isolated("unsupported_tls_and_authentication_preferences_refuse_before_transport")
    else {
        return;
    };
    let (repo, plan) = source(&root, "source", "sha1");
    let receiver = receiver(&root, "receiver.git", "sha1");
    for (key, value) in [
        ("http.sslVerify", "false"),
        ("http.cookieFile", "cookies"),
        ("http.sslCertPasswordProtected", "true"),
        ("http.sslKeyType", "ENG"),
        ("http.sslCertType", "PROV"),
    ] {
        git(repo.root(), &["config", key, value]);
        assert!(
            try_build(&repo, &plan, &receiver).is_err(),
            "unsupported preference must refuse: {key}"
        );
        git(repo.root(), &["config", "--unset", key]);
    }
    set_environment("GIT_SSL_NO_VERIFY", "true");
    assert!(try_build(&repo, &plan, &receiver).is_err());
    assert!(refs(&receiver).is_empty());
    let nested = repo.root().join("not-a-repository");
    std::fs::create_dir(&nested).unwrap();
    assert!(Source::new(&Repo::at(nested)).is_err());
    for url in [
        "origin",
        "https://elsewhere.invalid/repo.git",
        "https://fixture.invalid/a/../b.git",
        "https://fixture.invalid/repo.git?extra=true",
    ] {
        assert!(FrozenPublication::prepare(&repo, &plan, url, &identity()).is_err());
    }
    println!("{COMPLETE}");
}

#[test]
fn tls_paths_and_environment_are_captured_before_publication() {
    let Some(root) = isolated("tls_paths_and_environment_are_captured_before_publication") else {
        return;
    };
    let (repo, plan) = source(&root, "source", "sha1");
    let receiver = receiver(&root, "receiver.git", "sha1");
    #[cfg(windows)]
    {
        let path = std::env::var_os("PATH").unwrap();
        // The isolated child exercises native case-insensitive environment lookup.
        unsafe {
            std::env::remove_var("PATH");
        }
        set_environment("Path", path);
    }
    let ca_key = if cfg!(windows) {
        "git_ssl_cainfo"
    } else {
        "GIT_SSL_CAINFO"
    };
    let expected_root = inspection_git_path_spelling(repo.root().canonicalize().unwrap());
    let expected_ca = path_text(&expected_root.join("certs/root.pem")).unwrap();
    set_environment(ca_key, "certs/root.pem");
    set_environment("GIT_PROXY_SSL_CAINFO", "certs/proxy.pem");
    set_environment("GIT_SSL_CERT_TYPE", "P12");
    set_environment("GIT_SSL_KEY_TYPE", "DER");
    set_environment("http_proxy", "http://127.0.0.1:1");
    git(
        repo.root(),
        &["config", "http.sslCAInfo", "discarded-by-environment.pem"],
    );
    git(repo.root(), &["config", "http.sslCertType", "PEM"]);
    git(repo.root(), &["config", "http.sslKeyType", "PEM"]);
    let publication = build(&repo, &plan, &receiver);
    set_environment(ca_key, "changed-after-preparation.pem");
    set_environment("GIT_SSL_CERT_TYPE", "PEM");
    set_environment("GIT_SSL_KEY_TYPE", "PEM");
    set_environment("http_proxy", "http://127.0.0.1:2");
    let execution = publication.transport.execution.as_ref().unwrap();
    for (key, expected) in [
        ("http.sslCAInfo", expected_ca),
        (
            "http.proxySSLCAInfo",
            path_text(&expected_root.join("certs/proxy.pem")).unwrap(),
        ),
        ("http.sslCertType", "P12".into()),
        ("http.sslKeyType", "DER".into()),
    ] {
        let mut command = execution.command();
        command.args(["config", "--get", key]);
        command.envs(publication.transport.environment().unwrap());
        let output = bounded_inspection_output(command, 4096).unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim_end(),
            expected
        );
    }
    let proxy_key = if cfg!(windows) {
        "HTTP_PROXY"
    } else {
        "http_proxy"
    };
    assert_eq!(
        execution.environment.get(OsStr::new(proxy_key)).unwrap(),
        "http://127.0.0.1:1"
    );
    let (heads, tags) = publication.push_refs();
    assert!(heads.ok() && tags.as_ref().is_some_and(|phase| phase.ok()));
    assert_eq!(refs(&receiver), expected(&plan));
    println!("{COMPLETE}");
}

#[test]
fn windows_argument_and_environment_expansion_is_bounded() {
    let quote_dense = format!("{}:refs/tags/{}", "a".repeat(40), "\"".repeat(12 * 1024));
    assert!(argument_units(&quote_dense) > REQUEST_UNITS);
    assert!(argument_units("a\\\"b") >= "\"a\\\\\\\"b\" ".encode_utf16().count());
    let execution = Execution {
        root: PathBuf::new(),
        environment: BTreeMap::new(),
        parameters: OsString::new(),
    };
    assert!(
        execution
            .validate_parameters(OsStr::new(&"x".repeat(28 * 1024)))
            .is_ok()
    );
    assert!(
        execution
            .validate_parameters(OsStr::new(&"x".repeat(28 * 1024 + 1)))
            .is_err()
    );
    assert!(
        execution
            .validate_parameters(OsStr::new(&"😀".repeat(14 * 1024 + 1)))
            .is_err()
    );
}

fn cache_disk_snapshot(root: &Path) -> BTreeMap<PathBuf, (bool, Vec<u8>)> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .map(|entry| {
            let bytes = if entry.file_type().is_file() {
                std::fs::read(entry.path()).unwrap()
            } else if entry.file_type().is_symlink() {
                std::fs::read_link(entry.path())
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else {
                Vec::new()
            };
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                (entry.file_type().is_dir(), bytes),
            )
        })
        .collect()
}

fn prepare_policy_snapshot(repo: &Repo) -> PathBuf {
    let source = Source::new(repo).unwrap();
    let common = source.text(&["rev-parse", "--git-common-dir"]).unwrap();
    let common = absolute_path(&source.root, &common)
        .unwrap()
        .canonicalize()
        .unwrap();
    let expected = cache_disk_snapshot(&common);
    #[cfg(feature = "secret-vault")]
    let expected = {
        let mut expected = expected;
        for path in ["agit", "agit/secret-dictionary"] {
            let entry = expected.entry(path.into()).or_insert((true, Vec::new()));
            assert_eq!(*entry, (true, Vec::new()));
        }
        assert!(
            expected
                .insert(
                    "agit/secret-dictionary/vault.lock".into(),
                    (false, Vec::new())
                )
                .is_none()
        );
        assert!(!expected.contains_key(Path::new("agit/secret-dictionary/vault.json")));
        expected
    };
    crate::domain::secrets::publication::CapturedPolicy::capture(&common).unwrap();
    assert_eq!(cache_disk_snapshot(&common), expected);
    #[cfg(feature = "secret-vault")]
    std::fs::write(
        common.join("agit/secret-dictionary/vault.lock"),
        b"opaque fixture lock contents\n",
    )
    .unwrap();
    common
}

fn capture_cache_without_writes(repo: &Repo) -> PathBuf {
    let before = cache_disk_snapshot(repo.root());
    let path = lfs_cache::capture(&Source::new(repo).unwrap()).unwrap();
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    path
}

#[test]
fn lfs_cache_capture_uses_raw_config_and_captured_environment_without_initialization() {
    let name = "lfs_cache_capture_uses_raw_config_and_captured_environment_without_initialization";
    let Some(root) = isolated(name) else { return };
    let path = root.join("source");
    git(&root, &["init", "-q", "-b", "main", path.to_str().unwrap()]);
    let repo = Repo::at(path);
    let gitdir = inspection_git_path_spelling(repo.root().join(".git").canonicalize().unwrap());
    std::fs::write(
        repo.root().join(".lfsconfig"),
        "[lfs]\n storage = ignored-lfsconfig\n",
    )
    .unwrap();
    assert_eq!(
        capture_cache_without_writes(&repo),
        gitdir.join("lfs/objects")
    );
    let absolute = inspection_git_path_spelling(root.join("outside-cache"));
    for (value, expected) in [
        ("".to_owned(), gitdir.join("lfs/objects")),
        ("cache".into(), gitdir.join("cache/objects")),
        (
            "../shared/cache".into(),
            gitdir.parent().unwrap().join("shared/cache/objects"),
        ),
        ("~/literal".into(), gitdir.join("~/literal/objects")),
        ("a/../cache".into(), gitdir.join("cache/objects")),
        (" spaced ".into(), gitdir.join(" spaced /objects")),
        (absolute.to_str().unwrap().into(), absolute.join("objects")),
    ] {
        git(repo.root(), &["config", "lfs.storage", &value]);
        assert_eq!(capture_cache_without_writes(&repo), expected, "{value}");
        assert!(!expected.exists());
    }
    git(repo.root(), &["config", "--unset", "lfs.storage"]);
    git(
        repo.root(),
        &["config", "--global", "lfs.storage", "global-cache"],
    );
    assert_eq!(
        capture_cache_without_writes(&repo),
        gitdir.join("global-cache/objects")
    );
    std::fs::write(
        gitdir.join("cache-config"),
        "[lfs]\n storage = earlier-cache\n storage = included-cache\n",
    )
    .unwrap();
    git(repo.root(), &["config", "include.path", "cache-config"]);
    assert_eq!(
        capture_cache_without_writes(&repo),
        gitdir.join("included-cache/objects")
    );
    set_environment("GIT_CONFIG_COUNT", "1");
    set_environment("GIT_CONFIG_KEY_0", "lfs.storage");
    set_environment("GIT_CONFIG_VALUE_0", "captured-cache");
    let snapshot = Source::new(&repo).unwrap();
    set_environment("GIT_CONFIG_VALUE_0", "changed-cache");
    set_environment("GIT_DIR", root.join("unrelated.git"));
    set_environment("GIT_CONFIG_GLOBAL", root.join("changed-global"));
    let before = cache_disk_snapshot(repo.root());
    assert_eq!(
        lfs_cache::capture(&snapshot).unwrap(),
        gitdir.join("captured-cache/objects")
    );
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    assert!(!gitdir.join("lfs").exists());
    assert!(!gitdir.join("captured-cache").exists());
    println!("{COMPLETE}");
}

#[test]
fn lfs_cache_capture_tracks_admitted_git_directories_and_the_objects_exception() {
    let name = "lfs_cache_capture_tracks_admitted_git_directories_and_the_objects_exception";
    let Some(root) = isolated(name) else { return };
    let (repo, _) = source(&root, "source", "sha1");
    let gitdir = inspection_git_path_spelling(repo.root().join(".git").canonicalize().unwrap());
    let linked = root.join("linked");
    git(
        repo.root(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let linked = Repo::at(linked);
    let before = cache_disk_snapshot(repo.root());
    assert_eq!(
        capture_cache_without_writes(&linked),
        gitdir.join("lfs/objects")
    );
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    let linked_gitdir = Source::new(&linked)
        .unwrap()
        .text(&["rev-parse", "--absolute-git-dir"])
        .unwrap();
    let linked_gitdir =
        inspection_git_path_spelling(Path::new(&linked_gitdir).canonicalize().unwrap());
    std::fs::create_dir(linked_gitdir.join("objects")).unwrap();
    let before = cache_disk_snapshot(repo.root());
    assert_eq!(
        capture_cache_without_writes(&linked),
        linked_gitdir.join("lfs/objects")
    );
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    let separate = root.join("separate");
    let separate_gitdir = root.join("separate.git");
    git(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            separate_gitdir.to_str().unwrap(),
            separate.to_str().unwrap(),
        ],
    );
    let separate_gitdir = inspection_git_path_spelling(separate_gitdir.canonicalize().unwrap());
    let before = cache_disk_snapshot(&separate_gitdir);
    assert_eq!(
        capture_cache_without_writes(&Repo::at(separate)),
        separate_gitdir.join("lfs/objects")
    );
    assert_eq!(cache_disk_snapshot(&separate_gitdir), before);
    let bare = receiver(&root, "bare.git", "sha1");
    let bare = inspection_git_path_spelling(bare.canonicalize().unwrap());
    assert_eq!(
        capture_cache_without_writes(&Repo::at(bare.clone())),
        bare.join("lfs/objects")
    );
    for root in [&gitdir, &linked_gitdir, &separate_gitdir, &bare] {
        assert!(!root.join("lfs").exists());
    }
    let nested = repo.root().join("nested");
    std::fs::create_dir(&nested).unwrap();
    assert!(Source::new(&Repo::at(nested.clone())).is_err());
    std::fs::create_dir(nested.join(".git")).unwrap();
    let before = cache_disk_snapshot(repo.root());
    assert_eq!(
        lfs_cache::capture(&Source::new(&Repo::at(nested)).unwrap()).unwrap_err(),
        lfs_cache::Failure::GitDirectory
    );
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    println!("{COMPLETE}");
}

#[test]
fn frozen_cache_path_and_deferred_failure_survive_later_source_changes() {
    let name = "frozen_cache_path_and_deferred_failure_survive_later_source_changes";
    let Some(root) = isolated(name) else { return };
    let (repo, plan) = source(&root, "source", "sha1");
    let receiver = receiver(&root, "receiver.git", "sha1");
    let gitdir = inspection_git_path_spelling(repo.root().join(".git").canonicalize().unwrap());
    git(repo.root(), &["config", "lfs.storage", "selected-cache"]);
    prepare_policy_snapshot(&repo);
    let before = cache_disk_snapshot(repo.root());
    let publication = build(&repo, &plan, &receiver);
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    assert_eq!(
        publication.source_lfs_objects().unwrap(),
        gitdir.join("selected-cache/objects")
    );
    git(repo.root(), &["config", "lfs.storage", "changed-cache"]);
    git(
        repo.root(),
        &[
            "config",
            "remote.origin.url",
            "https://changed.invalid/repo.git",
        ],
    );
    std::fs::write(gitdir.join("commondir"), "../changed-common").unwrap();
    set_environment("HOME", root.join("changed-home"));
    let before = cache_disk_snapshot(repo.root());
    assert_eq!(
        publication.source_lfs_objects().unwrap(),
        gitdir.join("selected-cache/objects")
    );
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    std::fs::remove_file(gitdir.join("commondir")).unwrap();
    git(repo.root(), &["config", "lfs.storage", "PRIVATE\nCACHE"]);
    let before = cache_disk_snapshot(repo.root());
    let incomplete = build(&repo, &plan, &receiver);
    assert_eq!(cache_disk_snapshot(repo.root()), before);
    git(repo.root(), &["config", "--unset", "lfs.storage"]);
    assert_eq!(
        format!("{:#}", incomplete.source_lfs_objects().unwrap_err()),
        "captured LFS cache path is unsupported"
    );
    assert_eq!(incomplete.url(), file_url(&receiver));
    assert!(incomplete.transport.lfs.is_none());
    assert!(!gitdir.join("selected-cache").exists());
    assert!(!gitdir.join("changed-cache").exists());
    assert!(!gitdir.join("lfs").exists());
    println!("{COMPLETE}");
}

#[test]
fn local_capture_inspects_without_a_hub_account_or_remote_identity() {
    let Some(root) = isolated("local_capture_inspects_without_a_hub_account_or_remote_identity")
    else {
        return;
    };
    let (repo, plan) = source(&root, "unpublished", "sha1");
    assert!(crate::hub::identity::read(&repo).unwrap().is_none());
    let config = std::fs::read(repo.root().join(".git/config")).unwrap();
    let captured = CapturedPublication::capture(&repo, &plan, 0).unwrap();
    assert_eq!(refs(captured.snapshot_git_dir()), expected(&plan));
    let ContentInspection::Complete(complete) =
        captured.inspect(crate::domain::secrets::ScanLimits::DEFAULT)
    else {
        panic!("unpublished local content must be inspectable without a destination");
    };
    complete.verify_source(&repo).unwrap();
    assert!(!complete.has_findings());
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        config
    );
    println!("{COMPLETE}");
}
