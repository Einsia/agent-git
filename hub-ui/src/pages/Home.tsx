import { Link } from "react-router-dom"
import { ArrowUpRight } from "lucide-react"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { SpineLegend } from "@/components/Spine"

export function Home() {
  const { data, loading, error } = useAsync(() => api.agents(), [])

  return (
    <div>
      <section className="border-b pb-8">
        <p className="mb-3 font-mono text-[0.7rem] uppercase tracking-[0.16em] text-primary">
          版本化的 agent 工作记忆
        </p>
        <h1 className="max-w-[18ch] text-4xl font-bold leading-[1.1] tracking-tight">
          团队里每个 agent 的会话，<span className="text-primary">可读、可信、可拉</span>。
        </h1>
        <p className="mt-4 max-w-[60ch] text-muted-foreground">
          每条 session 是一次 agent 工作的完整记录——它读过什么、跑过什么、下一步是什么。
          在这里判断该不该把别人的上下文拉进自己的项目。
        </p>
        <div className="mt-5">
          <SpineLegend />
        </div>
      </section>

      <div className="mt-8">
        <div className="grid grid-cols-[1fr_2fr_auto] gap-4 px-4 pb-2 font-mono text-[0.68rem] uppercase tracking-[0.09em] text-muted-foreground">
          <span>agent</span>
          <span>最近活动</span>
          <span className="text-right">规模</span>
        </div>

        {loading && <Skeleton rows={3} />}
        {error && <p className="px-4 py-6 text-destructive">加载失败：{error}</p>}

        {data && (
          <div className="overflow-hidden rounded-xl border bg-card">
            {data.agents.length === 0 && (
              <p className="px-4 py-8 text-muted-foreground">
                还没有托管的 agent。<code className="rounded bg-muted px-1.5 py-0.5 font-mono text-sm">agit-hub add &lt;name&gt;</code> 新建一个。
              </p>
            )}
            {data.agents.map((a) => (
              <Link
                key={a.name}
                to={`/agent/${a.name}`}
                className="grid grid-cols-[1fr_2fr_auto] items-center gap-4 border-t px-4 py-3.5 first:border-t-0 transition-colors hover:bg-accent/40"
              >
                <span className="flex items-center gap-1.5 font-mono font-semibold">
                  {a.name}
                  <ArrowUpRight className="size-3.5 text-muted-foreground" />
                </span>
                <span className="min-w-0 truncate text-sm text-muted-foreground">
                  <span className="mr-2 font-mono text-[0.78rem] text-muted-foreground/70">{a.when || "空"}</span>
                  {a.subject || "尚未 push"}
                </span>
                <span className="text-right font-mono font-semibold tabular-nums">
                  {a.sessions}
                  <span className="block text-[0.62rem] font-normal uppercase tracking-wide text-muted-foreground">
                    session
                  </span>
                </span>
              </Link>
            ))}
          </div>
        )}

        {data && (
          <footer className="mt-8 flex flex-wrap gap-6 border-t pt-4 text-sm text-muted-foreground">
            <span>
              API <Link className="text-primary hover:underline" to="/api/agents" reloadDocument>/api/agents</Link>
            </span>
            <span>
              发布{" "}
              <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                agit -a push http://{data.host}/&lt;name&gt;.git
              </code>
            </span>
          </footer>
        )}
      </div>
    </div>
  )
}

function Skeleton({ rows }: { rows: number }) {
  return (
    <div className="overflow-hidden rounded-xl border bg-card">
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="grid grid-cols-[1fr_2fr_auto] gap-4 border-t px-4 py-4 first:border-t-0">
          <div className="h-4 w-24 animate-pulse rounded bg-muted" />
          <div className="h-4 w-full animate-pulse rounded bg-muted" />
          <div className="h-4 w-10 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </div>
  )
}
