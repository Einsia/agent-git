//! Interactive review returns advisory evidence; publication consent belongs to the caller.

use super::audit_report::{self, MAX_REPORT_BYTES};
use super::audit_workspace::AuditWorkspace;
use crate::commands::scan::{ForegroundExit, ForegroundReview};
use crate::hub::git::CapturedPublication;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const INPUT_CHUNK_BYTES: usize = 8 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_WORKFLOW_BYTES: usize = 256 * 1024;
const WORKFLOW: &str = include_str!("audit-workflow.md");

pub(super) struct ReviewResult {
    _workspace: AuditWorkspace,
    complete: bool,
}

impl ReviewResult {
    pub(super) fn complete(&self) -> bool {
        self.complete
    }
}

pub(super) fn review(captured: &CapturedPublication, destination: &Value) -> Result<ReviewResult> {
    crate::ui::info("Preparing the complete frozen publication for sensitivity review...");
    let workspace = AuditWorkspace::prepare(captured, destination)?;
    let mut inputs = Inputs::new(workspace.path());
    let inventory = inputs.json_parts("publication", workspace.manifest())?;
    let access = inputs.json_parts("read-access", workspace.read_access())?;
    let schema = audit_report::report_schema();
    let contract = json!({
        "report_path": workspace.path().join("audit-report.json"),
        "binding": workspace.binding(),
        "schema": schema,
        "maximum_report_bytes": MAX_REPORT_BYTES,
        "instructions": "Write exactly one JSON report file matching this schema. Include every manifest item exactly once. A successful process exit or answers inside this review do not authorize publication."
    });
    let workflow = render_workflow([
        (
            "{{AUDIT_BINDING_JSON}}",
            json!({
                "binding": workspace.binding(),
                "intended_destination": destination,
                "publication": captured.plan()
            }),
        ),
        ("{{PUBLICATION_MANIFEST_JSON}}", inventory),
        ("{{READ_ACCESS_JSON}}", access),
        ("{{REPORT_CONTRACT_JSON}}", contract),
    ])?;
    let workflow_path = inputs.write("workflow.md", workflow.as_bytes())?;
    let opening = format!(
        "Read the entire trusted workflow at this JSON-quoted path: {}. \
         Its metadata descriptors list bounded input parts; read every part in order. \
         Use the supplied immutable agit show argv for full LOG and review the complete \
         carrier exports as directed. Show progress and ask the user when clarification \
         is needed. Once the report file is ready, tell the user to exit this reviewer \
         normally to return to the parent publication confirmation.",
        serde_json::to_string(&workflow_path)?
    );
    let launcher = ForegroundReview::prepare(workspace.path(), &opening)?;
    workspace.verify()?;
    inputs.verify()?;
    crate::ui::info(format_args!(
        "Opening interactive sensitivity review (session {}).",
        launcher.session_id()
    ));
    crate::ui::info(
        "Answer the reviewer's questions in its interface. When the report is ready, exit normally to return here; interrupting review cancels this audited push.",
    );
    let finished: ForegroundExit = launcher.run()?;
    workspace.verify()?;
    inputs.verify()?;
    let raw = read_report(finished.report_path())?;
    let report =
        audit_report::validate_report(&raw, workspace.binding(), workspace.expected_items())?;
    let validated = serde_json::to_vec_pretty(&report.as_json()?)?;
    let report_path = inputs.write("validated-report.json", &validated)?;
    crate::ui::info(format_args!(
        "Returned from reviewer session {}.",
        finished.session_id()
    ));
    println!("\n{}", report.render());
    let complete = report.complete();
    if complete {
        crate::ui::info(format_args!(
            "Full JSON report (available until this push ends): {}",
            serde_json::to_string(&report_path)?
        ));
    } else {
        crate::ui::error("The sensitivity review is incomplete; this audited push cannot publish.");
    }
    Ok(ReviewResult {
        _workspace: workspace,
        complete,
    })
}

/// Replacements apply only to trusted template spans; evidence cannot introduce placeholders.
fn render_workflow(replacements: [(&str, Value); 4]) -> Result<String> {
    let mut spans = Vec::new();
    for (marker, value) in replacements {
        let occurrences: Vec<_> = WORKFLOW.match_indices(marker).collect();
        ensure!(
            occurrences.len() == 1,
            "audit workflow marker is not unique"
        );
        let start = occurrences[0].0;
        spans.push((
            start,
            start + marker.len(),
            serde_json::to_string_pretty(&value)?,
        ));
    }
    spans.sort_by_key(|span| span.0);
    let mut result = String::new();
    let mut at = 0;
    for (start, end, value) in spans {
        ensure!(start >= at, "audit workflow markers overlap");
        result.push_str(&WORKFLOW[at..start]);
        result.push_str(&value);
        at = end;
    }
    result.push_str(&WORKFLOW[at..]);
    ensure!(
        result.len() <= MAX_WORKFLOW_BYTES,
        "audit workflow exceeds its byte limit"
    );
    Ok(result)
}

struct InputFile {
    path: PathBuf,
    bytes: u64,
    digest: String,
}

struct Inputs {
    root: PathBuf,
    files: Vec<InputFile>,
}

impl Inputs {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            files: Vec::new(),
        }
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        ensure!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte)),
            "audit input filename is invalid"
        );
        let path = self.root.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .context("cannot create a fresh audit input")?;
        file.write_all(bytes)?;
        self.files.push(InputFile {
            path: path.clone(),
            bytes: bytes.len() as u64,
            digest: hex::encode(Sha256::digest(bytes)),
        });
        Ok(path)
    }

    fn json_parts(&mut self, prefix: &str, value: &Value) -> Result<Value> {
        let text = serde_json::to_string_pretty(value)?;
        ensure!(
            text.len() <= MAX_METADATA_BYTES,
            "audit metadata exceeds its byte limit"
        );
        let mut parts = Vec::new();
        for (ordinal, (start, end)) in text_chunks(&text).into_iter().enumerate() {
            let bytes = &text.as_bytes()[start..end];
            let path = self.write(&format!("{prefix}-{ordinal:05}.json-part"), bytes)?;
            parts.push(json!({
                "path": path, "start_byte": start, "end_byte": end,
                "sha256": hex::encode(Sha256::digest(bytes))
            }));
        }
        Ok(json!({
            "encoding": "UTF-8 JSON", "bytes": text.len(),
            "sha256": hex::encode(Sha256::digest(text.as_bytes())), "parts": parts,
            "instructions": "Read every part in order. Together their exact bytes, without added separators, are the complete JSON document. Do not infer completeness from the first tool output."
        }))
    }

    fn verify(&self) -> Result<()> {
        for input in &self.files {
            let mut file = open_regular(&input.path, input.bytes)?.take(input.bytes + 1);
            let mut digest = Sha256::new();
            let mut count = 0;
            let mut buffer = [0u8; INPUT_CHUNK_BYTES];
            loop {
                let received = file.read(&mut buffer)?;
                if received == 0 {
                    break;
                }
                count += received as u64;
                digest.update(&buffer[..received]);
            }
            ensure!(
                count == input.bytes && hex::encode(digest.finalize()) == input.digest,
                "a prepared audit instruction or metadata file changed during review"
            );
        }
        Ok(())
    }
}

fn text_chunks(text: &str) -> Vec<(usize, usize)> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + INPUT_CHUNK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push((start, end));
        start = end;
    }
    chunks
}

fn open_regular(path: &Path, limit: u64) -> Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path).context("audit file is unavailable")?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "audit file is not a bounded regular file"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("cannot open the audit file")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "opened audit file is not a bounded regular file"
    );
    Ok(file)
}

fn read_report(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_regular(path, MAX_REPORT_BYTES as u64)?
        .take(MAX_REPORT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_REPORT_BYTES,
        "audit report exceeds its byte limit"
    );
    Ok(bytes)
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
