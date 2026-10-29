//! Opaque secret placeholders shared by discovery, projection and publication.

use regex::Regex;
use std::sync::LazyLock;

pub(crate) const TOKEN_PREFIX: &str = "{{AGIT_SECRET_V1:";
pub(crate) const TOKEN_SUFFIX: &str = "}}";
pub(crate) const CANONICAL_TOKEN_LEN: usize =
    TOKEN_PREFIX.len() + 36 + 1 + 4 + 32 + TOKEN_SUFFIX.len();
const V2_PREFIX: &str = "{{AGIT_SECRET_V2:";
const MAX_TOKEN_LEN: usize = CANONICAL_TOKEN_LEN + 2;
#[cfg(feature = "secret-vault")]
const MAX_ESCAPED_TOKEN_LEN: usize = MAX_TOKEN_LEN * 6;

static CANONICAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A\{\{AGIT_SECRET_V1:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}:sec_[0-9A-Fa-f]{32}\}\}\z")
        .expect("canonical placeholder shape")
});

pub(crate) fn token_segments(text: &str) -> TokenSegments<'_> {
    TokenSegments {
        text,
        cursor: 0,
        legacy: None,
        typed: None,
    }
}

/// Typed placeholders spell `‹CLASS:KIND:ID›`. The class and kind say what was removed, never
/// anything about the value; the ID names a record in the owner's dictionary.
pub const TYPED_OPEN: char = '\u{2039}';
pub const TYPED_CLOSE: char = '\u{203A}';
const TYPED_OPEN_ESCAPED: &str = "\\u2039";
const TYPED_CLOSE_ESCAPED: &str = "\\u203a";
pub const TYPED_CLASSES: [&str; 6] = ["secret", "path", "user", "host", "omitted", "custom"];
const TYPED_ID_LEN: usize = 12;
const TYPED_ID_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const MAX_TYPED_KIND_LEN: usize = 32;
/// The longest body a valid typed token can have, which bounds every scan for a closing mark.
const MAX_TYPED_BODY_LEN: usize = 7 + 1 + MAX_TYPED_KIND_LEN + 1 + TYPED_ID_LEN;

/// A new typed token with a random ID. The ID is not derived from the value, so equal tokens
/// in two places say only that one dictionary replaced the same value there.
pub fn typed_token(class: &str, kind: &str) -> Option<String> {
    if !TYPED_CLASSES.contains(&class) || !valid_kind(kind) {
        return None;
    }
    let bits: u64 = rand::random();
    let id: String = (0..TYPED_ID_LEN)
        .map(|index| char::from(TYPED_ID_ALPHABET[((bits >> (index * 5)) & 31) as usize]))
        .collect();
    Some(format!("{TYPED_OPEN}{class}:{kind}:{id}{TYPED_CLOSE}"))
}

/// `(class, kind, id)` of a complete typed token, or `None` for any other text.
pub fn typed_parts(token: &str) -> Option<(&str, &str, &str)> {
    let body = token.strip_prefix(TYPED_OPEN)?.strip_suffix(TYPED_CLOSE)?;
    let mut parts = body.splitn(3, ':');
    let (class, kind, id) = (parts.next()?, parts.next()?, parts.next()?);
    (TYPED_CLASSES.contains(&class)
        && valid_kind(kind)
        && id.len() == TYPED_ID_LEN
        && id.bytes().all(|byte| TYPED_ID_ALPHABET.contains(&byte)))
    .then_some((class, kind, id))
}

fn valid_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= MAX_TYPED_KIND_LEN
        && kind.as_bytes()[0].is_ascii_alphanumeric()
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The kind a detector rule contributes to a secret placeholder.
pub fn secret_kind(rule: &str) -> &str {
    match rule {
        "explicit-block" => "blocked",
        "high-entropy-value" => "high-entropy",
        "generic-api-key" | "credential-assignment" => "credential",
        "private-key" | "agit-private-key-header" => "private-key",
        "agit-credentials-in-url" => "url-password",
        "agit-token" => "agentgit-token",
        rule if valid_kind(rule) => rule,
        _ => "credential",
    }
}

/// A cheap prefilter: false means text holds no placeholder in any spelling.
pub fn may_contain_token(text: &str) -> bool {
    text.contains("AGIT_SECRET_V") || text.contains(TYPED_OPEN) || text.contains("\\u2039")
}

/// Whether text holds any recognized placeholder spelling, typed or legacy.
pub fn contains_token(text: &str) -> bool {
    may_contain_token(text) && token_segments(text).next().is_some()
}

/// The next typed token in raw text, accepting JSON-escaped delimiters around an ASCII body.
/// One forward pass visits each byte once: only the lead byte of `‹` and a backslash can open a
/// token, and each candidate is checked within the bounded token length.
fn next_typed(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let lead = TYPED_OPEN.encode_utf8(&mut [0; 4]).as_bytes()[0];
    let bytes = text.as_bytes();
    let mut at = cursor;
    while let Some(offset) = bytes[at..]
        .iter()
        .position(|&byte| byte == lead || byte == b'\\')
    {
        let start = at + offset;
        at = start + 1;
        let rest = &text[start..];
        let open_len = if rest.starts_with(TYPED_OPEN) {
            TYPED_OPEN.len_utf8()
        } else if rest
            .get(..TYPED_OPEN_ESCAPED.len())
            .is_some_and(|open| open.eq_ignore_ascii_case(TYPED_OPEN_ESCAPED))
        {
            TYPED_OPEN_ESCAPED.len()
        } else {
            continue;
        };
        let body_start = start + open_len;
        let tail = &text[body_start..];
        let body_len = tail
            .bytes()
            .take(MAX_TYPED_BODY_LEN + 1)
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-'))
            .count();
        let after = &tail[body_len..];
        let close_len = if after.starts_with(TYPED_CLOSE) {
            Some(TYPED_CLOSE.len_utf8())
        } else {
            after
                .get(..TYPED_CLOSE_ESCAPED.len())
                .filter(|close| close.eq_ignore_ascii_case(TYPED_CLOSE_ESCAPED))
                .map(str::len)
        };
        if let Some(close_len) = close_len {
            let canonical = format!("{TYPED_OPEN}{}{TYPED_CLOSE}", &tail[..body_len]);
            if typed_parts(&canonical).is_some() {
                return Some((start, body_start + body_len + close_len));
            }
        }
    }
    None
}

/// Recognize repository tokens after decoding literal and JSON unicode-escaped bytes. Raw scans
/// run before JSON decoding, so every accepted spelling must remain opaque in both forms.
fn valid_token(token: &str) -> bool {
    if token.starts_with(V2_PREFIX) {
        return token
            .strip_prefix(V2_PREFIX)
            .and_then(|body| body.strip_suffix(TOKEN_SUFFIX))
            .and_then(|body| body.split_once(':'))
            .is_some_and(|(dictionary, record)| {
                dictionary.len() == 36
                    && record.len() == 36
                    && uuid::Uuid::parse_str(dictionary).is_ok()
                    && uuid::Uuid::parse_str(record).is_ok()
            });
    }
    if CANONICAL.is_match(token) {
        return true;
    }
    token
        .strip_prefix(TOKEN_PREFIX)
        .and_then(|body| body.strip_suffix(TOKEN_SUFFIX))
        .is_some_and(valid_legacy_token_body)
}

/// Return the start of an opaque repository token that is complete and crosses
/// `cut`, or that is still a structurally valid prefix at the end of the
/// current stream buffer. The latter case is what keeps a chunk boundary from
/// exposing the token body to the registered-literal matcher.
#[cfg(feature = "secret-vault")]
pub(crate) fn streaming_token_start(text: &str, cut: usize) -> Option<usize> {
    let mut earliest = token_segments(text)
        .take_while(|(start, _, _)| *start < cut)
        .find_map(|(start, end, _)| (end > cut).then_some(start));

    // Generated tokens have a fixed, bounded shape. Inspect only possible
    // starts close enough to the buffer end to remain an incomplete token, so
    // malformed input cannot make the stream buffer grow without bound.
    for (start, _) in text.rmatch_indices('{') {
        if text.len().saturating_sub(start) >= MAX_ESCAPED_TOKEN_LEN {
            break;
        }
        if start < cut && token_prefix(&text[start..]) {
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
    }
    for (start, _) in text.rmatch_indices("\\u") {
        if text.len().saturating_sub(start) >= MAX_ESCAPED_TOKEN_LEN {
            break;
        }
        if start < cut && token_prefix(&text[start..]) {
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
    }
    earliest
}

#[cfg(any(feature = "secret-vault", test))]
fn token_prefix(fragment: &str) -> bool {
    let bytes = fragment.as_bytes();
    let mut decoded = Vec::with_capacity(MAX_TOKEN_LEN);
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            let remaining = &bytes[cursor..];
            if remaining.len() < 6 {
                return partial_escape_prefix(&decoded, remaining);
            }
        }
        let Some((byte, next)) = decoded_unit(fragment, cursor) else {
            return false;
        };
        decoded.push(byte);
        cursor = next;
        if decoded.len() > MAX_TOKEN_LEN {
            return false;
        }
    }
    token_prefix_bytes(&decoded)
}

#[cfg(any(feature = "secret-vault", test))]
fn partial_escape_prefix(decoded: &[u8], fragment: &[u8]) -> bool {
    if fragment.is_empty() || fragment[0] != b'\\' {
        return false;
    }
    let digits = match fragment {
        [b'\\'] | [b'\\', b'u'] => &[][..],
        [b'\\', b'u', rest @ ..] if rest.len() < 5 => rest,
        _ => return false,
    };
    if digits.iter().any(|digit| !digit.is_ascii_hexdigit()) {
        return false;
    }
    (0..=u8::MAX).any(|value| {
        let hex = [
            b'0',
            b'0',
            b"0123456789abcdef"[(value >> 4) as usize],
            b"0123456789abcdef"[(value & 0x0f) as usize],
        ];
        let mut extended = decoded.to_vec();
        extended.push(value);
        hex[..digits.len()] == digits[..] && token_prefix_bytes(&extended)
    })
}

#[cfg(any(feature = "secret-vault", test))]
#[derive(Clone, Copy)]
enum UuidShape {
    Hyphenated,
    Simple,
    Braced,
}

#[cfg(any(feature = "secret-vault", test))]
fn token_prefix_bytes(decoded: &[u8]) -> bool {
    if decoded.len() <= TOKEN_PREFIX.len() {
        return TOKEN_PREFIX.as_bytes().starts_with(decoded);
    }
    let body = &decoded[TOKEN_PREFIX.len()..];
    [UuidShape::Hyphenated, UuidShape::Simple, UuidShape::Braced]
        .into_iter()
        .any(|shape| token_body_prefix(body, shape))
}

#[cfg(any(feature = "secret-vault", test))]
fn token_body_prefix(body: &[u8], shape: UuidShape) -> bool {
    let uuid_len = match shape {
        UuidShape::Hyphenated => 36,
        UuidShape::Simple => 32,
        UuidShape::Braced => 38,
    };
    let body_len = uuid_len + 1 + 4 + 32 + 2;
    if body.len() > body_len {
        return false;
    }
    body.iter().enumerate().all(|(index, &byte)| {
        if index < uuid_len {
            return match shape {
                UuidShape::Hyphenated => {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                }
                UuidShape::Simple => byte.is_ascii_hexdigit(),
                UuidShape::Braced => match index {
                    0 => byte == b'{',
                    37 => byte == b'}',
                    inner => {
                        let inner = inner - 1;
                        if matches!(inner, 8 | 13 | 18 | 23) {
                            byte == b'-'
                        } else {
                            byte.is_ascii_hexdigit()
                        }
                    }
                },
            };
        }
        let after_uuid = index - uuid_len;
        if after_uuid == 0 {
            byte == b':'
        } else if (1..5).contains(&after_uuid) {
            b"sec_"[after_uuid - 1] == byte
        } else if (5..37).contains(&after_uuid) {
            byte.is_ascii_hexdigit()
        } else {
            TOKEN_SUFFIX.as_bytes()[after_uuid - 37] == byte
        }
    })
}

fn decoded_unit(text: &str, start: usize) -> Option<(u8, usize)> {
    let bytes = text.as_bytes();
    match bytes.get(start) {
        Some(b'\\') if bytes.get(start + 1) == Some(&b'u') => {
            let digits = bytes.get(start + 2..start + 6)?;
            let mut value = 0u32;
            for &digit in digits {
                value = value.checked_mul(16)?.checked_add(match digit {
                    b'0'..=b'9' => u32::from(digit - b'0'),
                    b'a'..=b'f' => u32::from(digit - b'a' + 10),
                    b'A'..=b'F' => u32::from(digit - b'A' + 10),
                    _ => return None,
                })?;
            }
            (value <= u32::from(u8::MAX)).then_some((value as u8, start + 6))
        }
        Some(byte) if byte.is_ascii() => Some((*byte, start + 1)),
        _ => None,
    }
}

fn decoded_token_end(text: &str, start: usize) -> Option<usize> {
    let mut decoded = Vec::with_capacity(MAX_TOKEN_LEN);
    let mut cursor = start;
    while cursor < text.len() && decoded.len() < MAX_TOKEN_LEN {
        let (byte, next) = decoded_unit(text, cursor)?;
        decoded.push(byte);
        cursor = next;
        if decoded.len() <= TOKEN_PREFIX.len()
            && !TOKEN_PREFIX.as_bytes().starts_with(&decoded)
            && !V2_PREFIX.as_bytes().starts_with(&decoded)
        {
            return None;
        }
        if decoded.ends_with(TOKEN_SUFFIX.as_bytes()) {
            let decoded = std::str::from_utf8(&decoded).ok()?;
            return valid_token(decoded).then_some(cursor);
        }
    }
    None
}

fn next_token(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let mut search = cursor;
    loop {
        // Search both starts together so an absent spelling cannot rescan the remaining text.
        let start = search + text[search..].find(['{', '\\'])?;
        if decoded_unit(text, start).is_some_and(|(byte, _)| byte == b'{')
            && let Some(end) = decoded_token_end(text, start)
        {
            return Some((start, end));
        }
        search = start + 1;
    }
}

pub(crate) struct TokenSegments<'a> {
    text: &'a str,
    cursor: usize,
    /// Lookahead per spelling, so a spelling absent from the rest of the text is searched once.
    legacy: Option<Option<(usize, usize)>>,
    typed: Option<Option<(usize, usize)>>,
}

impl<'a> Iterator for TokenSegments<'a> {
    type Item = (usize, usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let (text, cursor) = (self.text, self.cursor);
        let stale = |found: &Option<Option<(usize, usize)>>| {
            found.is_none_or(|found| found.is_some_and(|(start, _)| start < cursor))
        };
        if stale(&self.legacy) {
            self.legacy = Some(next_token(text, cursor));
        }
        if stale(&self.typed) {
            self.typed = Some(next_typed(text, cursor));
        }
        let (start, end) = match (self.legacy.flatten(), self.typed.flatten()) {
            (Some(legacy), Some(typed)) => legacy.min(typed),
            (Some(found), None) | (None, Some(found)) => found,
            (None, None) => return None,
        };
        self.cursor = end;
        Some((start, end, &text[start..end]))
    }
}

/// Legacy placeholders accept the UUID parser's simple and braced spellings.
/// Record ids retain the same exact shape as canonical placeholders.
fn valid_legacy_token_body(body: &str) -> bool {
    let Some((vault, record)) = body.split_once(':') else {
        return false;
    };
    uuid::Uuid::parse_str(vault).is_ok()
        && record
            .strip_prefix("sec_")
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_parsed_placeholders_are_opaque() {
        let uuid = uuid::Uuid::new_v4();
        let record = "sec_0123456789abcdef0123456789abcdef";
        for vault in [
            uuid.hyphenated().to_string(),
            uuid.simple().to_string(),
            uuid.braced().to_string(),
        ] {
            let token = format!("{TOKEN_PREFIX}{vault}:{record}{TOKEN_SUFFIX}");
            assert_eq!(token_segments(&token).count(), 1);
            let text = format!("{{{{AGIT_SECRET_V1:malformed {token} adjacent");
            assert_eq!(
                token_segments(&text)
                    .map(|(_, _, token)| token)
                    .collect::<Vec<_>>(),
                [&token]
            );
        }
        let generated = format!("{TOKEN_PREFIX}{uuid}:{record}{TOKEN_SUFFIX}");
        assert!(CANONICAL.is_match(&generated));
        for invalid in [
            generated.replace("sec_", "sec_0"),
            generated.replace(":sec_", ":other_"),
            generated.replace(&uuid.to_string(), "invalid"),
        ] {
            assert_eq!(token_segments(&invalid).count(), 0);
        }
        for split in 1..generated.len() {
            assert!(token_prefix(&generated[..split]));
        }
    }

    #[test]
    fn escaped_json_placeholders_are_opaque_without_widening_the_shape() {
        let token = "{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_00000000000000000000000000000000}}";
        let escaped = token
            .chars()
            .map(|ch| match ch {
                '{' => "\\u007b".to_owned(),
                '}' => "\\u007d".to_owned(),
                _ => ch.to_string(),
            })
            .collect::<String>();
        let text = format!("before {escaped} after");
        let segments = token_segments(&text)
            .map(|(_, _, token)| token)
            .collect::<Vec<_>>();
        assert_eq!(segments, vec![escaped.as_str()]);
        assert!(
            token_segments("\\u007b\\u007bAGIT_SECRET_V1:bad\\u007d\\u007d")
                .next()
                .is_none()
        );
    }

    #[test]
    fn mixed_escaped_braces_and_invalid_prefixes_do_not_hide_tokens() {
        let token = "{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_00000000000000000000000000000000}}";
        let variants = [
            format!("{{\\u007b{}", &token[2..]),
            format!("\\u007b{{{}", &token[2..]),
            format!("{}\\u007d", &token[..token.len() - 1]),
        ];
        for variant in variants {
            assert_eq!(token_segments(&variant).count(), 1, "{variant}");
        }
        let escaped = token
            .chars()
            .map(|ch| match ch {
                '{' => "\\u007b".to_owned(),
                '}' => "\\u007d".to_owned(),
                _ => ch.to_string(),
            })
            .collect::<String>();
        let text = format!("{{{{AGIT_SECRET_V1:bad {escaped}");
        assert_eq!(token_segments(&text).count(), 1);
    }

    #[test]
    fn escaped_legacy_uuid_spellings_are_opaque() {
        let record = "sec_00000000000000000000000000000000";
        for vault in [
            "00000000-0000-0000-0000-000000000000",
            "00000000000000000000000000000000",
            "{00000000-0000-0000-0000-000000000000}",
        ] {
            let token = format!("{TOKEN_PREFIX}{vault}:{record}{TOKEN_SUFFIX}");
            let escaped = token
                .chars()
                .map(|ch| match ch {
                    '{' => "\\u007b".to_owned(),
                    '}' => "\\u007d".to_owned(),
                    _ => ch.to_string(),
                })
                .collect::<String>();
            assert_eq!(token_segments(&escaped).count(), 1, "{escaped}");
            for split in 1..escaped.len() {
                assert!(token_prefix(&escaped[..split]), "split {split}: {escaped}");
            }
        }
    }

    #[test]
    fn many_placeholders_are_each_found_once() {
        let token = "{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_00000000000000000000000000000000}}";
        let escaped = token.replace('{', "\\u007b").replace('}', "\\u007d");
        for (unit, expected) in [
            (token.to_owned(), 1),
            (escaped.clone(), 1),
            (format!("{token}\\u0020"), 1),
            (format!("{token}{escaped}"), 2),
            (format!("\\x\\{token}"), 1),
            ("\\u0020".to_owned(), 0),
        ] {
            let text = unit.repeat(128);
            assert_eq!(token_segments(&text).count(), expected * 128, "{unit}");
        }
    }

    /// Typed tokens are opaque in raw and JSON-escaped spellings, interleaved with legacy
    /// tokens, and near misses stay ordinary text. A scanner that matched only raw delimiters
    /// would scan inside an escaped token; one that accepted any bracketed text would hide
    /// content that is not a placeholder.
    #[test]
    fn typed_tokens_are_opaque_in_raw_and_escaped_spellings() {
        let token = typed_token("secret", "github-pat").unwrap();
        let (class, kind, id) = typed_parts(&token).unwrap();
        assert_eq!((class, kind), ("secret", "github-pat"));
        assert_eq!(id.len(), TYPED_ID_LEN);
        assert_ne!(token, typed_token("secret", "github-pat").unwrap());
        let escaped = token
            .replace(TYPED_OPEN, "\\u2039")
            .replace(TYPED_CLOSE, "\\u203A");
        let legacy = "{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_00000000000000000000000000000000}}";
        let text = format!("a {token} b {legacy} c {escaped} d {token}");
        let found: Vec<_> = token_segments(&text).map(|(_, _, found)| found).collect();
        assert_eq!(
            found,
            [token.as_str(), legacy, escaped.as_str(), token.as_str()]
        );
        assert!(contains_token(&escaped));
        assert!(!crate::domain::privacy::detector::view_of(&text).contains(id));
        for near in [
            "\u{2039}secret:github-pat:abcdefghijk\u{203A}",
            "\u{2039}secret:Github-pat:abcdefghijkl\u{203A}",
            "\u{2039}unknown:github-pat:abcdefghijkl\u{203A}",
            "\u{2039}secret:github-pat:abcdefghijk1\u{203A}",
            "\u{2039}secret:github-pat:abcdefghijkl",
        ] {
            assert_eq!(token_segments(near).count(), 0, "{near}");
            assert!(!contains_token(near), "{near}");
        }
        assert_eq!(secret_kind("explicit-block"), "blocked");
        assert_eq!(secret_kind("aws-access-token"), "aws-access-token");
        assert_eq!(secret_kind("Not A Kind"), "credential");
    }
}
