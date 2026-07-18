// Thin client over the agit-hub JSON API. Same origin; the Rust server serves this SPA.
//
// Every shape below is what src/bin/agit-hub.rs actually serializes — no invented fields.
// Where the server reports null honestly (an aid that doesn't exist yet, a caller with no
// grant), the type says so.

export type Visibility = "private" | "public"
/// A membership grant. The owner is not a member — ownership is separate.
export type Role = "read" | "write" | "admin"
/// What the caller may do here, as the server computed it. null = no explicit grant
/// (they can see it only because it's public).
export type EffectiveRole = "owner" | "admin" | "write" | "read"
export type Scope = "read" | "write"

export interface Me {
  username: string
  is_admin: boolean
}

export interface Member {
  username: string
  role: Role
}

/// The **org** role, a separate axis from the agent-level `Role` above. "member" grants write on
/// every agent the org owns; "admin" also manages the org's roster. (See OrgMember::agent_role in
/// src/hub/store.rs.)
export type OrgRole = "member" | "admin"

export interface OrgMember {
  username: string
  role: OrgRole
}

export interface Org {
  name: string
  created: string
  members: OrgMember[]
}

export interface AgentSummary {
  name: string
  /// The full owner string — a bare username, or "org:<name>" for an org-owned agent. null only
  /// for the fail-safe unowned row. Routing uses `full_name`, not this: for an org the URL segment
  /// is the owner_ns ("acme"), not the full owner ("org:acme").
  owner: string | null
  /// The scoped identity "<owner_ns>/<name>" — unique across owners (a name is unique only within
  /// one). This is what every agent URL and API path is built from.
  full_name: string
  /// null until the client pushes an agent.toml — an empty repo has no identity yet.
  aid: string | null
  aid_source: string
  sessions: number
  when: string
  subject: string
  visibility: Visibility
  role: EffectiveRole | null
}

export interface SessionSummary {
  id: string
  runtime: string
  branch: string
  model: string
  author: string
  when: string
  commit: string
  title: string
  conclusion: string
  files: string[]
  tools: number
  n_prompts: number
  n_texts: number
  spine: string
}

export interface HistoryEntry {
  sha: string
  subject: string
}

export interface Environment {
  env: string | null
  sessions: number
  last: string
}

export interface Branch {
  name: string
  commit: string
  when: string
}

export interface AgentPage {
  agent: string
  /// The scoped identity "<owner_ns>/<name>" — what the agent's URLs and API paths are built from.
  full_name: string
  git: string
  aid: string | null
  aid_source: string
  clone_url: string
  visibility: Visibility
  owner: string | null
  members: Member[]
  role: EffectiveRole | null
  environments: Environment[]
  branches: Branch[]
  size_bytes: number
  runtimes: string[]
  total: number
  page: number
  per_page: number
  scan_capped: boolean
  sessions: SessionSummary[]
  history: HistoryEntry[]
}

export interface Revision {
  sha: string
  when: string
  subject: string
}

export interface SessionDetail {
  id: string
  runtime: string
  branch: string
  model: string
  author: string
  when: string
  commit: string
  cwd: string
  prompts: string[]
  texts: string[]
  files: string[]
  spine: string
  revisions: Revision[]
  pinned?: string
}

export interface SessionDiff {
  from: string
  to: string
  added_prompts: string[]
  removed_prompts: string[]
  added_files: string[]
  removed_files: string[]
  conclusion_before: string
  conclusion_after: string
}

export interface CreatedAgent {
  name: string
  /// The owner the server assigned (the caller, or "org:<name>" under an org).
  owner: string
  /// The scoped identity "<owner_ns>/<name>" — route straight to it after create.
  full_name: string
  aid: string | null
  aid_source: string
  clone_url: string
  visibility: Visibility
}

export interface TokenInfo {
  id: string
  name: string
  owner: string | null
  /// null = the token reaches every agent its owner can reach.
  agent: string | null
  scope: Scope
  created: string
  expires: string | null
  last_used: string | null
  /// Old ownerless tokens are listed for what they are: they no longer authenticate.
  usable: boolean
}

export interface AuditEntry {
  when: string
  actor: string
  action: string
  agent: string | null
  detail: string
}

/// Carries the HTTP status, because the status *is* the meaning here: 401 = not signed in,
/// 403 = signed in and refused, 404 = gone or never visible to you (the server deliberately
/// gives the same answer for both).
export class ApiError extends Error {
  constructor(
    public status: number,
    message: string
  ) {
    super(message)
    this.name = "ApiError"
  }
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init?.body ? { "Content-Type": "application/json" } : null),
      ...init?.headers,
    },
  })
  if (!res.ok) {
    // Errors come back as {"error": "..."} — surface the server's own wording when there is
    // one; it is written for a person to read.
    const detail = await res
      .json()
      .then((v: unknown) => (v as { error?: string })?.error)
      .catch(() => undefined)
    throw new ApiError(res.status, detail || `${res.status} ${res.statusText}`)
  }
  // 204 on logout / delete / revoke — there is no body to parse.
  if (res.status === 204) return undefined as T
  return res.json() as Promise<T>
}

const get = <T,>(url: string) => request<T>(url)

export const api = {
  // ── auth ──
  login: (username: string, password: string) =>
    request<Me>("/api/login", { method: "POST", body: JSON.stringify({ username, password }) }),
  // Self-service signup. On 200 the server sets the session cookie and answers the Me shape
  // ({username, is_admin:false}); 403 = registration disabled, 409 = taken, 400 = invalid.
  register: (username: string, password: string) =>
    request<Me>("/api/register", { method: "POST", body: JSON.stringify({ username, password }) }),
  logout: () => request<void>("/api/logout", { method: "POST" }),
  me: () => get<Me>("/api/me"),

  // ── agents ──
  // Identity is (owner, name): every agent path is /api/agent/<owner>/<name>, where <owner> is the
  // owner_ns segment (the first half of `full_name`), NOT the full "org:<name>" owner string.
  agents: () => get<{ agents: AgentSummary[]; host: string }>("/api/agents"),
  agent: (owner: string, name: string, page = 1, q = "") =>
    get<AgentPage>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}?page=${page}&q=${encodeURIComponent(q)}`
    ),
  // The 201 carries {name, owner, full_name, ...} — callers route off `full_name`.
  createAgent: (name: string, visibility: Visibility) =>
    request<CreatedAgent>("/api/agents", { method: "POST", body: JSON.stringify({ name, visibility }) }),
  // Rename answers {name, renamed_from}; a visibility change answers {name, visibility, owner}.
  // Neither is worth a type: callers reload or navigate.
  patchAgent: (owner: string, name: string, patch: { name?: string; visibility?: Visibility }) =>
    request<unknown>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
      method: "PATCH",
      body: JSON.stringify(patch),
    }),
  deleteAgent: (owner: string, name: string) =>
    request<void>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, { method: "DELETE" }),

  // ── members ──
  members: (owner: string, name: string) =>
    get<Member[]>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/members`),
  // POST doubles as "change role": an existing member's role is overwritten in place.
  addMember: (owner: string, name: string, username: string, role: Role) =>
    request<Member[]>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/members`, {
      method: "POST",
      body: JSON.stringify({ username, role }),
    }),
  removeMember: (owner: string, name: string, username: string) =>
    request<void>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/members/${encodeURIComponent(username)}`,
      {
        method: "DELETE",
      }
    ),

  // ── orgs ──
  // The orgs the caller belongs to (a site admin sees all). 401 if signed out.
  orgs: () => get<Org[]>("/api/orgs"),
  // The creator becomes the org's first admin. The 201 body omits `created`, so this reports
  // only what the server sends back; callers reload the list for the full record.
  createOrg: (name: string) =>
    request<{ name: string; members: OrgMember[] }>("/api/orgs", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  org: (name: string) => get<Org>(`/api/orgs/${encodeURIComponent(name)}`),
  orgMembers: (name: string) => get<OrgMember[]>(`/api/orgs/${encodeURIComponent(name)}/members`),
  // POST doubles as "change role": an existing member's role is overwritten in place. Returns the
  // fresh roster. Org-admin gated (403), 400 if the role or username is bad / user doesn't exist.
  addOrgMember: (name: string, username: string, role: OrgRole) =>
    request<OrgMember[]>(`/api/orgs/${encodeURIComponent(name)}/members`, {
      method: "POST",
      body: JSON.stringify({ username, role }),
    }),
  // 204 on success; 409 if it would leave the org with no admin.
  removeOrgMember: (name: string, username: string) =>
    request<void>(`/api/orgs/${encodeURIComponent(name)}/members/${encodeURIComponent(username)}`, {
      method: "DELETE",
    }),

  // ── tokens ──
  tokens: () => get<TokenInfo[]>("/api/tokens"),
  // The plaintext comes back once, and only here. The server stores a sha256 digest. `agent`, when
  // set, is the scoped id "<owner_ns>/<name>" (a token binds to a scoped agent, not a bare name).
  createToken: (body: { name: string; agent?: string; scope: Scope; ttl_days?: number }) =>
    request<{ token: string }>("/api/tokens", { method: "POST", body: JSON.stringify(body) }),
  revokeToken: (id: string) => request<void>(`/api/tokens/${encodeURIComponent(id)}`, { method: "DELETE" }),

  // ── audit ──
  // No agent = the site-wide log (site admins only, 403 otherwise). A set `agent` is the scoped id
  // "<owner_ns>/<name>"; that agent's log needs Manage on it.
  audit: (agent: string, limit: number) =>
    get<AuditEntry[]>(
      `/api/audit?limit=${limit}${agent ? `&agent=${encodeURIComponent(agent)}` : ""}`
    ),

  // ── sessions ──
  session: (owner: string, name: string, id: string, at?: string) =>
    get<SessionDetail>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/session/${encodeURIComponent(id)}${at ? `?at=${at}` : ""}`
    ),
  diff: (owner: string, name: string, id: string, from: string, to: string) =>
    get<SessionDiff>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/session/${encodeURIComponent(id)}/diff?from=${from}&to=${to}`
    ),
}
