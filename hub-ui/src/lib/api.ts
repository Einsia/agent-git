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

export interface AgentSummary {
  name: string
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
  logout: () => request<void>("/api/logout", { method: "POST" }),
  me: () => get<Me>("/api/me"),

  // ── agents ──
  agents: () => get<{ agents: AgentSummary[]; host: string }>("/api/agents"),
  agent: (name: string, page = 1, q = "") =>
    get<AgentPage>(`/api/agent/${encodeURIComponent(name)}?page=${page}&q=${encodeURIComponent(q)}`),
  createAgent: (name: string, visibility: Visibility) =>
    request<CreatedAgent>("/api/agents", { method: "POST", body: JSON.stringify({ name, visibility }) }),
  // Rename answers {name, renamed_from}; a visibility change answers {name, visibility, owner}.
  // Neither is worth a type: callers reload or navigate.
  patchAgent: (name: string, patch: { name?: string; visibility?: Visibility }) =>
    request<unknown>(`/api/agent/${encodeURIComponent(name)}`, { method: "PATCH", body: JSON.stringify(patch) }),
  deleteAgent: (name: string) => request<void>(`/api/agent/${encodeURIComponent(name)}`, { method: "DELETE" }),

  // ── members ──
  members: (name: string) => get<Member[]>(`/api/agent/${encodeURIComponent(name)}/members`),
  // POST doubles as "change role": an existing member's role is overwritten in place.
  addMember: (name: string, username: string, role: Role) =>
    request<Member[]>(`/api/agent/${encodeURIComponent(name)}/members`, {
      method: "POST",
      body: JSON.stringify({ username, role }),
    }),
  removeMember: (name: string, username: string) =>
    request<void>(`/api/agent/${encodeURIComponent(name)}/members/${encodeURIComponent(username)}`, {
      method: "DELETE",
    }),

  // ── tokens ──
  tokens: () => get<TokenInfo[]>("/api/tokens"),
  // The plaintext comes back once, and only here. The server stores a sha256 digest.
  createToken: (body: { name: string; agent?: string; scope: Scope; ttl_days?: number }) =>
    request<{ token: string }>("/api/tokens", { method: "POST", body: JSON.stringify(body) }),
  revokeToken: (id: string) => request<void>(`/api/tokens/${encodeURIComponent(id)}`, { method: "DELETE" }),

  // ── audit ──
  // No agent = the site-wide log (site admins only, 403 otherwise).
  audit: (agent: string, limit: number) =>
    get<AuditEntry[]>(
      `/api/audit?limit=${limit}${agent ? `&agent=${encodeURIComponent(agent)}` : ""}`
    ),

  // ── sessions ──
  session: (name: string, id: string, at?: string) =>
    get<SessionDetail>(
      `/api/agent/${encodeURIComponent(name)}/session/${encodeURIComponent(id)}${at ? `?at=${at}` : ""}`
    ),
  diff: (name: string, id: string, from: string, to: string) =>
    get<SessionDiff>(
      `/api/agent/${encodeURIComponent(name)}/session/${encodeURIComponent(id)}/diff?from=${from}&to=${to}`
    ),
}
