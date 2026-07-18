import type { ReactNode } from "react"
import { Link, useParams, useSearchParams } from "react-router-dom"
import { GitCompare } from "lucide-react"

import { api } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { Crumb } from "@/components/Crumb"
import { SpineReadout } from "@/components/Spine"
import { ProvChips } from "@/components/ProvChips"
import { Forbidden, LoadError } from "@/components/States"

export function Session() {
  const { owner = "", name = "", id = "" } = useParams()
  const [params] = useSearchParams()
  const at = params.get("at") ?? undefined
  const { data, loading, error, status, forbidden } = useGuarded(
    () => api.session(owner, name, id, at),
    [owner, name, id, at]
  )

  if (forbidden) return <Forbidden what={`${owner}/${name}`} />

  return (
    <div>
      <Crumb owner={owner} name={name} session={id} />
      {loading && <p className="py-6 text-muted-foreground">Loading…</p>}
      {/* 401 is already redirecting to the login form; don't flash an error behind it. */}
      {error && status !== 401 && <LoadError message={error} />}

      {data && (
        <>
          <div className="mb-5">
            <span className="eyebrow">session</span>
            <h1 className="mt-1 break-all font-mono text-xl font-bold tracking-tight">{data.id}</h1>
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

          <SpineReadout spine={data.spine} />

          {data.pinned && (
            <p className="mt-4 rounded-md border bg-muted px-3 py-2 text-sm text-muted-foreground">
              Viewing revision <code className="font-mono">{data.pinned}</code>.{" "}
              <Link className="text-primary hover:underline" to={`/agent/${owner}/${name}/session/${id}`}>
                Back to latest
              </Link>
            </p>
          )}

          <div className="mt-7 grid grid-cols-1 gap-8 md:grid-cols-[1fr_260px]">
            <div>
              <Section title={`prompts · ${data.prompts.length}`}>
                <ul className="divide-y">
                  {data.prompts.map((p, i) => (
                    <li key={i} className="py-2 text-[0.92rem]">
                      {p.split("\n")[0]}
                    </li>
                  ))}
                </ul>
              </Section>

              <Section title="assistant · excerpt">
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
                <Section title="files changed">
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
              <h3 className="eyebrow mb-2">revisions · {data.revisions.length}</h3>
              <ul className="space-y-1.5 text-[0.8rem]">
                {data.revisions.map((r, i) => {
                  const prev = data.revisions[i + 1]
                  const shortSha = r.sha.slice(0, 9)
                  return (
                    <li key={r.sha} className="border-b pb-1.5">
                      <Link
                        to={`/agent/${owner}/${name}/session/${id}?at=${shortSha}`}
                        className="font-mono text-primary hover:underline"
                      >
                        {shortSha}
                      </Link>{" "}
                      <span className="font-mono text-[0.72rem] text-muted-foreground">{r.when}</span>
                      <div className="text-muted-foreground">{r.subject}</div>
                      {prev && (
                        <Link
                          to={`/agent/${owner}/${name}/session/${id}/diff?from=${prev.sha.slice(0, 9)}&to=${shortSha}`}
                          className="inline-flex items-center gap-1 text-[0.72rem] text-primary hover:underline"
                        >
                          <GitCompare className="size-3" /> diff vs previous
                        </Link>
                      )}
                    </li>
                  )
                })}
              </ul>

              <h3 className="eyebrow mb-2 mt-6">pull &amp; resume</h3>
              <pre className="overflow-auto rounded-md border bg-muted p-3 font-mono text-[0.72rem] leading-relaxed">
{`agit clone \\
  http://${location.host}/${owner}/${name}.git
agit -a merge origin/main`}
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
      <h3 className="eyebrow mb-2">{title}</h3>
      {children}
    </section>
  )
}
