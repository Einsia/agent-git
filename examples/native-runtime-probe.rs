//! Bridge for the isolated native runtime smoke script.
use std::path::Path;
fn main() -> agit::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 5,
        "expected source-runtime source-file target-runtime cwd, or --snapshot runtime session-id output-file"
    );
    if args[1] == "--snapshot" {
        let adapter = agit::adapter::get(&args[2])?;
        let source = adapter.lookup_native_readonly(&args[3], Default::default())?;
        let snapshot = adapter.snapshot_native_readonly(&source, Default::default())?;
        std::fs::write(&args[4], snapshot.bytes)?;
        return Ok(());
    }
    let raw = std::fs::read_to_string(&args[2])?;
    let (installed, lossy) =
        agit::domain::install::install(&raw, &args[1], &args[3], Path::new(&args[4]))?;
    let adapter = agit::adapter::get(&args[3])?;
    let session = adapter.parse_at(&installed.path)?;
    println!("{}\n{}\n{}", installed.path.display(), session.id, lossy);
    Ok(())
}
