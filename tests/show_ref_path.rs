//! `<ref>:<path>` is the verbatim file in the tree, `export` is that line's transcript — two
//! read paths over the same tree.
//!
//! v0 does not reserve the root names `LOG` / `VIEW`, so the tree of a v0 session line can carry
//! both a `LOG` user file the author committed and the real transcript `session/log.jsonl`. Once
//! two leaves are squeezed into one return value, one of the two directions must break: either
//! `agit export` exports plain text, or `agit show <ref>:LOG` prints the whole session record.
//! This pins the wiring on both CLI sides at once.

use agit::domain::meta::{self, LayoutVersion, Meta};
use agit::domain::repo::Repo;
use agit::domain::transcript;
use std::{fs, process::Command};

const USER_LOG: &str = "this is a user file named LOG, not a transcript\n";
const USER_VIEW: &str = "this is a user file named VIEW, not a transcript\n";

#[test]
fn explicit_ref_path_reads_the_tree_blob_while_export_reads_the_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");

    let repo = Repo::init(&home.join("repos/drh/qa")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

    let mut snapshot = Meta::new(meta::mint_session_id(), "claude-code".into(), "/w".into());
    snapshot.layout = LayoutVersion::V0;
    meta::write(repo.root(), &snapshot).unwrap();

    let enveloped = transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"TRANSCRIPT-MARKER\"}}\n",
        "claude-code",
        &snapshot.session,
    );
    fs::write(repo.root().join(meta::LEGACY_LOG_FILE), &enveloped).unwrap();
    fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), &enveloped).unwrap();
    fs::write(repo.root().join(meta::LOG_FILE), USER_LOG).unwrap();
    fs::write(repo.root().join(meta::VIEW_FILE), USER_VIEW).unwrap();
    repo.add_all().unwrap();
    repo.commit("v0 session line carrying same-named user files")
        .unwrap();
    let branch = repo.current_branch().unwrap();

    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .env("AGIT_HOME", &home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };

    // ① Explicit `ref:path`: the verbatim blob in the tree, the same semantics as
    //    `git show <sha>:LOG`.
    for (path, expected) in [(meta::LOG_FILE, USER_LOG), (meta::VIEW_FILE, USER_VIEW)] {
        let stdout = run(&["show", &format!("drh/qa@{branch}:{path}")]);
        assert_eq!(
            stdout, expected,
            "`{path}` reads the file in the tree, not the transcript"
        );
    }

    // ② The logical read: what comes out is that line's transcript, and not one byte of the
    //    same-named user file. `--view-only` gets its own run — it reads `VIEW`, and `VIEW` and
    //    `LOG` each have a same-named user file.
    let target = format!("drh/qa@{branch}");
    for flag in [None, Some("--view-only")] {
        let mut args = vec!["export", target.as_str()];
        args.extend(flag);
        let stdout = run(&args);
        assert!(
            stdout.contains("TRANSCRIPT-MARKER"),
            "{args:?} must export the transcript: {stdout}"
        );
        assert!(
            !stdout.contains("not a transcript"),
            "{args:?} must not export the same-named user file: {stdout}"
        );
    }
}
