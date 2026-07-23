import { Link, useParams, useSearchParams } from "react-router-dom"
import { ArrowUpRight, Building2, Lock } from "lucide-react"

import { api, type OrgOverview, type OrgOverviewAgent } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { cn } from "@/lib/utils"
import { Badge } from "@/components/ui/badge"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Forbidden, LoadError } from "@/components/States"
import { MemberCreateControl, MembersPanel, OrgCrypto } from "@/pages/orgs-management"

// The agent-page route segment is the owner_ns, not the full owner string: an org-owned agent's owner
// is "org:<name>" but its URL segment is the bare "<name>". A personal agent's owner already IS the ns.
function ownerNs(owner: string): string {
  return owner.startsWith("org:") ? owner.slice(4) : owner
}

const TABS = ["agents", "members", "settings"] as const
type TabKey = (typeof TABS)[number]

const TAB_LABELS: Record<TabKey, string> = {
  agents: "Agents",
  members: "Members",
  settings: "Settings",
}

export function OrgDetail() {
  const { name = "" } = useParams()
  const { me } = useSession()
  const [params, setParams] = useSearchParams()

  // The overview gate is member-or-site-admin, and a non-member gets the same 404 as a missing org
  // (existence non-disclosure), so 403 is not expected here; the error branch below renders the 404.
  const { data, loading, error, status, forbidden, reload } = useGuarded(
    () => api.orgOverview(name),
    [name]
  )

  // Deep-linkable via ?tab=; an absent or unknown value falls back to the default Agents tab.
  const raw = params.get("tab")
  const tab: TabKey = (TABS as readonly string[]).includes(raw ?? "") ? (raw as TabKey) : "agents"

  function selectTab(next: TabKey) {
    const p = new URLSearchParams(params)
    if (next === "agents") p.delete("tab")
    else p.set("tab", next)
    setParams(p, { replace: true })
  }

  // Managing the roster and settings needs org-admin (or a site admin) — the same gate the server
  // enforces. The caller's own org role comes off the (unfiltered) members roster.
  const canManage =
    !!me?.is_admin || (data?.members ?? []).some((m) => m.username === me?.username && m.role === "admin")

  if (forbidden) return <Forbidden what={`the ${name} org`} />

  return (
    <div className="flex flex-col gap-8">
      <header>
        <div className="flex flex-wrap items-center gap-2.5">
          <Building2 className="size-6 text-muted-foreground" />
          <h1 className="font-mono text-2xl font-bold tracking-tight">{name}</h1>
        </div>
        <p className="mt-2 max-w-[62ch] text-sm text-muted-foreground">
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
          <div role="tablist" aria-label="Organization sections" className="flex items-center gap-1 border-b">
            {TABS.map((t) => (
              <button
                key={t}
                type="button"
                role="tab"
                id={`org-tab-${t}`}
                aria-selected={tab === t}
                aria-controls={`org-panel-${t}`}
                onClick={() => selectTab(t)}
                className={cn(
                  "-mb-px border-b-2 border-transparent px-3 py-2 text-sm font-medium text-muted-foreground transition-colors hover:text-foreground",
                  tab === t && "border-primary text-foreground"
                )}
              >
                {TAB_LABELS[t]}
              </button>
            ))}
          </div>

          <div role="tabpanel" id={`org-panel-${tab}`} aria-labelledby={`org-tab-${tab}`}>
            {tab === "agents" && <AgentsTab data={data} />}
            {tab === "members" && (
              <MembersPanel org={name} members={data.members} canManage={canManage} reload={reload} />
            )}
            {tab === "settings" && <SettingsTab org={name} canManage={canManage} />}
          </div>
        </>
      )}
    </div>
  )
}

function AgentsTab({ data }: { data: OrgOverview }) {
  return (
    <section>
      <h2 className="mb-4 flex items-baseline gap-2 text-lg font-semibold tracking-tight">
        Agents
        <span className="font-mono text-base tabular-nums text-muted-foreground">{data.agents.length}</span>
      </h2>
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
                <TableHead>Agent</TableHead>
                <TableHead>Visibility</TableHead>
                <TableHead className="text-right">Sessions</TableHead>
                <TableHead>Environments</TableHead>
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
  )
}

function SettingsTab({ org, canManage }: { org: string; canManage: boolean }) {
  if (!canManage) {
    return (
      <section>
        <h2 className="mb-4 text-lg font-semibold tracking-tight">Settings</h2>
        <p className="rounded-lg border bg-card px-4 py-8 text-muted-foreground">
          Only an org admin can change these settings.
        </p>
      </section>
    )
  }
  return (
    <section className="flex flex-col gap-8">
      <MemberCreateControl org={org} />
      <OrgCrypto org={org} />
    </section>
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
            <Badge variant="muted">
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
                className="rounded-full border bg-muted px-2.5 py-0.5 font-mono text-xs text-muted-foreground transition-colors hover:text-foreground"
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
