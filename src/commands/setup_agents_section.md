## Session version control (agit)

Use these rules when the user requests an AgentGit operation or `AGIT_SESSION` or
`AGIT_MERGE_TX` identifies the current managed session. Otherwise continue the
user's task without agit checks, transcript discovery, or session adoption.
The working directory and this file alone do not activate AgentGit.

- Inspect `agit status --json` when the requested operation needs session or workspace state. An adopted session still needs an explicit `<owner/repo>@<branch>` or `AGIT_SESSION`; directory bindings and native runtime IDs do not select it.
- Settle completed phases with `agit commit <owner/repo>@<branch> --milestone "<summary>"` (add `--code` when relevant). Importing a session does not set `AGIT_SESSION` in the calling process.
- If resumed as a merge agent, follow the `AGIT_MERGE_TX` protocol in the agit skill.
- Never rebase or force-push AgentGit history; remove context with `agit revert <owner/repo>@<branch>#n.k`. `@` requires `AGIT_SESSION`.
