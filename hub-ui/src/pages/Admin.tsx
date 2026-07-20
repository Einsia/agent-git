import { useState } from "react"
import { KeyRound, ShieldOff, Terminal } from "lucide-react"

import { api } from "@/lib/api"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Badge } from "@/components/ui/badge"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
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

  return <AdminPanel />
}

function AdminPanel() {
  const [username, setUsername] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")
  const [resetFor, setResetFor] = useState<string | null>(null)
  const [log, setLog] = useState<LogRow[]>([])

  const target = username.trim().toLowerCase()

  function record(user: string, action: string, ok: boolean, detail: string) {
    setLog((prev) => [{ at: new Date().toLocaleTimeString(), user, action, ok, detail }, ...prev].slice(0, 20))
  }

  async function clear2fa() {
    if (!target || busy) return
    setBusy(true)
    setError("")
    try {
      const r = await api.adminDisable2fa(target)
      record(r.user, "clear 2FA", true, "2FA cleared")
    } catch (err) {
      const msg = String((err as Error)?.message ?? err)
      setError(msg)
      record(target, "clear 2FA", false, msg)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex flex-col gap-8">
      <header>
        <span className="eyebrow">admin</span>
        <h1 className="mt-1 text-2xl font-bold tracking-tight">Admin</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Account recovery tools for site administrators.
        </p>
      </header>

      <Alert>
        <Terminal />
        <AlertTitle>The full user roster lives in the CLI</AlertTitle>
        <AlertDescription>
          The hub exposes no HTTP endpoint to list, create, or enable/disable accounts — those run on
          the host with{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">agit-hub user …</code>. What
          you can do here is recover a specific user who's locked out.
        </AlertDescription>
      </Alert>

      <section>
        <Eyebrow className="mb-3">user recovery</Eyebrow>
        <div className="flex flex-col gap-3 rounded-lg border bg-card p-4">
          <div className="flex flex-wrap items-end gap-3">
            <label className="flex flex-col gap-1.5">
              <span className="eyebrow">username</span>
              <Input
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                placeholder="alice"
                className="w-[220px]"
                autoCapitalize="none"
                autoComplete="off"
              />
            </label>
            <Button variant="outline" disabled={busy || !target} onClick={clear2fa}>
              <ShieldOff />
              Clear 2FA
            </Button>
            <Button variant="outline" disabled={!target} onClick={() => setResetFor(target)}>
              <KeyRound />
              Reset password
            </Button>
          </div>
          <p className="text-[0.75rem] text-muted-foreground">
            Clearing 2FA lets a user locked out of their authenticator sign in with just their
            password. A password reset ends all of that user's sessions.
          </p>
          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}
        </div>
      </section>

      {log.length > 0 && (
        <section>
          <Eyebrow className="mb-3">actions this session</Eyebrow>
          <div className="overflow-hidden rounded-lg border bg-card">
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
