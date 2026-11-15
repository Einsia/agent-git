//! The author's home directory, written as `~` in what a push publishes.
//!
//! The home is an argument. This module never reads the process environment, so a caller that
//! re-runs a record rule on the author's behalf cannot substitute its own home for the author's.
//! The local commits and the transcripts resume installs keep the original text; only the
//! published copy changes.
//!
//! Not covered, because each needs a rule of its own:
//! - Windows 8.3 short names (`C:\Users\ABCDEF~1`);
//! - percent-encoded paths (`%2FUsers%2F<name>`);
//! - WSL's view of the Windows profile (`/mnt/c/Users/<name>`) when agit runs on Windows;
//! - a path a runtime encodes into a name, such as Claude Code's
//!   `~/.claude/projects/-Users-<name>-Code-x`, which keeps its encoded form;
//! - member names of JSON objects, which are not text a reader takes for a path.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// The spellings of one home directory and what each is replaced by. Empty when the home is
/// missing, a root, or not absolute, in which case nothing is replaced.
#[derive(Clone, Default)]
pub(super) struct Homes {
    /// What each spelling is replaced by, in the order the finder was built from them.
    replacements: Vec<String>,
    /// Finds every spelling in one walk over the text.
    finder: Option<AhoCorasick>,
}

impl Homes {
    /// `home` as the push sees it, and its canonical form when that differs.
    pub(super) fn of(home: Option<&std::path::Path>) -> Self {
        let Some(home) = home else {
            return Self::default();
        };
        let given = trim_separators(&home.to_string_lossy()).to_owned();
        if !usable(&given) {
            return Self::default();
        }
        let mut bases = vec![given.clone()];
        if let Ok(canonical) = home.canonicalize() {
            let canonical = crate::infra::git_runtime::path_for_git(canonical);
            let canonical = trim_separators(&canonical.to_string_lossy()).to_owned();
            if usable(&canonical) && canonical != given {
                bases.push(canonical);
            }
        }
        // Windows and macOS keep ASCII case-insensitive names on their usual file systems.
        Self::from_bases(&bases, cfg!(any(windows, target_os = "macos")))
    }

    fn from_bases(bases: &[String], case_insensitive: bool) -> Self {
        let mut spellings: Vec<(String, String)> = Vec::new();
        for base in bases {
            spellings.push((base.clone(), "~".into()));
            if !base.starts_with('/') || drive_form(base).is_some() {
                // `file://` followed by a rooted path names the same directory.
                if let Some(rooted) = base.strip_prefix('/') {
                    spellings.push((format!("file:///{rooted}"), "file://~".into()));
                }
            } else {
                spellings.push((format!("file://{base}"), "file://~".into()));
            }
            if let Some((drive, parts)) = drive_form(base) {
                let drive_upper = drive.to_ascii_uppercase();
                let drive_lower = drive.to_ascii_lowercase();
                let joined = |separator: &str| parts.join(separator);
                spellings.push((format!("{drive_upper}:\\{}", joined("\\")), "~".into()));
                spellings.push((format!("{drive_upper}:/{}", joined("/")), "~".into()));
                spellings.push((format!("{drive_upper}:\\\\{}", joined("\\\\")), "~".into()));
                spellings.push((format!("/{drive_lower}/{}", joined("/")), "~".into()));
                spellings.push((
                    format!("file:///{drive_upper}:/{}", joined("/")),
                    "file://~".into(),
                ));
            }
            if let Some(parts) = unc_form(base) {
                let joined = |separator: &str| parts.join(separator);
                spellings.push((format!("\\\\{}", joined("\\")), "~".into()));
                spellings.push((format!("//{}", joined("/")), "~".into()));
                spellings.push((format!("\\\\\\\\{}", joined("\\\\")), "~".into()));
                spellings.push((format!("file://{}", joined("/")), "file://~".into()));
            }
        }
        spellings.sort();
        spellings.dedup();
        let finder = AhoCorasickBuilder::new()
            .ascii_case_insensitive(case_insensitive)
            .match_kind(MatchKind::LeftmostLongest)
            .build(spellings.iter().map(|(text, _)| text.as_str()))
            .ok();
        if finder.is_none() {
            return Self::default();
        }
        Self {
            replacements: spellings.into_iter().map(|(_, to)| to).collect(),
            finder,
        }
    }

    /// `text` with the home written as `~` wherever it is a whole path prefix.
    pub(super) fn in_text(&self, text: &str) -> String {
        let Some(finder) = &self.finder else {
            return text.to_owned();
        };
        let mut output = String::with_capacity(text.len());
        let mut consumed = 0;
        for found in finder.find_iter(text) {
            if !boundary_before(text, found.start()) || !boundary_after(text, found.end()) {
                continue;
            }
            output.push_str(&text[consumed..found.start()]);
            output.push_str(&self.replacements[found.pattern().as_usize()]);
            consumed = found.end();
        }
        output.push_str(&text[consumed..]);
        output
    }

    /// `value` with the home written as `~` in every string, except the identity and linkage
    /// fields and the opaque carriers that protection also leaves untouched: a carrier can spell
    /// the home by chance, and changing one byte of it breaks resume. Those fields are the ones
    /// [`crate::adapter::protocol_ids::layout`] names, so the two never drift apart. Whether
    /// anything changed is returned.
    pub(super) fn in_record(&self, value: &mut Value) -> bool {
        if self.finder.is_none() {
            return false;
        }
        let layout = crate::adapter::protocol_ids::layout(value);
        let skipped: HashSet<&str> = layout
            .fields
            .iter()
            .map(|field| field.pointer.as_str())
            .chain(layout.opaque.iter().map(String::as_str))
            .collect();
        let encoded: HashMap<&str, &crate::adapter::protocol_ids::Layout> = layout
            .encoded
            .iter()
            .map(|(pointer, nested)| (pointer.as_str(), nested))
            .collect();
        self.walk(value, &mut String::new(), &skipped, &encoded)
    }

    /// A published `session/meta.json` with the home written as `~` in the fields that hold a
    /// path or the person's words: the session's directory, its code state and that state's
    /// origin, its title and its milestone. Every other field is an identity, a count, a digest
    /// or a choice from a fixed set, and a changed byte there would name a different session.
    /// Whether anything changed is returned.
    pub(super) fn in_meta(&self, value: &mut Value) -> bool {
        if self.finder.is_none() {
            return false;
        }
        let mut changed = false;
        for pointer in META_FIELDS {
            if let Some(Value::String(text)) = value.pointer_mut(pointer) {
                let rewritten = self.in_text(text);
                if rewritten != *text {
                    *text = rewritten;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Whether [`Homes::in_meta`] would change `metadata`.
    pub(super) fn found_in_meta(&self, metadata: &crate::domain::meta::Meta) -> bool {
        if self.finder.is_none() {
            return false;
        }
        let origin = metadata
            .cwd_state
            .as_ref()
            .and_then(|state| state.origin.as_deref());
        [
            Some(metadata.cwd.as_str()),
            metadata.code.as_deref(),
            origin,
            metadata.title.as_deref(),
            metadata.milestone.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| self.in_text(text) != text)
    }

    fn walk(
        &self,
        value: &mut Value,
        pointer: &mut String,
        skipped: &HashSet<&str>,
        encoded: &HashMap<&str, &crate::adapter::protocol_ids::Layout>,
    ) -> bool {
        if skipped.contains(pointer.as_str()) {
            return false;
        }
        match value {
            Value::String(text) => {
                // Encoded JSON is decoded first, so the home is matched in the text a reader
                // sees and a nested identity field is still left alone.
                if let Some(nested) = encoded.get(pointer.as_str())
                    && let Ok(mut decoded) = serde_json::from_str::<Value>(text)
                {
                    let mut changed = false;
                    let nested_layout = *nested;
                    let skipped_nested: HashSet<&str> = nested_layout
                        .fields
                        .iter()
                        .map(|field| field.pointer.as_str())
                        .chain(nested_layout.opaque.iter().map(String::as_str))
                        .collect();
                    changed |= self.walk(
                        &mut decoded,
                        &mut String::new(),
                        &skipped_nested,
                        &HashMap::new(),
                    );
                    if changed {
                        *text = decoded.to_string();
                    }
                    return changed;
                }
                let rewritten = self.in_text(text);
                if rewritten == *text {
                    return false;
                }
                *text = rewritten;
                true
            }
            Value::Array(items) => {
                let mut changed = false;
                for (index, item) in items.iter_mut().enumerate() {
                    let length = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&index.to_string());
                    changed |= self.walk(item, pointer, skipped, encoded);
                    pointer.truncate(length);
                }
                changed
            }
            Value::Object(fields) => {
                let mut changed = false;
                for (key, field) in fields.iter_mut() {
                    let length = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    changed |= self.walk(field, pointer, skipped, encoded);
                    pointer.truncate(length);
                }
                changed
            }
            _ => false,
        }
    }
}

/// The `session/meta.json` fields [`Homes::in_meta`] rewrites. `code_state` is the older name of
/// `cwd_state` that published metadata can still carry.
const META_FIELDS: &[&str] = &[
    "/cwd",
    "/code",
    "/cwd_state/origin",
    "/code_state/origin",
    "/title",
    "/milestone",
];

/// The drive letter and path components of a Windows path written `X:\a\b` or `X:/a/b`.
fn drive_form(path: &str) -> Option<(char, Vec<&str>)> {
    let bytes = path.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }
    let parts: Vec<&str> = path[3..]
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then_some((bytes[0] as char, parts))
}

/// A path prefix starts the string or follows a character that cannot end a path component.
/// Any other character before it (`/data/Users/<name>`) makes it part of a different path.
fn boundary_before(text: &str, at: usize) -> bool {
    at == 0
        || text.as_bytes()[at - 1].is_ascii_whitespace()
        || b"\"'`=([{<,;:@|>".contains(&text.as_bytes()[at - 1])
}

/// A path prefix ends the string, or is followed by a separator or a character that cannot
/// continue a directory name. Letters, digits, `_` and `-` can, and so can a `.` followed by one
/// of those: `/Users/<name>.bak` is another directory, while a sentence ending in
/// `/Users/<name>.` names the home.
fn boundary_after(text: &str, end: usize) -> bool {
    let bytes = text.as_bytes();
    let Some(&after) = bytes.get(end) else {
        return true;
    };
    if after == b'.' {
        return bytes.get(end + 1).is_none_or(|next| !continues_name(*next));
    }
    !continues_name(after)
}

/// A byte that can continue a directory name: letters of any script, digits, `_` and `-`.
fn continues_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte >= 0x80
}

/// The server, share and folders of a Windows network path written `\\server\share\…` or
/// `//server/share/…`; a network home is as absolute as one on a drive.
fn unc_form(path: &str) -> Option<Vec<&str>> {
    let rest = path
        .strip_prefix("\\\\")
        .or_else(|| path.strip_prefix("//"))
        .filter(|rest| !rest.starts_with(['\\', '/']))?;
    let parts: Vec<&str> = rest
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    (parts.len() >= 2).then_some(parts)
}

fn usable(path: &str) -> bool {
    if path.is_empty() || path == "/" {
        return false;
    }
    drive_form(path).is_some() || unc_form(path).is_some() || path.starts_with('/')
}

fn trim_separators(path: &str) -> &str {
    let mut end = path.len();
    while end > 1 && matches!(path.as_bytes()[end - 1], b'/' | b'\\') {
        end -= 1;
    }
    &path[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn homes(bases: &[&str], case_insensitive: bool) -> Homes {
        let bases: Vec<String> = bases.iter().map(|base| (*base).to_owned()).collect();
        Homes::from_bases(&bases, case_insensitive)
    }

    /// The home is replaced only as a whole path prefix, in every spelling tool output shows, and
    /// nowhere else: a longer directory name, a directory that merely contains the same parts, a
    /// member name, an identity field, an opaque carrier and a metadata field that holds an object
    /// id all stay as they are. A wrong implementation that replaced every occurrence of the user
    /// name, or only one spelling, or walked into a carrier, fails one row.
    #[test]
    fn the_home_is_replaced_only_as_a_whole_path_prefix() {
        let name = "canaryname";
        let home = format!("/Users/{name}");
        let posix = homes(&[&home, &format!("/private/var/{name}")], true);
        let exact = homes(&[&home], false);
        let windows = homes(&[&format!("C:\\Users\\{name}")], true);
        // A network home, as `HOME` or `USERPROFILE` gives it, is a home like any other.
        let network = Homes::of(Some(std::path::Path::new(&format!(
            "\\\\server\\profiles\\{name}\\"
        ))));
        let rows: Vec<(&Homes, String, String)> = vec![
            (&posix, format!("{home}/src/a.rs"), "~/src/a.rs".into()),
            (&posix, home.clone(), "~".into()),
            (&posix, format!("see {home}."), "see ~.".into()),
            (&posix, format!("{home}2"), format!("{home}2")),
            (&posix, format!("{home}-old"), format!("{home}-old")),
            (&posix, format!("{home}.bak"), format!("{home}.bak")),
            (&posix, format!("{home}\u{5f20}"), format!("{home}\u{5f20}")),
            (
                &posix,
                format!("/data/Users/{name}/x"),
                format!("/data/Users/{name}/x"),
            ),
            (&posix, format!("{name} ran"), format!("{name} ran")),
            (&posix, format!("/private/var/{name}/x"), "~/x".into()),
            (&posix, format!("/users/{name}/x"), "~/x".into()),
            (&posix, format!("file://{home}/x"), "file://~/x".into()),
            (
                &posix,
                format!("cd \"{home}/a\" && ls {home}/b"),
                "cd \"~/a\" && ls ~/b".into(),
            ),
            (&posix, format!("--root={home}/a"), "--root=~/a".into()),
            (
                &exact,
                format!("/users/{name}/x"),
                format!("/users/{name}/x"),
            ),
            (&windows, format!("C:\\Users\\{name}\\src"), "~\\src".into()),
            (&windows, format!("C:/Users/{name}/src"), "~/src".into()),
            (
                &windows,
                format!("C:\\\\Users\\\\{name}\\\\src"),
                "~\\\\src".into(),
            ),
            (&windows, format!("/c/Users/{name}/src"), "~/src".into()),
            (&windows, format!("c:\\users\\{name}\\src"), "~\\src".into()),
            (
                &windows,
                format!("file:///C:/Users/{name}/src"),
                "file://~/src".into(),
            ),
            (
                &windows,
                format!("C:\\Users\\{name}2\\src"),
                format!("C:\\Users\\{name}2\\src"),
            ),
            (
                &network,
                format!("\\\\server\\profiles\\{name}\\src"),
                "~\\src".into(),
            ),
            (
                &network,
                format!("//server/profiles/{name}/src"),
                "~/src".into(),
            ),
            (
                &network,
                format!("\\\\\\\\server\\\\profiles\\\\{name}\\\\src"),
                "~\\\\src".into(),
            ),
            (
                &network,
                format!("\\\\server\\profiles\\{name}2\\src"),
                format!("\\\\server\\profiles\\{name}2\\src"),
            ),
        ];
        for (homes, input, expected) in &rows {
            assert_eq!(&homes.in_text(input), expected, "{input}");
        }

        let mut record = serde_json::json!({
            "type": "assistant",
            "uuid": format!("{home}/uuid"),
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": format!("edited {home}/a.rs")},
                    {"type": "thinking", "thinking": format!("{home}/t"), "signature": format!("{home}/sig")},
                    {"type": "tool_use", "id": format!("toolu_{name}"), "name": "Read",
                     "input": {"file_path": format!("{home}/a.rs"), format!("{home}/key"): 1}}
                ]
            }
        });
        assert!(posix.in_record(&mut record));
        let text = record.to_string();
        assert!(text.contains("edited ~/a.rs") && text.contains("\"file_path\":\"~/a.rs\""));
        assert!(text.contains("~/t"), "{text}");
        for kept in [
            format!("\"signature\":\"{home}/sig\""),
            format!("\"uuid\":\"{home}/uuid\""),
            format!("\"{home}/key\""),
        ] {
            assert!(text.contains(&kept), "{kept} stays as it was: {text}");
        }
        assert!(!posix.in_record(
            &mut serde_json::json!({"type":"user","message":{"role":"user","content":"hi"}})
        ));

        let mut meta = serde_json::json!({
            "cwd": format!("{home}/proj"),
            "code": format!("git@{home}/proj@abc1234"),
            "cwd_state": {"origin": format!("file://{home}/proj"), "head": format!("{home}/head"), "staged": 1},
            "title": format!("work in {home}"),
            "milestone": format!("{home}/done"),
            "session": format!("agit-{}", "a".repeat(40)),
        });
        assert!(posix.in_meta(&mut meta));
        for (field, expected) in [
            ("/cwd", "~/proj".to_owned()),
            ("/code", "git@~/proj@abc1234".to_owned()),
            ("/cwd_state/origin", "file://~/proj".to_owned()),
            ("/title", "work in ~".to_owned()),
            ("/milestone", "~/done".to_owned()),
            ("/cwd_state/head", format!("{home}/head")),
        ] {
            assert_eq!(meta.pointer(field).unwrap(), &expected, "{field}");
        }
    }
}
