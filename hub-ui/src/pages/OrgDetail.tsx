import { Link, useParams } from "react-router-dom"
import { ArrowUpRight, Lock } from "lucide-react"

import { api, type OrgOverviewAgent } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { Badge } from "@/components/ui/badge"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Eyebrow, Forbidden, LoadError } from "@/components/States"

// The agent-page route segment is the owner_ns, not the full owner string: an org-owned agent's owner
// is "org:<name>" but its URL segment is the bare "<name>". A personal agent's owner already IS the ns.
function ownerNs(owner: string): string {
  return owner.startsWith("org:") ? owner.slice(4) : owner
}

export function OrgDetail() {
  const { name = "" } = useParams()
  // The overview gate is member-or-site-admin, and a non-member gets the same 404 as a missing org
  // (existence non-disclosure), so 403 is not expected here; the error branch below renders the 404.
  const { data, loading, error, status, forbidden } = useGuarded(() => api.orgOverview(name), [name])

  if (forbidden) return <Forbidden what={`the ${name} org`} />

  return (
    <div className="flex flex-col gap-8">
      <header>
        <span className="eyebrow">organization</span>
        <h1 className="mt-1 font-mono text-2xl font-bold tracking-tight">{name}</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Everyone in this org and the agents they can reach: the org's own agents plus each member's
          personal ones you have access to.
        </p>
      </header>

      {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
      {/* 401 is already on its way to the sign-in form; don't flash an error behind it. A 404 (missing
          org, or one you aren't a member of) lands here as a load error. */}
      {error && status !== 401 && <LoadError message={error} />}

      {data && (
        <>
          <section>
            <div className="mb-3 flex items-baseline gap-2">
              <Eyebrow>members</Eyebrow>
              <span className="font-mono text-sm tabular-nums text-muted-foreground">
                {data.members.length}
              </span>
            </div>
            <div className="overflow-x-auto rounded-lg border bg-card">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>user</TableHead>
                    <TableHead>role</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {data.members.map((m) => (
                    <TableRow key={m.username}>
                      <TableCell className="font-mono text-sm">{m.username}</TableCell>
                      <TableCell>
                        <Badge variant="muted" className="font-mono text-[0.6rem]">
                          {m.role}
                        </Badge>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          </section>

          <section>
            <div className="mb-3 flex items-baseline gap-2">
              <Eyebrow>agents</Eyebrow>
              <span className="font-mono text-sm tabular-nums text-muted-foreground">
                {data.agents.length}
              </span>
            </div>
            {data.agents.length === 0 ? (
              <p className="rounded-lg border bg-card px-4 py-8 text-muted-foreground">
                No agents you can reach here yet. The org's own agents and its members' personal agents
                show up once you have access to them.
              </p>
            ) : (
              <div className="overflow-x-auto rounded-lg border bg-card">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>agent</TableHead>
                      <TableHead>visibility</TableHead>
                      <TableHead className="text-right">sessions</TableHead>
                      <TableHead>environments</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {data.agents.map((a) => (
                      <AgentRow key={`${a.owner}/${a.name}`} a={a} />
                    ))}
                  </TableBody>
                </Table>
              </div>
            )}
          </section>
        </>
      )}
    </div>
  )
}

function AgentRow({ a }: { a: OrgOverviewAgent }) {
  const ns = ownerNs(a.owner)
  return (
    <TableRow>
      <TableCell>
        <div className="flex flex-wrap items-center gap-2">
          <Link
            to={`/agent/${encodeURIComponent(ns)}/${encodeURIComponent(a.name)}`}
            className="flex items-center gap-1.5 font-mono text-sm font-semibold hover:text-primary"
          >
            <span className="text-muted-foreground">{a.owner}/</span>
            {a.name}
            <ArrowUpRight className="size-3.5 shrink-0 text-muted-foreground" />
          </Link>
          {a.personal && (
            <Badge variant="muted" className="text-[0.6rem]">
              personal
            </Badge>
          )}
        </div>
      </TableCell>
      <TableCell>
        {a.visibility === "private" ? (
          <Badge variant="muted" className="gap-1">
            <Lock className="size-3" />
            private
          </Badge>
        ) : (
          <Badge variant="muted" className="text-kind-edit">
            public
          </Badge>
        )}
      </TableCell>
      <TableCell className="text-right font-mono tabular-nums">{a.sessions}</TableCell>
      <TableCell>
        {a.environments.length === 0 ? (
          <span className="text-sm text-muted-foreground">none</span>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {a.environments.map((env) => (
              <Link
                key={env}
                to="/repos"
                title="View this repo in the code-repo index"
                className="rounded-full border bg-muted px-2 py-0.5 font-mono text-[0.68rem] text-muted-foreground transition-colors hover:text-foreground"
              >
                {env}
              </Link>
            ))}
          </div>
        )}
      </TableCell>
    </TableRow>
  )
}
