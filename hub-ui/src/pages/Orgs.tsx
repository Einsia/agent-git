import { useState, type FormEvent } from "react"
import { Building2, Plus, Trash2, UserPlus } from "lucide-react"

import { api, type Org, type OrgMember, type OrgRole } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select } from "@/components/ui/select"
import { Badge } from "@/components/ui/badge"
import { Eyebrow, LoadError } from "@/components/States"

const ORG_ROLES: OrgRole[] = ["member", "admin"]

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
        <span className="eyebrow">organizations</span>
        <h1 className="mt-1 text-2xl font-bold tracking-tight">Organizations</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          A team that can own agents together. Every member gets write on the org's agents; an
          admin also manages the roster.
        </p>
      </header>

      <CreateOrg reload={reload} />

      <section>
        <Eyebrow className="mb-3">your orgs</Eyebrow>
        {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
        {error && status !== 401 && <LoadError message={error} />}
        {data && data.length === 0 && (
          <p className="rounded-lg border bg-card px-4 py-8 text-muted-foreground">
            You aren't in any orgs yet. Create one above.
          </p>
        )}
        {data && data.length > 0 && (
          <div className="flex flex-col gap-4">
            {data.map((o) => (
              <OrgCard
                key={o.name}
                org={o}
                me={me?.username ?? null}
                siteAdmin={!!me?.is_admin}
                reload={reload}
              />
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
    <section>
      <Eyebrow className="mb-3">new org</Eyebrow>
      <form onSubmit={submit} className="flex flex-col gap-3 rounded-lg border bg-card p-4">
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">name</span>
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
        <p className="text-[0.75rem] text-muted-foreground">
          You become its first admin. Names are lowercase [a-z0-9._-], 2-32 characters.
        </p>
        {nameError && <p className="text-[0.78rem] text-destructive">{nameError}</p>}
        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}
      </form>
    </section>
  )
}

function OrgCard({
  org,
  me,
  siteAdmin,
  reload,
}: {
  org: Org
  me: string | null
  siteAdmin: boolean
  reload: () => void
}) {
  // Managing the roster needs org-admin (or a site admin), the same gate the server enforces.
  const canManage = siteAdmin || org.members.some((m) => m.username === me && m.role === "admin")

  const [username, setUsername] = useState("")
  const [role, setRole] = useState<OrgRole>("member")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function add(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api.addOrgMember(org.name, username.trim(), role)
      setUsername("")
      setRole("member")
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  // POST overwrites an existing member's role, so a change is the same call as an add.
  async function changeRole(m: OrgMember, next: OrgRole) {
    setError("")
    try {
      await api.addOrgMember(org.name, m.username, next)
      reload()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    }
  }

  async function remove(m: OrgMember) {
    setError("")
    try {
      await api.removeOrgMember(org.name, m.username)
      reload()
    } catch (err) {
      // The server refuses to remove the last admin (409); surface its own wording.
      setError(String((err as Error)?.message ?? err))
    }
  }

  return (
    <div className="rounded-lg border bg-card p-4">
      <div className="mb-3 flex flex-wrap items-center gap-2.5">
        <Building2 className="size-4 text-muted-foreground" />
        <h2 className="font-mono text-lg font-semibold">{org.name}</h2>
        <Badge variant="muted" className="text-[0.6rem]">
          {org.members.length} {org.members.length === 1 ? "member" : "members"}
        </Badge>
        {!canManage && (
          <span className="text-[0.72rem] text-muted-foreground">read-only — you aren't an org admin</span>
        )}
      </div>

      <div className="overflow-hidden rounded-lg border">
        {org.members.map((m) => (
          <div
            key={m.username}
            className="flex items-center justify-between gap-4 border-t px-4 py-2.5 first:border-t-0"
          >
            <span className="truncate font-mono text-sm">{m.username}</span>
            <div className="flex items-center gap-2">
              {canManage ? (
                <>
                  <Select
                    value={m.role}
                    onChange={(e) => changeRole(m, e.target.value as OrgRole)}
                    className="h-8 w-[104px] text-xs"
                    aria-label={`Role for ${m.username}`}
                  >
                    {ORG_ROLES.map((r) => (
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
      </div>

      {canManage && (
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
            onChange={(e) => setRole(e.target.value as OrgRole)}
            className="w-[120px]"
            aria-label="Role for the new member"
          >
            {ORG_ROLES.map((r) => (
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
    </div>
  )
}
