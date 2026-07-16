import { useState, type ReactNode, type FormEvent } from "react"
import { Link, useNavigate, useParams } from "react-router-dom"
import { GitBranch, Globe, Lock, Trash2, UserPlus } from "lucide-react"

import { api, type AgentPage, type Member, type Role, type Visibility } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select } from "@/components/ui/select"
import { Badge } from "@/components/ui/badge"
import { CopyButton } from "@/components/CopyButton"
import { Eyebrow, Forbidden, LoadError } from "@/components/States"
import { validateName } from "@/pages/NewAgent"

function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "—"
  if (n < 1024) return `${n} B`
  const units = ["KB", "MB", "GB", "TB"]
  let v = n / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`
}

export function Settings() {
  const { name = "" } = useParams()
  const { data, loading, error, status, forbidden, reload } = useGuarded(() => api.agent(name), [name])

  if (forbidden) return <Forbidden />
  if (loading) return <p className="py-10 text-muted-foreground">Loading…</p>
  if (error && status !== 401) return <LoadError message={error} />
  if (!data) return null

  // The server computes the caller's effective role and returns it. Take its answer rather
  // than re-deriving one here: a second opinion is a second thing to get wrong.
  const canAdmin = data.role === "owner" || data.role === "admin"

  return (
    <div className="flex flex-col gap-8">
      <header>
        <Link to={`/agent/${name}`} className="eyebrow hover:text-foreground">
          ← {name}
        </Link>
        <div className="mt-2 flex flex-wrap items-center gap-2.5">
          <h1 className="font-mono text-2xl font-bold tracking-tight">{name}</h1>
          <VisibilityBadge v={data.visibility} />
          {data.role && (
            <Badge variant={data.role === "owner" ? "default" : "muted"} className="font-mono text-[0.6rem]">
              {data.role}
            </Badge>
          )}
        </div>
        <p className="mt-1 text-sm text-muted-foreground">
          {canAdmin ? "Settings" : "Settings — read-only; you aren't the owner or an admin."}
        </p>
      </header>

      <Identity data={data} />
      <Bind data={data} name={name} />
      <Environments data={data} />
      <Branches data={data} />
      <Members data={data} name={name} canAdmin={canAdmin} reload={reload} />
      {canAdmin && <Rename name={name} />}
      {canAdmin && <VisibilityControl name={name} current={data.visibility} reload={reload} />}
      {canAdmin && <Danger name={name} />}
    </div>
  )
}

function VisibilityBadge({ v }: { v: Visibility }) {
  return v === "public" ? (
    <Badge variant="muted" className="gap-1">
      <Globe className="size-3" />
      public
    </Badge>
  ) : (
    <Badge variant="muted" className="gap-1">
      <Lock className="size-3" />
      private
    </Badge>
  )
}

function Section({ title, desc, children }: { title: string; desc?: string; children: ReactNode }) {
  return (
    <section>
      <Eyebrow className="mb-1.5">{title}</Eyebrow>
      {desc && <p className="mb-3 text-sm text-muted-foreground">{desc}</p>}
      {children}
    </section>
  )
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 border-t px-4 py-3 first:border-t-0">
      <span className="eyebrow">{label}</span>
      <div className="flex min-w-0 items-center gap-2">{children}</div>
    </div>
  )
}

function Identity({ data }: { data: AgentPage }) {
  return (
    <Section title="identity" desc="The aid is the identity and survives a rename. The name is only a label.">
      <div className="rounded-lg border bg-card">
        <Row label="aid">
          {/* An empty store has no agent.toml until the first push, so the server reports a
              null aid. Say "not pushed yet" rather than invent one. */}
          <code className="truncate font-mono text-sm">{data.aid || "not pushed yet"}</code>
          {data.aid && <CopyButton value={data.aid} size="icon" variant="ghost" />}
        </Row>
        <Row label="clone url">
          <code className="truncate font-mono text-sm">{data.clone_url || "—"}</code>
          {data.clone_url && <CopyButton value={data.clone_url} size="icon" variant="ghost" />}
        </Row>
        <Row label="owner">
          <span className="font-mono text-sm">{data.owner || "—"}</span>
        </Row>
        <Row label="size">
          <span className="font-mono text-sm tabular-nums">{formatBytes(data.size_bytes)}</span>
        </Row>
        <Row label="sessions">
          <span className="font-mono text-sm tabular-nums">{data.total}</span>
        </Row>
        <Row label="runtimes">
          {/* claude-code and codex are peers: same styling, and the server's order stands. */}
          <div className="flex flex-wrap gap-1.5">
            {data.runtimes?.length ? (
              data.runtimes.map((r) => (
                <Badge key={r} variant="muted" className="font-mono">
                  {r}
                </Badge>
              ))
            ) : (
              <span className="text-sm text-muted-foreground">—</span>
            )}
          </div>
        </Row>
      </div>
    </Section>
  )
}

function Bind({ data, name }: { data: AgentPage; name: string }) {
  const snippet = `[agent]
id = "${data.aid || "agt_…"}"
name = "${name}"
remote = "${data.clone_url || "http://HOST:PORT/" + name + ".git"}"`

  return (
    <Section
      title="bind a code repo"
      desc="An .agit.toml at the repo root files that repo's sessions under this agent."
    >
      <div className="relative">
        <pre className="overflow-auto rounded-lg border bg-muted p-4 pr-14 font-mono text-[0.75rem] leading-relaxed">
          {snippet}
        </pre>
        <CopyButton value={snippet} size="icon" variant="ghost" className="absolute right-2 top-2" />
      </div>
    </Section>
  )
}

function Environments({ data }: { data: AgentPage }) {
  return (
    <Section title="environments" desc="Which code repos this agent's sessions came from.">
      {data.environments?.length ? (
        <div className="overflow-hidden rounded-lg border bg-card">
          {data.environments.map((e) => (
            <div
              key={e.env ?? "—"}
              className="grid grid-cols-[1fr_auto_auto] items-center gap-4 border-t px-4 py-2.5 first:border-t-0"
            >
              {/* Sessions written under the old layout carry no environment; the server sends
                  null for them. */}
              <code className="truncate font-mono text-sm">
                {e.env ?? <span className="text-muted-foreground">unrecorded</span>}
              </code>
              <span className="font-mono text-[0.72rem] tabular-nums text-muted-foreground">
                {e.sessions} {e.sessions === 1 ? "session" : "sessions"}
              </span>
              <span className="font-mono text-[0.72rem] text-muted-foreground">{e.last}</span>
            </div>
          ))}
        </div>
      ) : (
        <p className="rounded-lg border bg-card px-4 py-6 text-sm text-muted-foreground">
          No environments recorded yet.
        </p>
      )}
    </Section>
  )
}

function Branches({ data }: { data: AgentPage }) {
  return (
    <Section title="branches">
      {data.branches?.length ? (
        <div className="overflow-hidden rounded-lg border bg-card">
          {data.branches.map((b) => (
            <div
              key={b.name}
              className="grid grid-cols-[1fr_auto_auto] items-center gap-4 border-t px-4 py-2.5 first:border-t-0"
            >
              <span className="flex min-w-0 items-center gap-1.5">
                <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />
                <code className="truncate font-mono text-sm">{b.name}</code>
              </span>
              <code className="font-mono text-[0.72rem] text-muted-foreground">{b.commit}</code>
              <span className="font-mono text-[0.72rem] text-muted-foreground">{b.when}</span>
            </div>
          ))}
        </div>
      ) : (
        <p className="rounded-lg border bg-card px-4 py-6 text-sm text-muted-foreground">No branches yet.</p>
      )}
    </Section>
  )
}

const ROLES: Role[] = ["read", "write", "admin"]

function Members({
  data,
  name,
  canAdmin,
  reload,
}: {
  data: AgentPage
  name: string
  canAdmin: boolean
  reload: () => void
}) {
  const [username, setUsername] = useState("")
  const [role, setRole] = useState<Role>("read")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function add(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api.addMember(name, username.trim(), role)
      setUsername("")
      setRole("read")
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  // POST overwrites an existing member's role, so a change is the same call as an add.
  async function changeRole(m: Member, next: Role) {
    setError("")
    try {
      await api.addMember(name, m.username, next)
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    }
  }

  async function remove(m: Member) {
    setError("")
    try {
      await api.removeMember(name, m.username)
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    }
  }

  return (
    <Section title="members" desc="A private agent is readable by its owner and the people listed here.">
      <div className="overflow-hidden rounded-lg border bg-card">
        <div className="flex items-center justify-between gap-4 px-4 py-2.5">
          <span className="font-mono text-sm font-semibold">{data.owner || "—"}</span>
          <Badge variant="muted" className="text-[0.6rem]">
            owner
          </Badge>
        </div>
        {data.members?.map((m) => (
          <div key={m.username} className="flex items-center justify-between gap-4 border-t px-4 py-2.5">
            <span className="truncate font-mono text-sm">{m.username}</span>
            <div className="flex items-center gap-2">
              {canAdmin ? (
                <>
                  <Select
                    value={m.role}
                    onChange={(e) => changeRole(m, e.target.value as Role)}
                    className="h-8 w-[104px] text-xs"
                    aria-label={`Role for ${m.username}`}
                  >
                    {ROLES.map((r) => (
                      <option key={r} value={r}>
                        {r}
                      </option>
                    ))}
                  </Select>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-8 text-muted-foreground hover:text-destructive"
                    onClick={() => remove(m)}
                    aria-label={`Remove ${m.username}`}
                  >
                    <Trash2 />
                  </Button>
                </>
              ) : (
                <Badge variant="muted" className="text-[0.6rem]">
                  {m.role}
                </Badge>
              )}
            </div>
          </div>
        ))}
        {!data.members?.length && (
          <p className="border-t px-4 py-4 text-sm text-muted-foreground">No members besides the owner.</p>
        )}
      </div>

      {canAdmin && (
        <form onSubmit={add} className="mt-3 flex flex-wrap items-center gap-2">
          <Input
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="username"
            className="w-[200px]"
            required
          />
          <Select
            value={role}
            onChange={(e) => setRole(e.target.value as Role)}
            className="w-[120px]"
            aria-label="Role for the new member"
          >
            {ROLES.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </Select>
          <Button type="submit" variant="outline" disabled={busy || !username.trim()}>
            <UserPlus />
            Add
          </Button>
        </form>
      )}

      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </Section>
  )
}

function Rename({ name }: { name: string }) {
  const nav = useNavigate()
  const [next, setNext] = useState(name)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  const invalid = next && next !== name ? validateName(next) : null

  async function submit(e: FormEvent) {
    e.preventDefault()
    const bad = validateName(next)
    if (bad) return setError(bad)
    setBusy(true)
    setError("")
    try {
      await api.patchAgent(name, { name: next })
      nav(`/agent/${next}/settings`, { replace: true })
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Section title="rename" desc="The aid doesn't change. Existing clones need their remote updated.">
      <form onSubmit={submit} className="flex flex-wrap items-center gap-2">
        <Input value={next} onChange={(e) => setNext(e.target.value)} className="w-[240px]" />
        <Button type="submit" variant="outline" disabled={busy || next === name || !next || !!invalid}>
          Rename
        </Button>
      </form>
      {invalid && <p className="mt-2 text-sm text-destructive">{invalid}</p>}
      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </Section>
  )
}

// Named to stay clear of the Visibility type.
function VisibilityControl({
  name,
  current,
  reload,
}: {
  name: string
  current: Visibility
  reload: () => void
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function set(v: Visibility) {
    if (v === current) return
    setBusy(true)
    setError("")
    try {
      await api.patchAgent(name, { visibility: v })
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Section title="visibility">
      <div className="flex flex-wrap items-center gap-2">
        <Button
          variant={current === "private" ? "default" : "outline"}
          size="sm"
          disabled={busy}
          onClick={() => set("private")}
        >
          <Lock />
          private
        </Button>
        <Button
          variant={current === "public" ? "default" : "outline"}
          size="sm"
          disabled={busy}
          onClick={() => set("public")}
        >
          <Globe />
          public
        </Button>
      </div>
      <p className="mt-2 max-w-[62ch] text-[0.78rem] text-muted-foreground">
        {current === "public"
          ? "Anyone who can reach this hub can read every session here."
          : "Only the owner and members can read it."}
      </p>
      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </Section>
  )
}

function Danger({ name }: { name: string }) {
  const nav = useNavigate()
  const [typed, setTyped] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function del() {
    setBusy(true)
    setError("")
    try {
      await api.deleteAgent(name)
      nav("/", { replace: true })
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
      setBusy(false)
    }
  }

  return (
    <section className="rounded-lg border border-destructive/40 bg-destructive/5 p-5">
      <Eyebrow className="text-destructive">danger zone</Eyebrow>
      <p className="mb-1 mt-1.5 font-semibold">Delete this agent</p>
      <p className="mb-4 max-w-[62ch] text-sm text-muted-foreground">
        Takes every session and all history with it, unrecoverably. Clones already out there
        are untouched. Tokens bound to this name are revoked with it. Type{" "}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">{name}</code> to confirm.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          placeholder={name}
          className="w-[240px]"
          aria-label="Type the agent's name to confirm deletion"
        />
        <Button
          variant="default"
          className="bg-destructive text-white hover:bg-destructive/90"
          disabled={busy || typed !== name}
          onClick={del}
        >
          <Trash2 />
          {busy ? "Deleting…" : "Delete permanently"}
        </Button>
      </div>
      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </section>
  )
}
