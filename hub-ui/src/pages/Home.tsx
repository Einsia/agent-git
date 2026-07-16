import { Link } from "react-router-dom"
import { ArrowUpRight } from "lucide-react"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { SpineLegend } from "@/components/Spine"

export function Home() {
  const { data, loading, error } = useAsync(() => api.agents(), [])
  const totalSessions = data?.agents.reduce((n, a) => n + a.sessions, 0) ?? 0

  return (
    <div>
      {/* Status readout: the instrument's main gauge — registry state at a glance, no prose. */}
      <section className="readout rounded-lg border p-5">
        <div className="flex flex-wrap items-end justify-between gap-4">
          <div className="flex gap-8">
            <Stat value={data ? data.agents.length : "—"} label="agents" />
            <Stat value={data ? totalSessions : "—"} label="sessions" />
          </div>
          {data?.host && (
            <span className="font-mono text-[0.7rem] text-muted-foreground">
              host <span className="text-foreground/80">{data.host}</span>
            </span>
          )}
        </div>
        <div className="mt-4 border-t pt-3">
          <SpineLegend />
        </div>
      </section>

      <div className="mt-7">
        <div className="grid grid-cols-[1fr_2fr_auto] gap-4 px-4 pb-2">
          <span className="eyebrow">agent</span>
          <span className="eyebrow">latest</span>
          <span className="eyebrow text-right">sessions</span>
        </div>

        {loading && <Skeleton rows={3} />}
        {error && <p className="px-4 py-6 text-destructive">Couldn’t load agents — {error}</p>}

        {data && (
          <div className="overflow-hidden rounded-lg border bg-card">
            {data.agents.length === 0 && (
              <p className="px-4 py-8 text-muted-foreground">
                No agents yet. Create one with{" "}
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-sm">agit-hub add &lt;name&gt;</code>.
              </p>
            )}
            {data.agents.map((a) => (
              <Link
                key={a.name}
                to={`/agent/${a.name}`}
                className="grid grid-cols-[1fr_2fr_auto] items-center gap-4 border-t px-4 py-3.5 first:border-t-0 transition-colors hover:bg-accent/40"
              >
                <span className="flex items-center gap-1.5 font-mono font-semibold">
                  {a.name}
                  <ArrowUpRight className="size-3.5 text-muted-foreground" />
                </span>
                <span className="min-w-0 truncate text-sm text-muted-foreground">
                  <span className="mr-2 font-mono text-[0.78rem] text-muted-foreground/70">
                    {a.when || "—"}
                  </span>
                  {a.subject || "No sessions yet"}
                </span>
                <span className="text-right font-mono font-semibold tabular-nums">{a.sessions}</span>
              </Link>
            ))}
          </div>
        )}

        {data && (
          <footer className="mt-8 flex flex-wrap gap-x-8 gap-y-3 border-t pt-4 text-sm text-muted-foreground">
            <span>
              <span className="eyebrow mr-2">publish</span>
              <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                agit -a push http://{data.host}/&lt;name&gt;.git
              </code>
            </span>
            <Link className="font-mono text-xs text-primary hover:underline" to="/api/agents" reloadDocument>
              /api/agents
            </Link>
          </footer>
        )}
      </div>
    </div>
  )
}

function Stat({ value, label }: { value: number | string; label: string }) {
  return (
    <div>
      <div className="font-mono text-3xl font-bold tabular-nums leading-none">{value}</div>
      <div className="eyebrow mt-1.5">{label}</div>
    </div>
  )
}

function Skeleton({ rows }: { rows: number }) {
  return (
    <div className="overflow-hidden rounded-lg border bg-card">
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="grid grid-cols-[1fr_2fr_auto] gap-4 border-t px-4 py-4 first:border-t-0">
          <div className="h-4 w-24 animate-pulse rounded bg-muted" />
          <div className="h-4 w-full animate-pulse rounded bg-muted" />
          <div className="h-4 w-10 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </div>
  )
}
