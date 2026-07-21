# Hub views: Organization overview + Code-repo index

Date: 2026-07-21
Status: approved feature ("implement an org view ... see everyone and their agents and the
code repos they're attached to" + "a view for code repos and their agents (there may be
duplicates)")

## Data reality (from the codebase map)

- There is **no code-repo entity** on the hub and no repo/agent join. An "agent" IS a bare
  git repo at `<root>/<owner_ns>/<name>.git`, owned by a user (`alice`) or org (`org:acme`).
- The env <-> agent link exists only inside git content: session files at
  `sessions/<env>/<runtime>/<id>.jsonl`. `<env>` is a path-derived slug of the code repo's
  filesystem path (`claude_code::slug_for`) — a partition key, collision-prone, not an
  identity. The authoritative per-session repo path is the transcript `cwd`.
- `environments()` (gitplumb.rs) groups ONE agent's sessions by env. There is no cross-agent
  index. Both views must fan out over `list_agents` and group.
- Org membership is a JSON array in `orgs.members`. ACL is `acl::decide(caller, agent_acl,
  Read)` per agent; org agents fold org-member grants via `agent_acl()`. Org visibility is a
  membership-or-site-admin check returning the same 404 as a missing org (non-disclosure).

## Decisions (defaults; say if you want otherwise)

1. **Repo grouping key = the `env` slug** (what the UI already displays, cheap from the
   session path via `git ls-tree`, no transcript reads). Each repo also carries a
   representative `cwd` (read from its newest session once) for a human-readable path, since
   the slug is lossy. Grouping by `cwd` would be truer but costs a transcript read per
   session; the slug is the pragmatic v1 key.
2. **Org view lists both org-owned agents AND members' personal agents**, each ACL-filtered
   for the caller. "Everyone and their agents" reads as the whole team; a member's PRIVATE
   personal agent stays hidden from other members (personal agents do not inherit org
   grants), so it simply won't appear for callers without access. Org-owned agents honor
   org-member grants as usual.
3. **Both endpoints are ACL-filtered before paging** (never leak counts of hidden agents),
   reusing `agent_acl()` + `acl::decide(..., Read)` exactly as `api_agents` does.

## Endpoints (new)

### `GET /api/orgs/<name>/overview`
Auth: member-or-site-admin, else 404 (identical gate to `api_org_get`).
```json
{
  "name": "acme",
  "created": 1780000000,
  "members": [{ "username": "alice", "role": "admin" }, ...],
  "agents": [
    { "owner": "org:acme", "name": "frontend", "aid": "agt_...",
      "visibility": "public", "sessions": 12, "role": "owner",
      "environments": ["-home-alice-proj-app", "-home-alice-proj-api"] },
    { "owner": "alice", "name": "scratch", "personal": true, ... }
  ]
}
```
Agents = every agent whose `owner == org:<name>` OR `owner` in members, filtered to those the
caller may Read. `environments` = env slugs from that agent's session tree. Capped fan-out.

### `GET /api/repos`
Auth: any caller; only agents the caller may Read contribute (anonymous sees public only).
```json
{
  "repos": [
    { "env": "-home-alice-proj-app", "cwd": "/home/alice/proj/app",
      "total_sessions": 11, "last": "2d ago",
      "agents": [ { "owner": "org:acme", "name": "frontend", "sessions": 8 },
                  { "owner": "bob", "name": "api", "sessions": 3 } ] }
  ],
  "has_more": false, "scanned": 42, "capped": false
}
```
Cross-agent scan: `list_agents` -> each agent's `session_refs` -> group by `env` slug, dedup
agents per env, sum sessions, newest `last`, one representative `cwd`. Capped; report the cap
(`capped`) rather than silently truncating.

## Frontend (React, matches the existing hub design system)

Match `Admin.tsx` exactly (Table primitives, `.eyebrow` header, mono `Badge` chips, `.readout`
wells, dark-first oklch tokens). Do NOT introduce a new aesthetic — consistency with the
existing internal tool wins.

- `src/pages/OrgDetail.tsx` at route `/orgs/:name`: header (eyebrow + org name), a Members
  table (username, role badge), and an Agents table (owner/name link -> `/agent/:owner/:name`,
  visibility badge, session count, env chips linking to `/repos`). Personal agents tagged.
  Link into it from each org card in `Orgs.tsx`.
- `src/pages/Repos.tsx` at route `/repos`: one row/card per repo — the `cwd` (title) + `env`
  slug (mono sub), total sessions, last, and the attached agents as links. A top-nav `Tab`
  to `/repos`. Empty/loading/error/forbidden states per the `useGuarded` pattern.
- `src/lib/api.ts`: add `api.orgOverview(name)` and `api.repos()` typed methods + response
  types.
- `src/App.tsx`: two imports + two `<Route>` lines.
- Build: `npm run build` (regenerates `hub-ui/dist/`), commit `dist/`, then rebuild the Rust
  binary so `include_str!` embeds the new bundle.

## Proof obligations

- ACL: a private agent (org or personal) the caller cannot Read NEVER appears in either
  endpoint (Rust test with an anonymous / non-member caller); org overview 404s for a
  non-member; counts do not leak hidden agents.
- Aggregation: an env touched by two agents appears once with both agents listed and summed
  sessions; an org-owned agent and a member's personal agent both surface in the org view
  (when readable).
- Build integrity: `hub-ui/dist` is rebuilt and committed; the Rust binary rebuilds and
  serves the new pages; `cargo test` (hub) + `cargo build` green; `tsc -b` clean.
- No em dashes in any user-facing string or doc.
