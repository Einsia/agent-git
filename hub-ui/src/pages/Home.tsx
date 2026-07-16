import { Link } from "react-router-dom"
import { ArrowUpRight, Lock, Plus } from "lucide-react"

import { api, type AgentSummary, type EffectiveRole } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useSession } from "@/lib/session"
import { SpineLegend } from "@/components/Spine"
import { Badge } from "@/components/ui/badge"
import { buttonVariants } from "@/components/ui/button"
import { LoadError } from "@/components/States"
import { cn } from "@/lib/utils"

export function Home() {
  const { data, loading, error, status } = useAsync(() => api.agents(), [])
  const { me, loading: meLoading } = useSession()
  const totalSessions = data?.agents.reduce((n, a) => n + a.sessions, 0) ?? 0

  // /api/agents doesn't reject anonymous callers — it answers with the agents they may see,
  // which for a signed-out visitor is the public ones. So the signed-out state to handle is
  // an *empty* list, not an error. The 401 branch below is the honest fallback for a hub
  // configured to refuse anonymous reads outright.
  const signedOut = !meLoading && !me
  const empty = data?.agents.length === 0

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
        <div className="mb-2 flex items-center justify-between gap-4 px-4">
          <div className="grid flex-1 grid-cols-[1fr_2fr_auto] gap-4">
            <span className="eyebrow">agent</span>
            <span className="eyebrow">latest</span>
            <span className="eyebrow text-right">sessions</span>
          </div>
          {me && (
            <Link to="/new" className={cn(buttonVariants({ variant: "outline", size: "sm" }), "shrink-0")}>
              <Plus />
              new agent
            </Link>
          )}
        </div>

        {loading && <Skeleton rows={3} />}

        {status === 401 && <SignInWall />}
        {error && status !== 401 && <LoadError message={error} />}

        {data && (
          <div className="overflow-hidden rounded-lg border bg-card">
            {empty && (
              <div className="px-4 py-8 text-muted-foreground">
                {signedOut ? (
                  <p>
                    No public agents here.{" "}
                    <Link to="/login" className="text-primary hover:underline">
                      Sign in
                    </Link>{" "}
                    to see the ones you have access to.
                  </p>
                ) : (
                  <p>
                    No agents yet. Create one above, or with{" "}
                    <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-sm">
                      agit-hub add &lt;name&gt;
                    </code>
                    .
                  </p>
                )}
              </div>
            )}
            {data.agents.map((a) => (
              <Row key={a.name} a={a} />
            ))}
          </div>
        )}

        {data && !empty && signedOut && (
          <p className="mt-3 px-4 text-[0.8rem] text-muted-foreground">
            Showing public agents.{" "}
            <Link to="/login" className="text-primary hover:underline">
              Sign in
            </Link>{" "}
            to see private ones.
          </p>
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

function Row({ a }: { a: AgentSummary }) {
  return (
    <Link
      to={`/agent/${a.name}`}
      className="grid grid-cols-[1fr_2fr_auto] items-center gap-4 border-t px-4 py-3.5 first:border-t-0 transition-colors hover:bg-accent/40"
    >
      <span className="flex min-w-0 items-center gap-1.5 font-mono font-semibold">
        {/* Private is the default, so the lock marks the norm, not the exception — it stays
            quiet. What it buys is that a public agent is visibly missing one. */}
        {a.visibility === "private" && (
          <Lock className="size-3 shrink-0 text-muted-foreground" aria-label="private" />
        )}
        <span className="truncate">{a.name}</span>
        <ArrowUpRight className="size-3.5 shrink-0 text-muted-foreground" />
      </span>
      <span className="min-w-0 truncate text-sm text-muted-foreground">
        <span className="mr-2 font-mono text-[0.78rem] text-muted-foreground/70">{a.when || "—"}</span>
        {a.subject || "No sessions yet"}
      </span>
      <span className="flex items-center justify-end gap-2">
        <RoleBadge role={a.role} />
        <span className="w-8 text-right font-mono font-semibold tabular-nums">{a.sessions}</span>
      </span>
    </Link>
  )
}

// null = no explicit grant: they see it because it's public. Nothing to badge.
function RoleBadge({ role }: { role: EffectiveRole | null }) {
  if (!role) return null
  return (
    <Badge variant={role === "owner" ? "default" : "muted"} className="font-mono text-[0.6rem]">
      {role}
    </Badge>
  )
}

function SignInWall() {
  return (
    <div className="readout rounded-lg border px-6 py-12 text-center">
      <Lock className="mx-auto mb-3 size-6 text-muted-foreground" />
      <p className="mb-1 font-semibold">Sign in to see agents</p>
      <p className="mx-auto mb-5 max-w-[46ch] text-sm text-muted-foreground">
        This hub doesn't serve the registry to anonymous callers.
      </p>
      <Link to="/login" className={cn(buttonVariants({ size: "sm" }))}>
        Sign in
      </Link>
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
