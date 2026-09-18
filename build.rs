use sha2::{Digest, Sha256};
use std::{env, fs, io::Read, path::Path};

fn hash_file(hash: &mut Sha256, path: &Path) {
    hash.update(path.to_string_lossy().replace('\\', "/").as_bytes());
    hash.update([0]);
    let mut file = fs::File::open(path).expect("build identity input is unavailable");
    hash.update(file.metadata().unwrap().len().to_le_bytes());
    let mut buffer = [0; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .expect("cannot read build identity input");
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
}

fn hash_tree(hash: &mut Sha256, path: &Path) {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if entry.file_type().unwrap().is_dir() {
            hash_tree(hash, &entry.path());
        } else {
            hash_file(hash, &entry.path());
        }
    }
}

fn build_identity() {
    let mut hash = Sha256::new();
    for path in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "docs/cli-json-schema.json",
        "docs/cli-json-schema-v2.json",
    ] {
        println!("cargo:rerun-if-changed={path}");
        hash_file(&mut hash, Path::new(path));
    }
    for path in [
        "src",
        "crates/agit-controller/src",
        "crates/agit-peer/src",
        "crates/agit-tunnel/src",
    ] {
        println!("cargo:rerun-if-changed={path}");
        hash_tree(&mut hash, Path::new(path));
    }
    for path in [
        "crates/agit-controller/Cargo.toml",
        "crates/agit-peer/Cargo.toml",
        "crates/agit-tunnel/Cargo.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
        hash_file(&mut hash, Path::new(path));
    }
    let mut configuration: Vec<_> = env::vars()
        .filter(|(key, _)| key.starts_with("CARGO_FEATURE_"))
        .collect();
    for key in [
        "TARGET",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_ENCODED_RUSTFLAGS",
        "AGIT_BUILD_VERSION",
        "AGIT_RELEASE_CHANNEL",
        "AGIT_DEFAULT_HUB_URL",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
        configuration.push((key.into(), env::var(key).unwrap_or_default()));
    }
    configuration.sort();
    for (key, value) in configuration {
        hash.update(key.as_bytes());
        hash.update([0]);
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    let rustc = std::process::Command::new(env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .output()
        .unwrap();
    assert!(rustc.status.success(), "cannot identify the Rust compiler");
    hash.update(rustc.stdout);
    if env::var_os("CARGO_FEATURE_BUNDLED_GIT").is_some() {
        hash_file(
            &mut hash,
            Path::new(&env::var_os("AGIT_GIT_RUNTIME_ARCHIVE").unwrap()),
        );
    }
    println!("cargo:rustc-env=AGIT_BUILD_ID={:x}", hash.finalize());
}

fn main() {
    println!("cargo:rerun-if-env-changed=AGIT_GIT_RUNTIME_ARCHIVE");
    if std::env::var_os("CARGO_FEATURE_BUNDLED_GIT").is_some() {
        let archive = std::env::var_os("AGIT_GIT_RUNTIME_ARCHIVE").expect(
            "bundled-git requires AGIT_GIT_RUNTIME_ARCHIVE; run scripts/prepare-git-runtime.py",
        );
        let archive = std::fs::canonicalize(archive).expect("Git runtime archive is unavailable");
        let mut metadata = archive.as_os_str().to_owned();
        metadata.push(".target");
        println!(
            "cargo:rerun-if-changed={}",
            std::path::Path::new(&metadata).display()
        );
        let target =
            std::fs::read_to_string(metadata).expect("Git runtime target metadata is missing");
        assert_eq!(
            target.trim(),
            std::env::var("TARGET").unwrap(),
            "Git runtime target does not match the CLI target"
        );
        println!("cargo:rerun-if-changed={}", archive.display());
        println!(
            "cargo:rustc-env=AGIT_GIT_RUNTIME_ARCHIVE={}",
            archive.display()
        );
    }
    build_identity();
}
