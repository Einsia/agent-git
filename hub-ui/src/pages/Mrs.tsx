import { useEffect, useState, type FormEvent } from "react"
import { Link, useNavigate, useParams } from "react-router-dom"
import { GitPullRequest, MessageSquare } from "lucide-react"

import { api, type MrEndpoint, type MrSummary } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select } from "@/components/ui/select"
import { Badge } from "@/components/ui/badge"
import { Crumb } from "@/components/Crumb"
import { Eyebrow, Forbidden, LoadError } from "@/components/States"

// The stored state string maps to a colour. An unknown value (a hand-edited mrs.json) reads as
// "closed" here — the server treats anything that isn't "open" as not open.
export function MrStateBadge({ state }: { state: string }) {
  const tone =
    state === "open"
      ? "text-kind-edit"
      : state === "merged"
        ? "text-primary"
        : "text-muted-foreground"
  return (
    <Badge variant="muted" className={`font-mono text-[0.6rem] ${tone}`}>
      {state}
    </Badge>
  )
}

/// One endpoint, as a link when the caller may read it and a muted "private" when the server redacted
/// it. The server nulls every field on redaction, so `full_name` being present is the signal.
export function EndpointRef({ e }: { e: MrEndpoint }) {
  if (e.redacted || !e.full_name || !e.owner || !e.agent) {
    return <span className="font-mono text-muted-foreground">private</span>
  }
  return (
    <Link to={`/agent/${e.owner}/${e.agent}`} className="font-mono text-primary hover:underline">
      {e.full_name}
      {e.ref && e.ref !== "main" && <span className="text-muted-foreground">:{e.ref}</span>}
    </Link>
  )
}

export function Mrs() {
  const { owner = "", name = "" } = useParams()
  const { me } = useSession()
  const { data, loading, error, status, forbidden, reload } = useGuarded(() => api.mrs(owner, name), [owner, name])

  // Cursor pagination, accumulated: the first page comes from useGuarded (which owns the permission
  // routing); "Load more" appends the next window and advances the cursor.
  const [extra, setExtra] = useState<MrSummary[]>([])
  const [cursor, setCursor] = useState<string | null>(null)
  const [moreBusy, setMoreBusy] = useState(false)
  const [moreError, setMoreError] = useState("")
  useEffect(() => {
    setExtra([])
    setCursor(data?.next_cursor ?? null)
    setMoreError("")
  }, [data])

  async function loadMore() {
    if (!cursor) return
    setMoreBusy(true)
    setMoreError("")
    try {
      const next = await api.mrs(owner, name, cursor)
      setExtra((x) => [...x, ...next.mrs])
      setCursor(next.next_cursor)
    } catch (err) {
      setMoreError(String((err as Error)?.message ?? err))
    } finally {
      setMoreBusy(false)
    }
  }

  const rows = data ? [...data.mrs, ...extra] : []

  if (forbidden) return <Forbidden what={`${owner}/${name}`} />

  return (
    <div>
      <Crumb owner={owner} name={name} />
      <div className="mb-6 flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2.5">
          <GitPullRequest className="size-5 text-muted-foreground" />
          <h1 className="font-mono text-2xl font-bold tracking-tight">merge requests</h1>
        </div>
        <Link to={`/agent/${owner}/${name}`} className="eyebrow hover:text-foreground">
          ← {owner}/{name}
        </Link>
      </div>

      {me && <OpenForm owner={owner} name={name} onOpened={reload} />}

      {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
      {error && status !== 401 && <LoadError message={error} />}

      {data && rows.length === 0 && (
        <p className="rounded-lg border bg-card px-4 py-8 text-muted-foreground">
          No merge requests yet. An MR parks the transcript of a local <code className="font-mono">agit -a merge</code> here
          for review — the Hub itself merges nothing.
        </p>
      )}

      <div className="flex flex-col gap-2">
        {rows.map((m) => (
          <MrRow key={m.id} owner={owner} name={name} m={m} />
        ))}
      </div>

      {cursor && (
        <div className="mt-6 flex flex-col items-center gap-2">
          <Button variant="outline" size="sm" disabled={moreBusy} onClick={loadMore}>
            {moreBusy ? "Loading…" : "Load more"}
          </Button>
          {moreError && (
            <p role="alert" className="text-sm text-destructive">
              {moreError}
            </p>
          )}
        </div>
      )}
    </div>
  )
}

function MrRow({ owner, name, m }: { owner: string; name: string; m: MrSummary }) {
  return (
    <Link
      to={`/agent/${owner}/${name}/mrs/${m.id}`}
      className="flex flex-wrap items-center gap-3 rounded-lg border bg-card px-4 py-3 transition-colors hover:border-primary/50"
    >
      <span className="font-mono text-sm text-muted-foreground tabular-nums">#{m.id}</span>
      <span className="min-w-0 flex-1 truncate font-semibold">{m.title}</span>
      <MrStateBadge state={m.state} />
      {m.comments > 0 && (
        <span className="flex items-center gap-1 font-mono text-[0.72rem] text-muted-foreground">
          <MessageSquare className="size-3.5" />
          {m.comments}
        </span>
      )}
      <span className="w-full font-mono text-[0.72rem] text-muted-foreground">
        <EndpointRef e={m.source} /> → <EndpointRef e={m.target} /> · by {m.author} · {m.created}
      </span>
    </Link>
  )
}

// Opening an MR needs Write on the target; the server enforces it (403 lands in the form's error).
// The source is another agent addressed as owner/name — the same "<owner_ns>/<name>" that
// /api/agents already returns as `full_name`, so offer those and drop the target itself.
function OpenForm({ owner, name, onOpened }: { owner: string; name: string; onOpened: () => void }) {
  const nav = useNavigate()
  const agents = useAsync(() => api.agents(), [])
  const [open, setOpen] = useState(false)
  const [title, setTitle] = useState("")
  const [source, setSource] = useState("")
  const [sourceRef, setSourceRef] = useState("")
  const [targetRef, setTargetRef] = useState("")
  const [transcript, setTranscript] = useState("")
  const [busy, setBusy] = useState(false)
  const [formError, setFormError] = useState("")

  const target = `${owner}/${name}`

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setFormError("")
    try {
      const rec = await api.openMr(owner, name, {
        title: title.trim(),
        source,
        ...(sourceRef.trim() ? { source_ref: sourceRef.trim() } : {}),
        ...(targetRef.trim() ? { target_ref: targetRef.trim() } : {}),
        ...(transcript.trim() ? { dialogue_transcript: transcript } : {}),
      })
      onOpened()
      nav(`/agent/${owner}/${name}/mrs/${rec.id}`)
    } catch (err) {
      setFormError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  if (!open) {
    return (
      <div className="mb-6">
        <Button variant="outline" size="sm" onClick={() => setOpen(true)}>
          <GitPullRequest />
          Open a merge request
        </Button>
      </div>
    )
  }

  return (
    <section className="mb-6">
      <Eyebrow className="mb-3">new merge request</Eyebrow>
      <form onSubmit={submit} className="flex flex-col gap-3 rounded-lg border bg-card p-4">
        <label className="flex flex-col gap-1.5">
          <span className="eyebrow">title</span>
          <Input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Reconcile the payments memory" required />
        </label>
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">source (owner/name)</span>
            <Select value={source} onChange={(e) => setSource(e.target.value)} className="w-[220px]" required>
              <option value="">choose an agent…</option>
              {agents.data?.agents
                .filter((a) => a.full_name !== target)
                .map((a) => (
                  <option key={a.full_name} value={a.full_name}>
                    {a.full_name}
                  </option>
                ))}
            </Select>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">source ref</span>
            <Input value={sourceRef} onChange={(e) => setSourceRef(e.target.value)} placeholder="main" className="w-[130px]" />
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="eyebrow">target ref</span>
            <Input value={targetRef} onChange={(e) => setTargetRef(e.target.value)} placeholder="main" className="w-[130px]" />
          </label>
        </div>
        <label className="flex flex-col gap-1.5">
          <span className="eyebrow">dialogue transcript (optional)</span>
          <textarea
            value={transcript}
            onChange={(e) => setTranscript(e.target.value)}
            placeholder="What `agit -a merge` produced locally. Can be filled in later by comment."
            rows={5}
            className="flex w-full rounded-md border bg-transparent px-3 py-2 font-mono text-[0.78rem] shadow-sm transition-colors placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          />
        </label>
        <div className="flex items-center gap-2">
          <Button type="submit" disabled={busy || !title.trim() || !source}>
            <GitPullRequest />
            {busy ? "Opening…" : "Open"}
          </Button>
          <Button type="button" variant="ghost" size="sm" onClick={() => setOpen(false)}>
            Cancel
          </Button>
        </div>
        {formError && (
          <p role="alert" className="text-sm text-destructive">
            {formError}
          </p>
        )}
      </form>
    </section>
  )
}
