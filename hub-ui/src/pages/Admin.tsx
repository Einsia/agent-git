import { useCallback, useEffect, useState } from "react"
import { Ban, CircleCheck, KeyRound, Loader2, ShieldCheck, ShieldOff, UserPlus } from "lucide-react"

import { api, ApiError, type RosterUser } from "@/lib/api"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Badge } from "@/components/ui/badge"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Eyebrow } from "@/components/States"

interface LogRow {
  at: string
  user: string
  action: string
  ok: boolean
  detail: string
}

export function Admin() {
  const { me, loading } = useSession()

  if (loading) return <p className="py-10 text-muted-foreground">Loading…</p>

  // Client-side gate for a clean UX. The SERVER is the real gate: every /api/users/... action below
  // is 403 for a non-admin no matter what this component renders, so this check only decides what to
  // show, never what's allowed.
  if (!me?.is_admin) {
    return (
      <div className="readout rounded-lg border px-6 py-12 text-center">
        <ShieldOff className="mx-auto mb-3 size-6 text-muted-foreground" />
        <p className="mb-1 font-semibold">Not authorized</p>
        <p className="mx-auto max-w-[46ch] text-sm text-muted-foreground">
          The admin panel is for site administrators. If you need something here, ask an admin — the
          hub enforces this server-side, so signing in again won't change it.
        </p>
      </div>
    )
  }

  return <AdminPanel meUsername={me.username} />
}

function AdminPanel({ meUsername }: { meUsername: string }) {
  const [users, setUsers] = useState<RosterUser[] | null>(null)
  const [loadError, setLoadError] = useState("")
  // The username currently mid-action, so only its row's buttons show a spinner / go disabled.
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState("")
  const [resetFor, setResetFor] = useState<string | null>(null)
  const [createOpen, setCreateOpen] = useState(false)
  const [log, setLog] = useState<LogRow[]>([])

  const load = useCallback(async () => {
    setLoadError("")
    try {
      const r = await api.listUsers()
      setUsers(r.users)
    } catch (err) {
      setLoadError(String((err as Error)?.message ?? err))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  function record(user: string, action: string, ok: boolean, detail: string) {
    setLog((prev) => [{ at: new Date().toLocaleTimeString(), user, action, ok, detail }, ...prev].slice(0, 20))
  }

  // Run a per-user mutation, then refresh the roster. `busy` scopes the spinner to the acting row.
  async function act(user: string, action: string, run: () => Promise<string>) {
    if (busy) return
    setBusy(user)
    setError("")
    try {
      const detail = await run()
      record(user, action, true, detail)
      await load()
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : String((err as Error)?.message ?? err)
      setError(msg)
      record(user, action, false, msg)
    } finally {
      setBusy(null)
    }
  }

  const clear2fa = (user: string) =>
    act(user, "clear 2FA", async () => {
      await api.adminDisable2fa(user)
      return "2FA cleared"
    })

  const disable = (user: string) =>
    act(user, "disable", async () => {
      const r = await api.disableUser(user)
      return `disabled; revoked ${r.revoked_sessions} session(s)`
    })

  const enable = (user: string) =>
    act(user, "enable", async () => {
      await api.enableUser(user)
      return "enabled"
    })

  // The number of admins still able to log in — the client mirror of the server's last-admin guard, so
  // the Disable button on the sole admin is pre-disabled rather than only failing on submit.
  const enabledAdmins = (users ?? []).filter((u) => u.is_admin && !u.disabled).length

  return (
    <div className="flex flex-col gap-8">
      <header>
        <span className="eyebrow">admin</span>
        <h1 className="mt-1 text-2xl font-bold tracking-tight">Admin</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Manage the user roster and recover locked-out accounts. Every action here is enforced
          server-side for site administrators.
        </p>
      </header>

      <section>
        <div className="mb-3 flex items-end justify-between gap-3">
          <Eyebrow>user roster</Eyebrow>
          <Button size="sm" onClick={() => setCreateOpen(true)}>
            <UserPlus />
            New user
          </Button>
        </div>

        {error && (
          <p role="alert" className="mb-3 text-sm text-destructive">
            {error}
          </p>
        )}

        {loadError ? (
          <div className="rounded-lg border bg-card p-4">
            <p className="text-sm text-destructive">Couldn't load the roster: {loadError}</p>
            <Button variant="outline" size="sm" className="mt-3" onClick={() => void load()}>
              Retry
            </Button>
          </div>
        ) : users === null ? (
          <p className="rounded-lg border bg-card p-4 text-sm text-muted-foreground">Loading roster…</p>
        ) : (
          <div className="overflow-x-auto rounded-lg border bg-card">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>user</TableHead>
                  <TableHead>role</TableHead>
                  <TableHead>2FA</TableHead>
                  <TableHead>email</TableHead>
                  <TableHead>status</TableHead>
                  <TableHead className="text-right">actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {users.map((u) => {
                  const isSelf = u.username === meUsername
                  const rowBusy = busy === u.username
                  // Mirror the server guards: can't disable yourself or the last enabled admin.
                  const lastAdmin = u.is_admin && !u.disabled && enabledAdmins <= 1
                  const disableBlocked = isSelf || lastAdmin
                  return (
                    <TableRow key={u.username} className={u.disabled ? "opacity-60" : undefined}>
                      <TableCell className="font-mono text-sm">
                        {u.username}
                        {isSelf && <span className="ml-1.5 text-xs text-muted-foreground">(you)</span>}
                      </TableCell>
                      <TableCell>
                        {u.is_admin ? (
                          <Badge variant="muted" className="text-primary">
                            <ShieldCheck className="size-3" />
                            admin
                          </Badge>
                        ) : (
                          <span className="text-sm text-muted-foreground">member</span>
                        )}
                      </TableCell>
                      <TableCell className="text-sm text-muted-foreground">{u.totp_enabled ? "on" : "off"}</TableCell>
                      <TableCell className="text-sm text-muted-foreground">
                        {u.email_verified ? "verified" : "unverified"}
                      </TableCell>
                      <TableCell>
                        {u.disabled ? (
                          <Badge variant="muted" className="text-kind-warn">
                            disabled
                          </Badge>
                        ) : (
                          <Badge variant="muted" className="text-kind-edit">
                            active
                          </Badge>
                        )}
                      </TableCell>
                      <TableCell>
                        <div className="flex items-center justify-end gap-1.5">
                          {u.disabled ? (
                            <Button
                              variant="outline"
                              size="sm"
                              disabled={rowBusy}
                              onClick={() => void enable(u.username)}
                            >
                              {rowBusy ? <Loader2 className="animate-spin" /> : <CircleCheck />}
                              Enable
                            </Button>
                          ) : (
                            <Button
                              variant="outline"
                              size="sm"
                              disabled={rowBusy || disableBlocked}
                              title={
                                isSelf
                                  ? "You can't disable your own account"
                                  : lastAdmin
                                    ? "Can't disable the last remaining admin"
                                    : undefined
                              }
                              onClick={() => void disable(u.username)}
                            >
                              {rowBusy ? <Loader2 className="animate-spin" /> : <Ban />}
                              Disable
                            </Button>
                          )}
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={rowBusy}
                            title="Clear this user's 2FA"
                            onClick={() => void clear2fa(u.username)}
                          >
                            <ShieldOff />
                            Clear 2FA
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={rowBusy}
                            title="Reset this user's password"
                            onClick={() => setResetFor(u.username)}
                          >
                            <KeyRound />
                            Reset
                          </Button>
                        </div>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          </div>
        )}
        <p className="mt-2 text-[0.75rem] text-muted-foreground">
          Disabling an account revokes its live sessions and blocks login; the account, its agents and
          history are kept. Clearing 2FA lets someone locked out of their authenticator sign in with just
          their password. A password reset ends all of that user's sessions.
        </p>
      </section>

      {log.length > 0 && (
        <section>
          <Eyebrow className="mb-3">actions this session</Eyebrow>
          <div className="overflow-x-auto rounded-lg border bg-card">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>time</TableHead>
                  <TableHead>user</TableHead>
                  <TableHead>action</TableHead>
                  <TableHead>result</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {log.map((r, i) => (
                  <TableRow key={i}>
                    <TableCell className="font-mono text-xs tabular-nums text-muted-foreground">{r.at}</TableCell>
                    <TableCell className="font-mono text-sm">{r.user}</TableCell>
                    <TableCell className="text-sm">{r.action}</TableCell>
                    <TableCell>
                      <Badge
                        variant="muted"
                        className={r.ok ? "text-kind-edit" : "text-kind-warn"}
                        title={r.detail}
                      >
                        {r.ok ? "done" : "failed"}
                      </Badge>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        </section>
      )}

      <CreateUserDialog
        open={createOpen}
        onClose={() => setCreateOpen(false)}
        onDone={(user, isAdmin) => {
          record(user, "create user", true, isAdmin ? "created (admin)" : "created")
          setCreateOpen(false)
          void load()
        }}
      />

      <ResetPasswordDialog
        username={resetFor}
        onClose={() => setResetFor(null)}
        onDone={(user, revoked) => {
          record(user, "reset password", true, `revoked ${revoked} session(s)`)
          setResetFor(null)
        }}
      />
    </div>
  )
}

function CreateUserDialog({
  open,
  onClose,
  onDone,
}: {
  open: boolean
  onClose: () => void
  onDone: (user: string, isAdmin: boolean) => void
}) {
  const [username, setUsername] = useState("")
  const [pw, setPw] = useState("")
  const [isAdmin, setIsAdmin] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  const name = username.trim().toLowerCase()
  // Mirror store::MIN_PASSWORD_LEN (8) client-side — advisory; the server is the gate.
  const tooShort = pw.length > 0 && pw.length < 8
  const canSubmit = name.length >= 2 && pw.length >= 8 && !busy

  function reset() {
    setUsername("")
    setPw("")
    setIsAdmin(false)
    setError("")
  }

  async function submit() {
    if (!canSubmit) return
    setBusy(true)
    setError("")
    try {
      const r = await api.createUser(name, pw, isAdmin)
      reset()
      onDone(r.user, r.is_admin)
    } catch (err) {
      setError(err instanceof ApiError ? err.message : String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!o) {
          reset()
          onClose()
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New user</DialogTitle>
          <DialogDescription>
            Create an account. They can sign in immediately with the password you set here.
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="flex flex-col gap-3">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="new-username">username</Label>
            <Input
              id="new-username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="alice"
              autoCapitalize="none"
              autoComplete="off"
              autoFocus
            />
            <p className="text-[0.72rem] text-muted-foreground">
              2–32 lowercase letters, digits, dot, underscore or hyphen; no leading dot.
            </p>
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="new-user-pw">password</Label>
            <Input
              id="new-user-pw"
              type="password"
              value={pw}
              onChange={(e) => setPw(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && submit()}
              placeholder="at least 8 characters"
              autoComplete="new-password"
              aria-invalid={tooShort}
            />
            {tooShort && <p className="text-[0.78rem] text-destructive">At least 8 characters.</p>}
          </div>
          <label className="flex cursor-pointer items-start gap-2.5 rounded-md border px-3 py-2.5 transition-colors hover:bg-accent/40">
            <input
              type="checkbox"
              className="mt-1 accent-[var(--primary)]"
              checked={isAdmin}
              onChange={(e) => setIsAdmin(e.target.checked)}
            />
            <span>
              <span className="block text-sm font-medium">Site administrator</span>
              <span className="mt-0.5 block text-[0.78rem] text-muted-foreground">
                Grants full access to this admin panel and every account. Leave off for a normal member.
              </span>
            </span>
          </label>
          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}
        </DialogBody>
        <DialogFooter>
          <Button
            variant="ghost"
            onClick={() => {
              reset()
              onClose()
            }}
            disabled={busy}
          >
            Cancel
          </Button>
          <Button onClick={submit} disabled={!canSubmit}>
            <UserPlus />
            {busy ? "Creating…" : "Create user"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function ResetPasswordDialog({
  username,
  onClose,
  onDone,
}: {
  username: string | null
  onClose: () => void
  onDone: (user: string, revoked: number) => void
}) {
  const [pw, setPw] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  // Mirror store::MIN_PASSWORD_LEN (8) client-side — advisory; the server is the gate.
  const tooShort = pw.length > 0 && pw.length < 8

  async function submit() {
    if (!username || pw.length < 8 || busy) return
    setBusy(true)
    setError("")
    try {
      const r = await api.adminSetPassword(username, pw)
      setPw("")
      onDone(r.user, r.revoked_sessions)
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  if (!username) return null
  return (
    <Dialog open={!!username} onOpenChange={(o) => !o && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Reset password</DialogTitle>
          <DialogDescription>
            Set a new password for <span className="font-mono text-foreground/80">{username}</span>.
            This signs them out everywhere.
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="flex flex-col gap-2">
          <Label htmlFor="new-pw">new password</Label>
          <Input
            id="new-pw"
            type="password"
            value={pw}
            onChange={(e) => setPw(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
            placeholder="at least 8 characters"
            autoComplete="new-password"
            autoFocus
            aria-invalid={tooShort}
          />
          {tooShort && <p className="text-[0.78rem] text-destructive">At least 8 characters.</p>}
          {error && (
            <p role="alert" className="mt-1 text-sm text-destructive">
              {error}
            </p>
          )}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={submit} disabled={busy || pw.length < 8}>
            <KeyRound />
            {busy ? "Resetting…" : "Reset password"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
