# RC message attribution

The Hub resolves the sender when it accepts a viewer command. It replaces caller-supplied
`by`, `sender`, and `caller` values with authenticated connection metadata. The daemon derives
`sender: {account_id, username}` exclusively from `caller.account_id` and `caller.username`.
The account ID identifies the person; the username is the display snapshot at submission time.
Request parameters cannot override that identity.

`session.start` and `session.resume` preserve the sender with the initial prompt until the
native runtime accepts it. `turn.start` additionally retains `client_msg_id`. The resulting
`turn.started` event contains `turn_id`, `source`, `prompt`, `by`, optional `sender`, and optional
`client_msg_id`, including when native acceptance arrives before its turn-start notification.

After the native driver accepts `turn.steer`, the daemon emits `turn.steered` with `message`,
`delivery`, `by`, optional `sender`, and optional `client_msg_id`. The message receives the same
outbound redaction as other session text. A refused or unresolved steer does not produce an
accepted-message event. `at_tool_boundary` means the driver queued the message for delivery;
it does not claim the model has already consumed it.

Both events use the session's ordered journal and replay path. Viewers can correlate native
user-message echoes with accepted command events while retaining other local user messages.
The attribution is RC event metadata; it does not modify the native harness transcript or
inject sender text into the model prompt. Journal retention and Hub event retention govern
its availability when reopening history.

## Stopping and closing

`session.stop {session_id}` ends agit's hold on a live session. A harness that agit launched
exits; a session attached from its owner's own terminal is only released, never killed. The
session can be resumed later. `session.close` does the same and also stops this machine's
collection of the session's native transcript. An operator or the owner may stop a session;
only the owner may close it. Both are admitted, like `turn.interrupt`, on sessions running in a
dangerous permission mode. Closing a session that is not running here is refused and leaves its
collection unchanged. A close saves the release before it stops the session; if the save fails,
the session keeps running, the close can be retried, and the answer says whether this computer's
collection is unchanged or, when that cannot be confirmed, that it is uncertain.

The additions are optional JSON fields. Older Hubs retain their existing `by` display label
without an authenticated sender snapshot. Older daemons can ignore the new caller metadata;
a viewer must not label unattributed native messages as the current viewer's messages.

Initial-prompt retry coalescing requires matching text and sender identity. A different
member's identical message cannot attach to, consume, or drain the creator's pending prompt
or receipt. An account rename preserves retry identity; reuse of a display name does not.
Legacy claims match by their nonempty `by` label only when neither claim has a sender snapshot.
