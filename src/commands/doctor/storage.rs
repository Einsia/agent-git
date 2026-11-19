//! Metadata-only accounting for an explicitly selected Agit home.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use serde::Serialize;

#[cfg(unix)]
#[path = "storage/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "storage/windows.rs"]
mod platform;

const MAX_ENTRIES: u64 = 250_000;
const MAX_DEPTH: usize = 64;
const MAX_TIME: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Identity(u64, u64);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    Directory,
    Link,
    Other,
}

struct Metadata {
    identity: Identity,
    kind: Kind,
    links: u64,
    size: u64,
    allocated: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum Category {
    HomeMetadata,
    OriginalGitObjects,
    RepositoryMetadata,
    RepositoryCheckouts,
    SessionWorktrees,
    PublicationStores,
    RecoveredOriginals,
    PublicationReceipts,
    LfsCache,
    TemporaryPacks,
    Links,
    PrivacyState,
    AuthorityState,
    BundledGit,
    DaemonImages,
    Diagnostics,
    PendingArchives,
    RcState,
    Configuration,
    SharedHardlinks,
    Unclassified,
}

#[derive(Default, Serialize)]
struct Usage {
    files: u64,
    directories: u64,
    logical_file_bytes: u64,
    allocated_bytes: Option<u64>,
    allocation_unknown_entries: u64,
}

impl Usage {
    fn add(&mut self, metadata: &Metadata) -> bool {
        let mut overflow = false;
        if metadata.kind == Kind::File {
            self.files += 1;
            self.logical_file_bytes += metadata.size;
        } else {
            self.directories += 1;
        }
        if self.allocation_unknown_entries == 0 {
            self.allocated_bytes = metadata.allocated.and_then(|bytes| {
                let total = self.allocated_bytes.unwrap_or(0).checked_add(bytes);
                overflow = total.is_none();
                total
            });
            if self.allocated_bytes.is_none() {
                self.allocation_unknown_entries += 1;
            }
        } else if metadata.allocated.is_none() {
            self.allocation_unknown_entries += 1;
        }
        overflow
    }
}

#[derive(Default, Serialize)]
struct Gaps {
    io_errors: u64,
    skipped_links: u64,
    special_entries: u64,
    mount_boundaries: u64,
    changed_directories: u64,
    accounting_overflow: bool,
    limit_reached: bool,
}

impl Gaps {
    fn complete(&self) -> bool {
        self.io_errors == 0
            && self.skipped_links == 0
            && self.special_entries == 0
            && self.mount_boundaries == 0
            && self.changed_directories == 0
            && !self.accounting_overflow
            && !self.limit_reached
    }
}

#[derive(Serialize)]
struct Report {
    schema: &'static str,
    schema_version: u32,
    scope: String,
    complete: bool,
    observed: Usage,
    categories: BTreeMap<Category, Usage>,
    duplicate_file_entries: u64,
    gaps: Gaps,
    notes: [&'static str; 4],
}

struct Scan {
    report: Report,
    seen: HashSet<Identity>,
    entries: u64,
    maximum: u64,
    started: Instant,
}

impl Scan {
    fn new(root: &Path, maximum: u64) -> Self {
        Self {
            report: Report {
                schema: "storage-usage",
                schema_version: 1,
                scope: root.to_string_lossy().into_owned(),
                complete: false,
                observed: Usage::default(),
                categories: BTreeMap::new(),
                duplicate_file_entries: 0,
                gaps: Gaps::default(),
                notes: [
                    "Observed bytes are not a snapshot or an estimate of reclaimable space.",
                    "Logical bytes count unique regular files; allocated bytes also include directories. Null allocation means unavailable.",
                    "Hardlinks count once in shared_hardlinks. Shared filesystem extents may still overlap; these are not exclusive physical bytes.",
                    "Categories use the Agit home layout only. Symlink targets, other volumes, external LFS and native runtime stores are not inspected.",
                ],
            },
            seen: HashSet::new(),
            entries: 0,
            maximum,
            started: Instant::now(),
        }
    }

    fn exhausted(&mut self) -> bool {
        if self.entries >= self.maximum || self.started.elapsed() >= MAX_TIME {
            self.report.gaps.limit_reached = true;
        }
        self.report.gaps.limit_reached || self.report.gaps.accounting_overflow
    }

    fn record(&mut self, path: &[OsString], metadata: &Metadata) {
        if metadata.kind == Kind::File
            && self
                .report
                .observed
                .logical_file_bytes
                .checked_add(metadata.size)
                .is_none()
        {
            self.report.gaps.accounting_overflow = true;
            return;
        }
        let category = if metadata.kind == Kind::File && metadata.links > 1 {
            Category::SharedHardlinks
        } else {
            classify(path)
        };
        let total_overflow = self.report.observed.add(metadata);
        let category_overflow = self
            .report
            .categories
            .entry(category)
            .or_default()
            .add(metadata);
        self.report.gaps.accounting_overflow |= total_overflow || category_overflow;
    }

    fn walk(&mut self, directory: &mut platform::Directory, path: &mut Vec<OsString>, volume: u64) {
        while !self.exhausted() {
            let name = match directory.next_name() {
                Ok(Some(name)) => name,
                Ok(None) => return,
                Err(_) => {
                    self.report.gaps.io_errors += 1;
                    return;
                }
            };
            self.entries += 1;
            let metadata = match directory.metadata(&name) {
                Ok(metadata) => metadata,
                Err(_) => {
                    self.report.gaps.io_errors += 1;
                    continue;
                }
            };
            match metadata.kind {
                Kind::Link => {
                    self.report.gaps.skipped_links += 1;
                    continue;
                }
                Kind::Other => {
                    self.report.gaps.special_entries += 1;
                    continue;
                }
                _ => {}
            }
            if metadata.identity.0 != volume {
                self.report.gaps.mount_boundaries += 1;
                continue;
            }
            if !self.seen.insert(metadata.identity) {
                if metadata.kind == Kind::File {
                    self.report.duplicate_file_entries += 1;
                } else {
                    self.report.gaps.changed_directories += 1;
                }
                continue;
            }
            path.push(name);
            if metadata.kind == Kind::Directory {
                match directory.child(path.last().unwrap()) {
                    Ok(mut child) if child.identity() == metadata.identity => {
                        self.record(path, &metadata);
                        if path.len() >= MAX_DEPTH {
                            self.report.gaps.limit_reached = true;
                        } else {
                            self.walk(&mut child, path, volume);
                        }
                    }
                    Ok(_) => self.report.gaps.changed_directories += 1,
                    Err(_) => self.report.gaps.io_errors += 1,
                }
            } else {
                self.record(path, &metadata);
            }
            path.pop();
        }
    }
}

fn classify(path: &[OsString]) -> Category {
    use Category::*;
    let part = |index: usize| path.get(index).and_then(|value| value.to_str());
    match part(0) {
        None if path.is_empty() => HomeMetadata,
        Some("repos") => {
            if part(3) != Some(".git") {
                return RepositoryCheckouts;
            }
            let Some(tail) = path[4..]
                .iter()
                .map(|part| part.to_str())
                .collect::<Option<Vec<_>>>()
            else {
                return Unclassified;
            };
            match tail.as_slice() {
                ["agit", "privacy-publication", ..] => {
                    if tail.len() == 7
                        && tail[3..6] == [".git", "objects", "pack"]
                        && tail[6].starts_with("tmp_pack_")
                    {
                        TemporaryPacks
                    } else {
                        PublicationStores
                    }
                }
                ["agit", "privacy-recovery", ..] => RecoveredOriginals,
                ["agit", "privacy-accepted", ..] => PublicationReceipts,
                ["objects", "pack", name] if name.starts_with("tmp_pack_") => TemporaryPacks,
                ["objects", ..] => OriginalGitObjects,
                ["lfs", ..] => LfsCache,
                _ => RepositoryMetadata,
            }
        }
        Some("worktrees") => SessionWorktrees,
        Some("store") => Links,
        Some("privacy") => PrivacyState,
        Some("credentials" | "keystore" | "secret-filter") => AuthorityState,
        Some("git-runtime") => BundledGit,
        Some("rc" | "desktop-rc") => match part(1) {
            Some("images") => DaemonImages,
            Some("pending-archives") => PendingArchives,
            Some(name) if name.starts_with("diagnostics-") || name == "upgrade-watch.log" => {
                Diagnostics
            }
            _ => RcState,
        },
        Some(
            "projects"
            | "workspaces"
            | "folder-destinations"
            | "telemetry"
            | "config.json"
            | "privacy-policy-sources.json",
        ) => Configuration,
        _ => Unclassified,
    }
}

fn inspect(root: &Path, maximum: u64) -> crate::Result<Report> {
    anyhow::ensure!(
        !root.components().any(|c| c == Component::ParentDir),
        "select an Agit home directory without parent components"
    );
    let root = std::path::absolute(root)?;
    anyhow::ensure!(
        root.file_name().is_some() && !root.components().any(|c| c == Component::ParentDir),
        "select an Agit home directory without parent components, not a filesystem root"
    );
    let mut scan = Scan::new(&root, maximum);
    match platform::Directory::root(&root) {
        Ok((mut directory, metadata)) => {
            scan.seen.insert(metadata.identity);
            scan.record(&[], &metadata);
            scan.walk(&mut directory, &mut Vec::new(), metadata.identity.0);
        }
        Err(_) => scan.report.gaps.io_errors += 1,
    }
    scan.report.complete = scan.report.gaps.complete();
    Ok(scan.report)
}

pub(super) fn run(root: &Path) -> super::CmdResult {
    let report = inspect(root, MAX_ENTRIES)?;
    let code = if report.complete {
        crate::ExitCode::Ok
    } else {
        crate::ExitCode::Precondition
    };
    if crate::commands::json::requested() {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("Storage under {:?}", report.scope);
        println!("Category  Files  Directories  Logical file bytes  Allocated bytes");
        for (category, usage) in &report.categories {
            println!(
                "{}  {}  {}  {}  {}",
                serde_json::to_value(category)?.as_str().unwrap(),
                usage.files,
                usage.directories,
                usage.logical_file_bytes,
                usage
                    .allocated_bytes
                    .map_or_else(|| "unknown".into(), |n| n.to_string()),
            );
        }
        println!(
            "Observed: {} logical file bytes, {} allocated bytes; {} duplicate file entries counted once.",
            report.observed.logical_file_bytes,
            report
                .observed
                .allocated_bytes
                .map_or_else(|| "unknown".into(), |n| n.to_string()),
            report.duplicate_file_entries,
        );
        println!(
            "Traversal {}.",
            if report.complete {
                "complete"
            } else {
                "incomplete"
            }
        );
        println!("Gaps: {}", serde_json::to_string(&report.gaps)?);
        for note in report.notes {
            println!("{note}");
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bounded walk reports partial observation instead of a complete empty installation.
    #[test]
    fn a_budget_cutoff_is_incomplete() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("unclassified"), b"synthetic").unwrap();
        let report = inspect(&root.path().canonicalize().unwrap(), 0).unwrap();
        assert!(!report.complete);
        assert!(report.gaps.limit_reached);
        assert_eq!(report.observed.files, 0);
    }

    /// Summing individually valid sizes must not produce a complete report with wrapped bytes.
    #[test]
    fn aggregate_byte_overflow_is_incomplete() {
        for (size, allocated) in [(u64::MAX, Some(0)), (0, Some(u64::MAX))] {
            let mut scan = Scan::new(Path::new("synthetic"), MAX_ENTRIES);
            let metadata = Metadata {
                identity: Identity(1, 1),
                kind: Kind::File,
                links: 1,
                size,
                allocated,
            };
            scan.record(&[], &metadata);
            scan.record(
                &[],
                &Metadata {
                    size: 1,
                    allocated: Some(1),
                    ..metadata
                },
            );
            assert!(scan.report.gaps.accounting_overflow);
            assert!(!scan.report.gaps.complete());
            assert!(scan.exhausted());
        }
    }
}
