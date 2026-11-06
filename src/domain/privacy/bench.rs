//! Offline detector telemetry uses the same immutable policy as production projection.

use super::{
    detector::Entropy,
    policy::{Literal, Snapshot, Source},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Write};

const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    operation: Operation,
    #[serde(default)]
    text: String,
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default)]
    allows: Vec<String>,
    #[serde(default)]
    entropy: bool,
    #[serde(default)]
    policy: Option<super::PrivacyPolicy>,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Capabilities,
    ScanText,
    MemoryPolicy,
}

fn evaluate(request: Request) -> crate::Result<Value> {
    anyhow::ensure!(request.schema_version == 1, "unsupported bench schema");
    if matches!(request.operation, Operation::Capabilities) {
        return Ok(
            json!({"schema_version": 1, "capabilities": ["scan_text", "memory_policy"],
            "offset_unit": "utf8_bytes", "external_dependencies": false}),
        );
    }
    if matches!(request.operation, Operation::MemoryPolicy) {
        let policy = request.policy.unwrap_or_default();
        policy.validate()?;
        anyhow::ensure!(request.paths.len() <= 1024, "too many memory candidates");
        let decisions: Vec<_> = request
            .paths
            .iter()
            .map(|path| policy.evaluate_memory(path, None))
            .collect();
        return Ok(json!({"schema_version": 1, "status": "complete", "decisions": decisions}));
    }
    anyhow::ensure!(
        request.blocks.len() + request.allows.len() <= 1024,
        "too many rules"
    );
    let blocks: Vec<_> = request
        .blocks
        .iter()
        .map(|value| Literal {
            value,
            source: Source::GlobalUser,
        })
        .collect();
    let allows: Vec<_> = request.allows.iter().map(String::as_str).collect();
    let policy = Snapshot::compile(&blocks, &allows, true)?.with_entropy(if request.entropy {
        Entropy::Include
    } else {
        Entropy::Exclude
    });
    let batch = policy.scan(&request.text, 1024);
    let findings: Vec<_> = batch
        .findings
        .iter()
        .map(|finding| {
            json!({
                "start": finding.start, "end": finding.end, "rule": finding.rule,
                "source": match finding.source {
                    Source::Default => "default",
                    Source::GlobalUser => "global_user",
                    Source::RepositoryUser => "repository_user",
                },
            })
        })
        .collect();
    Ok(
        json!({"schema_version": 1, "status": if batch.complete { "complete" } else { "partial" },
        "offset_unit": "utf8_bytes", "findings": findings}),
    )
}

/// Dispatch before configuration, login, Git initialization or telemetry can read device state.
pub fn entry(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.get(1)?.to_str()? != "__privacy-bench-v1" {
        return None;
    }
    let result = (|| -> crate::Result<Value> {
        anyhow::ensure!(args.len() == 2, "unexpected bench arguments");
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= LIMIT,
            "bench request exceeds its budget"
        );
        evaluate(serde_json::from_slice(&bytes)?)
    })();
    let (code, value) = match result {
        Ok(value) => (0, value),
        Err(_) => (
            4,
            json!({"schema_version": 1, "status": "failed", "error": "invalid or unsupported bench request"}),
        ),
    };
    let written = writeln!(std::io::stdout().lock(), "{value}");
    Some(if written.is_ok() { code } else { 4 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_ranges_preserve_byte_coordinates_and_explicit_rule_priority() {
        let request: Request = serde_json::from_value(json!({"schema_version": 1,
            "operation": "scan_text", "text": "é bench-value tail",
            "blocks": ["bench-value"], "allows": ["bench-value"]}))
        .unwrap();
        let result = evaluate(request).unwrap();
        assert_eq!(result["status"], "complete");
        assert_eq!(result["findings"][0]["start"], 3);
        assert_eq!(result["findings"][0]["end"], 14);
        assert_eq!(result["findings"][0]["source"], "global_user");
        let unsupported: Request = serde_json::from_value(json!({"schema_version": 2,
            "operation": "capabilities"}))
        .unwrap();
        assert!(evaluate(unsupported).is_err());
    }
}
