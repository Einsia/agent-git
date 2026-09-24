//! Opaque secret placeholders shared by discovery, projection and publication.

use regex::Regex;
use std::sync::LazyLock;

pub(crate) const TOKEN_PREFIX: &str = "{{AGIT_SECRET_V1:";
pub(crate) const TOKEN_SUFFIX: &str = "}}";
pub(crate) const CANONICAL_TOKEN_LEN: usize =
    TOKEN_PREFIX.len() + 36 + 1 + 4 + 32 + TOKEN_SUFFIX.len();
const MAX_TOKEN_LEN: usize = CANONICAL_TOKEN_LEN + 2;
#[cfg(feature = "secret-vault")]
const MAX_ESCAPED_TOKEN_LEN: usize = MAX_TOKEN_LEN * 6;

static CANONICAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A\{\{AGIT_SECRET_V1:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}:sec_[0-9A-Fa-f]{32}\}\}\z")
        .expect("canonical placeholder shape")
});

pub(crate) fn token_segments(text: &str) -> TokenSegments<'_> {
    TokenSegments { text, cursor: 0 }
}

/// Recognize repository tokens after decoding literal and JSON unicode-escaped bytes. Raw scans
/// run before JSON decoding, so every accepted spelling must remain opaque in both forms.
fn valid_token(token: &str) -> bool {
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
        if decoded.len() <= TOKEN_PREFIX.len() && !TOKEN_PREFIX.as_bytes().starts_with(&decoded) {
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
}

impl<'a> Iterator for TokenSegments<'a> {
    type Item = (usize, usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let (start, end) = next_token(self.text, self.cursor)?;
        self.cursor = end;
        Some((start, end, &self.text[start..end]))
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
}
