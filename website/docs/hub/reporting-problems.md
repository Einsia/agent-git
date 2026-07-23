---
sidebar_position: 8
title: Reporting problems
---

# Reporting problems

When something goes wrong on the hub, one identifier makes it findable: the request id. Every hub response
carries an `X-Request-Id` header, and every JSON error body folds the same id in as a `request_id` field.
Include it in a report and an operator can find the matching entry in the server log without any access to
your account or session.

## The request id

The hub mints a fresh 16-character id for every request and stamps it on the response, success or error
alike. A client-supplied id is ignored; the server always mints its own. That id ties the request
end-to-end in the server log (auth event, handler, timing), so it is the single most useful thing to quote.

You will find it in:

- the `X-Request-Id` response header on any request, and
- the `request_id` field inside a JSON error body (status 400 and up).

## Report a problem from the web UI

The web UI's **Report a problem** collects the context an operator needs and nothing sensitive:

- your browser and the current page,
- the recent failed requests, each with its request id, and
- any console errors.

Copy or download the report and send it. Because it already carries the request ids, an operator can
correlate it against the server log directly.

## Report a failed push or pull

A push or pull that fails from the client is a hub connectivity problem, and `agit debug` captures it: its
`connectivity.txt` section records the request id the hub returned. Collect a bundle with `agit debug`
(it writes a timestamped directory; `--out <dir>` names your own), review it (everything is redacted
through the secret scanner before it is written, and nothing is uploaded), and attach the directory. See
[Diagnostics](../cli/diagnostics.md).

## Where to send it

Open an issue at [github.com/Einsia/agent-git/issues](https://github.com/Einsia/agent-git/issues). For a
hub problem, include the Report a problem contents and the request id; that pair is usually enough to
locate the failure. A one-line suggestion is welcome too and does not need to be a full report.

If you run the hub yourself, the request id is also the key into your own logs and metrics; see
[Operations](../self-hosting/operations.md).
