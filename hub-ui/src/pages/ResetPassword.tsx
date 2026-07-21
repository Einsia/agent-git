import { useState, type FormEvent } from "react"
import { Link, useNavigate, useSearchParams } from "react-router-dom"
import { KeyRound, MailCheck } from "lucide-react"

import { ApiError, api } from "@/lib/api"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

// One page, two modes. Without a `?token=` it is the REQUEST form (username → operator-forwarded link);
// with one it is the CONSUME form (the token from the forwarded link → a new password). The forwarded
// link is exactly `<base>/reset-password?token=...`, so it lands here in consume mode.
export function ResetPassword() {
  const [params] = useSearchParams()
  const token = params.get("token")?.trim() ?? ""
  return token ? <ConsumeForm token={token} /> : <RequestForm />
}

// Mode 1 — request a reset link. Anti-enumeration: the server ALWAYS answers a generic 200, so we show
// the same "if that account exists…" message on success regardless of whether the username is real.
function RequestForm() {
  const [username, setUsername] = useState("")
  const [busy, setBusy] = useState(false)
  const [sent, setSent] = useState(false)
  const [error, setError] = useState("")

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api.requestPasswordReset(username)
      // The response never confirms existence — neither does this screen.
      setSent(true)
    } catch (err) {
      if (err instanceof ApiError && err.status === 429) {
        setError("Too many requests from your address. Wait a minute and try again.")
      } else {
        setError(String((err as Error)?.message ?? err))
      }
    }
    setBusy(false)
  }

  return (
    <div className="mx-auto max-w-[380px] pt-10">
      <span className="eyebrow">reset password</span>
      <h1 className="mb-1 mt-1 text-2xl font-bold tracking-tight">
        <span className="font-mono">agit</span>
        <span className="font-mono text-muted-foreground">·hub</span>
      </h1>
      <p className="mb-6 text-sm text-muted-foreground">
        Locked out? Enter your username and we'll generate a reset link for an operator to forward to you.
      </p>

      {sent ? (
        <div className="flex flex-col gap-3 rounded-lg border bg-card p-5">
          <div className="flex items-start gap-2.5">
            <MailCheck className="mt-0.5 size-5 shrink-0 text-primary" />
            <p className="text-sm text-muted-foreground">
              If that account exists, a password reset link was generated. Check with your hub operator to
              receive it — the link takes you to a page to set a new password.
            </p>
          </div>
          <Link to="/login" className="text-[0.8rem] text-primary hover:underline">
            Back to sign in
          </Link>
        </div>
      ) : (
        <form onSubmit={submit} className="flex flex-col gap-3 rounded-lg border bg-card p-5">
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">username</span>
            <Input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoComplete="username"
              autoFocus
              required
            />
          </label>

          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}

          <Button type="submit" disabled={busy || !username} className="mt-1">
            {busy ? "Requesting…" : "Request reset link"}
          </Button>
        </form>
      )}

      {!sent && (
        <p className="mt-4 text-[0.8rem] text-muted-foreground">
          Remembered it?{" "}
          <Link to="/login" className="text-primary hover:underline">
            Sign in
          </Link>
          .
        </p>
      )}
    </div>
  )
}

// Mode 2 — consume a reset token. No old password: the token IS the authority. On success the server
// revokes every session, so we send the user to /login to sign in with the new password.
function ConsumeForm({ token }: { token: string }) {
  const nav = useNavigate()
  const [password, setPassword] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api.consumePasswordReset(token, password)
      // Every session was revoked; go sign in fresh with the new password.
      nav("/login", { replace: true })
    } catch (err) {
      if (err instanceof ApiError && err.status === 400) {
        // The 400 body names the reason (bad/expired/spent token, or too-short password); show it verbatim.
        setError(err.message)
      } else {
        setError(String((err as Error)?.message ?? err))
      }
      setBusy(false)
    }
  }

  return (
    <div className="mx-auto max-w-[380px] pt-10">
      <span className="eyebrow">set a new password</span>
      <h1 className="mb-1 mt-1 text-2xl font-bold tracking-tight">
        <span className="font-mono">agit</span>
        <span className="font-mono text-muted-foreground">·hub</span>
      </h1>
      <p className="mb-6 text-sm text-muted-foreground">
        Choose a new password for your account. This link is single-use and expires shortly.
      </p>

      <form onSubmit={submit} className="flex flex-col gap-3 rounded-lg border bg-card p-5">
        <div className="flex items-start gap-2.5">
          <KeyRound className="mt-0.5 size-5 shrink-0 text-primary" />
          <p className="text-sm text-muted-foreground">
            Setting a new password signs out every other session on this account.
          </p>
        </div>
        <label className="flex flex-col gap-1.5">
          <span className="eyebrow">new password</span>
          <Input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
            autoFocus
            required
          />
          <span className="text-[0.75rem] text-muted-foreground">At least 8 characters.</span>
        </label>

        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}

        <Button type="submit" disabled={busy || !password} className="mt-1">
          {busy ? "Saving…" : "Set new password"}
        </Button>
      </form>

      <p className="mt-4 text-[0.8rem] text-muted-foreground">
        Link expired or already used?{" "}
        <Link to="/reset-password" className="text-primary hover:underline">
          Request a new one
        </Link>
        .
      </p>
    </div>
  )
}
