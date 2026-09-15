//! A replacement CLI exposes which executable and skill bundle handle the continued command.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let root = PathBuf::from(env::var_os("AGIT_HOME").unwrap());
    let args: Vec<_> = env::args().skip(1).collect();
    let exe = env::current_exe().unwrap();
    if args == ["--fixture-daemon"] {
        fs::write(root.join("daemon-exe"), exe.to_str().unwrap()).unwrap();
        return;
    }
    let skill = PathBuf::from(env::var_os("HOME").unwrap()).join(".codex/skills/agit/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    if args.first().is_some_and(|arg| arg == "--no-tui") {
        fs::write(root.join("refreshed"), args.join("\n")).unwrap();
        fs::write(&skill, "new fixture skill\n").unwrap();
        if env::var_os("FIXTURE_REFRESH_FAILURE").is_some() {
            std::process::exit(4);
        }
        return;
    }
    fs::write(root.join("continued"), args.join("\n")).unwrap();
    fs::write(root.join("continued-exe"), exe.to_str().unwrap()).unwrap();
    match args.first().map(String::as_str) {
        Some("setup") if args.iter().any(|arg| arg == "--skill") => {
            fs::write(&skill, "new fixture skill\n").unwrap();
        }
        Some("setup") => {
            fs::write(root.join("hook-exe"), exe.to_str().unwrap()).unwrap();
        }
        Some("rc") => {
            assert!(Command::new(&exe).arg("--fixture-daemon").status().unwrap().success());
        }
        _ => panic!("unexpected continued command: {args:?}"),
    }
}
