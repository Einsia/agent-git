import { Link } from "react-router-dom"
import { ArrowUpRight, FolderGit2 } from "lucide-react"

import { api, type Repo } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { Badge } from "@/components/ui/badge"
import { Eyebrow, LoadError } from "@/components/States"

// The agent-page route segment is the owner_ns, not the full owner string: an org-owned agent's owner
// is "org:<name>" but its URL segment is the bare "<name>". A personal agent's owner already IS the ns.
function ownerNs(owner: string): string {
  return owner.startsWith("org:") ? owner.slice(4) : owner
}

export function Repos() {
  // Open to any caller (anonymous sees public-only), so there is no 401/403 to route: the ACL filter is
  // the whole gate. useGuarded still gives the standard loading / error branches.
  const { data, loading, error, status } = useGuarded(() => api.repos(), [])

  const empty = data && data.repos.length === 0

  return (
    <div className="flex flex-col gap-8">
      <header>
        <span className="eyebrow">code repos</span>
        <h1 className="mt-1 text-2xl font-bold tracking-tight">Code repos</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Every code repo the hub's agents have worked in, grouped by environment. One repo can be
          touched by several agents; each is listed here. Only agents you can read contribute.
        </p>
      </header>

      <section>
        <Eyebrow className="mb-3">repos</Eyebrow>

        {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
        {/* 401 is already on its way to the sign-in form; don't flash an error behind it. */}
        {error && status !== 401 && <LoadError message={error} />}

        {empty && (
          <p className="rounded-lg border bg-card px-4 py-8 text-muted-foreground">
            No repos to show. Once an agent records sessions against a code repo, it appears here.
          </p>
        )}

        {data && data.repos.length > 0 && (
          <div className="flex flex-col gap-4">
            {data.repos.map((r) => (
              <RepoCard key={r.env} repo={r} />
            ))}
          </div>
        )}

        {data && data.capped && (
          <p className="mt-4 rounded-lg border border-kind-warn/30 bg-kind-warn/5 px-4 py-3 text-sm text-muted-foreground">
            The scan stopped at its cap after {data.scanned.toLocaleString()} sessions, so some repos or
            counts may be short.
          </p>
        )}
        {data && data.has_more && !data.capped && (
          <p className="mt-4 text-[0.78rem] text-muted-foreground">
            Showing the most recently active repos; older ones are not listed.
          </p>
        )}
      </section>
    </div>
  )
}

function RepoCard({ repo }: { repo: Repo }) {
  return (
    <div className="rounded-lg border bg-card p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <FolderGit2 className="size-4 shrink-0 text-muted-foreground" />
            <h2 className="truncate font-mono text-sm font-semibold">{repo.cwd ?? repo.env}</h2>
          </div>
          {repo.cwd && (
            <p className="mt-1 truncate font-mono text-[0.72rem] text-muted-foreground">{repo.env}</p>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-2 text-sm">
          <Badge variant="muted" className="font-mono text-[0.6rem]">
            {repo.total_sessions} {repo.total_sessions === 1 ? "session" : "sessions"}
          </Badge>
          {repo.last && <span className="font-mono text-[0.72rem] text-muted-foreground">{repo.last}</span>}
        </div>
      </div>

      <div className="mt-3 border-t pt-3">
        <Eyebrow className="mb-2">agents</Eyebrow>
        <div className="flex flex-wrap gap-2">
          {repo.agents.map((ag) => (
            <Link
              key={`${ag.owner}/${ag.name}`}
              to={`/agent/${encodeURIComponent(ownerNs(ag.owner))}/${encodeURIComponent(ag.name)}`}
              className="inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 font-mono text-xs transition-colors hover:bg-accent/40"
            >
              <span className="text-muted-foreground">{ag.owner}/</span>
              <span className="font-semibold">{ag.name}</span>
              <span className="tabular-nums text-muted-foreground">{ag.sessions}</span>
              <ArrowUpRight className="size-3 text-muted-foreground" />
            </Link>
          ))}
        </div>
      </div>
    </div>
  )
}
