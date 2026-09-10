# Experience completion evidence

The checklist follows the original human CLI, agent CLI, TUI, Settings, and search
request. It describes the integrated implementation and the evidence used
to verify it; it is not a list of proposed future changes.

| Requirement | Current behavior | Evidence |
| --- | --- | --- |
| Basic human commands and intuitive omitted targets | `agit` chooses a session; push/log/share use transient selection; explicit flags remain available | `tests/tui_session_picker.rs`, `tests/mutation_context.rs`, `tests/explicit_push_target.rs` |
| Distinguish opening a source from continuation | `open` is canonical, `run` is a compatible alias, `resume` continues without creating a fork | `commands::json_cli_tests`, CLI help, bundled open/resume references |
| Remove shared implicit branch selection | `switch` is removed; workspace/native discovery does not select an existing-session target | `tests/at_ref_session_context.rs`, `tests/explicit_session_reads.rs`, `commands::context` tests |
| Agent-readable Skill and prompt injection | Valid YAML, executable command examples, explicit follow-up targets, current project injection | `commands::skill_bundle` contract tests, `tests/skill_agent_flow.rs`, independent YAML parsing |
| Noninteractive output and JSON option | Stable envelope; typed status/config/history/ref/search data; partial failures remain observable | `tests/status_json_fields.rs`, `tests/config_json_fields.rs`, `tests/history_json.rs`, `tests/search_json.rs`, `tests/json_stream_contract.rs` |
| Noninteractive credential-store access | macOS authorization dialogs are suppressed; failures explain the existing keystore requirement and leave vault/history unchanged | `domain::secret_filter::os_keychain` policy/error tests, file-keystore integration, live protected-vault rejection check |
| Saved code-state context | Current turns capture a sanitized summary; imported history does not invent past state; resume retains explicit cwd choices and offers additive/runtime hook context | `tests/resume_cwd_selection.rs`, `commands::resume` and `domain::meta` tests, `tests/hook_stdin_session.rs`, `tests/org_repo_import_and_hook.rs` |
| Bare config TUI | Edit and unset stored settings while distinguishing effective values | `bare_config_edits_and_unsets_the_persisted_value` |
| Bare init TUI | Name the repository and choose binding behavior | `bare_init_creates_only_the_named_repo_with_binding_disabled` |
| Bare import TUI | Choose the native runtime and preserve its cwd; link-only works before login | `bare_import_selects_a_native_session_and_can_register_it_without_login` |
| Bare new TUI | Choose a local/Hub-owned repo, validate the name, then pass an explicit session identity to the runtime | `bare_new_retries_invalid_names_and_hands_the_named_branch_to_the_runtime`, `tests/tui_hub_repositories.rs` |
| Bare log TUI | Select a session before resolving its history | `bare_log_selects_a_session_before_resolving_directory_context` |
| Bare resume and bare agit | Both show the same adopted session picker | `bare_resume_and_bare_agit_show_the_same_adopted_session` |
| Bare share TUI | Select source, visibility, expiry, and protection, then use the existing scan/confirmation path | `bare_share_can_select_and_cancel_without_a_workspace_binding`, `tests/share_selection.rs` |
| Native TUI session changes and naming | Hooks follow native IDs without claiming unrelated branches; unclaimed conversations enter an explicit naming inbox | `tests/hook_stdin_session.rs`, hook annotation tests, `native_unnamed_sessions_offer_skip_then_reopen_the_naming_inbox` |
| User Settings concepts | Profile/avatar, email, password/providers, devices, storage, shares, organizations and teams have working views | Backend accounts/auth/shares tests; frontend `routes/settings` tests; desktop/mobile browser interaction |
| Repo Settings concepts | Personal/org capabilities, icon/name/visibility/usage/direct/team access/delete; metadata remains editable in overview | Backend agent authorization tests; `AgentDetail.settings` and organization tests; browser team-access deep link |
| Fluent batch search and filters | Bounded batches, shared repo/owner/runtime/evidence filters, saved Git author and UTC time bounds | CLI/MCP/search tests and real authenticated CLI-to-backend HTTP wire test |
| UX and performance | Local first frame, cancellable Hub discovery without token rotation, batched immutable metadata, FIFO cache, direct identity lookup, fewer settings requests | PTY latency/cancellation tests, branch Git-process-count tests, filter/cache tests, request-count and indexed permission-query checks |
| Review and integration | Focused commits in isolated worktrees; synchronize main and require bot approval and a successful head pipeline before merging | Git refs, MR discussions, exact-head approval and pipeline state |

The individual bare-command tests are in `tests/tui_session_picker.rs` and run
real terminal interactions. Their fixtures use disposable runtime homes and a
stub executable to observe the launch identity without making a paid model call.
The paired search test uses the real authenticated backend router and a separately
built CLI; instructions live in the backend's `docs/search-filter-validation.md`.

Platform evidence is explicit: native macOS capture is exercised locally and
Windows CRT/Win32 capture has platform-specific integration coverage in
`tests/windows_json_capture.rs`. Windows execution is verified through the head
pipeline, not inferred from a macOS build. Official native-hook contracts and
trust requirements are linked in the local review and command references.
