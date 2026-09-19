//! Opaque secret placeholders shared by discovery, projection and publication.

use regex::Regex;
use std::sync::LazyLock;

pub(crate) const TOKEN_PREFIX: &str = "{{AGIT_SECRET_V1:";
pub(crate) const TOKEN_SUFFIX: &str = "}}";
#[cfg(feature = "secret-vault")]
pub(crate) const CANONICAL_TOKEN_LEN: usize =
    TOKEN_PREFIX.len() + 36 + 1 + 4 + 32 + TOKEN_SUFFIX.len();

static CANONICAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A\{\{AGIT_SECRET_V1:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}:sec_[0-9A-Fa-f]{32}\}\}\z")
        .expect("canonical placeholder shape")
});

pub(crate) fn token_segments(text: &str) -> TokenSegments<'_> {
    TokenSegments { text, cursor: 0 }
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
        if text.len().saturating_sub(start) >= CANONICAL_TOKEN_LEN {
            break;
        }
        if start < cut && canonical_token_prefix(&text[start..]) {
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
    }
    earliest
}

#[cfg(feature = "secret-vault")]
fn canonical_token_prefix(fragment: &str) -> bool {
    if fragment.is_empty() || fragment.len() >= CANONICAL_TOKEN_LEN {
        return false;
    }
    fragment
        .bytes()
        .enumerate()
        .all(|(index, byte)| canonical_token_byte(index, byte))
}

#[cfg(feature = "secret-vault")]
fn canonical_token_byte(index: usize, byte: u8) -> bool {
    if index < TOKEN_PREFIX.len() {
        return TOKEN_PREFIX.as_bytes()[index] == byte;
    }

    let uuid_index = index - TOKEN_PREFIX.len();
    if uuid_index < 36 {
        return if matches!(uuid_index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        };
    }

    let after_uuid = uuid_index - 36;
    if after_uuid == 0 {
        return byte == b':';
    }
    if (1..5).contains(&after_uuid) {
        return b"sec_"[after_uuid - 1] == byte;
    }
    if (5..37).contains(&after_uuid) {
        return byte.is_ascii_hexdigit();
    }
    TOKEN_SUFFIX.as_bytes()[after_uuid - 37] == byte
}

pub(crate) struct TokenSegments<'a> {
    text: &'a str,
    cursor: usize,
}

impl<'a> Iterator for TokenSegments<'a> {
    type Item = (usize, usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(relative) = self.text[self.cursor..].find(TOKEN_PREFIX) {
            let start = self.cursor + relative;
            let content_start = start + TOKEN_PREFIX.len();
            let Some(close) = self.text[content_start..].find(TOKEN_SUFFIX) else {
                self.cursor = self.text.len();
                return None;
            };
            let end = content_start + close + TOKEN_SUFFIX.len();
            let body = &self.text[content_start..content_start + close];
            if CANONICAL.is_match(&self.text[start..end]) || valid_legacy_token_body(body) {
                self.cursor = end;
                return Some((start, end, &self.text[start..end]));
            }
            self.cursor = content_start;
        }
        None
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
            assert!(canonical_token_prefix(&generated[..split]));
        }
    }
}
