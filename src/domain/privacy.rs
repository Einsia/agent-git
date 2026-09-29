//! Device-local repository privacy policy and path candidate evaluation.
//!
//! The policy is a fail-closed allowlist. It decides which local paths may enter a publication
//! candidate set; secret scanning and content rewriting remain mandatory
//! later stages. The policy file lives below the repository's Git common directory and is never a
//! tracked file.

use crate::domain::repo::Repo;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub mod mandatory;

/// Current on-disk policy schema.
pub const POLICY_VERSION: u32 = 1;
/// Conservative file classes that are useful for review while excluding local state by default.
/// Users can add precise patterns through the policy file or `agit privacy policy`.
pub const DEFAULT_INCLUDE_PATTERNS: &[&str] = &[
    "src/**",
    "tests/**",
    "docs/**",
    "examples/**",
    "benches/**",
    "README.md",
    "LICENSE",
    "CHANGELOG.md",
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "rust-toolchain.toml",
    "build.rs",
    "setup.sh",
];
const POLICY_DIRECTORY: &str = "agit";
const POLICY_FILE: &str = "privacy-policy.json";

/// A device-local publication policy. Absolute paths and replacement values are intentionally
/// kept out of tracked repository content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyPolicy {
    /// Schema version used for safe migrations.
    pub version: u32,
    /// Authorized workspace root. `None` means no file root is authorized.
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    /// Explicitly authorized roots outside the workspace.
    #[serde(default)]
    pub external_roots: Vec<ExternalRoot>,
    /// Repository-level include patterns. An empty list means no path matches.
    #[serde(default)]
    pub include: Vec<String>,
    /// Repository-level exclude patterns.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Memory paths that may be collected. An empty list keeps memory private.
    #[serde(default)]
    pub memory_allow: Vec<String>,
    /// Literal or regular-expression rewrites applied by the later projection layer.
    #[serde(default)]
    pub replacements: Vec<ReplacementRule>,
    /// Metadata publication mode.
    #[serde(default)]
    pub metadata: MetadataMode,
    /// Branch restrictions can only narrow the repository policy.
    #[serde(default)]
    pub branches: BTreeMap<String, BranchRestriction>,
    /// Effective restrictions come from device policy sources, never repository JSON.
    #[serde(default, skip_deserializing, skip_serializing_if = "Vec::is_empty")]
    pub mandatory: Vec<mandatory::MandatoryPolicy>,
}

/// An explicitly authorized external directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRoot {
    /// Stable logical label used by the path projection stage.
    pub label: String,
    /// Device-local absolute directory.
    pub path: PathBuf,
    /// Optional patterns relative to this root. Empty means no match.
    #[serde(default)]
    pub include: Vec<String>,
    /// Patterns rejected below this root.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// A configured content rewrite. The projection stage owns its execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementRule {
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub regex: bool,
}

/// Metadata fields exposed by a public projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MetadataMode {
    /// Logical workspace and format/runtime fields only.
    #[default]
    Minimal,
    /// Also expose explicitly selected project coordinates.
    Project,
}

/// Branch-specific restrictions. There is no branch-level allowlist expansion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BranchRestriction {
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub memory_exclude: Vec<String>,
}

impl Default for PrivacyPolicy {
    fn default() -> Self {
        Self {
            version: POLICY_VERSION,
            workspace: None,
            external_roots: Vec::new(),
            include: DEFAULT_INCLUDE_PATTERNS
                .iter()
                .map(|pattern| (*pattern).into())
                .collect(),
            exclude: Vec::new(),
            memory_allow: Vec::new(),
            replacements: Vec::new(),
            metadata: MetadataMode::Minimal,
            branches: BTreeMap::new(),
            mandatory: Vec::new(),
        }
    }
}

impl PrivacyPolicy {
    /// Load repository authorization with mandatory sources applied before its allowlists.
    pub fn load(repo: &Repo) -> Result<Self> {
        let mut policy = Self::load_local(repo)?;
        policy.mandatory = mandatory::load()?;
        policy.validate()?;
        Ok(policy)
    }

    /// Unbound sessions retain mandatory restrictions without acquiring a workspace allowlist.
    pub fn load_default() -> Result<Self> {
        let policy = Self {
            mandatory: mandatory::load()?,
            ..Self::default()
        };
        policy.validate()?;
        Ok(policy)
    }

    /// Editing repository authorization cannot absorb or replace mandatory source rules.
    pub(crate) fn load_local(repo: &Repo) -> Result<Self> {
        let path = policy_path(repo)?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let policy: Self = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a privacy policy", path.display()))?;
        policy.validate()?;
        Ok(policy)
    }

    /// Save a policy atomically below the Git common directory, outside the tracked tree.
    pub fn save(&self, repo: &Repo) -> Result<()> {
        self.validate()?;
        let path = policy_path(repo)?;
        let parent = path.parent().context("privacy policy has no parent")?;
        crate::infra::config::create_state_dir(parent)?;
        let temporary = parent.join(format!(".{POLICY_FILE}.tmp-{}", std::process::id()));
        let local = Self {
            mandatory: Vec::new(),
            ..self.clone()
        };
        let bytes = format!("{}\n", serde_json::to_string_pretty(&local)?);
        let mut file = crate::infra::config::state_file_options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        file.write_all(bytes.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)
            .with_context(|| format!("cannot install {}", path.display()))?;
        Ok(())
    }

    /// Return the policy path so the CLI can tell users where a file edit is applied.
    pub fn path(repo: &Repo) -> Result<PathBuf> {
        policy_path(repo)
    }

    /// Produce a stable, non-reversible strategy summary for a publication record.
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(serde_json::to_vec(self)?))
        ))
    }

    /// Validate the policy before it can influence candidate selection.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == POLICY_VERSION,
            "unsupported privacy policy version {}",
            self.version
        );
        for source in &self.mandatory {
            source.validate()?;
        }
        if let Some(workspace) = &self.workspace {
            ensure!(
                workspace.is_absolute(),
                "privacy workspace must be an absolute path"
            );
            ensure_no_control_path(workspace, "privacy workspace")?;
        }
        let mut labels = BTreeSet::new();
        for root in &self.external_roots {
            ensure_valid_label(&root.label)?;
            ensure!(
                labels.insert(root.label.clone()),
                "duplicate external root label `{}`",
                root.label
            );
            ensure!(
                root.path.is_absolute(),
                "external root `{}` must be absolute",
                root.label
            );
            ensure_no_control_path(&root.path, "external root")?;
            validate_patterns(&root.include)?;
            validate_patterns(&root.exclude)?;
        }
        validate_patterns(&self.include)?;
        validate_patterns(&self.exclude)?;
        validate_patterns(&self.memory_allow)?;
        for rule in &self.replacements {
            ensure!(
                !rule.pattern.is_empty(),
                "privacy replacement patterns must not be empty"
            );
            ensure!(
                !rule.pattern.contains(['\0', '\n', '\r']),
                "privacy replacement pattern contains a control character"
            );
            ensure!(
                !rule.replacement.contains(['\0', '\n', '\r']),
                "privacy replacement contains a control character"
            );
        }
        for (branch, restriction) in &self.branches {
            ensure!(
                !branch.is_empty() && !branch.contains(['\0', '\n', '\r']),
                "privacy branch name is invalid"
            );
            validate_patterns(&restriction.exclude)?;
            validate_patterns(&restriction.memory_exclude)?;
        }
        Ok(())
    }

    /// Evaluate a file path against roots, default exclusions, and branch restrictions.
    pub fn evaluate_file(&self, path: &Path, branch: Option<&str>) -> CandidateDecision {
        if let Err(reason) = reject_symlink_path(path) {
            return CandidateDecision::excluded(path, reason, "path.integrity");
        }
        let Some(path) = normalize_candidate_path(path) else {
            return CandidateDecision::excluded(
                path,
                "candidate path cannot be normalized",
                "path.normalization",
            );
        };
        let Some(root) = self.match_root(&path) else {
            return CandidateDecision::excluded(
                &path,
                "path is outside the authorized workspace and external roots",
                "path.authorized_roots",
            );
        };
        if is_forced_exclude(&root.relative) {
            return CandidateDecision::excluded(
                &path,
                "path matches the default sensitive-file exclusion",
                "default.sensitive_file",
            );
        }
        if let Some((source_index, source, rule_index)) =
            self.mandatory.iter().enumerate().find_map(|(i, source)| {
                matching_rule(&source.exclude, &root.relative).map(|rule| (i, source, rule))
            })
        {
            return CandidateDecision::excluded(
                &path,
                &format!("path is excluded by mandatory policy {}", source.id),
                format!("mandatory[{source_index}].exclude[{rule_index}]"),
            );
        }
        let branch_exclude = branch
            .and_then(|name| self.branches.get(name))
            .map(|restriction| restriction.exclude.as_slice())
            .unwrap_or(&[]);
        for (prefix, patterns) in [
            ("repository", self.exclude.as_slice()),
            (root.rule_prefix.as_str(), root.exclude),
            ("branch", branch_exclude),
        ] {
            if let Some(index) = matching_rule(patterns, &root.relative) {
                return CandidateDecision::excluded(
                    &path,
                    "path is excluded by the repository or branch policy",
                    format!("{prefix}.exclude[{index}]"),
                );
            }
        }
        let patterns = if root.workspace {
            self.include.as_slice()
        } else {
            root.include.unwrap_or(&[])
        };
        let Some(index) = matching_rule(patterns, &root.relative) else {
            return CandidateDecision::excluded(
                &path,
                if root.workspace {
                    "path does not match the repository include rules"
                } else {
                    "path does not match the root include rules"
                },
                format!("{}.include", root.rule_prefix),
            );
        };
        CandidateDecision::allowed(
            &path,
            format!("{}/{}", root.label, slash_path(&root.relative)),
            format!("{}.include[{index}]", root.rule_prefix),
        )
    }

    /// Evaluate a relative memory path. Memory is opt-in even inside the workspace.
    pub fn evaluate_memory(&self, path: &str, branch: Option<&str>) -> CandidateDecision {
        let relative = Path::new(path);
        if !is_relative_path(relative) {
            return CandidateDecision::excluded(
                relative,
                "memory candidates must be relative paths",
                "memory.relative_path",
            );
        }
        let normalized = slash_path(relative);
        if is_forced_exclude(relative) {
            return CandidateDecision::excluded(
                relative,
                "memory path matches the default sensitive-file exclusion",
                "default.sensitive_file",
            );
        }
        if let Some((source_index, source, rule_index)) =
            self.mandatory.iter().enumerate().find_map(|(i, source)| {
                matching_rule(&source.memory_exclude, relative).map(|rule| (i, source, rule))
            })
        {
            return CandidateDecision::excluded(
                relative,
                &format!("memory path is excluded by mandatory policy {}", source.id),
                format!("mandatory[{source_index}].memory_exclude[{rule_index}]"),
            );
        }
        let Some(index) = matching_rule(&self.memory_allow, relative) else {
            return CandidateDecision::excluded(
                relative,
                "memory path is not on the repository allowlist",
                "repository.memory_allow",
            );
        };
        if let Some(index) = branch
            .and_then(|name| self.branches.get(name))
            .and_then(|restriction| matching_rule(&restriction.memory_exclude, relative))
        {
            return CandidateDecision::excluded(
                relative,
                "memory path is excluded by the branch policy",
                format!("branch.memory_exclude[{index}]"),
            );
        }
        CandidateDecision::allowed(
            relative,
            format!("<memory>/{normalized}"),
            format!("repository.memory_allow[{index}]"),
        )
    }

    /// Build a deterministic report for a selected set of file and memory candidates.
    pub fn preview<'a, I>(
        &self,
        branch: Option<&str>,
        files: I,
        memory: &[&'a str],
    ) -> PreviewReport
    where
        I: IntoIterator<Item = &'a Path>,
    {
        let mut candidates = files
            .into_iter()
            .map(|path| self.evaluate_file(path, branch))
            .collect::<Vec<_>>();
        candidates.extend(memory.iter().map(|path| self.evaluate_memory(path, branch)));
        candidates.sort_by(|left, right| left.input.cmp(&right.input));
        let allowed = candidates
            .iter()
            .filter(|item| item.action == CandidateAction::Allowed)
            .count();
        let excluded = candidates
            .iter()
            .filter(|item| item.action == CandidateAction::Excluded)
            .count();
        let review = candidates
            .iter()
            .filter(|item| item.action == CandidateAction::Review)
            .count();
        PreviewReport {
            policy_version: self.version,
            policy_digest: self.digest().unwrap_or_else(|_| "invalid".into()),
            branch: branch.map(str::to_owned),
            allowed,
            excluded,
            review,
            candidates,
        }
    }

    fn match_root(&self, path: &Path) -> Option<RootMatch<'_>> {
        let mut best: Option<(usize, RootMatch<'_>)> = None;
        if let Some(workspace) = &self.workspace
            && let Some(relative) = relative_to(path, workspace)
        {
            best = Some((
                workspace.components().count(),
                RootMatch {
                    relative,
                    label: "<workspace>".into(),
                    workspace: true,
                    rule_prefix: "repository".into(),
                    include: None,
                    exclude: &[],
                },
            ));
        }
        for (index, root) in self.external_roots.iter().enumerate() {
            if let Some(relative) = relative_to(path, &root.path) {
                let depth = root.path.components().count();
                if best.as_ref().is_none_or(|current| depth > current.0) {
                    best = Some((
                        depth,
                        RootMatch {
                            relative,
                            label: format!("<{}>", root.label),
                            workspace: false,
                            rule_prefix: format!("external_roots[{index}]"),
                            include: Some(&root.include),
                            exclude: &root.exclude,
                        },
                    ));
                }
            }
        }
        best.map(|(_, root)| root)
    }
}

struct RootMatch<'a> {
    relative: PathBuf,
    label: String,
    workspace: bool,
    rule_prefix: String,
    include: Option<&'a [String]>,
    exclude: &'a [String],
}

/// A path-level decision. Content and attachment checks append later decisions to the same report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateDecision {
    pub input: String,
    /// Positional references identify policy rules without copying private patterns or source IDs.
    pub rule: String,
    pub action: CandidateAction,
    pub logical_path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateAction {
    Allowed,
    Excluded,
    Review,
}

impl CandidateDecision {
    fn allowed(path: &Path, logical_path: String, rule: impl Into<String>) -> Self {
        Self {
            input: path.to_string_lossy().into_owned(),
            rule: rule.into(),
            action: CandidateAction::Allowed,
            logical_path: Some(logical_path),
            reason: None,
        }
    }

    fn excluded(path: &Path, reason: &str, rule: impl Into<String>) -> Self {
        Self {
            input: path.to_string_lossy().into_owned(),
            rule: rule.into(),
            action: CandidateAction::Excluded,
            logical_path: None,
            reason: Some(reason.into()),
        }
    }
}

/// A preview is tied to the exact policy digest used to produce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewReport {
    pub policy_version: u32,
    pub policy_digest: String,
    pub branch: Option<String>,
    pub allowed: usize,
    pub excluded: usize,
    pub review: usize,
    pub candidates: Vec<CandidateDecision>,
}

fn policy_path(repo: &Repo) -> Result<PathBuf> {
    Ok(repo.common_dir()?.join(POLICY_DIRECTORY).join(POLICY_FILE))
}

fn ensure_valid_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty() && label.len() <= 64,
        "external root labels must be non-empty and bounded"
    );
    ensure!(
        label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "external root labels may contain only letters, digits, `-` and `_`"
    );
    Ok(())
}

fn ensure_no_control_path(path: &Path, name: &str) -> Result<()> {
    ensure!(
        !path.to_string_lossy().contains(['\0', '\n', '\r']),
        "{name} contains a control character"
    );
    Ok(())
}

fn validate_patterns(patterns: &[String]) -> Result<()> {
    for pattern in patterns {
        ensure!(
            !pattern.is_empty(),
            "privacy path patterns must not be empty"
        );
        ensure!(
            !pattern.starts_with('/') && !pattern.contains(['\0', '\n', '\r']),
            "privacy path patterns must be relative and free of control characters"
        );
        ensure!(
            is_relative_path(Path::new(pattern)),
            "privacy path patterns must not contain `..`"
        );
    }
    Ok(())
}

fn normalize_candidate_path(path: &Path) -> Option<PathBuf> {
    // A missing target cannot be canonicalized. Keeping parent components in that fallback
    // would let the pattern matcher discard them and classify an escaped file as project data.
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        || path
            .to_str()
            .is_none_or(|text| text.chars().any(char::is_control))
    {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    if let Ok(canonical) = absolute.canonicalize() {
        return Some(canonical);
    }
    let file_name = absolute.file_name()?.to_owned();
    let parent = absolute.parent()?.canonicalize().ok()?;
    Some(parent.join(file_name))
}

fn reject_symlink_path(path: &Path) -> Result<(), &'static str> {
    let current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "candidate path cannot be resolved from the current directory")?
            .join(path)
    };
    if let Ok(metadata) = fs::symlink_metadata(&current)
        && metadata.file_type().is_symlink()
    {
        return Err("symbolic links are excluded from privacy candidates");
    }
    Ok(())
}

fn relative_to(path: &Path, root: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    path.strip_prefix(root).ok().map(Path::to_path_buf)
}

fn is_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn is_forced_exclude(path: &Path) -> bool {
    let relative = slash_path(path);
    let segments: Vec<_> = relative.split('/').collect();
    if segments
        .iter()
        .any(|segment| matches!(*segment, ".git" | ".ssh" | ".aws" | ".gnupg"))
    {
        return true;
    }
    let Some(name) = segments.last().copied() else {
        return true;
    };
    name == ".env"
        || name.starts_with(".env.")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
}

fn matching_rule(patterns: &[String], path: &Path) -> Option<usize> {
    let path = slash_path(path);
    patterns
        .iter()
        .position(|pattern| glob_match(pattern, &path))
}

fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<_> = pattern.split('/').collect();
    let path: Vec<_> = path.split('/').collect();
    glob_segments(&pattern, &path)
}

fn glob_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((head, rest)) if *head == "**" => {
            glob_segments(rest, path) || (!path.is_empty() && glob_segments(pattern, &path[1..]))
        }
        Some((head, rest)) => {
            !path.is_empty() && segment_match(head, path[0]) && glob_segments(rest, &path[1..])
        }
    }
}

fn segment_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<_> = pattern.chars().collect();
    let value: Vec<_> = value.chars().collect();
    let mut table = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    table[0][0] = true;
    for i in 0..pattern.len() {
        for j in 0..=value.len() {
            if !table[i][j] {
                continue;
            }
            match pattern[i] {
                '*' => {
                    table[i + 1][j] = true;
                    if j < value.len() {
                        table[i][j + 1] = true;
                    }
                }
                '?' if j < value.len() => table[i + 1][j + 1] = true,
                literal if j < value.len() && literal == value[j] => table[i + 1][j + 1] = true,
                _ => {}
            }
        }
    }
    table[pattern.len()][value.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_fail_closed_and_memory_is_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("README.md");
        fs::write(&file, "readable").unwrap();
        let policy = PrivacyPolicy::default();
        assert_eq!(
            policy.evaluate_file(&file, None).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            policy.evaluate_memory("team.md", None).action,
            CandidateAction::Excluded
        );
    }

    #[test]
    fn workspace_and_external_roots_keep_only_explicit_patterns() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let external = temp.path().join("contracts");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(&external).unwrap();
        let source = workspace.join("src/main.rs");
        let config = external.join("contract.yaml");
        fs::write(&source, "fn main() {}\n").unwrap();
        fs::write(&config, "name: example\n").unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(workspace.clone()),
            external_roots: vec![ExternalRoot {
                label: "contracts".into(),
                path: external,
                include: vec!["**/*.yaml".into()],
                exclude: vec![],
            }],
            include: vec!["src/**".into()],
            ..PrivacyPolicy::default()
        };
        let decision = policy.evaluate_file(&source, None);
        assert_eq!(
            decision.logical_path.as_deref(),
            Some("<workspace>/src/main.rs")
        );
        let external = policy.evaluate_file(&config, None);
        assert_eq!(external.action, CandidateAction::Allowed);
        assert_eq!(external.rule, "external_roots[0].include[0]");
        assert_eq!(
            external.logical_path.as_deref(),
            Some("<contracts>/contract.yaml")
        );
    }

    #[test]
    fn default_workspace_allowlist_is_conservative() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src/main.rs");
        let private = temp.path().join("notes.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "fn main() {}\n").unwrap();
        fs::write(&private, "private\n").unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_path_buf()),
            ..PrivacyPolicy::default()
        };
        assert_eq!(
            policy.evaluate_file(&source, None).action,
            CandidateAction::Allowed
        );
        assert_eq!(
            policy.evaluate_file(&private, None).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            policy
                .evaluate_file(&temp.path().join("src/../../missing-private.txt"), None)
                .action,
            CandidateAction::Excluded
        );
    }

    #[test]
    fn forced_exclusions_win_over_includes_and_branch_rules_only_narrow() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().to_path_buf();
        let secret = workspace.join(".env");
        fs::write(&secret, "TOKEN=x\n").unwrap();
        let document = workspace.join("docs/private.md");
        fs::create_dir_all(document.parent().unwrap()).unwrap();
        let mut policy = PrivacyPolicy {
            workspace: Some(workspace),
            include: vec!["**/*".into()],
            memory_allow: vec!["team.md".into()],
            ..PrivacyPolicy::default()
        };
        policy.branches.insert(
            "public".into(),
            BranchRestriction {
                exclude: vec!["docs/**".into()],
                memory_exclude: vec!["team.md".into()],
            },
        );
        assert_eq!(
            policy.evaluate_file(&secret, Some("public")).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            policy.evaluate_file(&secret, Some("public")).rule,
            "default.sensitive_file"
        );
        assert_eq!(
            policy.evaluate_file(&document, Some("public")).rule,
            "branch.exclude[0]"
        );
        assert_eq!(
            policy.evaluate_file(&document, None).rule,
            "repository.include[0]"
        );
        assert_eq!(
            policy.evaluate_memory("team.md", Some("public")).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            policy.evaluate_memory("team.md", None).action,
            CandidateAction::Allowed
        );
        assert_eq!(
            policy.evaluate_memory("team.md", Some("public")).rule,
            "branch.memory_exclude[0]"
        );
        policy.exclude = vec!["unused/**".into(), "docs/**".into()];
        assert_eq!(
            policy.evaluate_file(&document, Some("public")).rule,
            "repository.exclude[1]"
        );
        policy.mandatory.push(mandatory::MandatoryPolicy {
            version: 1,
            id: "private-organization-name".into(),
            revision: "r1".into(),
            exclude: vec!["docs/**".into()],
            memory_exclude: vec!["team.md".into()],
        });
        assert_eq!(
            policy.evaluate_file(&document, Some("public")).rule,
            "mandatory[0].exclude[0]"
        );
        assert_eq!(
            policy.evaluate_memory("team.md", Some("public")).rule,
            "mandatory[0].memory_exclude[0]"
        );
    }

    #[test]
    fn digest_is_stable_and_version_is_validated() {
        let mut policy = PrivacyPolicy::default();
        assert_eq!(policy.digest().unwrap(), policy.digest().unwrap());
        policy.version += 1;
        assert!(policy.digest().is_err());
    }

    #[test]
    fn policy_file_round_trips_outside_the_tracked_tree() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&temp.path().join("agent")).unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_path_buf()),
            memory_allow: vec!["team.md".into()],
            ..PrivacyPolicy::default()
        };
        policy.save(&repo).unwrap();
        let path = PrivacyPolicy::path(&repo).unwrap();
        assert!(path.starts_with(repo.common_dir().unwrap()));
        assert_eq!(PrivacyPolicy::load(&repo).unwrap(), policy);
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }
}
