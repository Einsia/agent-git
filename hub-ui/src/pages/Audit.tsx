import { useSearchParams } from "react-router-dom"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useGuarded } from "@/lib/useGuarded"
import { Select } from "@/components/ui/select"
import { Badge } from "@/components/ui/badge"
import { Forbidden, LoadError } from "@/components/States"
import { cn } from "@/lib/utils"

// Colour actions by consequence, off the same kind palette as the spine: deletions and
// permission changes have to surface; a routine fetch shouldn't compete for attention.
// Colour is emphasis only — the action name is always spelled out beside it.
function actionTone(action: string): string {
  const a = action.toLowerCase()
  if (a.includes("denied") || a.includes("delete") || a.includes("revoke") || a.includes("failed"))
    return "text-kind-warn"
  if (a.includes("member") || a.includes("visibility") || a.includes("rename")) return "text-kind-tool"
  if (a.includes("push") || a.includes("create")) return "text-kind-edit"
  if (a.includes("login") || a.includes("logout")) return "text-kind-prompt"
  return "text-muted-foreground"
}

const LIMITS = [50, 100, 250, 500]

export function Audit() {
  const [params, setParams] = useSearchParams()
  const agent = params.get("agent") ?? ""
  const limit = Number(params.get("limit") ?? 100)

  const { data, loading, error, status, forbidden } = useGuarded(() => api.audit(agent, limit), [agent, limit])
  // Plain useAsync: /api/agents never rejects a caller, so this filter has no business
  // triggering a redirect of its own.
  const agents = useAsync(() => api.agents(), [])

  function set(key: string, value: string) {
    const next = new URLSearchParams(params)
    if (value) next.set(key, value)
    else next.delete(key)
    setParams(next)
  }

  return (
    <div>
      <header className="mb-6">
        <span className="eyebrow">audit</span>
        <h1 className="mt-1 text-2xl font-bold tracking-tight">Audit log</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Who did what to which agent, and when. Append-only. Refused attempts are recorded too.
        </p>
      </header>

      <div className="mb-3 flex flex-wrap items-center gap-2">
        <Select
          value={agent}
          onChange={(e) => set("agent", e.target.value)}
          className="w-[200px]"
          aria-label="Filter by agent"
        >
          <option value="">site-wide</option>
          {/* The audit log is keyed on the scoped id "<owner_ns>/<name>". */}
          {agents.data?.agents.map((a) => (
            <option key={a.full_name} value={a.full_name}>
              {a.full_name}
            </option>
          ))}
        </Select>
        <Select
          value={String(limit)}
          onChange={(e) => set("limit", e.target.value)}
          className="w-[120px]"
          aria-label="How many entries"
        >
          {LIMITS.map((l) => (
            <option key={l} value={l}>
              last {l}
            </option>
          ))}
        </Select>
      </div>

      {/* The server's reason is the honest one here: site-wide is admin-only and login-only,
          while a single agent's log needs manage rights on it. Don't guess between them. */}
      {forbidden && <Forbidden what={agent ? `${agent}'s audit log` : "the site-wide audit log"} reason={error ?? undefined} />}
      {loading && !forbidden && <p className="py-6 text-muted-foreground">Loading…</p>}
      {error && !forbidden && status !== 401 && <LoadError message={error} />}

      {data && !forbidden && (
        <div className="overflow-hidden rounded-lg border bg-card">
          {data.length === 0 && <p className="px-4 py-8 text-muted-foreground">Nothing recorded.</p>}
          {data.map((e, i) => (
            <div
              key={`${e.when}-${i}`}
              className="grid grid-cols-[auto_auto_1fr] items-baseline gap-x-3 gap-y-1 border-t px-4 py-2.5 first:border-t-0 sm:grid-cols-[150px_110px_1fr_auto]"
            >
              <span className="font-mono text-[0.72rem] tabular-nums text-muted-foreground">{e.when}</span>
              <span className="truncate font-mono text-[0.78rem]">{e.actor}</span>
              <span className="flex min-w-0 flex-wrap items-baseline gap-2">
                <span className={cn("font-mono text-[0.78rem] font-semibold", actionTone(e.action))}>
                  {e.action}
                </span>
                {e.detail && (
                  <span className="min-w-0 truncate text-[0.78rem] text-muted-foreground">{e.detail}</span>
                )}
              </span>
              {e.agent && (
                <Badge variant="muted" className="justify-self-start font-mono text-[0.6rem] sm:justify-self-end">
                  {e.agent}
                </Badge>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
