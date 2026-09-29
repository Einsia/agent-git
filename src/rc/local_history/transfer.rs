//! Only an explicit request opts into a compressed, already-redacted history page.

use anyhow::ensure;
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::io::Write;

pub(super) const ENCODING: &str = "gzip-base64-v1";
const COMPRESS_AT: usize = 32 * 1024;

pub(super) fn encode(page: Value) -> crate::Result<Value> {
    let bytes = serde_json::to_vec(&page)?;
    ensure!(
        bytes.len() <= agit_peer::MAX_FRAME_BYTES,
        "History page exceeds the transport limit"
    );
    if bytes.len() < COMPRESS_AT {
        return Ok(page);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&bytes)?;
    let encoded = json!({
        "history_encoding":ENCODING,
        "decoded_bytes":bytes.len(),
        "data":STANDARD.encode(encoder.finish()?),
    });
    Ok(if serde_json::to_vec(&encoded)?.len() < bytes.len() {
        encoded
    } else {
        page
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn compression_preserves_complete_tool_results_and_pagination() {
        let page = json!({"items":[{"item_id":"result", "event":{
            "call_id":"call", "output":"tool output\n".repeat(COMPRESS_AT)
        }}], "before":64,"has_more":true,"snapshot":"pinned"});
        let encoded = encode(page.clone()).unwrap();
        assert_eq!(encoded["history_encoding"], ENCODING);
        let compressed = STANDARD.decode(encoded["data"].as_str().unwrap()).unwrap();
        let mut decoded = Vec::new();
        GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(
            decoded.len(),
            encoded["decoded_bytes"].as_u64().unwrap() as usize
        );
        assert_eq!(serde_json::from_slice::<Value>(&decoded).unwrap(), page);
        let small = json!({"items":[],"before":0,"has_more":false});
        assert_eq!(encode(small.clone()).unwrap(), small);
        assert!(encode(json!({"items":"x".repeat(agit_peer::MAX_FRAME_BYTES)})).is_err());
    }
}
