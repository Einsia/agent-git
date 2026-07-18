import { useState, type FormEvent } from "react"
import { Link, useNavigate } from "react-router-dom"

import { ApiError, api } from "@/lib/api"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

export function Register() {
  const nav = useNavigate()
  const { refresh } = useSession()

  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")
  // A 403 means the operator turned signup off for this whole hub — a standing condition, not a
  // per-attempt error. Disable the form and say so, rather than invite a retry into the same wall.
  // This is the graceful-degrade path when registration is off: there's no GET to probe it, the
  // POST's 403 is the only signal, so we surface it plainly instead of crashing.
  const [disabled, setDisabled] = useState(false)

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      // On success the server sets the session cookie; refresh() then picks up the new Me so the
      // nav and layout reflect the signed-in account, exactly like Login does.
      await api.register(username, password)
      await refresh()
      nav("/", { replace: true })
    } catch (err) {
      if (err instanceof ApiError && err.status === 403) {
        setDisabled(true)
        setError("Self-service registration is disabled on this hub. Ask an admin to create your account.")
      } else if (err instanceof ApiError && err.status === 409) {
        setError("That username is taken. Pick another.")
      } else if (err instanceof ApiError && err.status === 400) {
        // The 400 body names the rule that failed (username shape or password length); it's
        // written for a person, so show it verbatim.
        setError(err.message)
      } else {
        setError(String((err as Error)?.message ?? err))
      }
      setBusy(false)
    }
  }

  return (
    <div className="mx-auto max-w-[380px] pt-10">
      <span className="eyebrow">create an account</span>
      <h1 className="mb-1 mt-1 text-2xl font-bold tracking-tight">
        <span className="font-mono">agit</span>
        <span className="font-mono text-muted-foreground">·hub</span>
      </h1>
      <p className="mb-6 text-sm text-muted-foreground">
        Sign up for an account on this hub. You'll see the agents you're given access to.
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
            disabled={disabled}
          />
        </label>
        <label className="flex flex-col gap-1.5">
          <span className="eyebrow">password</span>
          <Input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
            required
            disabled={disabled}
          />
          <span className="text-[0.75rem] text-muted-foreground">
            Lowercase username, 2-32 characters. Password at least 8 characters.
          </span>
        </label>

        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}

        <Button type="submit" disabled={busy || disabled || !username || !password} className="mt-1">
          {busy ? "Creating…" : "Create account"}
        </Button>
      </form>

      <p className="mt-4 text-[0.8rem] text-muted-foreground">
        Already have an account?{" "}
        <Link to="/login" className="text-primary hover:underline">
          Sign in
        </Link>
        .
      </p>
    </div>
  )
}
