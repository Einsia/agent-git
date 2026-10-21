//! Shared queues retain the native executor and reconcile positive delivery evidence.

use super::*;
use crate::rc::native_queue::{Claim, Request};

impl CodexDriver {
    pub async fn enqueue(&mut self, request: Request) -> crate::Result<Value> {
        anyhow::ensure!(
            self.proc.shared(),
            "shared queue requires a native server attachment"
        );
        anyhow::ensure!(
            self.thread_id.as_deref() == Some(&request.native_id),
            "queue native identity changed"
        );
        anyhow::ensure!(
            self.pending_mode.is_none() && self.pending_model.is_none(),
            "apply or clear pending RC settings before queueing native work"
        );
        let registry = crate::rc::runtime_sources::Registry::open()?;
        let context = crate::rc::runtime_context::RuntimeContext::resolve(
            &registry,
            &request.source.source_id,
        )?;
        // The attached server's home, not the generation saved when it attached, proves the store.
        anyhow::ensure!(
            self.source
                .as_ref()
                .is_some_and(|runtime| runtime.home() == context.source.home),
            "runtime source changed before queue delivery"
        );
        let thread = context.locate(
            &request.native_id,
            &crate::rc::policy::CanonicalRoots::from_untrusted([self.cwd.clone()]),
        )?;
        anyhow::ensure!(
            thread.cwd == self.cwd,
            "native conversation directory changed before queue delivery"
        );
        crate::rc::native_inbox::verify_transcript(&thread.transcript, &request.native_id)?;
        let mut claim = request.claim()?;
        if !claim.fresh {
            if claim.receipt.status == "unknown" {
                self.reconcile_queue(&request, &mut claim, &thread.transcript)
                    .await?;
            }
            return Ok(claim.reply(&request.client_id));
        }
        claim.receipt.log_path = Some(thread.transcript.clone());
        claim.receipt.log_cursor = std::fs::metadata(&thread.transcript)?.len();
        claim.save()?;
        // Native add may start work before its response arrives. The durable claim
        // precedes this write; neither a timeout nor a disconnect permits another add.
        let result = self
            .command_request(
                "thread/queue/add",
                json!({
                    "threadId":request.native_id,"clientUserMessageId":claim.receipt.client_id,
                    "input":[{"type":"text","text":request.message(),"text_elements":[]}]
                }),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if error
                    .downcast_ref::<super::commands::NativeCommandRefusal>()
                    .is_some()
                {
                    claim.receipt.status = "rejected".into();
                    claim.save()?;
                }
                return Err(error);
            }
        };
        let item = &result["queuedSubmission"];
        anyhow::ensure!(
            item["clientUserMessageId"] == claim.receipt.client_id,
            "native queue acknowledgement did not match this operation; delivery is unknown"
        );
        let id = item["id"]
            .as_str()
            .filter(|id| crate::rc::native_inbox::valid_id(id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "native queue acknowledgement omitted its identity; delivery is unknown"
                )
            })?;
        claim.receipt.queue_id = Some(id.into());
        claim.receipt.status = "queued".into();
        claim.save()?;
        Ok(claim.reply(&request.client_id))
    }

    async fn reconcile_queue(
        &mut self,
        request: &Request,
        claim: &mut Claim,
        transcript: &std::path::Path,
    ) -> crate::Result<()> {
        let mut cursor = json!(claim.receipt.queue_cursor);
        for _ in 0..8 {
            let page = self
                .command_request(
                    "thread/queue/list",
                    json!({"threadId":request.native_id,"limit":100,"cursor":cursor}),
                )
                .await?;
            if let Some(item) = page["data"].as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| item["clientUserMessageId"] == claim.receipt.client_id)
            }) {
                claim.receipt.queue_id = item["id"].as_str().map(str::to_owned);
                claim.receipt.status = "queued".into();
                return claim.save();
            }
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() {
                break;
            }
        }
        claim.receipt.queue_cursor = cursor.as_str().map(str::to_owned);
        claim.reconcile_log(transcript, &request.native_id)
    }
}
