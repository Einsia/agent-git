//! File locations and web links supply context for entropy-only discovery.

/// Address recognition is lexical: inspection must not depend on this machine's filesystem.
pub(super) fn regions(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let before = &text[..start];
        let rest = &text[start..];
        let previous = before.chars().next_back();
        let boundary = previous.is_none_or(|ch| {
            ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | '(' | '[' | '<' | '=' | ':')
        });
        let markdown = before.ends_with("](") || before.ends_with("](<");
        if boundary && (rooted(rest) || web(rest) || file_url(rest) || markdown) {
            let delimiter = match previous {
                Some('"' | '\'' | '`' | '<') => previous,
                Some('(') if markdown => Some(')'),
                _ => None,
            };
            let end = end_of_location(text, start, delimiter);
            let location = &text[start..end];
            if valid_url(location) || valid_path(location, markdown) {
                out.push((start, end));
            }
            start = end.max(start + rest.chars().next().unwrap().len_utf8());
        } else {
            start += rest.chars().next().unwrap().len_utf8();
        }
    }
    out
}

fn web(text: &str) -> bool {
    prefix(text, "http://") || prefix(text, "https://")
}

fn file_url(text: &str) -> bool {
    prefix(text, "file://")
}

fn prefix(text: &str, expected: &str) -> bool {
    text.get(..expected.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(expected))
}

fn rooted(text: &str) -> bool {
    text.starts_with('/')
        || text.starts_with("\\\\")
        || ["~/", "~\\", "./", "../", ".\\", "..\\"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
        || (text.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && text.as_bytes().get(1) == Some(&b':')
            && matches!(text.as_bytes().get(2), Some(b'/' | b'\\')))
}

fn end_of_location(text: &str, start: usize, delimiter: Option<char>) -> usize {
    let url = web(&text[start..]) || file_url(&text[start..]);
    let mut depth = 0usize;
    for (offset, ch) in text[start..].char_indices() {
        if ch.is_control()
            || (ch.is_whitespace() && (delimiter.is_none() || url))
            || matches!(ch, '"' | '\'' | '`' | '<' | '>')
            || (!ch.is_ascii() && !ch.is_alphanumeric())
        {
            return start + offset;
        }
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return start + offset,
            ')' => depth -= 1,
            ']' if delimiter.is_none() && !url => return start + offset,
            _ => {}
        }
    }
    text.len()
}

fn valid_url(text: &str) -> bool {
    if text.chars().any(char::is_whitespace) {
        return false;
    }
    if file_url(text) {
        return text.len() > "file://".len();
    }
    if !web(text) {
        return false;
    }
    let Some((_, body)) = text.split_once("://") else {
        return false;
    };
    let authority = body.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && authority.chars().any(|ch| ch.is_alphanumeric())
        && !authority.contains('\\')
}

/// Relative paths require a link destination or a named path field; a slash alone is not evidence.
pub(crate) fn field_path(text: &str) -> bool {
    valid_url(text) || valid_path(text, true)
}

fn valid_path(text: &str, relative: bool) -> bool {
    if text.is_empty() || (!rooted(text) && !relative) {
        return false;
    }
    let body = if text.as_bytes().get(1) == Some(&b':') {
        &text[2..]
    } else {
        text
    };
    let body = match body.split_once(':') {
        Some((path, position))
            if !position.is_empty()
                && position
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b':') =>
        {
            path
        }
        _ => body,
    };
    // The alphabet excludes base64 padding and plus signs. Provider and registered rules
    // still inspect the original path, even when its ordinary spelling earns an entropy exemption.
    body.chars().all(|ch| {
        ch.is_alphanumeric() || matches!(ch, '/' | '\\' | '.' | '_' | '-' | '~' | ' ' | '(' | ')')
    }) && body.chars().any(char::is_alphanumeric)
        && (body.contains(['/', '\\'])
            || body.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty() && !ext.is_empty() && ext.chars().all(char::is_alphanumeric)
            }))
}

pub(crate) fn is_path_field(field: &str) -> bool {
    matches!(
        field.to_ascii_lowercase().rsplit(['_', '-']).next(),
        Some("path" | "paths" | "cwd" | "directory" | "filename" | "filepath" | "filepaths")
    )
}

#[cfg(test)]
mod tests {
    use crate::domain::secrets::{Policy, scan_text_capped};
    use std::collections::HashSet;

    fn hits(text: &str) -> Vec<crate::domain::secrets::Hit> {
        let report = scan_text_capped(text, &HashSet::new(), Policy::STRICT, 100);
        assert!(!report.truncated);
        report.hits
    }

    /// Locations retain their meaning in prose, links and semantic JSON; unrelated slash-bearing tokens do not.
    #[test]
    fn location_context_excludes_only_entropy_findings() {
        let opaque = "R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        // CJK text exercises path boundaries and JSON decoding.
        let text = format!(
            "[Report](/Users/alice/Projects/frontier-code-2026-09-28.md:42)\n\
             [Report](artifacts/{opaque}/report.md)\n\
             [Report](</Users/alice/My Reports/质量/{opaque}/report.md>)\n\
             [Report](https://docs.example.org/docx/{opaque}?view=full)\n\
             http://docs.example.org/reports/{opaque}\n\
             file:///Users/alice/Reports/{opaque}.md\n\
             cwd=~/Projects/{opaque}/report.md\n\
             ./reports/{opaque}/report.md ../reports/{opaque}/report.md\n\
             \"C:\\Users\\Alice\\My Reports\\{opaque}\\report.md\"\n\
             C:\\temp\\{opaque}\\report.md\n\
             \\\\server\\reports\\{opaque}\\report.md"
        );
        assert!(hits(&text).is_empty(), "{:?}", hits(&text));
        let json =
            serde_json::json!({"text":text, "file_path":format!("artifacts/{opaque}/output")})
                .to_string();
        assert!(hits(&json).is_empty());
        assert!(hits(&json.replace('/', "\\/")).is_empty());

        for text in [
            format!("{text}\n{opaque}"),
            "R7kQ2mXv9LpZ4tNc8/WjF3bHy6sVd1aGe5uKr".to_owned(),
            serde_json::json!({"message":format!("artifacts/{opaque}/output")}).to_string(),
            serde_json::json!({"password":format!("https://docs.example.org/{opaque}")})
                .to_string(),
            "[Report](https://docs.example.org/AKIA4X7QZ2M5RT6VW3JH/report)".to_owned(),
        ] {
            assert!(!hits(&text).is_empty());
        }

        #[cfg(feature = "secret-vault")]
        {
            let registered =
                crate::domain::secret_filter::Matcher::for_test(&[("explicit", opaque)]);
            assert!(
                !crate::domain::secrets::scan_text_registered_with(
                    &json,
                    &HashSet::new(),
                    &registered
                )
                .is_empty()
            );
        }
    }
}
