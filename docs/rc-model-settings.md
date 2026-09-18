# RC model and effort settings

The executor owns model selection. Controllers forward the existing
`session.model` and `session.setModel` RPCs to the process that owns the session;
they do not restart a runtime, edit its configuration files, or acquire another
native writer.

`session.model` returns the current `model`, `effort`, `effort_known`, optional
`selected_model` alias, `pending`, `models`, `efforts`, and `capabilities`.
Capabilities explicitly advertise model/effort writes and each reset operation.
An absent effort value with `effort_known: false` means unknown, not default.
`settings_unknown: true` means a native acknowledgement is still outstanding.

`session.setModel` accepts `session_id` and optional `model` and `effort` fields.
An omitted field remains unchanged. Null restores an advertised default; an
empty string accepts the same reset intent for existing controllers. Changing a
model chooses its compatible default effort instead of carrying an incompatible
override. Changes are refused while a turn is running. The next message uses
the confirmed setting without losing conversation context.

- Codex reads `model/list` and project `config/read`. Changes remain in `pending`
  until native turn acceptance, and travel in both `turn/start.effort` and
  `collaborationMode.settings.reasoning_effort`. Native opening/resume replies
  supply current settings; immutable launch intent is not replayed on resume.
- Claude Code reads its initialization model catalog, changes the model through
  `set_model`, and changes effort through `apply_flag_settings.effortLevel`.
  Each write waits for the matching native response. Model aliases stay distinct
  from resolved provider IDs. A resumed effort that the runtime does not report
  remains unknown. Older runtimes without advertised effort choices retain model
  selection and ordinary messaging.
- OpenCode reads the active ACP session's `configOptions`, including grouped
  models and native effort/variant names. `session/set_config_option` returns the
  new choices after each change. Change the model before choosing its effort.
  A native `default` effort is offered when advertised; ACP does not advertise a
  model reset. No discovery-only session is created.

After a settings mutation, the supervisor broadcasts a `session.model`
invalidation to session viewers, including after partial native failure. The
controller rereads state on that event, turn boundaries, reconnect, or focus.
Model discovery and settings failures do not disable message delivery.

## Local validation

In the CLI repository:

```sh
cargo build --bin agit
cargo test --lib rc::harness::
cargo test --lib rc::daemon::tests::metadata
```

In the backend repository:

```sh
cargo build --bin agentgit-backend --bin agitd-controller
cd website
npm run typecheck
npm test -- src/features/remote-control/WebConversation.test.tsx src/routes/WorkspaceLive.test.tsx
```

For a full local stack, run the backend with an isolated data root and
`--peer-relay true`. Set its existing `AGIT_BACKEND_PUBLIC_URL` to the browser
origin, for example `http://127.0.0.1:5181`; the WebSocket origin check requires
an exact match. Run Vite with `AGIT_MOCK_HUB=0 AGIT_SPA_STANDALONE=1` and the
existing `AGITHUB_BACKEND` proxy pointing at the backend. The controller binary
must be next to the backend executable.

Use a separate `AGIT_HOME` to log the development CLI into that local Hub,
enroll it, and run `rc local start`. Put the intended runtime binaries on the
daemon's PATH. Do not replace a daemon that owns unrelated live sessions.
Sign into the same local account in the browser, bind a test folder, and create
one conversation per runtime.

1. Select a model and an advertised effort; send a short prompt without tools.
2. Verify the reply and send another message that refers to the first one.
3. Restore an advertised default and repeat. During a running turn, the settings
   controls are disabled while send/steer and interrupt retain their runtime behavior.
4. Open another tab, change settings, and verify the other tab refreshes. Reload,
   reconnect, and resume an inactive test session; current native settings must
   not revert to an old launch override.
5. A refused or unavailable setting must leave the conversation usable. An
   unconfirmed write displays an unknown outcome until a subsequent read observes
   the native acknowledgement.

OpenCode can use an isolated local OpenAI-compatible synthetic provider for this
test. Configure model variants with distinct `reasoningEffort` values and record
the synthetic provider's requests to verify the model and effort that actually
reach inference. This validates transport and adapter behavior without claiming
that any external provider accepts those settings.
