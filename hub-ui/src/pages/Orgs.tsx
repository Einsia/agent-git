import { useState, type FormEvent } from "react"
import { Link } from "react-router-dom"
import { Building2, KeyRound, Mail, Plus, ShieldAlert, Trash2, UserPlus } from "lucide-react"

import { api, type EscrowMode, type Org, type OrgMember, type OrgRole } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select } from "@/components/ui/select"
import { Badge } from "@/components/ui/badge"
import { Label } from "@/components/ui/label"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { CopyButton } from "@/components/CopyButton"
import { Eyebrow, LoadError } from "@/components/States"

// Mirrors the server's key check (src/bin/agit-hub/api.rs api_org_recovery): a 64-hex-char (32-byte)
// X25519 public key. Advisory only — the server is the gate.
function validateRecoveryKey(key: string): string | null {
  const k = key.trim().toLowerCase()
  if (!k) return "Enter a key."
  if (!/^[0-9a-f]{64}$/.test(k)) return "Must be 64 hex characters (a 32-byte X25519 public key)."
  return null
}

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

  const [error, setError] = useState("")

  // Membership is invitation-only — an existing member's role can be changed here, but new members
  // join only by accepting an invitation (see OrgInvitations below). The POST changes a role in place.
  async function changeRole(m: OrgMember, next: OrgRole) {
    setError("")
    try {
      await api.setOrgMemberRole(org.name, m.username, next)
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
        <h2 className="font-mono text-lg font-semibold">
          <Link to={`/orgs/${encodeURIComponent(org.name)}`} className="hover:text-primary hover:underline">
            {org.name}
          </Link>
        </h2>
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

      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}

      {canManage && <MemberCreateControl org={org.name} />}
      {canManage && <OrgInvitations org={org.name} />}
      {canManage && <OrgCrypto org={org.name} />}
    </div>
  )
}

// The org's member-create policy: whether a plain member (not just an admin) may create agents under
// the org. Admin-only (this whole card only renders to a manager). The current value comes off the org
// detail (GET /api/orgs/<name> now carries members_can_create); the toggle hits POST .../settings.
function MemberCreateControl({ org }: { org: string }) {
  const { data, loading, error, reload } = useAsync(() => api.orgCrypto(org), [org])
  const [busy, setBusy] = useState(false)
  const [actionError, setActionError] = useState("")

  async function set(next: boolean) {
    if (busy || data?.members_can_create === next) return
    setBusy(true)
    setActionError("")
    try {
      await api.setMemberCreate(org, next)
      reload()
    } catch (err) {
      setActionError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="mt-5 border-t pt-4">
      <Eyebrow className="mb-2">member permissions</Eyebrow>
      {loading && <p className="text-sm text-muted-foreground">Loading…</p>}
      {error && <p className="text-sm text-muted-foreground">Couldn't load org settings.</p>}
      {data && (
        <div className="rounded-lg border bg-card/60 p-4">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm font-medium">Members can create agents</span>
            <Badge variant={data.members_can_create ? "default" : "muted"} className="font-mono text-[0.6rem]">
              {data.members_can_create ? "on" : "off"}
            </Badge>
          </div>
          <p className="mt-1.5 max-w-[62ch] text-[0.78rem] text-muted-foreground">
            When on, any member may create agents owned by this org. When off, only admins can.
          </p>
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <Button
              variant={data.members_can_create ? "default" : "outline"}
              size="sm"
              disabled={busy}
              onClick={() => set(true)}
            >
              <UserPlus />
              Members
            </Button>
            <Button
              variant={!data.members_can_create ? "default" : "outline"}
              size="sm"
              disabled={busy}
              onClick={() => set(false)}
            >
              Admins only
            </Button>
          </div>
          {actionError && (
            <p role="alert" className="mt-2 text-sm text-destructive">
              {actionError}
            </p>
          )}
        </div>
      )}
    </div>
  )
}

// Invite someone who then accepts (POST .../invitations), distinct from "Add directly" above which
// grants membership at once. Org-admin gated on the server. Lists still-pending invitations with a
// Revoke on each. useAsync (not useGuarded): a stray 403/404 here shouldn't bounce the whole page.
function OrgInvitations({ org }: { org: string }) {
  const { data, error, reload } = useAsync(() => api.orgInvitations(org), [org])
  const [open, setOpen] = useState(false)
  const [actionError, setActionError] = useState("")
  const invites = data ?? []

  async function revoke(id: string) {
    setActionError("")
    try {
      await api.revokeInvitation(org, id)
      reload()
    } catch (err) {
      setActionError(String((err as Error)?.message ?? err))
    }
  }

  return (
    <div className="mt-5 border-t pt-4">
      <div className="mb-2 flex items-center justify-between gap-2">
        <Eyebrow>invitations</Eyebrow>
        <Button variant="outline" size="sm" onClick={() => setOpen(true)}>
          <Mail />
          Invite a member
        </Button>
      </div>
      {error ? (
        <p className="text-sm text-muted-foreground">Couldn't load invitations.</p>
      ) : invites.length === 0 ? (
        <p className="text-sm text-muted-foreground">No pending invitations.</p>
      ) : (
        <div className="overflow-hidden rounded-lg border">
          {invites.map((inv) => (
            <div
              key={inv.id}
              className="flex items-center justify-between gap-4 border-t px-4 py-2.5 first:border-t-0"
            >
              <div className="flex min-w-0 flex-wrap items-center gap-2">
                <span className="truncate font-mono text-sm">{inv.username}</span>
                <Badge variant="muted" className="text-[0.6rem]">
                  {inv.role}
                </Badge>
                <span className="text-[0.68rem] text-muted-foreground">pending</span>
              </div>
              <Button
                variant="ghost"
                size="icon"
                className="size-8 text-muted-foreground hover:text-destructive"
                onClick={() => revoke(inv.id)}
                aria-label={`Revoke invitation for ${inv.username}`}
              >
                <Trash2 />
              </Button>
            </div>
          ))}
        </div>
      )}
      {actionError && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {actionError}
        </p>
      )}
      <InviteDialog
        org={org}
        open={open}
        onOpenChange={setOpen}
        onInvited={() => {
          setOpen(false)
          reload()
        }}
      />
    </div>
  )
}

function InviteDialog({
  org,
  open,
  onOpenChange,
  onInvited,
}: {
  org: string
  open: boolean
  onOpenChange: (open: boolean) => void
  onInvited: () => void
}) {
  const [username, setUsername] = useState("")
  const [role, setRole] = useState<OrgRole>("member")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function submit() {
    if (!username.trim() || busy) return
    setBusy(true)
    setError("")
    try {
      await api.inviteOrgMember(org, username.trim().toLowerCase(), role)
      setUsername("")
      setRole("member")
      onInvited()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  if (!open) return null
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Invite a member</DialogTitle>
          <DialogDescription>
            They'll get a pending invitation to <span className="font-mono text-foreground/80">{org}</span>{" "}
            and join once they accept it.
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="flex flex-wrap items-end gap-3">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="invite-username">username</Label>
            <Input
              id="invite-username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && submit()}
              placeholder="alice"
              className="w-[200px]"
              autoCapitalize="none"
              autoComplete="off"
              autoFocus
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="invite-role">role</Label>
            <Select
              id="invite-role"
              value={role}
              onChange={(e) => setRole(e.target.value as OrgRole)}
              className="w-[120px]"
            >
              {ORG_ROLES.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </Select>
          </div>
          {error && (
            <p role="alert" className="w-full text-sm text-destructive">
              {error}
            </p>
          )}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={submit} disabled={busy || !username.trim()}>
            <Mail />
            {busy ? "Sending…" : "Send invitation"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

// The org's opt-in, hub-side crypto: escrow mode + the offline recovery recipient. Both are
// server-side and owner-manageable (this card only shows to a manager). The per-session reader set is
// NOT here — it lives in each client's keybox and the hub never sees it; readers are managed with the
// `agit a readers …` CLI.
function OrgCrypto({ org }: { org: string }) {
  const { data, loading, error, reload } = useAsync(() => api.orgCrypto(org), [org])

  return (
    <div className="mt-5 border-t pt-4">
      <Eyebrow className="mb-2">encryption &amp; recovery</Eyebrow>
      {loading && <p className="text-sm text-muted-foreground">Loading…</p>}
      {error && <p className="text-sm text-muted-foreground">Couldn't load encryption settings.</p>}
      {data && (
        <div className="flex flex-col gap-4">
          <EscrowControl org={org} mode={data.escrow_mode} onChanged={reload} />
          <RecoveryControl org={org} current={data.recovery_x25519} onChanged={reload} />
        </div>
      )}
    </div>
  )
}

function EscrowControl({
  org,
  mode,
  onChanged,
}: {
  org: string
  mode: EscrowMode
  onChanged: () => void
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function set(next: EscrowMode) {
    if (next === mode || busy) return
    setBusy(true)
    setError("")
    try {
      await api.setEscrowMode(org, next)
      onChanged()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="rounded-lg border bg-card/60 p-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium">Hub-assist escrow</span>
        <Badge variant={mode === "hub-assist" ? "default" : "muted"} className="font-mono text-[0.6rem]">
          {mode}
        </Badge>
      </div>
      <p className="mt-1.5 max-w-[62ch] text-[0.78rem] text-muted-foreground">
        When on, writers can escrow session content keys to the hub, and the hub may release them to a
        reader under the same access check as a git fetch.
      </p>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <Button
          variant={mode === "none" ? "default" : "outline"}
          size="sm"
          disabled={busy}
          onClick={() => set("none")}
        >
          Off
        </Button>
        <Button
          variant={mode === "hub-assist" ? "default" : "outline"}
          size="sm"
          disabled={busy}
          onClick={() => set("hub-assist")}
        >
          <ShieldAlert />
          Hub-assist
        </Button>
      </div>
      {mode === "hub-assist" && (
        <Alert variant="warn" className="mt-3">
          <ShieldAlert />
          <AlertTitle>Hub-assist re-trusts the hub</AlertTitle>
          <AlertDescription>
            The hub can release this org's escrowed session keys to any reader it authorizes. Leave it
            off if you want the hub to stay unable to hand out keys.
          </AlertDescription>
        </Alert>
      )}
      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </div>
  )
}

function RecoveryControl({
  org,
  current,
  onChanged,
}: {
  org: string
  current: string
  onChanged: () => void
}) {
  const [key, setKey] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  const keyError = key.trim() ? validateRecoveryKey(key) : null

  async function save() {
    const bad = validateRecoveryKey(key)
    if (bad) return setError(bad)
    setBusy(true)
    setError("")
    try {
      await api.setRecoveryRecipient(org, key.trim().toLowerCase())
      setKey("")
      onChanged()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  async function clear() {
    setBusy(true)
    setError("")
    try {
      await api.clearRecoveryRecipient(org)
      onChanged()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="rounded-lg border bg-card/60 p-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium">Offline recovery recipient</span>
        <Badge variant={current ? "default" : "muted"} className="font-mono text-[0.6rem]">
          {current ? "set" : "not set"}
        </Badge>
      </div>
      <p className="mt-1.5 max-w-[62ch] text-[0.78rem] text-muted-foreground">
        An X25519 public key the team key is also sealed to during{" "}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.72rem]">agit team rekey</code>,
        so an offline admin (not the hub) can recover it.
      </p>
      {current && (
        <div className="mt-2 flex items-center gap-2">
          <code className="min-w-0 flex-1 truncate rounded-md bg-muted px-2.5 py-1.5 font-mono text-xs">
            {current}
          </code>
          <CopyButton value={current} size="icon" variant="ghost" label="Copy recovery key" />
        </div>
      )}
      <div className="mt-3 flex flex-wrap items-end gap-2">
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`recovery-${org}`}>{current ? "replace key" : "set key"}</Label>
          <Input
            id={`recovery-${org}`}
            value={key}
            onChange={(e) => setKey(e.target.value)}
            placeholder="64 hex characters"
            className="w-[280px] font-mono text-xs"
            autoCapitalize="none"
            autoComplete="off"
            aria-invalid={!!keyError}
          />
        </div>
        <Button variant="outline" size="sm" disabled={busy || !key.trim() || !!keyError} onClick={save}>
          <KeyRound />
          Save
        </Button>
        {current && (
          <Button
            variant="ghost"
            size="sm"
            className="text-muted-foreground hover:text-destructive"
            disabled={busy}
            onClick={clear}
          >
            Clear
          </Button>
        )}
      </div>
      {keyError && <p className="mt-1 text-[0.78rem] text-destructive">{keyError}</p>}
      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </div>
  )
}
