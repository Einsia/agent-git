import { useState, type FormEvent } from "react"
import { Link, useParams } from "react-router-dom"
import { ArrowRight, MessageSquare } from "lucide-react"

import { api, type MrComment, type MrDetail as MrDetailData } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { useGuarded } from "@/lib/useGuarded"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Select } from "@/components/ui/select"
import { Crumb } from "@/components/Crumb"
import { Eyebrow, Forbidden, LoadError } from "@/components/States"
import { EndpointRef, MrStateBadge } from "@/pages/Mrs"

export function MrDetail() {
  const { owner = "", name = "", id = "" } = useParams()
  const mrId = Number(id)
  const { me } = useSession()
  const { data, loading, error, status, forbidden, reload } = useGuarded(
    () => api.mr(owner, name, mrId),
    [owner, name, mrId]
  )
  // The MR views don't carry the caller's role, so read it off the agent — the same source Settings
  // trusts. Plain useAsync: anyone who can read the MR can read the target (same ACL), so it won't
  // 403 here, and guarding it would let it drive a redirect this page shouldn't own.
  const agent = useAsync(() => api.agent(owner, name), [owner, name])
  const canWrite = agent.data?.role === "owner" || agent.data?.role === "admin" || agent.data?.role === "write"

  if (forbidden) return <Forbidden what={`${owner}/${name}`} />

  return (
    <div>
      <Crumb owner={owner} name={name} />
      <div className="mb-6">
        <Link to={`/agent/${owner}/${name}/mrs`} className="eyebrow hover:text-foreground">
          ← merge requests
        </Link>
      </div>

      {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
      {error && status !== 401 && <LoadError message={error} />}

      {data && (
        <div className="flex flex-col gap-6">
          <header>
            <div className="flex flex-wrap items-center gap-2.5">
              <span className="font-mono text-lg text-muted-foreground tabular-nums">#{data.id}</span>
              <h1 className="text-2xl font-bold tracking-tight">{data.title}</h1>
              <MrStateBadge state={data.state} />
            </div>
            <p className="mt-2 flex flex-wrap items-center gap-2 font-mono text-[0.8rem] text-muted-foreground">
              <EndpointRef e={data.source} />
              <ArrowRight className="size-3.5" />
              <EndpointRef e={data.target} />
            </p>
            <p className="mt-1 text-sm text-muted-foreground">
              opened by {data.author} · {data.created}
              {data.updated && data.updated !== data.created && <> · updated {data.updated}</>}
            </p>
          </header>

          <Transcript data={data} />

          <section>
            <Eyebrow className="mb-3">
              discussion{data.comments.length > 0 && <span className="ml-1.5 text-muted-foreground">{data.comments.length}</span>}
            </Eyebrow>
            {data.comments.length === 0 ? (
              <p className="rounded-lg border bg-card px-4 py-6 text-sm text-muted-foreground">No comments yet.</p>
            ) : (
              <div className="flex flex-col gap-2">
                {data.comments.map((c) => (
                  <CommentRow key={c.id} c={c} />
                ))}
              </div>
            )}

            {data.state === "open" && me && (
              <CommentForm owner={owner} name={name} id={mrId} onDone={reload} />
            )}
            {data.state === "open" && !me && (
              <p className="mt-3 text-sm text-muted-foreground">
                <Link to="/login" className="text-primary hover:underline">
                  Sign in
                </Link>{" "}
                to join the discussion.
              </p>
            )}
          </section>

          {data.state === "open" && canWrite && (
            <CloseControls owner={owner} name={name} id={mrId} onDone={reload} />
          )}
        </div>
      )}
    </div>
  )
}

function Transcript({ data }: { data: MrDetailData }) {
  if (data.transcript_redacted) {
    return (
      <section>
        <Eyebrow className="mb-2">dialogue transcript</Eyebrow>
        <p className="rounded-lg border bg-card px-4 py-4 text-sm text-muted-foreground">
          Withheld: the transcript quotes the source agent, which you can't read.
        </p>
      </section>
    )
  }
  if (!data.dialogue_transcript) {
    return (
      <section>
        <Eyebrow className="mb-2">dialogue transcript</Eyebrow>
        <p className="rounded-lg border bg-card px-4 py-4 text-sm text-muted-foreground">
          None yet — this MR was opened before the merge dialogue was run.
        </p>
      </section>
    )
  }
  return (
    <section>
      <Eyebrow className="mb-2">dialogue transcript</Eyebrow>
      <pre className="overflow-auto whitespace-pre-wrap break-words rounded-lg border bg-muted p-4 font-mono text-[0.78rem] leading-relaxed">
        {data.dialogue_transcript}
      </pre>
    </section>
  )
}

function CommentRow({ c }: { c: MrComment }) {
  return (
    <div className="rounded-lg border bg-card px-4 py-3">
      <div className="flex items-center gap-2 font-mono text-[0.72rem] text-muted-foreground">
        <MessageSquare className="size-3.5" />
        <span className="text-foreground/80">{c.author}</span>
        <span>{c.created}</span>
      </div>
      <p className="mt-1.5 whitespace-pre-wrap break-words text-sm">{c.body}</p>
    </div>
  )
}

function CommentForm({ owner, name, id, onDone }: { owner: string; name: string; id: number; onDone: () => void }) {
  const [body, setBody] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api.commentMr(owner, name, id, body.trim())
      setBody("")
      onDone()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form onSubmit={submit} className="mt-3 flex flex-col gap-2">
      <textarea
        value={body}
        onChange={(e) => setBody(e.target.value)}
        placeholder="Add a comment…"
        rows={3}
        className="flex w-full rounded-md border bg-transparent px-3 py-2 text-sm shadow-sm transition-colors placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      />
      <div className="flex items-center gap-2">
        <Button type="submit" size="sm" disabled={busy || !body.trim()}>
          {busy ? "Posting…" : "Comment"}
        </Button>
      </div>
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
    </form>
  )
}

// Close settles the MR; "merged" *records* that someone ran `agit a merge` locally and pushed the
// result — the Hub merges nothing. Both need Write on the target.
function CloseControls({ owner, name, id, onDone }: { owner: string; name: string; id: number; onDone: () => void }) {
  const [state, setState] = useState<"closed" | "merged">("closed")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function submit() {
    setBusy(true)
    setError("")
    try {
      await api.closeMr(owner, name, id, state)
      onDone()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="rounded-lg border bg-card p-4">
      <Eyebrow className="mb-1.5">resolve</Eyebrow>
      <p className="mb-3 max-w-[62ch] text-sm text-muted-foreground">
        Close it, or record that it was merged locally. Recording “merged” does not merge anything here.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <Select
          value={state}
          onChange={(e) => setState(e.target.value as "closed" | "merged")}
          className="w-[140px]"
          aria-label="Resolution"
        >
          <option value="closed">close</option>
          <option value="merged">mark merged</option>
        </Select>
        <Button variant="outline" size="sm" disabled={busy} onClick={submit}>
          {busy ? "Saving…" : state === "merged" ? "Record as merged" : "Close"}
        </Button>
      </div>
      {error && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {error}
        </p>
      )}
    </section>
  )
}
