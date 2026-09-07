//! Measure the clean-store startup migration path as the number of Agent repos grows.
//!
//! Run the default matrix with:
//!
//! ```text
//! cargo bench --bench startup_migration
//! ```
//!
//! The first optional argument is a comma-separated repo-count matrix and the second is the
//! number of timing samples per point:
//!
//! ```text
//! cargo bench --bench startup_migration -- 0,1,4 3
//! ```

use agit::commands::migration::{Report, migrate_startup};
use agit::domain::meta::{self, LayoutVersion, Meta};
use agit::domain::repo::Repo;
use anyhow::{Context, Result};
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

const DEFAULT_COUNTS: &str = "0,1,4,12";
const DEFAULT_SAMPLES: usize = 5;

fn main() -> Result<()> {
    let (counts, samples) = arguments()?;
    let fixture = tempfile::tempdir().context("cannot create benchmark home")?;
    let home = fixture.path().join("agit-home");
    std::fs::create_dir_all(home.join("repos/bench"))?;

    // This benchmark sets process input before starting any concurrent work.
    unsafe { std::env::set_var("AGIT_HOME", &home) };

    println!("repos\tsamples\tmedian_ms\tmin_ms\tmax_ms");
    let mut present = 0usize;
    for count in counts {
        while present < count {
            create_clean_repo(&home, present)?;
            present += 1;
        }

        assert_eq!(migrate_startup()?, Report::default());
        let mut timings = Vec::with_capacity(samples);
        for _ in 0..samples {
            let started = Instant::now();
            let report = black_box(migrate_startup()?);
            timings.push(started.elapsed());
            assert_eq!(report, Report::default());
        }
        timings.sort_unstable();
        println!(
            "{count}\t{samples}\t{:.3}\t{:.3}\t{:.3}",
            millis(timings[timings.len() / 2]),
            millis(timings[0]),
            millis(*timings.last().expect("a sample exists")),
        );
    }
    Ok(())
}

fn arguments() -> Result<(Vec<usize>, usize)> {
    let mut args = std::env::args().skip(1).filter(|arg| arg != "--bench");
    let raw_counts = args.next().unwrap_or_else(|| DEFAULT_COUNTS.into());
    let samples = args
        .next()
        .map(|raw| raw.parse::<usize>())
        .transpose()
        .context("sample count must be a positive integer")?
        .unwrap_or(DEFAULT_SAMPLES);
    anyhow::ensure!(samples > 0, "sample count must be positive");
    anyhow::ensure!(args.next().is_none(), "too many benchmark arguments");

    let mut counts = raw_counts
        .split(',')
        .map(|raw| {
            raw.parse::<usize>()
                .with_context(|| format!("invalid repo count {raw:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(!counts.is_empty(), "repo-count matrix must not be empty");
    counts.sort_unstable();
    counts.dedup();
    Ok((counts, samples))
}

fn create_clean_repo(home: &Path, index: usize) -> Result<()> {
    let path = home.join("repos/bench").join(format!("repo-{index}"));
    let repo = Repo::init(&path)?;
    repo.git(&["config", "user.name", "Migration Benchmark"])?;
    repo.git(&["config", "user.email", "benchmark@example.invalid"])?;
    repo.git(&["config", "commit.gpgsign", "false"])?;

    let mut snapshot = Meta::new_file_line();
    snapshot.layout = LayoutVersion::V1;
    meta::write(repo.root(), &snapshot)?;
    repo.add_all()?;
    repo.commit("benchmark v1 file line")?;
    Ok(())
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
