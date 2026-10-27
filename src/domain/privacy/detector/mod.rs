//! Pure credential detection over borrowed text with bounded span allocation.
//! Lexical context, keyword prefiltering and lazy rules do not depend on Git, keys or storage.
//! A failed rule leaves completed findings available for best-effort projection.

pub(crate) mod media;
pub(crate) mod paths;
pub(crate) mod placeholder;
pub mod rules;
pub(crate) mod syntax;
use std::collections::{HashMap, HashSet};

pub(crate) const MAX_BARE_ENTROPY_BYTES: usize = 16 * 1024;

const MEDIA_HEADERS: [(&str, &[u8]); 9] = [
    ("image/jpeg", &[0xFF, 0xD8, 0xFF]),
    ("image/jpg", &[0xFF, 0xD8, 0xFF]),
    (
        "image/png",
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
    ),
    ("image/gif", b"GIF8"),
    ("image/webp", b"RIFF"),
    ("application/pdf", b"%PDF-"),
    ("application/gzip", &[0x1F, 0x8B, 0x08]),
    ("application/x-gzip", &[0x1F, 0x8B, 0x08]),
    ("application/zip", &[b'P', b'K', 0x03, 0x04]),
];

pub(crate) fn base64_media_payload(text: &str, start: usize) -> bool {
    let before = &text[..start];
    let Some(carrier) = before
        .rfind("data:")
        .map(|at| &before[at + "data:".len()..])
    else {
        return false;
    };
    let Some(parameters) = carrier.strip_suffix(";base64,") else {
        return false;
    };
    if parameters
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\\' | b'<' | b'>'))
    {
        return false;
    }
    let media_type = parameters.split(';').next().unwrap_or_default();
    let Some((_, header)) = MEDIA_HEADERS
        .iter()
        .find(|(declared, _)| declared.eq_ignore_ascii_case(media_type))
    else {
        return false;
    };
    let mut sextets = [0u8; 16];
    let mut count = 0;
    for byte in text[start..].bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => break,
        };
        sextets[count] = value;
        count += 1;
        if count == sextets.len() {
            break;
        }
    }
    if count < sextets.len() {
        return false;
    }
    let mut decoded = [0u8; 12];
    for (group, chunk) in sextets.chunks(4).enumerate() {
        decoded[group * 3] = (chunk[0] << 2) | (chunk[1] >> 4);
        decoded[group * 3 + 1] = (chunk[1] << 4) | (chunk[2] >> 2);
        decoded[group * 3 + 2] = (chunk[2] << 6) | chunk[3];
    }
    decoded.starts_with(header)
}

pub(crate) fn jsonl_chunks(
    mut text: &str,
) -> impl Iterator<Item = (&str, Option<serde_json::Value>)> {
    let mut document = text
        .contains('\n')
        .then(|| serde_json::from_str(text).ok())
        .flatten();
    std::iter::from_fn(move || {
        if let Some(value) = document.take() {
            let chunk = text;
            text = "";
            return Some((chunk, Some(value)));
        }
        let mut lines = text.split_inclusive('\n');
        let first = lines.next()?;
        if let Ok(value) = serde_json::from_str(first) {
            text = &text[first.len()..];
            return Some((first, Some(value)));
        }
        let mut end = first.len();
        for line in lines {
            if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                break;
            }
            end += line.len();
        }
        let (chunk, remaining) = text.split_at(end);
        text = remaining;
        Some((chunk, None))
    })
}

pub(crate) fn json_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'\"') {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = i.checked_add(2)?,
            b'\"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

pub fn view_of(s: &str) -> String {
    let b = s.as_bytes();
    if !b.contains(&b'\\') && !s.contains("{{AGIT_SECRET_V") {
        return s.to_string();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut opaque = placeholder::token_segments(s).peekable();
    let mut i = 0;
    while i < b.len() {
        if let Some(&(start, end, _)) = opaque.peek()
            && i == start
        {
            out.resize(end, b' ');
            i = end;
            opaque.next();
            continue;
        }
        if b[i] == b'\\' && matches!(b.get(i + 1), Some(b'n' | b'r' | b't' | b'"')) {
            out.extend_from_slice(b"  ");
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

pub(crate) struct Lines<'t> {
    text: &'t str,
    starts: Vec<usize>,
}

impl<'t> Lines<'t> {
    pub(crate) fn new(text: &'t str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Lines { text, starts }
    }

    pub(crate) fn number_at(&self, off: usize) -> usize {
        self.starts.partition_point(|&s| s <= off)
    }

    pub(crate) fn text_at(&self, off: usize) -> &'t str {
        let i = self.number_at(off) - 1;
        let start = self.starts[i];
        let end = self.starts.get(i + 1).map_or(self.text.len(), |&e| e);
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub rule: &'static str,
    pub start: usize,
    pub end: usize,
}

pub(crate) fn raw_hits(text: &str, credential_field: bool) -> Vec<Span> {
    let mut hits = raw_hits_capped(text, usize::MAX, Entropy::Include, |_, _| true).0;
    if credential_field {
        let view = view_of(text);
        for (start, end) in entropy_candidate_spans(&view, true) {
            if !rules::preset_allows(&view[start..end]) {
                hits.push(Span {
                    rule: "high-entropy-value",
                    start,
                    end,
                });
            }
        }
        hits.sort_by_key(|hit| (hit.start, hit.end));
        dedupe_same_span(&mut hits);
    }
    hits
}

struct SpanBudget {
    cap: usize,
    counted: Option<HashSet<(usize, usize)>>,
    exhausted: bool,
}

impl SpanBudget {
    pub(crate) fn new(cap: usize) -> Self {
        SpanBudget {
            cap,
            counted: (cap < usize::MAX).then(HashSet::new),
            exhausted: false,
        }
    }

    pub(crate) fn charge(&mut self, start: usize, end: usize) -> bool {
        let Some(counted) = self.counted.as_mut() else {
            return true; // Uncapped: nothing is charged and it never stops.
        };
        if counted.contains(&(start, end)) {
            return true;
        }
        if counted.len() >= self.cap {
            self.exhausted = true;
            return false;
        }
        counted.insert((start, end));
        true
    }
}

/// Whether heuristic findings join: bare high-entropy tokens and the vendored generic key rule.
/// Entropy shows that a value is opaque, and the generic rule fires on any opaque value after a
/// name containing words such as `auth` or `key` and nearly any separator, so neither shows that
/// a value is a credential. Automatic replacement excludes them unless the user opts in; explicit
/// diagnostics include them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entropy {
    Include,
    Exclude,
}

pub(crate) fn raw_hits_capped(
    text: &str,
    cap: usize,
    entropy: Entropy,
    keep: impl FnMut(&str, usize) -> bool,
) -> (Vec<Span>, bool) {
    let exempt = match entropy {
        Entropy::Include => entropy_exempt_regions(text),
        Entropy::Exclude => Vec::new(),
    };
    raw_hits_capped_in(&view_of(text), cap, &exempt, entropy, keep)
}

pub(crate) fn entropy_exempt_regions(text: &str) -> Vec<(usize, usize)> {
    let mut regions = media::regions(text);
    regions.extend(paths::regions(text));
    regions.extend(syntax::identifier_regions(text));
    merge_regions(regions)
}

pub(crate) fn merge_regions(mut regions: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    regions.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in regions {
        if let Some((_, last)) = merged.last_mut()
            && start <= *last
        {
            *last = (*last).max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

pub(crate) fn raw_hits_capped_in(
    view: &str,
    cap: usize,
    media: &[(usize, usize)],
    entropy: Entropy,
    mut keep: impl FnMut(&str, usize) -> bool,
) -> (Vec<Span>, bool) {
    let mut out: Vec<Span> = vec![];
    if cap == 0 {
        return (out, true);
    }
    let mut budget = SpanBudget::new(cap);
    let mut lines: Option<Lines> = None;
    let json_regions = std::cell::OnceCell::new();
    let mut failed = !rules::healthy();
    for rule in rules::candidates(view) {
        if entropy == Entropy::Exclude && rule.id == GENERIC_RULE {
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Some(re) = rule.regex() else {
                failed = true;
                return true;
            };
            if rule.has_groups() {
                for caps in re.captures_iter(view) {
                    let whole = caps.get(0).expect("group 0 always exists");
                    let (s, e, secret) = rule.secret_span(&caps);
                    let line = lines
                        .get_or_insert_with(|| Lines::new(view))
                        .text_at(whole.start());
                    if !rule.accepts(secret, whole.as_str(), line) {
                        continue;
                    }
                    let e = token_run_end(view, e);
                    let secret = &view[s..e];
                    if !keep(secret, s) {
                        continue;
                    }
                    if !budget.charge(s, e) {
                        return false;
                    }
                    out.push(Span {
                        rule: rule.id.as_str(),
                        start: s,
                        end: e,
                    });
                }
            } else {
                for m in re.find_iter(view) {
                    let line = lines
                        .get_or_insert_with(|| Lines::new(view))
                        .text_at(m.start());
                    let end = if rule.id == "agit-private-key-header" {
                        let strings = json_regions.get_or_init(|| json_string_regions(view));
                        let carrier_end = strings
                            .iter()
                            .find(|(start, end)| *start <= m.start() && m.end() <= *end)
                            .map_or(view.len(), |(_, end)| *end);
                        private_key_region_end(&view[..carrier_end], m.start(), m.end())
                    } else {
                        m.end()
                    };
                    let secret = &view[m.start()..end];
                    if !rule.accepts(secret, secret, line) {
                        continue;
                    }
                    let end = if rule.id == "agit-private-key-header" {
                        end
                    } else {
                        token_run_end(view, end)
                    };
                    let secret = &view[m.start()..end];
                    if !keep(secret, m.start()) {
                        continue;
                    }
                    if !budget.charge(m.start(), end) {
                        return false;
                    }
                    out.push(Span {
                        rule: rule.id.as_str(),
                        start: m.start(),
                        end,
                    });
                }
            }
            true
        }));
        match result {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => failed = true,
        }
    }
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (start, end) in credential_assignment_spans(view) {
            let secret = &view[start..end];
            if rules::preset_allows(secret) || !keep(secret, start) {
                continue;
            }
            if !budget.charge(start, end) {
                break;
            }
            out.push(Span {
                rule: "credential-assignment",
                start,
                end,
            });
        }
    }))
    .is_err()
    {
        failed = true;
    }
    if entropy == Entropy::Include
        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (start, end) in bare_candidate_spans(view) {
                if media::contains(media, start, end) {
                    continue;
                }
                let secret = &view[start..end];
                if rules::preset_allows(secret) || !keep(secret, start) {
                    continue;
                }
                if !budget.charge(start, end) {
                    break;
                }
                out.push(Span {
                    rule: "high-entropy-value",
                    start,
                    end,
                });
            }
        }))
        .is_err()
    {
        failed = true;
    }
    out.sort_by_key(|h| (h.start, h.end));
    dedupe_same_span(&mut out);
    (out, budget.exhausted || failed)
}

/// Where the token a rule's match starts ends. A rule that names a token by its length stops at
/// that length, so for a longer token of the same family (GitLab's current tokens, the dotted
/// segments of a routable one) replacing the match alone publishes the rest of the token in plain
/// text. The span therefore runs on over the characters tokens are written with, and over a dot
/// that another such character follows; a match that ends at a boundary keeps its end.
fn token_run_end(view: &str, end: usize) -> usize {
    let bytes = view.as_bytes();
    let token = |at: usize| {
        bytes
            .get(at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    let mut at = end;
    loop {
        if token(at) {
            at += 1;
        } else if bytes.get(at) == Some(&b'.') && token(at + 1) {
            at += 2;
        } else {
            return at;
        }
    }
}

/// A credential-named field or variable supplies the evidence that a value's characters cannot:
/// assigned values are findings even with the punctuation passwords carry, which the vendored
/// generic rule's value class excludes. A quoted value is taken whole, up to its closing quote or,
/// in text cut short, to where the text ends. Names that describe where
/// or how a credential is used (endpoints, paths, types) and values that refer to other code,
/// configuration or locations are not credentials, and neither is an unquoted value that reads as
/// code: a path (`secret_filter::Session`), a reference (`token: &State`) or an identifier.
static CREDENTIAL_ASSIGNMENT: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(
    || {
        regex::Regex::new(
            r#"(?i)([A-Za-z0-9_.-]{0,40}(?:password|passwd|passphrase|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credentials?)[A-Za-z0-9_.-]{0,24})["']?[ \t]{0,3}(?::=|=>|=|:)[ \t]{0,3}(?:"([^"\s]{8,256})"?|'([^'\s]{8,256})'?|`([^`\s]{8,256})`?|([^\s"'`;,<>(){}\[\]\\]{10,256}))"#,
        )
        .ok()
    },
);

const NON_CREDENTIAL_NAMES: [&str; 28] = [
    "endpoint",
    "url",
    "uri",
    "path",
    "file",
    "dir",
    "type",
    "name",
    "id",
    "ids",
    "count",
    "length",
    "limit",
    "ttl",
    "expiry",
    "expires",
    "expires_in",
    "header",
    "prefix",
    "format",
    "size",
    "kind",
    "mode",
    "env",
    "var",
    "label",
    "scope",
    "method",
];

/// File extensions: a name ending in one is a file, as in search output (`secrets.md:42:…`).
const FILE_EXTENSIONS: [&str; 24] = [
    "md", "mdx", "txt", "log", "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go", "java", "kt",
    "swift", "rb", "php", "json", "jsonl", "yaml", "yml", "toml", "sh", "sql",
];

/// Things whose `token` names a document, page or cursor rather than a credential, such as
/// `file_token`, `node_token` or `nextPageToken`.
const RESOURCE_TOKEN_OWNERS: [&str; 13] = [
    "file",
    "node",
    "obj",
    "doc",
    "docs",
    "base",
    "wiki",
    "sheet",
    "folder",
    "page",
    "cursor",
    "continuation",
    "sync",
];

pub(crate) fn credential_assignment_spans(text: &str) -> Vec<(usize, usize)> {
    let Some(pattern) = CREDENTIAL_ASSIGNMENT.as_ref() else {
        return Vec::new();
    };
    pattern
        .captures_iter(text)
        .filter(|captures| {
            captures
                .get(1)
                .is_some_and(|name| credential_name(name.as_str()))
        })
        .filter_map(|captures| {
            let name = captures.get(1)?;
            (2..=5).find_map(|group| {
                captures.get(group).map(|value| {
                    let shape = if group < 5 {
                        Shape::Quoted
                    } else {
                        Shape::Unquoted {
                            in_code: code_context(text, name, value),
                        }
                    };
                    (value, shape)
                })
            })
        })
        .filter(|(value, shape)| assigned_credential(value.as_str(), *shape))
        .map(|(value, _)| (value.start(), value.end()))
        .collect()
}

/// A field named for a credential whose whole value has the shape of one.
#[cfg(feature = "secret-vault")]
pub(crate) fn credential_field_value(field: &str, value: &str) -> bool {
    let probe = format!("{field} = 0123456789aB");
    let named = field.len() <= 64
        && CREDENTIAL_ASSIGNMENT
            .as_ref()
            .and_then(|pattern| pattern.captures(&probe))
            .and_then(|captures| captures.get(1))
            .is_some_and(|name| name.as_str() == field && credential_name(field));
    named
        && (8..=256).contains(&value.len())
        && !value.contains(['\n', '\r'])
        && assigned_credential(value, Shape::Quoted)
}

fn credential_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let last = name.rsplit(['_', '-', '.']).next().unwrap_or_default();
    let compact: String = name.chars().filter(|c| !"_-.".contains(*c)).collect();
    let file = name.contains('.') && FILE_EXTENSIONS.contains(&last);
    !file
        && !NON_CREDENTIAL_NAMES.contains(&last)
        && !NON_CREDENTIAL_NAMES
            .iter()
            .any(|suffix| suffix.len() > 3 && name.ends_with(suffix))
        && !compact.strip_suffix("token").is_some_and(|owner| {
            RESOURCE_TOKEN_OWNERS
                .iter()
                .any(|resource| owner.ends_with(resource))
        })
}

/// How an assigned value is written.
#[derive(Clone, Copy)]
enum Shape {
    Quoted,
    /// `in_code`: the assignment is a program declaration, so an identifier there names code
    /// rather than holding a value.
    Unquoted {
        in_code: bool,
    },
}

/// Keywords of program declarations, possibly with `mut` between keyword and name. Shell
/// `local` and `readonly` are absent: they declare variables whose values are literal data.
const DECLARATIONS: [&str; 7] = ["const", "let", "var", "val", "final", "mut", "static"];

/// Whether an unquoted assignment is a program declaration (`const token = …`,
/// `let mut secret = …`): a declaration keyword right before the name and an `=` with
/// whitespace on both sides. Shell, `.env`, INI and YAML assignments, including shell
/// `local NAME=…` and YAML flow mappings, are not, whatever follows the value.
fn code_context(text: &str, name: regex::Match<'_>, value: regex::Match<'_>) -> bool {
    let line = text[..name.start()]
        .rsplit(['\n', '\r'])
        .next()
        .unwrap_or_default();
    let declared = line
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .rfind(|word| !word.is_empty())
        .is_some_and(|word| line.trim_end().ends_with(word) && DECLARATIONS.contains(&word));
    let between = &text[name.end()..value.start()];
    let spaced = between.starts_with([' ', '\t'])
        && between.trim_start().starts_with('=')
        && !between.trim_start().starts_with("=>")
        && between.trim_start()[1..].starts_with([' ', '\t']);
    declared && spaced
}

/// Whether an assigned value has a credential's shape. An unquoted path or reference
/// (`secret_filter::Session`, `token: &State`) is code; so is a digit-free camel-case identifier
/// in a program declaration. Elsewhere a camel-shaped value is still a credential, since random
/// and passphrase passwords take that shape too.
fn assigned_credential(value: &str, shape: Shape) -> bool {
    let code = match shape {
        Shape::Quoted => false,
        Shape::Unquoted { in_code } => {
            value.starts_with([':', '&', '*'])
                || (in_code
                    && value.bytes().all(|byte| byte.is_ascii_alphabetic())
                    && value.bytes().skip(1).any(|byte| byte.is_ascii_uppercase())
                    && camel_identifier(value))
        }
    };
    let identifier = |part: &str| {
        part.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    let environment = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))
        .or_else(|| {
            value
                .strip_prefix('%')
                .and_then(|rest| rest.strip_suffix('%'))
        })
        .is_some_and(identifier);
    let path = ["/", "~/", "./", "../"]
        .iter()
        .any(|root| value.starts_with(root))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-~".contains(&byte));
    let location = value.contains("://") || path;
    let reference = environment
        || location
        || (value.contains('.') && value.split('.').all(identifier))
        || (value.contains('_')
            && value.split('_').all(|word| {
                !word.is_empty()
                    && (word
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                        || word.bytes().all(|byte| byte.is_ascii_lowercase()))
            }));
    let mixed_alpha = value.bytes().any(|byte| byte.is_ascii_uppercase())
        && value.bytes().any(|byte| byte.is_ascii_lowercase());
    let has_digit = value.bytes().any(|byte| byte.is_ascii_digit());
    !code && !reference && (mixed_alpha || has_digit) && rules::shannon(value) >= 3.5
}

/// Words that each start with one capital and continue in lowercase letters or digits, after an
/// optional lowercase head: `SnapshotToken`, `accessToken`.
fn camel_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && !bytes[index].is_ascii_uppercase() {
        if !bytes[index].is_ascii_lowercase() {
            return false;
        }
        index += 1;
    }
    while index < bytes.len() {
        if !bytes[index].is_ascii_uppercase() {
            return false;
        }
        index += 1;
        let word = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
        {
            index += 1;
        }
        if index - word < 2 {
            return false;
        }
    }
    true
}

pub(crate) fn bare_candidate_spans(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    entropy_candidate_spans(text, false)
}

pub(crate) fn entropy_candidate_spans(
    text: &str,
    credential_field: bool,
) -> impl Iterator<Item = (usize, usize)> + '_ {
    let bytes = text.as_bytes();
    let locations = if credential_field {
        Vec::new()
    } else {
        paths::regions(text)
    };
    let mut location = 0;
    let mut start = 0;
    std::iter::from_fn(move || {
        while start < bytes.len() {
            while location < locations.len() && locations[location].1 <= start {
                location += 1;
            }
            if let Some(&(left, right)) = locations.get(location)
                && left <= start
            {
                start = right;
                continue;
            }
            if !is_token_byte(bytes[start]) {
                start += 1;
                continue;
            }
            let mut end = start + 1;
            while end < bytes.len()
                && is_token_byte(bytes[end])
                && locations.get(location).is_none_or(|(left, _)| end < *left)
            {
                end += 1;
            }
            if !credential_field && end - start > MAX_BARE_ENTROPY_BYTES {
                start = end;
                continue;
            }
            let candidate = &text[start..end];
            let hex_candidate = ["agit-", "sha1-", "sha256-"]
                .iter()
                .find_map(|prefix| candidate.strip_prefix(prefix))
                .unwrap_or(candidate);
            let is_hex = hex_candidate.bytes().any(|byte| byte.is_ascii_hexdigit())
                && hex_candidate
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
            let is_alpha = candidate.bytes().all(|byte| byte.is_ascii_alphabetic());
            let has_separator = candidate.bytes().any(|byte| !byte.is_ascii_alphanumeric());
            let min_len = if credential_field {
                10
            } else if is_hex {
                32
            } else if is_alpha {
                24
            } else {
                20
            };
            let floor = if credential_field {
                3.5
            } else if is_hex {
                3.2
            } else if candidate.len() > 24 {
                if has_separator { 4.45 } else { 4.3 }
            } else if is_alpha {
                3.8
            } else if has_separator {
                4.2
            } else {
                4.0
            };
            let mixed_alpha = candidate.bytes().any(|byte| byte.is_ascii_uppercase())
                && candidate.bytes().any(|byte| byte.is_ascii_lowercase());
            let has_digit = candidate.bytes().any(|byte| byte.is_ascii_digit());
            if candidate.len() >= min_len
                && rules::shannon(candidate) >= floor
                && (mixed_alpha || has_digit || is_hex)
                && !base64_media_payload(text, start)
            {
                let span = (start, end);
                start = end;
                return Some(span);
            }
            start = end;
        }
        None
    })
}

pub(crate) fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_-+/=.!@#$%^&*?~".contains(&byte)
}

pub(crate) fn private_key_region_end(text: &str, start: usize, header_end: usize) -> usize {
    // Letter case does not change byte offsets, so the footer is found in a lower-cased copy.
    let footer = text[start..header_end]
        .to_ascii_lowercase()
        .replacen("-----begin", "-----end", 1);
    text[header_end..]
        .to_ascii_lowercase()
        .find(&footer)
        .map_or(text.len(), |offset| header_end + offset + footer.len())
}

pub(crate) fn json_string_regions(text: &str) -> Vec<(usize, usize)> {
    if serde_json::from_str::<serde_json::Value>(text).is_err() {
        return Vec::new();
    }
    let mut strings = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find('"') {
        let start = cursor + relative;
        let Some(end) = json_string_end(text.as_bytes(), start) else {
            return Vec::new();
        };
        strings.push((start + 1, end - 1));
        cursor = end;
    }
    strings
}

pub(crate) fn semantic_match_value<'a>(
    text: &'a str,
    strings: &[(usize, usize)],
    start: usize,
    end: usize,
) -> Option<std::borrow::Cow<'a, str>> {
    let raw = text.get(start..end)?;
    let index = strings.partition_point(|(s, _)| *s <= start);
    let Some(&(string_start, string_end)) = index.checked_sub(1).and_then(|i| strings.get(i))
    else {
        return Some(raw.into());
    };
    if end > string_end {
        return Some(raw.into());
    }
    let boundary = |offset: usize| {
        let bytes = text.as_bytes();
        for escape in offset.saturating_sub(5).max(string_start)..offset {
            if bytes[escape] != b'\\' {
                continue;
            }
            let mut preceding = escape;
            while preceding > string_start && bytes[preceding - 1] == b'\\' {
                preceding -= 1;
            }
            if (escape - preceding) % 2 != 0 {
                continue;
            }
            let width = if bytes.get(escape + 1) == Some(&b'u') {
                6
            } else {
                2
            };
            if escape + width > offset {
                return false;
            }
        }
        true
    };
    if !boundary(start) || !boundary(end) {
        return None;
    }
    if !raw.contains('\\') {
        return Some(raw.into());
    }
    serde_json::from_str::<String>(&format!("\"{raw}\""))
        .ok()
        .map(Into::into)
}

pub(crate) fn field_value_regions(
    text: &str,
    matches_field: fn(&str) -> bool,
) -> Vec<(usize, usize)> {
    use serde_json::value::RawValue;

    pub(crate) fn collect(
        raw: &RawValue,
        base: usize,
        selected: bool,
        matches_field: fn(&str) -> bool,
        out: &mut Vec<(usize, usize)>,
    ) {
        match raw.get().as_bytes().first() {
            Some(b'"') if selected => {
                let start = raw.get().as_ptr() as usize - base;
                out.push((start + 1, start + raw.get().len() - 1));
            }
            Some(b'{') => {
                if let Ok(map) = serde_json::from_str::<HashMap<String, &RawValue>>(raw.get()) {
                    for (key, value) in map {
                        collect(value, base, matches_field(&key), matches_field, out);
                    }
                }
            }
            Some(b'[') => {
                if let Ok(values) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) {
                    for value in values {
                        collect(value, base, selected, matches_field, out);
                    }
                }
            }
            _ => {}
        }
    }

    let mut regions = Vec::new();
    if let Ok(raw) = serde_json::from_str::<&RawValue>(text) {
        collect(
            raw,
            text.as_ptr() as usize,
            false,
            matches_field,
            &mut regions,
        );
    }
    regions.sort_unstable();
    regions
}

const GENERIC_RULE: &str = "generic-api-key";

pub(crate) fn dedupe_same_span(hits: &mut Vec<Span>) {
    const CATCH_ALL: &str = GENERIC_RULE;
    let mut i = 0;
    while i < hits.len() {
        let mut j = i + 1;
        while j < hits.len() && hits[j].start == hits[i].start && hits[j].end == hits[i].end {
            j += 1;
        }
        if j - i > 1 {
            let keep = (i..j).find(|&k| hits[k].rule != CATCH_ALL).unwrap_or(i);
            hits.swap(i, keep);
            hits.drain(i + 1..j);
        }
        i += 1;
    }
}

/// Incomplete detection still returns every finding produced within the budget.
#[derive(Debug)]
pub struct Batch {
    pub findings: Vec<Span>,
    pub complete: bool,
}

/// Every finding, including bare entropy, for explicit diagnostics.
pub fn scan(text: &str, limit: usize) -> Batch {
    scan_filtered(text, limit, Entropy::Include, |_, _| true)
}

/// Provider signatures, private keys and credential assignments: the findings that are
/// replaced automatically and that the Hub re-checks after publication.
pub fn scan_high_confidence(text: &str, limit: usize) -> Batch {
    scan_filtered(text, limit, Entropy::Exclude, |_, _| true)
}

pub(crate) fn scan_filtered(
    text: &str,
    limit: usize,
    entropy: Entropy,
    keep: impl FnMut(&str, usize) -> bool,
) -> Batch {
    let (findings, incomplete) = raw_hits_capped(text, limit, entropy, keep);
    Batch {
        findings,
        complete: !incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_detection_keeps_exact_spans_and_does_not_claim_completion() {
        let input = "key AKIA2E7YQXK4NMZ5VJ3T and ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let batch = scan(input, 1);
        assert!(!batch.complete);
        assert_eq!(batch.findings.len(), 1);
        assert_eq!(
            &input[batch.findings[0].start..batch.findings[0].end],
            "AKIA2E7YQXK4NMZ5VJ3T"
        );
        let batch = scan_filtered(input, 32, Entropy::Include, |value, _| {
            assert!(!value.starts_with("ghp_"), "synthetic rule failure");
            true
        });
        assert!(!batch.complete);
        assert!(
            batch
                .findings
                .iter()
                .any(|hit| hit.rule == "aws-access-token")
        );
    }
}
