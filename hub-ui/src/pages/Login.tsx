import { useState, type FormEvent } from "react"
import { useNavigate, useSearchParams } from "react-router-dom"

import { ApiError, api } from "@/lib/api"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

export function Login() {
  const [params] = useSearchParams()
  const nav = useNavigate()
  const { refresh } = useSession()

  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  // `next` may only be a site-relative path: the browser reads `//evil.com` and `http://…` as
  // off-site, and handing either to navigate is an open redirect.
  const raw = params.get("next") ?? "/"
  const next = raw.startsWith("/") && !raw.startsWith("//") ? raw : "/"

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api.login(username, password)
      await refresh()
      nav(next, { replace: true })
    } catch (err) {
      // 401 says only "wrong username or password" — never whether the account exists. The
      // difference is a username oracle.
      if (err instanceof ApiError && err.status === 401) setError("Wrong username or password.")
      else setError(String((err as Error)?.message ?? err))
      setBusy(false)
    }
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

      <p className="mt-4 text-[0.8rem] text-muted-foreground">
        Scripts and git use a token, not a password: sign in and make one at{" "}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">/tokens</code>.
      </p>
    </div>
  )
}
