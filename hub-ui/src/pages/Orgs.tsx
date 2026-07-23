import { useState, type FormEvent } from "react"
import { Link } from "react-router-dom"
import { Building2, Plus } from "lucide-react"

import { api, type Org } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Badge } from "@/components/ui/badge"
import { LoadError } from "@/components/States"

// Mirrors valid_username() in src/hub/store.rs — orgs share the username rules: 2-32 chars,
// lowercase [a-z0-9._-], no leading dot. Advisory only; the server is the gate, and it lowercases
// (normalize_username) before checking, so we validate the normalized form to match its verdict.
export function validateOrgName(name: string): string | null {
  if (name.length < 2) return "At least 2 characters."
  if (name.length > 32) return "32 characters max."
  if (name.startsWith(".")) return "Can't start with a dot."
  if (!/^[a-z0-9._-]+$/.test(name)) return "Lowercase letters, digits, dot, underscore and dash only."
  return null
}

export function Orgs() {
  const { me } = useSession()
  // orgs answers 401 to a signed-out caller — useGuarded carries them to the sign-in form and
  // back, exactly like /tokens.
  const { data, loading, error, status, reload } = useGuarded(() => api.orgs(), [])

  return (
    <div className="flex flex-col gap-8">
      <header>
        <h1 className="text-2xl font-bold tracking-tight">Organizations</h1>
        <p className="mt-2 max-w-[62ch] text-sm text-muted-foreground">
          A team that can own agents together. Every member gets write on the org's agents; an
          admin also manages the roster. Open one to see its agents, members and settings.
        </p>
      </header>

      <CreateOrg reload={reload} />

      <section>
        <h2 className="mb-4 flex items-baseline gap-2 text-lg font-semibold tracking-tight">
          Your organizations
          {data && (
            <span className="font-mono text-base tabular-nums text-muted-foreground">{data.length}</span>
          )}
        </h2>
        {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
        {error && status !== 401 && <LoadError message={error} />}
        {data && data.length === 0 && (
          <p className="rounded-lg border bg-card px-4 py-8 text-muted-foreground">
            You aren't in any orgs yet. Create one above.
          </p>
        )}
        {data && data.length > 0 && (
          <div className="flex flex-col gap-3">
            {data.map((o) => (
              <OrgRow key={o.name} org={o} me={me?.username ?? null} />
            ))}
          </div>
        )}
      </section>
    </div>
  )
}

function CreateOrg({ reload }: { reload: () => void }) {
  const [name, setName] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  // The server normalizes (trim + lowercase) before validating, so validate that same form.
  const normalized = name.trim().toLowerCase()
  const nameError = normalized ? validateOrgName(normalized) : null

  async function submit(e: FormEvent) {
    e.preventDefault()
    const bad = validateOrgName(normalized)
    if (bad) return setError(bad)
    setBusy(true)
    setError("")
    try {
      await api.createOrg(normalized)
      setName("")
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="rounded-lg border bg-card p-4">
      <h2 className="text-sm font-semibold">Create an organization</h2>
      <form onSubmit={submit} className="mt-4 flex flex-col gap-3">
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex flex-col gap-1.5">
            <span className="text-sm font-medium">Name</span>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="acme"
              className="w-[220px]"
              aria-invalid={!!nameError}
              required
            />
          </label>
          <Button type="submit" disabled={busy || !normalized || !!nameError}>
            <Plus />
            {busy ? "Creating…" : "Create org"}
          </Button>
        </div>
        <p className="text-sm text-muted-foreground">
          You become its first admin. Names are lowercase [a-z0-9._-], 2-32 characters.
        </p>
        {nameError && <p className="text-sm text-destructive">{nameError}</p>}
        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}
      </form>
    </section>
  )
}

// A single navigation row: the org name links to its detail page, plus your role and the member
// count. All per-org management (roster, invitations, settings) lives on the detail tabs now.
function OrgRow({ org, me }: { org: Org; me: string | null }) {
  const mine = org.members.find((m) => m.username === me)
  return (
    <Link
      to={`/orgs/${encodeURIComponent(org.name)}`}
      className="flex flex-wrap items-center gap-3 rounded-lg border bg-card p-4 transition-colors hover:border-primary/50 hover:bg-accent/40"
    >
      <Building2 className="size-5 text-muted-foreground" />
      <span className="font-mono text-lg font-semibold">{org.name}</span>
      {mine && (
        <Badge variant="muted" className="font-mono">
          {mine.role}
        </Badge>
      )}
      <Badge variant="muted" className="ml-auto">
        {org.members.length} {org.members.length === 1 ? "member" : "members"}
      </Badge>
    </Link>
  )
}
