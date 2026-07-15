// Thin client over the agit-hub JSON API. Same origin; the Rust server serves this SPA.

export interface AgentSummary {
  name: string
  sessions: number
  when: string
  subject: string
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

export interface AgentPage {
  agent: string
  git: string
  total: number
  page: number
  per_page: number
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

async function get<T>(url: string): Promise<T> {
  const res = await fetch(url, { headers: { Accept: "application/json" } })
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`)
  return res.json() as Promise<T>
}

export const api = {
  agents: () => get<{ agents: AgentSummary[]; host: string }>("/api/agents"),
  agent: (name: string, page = 1, q = "") =>
    get<AgentPage>(`/api/agent/${encodeURIComponent(name)}?page=${page}&q=${encodeURIComponent(q)}`),
  session: (name: string, id: string, at?: string) =>
    get<SessionDetail>(
      `/api/agent/${encodeURIComponent(name)}/session/${encodeURIComponent(id)}${at ? `?at=${at}` : ""}`
    ),
  diff: (name: string, id: string, from: string, to: string) =>
    get<SessionDiff>(
      `/api/agent/${encodeURIComponent(name)}/session/${encodeURIComponent(id)}/diff?from=${from}&to=${to}`
    ),
}
