import { useEffect, useState, type FormEvent } from "react"
import { useParams, useSearchParams } from "react-router-dom"
import { Search } from "lucide-react"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { Input } from "@/components/ui/input"
import { Button } from "@/components/ui/button"
import { SessionCard } from "@/components/SessionCard"
import { Crumb } from "@/components/Crumb"

export function Agent() {
  const { name = "" } = useParams()
  const [params, setParams] = useSearchParams()
  const page = Math.max(1, Number(params.get("page") ?? 1))
  const q = params.get("q") ?? ""
  const [query, setQuery] = useState(q)

  useEffect(() => setQuery(q), [q])

  const { data, loading, error } = useAsync(() => api.agent(name, page, q), [name, page, q])

  function submitSearch(e: FormEvent) {
    e.preventDefault()
    const next = new URLSearchParams()
    if (query.trim()) next.set("q", query.trim())
    setParams(next)
  }

  function goPage(p: number) {
    const next = new URLSearchParams(params)
    next.set("page", String(p))
    setParams(next)
  }

  const totalPages = data ? Math.max(1, Math.ceil(data.total / data.per_page)) : 1
  const host = data?.git ? location.host : "HOST:PORT"

  return (
    <div>
      <Crumb name={name} />
      <div className="mb-6 flex flex-wrap items-center justify-between gap-3">
        <h1 className="font-mono text-2xl font-bold tracking-tight">{name}</h1>
        <form onSubmit={submitSearch} className="flex gap-2">
          <div className="relative">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search sessions…"
              className="w-[240px] pl-8"
            />
          </div>
          <Button type="submit">Search</Button>
        </form>
      </div>

      <div className="grid grid-cols-1 gap-8 md:grid-cols-[1fr_260px]">
        <div>
          <div className="mb-3 flex items-baseline gap-2">
            <span className="eyebrow">sessions</span>
            {data && <span className="font-mono text-sm tabular-nums text-muted-foreground">{data.total}</span>}
          </div>

          {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
          {error && <p className="py-6 text-destructive">Couldn’t load sessions — {error}</p>}

          {data && data.sessions.length === 0 && (
            <p className="py-6 text-muted-foreground">
              {q ? `No sessions match “${q}”.` : "No sessions yet."}
            </p>
          )}

          <div className="flex flex-col gap-3">
            {data?.sessions.map((s) => <SessionCard key={s.id} name={name} s={s} />)}
          </div>

          {data && totalPages > 1 && (
            <nav className="mt-6 flex items-center justify-center gap-4 text-sm">
              <Button variant="outline" size="sm" disabled={page <= 1} onClick={() => goPage(page - 1)}>
                ← Prev
              </Button>
              <span className="font-mono tabular-nums text-muted-foreground">
                {page} / {totalPages}
              </span>
              <Button variant="outline" size="sm" disabled={page >= totalPages} onClick={() => goPage(page + 1)}>
                Next →
              </Button>
            </nav>
          )}
        </div>

        <aside className="text-sm">
          <h3 className="eyebrow mb-2">pull &amp; merge</h3>
          <pre className="overflow-auto rounded-md border bg-muted p-3 font-mono text-[0.72rem] leading-relaxed">
{`agit clone \\
  http://${host}/${name}.git
agit -a merge origin/main`}
          </pre>
          <h3 className="eyebrow mb-2 mt-6">commits</h3>
          <ul className="space-y-1 text-[0.8rem]">
            {data?.history.map((h) => (
              <li key={h.sha} className="truncate">
                <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.72rem]">{h.sha}</code>{" "}
                {h.subject}
              </li>
            ))}
          </ul>
        </aside>
      </div>
    </div>
  )
}
