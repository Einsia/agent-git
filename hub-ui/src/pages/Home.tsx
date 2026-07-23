import { Link } from "react-router-dom"
import { ArrowUpRight, Layers, Lock, Plus } from "lucide-react"

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
    <div className="flex flex-col gap-8">
      {/* Status readout: the instrument's main gauge — registry state at a glance. */}
      <section className="readout rounded-lg border p-5">
        <div className="flex flex-wrap items-end justify-between gap-4">
          <div className="flex gap-8">
            <Stat value={data ? data.agents.length : "—"} label="agents" />
            <Stat value={data ? totalSessions : "—"} label="sessions" />
          </div>
          {data?.host && (
            <span className="text-sm text-muted-foreground">
              host <span className="font-mono text-foreground/80">{data.host}</span>
            </span>
          )}
        </div>
        <div className="mt-4 border-t pt-3">
          <SpineLegend />
        </div>
      </section>

      {/* The agent index: its own titled section with a labeled create action, GitHub-style. */}
      <section>
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <h2 className="flex items-baseline gap-2 text-lg font-semibold tracking-tight">
            Agents
            {data && (
              <span className="font-mono text-base tabular-nums text-muted-foreground">
                {data.agents.length}
              </span>
            )}
          </h2>
          {me && (
            <Link to="/new" className={cn(buttonVariants({ variant: "outline", size: "sm" }))}>
              <Plus />
              New agent
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
              <Row key={a.full_name} a={a} />
            ))}
          </div>
        )}

        {data && !empty && signedOut && (
          <p className="mt-3 text-sm text-muted-foreground">
            Showing public agents.{" "}
            <Link to="/login" className="text-primary hover:underline">
              Sign in
            </Link>{" "}
            to see private ones.
          </p>
        )}
      </section>

      {/* Publish + API pointers, kept apart from the list rather than crammed into one strip. */}
      {data && (
        <section className="rounded-lg border bg-card p-4">
          <h2 className="text-sm font-semibold">Publish an agent</h2>
          <p className="mt-2 text-sm text-muted-foreground">
            Push a local agit repo to this hub to add it to the index:
          </p>
          <pre className="mt-3 overflow-x-auto rounded-md border bg-muted p-3 font-mono text-sm leading-relaxed">
            agit -a push http://{data.host}/&lt;name&gt;.git
          </pre>
          <div className="mt-4 border-t pt-3">
            <Link className="text-sm text-primary hover:underline" to="/api/agents" reloadDocument>
              Browse the raw index at /api/agents
            </Link>
          </div>
        </section>
      )}
    </div>
  )
}

function Row({ a }: { a: AgentSummary }) {
  return (
    <Link
      to={`/agent/${a.full_name}`}
      className="flex items-start justify-between gap-4 border-t px-4 py-4 first:border-t-0 transition-colors hover:bg-accent/40"
    >
      <div className="min-w-0">
        <span className="flex items-center gap-1.5 font-mono text-base font-semibold">
          {/* Private is the default, so the lock marks the norm, not the exception — it stays
              quiet. What it buys is that a public agent is visibly missing one. */}
          {a.visibility === "private" && (
            <Lock className="size-3.5 shrink-0 text-muted-foreground" aria-label="private" />
          )}
          {/* The display identity is the scoped owner/name — a name is unique only within an owner. */}
          <span className="truncate">{a.full_name}</span>
          <ArrowUpRight className="size-4 shrink-0 text-muted-foreground" />
        </span>
        <p className="mt-1 truncate text-sm text-muted-foreground">
          {a.subject || "No sessions yet"}
        </p>
        {a.when && <p className="mt-1 text-xs text-muted-foreground">Updated {a.when}</p>}
      </div>
      <div className="flex shrink-0 items-center gap-3">
        <RoleBadge role={a.role} />
        <span className="inline-flex items-center gap-1.5 text-sm text-muted-foreground">
          <Layers className="size-4" />
          <span className="font-mono font-semibold tabular-nums text-foreground">{a.sessions}</span>
        </span>
      </div>
    </Link>
  )
}

// null = no explicit grant: they see it because it's public. Nothing to badge.
function RoleBadge({ role }: { role: EffectiveRole | null }) {
  if (!role) return null
  return (
    <Badge variant={role === "owner" ? "default" : "muted"} className="font-mono">
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
        <div key={i} className="flex items-center justify-between gap-4 border-t px-4 py-5 first:border-t-0">
          <div className="flex-1 space-y-2">
            <div className="h-4 w-40 animate-pulse rounded bg-muted" />
            <div className="h-3 w-full max-w-md animate-pulse rounded bg-muted" />
          </div>
          <div className="h-4 w-10 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </div>
  )
}
