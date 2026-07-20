import { useState, type FormEvent } from "react"
import { Link, useNavigate, useSearchParams } from "react-router-dom"
import { ShieldCheck } from "lucide-react"

import { ApiError, api } from "@/lib/api"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { InputOTP } from "@/components/ui/input-otp"

export function Login() {
  const [params] = useSearchParams()
  const nav = useNavigate()
  const { refresh } = useSession()

  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  // Second-factor step: the server answered 401 {"error":"2fa_required"} to the password, so we hold
  // the credentials and ask for a code rather than treating it as a normal wrong-password failure.
  const [needCode, setNeedCode] = useState(false)
  const [code, setCode] = useState("")
  const [useBackup, setUseBackup] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  // `next` may only be a site-relative path: the browser reads `//evil.com` and `http://…` as
  // off-site, and handing either to navigate is an open redirect.
  const raw = params.get("next") ?? "/"
  const next = raw.startsWith("/") && !raw.startsWith("//") ? raw : "/"

  // The server returns this exact string (401) when a correct password needs a second factor. It is
  // NOT a normal auth failure — advance to the code step instead of showing "wrong password".
  const isTwoFactor = (err: unknown) =>
    err instanceof ApiError && err.status === 401 && err.message === "2fa_required"

  async function attempt(withCode?: string) {
    setBusy(true)
    setError("")
    try {
      await api.login(username, password, withCode)
      await refresh()
      nav(next, { replace: true })
    } catch (err) {
      if (isTwoFactor(err)) {
        // First time here: advance to the code step. Already there: the code was wrong.
        if (needCode) {
          setError("That code isn't valid. Try again, or use a backup code.")
          setCode("")
        } else {
          setNeedCode(true)
        }
      } else if (err instanceof ApiError && err.status === 401) {
        // 401 says only "wrong username or password" — never whether the account exists.
        setError("Wrong username or password.")
      } else {
        setError(String((err as Error)?.message ?? err))
      }
      setBusy(false)
    }
  }

  function submitPassword(e: FormEvent) {
    e.preventDefault()
    void attempt()
  }

  function submitCode(e: FormEvent) {
    e.preventDefault()
    if (!code.trim()) return
    void attempt(code.trim())
  }

  return (
    <div className="mx-auto max-w-[380px] pt-10">
      <span className="eyebrow">sign in</span>
      <h1 className="mb-1 mt-1 text-2xl font-bold tracking-tight">
        <span className="font-mono">agit</span>
        <span className="font-mono text-muted-foreground">·hub</span>
      </h1>
      <p className="mb-6 text-sm text-muted-foreground">
        Sessions are private. You'll see the agents you have access to.
      </p>

      {!needCode ? (
        <form onSubmit={submitPassword} className="flex flex-col gap-3 rounded-lg border bg-card p-5">
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
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">password</span>
            <Input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
              required
            />
          </label>

          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}

          <Button type="submit" disabled={busy || !username || !password} className="mt-1">
            {busy ? "Signing in…" : "Sign in"}
          </Button>
        </form>
      ) : (
        <form onSubmit={submitCode} className="flex flex-col gap-4 rounded-lg border bg-card p-5">
          <div className="flex items-start gap-2.5">
            <ShieldCheck className="mt-0.5 size-5 shrink-0 text-primary" />
            <div>
              <p className="text-sm font-medium">Two-factor authentication</p>
              <p className="mt-0.5 text-sm text-muted-foreground">
                Signed in as <span className="font-mono text-foreground/80">{username}</span>. Enter the
                code from your authenticator app.
              </p>
            </div>
          </div>

          {useBackup ? (
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="backup-code">backup code</Label>
              <Input
                id="backup-code"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                placeholder="abcd-ef01-2345-6789"
                autoComplete="one-time-code"
                autoFocus
                required
              />
            </div>
          ) : (
            <div className="flex flex-col gap-1.5">
              <Label>authenticator code</Label>
              <InputOTP
                value={code}
                onChange={setCode}
                onComplete={(c) => void attempt(c)}
                disabled={busy}
                autoFocus
                aria-label="6-digit authenticator code"
              />
            </div>
          )}

          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}

          <Button
            type="submit"
            disabled={busy || (useBackup ? !code.trim() : code.length !== 6)}
            className="mt-1"
          >
            {busy ? "Verifying…" : "Verify"}
          </Button>

          <div className="flex items-center justify-between text-[0.8rem]">
            <button
              type="button"
              className="text-primary hover:underline"
              onClick={() => {
                setUseBackup((b) => !b)
                setCode("")
                setError("")
              }}
            >
              {useBackup ? "Use an authenticator code" : "Use a backup code"}
            </button>
            <button
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={() => {
                setNeedCode(false)
                setCode("")
                setPassword("")
                setError("")
              }}
            >
              Start over
            </button>
          </div>
        </form>
      )}

      <p className="mt-4 text-[0.8rem] text-muted-foreground">
        New here?{" "}
        <Link to="/register" className="text-primary hover:underline">
          Create an account
        </Link>
        .
      </p>

      <p className="mt-2 text-[0.8rem] text-muted-foreground">
        Scripts and git use a token, not a password: sign in and make one at{" "}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">/tokens</code>.
      </p>
    </div>
  )
}
