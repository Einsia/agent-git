import { useEffect, useState, type FormEvent } from "react"
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom"
import { GitFork, GitPullRequest, Lock, Search, Settings2, ShieldCheck, Star } from "lucide-react"

import { api } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Input } from "@/components/ui/input"
import { Button, buttonVariants } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import { SessionCard } from "@/components/SessionCard"
import { Crumb } from "@/components/Crumb"
import { Forbidden, LoadError } from "@/components/States"
import { cn } from "@/lib/utils"

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

  return (
    <div>
      <Crumb owner={owner} name={name} />
      <div className="mb-6 flex flex-wrap items-center justify-between gap-3">
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
            <Badge variant={data.role === "owner" ? "default" : "muted"} className="font-mono text-[0.6rem]">
              {data.role}
            </Badge>
          )}
        </div>
        <div className="flex items-center gap-2">
          <form onSubmit={submitSearch} className="flex gap-2">
            <div className="relative">
              <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Search sessions…"
                className="w-[240px] pl-8"
              />
            </div>
            <Button type="submit">Search</Button>
          </form>
          {data && <StarButton owner={owner} name={name} starred={data.starred} stars={data.stars} onDone={reload} />}
          {data && <ForkButton owner={owner} name={name} />}
          {/* Merge requests are readable by anyone who can read the agent. */}
          <Link
            to={`/agent/${owner}/${name}/mrs`}
            aria-label="Merge requests"
            title="Merge requests"
            className={cn(buttonVariants({ variant: "ghost", size: "icon" }))}
          >
            <GitPullRequest className="size-4" />
          </Link>
          {/* Settings is readable by anyone who can read the agent; it just shows less to
              someone who can't administer it. */}
          <Link
            to={`/agent/${owner}/${name}/settings`}
            aria-label="Settings"
            title="Settings"
            className={cn(buttonVariants({ variant: "ghost", size: "icon" }))}
          >
            <Settings2 className="size-4" />
          </Link>
        </div>
      </div>

      <div className="grid grid-cols-1 gap-8 md:grid-cols-[1fr_260px]">
        <div>
          <div className="mb-3 flex items-baseline gap-2">
            <span className="eyebrow">sessions</span>
            {data && <span className="font-mono text-sm tabular-nums text-muted-foreground">{data.total}</span>}
          </div>

          {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
          {/* 401 is already on its way to the login form; don't flash an error behind it. */}
          {error && status !== 401 && <LoadError message={error} />}

          {data && data.sessions.length === 0 && (
            <p className="py-6 text-muted-foreground">
              {q ? `No sessions match “${q}”.` : "No sessions yet."}
            </p>
          )}

          <div className="flex flex-col gap-3">
            {data?.sessions.map((s) => <SessionCard key={s.id} owner={owner} name={name} s={s} />)}
          </div>

          {data && totalPages > 1 && (
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

        <aside className="text-sm">
          <h3 className="eyebrow mb-2">pull &amp; merge</h3>
          <pre className="overflow-auto rounded-md border bg-muted p-3 font-mono text-[0.72rem] leading-relaxed">
{`agit clone \\
  ${cloneUrl}
agit -a merge origin/main`}
          </pre>
          <h3 className="eyebrow mb-2 mt-6">commits</h3>
          <ul className="space-y-1 text-[0.8rem]">
            {data?.history.map((h) => (
              <li key={h.sha} className="truncate">
                <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.72rem]">{h.sha}</code>{" "}
                {h.subject}
              </li>
            ))}
          </ul>

          {data && <EncryptionInfo owner={data.owner} />}
        </aside>
      </div>
    </div>
  )
}

// Encryption visibility, read-only. The hub deliberately never holds the session keys, and it exposes
// no per-agent reader set or team generation over the API — that state lives in each client's keybox.
// So this is an honest, informational panel, not a management surface: it points at the `agit a
// readers …` CLI (the only place readers can be changed) and, for an org-owned agent, at the org page
// where the server-side escrow/recovery settings live. There is no client-side readers add/rm here —
// the browser has no keys to wrap.
function EncryptionInfo({ owner }: { owner: string | null }) {
  const org = owner?.startsWith("org:") ? owner.slice("org:".length) : null
  return (
    <section className="mt-6">
      <h3 className="eyebrow mb-2">encryption</h3>
      <div className="rounded-md border bg-card p-3">
        <Badge variant="muted" className="gap-1">
          <ShieldCheck className="size-3" />
          end-to-end
        </Badge>
        <p className="mt-2 text-[0.78rem] leading-relaxed text-muted-foreground">
          Sessions can be encrypted per-session before they reach the hub — it stores only ciphertext
          and never holds the keys, so it can't report who the readers are.
        </p>
        <p className="mt-2 text-[0.78rem] leading-relaxed text-muted-foreground">
          Manage readers from the CLI:{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.72rem]">agit a readers …</code>
        </p>
        {org && (
          <p className="mt-2 text-[0.78rem] leading-relaxed text-muted-foreground">
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
