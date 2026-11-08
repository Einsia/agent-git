# Privacy service budgets

Every byte agit commits or publishes passes through the local privacy worker first. This document
says how long callers wait for that worker and what happens to input it did not protect in full.

Remote control is not on this path. A session is its owner's own, shown live to them or to
colleagues in their workspace, so its records, deltas, approval cards, history pages and
session-list labels go to its viewers as they are. What is kept where others can read it leaves
through settlement, push or export, which check it in full.

## Budgets

- **Start-up.** A new worker loads the repository's dictionary and policy, then answers a
  `Prepare` request. Start-up has its own deadline. A caller's budget starts once the worker is
  ready, so a large allowlist or journal never costs the caller its budget.
- **Live.** Text and JSON values protected one at a time (`Redactor::scrub` and its fallible
  forms) get a short budget. A live caller that finds another caller in this process using the
  worker waits up to `LIVE_LOCK_WAIT` for it before it reports `lock_busy`. Durable callers do not
  wait: each has its own retry.
- **Durable.** Settlement, import, push preparation, memory collection and share or export get a
  budget of seconds that grows with the input.
  Privacy publication protects decoded fields under this budget, including `export --privacy`.
  A refused export reports the worker's reason and leaves an existing output file untouched.
  Local fallback applies the same repository and mandatory policy; a worker failure cannot
  switch publication to default rules.
- **Restoration.** Ordinary hydration uses the durable cap because a small placeholder can
  expand into a large original. Live callers retain their live budget.
- **Deadlines.** A deadline is a normal outcome. The caller stops sending at a batch boundary;
  the worker finishes the request it holds, and its late reply is recognized by its request id
  and discarded. Only a failure to spawn the worker or a malformed reply ends the worker and
  starts a back-off.

## Outcomes

`Outcome.unprocessed` counts the bytes at the end of the content that the worker did not process.
`Outcome.reason` says why an outcome is not complete:

| reason | meaning |
|---|---|
| `deadline` | the caller's budget ran out first |
| `lock_busy` | another caller in this process was using the worker |
| `backoff` | the worker failed recently and is not started again yet |
| `spawn_failed` | the worker could not start, or was not ready within its start-up deadline |
| `protocol_error` | the worker's reply was missing or malformed |
| `oversize` | one unit was larger than the worker accepts and could not be divided |
| `store_busy` | the worker could not read or write its dictionary store |
| `source_incomplete` | the worker applied only the part of the policy this device could load |
| `findings_cap` | reserved: a field whose scan cannot finish is now replaced whole (below) |
| `device_rules` | a rule this device registered under another account or Hub could not be read |
| `legacy_rules` | the repository's legacy secret dictionary could not be read whole |
| `unsupported_scalar` | a numeric or boolean data value matches a blocked literal and cannot use a string-only recovery mapping |

`source_incomplete`, `device_rules` and `legacy_rules` content is handled like unprocessed input,
because a value the user blocked may be left in it, and push publishes the branch as stored. For
`device_rules`, settlement, `agit status` and `agit doctor` name the account and Hub of the
unreadable rules: signing in to that account once caches its key and makes them readable again.
Blocked numeric and boolean data values refuse capture with an explanation; change the data to
a string or review the rule before retrying. Encoded JSON strings can use the existing
whole-carrier mapping, preserving their exact scalar types without a storage format change.
Native control flags and counters retain their schema semantics.

## Units larger than one request

A request to the worker carries at most `MAX_UNIT_BYTES`. Larger units are divided before they
are sent, so a large record never holds a session back:

- **Ordinary restoration** sends saved events and receives their originals in bounded pages.
  The worker applies native schema and encoded-carrier rules to the complete event, using one
  dictionary snapshot for the hydration pass. Input follows `MAX_EVENT_BYTES`; expanded output
  follows the remaining `MAX_MATERIALIZED_BYTES` budget. Serialization uses an anonymous
  temporary file that disappears when closed, including when the worker exits, without a named
  plaintext recovery file. Missing mappings or expansion beyond the budget leave unresolved
  placeholders and a partial outcome. Frozen publication hydration also pages oversized events,
  using its retained dictionary, protocol identity policy and set of protected originals.

- **Media is opaque.** A base64 media payload, the `data` of a base64 source or a
  `data:…;base64,` URI, is never scanned or replaced, in any mode: encoded bytes carry no readable
  credential, and a placeholder in them only corrupts the image, document or font.
- **A JSONL line too large for one request** keeps its media payloads on the caller's side, and
  every other string too large to travel with the record is protected on its own. The record
  then goes to the worker with short stand-ins in their place, and the parts are put back. The
  line keeps its bytes unless something in it was replaced. A line that is still too large with
  every such string set aside, because it holds only short strings, is `oversize`: it cannot be
  divided, and its turn is refused by size rather than left pending.
  Encoded JSON strings retain the native projector's decoded-field check. If fitting a record
  would require removing such a carrier, the record is refused as `oversize`: plain-text
  windows cannot establish that escaped field names and values were inspected. Media remains
  opaque under its existing schema rules.
- **Recovered fork envelopes** apply that JSONL protection to native content and recompute the
  envelope's content address before storage. An indivisible record is refused with a size
  explanation; repeating the same input is not presented as a remedy.
- **Text too large for one request** (a memory file, a long string from such a line) is
  protected window by window. Every window carries the text before it as context, so a rule
  whose name or keyword ended the previous window still recognizes its value. A window ends
  where the worker's stream cut splits no finding, and the last window is protected whole. A
  string that no window can divide, with no cut point in a whole request, is replaced by
  "[content omitted: too large for the privacy check]".
- **A field whose scan cannot finish**, because it holds more findings than one pass replaces,
  is replaced whole as one placeholder. It is Complete, and nothing in it is left raw.

## What happens to input that was not protected

| caller | deadline | lock_busy / backoff | oversize (single unit beyond the limit) | store_busy | source_incomplete | findings_cap | spawn_failed / protocol_error | retry |
|---|---|---|---|---|---|---|---|---|
| start-up (worker not ready yet) | Live callers show pending (the marker), never raw. Durable callers wait for ready within the start-up deadline of seconds, then their per-call budget starts | – | – | – | – | – | start-up fails: as spawn_failed | each caller's own retry, below |
| settle (Stop hook, `agit commit`) | commits the turn-aligned processed prefix; the remainder waits for the next settle | same | only a unit that cannot be divided (above): the turn holding it is refused by the unit's size, in the output and in `agit status`, with `agit project drop` as the way out, since settling again meets the same unit | same as deadline | nothing from that increment is committed; waits, shown in `agit status` with the cause | does not occur: the field is replaced whole | same as deadline; shown in `agit status` | every Stop and every `agit commit` retries on the normal cadence, with no growing back-off |
| settle: metadata, milestone, message | that settlement waits; nothing is committed | same | same | same | same | same | same | as settle |
| import | as settle; import reports how many turns are pending, and the next settle drains them | same | same | same | same | same | same | as settle |
| push preparation: branch name | in-process detectors, then the worker for registered values: one retry after back-off, then a retryable refusal; never published unchecked | same | same | same | refused, retryable | same | same | the next push |
| push preparation: plain publication | every record and commit message the push adds passes protection again; a line the Hub already holds does not. Any reason refuses with nothing sent, and `agit status` shows the target, the reason and what ends the wait until a push of it lands | same | same | same | same | same | same | the next push |
| memory (collection, distill) | file withheld, retried next sync; never routes around `passed()` | same | same | same | same | same | same; `not_utf8` is withheld as well | the next sync or settle |
| share / export (S) | refused for that record, retryable; never falls back to `PrivacyPolicy::default()` | same | same | same | same | same | same | the user runs it again |

No caller sends, commits or publishes input the worker did not protect. Settlement makes progress
on every attempt: it commits the turns the worker covered and leaves the rest for the next
settlement, and `agit status` shows a settlement that is waiting, with its cause.

## Deliberate non-publication egress

`fs.readFile` (`rc/daemon/dispatch.rs`) sends the raw bytes of a workspace file to the
remote-control client that asked for them. The device requires the operator role for it, so
viewers are refused, and it reads only inside the workspace's bound folders. An operator can
already run commands in those folders, so reading a file grants nothing new. It never reaches a
commit or a publication.
