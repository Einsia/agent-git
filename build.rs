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
}
