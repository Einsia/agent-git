import { useState, type ReactNode, type FormEvent } from "react"
import { Link, useNavigate } from "react-router-dom"
import { Globe, Lock } from "lucide-react"

import { api, type Visibility } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select } from "@/components/ui/select"
import { cn } from "@/lib/utils"

// Mirrors valid_agent_name() in src/hub/net.rs: [A-Za-z0-9._-] only, max 64, no leading dot,
// no "..". This is here only to say what's wrong while you type — the server is the gate.
export function validateName(name: string): string | null {
  if (!name) return "A name is required."
  if (name.length > 64) return "64 characters max."
  if (name.startsWith(".")) return "Can't start with a dot."
  if (name.includes("..")) return "Can't contain “..”."
  if (!/^[A-Za-z0-9._-]+$/.test(name)) return "Letters, digits, dot, underscore and dash only."
  return null
}

export function NewAgent() {
  const nav = useNavigate()
  const { me } = useSession()
  const [name, setName] = useState("")
  const [visibility, setVisibility] = useState<Visibility>("private")
  // "" = the caller's own account; otherwise an org name they belong to.
  const [owner, setOwner] = useState("")
  // Default-checked: a web-created store should be immediately cloneable. Unchecking makes a bare name
  // reservation for pushing an EXISTING agent instead.
  const [initialize, setInitialize] = useState(true)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  // The orgs the caller belongs to (a site admin sees all) — the owner choices beyond "your account".
  // A stray 401/403 here shouldn't block a personal create, so useAsync (not useGuarded).
  const { data: orgs } = useAsync(() => api.orgs(), [])

  const nameError = name ? validateName(name) : null

  async function submit(e: FormEvent) {
    e.preventDefault()
    const bad = validateName(name)
    if (bad) return setError(bad)
    setBusy(true)
    setError("")
    try {
      const created = await api.createAgent(name, visibility, { org: owner || undefined, initialize })
      // Route off the server's `full_name` (owner_ns/name), which already carries the owner it
      // assigned (the caller, or the org's namespace segment).
      nav(`/agent/${created.full_name}/settings`)
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
      setBusy(false)
    }
  }

  return (
    <div className="mx-auto max-w-[520px]">
      <span className="eyebrow">new agent</span>
      <h1 className="mb-1 mt-1 text-2xl font-bold tracking-tight">New agent</h1>
      <p className="mb-6 text-sm text-muted-foreground">
        Creates a store you can clone right away. Bind it to a code repo and its sessions ride along
        on push.
      </p>

      <form onSubmit={submit} className="flex flex-col gap-5 rounded-lg border bg-card p-5">
        <label className="flex flex-col gap-1.5">
          <span className="eyebrow">name</span>
          <Input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="billing-agent"
            autoFocus
            required
            aria-invalid={!!nameError}
          />
          <span className="text-[0.75rem] text-muted-foreground">
            The name is a mutable label. Renaming leaves identity alone: an agent is its aid,
            written into agent.toml when the store is created.
          </span>
          {nameError && <span className="text-[0.78rem] text-destructive">{nameError}</span>}
        </label>

        {orgs && orgs.length > 0 && (
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">owner</span>
            <Select value={owner} onChange={(e) => setOwner(e.target.value)} aria-label="Owner">
              <option value="">{me?.username ? `${me.username} (your account)` : "your account"}</option>
              {orgs.map((o) => (
                <option key={o.name} value={o.name}>
                  {o.name} (org)
                </option>
              ))}
            </Select>
            <span className="text-[0.75rem] text-muted-foreground">
              An org-owned agent is writable by every member and managed by its admins.
            </span>
          </label>
        )}

        <fieldset className="flex flex-col gap-1.5">
          <legend className="eyebrow mb-1.5">visibility</legend>
          <VisibilityOption
            checked={visibility === "private"}
            onSelect={() => setVisibility("private")}
            icon={<Lock className="size-3.5" />}
            title="private"
            desc="Owner and members only. The default."
          />
          <VisibilityOption
            checked={visibility === "public"}
            onSelect={() => setVisibility("public")}
            icon={<Globe className="size-3.5" />}
            title="public"
            desc="Anyone who can reach this hub can read every session."
            loud
          />
        </fieldset>

        {visibility === "public" && (
          <p className="rounded-md border border-kind-warn/40 bg-kind-warn/5 px-3 py-2 text-[0.78rem] text-foreground">
            Sessions usually carry source, paths and the prompts verbatim. Public means all of
            that is readable by anyone who can reach this hub.
          </p>
        )}

        <label className="flex cursor-pointer items-start gap-2.5 rounded-md border px-3 py-2.5 transition-colors hover:bg-accent/40">
          <input
            type="checkbox"
            className="mt-1 accent-[var(--primary)]"
            checked={initialize}
            onChange={(e) => setInitialize(e.target.checked)}
          />
          <span>
            <span className="block text-sm font-medium">Initialize with an empty agent</span>
            <span className="mt-0.5 block text-[0.78rem] text-muted-foreground">
              Commits an <span className="font-mono">agent.toml</span> so the store is cloneable right
              away. Uncheck to make a bare name reservation for pushing an existing agent.
            </span>
          </span>
        </label>

        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}

        <div className="flex items-center gap-2">
          <Button type="submit" disabled={busy || !name || !!nameError}>
            {busy ? "Creating…" : "Create"}
          </Button>
          <Link to="/" className="text-sm text-muted-foreground hover:text-foreground">
            Cancel
          </Link>
        </div>
      </form>
    </div>
  )
}

function VisibilityOption({
  checked,
  onSelect,
  icon,
  title,
  desc,
  loud = false,
}: {
  checked: boolean
  onSelect: () => void
  icon: ReactNode
  title: string
  desc: string
  loud?: boolean
}) {
  return (
    <label
      className={cn(
        "flex cursor-pointer items-start gap-2.5 rounded-md border px-3 py-2.5 transition-colors",
        checked
          ? loud
            ? "border-kind-warn/50 bg-kind-warn/5"
            : "border-primary/50 bg-primary/5"
          : "hover:bg-accent/40"
      )}
    >
      <input
        type="radio"
        name="visibility"
        className="mt-1 accent-[var(--primary)]"
        checked={checked}
        onChange={onSelect}
      />
      <span>
        <span className="flex items-center gap-1.5 font-mono text-sm font-semibold">
          {icon}
          {title}
        </span>
        <span className="mt-0.5 block text-[0.78rem] text-muted-foreground">{desc}</span>
      </span>
    </label>
  )
}
