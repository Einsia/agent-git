//! Structural image carriers exclude entropy findings only; credential rules still inspect them.

use serde_json::value::RawValue;
#[cfg(feature = "secret-vault")]
use serde_json::{Map, Value};
use std::{borrow::Cow, collections::HashMap};

/// MIME labels identify an image carrier, but producers may mislabel the actual image format.
/// Both the base64 framing and the decoded image header and trailer must validate independently.
#[cfg(feature = "secret-vault")]
pub(crate) fn image_data(map: &Map<String, Value>) -> Option<&str> {
    let data = map.get("data")?.as_str()?;
    is_image(
        map.get("type")?.as_str()?,
        map.get("mimeType")?.as_str()?,
        data,
    )
    .then_some(data)
}

fn is_image(kind: &str, mime: &str, data: &str) -> bool {
    kind == "image"
        && matches!(
            mime,
            "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp"
        )
        && valid_image(data)
}

fn sextet(byte: u8) -> Option<u8> {
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

/// Validate the complete encoding with constant decoded storage, even for a large screenshot.
pub(crate) fn valid_image(data: &str) -> bool {
    let Some((data, gaps)) = visible_base64(data) else {
        return false;
    };
    if data.len() < 16 || !data.len().is_multiple_of(4) {
        return false;
    }
    let mut head = [0u8; 12];
    let mut tail = [0u8; 12];
    let mut length = 0usize;
    for (index, group) in data.as_bytes().chunks_exact(4).enumerate() {
        let Some(a) = sextet(group[0]) else {
            return false;
        };
        let Some(b) = sextet(group[1]) else {
            return false;
        };
        let last = (index + 1) * 4 == data.len();
        let (c, d, count) = match (group[2], group[3]) {
            (b'=', b'=') if last && b & 15 == 0 => (0, 0, 1),
            (c, b'=') if last => {
                let Some(c) = sextet(c) else { return false };
                if c & 3 != 0 {
                    return false;
                }
                (c, 0, 2)
            }
            (c, d) => {
                let (Some(c), Some(d)) = (sextet(c), sextet(d)) else {
                    return false;
                };
                (c, d, 3)
            }
        };
        for byte in [a << 2 | b >> 4, b << 4 | c >> 2, c << 6 | d]
            .into_iter()
            .take(count)
        {
            if length < head.len() {
                head[length] = byte;
            }
            tail.rotate_left(1);
            tail[11] = byte;
            length += 1;
        }
    }
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    let visible_framing = |header: usize, trailer: usize| {
        let trailer = (trailer + padding).div_ceil(3) * 4;
        gaps.first().is_none_or(|start| *start >= header)
            && gaps
                .last()
                .is_none_or(|end| *end <= data.len().saturating_sub(trailer))
    };
    (head.starts_with(&[0xff, 0xd8, 0xff])
        && tail.ends_with(&[0xff, 0xd9])
        && visible_framing(4, 2))
        || (head.starts_with(b"\x89PNG\r\n\x1a\n")
            && tail == *b"\0\0\0\0IEND\xaeB`\x82"
            && visible_framing(12, 12))
        || ((head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"))
            && tail[11] == 0x3b
            && visible_framing(8, 1))
        || (head.starts_with(b"RIFF")
            && &head[8..12] == b"WEBP"
            && visible_framing(16, 1)
            && u32::from_le_bytes(head[4..8].try_into().unwrap())
                .checked_add(8)
                .is_some_and(|declared| {
                    let missing = (declared as usize).checked_sub(length);
                    if gaps.is_empty() {
                        missing == Some(0)
                    } else {
                        missing
                            .is_some_and(|bytes| bytes >= gaps.len() * 3 && bytes.is_multiple_of(3))
                    }
                }))
}

/// Opaque dictionary tokens stand for whole base64 quartets, never for arbitrary malformed text.
/// The visible prefix and suffix still have to prove the image framing independently.
fn visible_base64(data: &str) -> Option<(Cow<'_, str>, Vec<usize>)> {
    let mut visible = String::new();
    let mut gaps = Vec::new();
    let mut cursor = 0;
    for (start, end, _) in super::placeholder::token_segments(data) {
        let part = &data[cursor..start];
        if !part.len().is_multiple_of(4) || part.contains('=') {
            return None;
        }
        visible.push_str(part);
        gaps.push(visible.len());
        cursor = end;
    }
    if gaps.is_empty() {
        return Some((Cow::Borrowed(data), gaps));
    }
    visible.push_str(&data[cursor..]);
    Some((Cow::Owned(visible), gaps))
}

/// Source ranges preserve occurrence identity: identical bytes in an unrelated field stay scanned.
pub(crate) fn regions(text: &str) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    for (chunk, value) in super::jsonl_chunks(text) {
        if value.is_some()
            && let Ok(raw) = serde_json::from_str::<&RawValue>(chunk)
        {
            collect(raw, text.as_ptr() as usize, 0, &mut regions);
        }
    }
    regions.sort_unstable();
    regions
}

fn collect(raw: &RawValue, base: usize, depth: usize, out: &mut Vec<(usize, usize)>) {
    if depth >= 128 {
        return;
    }
    match raw.get().as_bytes().first() {
        Some(b'{') => {
            let Ok(map) = serde_json::from_str::<HashMap<String, &RawValue>>(raw.get()) else {
                return;
            };
            if let (Some(kind), Some(mime), Some(data)) =
                (map.get("type"), map.get("mimeType"), map.get("data"))
                && let (Ok(kind), Ok(mime), Ok(decoded)) = (
                    serde_json::from_str::<String>(kind.get()),
                    serde_json::from_str::<String>(mime.get()),
                    serde_json::from_str::<String>(data.get()),
                )
                && is_image(&kind, &mime, &decoded)
            {
                let start = data.get().as_ptr() as usize - base;
                out.push((start + 1, start + data.get().len() - 1));
            }
            for child in map.values() {
                collect(child, base, depth + 1, out);
            }
        }
        Some(b'[') => {
            if let Ok(values) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) {
                for child in values {
                    collect(child, base, depth + 1, out);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn contains(regions: &[(usize, usize)], start: usize, end: usize) -> bool {
    let index = regions.partition_point(|(left, _)| *left <= start);
    index > 0 && end <= regions[index - 1].1
}

#[cfg(test)]
pub(crate) fn fixture() -> String {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut data = String::from("/9j/");
    for _ in 0..76800 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push(
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
                [(seed >> 33) as usize % 64] as char,
        );
    }
    data.push_str("/9k=");
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::secrets::{Policy, scan_text_capped};
    use std::collections::HashSet;

    fn scan(text: &str) -> Vec<crate::domain::secrets::Hit> {
        let report = scan_text_capped(text, &HashSet::new(), Policy::STRICT, 100);
        assert!(!report.truncated);
        report.hits
    }

    /// An image exemption belongs to its occurrence, including when JSON escapes hide its bytes.
    #[test]
    fn mcp_image_context_survives_raw_and_decoded_scans() {
        let data = fixture();
        let image = serde_json::json!({"type":"image", "mimeType":"image/png", "data":data});
        let event = serde_json::json!({"type":"event_msg", "payload":{"item":{"type":"McpToolCall", "result":{"content":[image]}}}});
        let text = event.to_string();
        assert!(scan(&text).is_empty());
        assert!(scan(&text.replace('/', "\\/")).is_empty());
        let with_untyped_copy = serde_json::json!({"image":image,"message":data}).to_string();
        assert!(
            scan(&with_untyped_copy)
                .iter()
                .any(|hit| hit.rule == "high-entropy-value")
        );
        assert!(
            scan(&data)
                .iter()
                .any(|hit| hit.rule == "high-entropy-value")
        );
    }

    /// Structural media classification never overrides provider-specific credential rules.
    #[test]
    fn mcp_image_provider_findings_remain_visible() {
        let mut data = fixture();
        data.insert_str(4, "/AKIA4X7QZ2M5RT6VW3JH///");
        assert!(valid_image(&data));
        let text =
            serde_json::json!({"type":"image","mimeType":"image/jpeg","data":data}).to_string();
        assert!(scan(&text).iter().any(|hit| hit.rule == "aws-access-token"));
    }

    /// Redaction gaps preserve quartet boundaries and cannot stand in for damaged encoding or framing.
    #[test]
    fn mcp_image_redaction_gaps_require_valid_surrounding_base64() {
        let data = fixture();
        let token = "{{AGIT_SECRET_V1:00000000-0000-4000-8000-000000000001:sec_00000000000000000000000000000001}}";
        let projected = format!("{}{token}{}", &data[..12], &data[36..]);
        assert!(valid_image(&projected));
        let object = |data: &str| {
            serde_json::json!({"type":"image", "mimeType":"image/jpeg", "data":data}).to_string()
        };
        assert!(scan(&object(&projected)).is_empty());
        assert!(scan(&object(&projected).replace('/', "\\/")).is_empty());
        for malformed in [
            format!("{}{token}{}", &data[..13], &data[37..]),
            format!("{}{token}{}", &data[..12], &data[35..]),
            projected.replacen("/9j/", "QUJD", 1),
            projected.replace("/9k=", "QUJD"),
            projected.replace("AGIT_SECRET_V1:", "AGIT_SECRET_V1:invalid"),
            format!("{}{token}!{}", &data[..12], &data[37..]),
            format!("{token}{data}"),
            format!("{data}{token}"),
        ] {
            assert!(!valid_image(&malformed));
            assert!(
                scan(&object(&malformed))
                    .iter()
                    .any(|hit| hit.rule == "high-entropy-value")
            );
        }
    }

    /// RIFF size accounts for whole decoded groups hidden by an opaque dictionary token.
    #[cfg(feature = "secret-vault")]
    #[test]
    fn mcp_image_webp_size_accounts_for_redacted_quartets() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let mut bytes = b"RIFF".to_vec();
        bytes.extend(100u32.to_le_bytes());
        bytes.extend(b"WEBP");
        bytes.resize(108, 0);
        let data = STANDARD.encode(&bytes);
        let token = "{{AGIT_SECRET_V1:00000000-0000-4000-8000-000000000001:sec_00000000000000000000000000000001}}";
        assert!(valid_image(&data));
        assert!(valid_image(&format!(
            "{}{token}{}",
            &data[..20],
            &data[40..]
        )));
        bytes[4..8].copy_from_slice(&101u32.to_le_bytes());
        let malformed = STANDARD.encode(&bytes);
        assert!(!valid_image(&format!(
            "{}{token}{}",
            &malformed[..20],
            &malformed[40..]
        )));
    }

    /// Field names, MIME claims and a header alone cannot exempt arbitrary token-like text.
    #[test]
    fn mcp_image_validation_rejects_untrusted_claims_and_incomplete_encodings() {
        let data = fixture();
        for object in [
            serde_json::json!({"data": data}),
            serde_json::json!({"type":"text","mimeType":"image/jpeg","data":data}),
            serde_json::json!({"type":"image","mimeType":"text/plain","data":data}),
            serde_json::json!({"type":"image","mimeType":"image/jpeg","data":data.trim_end_matches('=')}),
            serde_json::json!({"type":"image","mimeType":"image/jpeg","data":data.replace("/9j/", "QUJD")}),
            serde_json::json!({"type":"image","mimeType":"image/jpeg","data":data.replace("/9k=", "QUJD")}),
        ] {
            assert!(
                scan(&object.to_string())
                    .iter()
                    .any(|hit| hit.rule == "high-entropy-value")
            );
        }
        assert!(
            scan(&format!(
                "{{\"type\":\"image\",\"mimeType\":\"image/jpeg\",\"data\":\"{data}"
            ))
            .iter()
            .any(|hit| hit.rule == "high-entropy-value")
        );
    }
}
