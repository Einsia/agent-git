# Conversation history view

Clients rendering a conversation can pass `view: "conversation"` to
`session.history`. The executor omits Codex `session_meta`, `turn_context`, and
`world_state`, `token_usage_record`, and `event_msg/token_count` records from this response.
System and developer context messages keep their boundaries and native IDs, but
omit their non-displayed text and content. The default view retains the
full history projection; native transcript storage is unchanged.

Filtering happens after page boundaries are selected. Retained items keep their
original identities and byte cursors. An empty page can still have
`has_more: true`; clients follow `before` to read preceding records. Tool calls,
reasoning, user and assistant messages, compaction records, and unknown record types remain available
for rendering and reconciliation. Claude history is unchanged.

Executors that do not recognize the optional view return their ordinary pages.
Clients accept those pages without capability negotiation or a minimum CLI version.
