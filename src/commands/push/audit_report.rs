//! A validated audit report describes protocol-consistent claims without authorizing publication.

use anyhow::{Result, ensure};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

pub(crate) const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_ITEMS: usize = 4096;
const MAX_FINDINGS: usize = 512;
const MAX_QUESTIONS: usize = 128;
const MAX_ID_BYTES: usize = 128;
const MAX_TEXT_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestBinding {
    #[serde(deserialize_with = "bounded_id")]
    pub audit_id: String,
    #[serde(deserialize_with = "bounded_id")]
    pub manifest_sha256: String,
}

/// Content availability comes from the caller; it does not establish model attention or judgment.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ExpectedKind {
    Text {
        extent_bytes: u64,
        available_bytes: u64,
        content_complete: bool,
    },
    VerifiedBinary,
    Unavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct ExpectedItem {
    pub id: String,
    pub kind: ExpectedKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportStatus {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemStatus {
    Reviewed,
    Partial,
    BinaryExcluded,
    Unread,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemAssessment {
    #[serde(deserialize_with = "bounded_id")]
    id: String,
    status: ItemStatus,
    reviewed_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Category {
    PrivateInformation,
    InternalInformation,
    PersonalConversation,
    UnrelatedContent,
    PrivatePath,
    OtherDisclosureRisk,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Self::PrivateInformation => "private information",
            Self::InternalInformation => "internal information",
            Self::PersonalConversation => "personal conversation",
            Self::UnrelatedContent => "unrelated content",
            Self::PrivatePath => "private path",
            Self::OtherDisclosureRisk => "other disclosure risk",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    #[serde(deserialize_with = "bounded_id")]
    item_id: String,
    start_byte: u64,
    end_byte: u64,
    category: Category,
    #[serde(deserialize_with = "bounded_text")]
    explanation: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingQuestion {
    #[serde(deserialize_with = "bounded_id")]
    id: String,
    #[serde(deserialize_with = "bounded_id")]
    item_id: String,
    #[serde(deserialize_with = "bounded_text")]
    question: String,
    required: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    binding: ManifestBinding,
    status: ReportStatus,
    #[serde(deserialize_with = "bounded_text")]
    summary: String,
    #[serde(deserialize_with = "bounded_items")]
    items: Vec<ItemAssessment>,
    #[serde(deserialize_with = "bounded_findings")]
    findings: Vec<Finding>,
    #[serde(deserialize_with = "bounded_questions")]
    pending_questions: Vec<PendingQuestion>,
}

/// Protocol validation does not establish model diligence, a user answer, or publication consent.
#[derive(Debug)]
pub(crate) struct ValidatedReport {
    report: Report,
}

impl ValidatedReport {
    pub(crate) fn complete(&self) -> bool {
        self.report.status == ReportStatus::Complete
    }

    /// Structured data retains original strings; terminal presentation uses the escaped renderer.
    pub(crate) fn as_json(&self) -> Result<Value> {
        Ok(serde_json::to_value(&self.report)?)
    }

    pub(crate) fn render(&self) -> String {
        let report = &self.report;
        let mut lines = vec![
            "Publication sensitivity review".to_owned(),
            format!("Audit: {}", quoted(&report.binding.audit_id)),
            format!("Manifest: {}", quoted(&report.binding.manifest_sha256)),
            format!(
                "Review status: {}",
                if self.complete() {
                    "complete"
                } else {
                    "incomplete"
                }
            ),
            format!("Summary: {}", quoted(&report.summary)),
            "Model findings are advisory. This report does not authorize publication.".to_owned(),
        ];
        let count = |status| {
            report
                .items
                .iter()
                .filter(|item| item.status == status)
                .count()
        };
        lines.push(format!(
            "Coverage: {} text items reviewed; {} verified binary items excluded; {} partial; {} unread.",
            count(ItemStatus::Reviewed), count(ItemStatus::BinaryExcluded),
            count(ItemStatus::Partial), count(ItemStatus::Unread)
        ));
        for finding in &report.findings {
            lines.push(format!(
                "Finding: {}; bytes {}..{}; {}; {}",
                quoted(&finding.item_id),
                finding.start_byte,
                finding.end_byte,
                finding.category.label(),
                quoted(&finding.explanation)
            ));
        }
        for question in &report.pending_questions {
            lines.push(format!(
                "Unanswered question: {}; item {}; {}; {}",
                quoted(&question.id),
                quoted(&question.item_id),
                if question.required {
                    "required"
                } else {
                    "optional"
                },
                quoted(&question.question)
            ));
        }
        if report.findings.is_empty() {
            lines.push(if self.complete() {
                "No sensitive findings reported in the declared readable text. This is not a guarantee of absence.".to_owned()
            } else {
                "No findings reported; incomplete coverage does not establish absence of sensitive content.".to_owned()
            });
        }
        lines.join("\n")
    }
}

fn quoted(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len().saturating_add(2));
    escaped.push('"');
    for character in text.chars() {
        escaped.extend(character.escape_default());
    }
    escaped.push('"');
    escaped
}

/// The caller binds complete supplied evidence; model claims cannot widen its inventory or extent.
pub(crate) fn validate_report(
    raw_bytes: &[u8],
    expected_manifest_binding: &ManifestBinding,
    expected_items: &[ExpectedItem],
) -> Result<ValidatedReport> {
    ensure!(
        raw_bytes.len() <= MAX_REPORT_BYTES,
        "audit report exceeds its byte limit"
    );
    validate_binding(expected_manifest_binding)?;
    ensure!(
        expected_items.len() <= MAX_ITEMS,
        "audit manifest exceeds its item limit"
    );
    let mut expected = BTreeMap::new();
    for item in expected_items {
        validate_id(&item.id)?;
        ensure!(
            expected.insert(item.id.as_str(), item.kind).is_none(),
            "audit manifest repeats an item"
        );
        if let ExpectedKind::Text {
            extent_bytes,
            available_bytes,
            content_complete,
        } = item.kind
        {
            ensure!(
                available_bytes <= extent_bytes,
                "audit content availability exceeds the text extent"
            );
            ensure!(
                !content_complete || available_bytes == extent_bytes,
                "complete audit content availability has a mismatched extent"
            );
        }
    }
    let report: Report = serde_json::from_slice(raw_bytes)
        .map_err(|_| anyhow::anyhow!("audit report is not valid bounded report JSON"))?;
    validate_binding(&report.binding)?;
    ensure!(
        report.binding == *expected_manifest_binding,
        "audit report names another frozen manifest"
    );
    ensure!(
        report.items.len() == expected.len(),
        "audit report does not cover the manifest inventory"
    );
    ensure!(
        !report.summary.trim().is_empty(),
        "audit report has no summary"
    );
    let mut assessments = BTreeMap::new();
    let mut coverage_complete = true;
    for item in &report.items {
        validate_id(&item.id)?;
        ensure!(
            assessments.insert(item.id.as_str(), item).is_none(),
            "audit report repeats an item"
        );
        let kind = expected
            .get(item.id.as_str())
            .ok_or_else(|| anyhow::anyhow!("audit report names an unknown item"))?;
        let complete = match (kind, item.status) {
            (
                ExpectedKind::Text {
                    extent_bytes,
                    available_bytes,
                    content_complete,
                },
                ItemStatus::Reviewed,
            ) => {
                ensure!(
                    item.reviewed_bytes == *extent_bytes,
                    "reviewed audit item has a mismatched extent"
                );
                ensure!(
                    *content_complete && *available_bytes == *extent_bytes,
                    "audit item claims review without complete caller supplied evidence"
                );
                true
            }
            (
                ExpectedKind::Text {
                    extent_bytes,
                    available_bytes,
                    ..
                },
                ItemStatus::Partial,
            ) => {
                ensure!(
                    item.reviewed_bytes <= *extent_bytes && item.reviewed_bytes <= *available_bytes,
                    "partial audit review exceeds caller supplied evidence"
                );
                false
            }
            (ExpectedKind::VerifiedBinary, ItemStatus::BinaryExcluded) => {
                ensure!(
                    item.reviewed_bytes == 0,
                    "binary audit item claims text review"
                );
                true
            }
            (_, ItemStatus::Unread) => {
                ensure!(
                    item.reviewed_bytes == 0,
                    "unread audit item claims text review"
                );
                false
            }
            _ => anyhow::bail!("audit item classification differs from the caller inventory"),
        };
        coverage_complete &= complete;
    }
    for finding in &report.findings {
        let item = assessments
            .get(finding.item_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("audit finding names an unknown item"))?;
        ensure!(
            matches!(item.status, ItemStatus::Reviewed | ItemStatus::Partial),
            "audit finding requires reviewed text within a declared full or partial read"
        );
        ensure!(
            finding.start_byte < finding.end_byte && finding.end_byte <= item.reviewed_bytes,
            "audit finding has an invalid text extent"
        );
        ensure!(
            !finding.explanation.trim().is_empty(),
            "audit finding has no explanation"
        );
    }
    let mut questions = BTreeSet::new();
    for question in &report.pending_questions {
        validate_id(&question.id)?;
        ensure!(
            questions.insert(question.id.as_str()),
            "audit report repeats a question"
        );
        ensure!(
            expected.contains_key(question.item_id.as_str()),
            "audit question names an unknown item"
        );
        ensure!(
            !question.question.trim().is_empty(),
            "audit question is empty"
        );
    }
    if report.status == ReportStatus::Complete {
        ensure!(
            coverage_complete,
            "complete audit report has missing review coverage"
        );
        ensure!(
            !report
                .pending_questions
                .iter()
                .any(|question| question.required),
            "complete audit report has a required unanswered question"
        );
    }
    Ok(ValidatedReport { report })
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= MAX_ID_BYTES,
        "audit identifier is empty or oversized"
    );
    Ok(())
}

fn validate_binding(binding: &ManifestBinding) -> Result<()> {
    validate_id(&binding.audit_id)?;
    ensure!(
        binding.manifest_sha256.len() == 64
            && binding
                .manifest_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "audit manifest binding is not a canonical SHA-256 digest"
    );
    Ok(())
}

fn bounded_id<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<String, D::Error> {
    bounded_string::<D, MAX_ID_BYTES>(deserializer)
}

fn bounded_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    bounded_string::<D, MAX_TEXT_BYTES>(deserializer)
}

fn bounded_string<'de, D: Deserializer<'de>, const LIMIT: usize>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    struct BoundedString<const LIMIT: usize>;
    impl<'de, const LIMIT: usize> Visitor<'de> for BoundedString<LIMIT> {
        type Value = String;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded string")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<String, E> {
            if value.len() > LIMIT {
                return Err(E::custom("audit string exceeds its byte limit"));
            }
            Ok(value.to_owned())
        }
        fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<String, E> {
            if value.len() > LIMIT {
                return Err(E::custom("audit string exceeds its byte limit"));
            }
            Ok(value)
        }
    }
    deserializer.deserialize_string(BoundedString::<LIMIT>)
}

fn bounded_items<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<ItemAssessment>, D::Error> {
    bounded_sequence::<D, ItemAssessment, MAX_ITEMS>(deserializer)
}

fn bounded_findings<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Finding>, D::Error> {
    bounded_sequence::<D, Finding, MAX_FINDINGS>(deserializer)
}

fn bounded_questions<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<PendingQuestion>, D::Error> {
    bounded_sequence::<D, PendingQuestion, MAX_QUESTIONS>(deserializer)
}

fn bounded_sequence<'de, D: Deserializer<'de>, T: Deserialize<'de>, const LIMIT: usize>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error> {
    struct BoundedSequence<T, const LIMIT: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for BoundedSequence<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded array")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == LIMIT {
                    return Err(de::Error::custom("audit array exceeds its item limit"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(BoundedSequence::<T, LIMIT>(PhantomData))
}

pub(crate) fn report_schema() -> Value {
    let id = json!({"type":"string","minLength":1,"maxLength":MAX_ID_BYTES});
    let text = json!({"type":"string","minLength":1,"maxLength":MAX_TEXT_BYTES});
    let integer = json!({"type":"integer","minimum":0,"maximum":u64::MAX});
    json!({
        "type":"object","additionalProperties":false,
        "required":["binding","status","summary","items","findings","pending_questions"],
        "properties":{
            "binding":{"type":"object","additionalProperties":false,"required":["audit_id","manifest_sha256"],"properties":{
                "audit_id":id,"manifest_sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"}
            }},
            "status":{"type":"string","enum":["complete","incomplete"]},"summary":text,
            "items":{"type":"array","maxItems":MAX_ITEMS,"items":{
                "type":"object","additionalProperties":false,"required":["id","status","reviewed_bytes"],"properties":{
                    "id":id,"status":{"type":"string","enum":["reviewed","partial","binary_excluded","unread"]},"reviewed_bytes":integer
                }
            }},
            "findings":{"type":"array","maxItems":MAX_FINDINGS,"items":{
                "type":"object","additionalProperties":false,"required":["item_id","start_byte","end_byte","category","explanation"],"properties":{
                    "item_id":id,"start_byte":integer,"end_byte":integer,
                    "category":{"type":"string","enum":["private_information","internal_information","personal_conversation","unrelated_content","private_path","other_disclosure_risk"]},"explanation":text
                }
            }},
            "pending_questions":{"type":"array","maxItems":MAX_QUESTIONS,"items":{
                "type":"object","additionalProperties":false,"required":["id","item_id","question","required"],"properties":{
                    "id":id,"item_id":id,"question":text,"required":{"type":"boolean"}
                }
            }}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> ManifestBinding {
        ManifestBinding {
            audit_id: "audit-fixture".into(),
            manifest_sha256: "a".repeat(64),
        }
    }

    fn items() -> Vec<ExpectedItem> {
        vec![
            ExpectedItem {
                id: "log-text".into(),
                kind: ExpectedKind::Text {
                    extent_bytes: 32,
                    available_bytes: 32,
                    content_complete: true,
                },
            },
            ExpectedItem {
                id: "lfs-binary".into(),
                kind: ExpectedKind::VerifiedBinary,
            },
        ]
    }

    fn wire() -> Value {
        json!({
            "binding":binding(),"status":"complete","summary":"Review finished.",
            "items":[{"id":"log-text","status":"reviewed","reviewed_bytes":32},{"id":"lfs-binary","status":"binary_excluded","reviewed_bytes":0}],
            "findings":[],"pending_questions":[]
        })
    }

    fn validate(value: &Value, expected: &[ExpectedItem]) -> Result<ValidatedReport> {
        validate_report(&serde_json::to_vec(value).unwrap(), &binding(), expected)
    }

    #[test]
    fn report_requires_exact_binding_inventory_and_extent() {
        assert!(validate(&wire(), &items()).unwrap().complete());
        let mut wrong = wire();
        wrong["binding"]["audit_id"] = json!("another-audit");
        assert!(validate(&wrong, &items()).is_err());
        let mut wrong = wire();
        wrong["binding"]["manifest_sha256"] = json!("b".repeat(64));
        assert!(validate(&wrong, &items()).is_err());
        let mut wrong = wire();
        wrong["items"].as_array_mut().unwrap().pop();
        assert!(validate(&wrong, &items()).is_err());
        let mut wrong = wire();
        wrong["items"][1] = wrong["items"][0].clone();
        assert!(validate(&wrong, &items()).is_err());
        let mut wrong = wire();
        wrong["items"][1]["id"] = json!("outside-manifest");
        assert!(validate(&wrong, &items()).is_err());
        let mut wrong = wire();
        wrong["items"][0]["reviewed_bytes"] = json!(31);
        assert!(validate(&wrong, &items()).is_err());
    }

    #[test]
    fn model_claims_cannot_create_available_content_or_binary_exclusions() {
        let mut expected = items();
        expected[0].kind = ExpectedKind::Text {
            extent_bytes: 32,
            available_bytes: 32,
            content_complete: false,
        };
        assert!(validate(&wire(), &expected).is_err());
        let mut wrong = wire();
        wrong["items"][0]["status"] = json!("binary_excluded");
        wrong["items"][0]["reviewed_bytes"] = json!(0);
        assert!(validate(&wrong, &items()).is_err());
        expected[1].kind = ExpectedKind::Unavailable;
        assert!(validate(&wire(), &expected).is_err());
        let mut wrong = wire();
        wrong["items"][1]["status"] = json!("reviewed");
        assert!(validate(&wrong, &items()).is_err());
    }

    #[test]
    fn partial_and_unavailable_items_keep_a_report_incomplete() {
        let mut expected = items();
        expected[0].kind = ExpectedKind::Text {
            extent_bytes: 32,
            available_bytes: 12,
            content_complete: false,
        };
        expected[1].kind = ExpectedKind::Unavailable;
        let mut report = wire();
        report["status"] = json!("incomplete");
        report["items"][0]["status"] = json!("partial");
        report["items"][0]["reviewed_bytes"] = json!(12);
        report["items"][1]["status"] = json!("unread");
        let accepted = validate(&report, &expected).unwrap();
        assert!(!accepted.complete());
        assert!(
            accepted
                .render()
                .contains("incomplete coverage does not establish absence")
        );
        report["status"] = json!("complete");
        assert!(validate(&report, &expected).is_err());
        report["status"] = json!("incomplete");
        report["items"][0]["reviewed_bytes"] = json!(13);
        assert!(validate(&report, &expected).is_err());
    }

    #[test]
    fn incomplete_reports_preserve_findings_within_the_reviewed_prefix() {
        let mut report = wire();
        report["status"] = json!("incomplete");
        report["items"][0]["status"] = json!("partial");
        report["items"][0]["reviewed_bytes"] = json!(12);
        report["findings"] = json!([{
            "item_id":"log-text", "start_byte":2, "end_byte":8,
            "category":"private_information", "explanation":"May identify a private customer."
        }]);
        let accepted = validate(&report, &items()).unwrap();
        assert!(!accepted.complete());
        assert!(
            accepted
                .render()
                .contains("May identify a private customer.")
        );
        report["status"] = json!("complete");
        assert!(validate(&report, &items()).is_err());
        report["status"] = json!("incomplete");
        report["findings"][0]["end_byte"] = json!(13);
        assert!(validate(&report, &items()).is_err());
    }

    #[test]
    fn pending_required_questions_cannot_be_model_marked_as_answered() {
        let mut report = wire();
        report["pending_questions"] = json!([{"id":"audience-question","item_id":"log-text","question":"Is this intended for the publication audience?","required":true}]);
        assert!(validate(&report, &items()).is_err());
        report["status"] = json!("incomplete");
        assert!(!validate(&report, &items()).unwrap().complete());
        report["pending_questions"][0]["answered"] = json!(true);
        assert!(validate(&report, &items()).is_err());
    }

    #[test]
    fn findings_are_located_advice_without_commands_or_consent() {
        let mut report = wire();
        report["findings"] = json!([{"item_id":"log-text","start_byte":3,"end_byte":8,"category":"private_information","explanation":"May identify a private customer."}]);
        let accepted = validate(&report, &items()).unwrap();
        assert_eq!(accepted.as_json().unwrap(), report);
        report["findings"][0]["end_byte"] = json!(33);
        assert!(validate(&report, &items()).is_err());
        report["findings"][0]["end_byte"] = json!(8);
        report["findings"][0]["command"] = json!("agit push");
        assert!(validate(&report, &items()).is_err());
        let mut report = wire();
        report["publication_approved"] = json!(true);
        assert!(validate(&report, &items()).is_err());
    }

    #[test]
    fn report_limits_reject_excess_before_rendering() {
        assert!(validate_report(&vec![b' '; MAX_REPORT_BYTES + 1], &binding(), &items()).is_err());
        let mut report = wire();
        report["summary"] = json!("x".repeat(MAX_TEXT_BYTES + 1));
        assert!(validate(&report, &items()).is_err());
        report["summary"] = json!("\u{e9}".repeat(MAX_TEXT_BYTES / 2 + 1));
        assert!(validate(&report, &items()).is_err());
        let mut report = wire();
        let item = report["items"][0].clone();
        report["items"] = json!(vec![item; MAX_ITEMS + 1]);
        assert!(validate(&report, &items()).is_err());
        let mut raw = serde_json::to_vec(&wire()).unwrap();
        raw.extend_from_slice(b"{}");
        assert!(validate_report(&raw, &binding(), &items()).is_err());
    }

    #[test]
    fn hostile_model_text_cannot_emit_terminal_controls_or_hidden_lines() {
        let hostile = "line\n\r\t\u{1b}[2J\u{9b}31m\u{202e}hidden\u{2066}x\u{7f}";
        let mut report = wire();
        let mut expected = items();
        expected[0].id = hostile.into();
        let mut expected_binding = binding();
        expected_binding.audit_id = hostile.into();
        report["binding"]["audit_id"] = json!(hostile);
        report["items"][0]["id"] = json!(hostile);
        report["summary"] = json!(hostile);
        report["findings"] = json!([{"item_id":"log-text","start_byte":0,"end_byte":1,"category":"private_path","explanation":hostile}]);
        report["findings"][0]["item_id"] = json!(hostile);
        report["pending_questions"] =
            json!([{"id":hostile,"item_id":hostile,"question":hostile,"required":false}]);
        let rendered = validate_report(
            &serde_json::to_vec(&report).unwrap(),
            &expected_binding,
            &expected,
        )
        .unwrap()
        .render();
        assert!(rendered.is_ascii());
        assert!(
            !rendered
                .chars()
                .any(|character| character.is_control() && character != '\n')
        );
        assert!(!rendered.contains(hostile));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\u{202e}"));
    }

    #[test]
    fn schema_exposes_only_the_bounded_report_protocol() {
        let schema = report_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["items"]["maxItems"], MAX_ITEMS);
        assert_eq!(schema["properties"]["findings"]["maxItems"], MAX_FINDINGS);
        assert_eq!(
            schema["properties"]["pending_questions"]["maxItems"],
            MAX_QUESTIONS
        );
        assert!(schema["properties"].get("publication_approved").is_none());
    }
}
