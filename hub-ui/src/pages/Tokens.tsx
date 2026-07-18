import { useState, type FormEvent } from "react"
import { KeyRound, Trash2 } from "lucide-react"

import { api, type Scope } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useGuarded } from "@/lib/useGuarded"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select } from "@/components/ui/select"
import { Badge } from "@/components/ui/badge"
import { CopyButton } from "@/components/CopyButton"
import { Eyebrow, LoadError } from "@/components/States"

export function Tokens() {
  const { data, loading, error, status, reload } = useGuarded(() => api.tokens(), [])
  // Plain useAsync: /api/agents never rejects a caller, it just answers with what they may
  // see. Guarding it would let this dropdown drive a redirect it can't have a reason for.
  const agents = useAsync(() => api.agents(), [])

  const [name, setName] = useState("")
  const [agent, setAgent] = useState("")
  const [scope, setScope] = useState<Scope>("read")
  const [ttl, setTtl] = useState("30")
  const [busy, setBusy] = useState(false)
  const [formError, setFormError] = useState("")
  // The plaintext appears in the create response and nowhere else — the server keeps only a
  // sha256. It's gone on refresh, so say so.
  const [fresh, setFresh] = useState<{ token: string; name: string } | null>(null)

  async function create(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setFormError("")
    try {
      const days = Number(ttl)
      const res = await api.createToken({
        name: name.trim(),
        // No agent picked = omit the field: the token reaches everything its owner reaches.
        // Picking one binds the token to that agent alone.
        ...(agent ? { agent } : {}),
        scope,
        ...(Number.isFinite(days) && days > 0 ? { ttl_days: days } : {}),
      })
      setFresh({ token: res.token, name: name.trim() })
      setName("")
      setAgent("")
      setScope("read")
      reload()
    } catch (err) {
      setFormError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  async function revoke(id: string) {
    try {
      await api.revokeToken(id)
      reload()
    } catch (err) {
      setFormError(String((err as Error)?.message ?? err))
    }
  }

  return (
    <div className="flex flex-col gap-8">
      <header>
        <span className="eyebrow">tokens</span>
        <h1 className="mt-1 text-2xl font-bold tracking-tight">Tokens</h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Credentials for git and scripts. Scope one to a single agent, to reads only, or give
          it an expiry.
        </p>
      </header>

      {fresh && (
        <section className="readout rounded-lg border border-primary/40 p-4">
          <Eyebrow className="text-primary">shown once</Eyebrow>
          <p className="mb-3 mt-1.5 text-sm text-muted-foreground">
            The plaintext for <span className="font-mono text-foreground/80">{fresh.name}</span>. Store it
            now — the server keeps only a hash, so closing this is final.
          </p>
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 truncate rounded-md bg-muted px-3 py-2 font-mono text-sm">
              {fresh.token}
            </code>
            <CopyButton value={fresh.token} />
            <Button variant="ghost" size="sm" onClick={() => setFresh(null)}>
              Stored it
            </Button>
          </div>
        </section>
      )}

      <section>
        <Eyebrow className="mb-3">new token</Eyebrow>
        <form onSubmit={create} className="flex flex-col gap-3 rounded-lg border bg-card p-4">
          <div className="flex flex-wrap items-end gap-3">
            <label className="flex flex-col gap-1.5">
              <span className="eyebrow">name</span>
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="ci-deploy"
                className="w-[200px]"
                required
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="eyebrow">scope</span>
              <Select value={agent} onChange={(e) => setAgent(e.target.value)} className="w-[180px]">
                <option value="">all agents</option>
                {/* A token binds to the scoped id "<owner_ns>/<name>", so that's the option value. */}
                {agents.data?.agents.map((a) => (
                  <option key={a.full_name} value={a.full_name}>
                    {a.full_name}
                  </option>
                ))}
              </Select>
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="eyebrow">access</span>
              <Select value={scope} onChange={(e) => setScope(e.target.value as Scope)} className="w-[110px]">
                <option value="read">read</option>
                <option value="write">write</option>
              </Select>
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="eyebrow">expires (days)</span>
              <Input
                type="number"
                min={1}
                value={ttl}
                onChange={(e) => setTtl(e.target.value)}
                placeholder="30"
                className="w-[130px]"
              />
            </label>
            <Button type="submit" disabled={busy || !name.trim()}>
              <KeyRound />
              {busy ? "Creating…" : "Create"}
            </Button>
          </div>
          <p className="text-[0.75rem] text-muted-foreground">
            Blank expiry = never expires. Bound to one agent, a token can't touch any other.
          </p>
          {formError && (
            <p role="alert" className="text-sm text-destructive">
              {formError}
            </p>
          )}
        </form>
      </section>

      <section>
        <Eyebrow className="mb-3">existing</Eyebrow>
        {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
        {error && status !== 401 && <LoadError message={error} />}
        {data && (
          <div className="overflow-hidden rounded-lg border bg-card">
            {data.length === 0 && <p className="px-4 py-8 text-muted-foreground">No tokens yet.</p>}
            {data.map((t) => (
              <div
                key={t.id}
                className="flex flex-wrap items-center justify-between gap-3 border-t px-4 py-3 first:border-t-0"
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="truncate font-mono text-sm font-semibold">{t.name}</span>
                    <Badge variant="muted" className="font-mono text-[0.6rem]">
                      {t.scope}
                    </Badge>
                    <Badge variant="muted" className="font-mono text-[0.6rem]">
                      {t.agent ?? "all agents"}
                    </Badge>
                    {/* The server reports whether a token still authenticates. Old ownerless
                        ones list as what they are: dead. */}
                    {!t.usable && (
                      <Badge variant="muted" className="font-mono text-[0.6rem] text-kind-warn">
                        unusable
                      </Badge>
                    )}
                  </div>
                  <div className="mt-0.5 flex flex-wrap gap-3 font-mono text-[0.68rem] tabular-nums text-muted-foreground">
                    <span>created {t.created || "—"}</span>
                    <span>expires {t.expires || "never"}</span>
                    <span>last used {t.last_used || "never"}</span>
                  </div>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  className="text-muted-foreground hover:text-destructive"
                  onClick={() => revoke(t.id)}
                >
                  <Trash2 />
                  Revoke
                </Button>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  )
}
