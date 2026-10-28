# Privacy service budgets

Every byte agit commits, publishes or sends to a remote-control viewer passes through the local
privacy worker first. This document says how long callers wait for that worker and what happens
to input it did not protect in full.

## Budgets

- **Start-up.** A new worker loads the repository's dictionary and policy, then answers a
  `Prepare` request. Start-up has its own deadline. A caller's budget starts once the worker is
  ready, so a large allowlist or journal never costs the caller its budget.
- **Live.** Remote-control text, JSON and streams that a viewer is waiting for get a short
  budget.
- **Durable.** Settlement, import, push preparation, memory collection and share or export get a
  budget of seconds that grows with the input.
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

`source_incomplete`, `device_rules` and `legacy_rules` content is handled like unprocessed input,
because a value the user blocked may be left in it, and push publishes the branch as stored. For
`device_rules`, settlement, `agit status` and `agit doctor` name the account and Hub of the
unreadable rules: signing in to that account once caches its key and makes them readable again.

## Units larger than one request

A request to the worker carries at most `MAX_UNIT_BYTES`. Larger units are divided before they
are sent, so a large record never holds a session back:

- **Media is opaque.** A base64 media payload, the `data` of a base64 source or a
  `data:…;base64,` URI, is never scanned or replaced, in any mode: encoded bytes carry no readable
  credential, and a placeholder in them only corrupts the image, document or font.
- **A JSONL line too large for one request** keeps its media payloads on the caller's side, and
  every other string too large to travel with the record is protected on its own. The record
  then goes to the worker with short stand-ins in their place, and the parts are put back. The
  line keeps its bytes unless something in it was replaced. A line that is still too large with
  every such string set aside, because it holds only short strings, is `oversize`: it cannot be
  divided, and its turn is refused by size rather than left pending.
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
| RC live: text (`scrub`) | marker "[privacy check pending: text withheld]"; never raw | same | same | same | same | same | same | the next event carries new text; a withheld prompt or error is not re-sent |
| RC live: JSON and native records | `{"protection_error": …}` marker; raw dropped | same | same | same | same | same | same | live tail: lines from the first withheld record are held and projected again with the next polls, and the cursor never moves past an unprotected record. The first one's marker shows once it has waited `HELD_TAIL_NOTICE`, and before a turn boundary, while it is still retried. It is given up alone, with the given-up marker, only after its own check failed with a deadline or a protocol error `HELD_TAIL_GIVE_UP_FAILURES` times over `HELD_TAIL_GIVE_UP`; lock contention and back-offs never give a line up (`HELD_TAIL_BYTES` bounds the wait). A given-up line never shows raw text, and history serves it once its protection passes |
| RC live: stream | held up to 1 MiB, released once processed Complete; otherwise the rest of the item is withheld behind one marker | same | withheld at once, with the rest of the item | same as deadline | same as deadline | same as deadline | same as deadline | the completed item arrives through the tail (row above) and is retried there |
| RC history page | page refused as busy; the cursor does not advance | same | same | same | same | same | same | the viewer requests the same page again; once the page has been refused for `HELD_TAIL_NOTICE`, it is returned with the given-up marker for each record that did not pass, and the next request for it projects it again |
| RC session-list labels (gist = opening prompt, native title) | each label shows the marker; never raw | same | same | same | same | same | same | the next listing protects them again |
| settle (Stop hook, `agit commit`) | commits the turn-aligned processed prefix; the remainder waits for the next settle | same | only a unit that cannot be divided (above): the turn holding it is refused by the unit's size, in the output and in `agit status`, with `agit project drop` as the way out, since settling again meets the same unit | same as deadline | nothing from that increment is committed; waits, shown in `agit status` with the cause | does not occur: the field is replaced whole | same as deadline; shown in `agit status` | every Stop and every `agit commit` retries on the normal cadence, with no growing back-off |
| settle: metadata, milestone, message | that settlement waits; nothing is committed | same | same | same | same | same | same | as settle |
| import | as settle; import reports how many turns are pending, and the next settle drains them | same | same | same | same | same | same | as settle |
| push preparation: branch name | in-process detectors, then the worker for registered values: one retry after back-off, then a retryable refusal; never published unchecked | same | same | same | refused, retryable | same | same | the next push |
| push preparation: plain publication | publishes only committed turns, which already passed (rows above); no transform runs | – | – | – | – | – | – | – |
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
