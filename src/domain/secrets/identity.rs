//! Occurrence-scoped identity evidence, reconstructed from native command/result records.

use crate::domain::{meta, repo::Repo};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const MAX_PROOFS: usize = 1024;
static CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:refs/tags/)?agit-[0-9a-f]+|[0-9a-f]{4,64}").unwrap());
static GIT_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[0-9a-f]{4,64}\z").unwrap());
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\Aagit-(?:[0-9a-f]{8}|[0-9a-f]{40})\z").unwrap());

#[derive(Clone, Debug, Default)]
pub(crate) struct RecordMask(pub Vec<(String, Range<usize>)>);

impl RecordMask {
    /// Substitution touches the proven occurrence only, leaving neighboring credentials visible.
    pub(crate) fn apply(&self, value: &mut Value, mut replacement: impl FnMut(&str) -> String) {
        for (pointer, span) in self.0.iter().rev() {
            if let Some(Value::String(text)) = value.pointer_mut(pointer)
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end)
                && span.end <= text.len()
            {
                let next = replacement(&text[span.clone()]);
                text.replace_range(span.clone(), &next);
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Operation {
    root: PathBuf,
    program: String,
    verb: String,
    commit: bool,
    oneline: bool,
}

/// Evidence is replayable from saved native records; it is never a global value allowlist.
pub(crate) struct Evidence {
    agent: Repo,
    cwd: PathBuf,
    cwd_is_agent: bool,
    calls: HashMap<String, Operation>,
    completed: HashMap<String, Operation>,
    seen_calls: HashSet<String>,
    proven: HashSet<(PathBuf, String, bool)>,
    resolved: HashMap<(PathBuf, String, bool), Option<String>>,
    cwd_aliases: HashSet<PathBuf>,
}

impl Evidence {
    pub(crate) fn new(agent: &Repo, cwd: &Path) -> Self {
        Self {
            agent: Repo::at(agent.root()).local_objects_only(),
            cwd: cwd.to_owned(),
            cwd_is_agent: false,
            calls: HashMap::new(),
            completed: HashMap::new(),
            seen_calls: HashSet::new(),
            proven: HashSet::new(),
            resolved: HashMap::new(),
            cwd_aliases: HashSet::new(),
        }
    }

    pub(crate) fn with_agent_cwd(mut self, is_agent: bool) -> Self {
        self.cwd_is_agent = is_agent;
        self
    }

    #[cfg(feature = "rc")]
    pub(crate) fn reset(&mut self) {
        *self = Self::new(&self.agent, &self.cwd);
    }

    pub(crate) fn add_cwd_alias(&mut self, alias: &str) {
        self.cwd_aliases.insert(PathBuf::from(alias));
    }

    #[cfg(feature = "secret-vault")]
    pub(crate) fn seed_native(&mut self, runtime: &str, native: &str) -> crate::Result<()> {
        let repo = Repo::at(self.agent.root()).local_objects_only();
        let cwd = self.cwd.clone();
        super::seed_native_evidence(&repo, &cwd, runtime, native, self)
    }

    /// A text delta has no typed result field. Delay possible object identities until its native record.
    #[cfg(feature = "rc")]
    pub(crate) fn contains_object_identity(&mut self, text: &str) -> bool {
        let roots = [self.agent.root().to_owned(), self.cwd.clone()];
        candidates(text).any(|span| {
            let token = &text[span];
            token.len() >= 32
                && roots
                    .iter()
                    .any(|root| self.resolve(root, token, false).is_some())
        })
    }

    pub(crate) fn record(&mut self, runtime: &str, native: &str, value: &Value) -> RecordMask {
        let mut mask = RecordMask::default();
        for pointer in native_session_pointers(runtime, native, value) {
            if let Some(text) = value.pointer(pointer).and_then(Value::as_str) {
                mask.0.push((pointer.into(), 0..text.len()));
            }
        }
        match runtime {
            "codex" if value["type"] == "response_item" => {
                let item = &value["payload"];
                match item["type"].as_str() {
                    Some("function_call") => {
                        if matches!(
                            item["name"].as_str(),
                            Some("exec_command" | "functions.exec_command" | "shell_command")
                        ) && let Some(args) = item["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        {
                            self.call(item["call_id"].as_str(), &args);
                        }
                    }
                    Some("function_call_output") => {
                        if let Some(id) = item["call_id"].as_str()
                            && let Some(op) = self
                                .calls
                                .remove(id)
                                .or_else(|| self.completed.get(id).cloned())
                            && let Some(text) = item["output"].as_str()
                        {
                            self.output(&op, text, "/payload/output", &mut mask);
                            self.completed.insert(id.into(), op);
                        }
                    }
                    Some("message") if item["role"] == "assistant" => {
                        if let Some(blocks) = item["content"].as_array() {
                            for (i, block) in blocks.iter().enumerate() {
                                if block["type"] == "output_text"
                                    && let Some(text) = block["text"].as_str()
                                {
                                    self.narrative(
                                        text,
                                        &format!("/payload/content/{i}/text"),
                                        &mut mask,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "claude-code" if value.pointer("/message/role") == value.get("type") => {
                if let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) {
                    for (i, block) in blocks.iter().enumerate() {
                        match (value["type"].as_str(), block["type"].as_str()) {
                            (Some("assistant"), Some("tool_use")) if block["name"] == "Bash" => {
                                self.call(block["id"].as_str(), &block["input"]);
                            }
                            (Some("user"), Some("tool_result")) if block["is_error"] != true => {
                                if let Some(id) = block["tool_use_id"].as_str()
                                    && let Some(op) = self
                                        .calls
                                        .remove(id)
                                        .or_else(|| self.completed.get(id).cloned())
                                    && let Some(text) = block["content"].as_str()
                                {
                                    self.output(
                                        &op,
                                        text,
                                        &format!("/message/content/{i}/content"),
                                        &mut mask,
                                    );
                                    self.completed.insert(id.into(), op);
                                }
                            }
                            (Some("assistant"), Some("text")) => {
                                if let Some(text) = block["text"].as_str() {
                                    self.narrative(
                                        text,
                                        &format!("/message/content/{i}/text"),
                                        &mut mask,
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        mask.0
            .sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start.cmp(&b.1.start)));
        mask
    }

    fn call(&mut self, id: Option<&str>, args: &Value) {
        let Some(id) = id.filter(|id| id.len() <= 256) else {
            return;
        };
        let operation = self.operation(args);
        if self.seen_calls.len() >= MAX_PROOFS || !self.seen_calls.insert(id.to_owned()) {
            self.calls.remove(id);
            if operation.as_ref() != self.completed.get(id) {
                self.completed.remove(id);
            }
            return;
        }
        if let Some(op) = operation {
            self.calls.insert(id.into(), op);
        }
    }

    fn operation(&self, args: &Value) -> Option<Operation> {
        let command = args["cmd"].as_str().or_else(|| args["command"].as_str())?;
        let words = command_words(command)?;
        let program = words.first()?.as_str();
        if !matches!(program, "git" | "agit") {
            return None;
        }
        let mut at = 1;
        let mut root = args["workdir"]
            .as_str()
            .or_else(|| args["cwd"].as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.to_owned());
        if words.get(at).map(String::as_str) == Some("-C") {
            root = root.join(words.get(at + 1)?);
            at += 2;
        }
        let verb = words.get(at)?.as_str();
        if !matches!(
            verb,
            "commit" | "log" | "show" | "rev-parse" | "rev-list" | "show-ref"
        ) {
            return None;
        }
        let options = &words[at + 1..];
        if options.iter().any(|arg| {
            arg.starts_with("--format") || arg.starts_with("--pretty") || arg.starts_with("--sq")
        }) {
            return None;
        }
        if verb == "rev-parse"
            && options.iter().any(|arg| {
                arg.starts_with('-')
                    && arg != "--verify"
                    && arg != "--short"
                    && !arg.starts_with("--short=")
            })
        {
            return None;
        }
        if program == "agit" {
            if !matches!(verb, "commit" | "log" | "show") {
                return None;
            }
            // Explicit destinations must agree with the selected repository, never a neighboring checkout.
            for word in &words[at + 1..] {
                if let Some((slug, _)) = word.split_once('@') {
                    let agent = self.agent.root().file_name()?.to_str()?;
                    let owner = self.agent.root().parent()?.file_name()?.to_str()?;
                    if slug != format!("{owner}/{agent}") {
                        return None;
                    }
                }
            }
            root = self.agent.root().to_owned();
        } else {
            if self.cwd_aliases.contains(&root) {
                root = self.cwd.clone();
            }
            if self.cwd_is_agent && root == self.cwd {
                root = self.agent.root().to_owned();
            } else {
                root = root.canonicalize().ok()?;
                if root != self.cwd.canonicalize().ok()?
                    && root != self.agent.root().canonicalize().ok()?
                {
                    return None;
                }
            }
        }
        let commit = program == "agit" || matches!(verb, "commit" | "log" | "rev-list");
        Some(Operation {
            root,
            program: program.into(),
            verb: verb.into(),
            commit,
            oneline: options.iter().any(|arg| arg == "--oneline"),
        })
    }

    fn resolve(&mut self, root: &Path, token: &str, commit: bool) -> Option<String> {
        let key = (root.to_owned(), token.to_owned(), commit);
        if matches!(token.len(), 40 | 64)
            && GIT_ID.is_match(token)
            && let Some(result) = self.resolved.get(&key)
        {
            return result.clone();
        }
        if self.resolved.len() >= MAX_PROOFS {
            return None;
        }
        let result = resolve_identity(&Repo::at(root).local_objects_only(), token, commit);
        self.resolved.insert(key, result.clone());
        result
    }

    fn output(&mut self, op: &Operation, text: &str, pointer: &str, mask: &mut RecordMask) {
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            for span in candidates(line) {
                let token = &line[span.clone()];
                if output_location(op, line, &span)
                    && let Some(oid) = self.resolve(&op.root, token, op.commit)
                {
                    self.proven.insert((op.root.clone(), oid, op.commit));
                    mask.0
                        .push((pointer.into(), offset + span.start..offset + span.end));
                }
            }
            offset += line.len();
        }
    }

    fn narrative(&mut self, text: &str, pointer: &str, mask: &mut RecordMask) {
        for span in candidates(text) {
            let line_prefix = text[..span.start].rsplit('\n').next().unwrap_or("");
            if line_prefix.split(['=', ':']).next().is_some_and(|prefix| {
                prefix
                    .split_whitespace()
                    .last()
                    .is_some_and(super::is_credential_field)
            }) && line_prefix.contains(['=', ':'])
            {
                continue;
            }
            let prefix = text[..span.start].trim_end_matches([' ', '`', '[', '(']);
            let label = prefix
                .rsplit(|c: char| !c.is_ascii_alphabetic())
                .next()
                .unwrap_or("");
            if !matches!(
                label.to_ascii_lowercase().as_str(),
                "commit" | "version" | "ref" | "revision"
            ) {
                continue;
            }
            let token = &text[span.clone()];
            let proven: Vec<_> = self.proven.iter().cloned().collect();
            if proven.into_iter().any(|(root, oid, commit)| {
                self.resolve(&root, token, commit).as_ref() == Some(&oid)
            }) {
                mask.0.push((pointer.into(), span));
            }
        }
    }
}

fn candidates(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    CANDIDATE
        .find_iter(text)
        .filter(|m| {
            let word = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/');
            !m.start()
                .checked_sub(1)
                .is_some_and(|i| word(text.as_bytes()[i]))
                && !text.as_bytes().get(m.end()).is_some_and(|b| word(*b))
        })
        .map(|m| m.range())
}

fn output_location(op: &Operation, line: &str, span: &Range<usize>) -> bool {
    let before = line[..span.start].trim();
    let after = line[span.end..].trim();
    if op.program == "agit" {
        return before
            .strip_prefix('#')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            || (before.is_empty() && VERSION.is_match(&line[span.clone()]));
    }
    match op.verb.as_str() {
        "commit" => before.starts_with('[') && !before.contains(']') && after.starts_with(']'),
        "rev-parse" | "rev-list" => before.is_empty() && after.is_empty(),
        "log" | "show" => &line[..span.start] == "commit " || op.oneline && span.start == 0,
        "show-ref" => {
            before.is_empty() && after.starts_with("refs/")
                || before.split_whitespace().count() == 1 && before.len() >= 40 && after.is_empty()
        }
        _ => false,
    }
}

/// Only literal single commands are evidence. Shell expansions, pipelines and scripts need their own parser.
fn command_words(command: &str) -> Option<Vec<String>> {
    if command.len() > 16 * 1024 {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in command.chars() {
        if matches!(c, '$' | '`' | '\\' | '\n' | '\r') {
            return None;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
            return None;
        } else if c.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

pub(crate) fn resolve_identity(repo: &Repo, token: &str, commit: bool) -> Option<String> {
    let width = match repo
        .git(&["rev-parse", "--show-object-format"])
        .ok()?
        .as_str()
    {
        "sha1" => 40,
        "sha256" => 64,
        _ => return None,
    };
    let reference = token.strip_prefix("refs/tags/");
    let version = reference.unwrap_or(token);
    let (hex, is_version) = if VERSION.is_match(version) && width == meta::ID_HEX_LEN {
        (version.strip_prefix(meta::ID_PREFIX)?, true)
    } else if GIT_ID.is_match(token) && token.len() <= width && reference.is_none() {
        (token, false)
    } else {
        return None;
    };
    // Git's disambiguation considers every object type, not only commits.
    let objects = repo
        .git(&["rev-parse", &format!("--disambiguate={hex}")])
        .ok()?;
    let mut objects = objects.lines();
    let oid = objects.next()?;
    if objects.next().is_some() || oid.len() != width {
        return None;
    }
    let kind = repo.git(&["cat-file", "-t", oid]).ok()?;
    if (commit || is_version) && kind != "commit" {
        return None;
    }
    if is_version {
        let full = meta::id_from_sha(oid);
        let tag = format!("refs/tags/{full}");
        let (status, _, _) = repo
            .git_status(&["show-ref", "--verify", "--quiet", &tag])
            .ok()?;
        if status == Some(0) {
            if repo
                .git(&["rev-parse", "--verify", &format!("{tag}^{{commit}}")])
                .ok()?
                != oid
            {
                return None;
            }
        } else if reference.is_some() || status != Some(1) {
            return None;
        }
        let metadata = meta::read_at_ref_result(repo, oid).ok()??;
        if meta::validate(&metadata).is_err() {
            return None;
        }
    }
    Some(oid.to_owned())
}

pub(crate) fn empty_status_digest(state: &meta::CwdState) -> bool {
    use sha2::Digest as _;
    state.worktree == meta::WorktreeStatus::Clean
        && state.staged == 0
        && state.unstaged == 0
        && state.untracked == 0
        && state.conflicted == 0
        && state.status_digest.as_deref() == Some(hex::encode(sha2::Sha256::digest([])).as_str())
}

pub(crate) fn native_session_pointers(
    runtime: &str,
    native: &str,
    value: &Value,
) -> Vec<&'static str> {
    let canonical_uuid =
        uuid::Uuid::parse_str(native).is_ok_and(|uuid| uuid.hyphenated().to_string() == native);
    if !canonical_uuid {
        return Vec::new();
    }
    match runtime {
        "codex"
            if value["type"] == "session_meta"
                && value.pointer("/payload/id").and_then(Value::as_str) == Some(native)
                && value
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .is_some()
                && value
                    .pointer("/payload/timestamp")
                    .and_then(Value::as_str)
                    .is_some() =>
        {
            vec!["/payload/id"]
        }
        "claude-code"
            if value["sessionId"] == native
                && matches!(value["type"].as_str(), Some("user" | "assistant"))
                && value.pointer("/message/role").and_then(Value::as_str)
                    == value["type"].as_str()
                && value.pointer("/message/content").is_some() =>
        {
            vec!["/sessionId"]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    fn object(repo: &Repo, kind: &str, bytes: &[u8]) -> String {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(bytes).unwrap();
        input.rewind().unwrap();
        repo.git_with_stdin_file(&["hash-object", "-w", "-t", kind, "--stdin"], input)
            .unwrap()
    }

    #[test]
    fn identity_resolution_checks_repository_object_format_type_and_every_prefix_collision() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("one")).unwrap();
        object(&repo, "tree", b"");
        let commit = object(&repo, "commit", b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor Example <test@example.invalid> 0 +0000\ncommitter Example <test@example.invalid> 0 +0000\n\nidentity fixture\n");
        assert_eq!(commit, "8874e1c5052247eadf5c8912c271d3fc0786bf2e");
        assert_eq!(resolve_identity(&repo, &commit, true), Some(commit.clone()));
        assert_eq!(resolve_identity(&repo, "8874", true), Some(commit.clone()));
        let blob = object(&repo, "blob", b"collision-23797");
        assert!(blob.starts_with("8874"));
        assert_eq!(resolve_identity(&repo, "8874", true), None);
        assert_eq!(resolve_identity(&repo, &blob, true), None);
        assert_eq!(resolve_identity(&repo, &blob, false), Some(blob));
        let other = Repo::init(&dir.path().join("two")).unwrap();
        assert_eq!(resolve_identity(&other, &commit, true), None);
        assert_eq!(
            resolve_identity(&repo, &format!("{commit}000000000000000000000000"), true),
            None
        );
        assert_eq!(
            resolve_identity(&repo, &format!("agit-{commit}"), true),
            None
        );

        let root = dir.path().join("sha256");
        repo.git(&["init", "--object-format=sha256", root.to_str().unwrap()])
            .unwrap();
        let sha256 = Repo::at(root);
        let tree = object(&sha256, "tree", b"");
        let raw = format!(
            "tree {tree}\nauthor Example <test@example.invalid> 0 +0000\ncommitter Example <test@example.invalid> 0 +0000\n\nsha256 fixture\n"
        );
        let oid = object(&sha256, "commit", raw.as_bytes());
        assert_eq!(oid.len(), 64);
        assert_eq!(resolve_identity(&sha256, &oid, true), Some(oid.clone()));
        assert_eq!(
            resolve_identity(&sha256, &oid[..8], true),
            Some(oid.clone())
        );
        assert_eq!(
            resolve_identity(&sha256, &format!("agit-{oid}"), true),
            None
        );
    }

    #[test]
    fn reserved_version_tags_must_agree_with_the_version_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.git(&["config", "user.name", "Identity fixture"])
            .unwrap();
        repo.git(&["config", "user.email", "test@example.invalid"])
            .unwrap();
        std::fs::create_dir_all(dir.path().join("session")).unwrap();
        std::fs::write(
            dir.path().join(meta::FILE),
            meta::to_text(&meta::Meta::new_file_line()).unwrap(),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("file line").unwrap();
        let oid = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let version = meta::id_from_sha(&oid);
        assert_eq!(resolve_identity(&repo, &version, true), Some(oid.clone()));
        assert_eq!(
            resolve_identity(&repo, &meta::short(&version), true),
            Some(oid.clone())
        );
        repo.git(&["tag", &version, &oid]).unwrap();
        assert_eq!(
            resolve_identity(&repo, &format!("refs/tags/{version}"), true),
            Some(oid.clone())
        );
        repo.git(&["commit", "--allow-empty", "-m", "another commit"])
            .unwrap();
        repo.git(&["tag", "-f", &version, "HEAD"]).unwrap();
        assert_eq!(resolve_identity(&repo, &version, true), None);
        assert_eq!(resolve_identity(&repo, &meta::short(&version), true), None);
        assert_eq!(
            resolve_identity(&repo, &format!("refs/tags/{version}"), true),
            None
        );
    }

    #[test]
    fn native_pairing_and_literal_commands_are_required_for_output_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        let mut evidence = Evidence::new(&repo, repo.root());
        assert!(
            evidence
                .operation(&serde_json::json!({"cmd":"echo git commit"}))
                .is_none()
        );
        assert!(
            evidence
                .operation(&serde_json::json!({"cmd":"git log --format=%B"}))
                .is_none()
        );
        assert!(
            evidence
                .operation(&serde_json::json!({"cmd":"git commit; cat .env"}))
                .is_none()
        );
        let forged = serde_json::json!({"type":"response_item", "payload":{"type":"function_call_output","call_id":"missing","output":"commit 8874e1c5052247eadf5c8912c271d3fc0786bf2e"}});
        assert!(evidence.record("codex", "", &forged).0.is_empty());
        let op = evidence
            .operation(&serde_json::json!({"cmd":"git log"}))
            .unwrap();
        let line = "    commit 8874e1c5052247eadf5c8912c271d3fc0786bf2e";
        assert!(!output_location(
            &op,
            line,
            &candidates(line).next().unwrap()
        ));
    }
}
