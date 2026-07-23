import { useEffect, useState, type FormEvent, type ReactNode } from "react"
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom"
import { GitFork, GitPullRequest, Lock, Search, Settings2, ShieldCheck, Star } from "lucide-react"

import { api } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Input } from "@/components/ui/input"
import { Button, buttonVariants } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import { SessionCard } from "@/components/SessionCard"
import { CopyButton } from "@/components/CopyButton"
import { Crumb } from "@/components/Crumb"
import { Forbidden, LoadError } from "@/components/States"
import { cn } from "@/lib/utils"

const QUICKSTART_URL = "https://einsia.github.io/agent-git/get-started/quickstart"

export function Agent() {
  const { owner = "", name = "" } = useParams()
  const [params, setParams] = useSearchParams()
  const page = Math.max(1, Number(params.get("page") ?? 1))
  const q = params.get("q") ?? ""
  const [query, setQuery] = useState(q)

  useEffect(() => setQuery(q), [q])

  const { data, loading, error, status, forbidden, reload } = useGuarded(
    () => api.agent(owner, name, page, q),
    [owner, name, page, q]
  )

  function submitSearch(e: FormEvent) {
    e.preventDefault()
    const next = new URLSearchParams()
    if (query.trim()) next.set("q", query.trim())
    setParams(next)
  }

  function goPage(p: number) {
    const next = new URLSearchParams(params)
    next.set("page", String(p))
    setParams(next)
  }

  const totalPages = data ? Math.max(1, Math.ceil(data.total / data.per_page)) : 1
  // The server builds the clone url (it knows the scheme — http vs https behind TLS).
  const cloneUrl = data?.clone_url || `http://HOST:PORT/${owner}/${name}.git`

  if (forbidden) return <Forbidden what={`${owner}/${name}`} />

  // A brand-new agent with nothing pushed yet gets GitHub's empty-repo treatment: the whole
  // column becomes a "quick setup" guide. A search that simply found nothing is NOT that — it
  // keeps the ordinary two-column layout and a "no match" note.
  const isEmpty = !!data && data.sessions.length === 0 && !q

  return (
    <div>
      <Crumb owner={owner} name={name} />

      <div className="mb-8 flex flex-wrap items-start justify-between gap-4">
        <div className="flex flex-wrap items-center gap-2.5">
          {/* The display identity is the scoped owner/name; the owner half stays quiet. */}
          <h1 className="font-mono text-2xl font-bold tracking-tight">
            <span className="text-muted-foreground">{owner}/</span>
            {name}
          </h1>
          {data?.visibility === "private" && (
            <Badge variant="muted" className="gap-1">
              <Lock className="size-3" />
              private
            </Badge>
          )}
          {data?.role && (
            <Badge variant={data.role === "owner" ? "default" : "muted"} className="font-mono text-[0.68rem]">
              {data.role}
            </Badge>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <form onSubmit={submitSearch} className="flex gap-2">
            <div className="relative">
              <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Search sessions…"
                className="w-[220px] pl-8"
              />
            </div>
            <Button type="submit">Search</Button>
          </form>
          {/* The agent actions, grouped GitHub-style rather than strung across one strip. */}
          {data && <StarButton owner={owner} name={name} starred={data.starred} stars={data.stars} onDone={reload} />}
          {data && <ForkButton owner={owner} name={name} />}
          {/* Merge requests are readable by anyone who can read the agent. */}
          <Link
            to={`/agent/${owner}/${name}/mrs`}
            className={cn(buttonVariants({ variant: "outline", size: "sm" }))}
          >
            <GitPullRequest className="size-4" />
            Merge requests
          </Link>
          {/* Settings is readable by anyone who can read the agent; it just shows less to
              someone who can't administer it. */}
          <Link
            to={`/agent/${owner}/${name}/settings`}
            className={cn(buttonVariants({ variant: "outline", size: "sm" }))}
          >
            <Settings2 className="size-4" />
            Settings
          </Link>
        </div>
      </div>

      {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
      {/* 401 is already on its way to the login form; don't flash an error behind it. */}
      {error && status !== 401 && <LoadError message={error} />}

      {data && isEmpty && <QuickSetup name={name} cloneUrl={cloneUrl} />}

      {data && !isEmpty && (
        <div className="grid grid-cols-1 gap-8 md:grid-cols-[1fr_300px]">
          <div>
            <h2 className="mb-4 flex items-baseline gap-2 text-lg font-semibold tracking-tight">
              Sessions
              <span className="font-mono text-base tabular-nums text-muted-foreground">{data.total}</span>
            </h2>

            {data.sessions.length === 0 && (
              <p className="py-6 text-muted-foreground">No sessions match “{q}”.</p>
            )}

            <div className="flex flex-col gap-3">
              {data.sessions.map((s) => (
                <SessionCard key={s.id} owner={owner} name={name} s={s} />
              ))}
            </div>

            {totalPages > 1 && (
              <nav className="mt-6 flex items-center justify-center gap-4 text-sm">
                <Button variant="outline" size="sm" disabled={page <= 1} onClick={() => goPage(page - 1)}>
                  ← Prev
                </Button>
                <span className="font-mono tabular-nums text-muted-foreground">
                  {page} / {totalPages}
                </span>
                <Button variant="outline" size="sm" disabled={page >= totalPages} onClick={() => goPage(page + 1)}>
                  Next →
                </Button>
              </nav>
            )}
          </div>

          <aside className="flex flex-col gap-4">
            <AboutCard
              owner={data.owner}
              visibility={data.visibility}
              role={data.role}
              stars={data.stars}
              description={data.description}
            />
            <CloneCard cloneUrl={cloneUrl} />
            <CommitsCard history={data.history} />
          </aside>
        </div>
      )}
    </div>
  )
}

// A copyable, read-only command block — GitHub's grey terminal box. The copy button floats in the
// top-right so a long command still scrolls under it without collision.
function CommandBlock({ code }: { code: string }) {
  return (
    <div className="relative">
      <pre className="overflow-x-auto rounded-md border bg-muted p-3 pr-12 font-mono text-[0.8rem] leading-relaxed">
        {code}
      </pre>
      <CopyButton value={code} size="icon" variant="ghost" className="absolute right-2 top-2" label="Copy command" />
    </div>
  )
}

// Empty-repo "Quick setup": what a new agent with no sessions shows instead of a bare "none yet".
// The URL box, then the two ways to fill the agent — push an existing one, or start fresh from a repo.
// Commands are verified against the CLI; do not alter them.
function QuickSetup({ name, cloneUrl }: { name: string; cloneUrl: string }) {
  const pushExisting = `agit a remote add origin ${cloneUrl}\nagit a push -u origin HEAD`
  const startNew = `cd your-repo\nagit a init ${name}\nagit a snap\nagit a remote add origin ${cloneUrl}\nagit a push -u origin HEAD`

  return (
    <section className="mx-auto max-w-3xl">
      <div className="mb-6">
        <h2 className="text-xl font-semibold tracking-tight">Quick setup - push your sessions to this agent</h2>
        <p className="mt-1 text-sm text-muted-foreground">
          This agent is empty. Point a local agit remote at it and push to fill it.
        </p>
      </div>

      {/* The agent URL, front and centre — GitHub's repo-URL box. */}
      <div className="mb-6 flex items-center gap-2">
        <Input readOnly value={cloneUrl} className="flex-1 font-mono text-sm" aria-label="Agent URL" />
        <CopyButton value={cloneUrl} size="icon" variant="outline" label="Copy agent URL" />
      </div>

      <div className="flex flex-col gap-4">
        <div className="rounded-lg border bg-card p-4">
          <h3 className="mb-3 text-sm font-semibold">Push an existing agent from the command line</h3>
          <CommandBlock code={pushExisting} />
        </div>

        <div className="rounded-lg border bg-card p-4">
          <h3 className="mb-3 text-sm font-semibold">…or start a new agent from your sessions</h3>
          <CommandBlock code={startNew} />
        </div>
      </div>

      <p className="mt-6 text-sm text-muted-foreground">
        New to agit?{" "}
        <a
          href={QUICKSTART_URL}
          target="_blank"
          rel="noreferrer"
          className="text-primary hover:underline"
        >
          Read the quickstart
        </a>
        .
      </p>
    </section>
  )
}

// The "About" sidebar card: the agent's facts the API actually reports — visibility, role, star
// count, owner — plus the honest end-to-end encryption summary folded in. No invented runtime or
// provenance fields; only what AgentPage returns.
function AboutCard({
  owner,
  visibility,
  role,
  stars,
  description,
}: {
  owner: string | null
  visibility: "private" | "public"
  role: string | null
  stars: number
  description: string | null
}) {
  const org = owner?.startsWith("org:") ? owner.slice("org:".length) : null

  return (
    <section className="rounded-lg border bg-card p-4">
      <h2 className="text-sm font-semibold">About</h2>

      {description && <p className="mt-2 text-sm leading-relaxed text-muted-foreground">{description}</p>}

      <dl className="mt-3 flex flex-col gap-2.5 text-sm">
        <Fact label="Visibility">
          <span className="inline-flex items-center gap-1.5 capitalize">
            {visibility === "private" && <Lock className="size-3.5 text-muted-foreground" />}
            {visibility}
          </span>
        </Fact>
        {role && (
          <Fact label="Your role">
            <span className="font-mono">{role}</span>
          </Fact>
        )}
        <Fact label="Stars">
          <span className="inline-flex items-center gap-1.5 tabular-nums">
            <Star className="size-3.5 text-muted-foreground" />
            {stars}
          </span>
        </Fact>
        <Fact label="Owner">
          <span className="truncate font-mono">{owner || "—"}</span>
        </Fact>
      </dl>

      {/* Encryption is read-only here: the hub never holds the session keys and exposes no reader
          set, so this is an honest summary that points at the CLI (and, for an org, the org page)
          rather than a management surface. */}
      <div className="mt-4 border-t pt-4">
        <Badge variant="muted" className="gap-1">
          <ShieldCheck className="size-3" />
          end-to-end
        </Badge>
        <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
          Sessions can be encrypted per-session before they reach the hub — it stores only ciphertext
          and never holds the keys, so it can't report who the readers are.
        </p>
        <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
          Manage readers from the CLI:{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.8rem]">agit a readers …</code>
        </p>
        {org && (
          <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
            Owned by <span className="font-mono text-foreground/80">{org}</span> — hub-assist escrow and
            offline recovery are set on the{" "}
            <Link to="/orgs" className="text-primary hover:underline">
              org page
            </Link>
            .
          </p>
        )}
      </div>
    </section>
  )
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="min-w-0 text-right text-foreground">{children}</dd>
    </div>
  )
}

// The "Clone" card: the one command that pulls this agent down, copyable.
function CloneCard({ cloneUrl }: { cloneUrl: string }) {
  return (
    <section className="rounded-lg border bg-card p-4">
      <h2 className="mb-3 text-sm font-semibold">Clone</h2>
      <CommandBlock code={`agit a clone ${cloneUrl}`} />
    </section>
  )
}

// The "Recent commits" card: the agent store's history, readable rather than micro-type.
function CommitsCard({ history }: { history: { sha: string; subject: string }[] }) {
  return (
    <section className="rounded-lg border bg-card p-4">
      <h2 className="mb-3 text-sm font-semibold">Recent commits</h2>
      {history.length === 0 ? (
        <p className="text-sm text-muted-foreground">No commits yet.</p>
      ) : (
        <ul className="flex flex-col gap-2 text-sm">
          {history.map((h) => (
            <li key={h.sha} className="flex items-baseline gap-2">
              <code className="shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-[0.8rem]">{h.sha}</code>
              <span className="truncate text-muted-foreground">{h.subject}</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

// Star is the caller's own bookmark: gated at Read, so it needs only a sign-in (the server refuses an
// anonymous or read-only-token caller). Signed out, the count still shows, read-only.
function StarButton({
  owner,
  name,
  starred,
  stars,
  onDone,
}: {
  owner: string
  name: string
  starred: boolean
  stars: number
  onDone: () => void
}) {
  const { me } = useSession()
  const [busy, setBusy] = useState(false)

  async function toggle() {
    setBusy(true)
    try {
      await api.starAgent(owner, name, !starred)
      onDone()
    } catch {
      // A failed star is not worth a wall; the count just won't move.
    } finally {
      setBusy(false)
    }
  }

  if (!me) {
    return (
      <Badge variant="muted" className="gap-1 font-mono text-[0.72rem]">
        <Star className="size-3.5" />
        {stars}
      </Badge>
    )
  }
  return (
    <Button
      variant={starred ? "secondary" : "outline"}
      size="sm"
      disabled={busy}
      onClick={toggle}
      aria-pressed={starred}
      title={starred ? "Unstar" : "Star"}
    >
      <Star className={cn("size-4", starred && "fill-current text-primary")} />
      <span className="font-mono tabular-nums">{stars}</span>
    </Button>
  )
}

// Fork is a write the caller performs into their own namespace; the server needs a sign-in and a
// write token. The 201 carries the fork's full_name to route to.
function ForkButton({ owner, name }: { owner: string; name: string }) {
  const { me } = useSession()
  const nav = useNavigate()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  if (!me) return null

  async function fork() {
    setBusy(true)
    setError("")
    try {
      const rec = await api.forkAgent(owner, name)
      const [forkOwner, forkName] = rec.full_name.split("/")
      nav(`/agent/${forkOwner}/${forkName}`)
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
      setBusy(false)
    }
  }

  return (
    <>
      <Button variant="outline" size="sm" disabled={busy} onClick={fork} title="Fork into your namespace">
        <GitFork className="size-4" />
        {busy ? "Forking…" : "Fork"}
      </Button>
      {error && (
        <span role="alert" className="text-xs text-destructive">
          {error}
        </span>
      )}
    </>
  )
}
