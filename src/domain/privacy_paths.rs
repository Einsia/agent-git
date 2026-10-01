//! Stable, device-local path normalization and publication aliases.
//!
//! Public projections use logical roots and private placeholders. The reverse mapping stays in
//! this repository's local state so a tracked tree cannot accidentally disclose source paths.

use crate::domain::privacy::{CandidateAction, PrivacyPolicy};
use crate::domain::repo::Repo;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;

const ALIAS_VERSION: u32 = 1;
const ALIAS_DIRECTORY: &str = "agit";
const ALIAS_FILE: &str = "privacy-path-aliases.json";

/// A root that may retain a logical relative path in a public projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRoot {
    pub label: String,
    pub path: String,
}

impl AliasRoot {
    /// Construct a root from any supported POSIX, Windows, UNC, or `file://` spelling.
    pub fn new(label: impl Into<String>, path: &str) -> Result<Self> {
        let label = label.into();
        ensure_valid_label(&label)?;
        let path = normalize_absolute(path)?;
        Ok(Self { label, path })
    }
}

/// The normalized path representation used as the local mapping key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedPath(String);

impl NormalizedPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A stable mapping from normalized source paths to public logical aliases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathAliasStore {
    version: u32,
    entries: BTreeMap<String, String>,
    #[serde(default)]
    private_entries: BTreeMap<String, String>,
    next_private: u64,
}

/// A path projection combines the policy decision with the stable public spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathProjection {
    pub action: CandidateAction,
    pub logical_path: Option<String>,
    pub reason: Option<String>,
}

impl Default for PathAliasStore {
    fn default() -> Self {
        Self {
            version: ALIAS_VERSION,
            entries: BTreeMap::new(),
            private_entries: BTreeMap::new(),
            next_private: 1,
        }
    }
}

impl PathAliasStore {
    /// Persist a complete alias allocation before returning its projection. Holding the lock
    /// across read, allocation, and replacement prevents concurrent previews from reusing an ID.
    pub fn transact<T>(repo: &Repo, operation: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let path = alias_path(repo)?;
        let parent = path.parent().context("path alias store has no parent")?;
        crate::infra::config::create_state_dir(parent)?;
        let lock = crate::infra::config::state_file_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join("privacy-path-aliases.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("cannot lock privacy path aliases")?;
        let mut aliases = Self::load(repo)?;
        let result = operation(&mut aliases)?;
        aliases.save(repo)?;
        Ok(result)
    }

    /// Load local aliases or create an empty mapping when this repository has none yet.
    fn load(repo: &Repo) -> Result<Self> {
        let path = alias_path(repo)?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let store: Self = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a path alias store", path.display()))?;
        store.validate()?;
        Ok(store)
    }

    /// Save aliases atomically below the Git common directory, outside tracked content.
    fn save(&self, repo: &Repo) -> Result<()> {
        self.validate()?;
        let path = alias_path(repo)?;
        let parent = path.parent().context("path alias store has no parent")?;
        crate::infra::config::create_state_dir(parent)?;
        let bytes = format!("{}\n", serde_json::to_string_pretty(self)?);
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("cannot install {}", path.display()))?;
        Ok(())
    }

    /// The local path used by this mapping.
    pub fn path(repo: &Repo) -> Result<std::path::PathBuf> {
        alias_path(repo)
    }

    /// Return the stable public alias for a source path and persist it when the caller saves.
    ///
    /// Root matches use the longest directory-boundary prefix. A path first seen outside an
    /// authorized root receives a private placeholder that remains stable if policy changes.
    pub fn alias_for(&mut self, path: &str, roots: &[AliasRoot]) -> Result<String> {
        let normalized = normalize_absolute(path)?;
        if let Some(alias) = self.entries.get(&normalized) {
            return Ok(alias.clone());
        }
        if let Some(alias) = self.private_entries.get(&normalized) {
            return Ok(alias.clone());
        }
        let best = roots
            .iter()
            .filter_map(|root| {
                relative_to(&root.path, &normalized).map(|relative| (root, relative))
            })
            .max_by_key(|(root, _)| component_count(&root.path));
        let alias = if let Some((root, relative)) = best {
            if relative.is_empty() {
                format!("<{}>", root.label)
            } else {
                format!("<{}>/{relative}", root.label)
            }
        } else {
            return self.private_alias_for(&normalized);
        };
        self.entries.insert(normalized, alias.clone());
        Ok(alias)
    }

    fn private_alias_for(&mut self, path: &str) -> Result<String> {
        let normalized = normalize_absolute(path)?;
        self.allocate_private(&normalized)
    }

    fn allocate_private(&mut self, source: &str) -> Result<String> {
        if let Some(alias) = self.private_entries.get(source) {
            return Ok(alias.clone());
        }
        let alias = format!("<private-file-{}>", self.next_private);
        self.next_private = self
            .next_private
            .checked_add(1)
            .context("private path alias counter exhausted")?;
        self.private_entries.insert(source.into(), alias.clone());
        Ok(alias)
    }

    /// Reserve imported placeholders before allocating aliases for any new source in a batch.
    /// Reservation does not associate an imported placeholder with a local file.
    pub fn reserve_aliases(&mut self, text: &str) -> Result<()> {
        for (start, _) in text.match_indices("<private-file-") {
            if let Some(end) = text[start..].find('>')
                && let Some(id) = private_id(&text[start..=start + end])
            {
                self.next_private = self.next_private.max(
                    id.checked_add(1)
                        .context("private path alias counter exhausted")?,
                );
            }
        }
        Ok(())
    }

    /// Apply the repository policy and produce the path that a public projection may carry.
    ///
    /// Excluded paths receive a private placeholder for reports that retain structure. The
    /// caller must still omit their content; an alias never grants publication permission.
    pub fn project_file(
        &mut self,
        policy: &PrivacyPolicy,
        path: &std::path::Path,
        branch: Option<&str>,
    ) -> Result<PathProjection> {
        self.project_file_with_decision(policy, path, branch)
            .map(|(projection, _)| projection)
    }

    pub(crate) fn project_file_with_decision(
        &mut self,
        policy: &PrivacyPolicy,
        path: &std::path::Path,
        branch: Option<&str>,
    ) -> Result<(PathProjection, crate::domain::privacy::CandidateDecision)> {
        let decision = policy.evaluate_file(path, branch);
        let source = absolute_text(path)?;
        let alias = if let Some(logical) = decision.logical_path.clone()
            && decision.action == CandidateAction::Allowed
        {
            let normalized = normalize_absolute(&source)?;
            if let Some(existing) = self.entries.get(&normalized) {
                existing.clone()
            } else if let Some(existing) = self.private_entries.get(&normalized) {
                existing.clone()
            } else {
                self.entries.insert(normalized, logical.clone());
                logical
            }
        } else {
            self.private_alias_for(&source)?
        };
        Ok((
            PathProjection {
                action: decision.action,
                logical_path: Some(alias),
                reason: decision.reason.clone(),
            },
            decision,
        ))
    }

    /// Build a local preview from one policy evaluation per candidate. Excluded memory names
    /// receive placeholders too, because an allowlist rejection also hides the filename.
    pub fn preview<'a, I>(
        &mut self,
        policy: &PrivacyPolicy,
        branch: Option<&str>,
        files: I,
        memory: &[&'a str],
    ) -> Result<crate::domain::privacy::PreviewReport>
    where
        I: IntoIterator<Item = &'a std::path::Path>,
    {
        let files = files
            .into_iter()
            .map(|path| absolute_text(path).map(std::path::PathBuf::from))
            .collect::<Result<Vec<_>>>()?;
        let mut aliases = HashMap::new();
        for path in &files {
            let projection = self.project_file(policy, path, branch)?;
            aliases.insert(
                policy.evaluate_file(path, branch).input,
                projection.logical_path,
            );
        }
        let mut report = policy.preview(
            branch,
            files.iter().map(std::path::PathBuf::as_path),
            memory,
        );
        for candidate in &report.candidates {
            self.reserve_aliases(&candidate.input)?;
        }
        for candidate in &mut report.candidates {
            if let Some(alias) = aliases.get(&candidate.input) {
                candidate.logical_path = alias.clone();
            } else if candidate.action != CandidateAction::Allowed {
                let source = if std::path::Path::new(&candidate.input).is_absolute() {
                    normalize_absolute(&candidate.input).unwrap_or_else(|_| candidate.input.clone())
                } else {
                    // Relative memory names occupy their own namespace and never resolve on disk.
                    format!("memory:{}", candidate.input)
                };
                candidate.logical_path = Some(self.allocate_private(&source)?);
            }
        }
        Ok(report)
    }

    /// Expose the number of persisted mappings for diagnostics and tests without exposing keys.
    pub fn len(&self) -> usize {
        self.entries.len() + self.private_entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.private_entries.is_empty()
    }

    #[cfg(feature = "secret-vault")]
    pub(crate) fn reservation_boundary(&self) -> u64 {
        self.next_private
    }

    /// Return the reverse mapping for the encrypted publication layer.
    ///
    /// Public projections carry aliases only. Callers must keep this mapping inside an
    /// authenticated private payload; writing it to a tracked tree would disclose source paths.
    pub fn private_mappings(&self) -> BTreeMap<String, String> {
        self.entries
            .iter()
            .chain(self.private_entries.iter())
            .map(|(source, alias)| (alias.clone(), source.clone()))
            .collect()
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == ALIAS_VERSION,
            "unsupported path alias version {}",
            self.version
        );
        ensure!(self.next_private > 0, "path alias counter must be positive");
        let mut aliases = std::collections::BTreeSet::new();
        for source in self.entries.keys() {
            ensure!(
                normalize_absolute(source)? == *source,
                "path alias source is not normalized"
            );
        }
        for alias in self.entries.values().chain(self.private_entries.values()) {
            ensure!(aliases.insert(alias), "duplicate path alias `{alias}`");
            if let Some(id) = private_id(alias) {
                ensure!(
                    id < self.next_private,
                    "private path alias counter overlaps a stored ID"
                );
            }
        }
        Ok(())
    }
}

fn absolute_text(path: &std::path::Path) -> Result<String> {
    if path.is_absolute() {
        return Ok(path.to_string_lossy().into_owned());
    }
    Ok(std::env::current_dir()?
        .join(path)
        .to_string_lossy()
        .into_owned())
}

fn alias_path(repo: &Repo) -> Result<std::path::PathBuf> {
    Ok(repo.common_dir()?.join(ALIAS_DIRECTORY).join(ALIAS_FILE))
}

fn private_id(alias: &str) -> Option<u64> {
    let digits = alias.strip_prefix("<private-file-")?.strip_suffix('>')?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn ensure_valid_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty() && label.len() <= 64,
        "path alias labels must be non-empty and bounded"
    );
    ensure!(
        label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "path alias labels may contain only letters, digits, `-` and `_`"
    );
    Ok(())
}

/// Normalize path syntax without consulting the host filesystem.
pub fn normalize_path(input: &str) -> Result<NormalizedPath> {
    let text = decode_file_uri(input)?;
    ensure!(!text.is_empty(), "path must not be empty");
    ensure!(
        !text.contains(['\0', '\n', '\r']),
        "path contains a control character"
    );
    let text = text.replace('\\', "/");
    let (prefix, rest) = split_prefix(&text)?;
    let mut parts = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("path escapes its root through `..`");
                }
            }
            value => parts.push(value.to_owned()),
        }
    }
    let relative = parts.join("/");
    let normalized = if prefix.is_empty() {
        relative
    } else if prefix == "/" {
        format!("/{relative}")
    } else if relative.is_empty() && prefix.ends_with(':') {
        format!("{prefix}/")
    } else if relative.is_empty() {
        prefix
    } else {
        format!("{prefix}/{relative}")
    };
    ensure!(
        !normalized.is_empty(),
        "path must contain a name or an absolute root"
    );
    Ok(NormalizedPath(normalized))
}

fn normalize_absolute(input: &str) -> Result<String> {
    let normalized = normalize_path(input)?;
    ensure!(
        is_absolute(normalized.as_str()),
        "path roots and aliases require an absolute path"
    );
    Ok(normalized.0)
}

pub(crate) fn decode_file_uri(input: &str) -> Result<String> {
    if !input
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
    {
        return Ok(input.to_owned());
    }
    let rest = &input[7..];
    ensure!(
        !rest.contains(['?', '#']),
        "file URL query and fragment are not path components"
    );
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    ensure!(
        !authority.contains(['@', ':', '%', '\\']),
        "file URL authority is invalid"
    );
    let decoded_path = percent_decode(path)?;
    if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
        if decoded_path.len() >= 3
            && decoded_path.as_bytes()[1] == b':'
            && decoded_path.as_bytes()[2] == b'/'
        {
            Ok(decoded_path)
        } else {
            Ok(format!("/{decoded_path}"))
        }
    } else {
        Ok(format!("//{authority}/{decoded_path}"))
    }
}

fn percent_decode(input: &str) -> Result<String> {
    let mut output = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        ensure!(index + 2 < bytes.len(), "file URL has an incomplete escape");
        let high = hex_digit(bytes[index + 1]).context("file URL has an invalid escape")?;
        let low = hex_digit(bytes[index + 2]).context("file URL has an invalid escape")?;
        output.push(high * 16 + low);
        index += 3;
    }
    String::from_utf8(output).context("file URL escape is not valid UTF-8")
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn split_prefix(input: &str) -> Result<(String, &str)> {
    if let Some(stripped) = input.strip_prefix('/') {
        if let Some(stripped) = input.strip_prefix("//") {
            let mut pieces = stripped.splitn(3, '/');
            let server = pieces.next().unwrap_or_default();
            let share = pieces.next().unwrap_or_default();
            ensure!(
                !server.is_empty()
                    && !share.is_empty()
                    && !matches!(server, "." | ".." | "?")
                    && !matches!(share, "." | "..")
                    && !server.contains(':')
                    && !share.contains(':'),
                "UNC path needs a server and share"
            );
            let rest = pieces.next().unwrap_or_default();
            return Ok((format!("//{server}/{share}"), rest));
        }
        return Ok(("/".into(), stripped));
    }
    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        ensure!(
            bytes.get(2) == Some(&b'/'),
            "drive-relative paths require a working directory"
        );
        let drive = (bytes[0] as char).to_ascii_uppercase();
        let rest = input[2..].trim_start_matches('/');
        return Ok((format!("{drive}:"), rest));
    }
    Ok((String::new(), input))
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
        || (path.len() >= 3 && path.as_bytes()[1] == b':' && path.as_bytes()[2] == b'/')
}

fn component_count(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

fn relative_to(root: &str, path: &str) -> Option<String> {
    let windowsish = root.starts_with("//") || root.as_bytes().get(1) == Some(&b':');
    let equal = if windowsish {
        root.eq_ignore_ascii_case(path)
    } else {
        root == path
    };
    if equal {
        return Some(String::new());
    }
    let prefix = format!("{}/", root.trim_end_matches('/'));
    let matched = if windowsish {
        path.get(prefix.len()..)
            .filter(|_| path.len() > prefix.len())
            .filter(|_| path[..prefix.len()].eq_ignore_ascii_case(&prefix))
    } else {
        path.strip_prefix(&prefix)
    }?;
    Some(matched.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repo::Repo;

    #[test]
    fn normalizes_supported_path_spellings_and_rejects_escape() {
        assert_eq!(
            normalize_path("/work/./app/../src/main.rs")
                .unwrap()
                .as_str(),
            "/work/src/main.rs"
        );
        assert_eq!(
            normalize_path(r"C:\Users\alice\Code\app\src\main.rs")
                .unwrap()
                .as_str(),
            "C:/Users/alice/Code/app/src/main.rs"
        );
        assert_eq!(normalize_path("C:/").unwrap().as_str(), "C:/");
        assert_eq!(
            normalize_path("file:///C:/Users/alice/Code/app/main.rs")
                .unwrap()
                .as_str(),
            "C:/Users/alice/Code/app/main.rs"
        );
        assert_eq!(
            normalize_path(r"\\server\share\project\file.txt")
                .unwrap()
                .as_str(),
            "//server/share/project/file.txt"
        );
        assert_eq!(
            normalize_path("file:///Users/alice/Code/app/a%20b.txt")
                .unwrap()
                .as_str(),
            "/Users/alice/Code/app/a b.txt"
        );
        assert!(normalize_path("/work/../../secret").is_err());
    }

    #[test]
    fn aliases_use_longest_boundary_root_and_stable_private_numbers() {
        let mut store = PathAliasStore::default();
        let roots = vec![
            AliasRoot::new("workspace", "/work").unwrap(),
            AliasRoot::new("nested", "/work/app").unwrap(),
        ];
        assert_eq!(
            store.alias_for("/work/app/src/main.rs", &roots).unwrap(),
            "<nested>/src/main.rs"
        );
        assert_eq!(
            store.alias_for("/work/app2/src/main.rs", &roots).unwrap(),
            "<workspace>/app2/src/main.rs"
        );
        assert_eq!(
            store.alias_for("/Users/alice/private.txt", &roots).unwrap(),
            "<private-file-1>"
        );
        assert_eq!(
            store.alias_for("/Users/alice/private.txt", &roots).unwrap(),
            "<private-file-1>"
        );
        assert_eq!(
            store.alias_for("/work/app/src/other.rs", &roots).unwrap(),
            "<nested>/src/other.rs"
        );
        assert_eq!(store.len(), 4);
    }

    #[test]
    fn aliases_round_trip_outside_the_tracked_tree() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&temp.path().join("agent")).unwrap();
        let mut store = PathAliasStore::default();
        let roots = vec![AliasRoot::new("workspace", temp.path().to_str().unwrap()).unwrap()];
        store
            .alias_for(&format!("{}/src/main.rs", temp.path().display()), &roots)
            .unwrap();
        store.save(&repo).unwrap();
        let loaded = PathAliasStore::load(&repo).unwrap();
        assert_eq!(loaded, store);
        assert!(
            PathAliasStore::path(&repo)
                .unwrap()
                .starts_with(repo.common_dir().unwrap())
        );
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn successful_allocations_preserve_imported_numbers_under_contention() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&temp.path().join("agent")).unwrap();
        PathAliasStore::transact(&repo, |store| {
            store.reserve_aliases("Imported <private-file-7>")
        })
        .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = ["/private/first.txt", "/private/second.txt"]
            .into_iter()
            .map(|path| {
                let repo = repo.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    PathAliasStore::transact(&repo, |store| store.private_alias_for(path))
                        .map(|alias| (path, alias))
                })
            })
            .collect();
        let assigned: Vec<_> = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().ok())
            .collect();
        assert!(!assigned.is_empty());
        let distinct: std::collections::BTreeSet<_> =
            assigned.iter().map(|(_, alias)| alias).collect();
        assert_eq!(distinct.len(), assigned.len());
        PathAliasStore::transact(&repo, |store| {
            for (path, alias) in &assigned {
                assert_eq!(&store.private_alias_for(path)?, alias);
                assert!(private_id(alias).unwrap() > 7);
            }
            Ok(())
        })
        .unwrap();
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn policy_projection_keeps_structure_but_marks_excluded_content() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        let allowed = workspace.join("src/main.rs");
        let excluded = workspace.join("notes.txt");
        fs::write(&allowed, "fn main() {}\n").unwrap();
        fs::write(&excluded, "private\n").unwrap();
        let mut policy = PrivacyPolicy {
            workspace: Some(workspace),
            ..PrivacyPolicy::default()
        };
        let mut aliases = PathAliasStore::default();
        let public = aliases.project_file(&policy, &allowed, None).unwrap();
        assert_eq!(public.action, CandidateAction::Allowed);
        assert_eq!(
            public.logical_path.as_deref(),
            Some("<workspace>/src/main.rs")
        );
        let hidden = aliases.project_file(&policy, &excluded, None).unwrap();
        assert_eq!(hidden.action, CandidateAction::Excluded);
        assert_eq!(hidden.logical_path.as_deref(), Some("<private-file-1>"));
        policy.exclude.push("src/**".into());
        let narrowed = aliases.project_file(&policy, &allowed, None).unwrap();
        assert_eq!(narrowed.action, CandidateAction::Excluded);
        assert_eq!(narrowed.logical_path.as_deref(), Some("<private-file-2>"));
        assert_eq!(
            aliases.project_file(&policy, &allowed, None).unwrap(),
            narrowed
        );
        policy.exclude.clear();
        let reopened = aliases.project_file(&policy, &allowed, None).unwrap();
        assert_eq!(
            reopened.logical_path.as_deref(),
            Some("<workspace>/src/main.rs")
        );
    }
}
