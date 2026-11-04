#![cfg(feature = "cli")]

//! `agit repair placeholders` against the real privacy worker this binary starts.

use agit::domain::privacy::{
    dictionary::{Origin, Record},
    keys,
    storage::Store,
};
use agit::domain::{meta, repo::Repo};
use std::process::Command;

#[path = "support/startup_cache.rs"]
mod startup_cache;

const HUB: &str = "http://127.0.0.1:1";

/// Files an older agit published with placeholders in place of a checksum and a font data URI
/// get those values back from the dictionary, while a GitHub token stays replaced, as the current
/// policy replaces it, now in the current placeholder spelling; the repair is one forward commit
/// that touches only files that held an older placeholder. A repair that wrote the dictionary's
/// values back without protecting them again would publish the token; one that left a file
/// holding only credentials in the older spelling would leave `agit push` pointing at it for good;
/// one that rewrote every file would touch a file that held no placeholder; one that printed a
/// value, or didn't ask for a look before pushing, would put back an unknown key form unseen; one
/// that read git's quoted spelling of a non-ASCII name would stop at that file; and one that wrote
/// a value with a quote into a JSON string as it stands would commit a broken file; one that
/// protected a JSON file as a transcript would leave a credential in its `id` as written; and one
/// that checked a JSON value only in its escaped spelling, or on its own without the rest of its
/// string, would write back a value the person blocked whose quote and backslash the escaping
/// changes.
#[test]
fn repair_restores_what_is_no_credential_and_keeps_credentials_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    for config in [
        ["user.name", "Synthetic User"],
        ["user.email", "synthetic@example.invalid"],
    ] {
        repo.git(&["config", config[0], config[1]]).unwrap();
    }
    repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    std::fs::write(
        repo.root().join(".gitattributes"),
        agit::domain::storage::attributes_text(None),
    )
    .unwrap();

    let directory = keys::pending_directory(&home.join("privacy"), HUB, None).unwrap();
    let mut dictionary = Store::open(&directory, None).unwrap();
    let digest = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    let font = format!(
        "data:font/ttf;base64,{}",
        "AAEAAAALAIAAAwAwT1MvMg8SBf".repeat(20)
    );
    let token = "ghp_Z9xY8wV7uT6sR5qP4oN3mL2kJ1iH0gF9eD8c";
    // Placeholders in the spelling an older agit wrote.
    let quoted = "printf \"hello\"\\n\nnext line";
    let config_token = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    let blocked = "vault \"repair-canary\" C:\\keys\\synthetic-blocked-value";
    // Blocked only together with the words before its placeholder in the file.
    let spanned = "ledger \"span-canary\" D:\\books\\synthetic-spanned-value";
    let span_rule = format!("see {spanned}");
    let originals = [
        digest,
        font.as_str(),
        token,
        quoted,
        config_token,
        blocked,
        spanned,
    ];
    let records = originals.map(|value| Record {
        token: format!(
            "{{{{AGIT_SECRET_V2:{}:{}}}}}",
            dictionary.dictionary_id,
            uuid::Uuid::new_v4()
        ),
        original: value.to_owned(),
        origins: [Origin::Heuristic].into(),
    });
    dictionary.append(&records, None).unwrap();
    let files = [
        (
            "SHA256SUMS.txt",
            format!("{}  build.tar.gz\n", records[0].token),
        ),
        (
            "artifacts/logo.svg",
            format!(
                "<svg><style>@font-face{{src:url({})}}</style></svg>\n",
                records[1].token
            ),
        ),
        (
            "deploy.sh",
            format!("export GITHUB_TOKEN={}\n", records[2].token),
        ),
        ("README.md", "nothing replaced here\n".to_owned()),
        (
            "settings.json",
            format!(
                "{{\"id\":\"{}\",\"timestamp\":\"2026-11-04\"}}\n",
                records[4].token
            ),
        ),
        (
            "notes.txt",
            format!(
                "{{\"note\":\"{}\",\"span\":\"see {}\"}}\n",
                records[5].token, records[6].token
            ),
        ),
        (
            "设置/package.json",
            format!(
                "{{\n  \"scripts\": {{\"hello\": \"{}\"}}\n}}\n",
                records[3].token
            ),
        ),
    ];
    for (path, content) in &files {
        let path = repo.root().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Published by an older agit"])
        .unwrap();
    let before = repo.git(&["rev-parse", "main"]).unwrap();

    let agit = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", HUB)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    };
    for (name, value) in [
        ("repair-canary", blocked),
        ("span-canary", span_rule.as_str()),
    ] {
        let mut input = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut input, value.as_bytes()).unwrap();
        std::io::Seek::rewind(&mut input).unwrap();
        let added = agit()
            .args(["secrets", "add", name, "--stdin"])
            .stdin(input)
            .output()
            .unwrap();
        assert!(added.status.success(), "{added:?}");
    }
    let repaired = agit()
        .args(["repair", "placeholders", "alice/app"])
        .output()
        .unwrap();
    assert!(repaired.status.success(), "{repaired:?}");
    let said = String::from_utf8_lossy(&repaired.stderr).into_owned()
        + &String::from_utf8_lossy(&repaired.stdout);
    assert!(
        said.contains("look through the files listed above"),
        "{said}"
    );
    assert!(!said.contains(digest) && !said.contains(token), "{said}");

    let show = |path: &str| repo.git(&["show", &format!("main:{path}")]).unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "main~1"]).unwrap(),
        before,
        "the repair is one commit on top"
    );
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s", "main"]).unwrap(),
        "agit: restore values an earlier agit replaced"
    );
    assert_eq!(
        repo.git(&["diff", "--name-only", "main~1", "main"])
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "SHA256SUMS.txt",
            "artifacts/logo.svg",
            "deploy.sh",
            "notes.txt",
            "settings.json",
            "\"\\350\\256\\276\\347\\275\\256/package.json\""
        ]
    );
    // A credential in a plain JSON file's own `id` stays replaced: transcript protocol fields keep
    // their values only in transcripts.
    let settings = show("settings.json");
    assert!(!settings.contains(config_token), "{settings}");
    assert!(settings.contains("‹secret:"), "{settings}");
    serde_json::from_str::<serde_json::Value>(&settings).unwrap();
    // A value the person blocked stays replaced in a JSON file of any name, in neither spelling.
    let notes = show("notes.txt");
    for value in [blocked, spanned] {
        let escaped = serde_json::to_string(value).unwrap();
        assert!(
            !notes.contains(value) && !notes.contains(&escaped[1..escaped.len() - 1]),
            "{value}: {notes}"
        );
    }
    assert!(notes.contains("‹secret:"), "{notes}");
    serde_json::from_str::<serde_json::Value>(&notes).unwrap();
    let package: serde_json::Value = serde_json::from_str(&show("设置/package.json")).unwrap();
    assert_eq!(package["scripts"]["hello"], quoted);
    assert!(show("设置/package.json").starts_with("{\n  \"scripts\""));
    assert_eq!(
        show("SHA256SUMS.txt").trim_end(),
        format!("{digest}  build.tar.gz")
    );
    assert!(show("artifacts/logo.svg").contains(&font));
    let deploy = show("deploy.sh");
    assert!(!deploy.contains(token));
    assert!(deploy.contains("‹secret:"), "{deploy}");
    assert!(!deploy.contains("{{AGIT_SECRET_V"), "{deploy}");
}
