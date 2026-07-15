import type { ReactNode } from "react"
import { Link, useParams, useSearchParams } from "react-router-dom"
import { GitCompare } from "lucide-react"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { Crumb } from "@/components/Crumb"
import { Spine } from "@/components/Spine"
import { ProvChips } from "@/components/ProvChips"

export function Session() {
  const { name = "", id = "" } = useParams()
  const [params] = useSearchParams()
  const at = params.get("at") ?? undefined
  const { data, loading, error } = useAsync(() => api.session(name, id, at), [name, id, at])

  return (
    <div>
      <Crumb name={name} session={id} />
      {loading && <p className="py-6 text-muted-foreground">加载中…</p>}
      {error && <p className="py-6 text-destructive">加载失败：{error}</p>}

      {data && (
        <>
          <div className="flex flex-wrap items-start justify-between gap-4">
            <div>
              <h1 className="text-2xl font-bold tracking-tight">
                会话 <span className="font-mono text-xl">{data.id}</span>
              </h1>
              <div className="mt-2.5">
                <ProvChips
                  runtime={data.runtime}
                  model={data.model}
                  branch={data.branch}
                  author={data.author}
                  when={data.when}
                />
              </div>
            </div>
            <Spine spine={data.spine} height="h-6" cap={320} />
          </div>

          {data.pinned && (
            <p className="mt-4 rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground">
              正在看 pin 到 <code className="font-mono">{data.pinned}</code> 的历史版本。{" "}
              <Link className="text-primary hover:underline" to={`/agent/${name}/session/${id}`}>
                回到最新
              </Link>
            </p>
          )}

          <div className="mt-6 grid grid-cols-1 gap-8 md:grid-cols-[1fr_280px]">
            <div>
              <Section title={`要它做的（${data.prompts.length}）`}>
                <ul className="divide-y">
                  {data.prompts.map((p, i) => (
                    <li key={i} className="py-2 text-[0.92rem]">
                      {p.split("\n")[0]}
                    </li>
                  ))}
                </ul>
              </Section>

              <Section title="它说过的（节选）">
                <div className="space-y-2">
                  {data.texts.slice(-6).map((t, i) => (
                    <p
                      key={i}
                      className="rounded-r-md border-l-2 border-kind-assist bg-card px-3 py-2 text-[0.92rem]"
                    >
                      {t.slice(0, 600)}
                    </p>
                  ))}
                </div>
              </Section>

              {data.files.length > 0 && (
                <Section title="改动文件">
                  <div className="flex flex-wrap gap-1.5">
                    {data.files.map((f) => (
                      <code
                        key={f}
                        className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.74rem] text-muted-foreground"
                      >
                        {f}
                      </code>
                    ))}
                  </div>
                </Section>
              )}
            </div>

            <aside>
              <h3 className="mb-2 font-mono text-[0.72rem] uppercase tracking-[0.08em] text-muted-foreground">
                revision（{data.revisions.length}）
              </h3>
              <ul className="space-y-1.5 text-[0.8rem]">
                {data.revisions.map((r, i) => {
                  const prev = data.revisions[i + 1]
                  const shortSha = r.sha.slice(0, 9)
                  return (
                    <li key={r.sha} className="border-b pb-1.5">
                      <Link
                        to={`/agent/${name}/session/${id}?at=${shortSha}`}
                        className="font-mono text-primary hover:underline"
                      >
                        {shortSha}
                      </Link>{" "}
                      <span className="font-mono text-[0.72rem] text-muted-foreground">{r.when}</span>
                      <div className="text-muted-foreground">{r.subject}</div>
                      {prev && (
                        <Link
                          to={`/agent/${name}/session/${id}/diff?from=${prev.sha.slice(0, 9)}&to=${shortSha}`}
                          className="inline-flex items-center gap-1 text-[0.72rem] text-primary hover:underline"
                        >
                          <GitCompare className="size-3" /> 与上一版 diff
                        </Link>
                      )}
                    </li>
                  )
                })}
              </ul>

              <h3 className="mb-2 mt-6 font-mono text-[0.72rem] uppercase tracking-[0.08em] text-muted-foreground">
                拉取并 resume
              </h3>
              <pre className="overflow-auto rounded-lg bg-muted p-3 font-mono text-[0.72rem] leading-relaxed">
{`agit clone \\
  http://${location.host}/${name}.git
agit -a reconcile origin/main`}
              </pre>
              <p className="mt-3 font-mono text-[0.72rem] text-muted-foreground">{data.commit.slice(0, 12)}</p>
            </aside>
          </div>
        </>
      )}
    </div>
  )
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="mb-6">
      <h3 className="mb-2 font-mono text-[0.72rem] uppercase tracking-[0.08em] text-muted-foreground">
        {title}
      </h3>
      {children}
    </section>
  )
}
