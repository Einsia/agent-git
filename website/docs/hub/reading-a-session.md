---
sidebar_position: 5
title: Reading a session
---

# Reading a session

An agent page lists its sessions. Open one to read it. The session view is the point of the hub: it turns
a transcript into something a person can read and judge, keeping the reasoning, not just the diff.

## The conversation

The body of the view is the ordered conversation: the user and assistant turns interleaved in the order
they happened, rather than flattened into two separate lists. Each turn's text is rendered as markdown, and
markdown is sanitized before it is displayed, so a transcript cannot inject active content into the page.

Each turn shows:

- its **role** (user or assistant),
- its **text**, rendered as markdown, and
- a **tool-call count** for the tools the turn invoked, so you can see where the agent reached for the
  environment without wading through raw tool payloads.

Very long sessions are capped for the view; when the conversation is truncated the page marks it, and the
full history is always available by cloning the agent's store.

## The spine

Alongside the conversation the view shows the session's spine: a compact, ordered summary of the event
sequence (prompts, replies, tool calls, edits). It is built from the same event walk as the conversation,
so it reads the shape of a session at a glance before you read the words.

## Provenance badge

The view carries a provenance badge, the cryptographic verdict on who produced the session and whether it
is intact. It reads one of:

- **verified as `<person>`**: the signature checks out and the committer email maps to a registered
  account with a matching device key.
- **verified**: the signature and content are intact and the key is known, but not attributed to a hub
  account.
- **signed, unregistered**: signed and intact, but the signing key is not enrolled with the hub.
- **unsigned**: no signature. Not an error; provenance degrades gracefully.
- **key mismatch**: the committer email claims an account, but the signing key is not one of that
  account's keys. A possible forgery, and never shown as verified.
- **content tampered** or **bad signature**: the transcript does not match what was signed.

To make your own sessions read as `verified as you`, enroll a device key and verify your email. See
[Signing keys](./signing-keys.md) and [Verify who produced a session](../integration/provenance.md).

## Files and provenance facts

The view also shows the session's facts drawn from its commits: the runtime, model, branch, working
directory, author, and time, plus the **files** the session touched. These are what you weigh when
deciding whether a session is worth pulling or merging.

## Revisions

A session accumulates revisions as it is captured and re-pushed. The view lists them (each with its commit,
time, and subject) and lets you pin the transcript to a specific revision to read the session as it was at
that point. The provenance verdict is computed at the same revision you are reading, so a badge always
describes the bytes on screen.

## Compare two revisions

From the agent view you can compare two points in its history. The comparison is semantic: it shows the
instructions, files, and conclusions added and removed between the two revisions, not the raw transcript
bytes. This is the read-only counterpart to reconciling diverged work locally; see
[Divergence](../cli/divergence.md) and [Reconcile diverged sessions](../cli/merging.md).

## Related

- [Signing keys](./signing-keys.md): unlock the `verified as` badge on your own sessions.
- [Verify who produced a session](../integration/provenance.md): the provenance model in full.
- [Tokens](./tokens.md): the read credential a script uses to pull a session's raw bytes.
