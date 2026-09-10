//! Ambiguous selectors refuse before refs or claims change, with diagnostics separate from data.

use agit::domain::{link, meta, storage, store::Store, transcript};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Lab {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    template: PathBuf,
    repos: Vec<PathBuf>,
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let store = temporary.path().join("agit");
        let work = temporary.path().join("work");
        let template = temporary.path().join("empty-template");
        for path in [&home, &work, &template] {
            fs::create_dir_all(path).unwrap();
        }
        let repos = ["alice", "bob"]
            .map(|owner| store.join("repos").join(owner).join("same"))
            .to_vec();
        let lab = Self {
            _temporary: temporary,
            home,
            store,
            work,
            template,
            repos,
        };
        for repo in &lab.repos {
            fs::create_dir_all(repo.join("session")).unwrap();
            lab.git(repo, &["init", "-q", "--initial-branch=main"]);
            fs::write(
                repo.join("session/meta.json"),
                "{\"layout\":\"v1\",\"line\":\"file\",\"kind\":\"file\"}\n",
            )
            .unwrap();
            fs::write(repo.join("AGENTS.md"), "SYNTHETIC SHARED FILE\n").unwrap();
            lab.git(repo, &["add", "."]);
            lab.git(repo, &["commit", "-qm", "synthetic file line"]);
        }
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_SESSION", "alice/same@main")
            .env("AGIT_TUI", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("absent-gitconfig"))
            .env("GIT_TEMPLATE_DIR", &self.template)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_AUTHOR_NAME", "Synthetic")
            .env("GIT_AUTHOR_EMAIL", "synthetic@example.test")
            .env("GIT_COMMITTER_NAME", "Synthetic")
            .env("GIT_COMMITTER_EMAIL", "synthetic@example.test")
            .stdin(Stdio::null())
            .current_dir(&self.work);
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn resumable() -> (Self, String, String) {
        let mut lab = Self::new();
        for (index, owner, name) in [(0, "local", "same"), (1, "bob", "other")] {
            let destination = lab.store.join("repos").join(owner).join(name);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::rename(&lab.repos[index], &destination).unwrap();
            lab.repos[index] = destination;
        }
        let repo = &lab.repos[0];
        lab.git(repo, &["switch", "-qc", "collision"]);
        let claim = format!("agit-{}", "a".repeat(40));
        let raw = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"user", "message":{"role":"user", "content":"SYNTHETIC CONTINUABLE SESSION"}}),
            serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":"SYNTHETIC SESSION REPLY"}}),
        );
        let events = transcript::wrap_lines(&raw, "claude-code", &claim);
        storage::write_snapshot(repo, &events, &events).unwrap();
        let mut snapshot = meta::Meta::new(
            claim.clone(),
            "claude-code".into(),
            lab.work
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        snapshot.turn = Some(1);
        meta::write(repo, &snapshot).unwrap();
        lab.git(repo, &["add", "."]);
        lab.git(repo, &["commit", "-qm", "synthetic resumable session"]);
        let head = lab.git(repo, &["rev-parse", "HEAD"]);
        assert_ne!(head, lab.git(repo, &["rev-parse", "main"]));
        assert!(
            !lab.command("git")
                .arg("-C")
                .arg(repo)
                .args(["cat-file", "-e", &format!("{}^{{commit}}", &claim[5..])])
                .output()
                .unwrap()
                .status
                .success(),
            "the declaration alias must not depend on a matching Git object"
        );
        fs::create_dir_all(lab.home.join(".claude/projects/unrelated")).unwrap();
        fs::write(
            lab.home.join(".claude/projects/unrelated/untouched.jsonl"),
            &raw,
        )
        .unwrap();
        (lab, head, claim)
    }

    fn continuation_files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let _ = self.state();
        [&self.home, &self.work]
            .into_iter()
            .chain(self.repos.iter())
            .flat_map(|root| walkdir::WalkDir::new(root).into_iter())
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let bytes = fs::read(entry.path()).unwrap();
                (entry.into_path(), bytes)
            })
            .collect()
    }

    fn materialize(&self, selector: &str, branch: &str, head: &str) {
        // A successful resume uses the file-line primary checkout and a linked session worktree.
        self.git(&self.repos[0], &["switch", "--quiet", "main"]);
        let refs_before = self.state().0;
        let output = self
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["run", selector, "--no-launch"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("materialized VIEW"), "{stdout}");
        assert!(!stdout.contains("forked out"), "{stdout}");
        assert_eq!(self.state().0, refs_before);
        let checkout = self.store.join("worktrees/local/same").join(branch);
        assert_eq!(
            self.git(&checkout, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{branch}")
        );
        assert_eq!(self.git(&checkout, &["rev-parse", "HEAD"]), head);
        let claims = link::list(&Store::at(self.store.join("store")));
        assert_eq!(claims.len(), 1, "{claims:?}");
        let claim = &claims[0];
        assert_eq!(claim.owner.as_deref(), Some("local"));
        assert_eq!(claim.agent.as_deref(), Some("same"));
        assert_eq!(claim.branch.as_deref(), Some(branch));
        assert_eq!(claim.materialized_from.as_deref(), Some(head));
        assert_eq!(claim.source, "claude-code");
        let name = format!("{}.jsonl", claim.session_id);
        let carriers = walkdir::WalkDir::new(self.home.join(".claude/projects"))
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file() && entry.file_name() == name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(carriers.len(), 1, "{carriers:?}");
        let native = fs::read_to_string(carriers[0].path()).unwrap();
        assert!(native.contains("SYNTHETIC CONTINUABLE SESSION"), "{native}");
        assert!(native.contains("SYNTHETIC SESSION REPLY"), "{native}");
    }

    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let output = self
            .command("git")
            .args(["-c", "commit.gpgsign=false", "-C"])
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn object(&self, kind: &str, bytes: &[u8]) -> String {
        let mut child = self
            .command("git")
            .arg("-C")
            .arg(&self.repos[0])
            .args(["hash-object", "-w", "-t", kind, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn state(&self) -> (Vec<String>, BTreeMap<PathBuf, Vec<u8>>) {
        let refs = self
            .repos
            .iter()
            .flat_map(|repo| {
                [
                    self.git(repo, &["show-ref"]),
                    self.git(repo, &["symbolic-ref", "HEAD"]),
                    self.git(repo, &["status", "--porcelain"]),
                ]
            })
            .collect();
        let mut claims = BTreeMap::new();
        let root = self.store.join("store");
        if root.exists() {
            for entry in walkdir::WalkDir::new(root) {
                let entry = entry.unwrap();
                if entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|suffix| suffix == "json")
                {
                    claims.insert(entry.path().to_owned(), fs::read(entry.path()).unwrap());
                }
            }
        }
        (refs, claims)
    }

    fn refusal(&self, flags: &[&str], args: &[&str], code: i32) -> String {
        let before = self.state();
        let output = self
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(flags)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
        assert_eq!(
            self.state(),
            before,
            "a refused selection changed local identity"
        );
        diagnostics(flags, &output)
    }

    fn commands(&self, selector: &str) -> Vec<Vec<String>> {
        vec![
            vec![
                "fork".into(),
                format!("alice/same@{selector}"),
                "-b".into(),
                "new".into(),
            ],
            vec!["run".into(), selector.into(), "--no-launch".into()],
            vec![
                "new".into(),
                "alice/same".into(),
                "--from".into(),
                selector.into(),
                "--no-launch".into(),
            ],
        ]
    }

    fn all_presentations(&self, selector: &str, code: i32, expected: &[&str]) {
        for flags in presentations() {
            for args in self.commands(selector) {
                let args = args.iter().map(String::as_str).collect::<Vec<_>>();
                let text = self.refusal(flags, &args, code);
                for needle in expected {
                    assert!(text.contains(needle), "missing {needle:?}: {text}");
                }
            }
        }
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

fn presentations() -> [&'static [&'static str]; 4] {
    [
        &[],
        &["--quiet"],
        &["--json", "--json-version", "1"],
        &["--json", "--json-version", "2"],
    ]
}

fn diagnostics(flags: &[&str], output: &Output) -> String {
    if flags.contains(&"--json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["ok"], false);
        assert_eq!(document["exit_code"], output.status.code().unwrap());
        assert_eq!(
            document["schema_version"],
            flags.last().unwrap().parse::<u64>().unwrap()
        );
        assert_eq!(document["result"]["format"], "empty");
        document["diagnostics"]["stderr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["message"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        assert!(output.stdout.is_empty(), "{output:?}");
        String::from_utf8(output.stderr.clone()).unwrap()
    }
}

#[test]
fn competing_named_refs_need_selection_without_mutation() {
    let lab = Lab::new();
    let repo = &lab.repos[0];
    lab.git(repo, &["branch", "collision", "main"]);
    lab.git(repo, &["tag", "collision", "main"]);
    lab.all_presentations("collision", 8, &["branch collision", "tag collision"]);
    lab.git(
        repo,
        &["update-ref", "refs/remotes/origin/remoteonly", "main"],
    );
    lab.git(
        repo,
        &["update-ref", "refs/remotes/upstream/remoteonly", "main"],
    );
    lab.all_presentations(
        "remoteonly",
        8,
        &["origin/remoteonly", "upstream/remoteonly"],
    );
    let prefix = lab.git(repo, &["rev-parse", "HEAD"])[..8].to_owned();
    lab.git(repo, &["branch", &prefix, "main"]);
    lab.all_presentations(&prefix, 8, &["branch", "sha prefix"]);
}

#[test]
fn a_writable_session_does_not_override_competing_named_refs() {
    let (lab, head, _) = Lab::resumable();
    let repo = &lab.repos[0];
    lab.git(repo, &["tag", "collision", "main"]);
    let before = lab.continuation_files();
    for flags in presentations() {
        let text = lab.refusal(flags, &["run", "same@collision", "--no-launch"], 8);
        assert!(
            text.contains("branch collision") && text.contains("tag collision"),
            "{text}"
        );
        assert_eq!(lab.continuation_files(), before);
    }
    lab.git(repo, &["tag", "-d", "collision"]);
    lab.git(
        repo,
        &["update-ref", "refs/remotes/origin/collision", &head],
    );
    lab.materialize("same@collision", "collision", &head);
}

#[test]
fn a_writable_hex_branch_does_not_override_an_object_prefix() {
    let (lab, head, _) = Lab::resumable();
    let prefix = &head[..8];
    lab.git(&lab.repos[0], &["branch", "-m", prefix]);
    let before = lab.continuation_files();
    for flags in presentations() {
        let selector = format!("same@{prefix}");
        let text = lab.refusal(flags, &["run", &selector, "--no-launch"], 8);
        assert!(
            text.contains("branch") && text.contains("sha prefix"),
            "{text}"
        );
        assert_eq!(lab.continuation_files(), before);
    }
}

#[test]
fn exact_web_ids_keep_their_selected_session_despite_a_tag_collision() {
    for declaration in [false, true] {
        let (lab, head, claim) = Lab::resumable();
        lab.git(&lab.repos[0], &["tag", "collision", "main"]);
        let id = if declaration {
            claim
        } else {
            format!("agit-{head}")
        };
        lab.materialize(&format!("same@{id}"), "collision", &head);
    }
}

#[test]
fn object_prefix_ambiguity_is_typed_and_full_ids_remain_exact() {
    let lab = Lab::new();
    let tree = lab.git(&lab.repos[0], &["rev-parse", "HEAD^{tree}"]);
    let mut seen = BTreeMap::new();
    let mut collision = None;
    for i in 0..10_000 {
        let data = format!("tree {tree}\nauthor Synthetic <synthetic@example.test> 1 +0000\ncommitter Synthetic <synthetic@example.test> 1 +0000\n\nsynthetic prefix {i}\n").into_bytes();
        let mut hash = Sha1::new();
        hash.update(format!("commit {}\0", data.len()).as_bytes());
        hash.update(&data);
        let oid = format!("{:x}", hash.finalize());
        if let Some(previous) = seen.insert(oid[..4].to_owned(), (oid.clone(), data.clone())) {
            collision = Some([previous, (oid, data)]);
            break;
        }
    }
    let collision = collision.expect("the bounded synthetic set must contain a prefix collision");
    for (oid, data) in &collision {
        assert_eq!(&lab.object("commit", data), oid);
    }
    lab.all_presentations(&collision[0].0[..4], 8, &["matches more than one object"]);
    for (oid, _) in &collision {
        let text = lab.refusal(&[], &["new", "alice/same", "--from", oid, "--no-launch"], 8);
        assert!(text.contains("requires -b"), "{text}");
        assert!(!text.contains("ambiguous") && !text.contains("more than one object"));
    }
}

#[test]
fn a_bare_repository_requires_an_explicit_owner_when_not_unique() {
    let lab = Lab::new();
    for flags in presentations() {
        let text = lab.refusal(flags, &["new", "same", "--no-launch"], 8);
        assert!(
            text.contains("alice/same") && text.contains("bob/same"),
            "{text}"
        );
        let text = lab.refusal(flags, &["new", "absent", "--no-launch"], 3);
        assert!(text.contains("no local repo"), "{text}");
    }
}

#[test]
fn missing_and_corrupt_objects_are_not_selection_requests() {
    let lab = Lab::new();
    lab.all_presentations("absent", 3, &["not a branch, tag, or commit prefix"]);
    let blob = lab.object("blob", b"SYNTHETIC NON-COMMIT\n");
    lab.git(&lab.repos[0], &["update-ref", "refs/tags/broken", &blob]);
    lab.all_presentations("broken", 3, &["does not resolve to a commit"]);
}

#[test]
fn local_qualifiers_refuse_missing_or_ambiguous_repositories_without_fallback() {
    let lab = Lab::new();
    for (selector, code) in [("same@main", 8), ("absent@main", 3)] {
        let turn_selector = format!("{selector}#1");
        for flags in presentations() {
            for args in [
                vec!["export", selector],
                vec!["scan", selector, "--secrets"],
                vec!["fork", selector, "-b", "new"],
                vec!["run", selector, "--no-launch"],
                vec!["new", selector, "--no-launch"],
                vec!["pull", selector],
                vec!["resume", selector, "--no-launch"],
                vec!["show", selector],
                vec!["cherry-pick", &turn_selector],
                vec!["revert", &turn_selector],
            ] {
                let text = lab.refusal(flags, &args, code);
                if code == 8 {
                    assert!(
                        text.contains("alice/same") && text.contains("bob/same"),
                        "{args:?}: {text}"
                    );
                } else {
                    assert!(text.contains("no local repo"), "{args:?}: {text}");
                }
            }
        }
    }
}

#[test]
fn unique_local_qualifiers_reach_branch_selection_in_the_named_checkout() {
    let mut lab = Lab::new();
    let unique = lab.store.join("repos/bob/unique");
    fs::rename(&lab.repos[1], &unique).unwrap();
    lab.repos[1] = unique;
    lab.git(&lab.repos[0], &["tag", "main", "refs/heads/main"]);
    for flags in presentations() {
        let text = lab.refusal(flags, &["new", "unique@main", "--no-launch"], 8);
        assert!(text.contains("requires -b"), "{text}");
        assert!(!text.contains("ambiguous"), "{text}");
        let text = lab.refusal(flags, &["pull", "unique@main"], 4);
        assert!(text.contains("no origin remote"), "{text}");
    }
}
