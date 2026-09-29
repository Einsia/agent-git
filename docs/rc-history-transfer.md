# Native history transfer

`session.history` accepts the optional request field
`response_encoding: "gzip-base64-v1"`. Without this exact opt-in, the executor
returns its ordinary history page. An older executor can ignore the field and
return the ordinary page; callers must accept that response.

An opted-in executor may return a compressed result envelope:

```json
{
  "history_encoding": "gzip-base64-v1",
  "decoded_bytes": 123456,
  "data": "<standard base64 of a gzip stream>"
}
```

The decompressed bytes are the complete UTF-8 JSON history page, including item
identities, full tool results, the pinned snapshot, and pagination fields.
Compression runs after history authorization, projection, and redaction. It is
transport encoding only; it creates no separate archive or stored checkpoint.

Both the encoded envelope and decompressed page remain bounded by the existing
peer frame limit. Readers must bound decompression, verify the declared byte
count, and reject unknown encodings or malformed payloads. Small pages or pages
whose envelope would be larger retain their ordinary JSON representation.
