// Thin client over the agit-hub JSON API. Same origin; the Rust server serves this SPA.
//
// Every shape below is what src/bin/agit-hub.rs actually serializes — no invented fields.
// Where the server reports null honestly (an aid that doesn't exist yet, a caller with no
// grant), the type says so.

export type Visibility = "private" | "public"
/// An agent's lifecycle, as `Lifecycle::as_str` writes it (src/hub/acl.rs). "deleted" is the trash,
/// not a purge — a deleted agent can still be restored.
export type Lifecycle = "active" | "archived" | "deleted"
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

/// A pending org invitation, exactly as `invitation_json` (src/bin/agit-hub/api.rs) serializes it.
/// `username` is the invitee; `role` is the org role they'd get on accept. Both the invitee's own
/// list (GET /api/me/invitations) and an org admin's list (GET /api/orgs/<org>/invitations) return
/// this shape, filtered to `status == "pending"`.
export interface Invitation {
  id: string
  org: string
  username: string
  role: OrgRole
  status: string
  created_by: string
  created: string
}

/// The org's opt-in, hub-side crypto settings — the two fields GET /api/orgs/<name> adds beyond the
/// list shape (they are NOT in GET /api/orgs). `escrow_mode` gates whether the hub may release
/// escrowed session keys under the ACL; `recovery_x25519` is the offline recovery recipient's public
/// key (empty = unset). Only these two are server-side and admin-manageable from a browser — the
/// per-session reader set lives in the client's keybox and is never exposed here.
export type EscrowMode = "none" | "hub-assist"
export interface OrgCrypto {
  name: string
  escrow_mode: EscrowMode
  /// Empty string = no offline recovery recipient set. A 64-hex-char (32-byte) X25519 pubkey when set.
  recovery_x25519: string
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
  /// active / archived / deleted — governs whether writes are refused (see acl.rs). The action
  /// panel keys the archive ↔ unarchive ↔ restore controls off this.
  lifecycle: Lifecycle
  /// null until the owner sets one; the server sends the raw Option<String> through.
  description: string | null
  /// The name the agent was forked from, or null. A bare name, not a scoped id.
  forked_from: string | null
  /// How many callers have starred it, and whether the current caller is one (false when signed out).
  stars: number
  starred: boolean
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

// ── merge requests ──
// A review object parked against a *target* agent. The Hub merges nothing — see src/hub/mr.rs. Every
// shape below is what api_mr_* in src/bin/agit-hub/api.rs serializes.

/// "open" | "merged" | "closed". A raw string, because the server stores it as one and lets an
/// unknown value degrade to "not open" rather than fail the record.
export type MrState = "open" | "merged" | "closed"

/// One end of an MR, serialized **for the reading caller**. When the caller can't read that side's
/// agent the server nulls every field and sets `redacted` — existence is itself a secret.
export interface MrEndpoint {
  aid: string | null
  owner: string | null
  agent: string | null
  full_name: string | null
  ref: string | null
  redacted?: boolean
}

export interface MrComment {
  id: number
  author: string
  body: string
  created: string
}

/// A row in the MR index: `comments` is the count, and there is no transcript (the list omits the big
/// field). `state` is the raw stored string.
export interface MrSummary {
  id: number
  title: string
  author: string
  state: string
  created: string
  updated: string
  source: MrEndpoint
  target: MrEndpoint
  comments: number
  has_transcript: boolean
}

export interface MrList {
  agent: string
  mrs: MrSummary[]
  has_more: boolean
  /// Pass straight back as the `cursor` of the next page; null when there is no more.
  next_cursor: string | null
}

/// The full MR: `comments` is the whole thread, and the transcript is present unless the caller can't
/// read the source (then it's null and `transcript_redacted` is true).
export interface MrDetail {
  id: number
  title: string
  author: string
  state: string
  created: string
  updated: string
  source: MrEndpoint
  target: MrEndpoint
  dialogue_transcript: string | null
  transcript_redacted: boolean
  comments: MrComment[]
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

/// The three lifecycle verbs share a route shape and a response shape.
const lifecycle = (owner: string, name: string, verb: "archive" | "unarchive" | "restore") =>
  request<{ name: string; full_name: string; lifecycle: Lifecycle; aid: string | null }>(
    `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/${verb}`,
    { method: "POST" }
  )

export const api = {
  // ── auth ──
  // `code` is the second factor, sent only when the account has 2FA on: a current TOTP or an unused
  // backup code. Password alone against a 2FA account is a 401 {"error":"2fa_required"} (the server
  // never issues a session for it) — the Login page reads that string to advance to the code step.
  login: (username: string, password: string, code?: string) =>
    request<Me>("/api/login", {
      method: "POST",
      body: JSON.stringify(code ? { username, password, code } : { username, password }),
    }),
  // Self-service signup. On 200 the server sets the session cookie and answers the Me shape
  // ({username, is_admin:false}); 403 = registration disabled, 409 = taken, 400 = invalid.
  register: (username: string, password: string) =>
    request<Me>("/api/register", { method: "POST", body: JSON.stringify({ username, password }) }),
  logout: () => request<void>("/api/logout", { method: "POST" }),
  me: () => get<Me>("/api/me"),

  // ── two-factor auth (TOTP) ──
  // There is deliberately no "is 2FA on?" read endpoint (GET /api/me returns only {username,
  // is_admin}). The Settings card drives state off the flow instead: enroll answers the QR when 2FA
  // is off, or 409 "already enabled" when it's on.
  //
  // Begin enrollment: stores a PENDING secret (2FA not yet active) and returns it + the otpauth://
  // provisioning URI to render as a QR. 409 if 2FA is already active (disable first).
  enroll2fa: () =>
    request<{ secret: string; otpauth_uri: string; issuer: string; account: string | null }>(
      "/api/me/2fa/enroll",
      { method: "POST" }
    ),
  // Verify a 6-digit TOTP against the pending secret. On success 2FA goes active and the 10 one-time
  // backup codes are returned ONCE (only their digests are stored) — show them and never re-fetch.
  confirm2fa: (code: string) =>
    request<{ enabled: boolean; backup_codes: string[] }>("/api/me/2fa/confirm", {
      method: "POST",
      body: JSON.stringify({ code }),
    }),
  // Turn the caller's own 2FA off. Any ONE of a current TOTP, an unused backup code, or the account
  // password proves control. 401 on a wrong proof; 400 if 2FA wasn't on.
  disable2fa: (codeOrPassword: string) =>
    request<{ enabled: boolean }>("/api/me/2fa/disable", {
      method: "POST",
      body: JSON.stringify({ code_or_password: codeOrPassword }),
    }),

  // ── invitations (invitee side) ──
  // The caller's own pending org invitations. Self-scoped: needs only a sign-in (you aren't a member
  // of the inviting org yet). 401 signed out.
  myInvitations: () => get<Invitation[]>("/api/me/invitations"),
  // Accept / decline one of the caller's invitations. The unguessable id is the handle; only the named
  // invitee may act. Accept mints the membership; both answer the settled invitation.
  acceptInvitation: (org: string, id: string) =>
    request<unknown>(`/api/orgs/${encodeURIComponent(org)}/invitations/${encodeURIComponent(id)}/accept`, {
      method: "POST",
    }),
  declineInvitation: (org: string, id: string) =>
    request<unknown>(`/api/orgs/${encodeURIComponent(org)}/invitations/${encodeURIComponent(id)}/decline`, {
      method: "POST",
    }),

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

  // ── org invitations (admin side) ──
  // GET lists the org's pending invitations; POST invites a user with a target org role. Org-admin
  // gated (403 for a plain member, uniform 404 for an org the caller can't see). POST 201 carries the
  // new invitation; 400 no-such-user / bad role, 409 already a member / already invited.
  orgInvitations: (org: string) => get<Invitation[]>(`/api/orgs/${encodeURIComponent(org)}/invitations`),
  inviteOrgMember: (org: string, username: string, role: OrgRole) =>
    request<Invitation>(`/api/orgs/${encodeURIComponent(org)}/invitations`, {
      method: "POST",
      body: JSON.stringify({ username, role }),
    }),
  // Revoke a still-pending invitation (org-admin). 204 on success.
  revokeInvitation: (org: string, id: string) =>
    request<void>(`/api/orgs/${encodeURIComponent(org)}/invitations/${encodeURIComponent(id)}`, {
      method: "DELETE",
    }),

  // ── org crypto (opt-in, hub-side) ──
  // GET /api/orgs/<name> returns the crypto fields alongside the roster; pull them typed for the
  // escrow/recovery controls. (There are also dedicated GET .../escrow and .../recovery routes, but the
  // detail already carries both, so one call does.)
  orgCrypto: (name: string) => get<Org & OrgCrypto>(`/api/orgs/${encodeURIComponent(name)}`),
  // Set the hub-assist escrow mode. Owner-only (403 otherwise). "hub-assist" re-trusts the hub to
  // release the org's escrowed session keys under the ACL gate; "none" turns it off. Answers {org,
  // escrow_mode}.
  setEscrowMode: (org: string, mode: EscrowMode) =>
    request<{ org: string; escrow_mode: EscrowMode }>(`/api/orgs/${encodeURIComponent(org)}/escrow`, {
      method: "POST",
      body: JSON.stringify({ mode }),
    }),
  // Set the offline recovery recipient — a 64-hex-char (32-byte) X25519 public key the client seals
  // the team key to during `team rekey`. Owner-only. Answers {org, recovery_x25519}.
  setRecoveryRecipient: (org: string, key: string) =>
    request<{ org: string; recovery_x25519: string }>(`/api/orgs/${encodeURIComponent(org)}/recovery`, {
      method: "POST",
      body: JSON.stringify({ key }),
    }),
  // Clear the offline recovery recipient (owner-only). Answers {org, recovery_x25519:""}.
  clearRecoveryRecipient: (org: string) =>
    request<{ org: string; recovery_x25519: string }>(`/api/orgs/${encodeURIComponent(org)}/recovery`, {
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

  // ── merge requests ──
  // Keyed on the TARGET agent (owner/name) — that's the memory being changed, so its ACL governs.
  // Listing/reading needs Read; opening/closing needs Write; commenting needs Read + a write token.
  mrs: (owner: string, name: string, cursor?: string) =>
    get<MrList>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/mrs${cursor ? `?cursor=${encodeURIComponent(cursor)}` : ""}`
    ),
  mr: (owner: string, name: string, id: number) =>
    get<MrDetail>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/mrs/${id}`),
  // `source` is the agent the change comes from, as "owner/name". Refs default to main server-side.
  // The 201 carries the full MR (detail shape).
  openMr: (
    owner: string,
    name: string,
    body: { title: string; source: string; source_ref?: string; target_ref?: string; dialogue_transcript?: string }
  ) =>
    request<MrDetail>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/mrs`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  // The 201 is the new comment alone, not the whole MR.
  commentMr: (owner: string, name: string, id: number, comment: string) =>
    request<MrComment>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/mrs/${id}/comments`, {
      method: "POST",
      body: JSON.stringify({ body: comment }),
    }),
  // "closed" settles it; "merged" *records* that someone ran `agit a merge` locally — the Hub merges
  // nothing. Answers the fresh MR (detail shape).
  closeMr: (owner: string, name: string, id: number, state: "closed" | "merged") =>
    request<MrDetail>(`/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/mrs/${id}/close`, {
      method: "POST",
      body: JSON.stringify({ state }),
    }),

  // ── agent actions ──
  // Fork: any caller who can read it, signed in with a write token. The 201 carries the fork's
  // `full_name` to route to.
  forkAgent: (owner: string, name: string, forkName?: string) =>
    request<{ name: string; owner: string; full_name: string; forked_from: string }>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/fork`,
      { method: "POST", body: JSON.stringify(forkName ? { name: forkName } : {}) }
    ),
  // Star / unstar, per caller. Gated at Read (it's the caller's own bookmark). Answers the fresh count.
  starAgent: (owner: string, name: string, starred: boolean) =>
    request<{ name: string; full_name: string; starred: boolean; stars: number }>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/star`,
      { method: "POST", body: JSON.stringify({ starred }) }
    ),
  // Lifecycle moves, each Manage-gated (owner/admin). Answers {name, full_name, lifecycle, aid}.
  archiveAgent: (owner: string, name: string) => lifecycle(owner, name, "archive"),
  unarchiveAgent: (owner: string, name: string) => lifecycle(owner, name, "unarchive"),
  restoreAgent: (owner: string, name: string) => lifecycle(owner, name, "restore"),
  // Transfer ownership to another user (`to`) or an org (`org`) — mutually exclusive. Manage-gated;
  // the aid does not move. Answers {name, owner, full_name, previous_owner, aid}.
  transferAgent: (owner: string, name: string, dest: { to: string } | { org: string }) =>
    request<{ name: string; owner: string; full_name: string; previous_owner: string | null; aid: string | null }>(
      `/api/agent/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/transfer`,
      { method: "POST", body: JSON.stringify(dest) }
    ),

  // ── admin user recovery ──
  // Site-admin only, and the SERVER is the gate (both 403 for a non-admin regardless of any client
  // check). These are the only two per-user admin doors the HTTP API exposes — there is deliberately
  // NO list-users / disable-account endpoint here; the full roster and account enable/disable live in
  // the `agit-hub user` CLI on the host.
  //
  // Clear a user's 2FA — recovery for someone locked out of their authenticator. Answers {ok, user,
  // enabled:false}; 404 for an unknown user.
  adminDisable2fa: (username: string) =>
    request<{ ok: boolean; user: string; enabled: boolean }>(
      `/api/users/${encodeURIComponent(username)}/2fa-disable`,
      { method: "POST" }
    ),
  // Reset a user's password (a lockout/recovery action) — revokes all of that user's sessions.
  // Answers {ok, user, revoked_sessions}; 400 too-short, 404 unknown user.
  adminSetPassword: (username: string, newPassword: string) =>
    request<{ ok: boolean; user: string; revoked_sessions: number }>(
      `/api/users/${encodeURIComponent(username)}/password`,
      { method: "POST", body: JSON.stringify({ new_password: newPassword }) }
    ),
}
