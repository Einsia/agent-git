//! Captures complete native records after an unchanged materialized prefix.
//!
//! The materialized prefix is already represented by the session VIEW. Archiving it again
//! duplicates inherited evidence, so only verified append bytes can cross this frontier.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A persisted frontier binds the next capture to the exact consumed native prefix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frontier {
    pub bytes: u64,
    pub sha256: String,
}

/// Native bytes stay intact until the caller supplies the archive's logical identity.
#[derive(Debug, Eq, PartialEq)]
pub struct CapturedRegion<'a> {
    pub start: u64,
    pub records: &'a str,
    pub record_count: usize,
    pub frontier: Frontier,
    pub unconsumed: &'a [u8],
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("native archive baseline is outside the transcript")]
    BaselineOutOfRange,
    #[error("native archive baseline digest is invalid")]
    InvalidBaselineHash,
    #[error("native archive baseline bytes have changed")]
    BaselineChanged,
    #[error("native archive baseline is not valid UTF-8")]
    InvalidBaselineUtf8,
    #[error("native archive baseline does not end at a record boundary")]
    BaselineNotRecordBoundary,
    #[error("completed native archive records are not valid UTF-8")]
    InvalidRecordUtf8,
    #[error("native archive record at byte {offset} is invalid: {source}")]
    InvalidRecord {
        offset: u64,
        #[source]
        source: serde_json::Error,
    },
    #[error("native archive record at byte {offset} is not a JSON object")]
    NonObjectRecord { offset: u64 },
    #[error("native archive frontier cannot be represented")]
    FrontierOutOfRange,
}

/// Captures newline-terminated object records without interpreting runtime-specific fields.
///
/// A trailing fragment stays unconsumed even if it is valid JSON without its terminating newline.
/// Completed malformed records fail the entire capture; skipping them would advance past evidence.
/// Duplicate object keys are rejected because envelope serialization would otherwise discard values.
/// Parsing includes the enclosing envelope so accepted records remain readable after wrapping.
pub fn capture<'a>(
    snapshot: &'a [u8],
    baseline_bytes: u64,
    baseline_hash: &str,
) -> Result<CapturedRegion<'a>, CaptureError> {
    let start = usize::try_from(baseline_bytes).map_err(|_| CaptureError::BaselineOutOfRange)?;
    let prefix = snapshot
        .get(..start)
        .ok_or(CaptureError::BaselineOutOfRange)?;
    let mut expected = [0_u8; 32];
    hex::decode_to_slice(baseline_hash, &mut expected)
        .map_err(|_| CaptureError::InvalidBaselineHash)?;
    if <[u8; 32]>::from(Sha256::digest(prefix)) != expected {
        return Err(CaptureError::BaselineChanged);
    }
    std::str::from_utf8(prefix).map_err(|_| CaptureError::InvalidBaselineUtf8)?;
    if !prefix.is_empty() && prefix.last() != Some(&b'\n') {
        return Err(CaptureError::BaselineNotRecordBoundary);
    }

    let appended = &snapshot[start..];
    let completed_len = appended
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let records = std::str::from_utf8(&appended[..completed_len])
        .map_err(|_| CaptureError::InvalidRecordUtf8)?;
    let mut offset = baseline_bytes;
    let mut record_count = 0;
    for line in records.split_inclusive('\n') {
        let enclosed = b"{\"content\":"
            .as_slice()
            .chain(line.as_bytes())
            .chain(b"}".as_slice());
        let envelope: CheckedEnvelope = serde_json::from_reader(enclosed)
            .map_err(|source| CaptureError::InvalidRecord { offset, source })?;
        if !envelope.content.is_object {
            return Err(CaptureError::NonObjectRecord { offset });
        }
        let line_bytes = u64::try_from(line.len()).map_err(|_| CaptureError::FrontierOutOfRange)?;
        offset = offset
            .checked_add(line_bytes)
            .ok_or(CaptureError::FrontierOutOfRange)?;
        record_count += 1;
    }
    let end = start
        .checked_add(completed_len)
        .ok_or(CaptureError::FrontierOutOfRange)?;
    Ok(CapturedRegion {
        start: baseline_bytes,
        records,
        record_count,
        frontier: Frontier {
            bytes: offset,
            sha256: hex::encode(Sha256::digest(&snapshot[..end])),
        },
        unconsumed: &appended[completed_len..],
    })
}

/// The content occupies the same nesting position as it does in a stored envelope.
/// Unexpected outer fields are rejected without echoing native text into diagnostics.
struct CheckedEnvelope {
    content: CheckedValue,
}

impl<'de> Deserialize<'de> for CheckedEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CheckedEnvelopeVisitor;

        impl<'de> Visitor<'de> for CheckedEnvelopeVisitor {
            type Value = CheckedEnvelope;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an enclosing native archive object")
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                if object.next_key::<String>()?.as_deref() != Some("content") {
                    return Err(de::Error::custom(
                        "native archive record has an invalid boundary",
                    ));
                }
                let content = object.next_value::<CheckedValue>()?;
                if object.next_key::<String>()?.is_some() {
                    return Err(de::Error::custom(
                        "native archive record has trailing content",
                    ));
                }
                Ok(CheckedEnvelope { content })
            }
        }

        deserializer.deserialize_struct("CheckedEnvelope", &["content"], CheckedEnvelopeVisitor)
    }
}

/// Validation retains no projected content, and checks keys inside every nested object.
struct CheckedValue {
    is_object: bool,
}

impl CheckedValue {
    const OTHER: Self = Self { is_object: false };
}

impl<'de> Deserialize<'de> for CheckedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CheckedVisitor;

        impl<'de> Visitor<'de> for CheckedVisitor {
            type Value = CheckedValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value with unique object keys")
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(CheckedValue::OTHER)
            }

            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(CheckedValue::OTHER)
            }

            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(CheckedValue::OTHER)
            }

            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(CheckedValue::OTHER)
            }

            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(CheckedValue::OTHER)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CheckedValue::OTHER)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while sequence.next_element::<CheckedValue>()?.is_some() {}
                Ok(CheckedValue::OTHER)
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = BTreeSet::new();
                while let Some(key) = object.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(de::Error::custom("duplicate object key"));
                    }
                    object.next_value::<CheckedValue>()?;
                }
                Ok(CheckedValue { is_object: true })
            }
        }

        deserializer.deserialize_any(CheckedVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{CaptureError, Frontier, capture};
    use sha2::{Digest, Sha256};

    fn baseline(bytes: &[u8]) -> Frontier {
        Frontier {
            bytes: u64::try_from(bytes.len()).unwrap(),
            sha256: hex::encode(Sha256::digest(bytes)),
        }
    }

    #[test]
    fn unchanged_materialization_is_never_archived() {
        let bytes = b"{\"reminted_view\":true}\n";
        let initial = baseline(bytes);
        let capture = capture(bytes, initial.bytes, &initial.sha256).unwrap();
        assert_eq!(capture.records, "");
        assert_eq!(capture.record_count, 0);
        assert_eq!(capture.frontier, initial);
        assert!(capture.unconsumed.is_empty());
    }

    #[test]
    fn a_tail_without_any_complete_record_does_not_advance() {
        let initial = baseline(b"");
        for bytes in [b"".as_slice(), b"{\"pending\":", b"\xf0\x9f"] {
            let captured = capture(bytes, initial.bytes, &initial.sha256).unwrap();
            assert_eq!(captured.records, "");
            assert_eq!(captured.record_count, 0);
            assert_eq!(captured.frontier, initial);
            assert_eq!(captured.unconsumed, bytes);
        }
    }

    #[test]
    fn captures_unknown_records_without_projection_or_reserialization() {
        let initial_bytes = b"{\"reminted_view\":true}\n";
        let appended =
            " { \"unknown_native_kind\": [null, true, -3, 2.5, {\"text\":\"literal\"}] }\r\n{}\n";
        let initial = baseline(initial_bytes);
        let mut bytes = initial_bytes.to_vec();
        bytes.extend_from_slice(appended.as_bytes());
        let captured = capture(&bytes, initial.bytes, &initial.sha256).unwrap();
        assert_eq!(captured.start, initial.bytes);
        assert_eq!(captured.records, appended);
        assert_eq!(captured.record_count, 2);
        assert_eq!(captured.frontier, baseline(&bytes));
        assert!(captured.unconsumed.is_empty());
    }

    #[test]
    fn malformed_completed_records_do_not_yield_a_partial_success() {
        for invalid in ["{broken}\n", "\n", "{} {}\n", "{\"a\":1,}\n"] {
            let bytes = format!("{{\"valid\":true}}\n{invalid}");
            let initial = baseline(b"");
            assert!(matches!(
                capture(bytes.as_bytes(), initial.bytes, &initial.sha256),
                Err(CaptureError::InvalidRecord { offset, .. })
                    if offset == b"{\"valid\":true}\n".len() as u64
            ));
        }
    }

    #[test]
    fn completed_nonobject_values_are_not_native_records() {
        for bytes in ["null\n", "true\n", "7\n", "\"text\"\n", "[{}]\n"] {
            let initial = baseline(b"");
            assert!(matches!(
                capture(bytes.as_bytes(), initial.bytes, &initial.sha256),
                Err(CaptureError::NonObjectRecord { offset: 0 })
            ));
        }
    }

    #[test]
    fn records_outside_the_storage_parser_domain_fail_without_advancing() {
        let deep = format!("{{\"nested\":{}0{}}}\n", "[".repeat(128), "]".repeat(128));
        for bytes in ["{\"number\":1e999}\n", &deep] {
            assert!(matches!(
                capture(bytes.as_bytes(), 0, &baseline(b"").sha256),
                Err(CaptureError::InvalidRecord { offset: 0, .. })
            ));
        }
    }

    #[test]
    fn captured_record_depth_matches_the_actual_stored_envelope_boundary() {
        use crate::domain::{storage, transcript};

        for (depth, accepted) in [(124, true), (125, true), (126, false)] {
            for (opening, closing) in [("[", "]"), ("{\"child\":", "}")] {
                let raw = format!(
                    "{{\"unknown\":{}0{}}}\n",
                    opening.repeat(depth),
                    closing.repeat(depth)
                );
                assert!(serde_json::from_str::<serde_json::Value>(&raw).is_ok());
                let wrapped = transcript::wrap_lines(
                    &raw,
                    "codex",
                    "agit-0000000000000000000000000000000000000000",
                );
                assert!(!wrapped.is_empty());
                assert_eq!(
                    storage::parse_envelope_line(&wrapped).is_ok(),
                    accepted,
                    "stored envelope depth {depth}"
                );
                let captured = capture(raw.as_bytes(), 0, &baseline(b"").sha256);
                assert_eq!(captured.is_ok(), accepted, "capture depth {depth}");
                if accepted {
                    let captured = captured.unwrap();
                    assert_eq!(captured.records, raw);
                    assert_eq!(captured.record_count, 1);
                    assert_eq!(captured.frontier, baseline(raw.as_bytes()));
                } else {
                    assert!(matches!(
                        captured,
                        Err(CaptureError::InvalidRecord { offset: 0, .. })
                    ));
                }
            }
        }
    }

    #[test]
    fn raw_records_cannot_escape_the_checked_envelope_content() {
        for raw in [
            "{},\"content\":{}\n",
            "{},\"foreign\":{}\n",
            "{}}{\"content\":{}\n",
            "{}} trailing {\"content\":{}\n",
        ] {
            assert!(serde_json::from_str::<serde_json::Value>(raw).is_err());
            assert!(matches!(
                capture(raw.as_bytes(), 0, &baseline(b"").sha256),
                Err(CaptureError::InvalidRecord { offset: 0, .. })
            ));
        }
    }

    #[test]
    fn rejected_wrapper_keys_do_not_enter_error_diagnostics() {
        for key in [
            r"\u001b[2Jsecret-key".to_owned(),
            r"\u202esecret-key".to_owned(),
            "secret-key".repeat(128),
        ] {
            let raw = format!("{{}},\"{key}\":{{}}\n");
            let error = capture(raw.as_bytes(), 0, &baseline(b"").sha256).unwrap_err();
            assert!(matches!(error, CaptureError::InvalidRecord { .. }));
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("trailing content"));
            assert!(!diagnostic.contains("secret-key"));
            assert!(!diagnostic.contains(['\u{1b}', '\u{202e}']));
            assert!(diagnostic.len() < 256);
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_including_nested_and_escaped_aliases() {
        for bytes in [
            "{\"a\":1,\"a\":2}\n",
            "{\"a\": [{\"b\":1,\"b\":2}]}\n",
            "{\"a\":1,\"\\u0061\":2}\n",
        ] {
            let initial = baseline(b"");
            let error = capture(bytes.as_bytes(), initial.bytes, &initial.sha256).unwrap_err();
            assert!(matches!(error, CaptureError::InvalidRecord { .. }));
            assert!(error.to_string().contains("duplicate object key"));
        }
        let bytes = b"{\"a\":{\"b\":1},\"c\":{\"b\":2}}\n";
        assert_eq!(
            capture(bytes, 0, &baseline(b"").sha256)
                .unwrap()
                .record_count,
            1
        );
    }

    #[test]
    fn complete_json_without_a_newline_remains_unconsumed() {
        let bytes = b"{}\n{\"pending\":true}";
        let captured = capture(bytes, 0, &baseline(b"").sha256).unwrap();
        assert_eq!(captured.records, "{}\n");
        assert_eq!(captured.record_count, 1);
        assert_eq!(captured.frontier, baseline(b"{}\n"));
        assert_eq!(captured.unconsumed, b"{\"pending\":true}");
    }

    #[test]
    fn incomplete_utf8_stays_unconsumed_until_its_record_is_complete() {
        let complete = "{}\n{\"text\":\"\u{1f30d}\"}\n";
        let scalar_start = complete.find('\u{1f30d}').unwrap();
        for length in 1..4 {
            let partial = &complete.as_bytes()[..scalar_start + length];
            let captured = capture(partial, 0, &baseline(b"").sha256).unwrap();
            assert_eq!(captured.frontier, baseline(b"{}\n"));
            assert_eq!(captured.record_count, 1);
            assert_eq!(captured.unconsumed, &partial[3..]);
            let resumed = capture(
                complete.as_bytes(),
                captured.frontier.bytes,
                &captured.frontier.sha256,
            )
            .unwrap();
            assert_eq!(resumed.record_count, 1);
            assert_eq!(resumed.records, &complete[3..]);
            assert_eq!(resumed.frontier, baseline(complete.as_bytes()));
        }
    }

    #[test]
    fn invalid_utf8_in_a_completed_record_is_an_error() {
        let bytes = b"{}\n{\"text\":\"\xff\"}\n";
        assert!(matches!(
            capture(bytes, 0, &baseline(b"").sha256),
            Err(CaptureError::InvalidRecordUtf8)
        ));
    }

    #[test]
    fn rewritten_or_truncated_prefix_cannot_advance() {
        let initial = baseline(b"{\"seed\":true}\n");
        assert!(matches!(
            capture(b"{\"seed\":false}\n{}\n", initial.bytes, &initial.sha256),
            Err(CaptureError::BaselineChanged)
        ));
        assert!(matches!(
            capture(b"{}\n", initial.bytes, &initial.sha256),
            Err(CaptureError::BaselineOutOfRange)
        ));
    }

    #[test]
    fn baseline_must_be_valid_utf8_at_a_native_record_boundary() {
        let middle = b"{\"seed\":";
        let initial = baseline(middle);
        assert!(matches!(
            capture(b"{\"seed\":true}\n", initial.bytes, &initial.sha256),
            Err(CaptureError::BaselineNotRecordBoundary)
        ));
        let invalid = b"\xff\n";
        let initial = baseline(invalid);
        assert!(matches!(
            capture(invalid, initial.bytes, &initial.sha256),
            Err(CaptureError::InvalidBaselineUtf8)
        ));
    }

    #[test]
    fn invalid_digest_and_unrepresentable_baseline_are_errors() {
        for digest in ["", "not-a-digest", &"z".repeat(64), &"0".repeat(63)] {
            assert!(matches!(
                capture(b"", 0, digest),
                Err(CaptureError::InvalidBaselineHash)
            ));
        }
        assert!(matches!(
            capture(b"{}\n", u64::MAX, &baseline(b"").sha256),
            Err(CaptureError::BaselineOutOfRange)
        ));
    }

    #[test]
    fn unchanged_retries_have_identical_frontiers_and_do_not_repeat_records() {
        let bytes = b"{}\n{\"new\":true}\n{\"pending\":";
        let initial = baseline(b"{}\n");
        let first = capture(bytes, initial.bytes, &initial.sha256).unwrap();
        let retry = capture(bytes, initial.bytes, &initial.sha256).unwrap();
        assert_eq!(first, retry);
        let next = capture(bytes, first.frontier.bytes, &first.frontier.sha256).unwrap();
        assert_eq!(next.record_count, 0);
        assert_eq!(next.records, "");
        assert_eq!(next.frontier, first.frontier);
        assert_eq!(next.unconsumed, first.unconsumed);
    }
}
