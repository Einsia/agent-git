import { useState, type ReactNode } from "react"
import { Link, useParams, useSearchParams } from "react-router-dom"
import { GitCompare, Loader2, Maximize2, Minimize2 } from "lucide-react"

import { api } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { useAsync } from "@/lib/useAsync"
import { Crumb } from "@/components/Crumb"
import { SpineReadout } from "@/components/Spine"
import { ProvChips } from "@/components/ProvChips"
import { ProvenanceBadge } from "@/components/ProvenanceBadge"
import { Forbidden, LoadError } from "@/components/States"
import { Transcript } from "@/components/Transcript"

export function Session() {
  const { owner = "", name = "", id = "" } = useParams()
  const [params] = useSearchParams()
  const at = params.get("at") ?? undefined
  // The default view is the light, clipped/capped transcript. Toggling `full` refetches with ?full=1 to
  // pull the UNCLIPPED transcript, rendered through the SAME virtualized list so even a huge one stays
  // performant. useAsync preserves the previous data across a refetch, so the old view stays on screen
  // (with a subtle loading state) instead of blanking while the full payload loads.
  const [full, setFull] = useState(false)
  const { data, loading, error, status, forbidden } = useGuarded(
    () => api.session(owner, name, id, at, full),
    [owner, name, id, at, full]
  )
  // The live, registry-classified provenance verdict, fetched alongside the session (its own read so a
  // slow registry lookup never holds up the transcript). Best-effort: a failure just hides the badge —
  // the session gate above already owns the 401/403 routing, so this needn't redirect.
  const prov = useAsync(() => api.sessionProvenance(owner, name, id, at), [owner, name, id, at])

  if (forbidden) return <Forbidden what={`${owner}/${name}`} />

  return (
    <div>
      <Crumb owner={owner} name={name} session={id} />
      {/* Only the FIRST load blanks to "Loading…"; a full/clipped switch keeps the old view up (data is
          preserved across the refetch) and shows a subtle inline indicator by the toggle instead. */}
      {loading && !data && <p className="py-6 text-muted-foreground">Loading…</p>}
      {/* 401 is already redirecting to the login form; don't flash an error behind it. */}
      {error && status !== 401 && <LoadError message={error} />}

      {data && (
        <>
          <div className="mb-5">
            <span className="eyebrow">session</span>
            <h1 className="mt-1 break-all font-mono text-xl font-bold tracking-tight">{data.id}</h1>
            <div className="mt-2.5 flex flex-wrap items-center gap-1.5">
              <ProvChips
                runtime={data.runtime}
                model={data.model}
                branch={data.branch}
                author={data.author}
                when={data.when}
              />
              {prov.data && <ProvenanceBadge verdict={prov.data} />}
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
              <Section
                title={`conversation · ${data.turns.length} turns`}
                action={
                  <div className="flex items-center gap-2">
                    {loading && (
                      <span className="inline-flex items-center gap-1 text-[0.72rem] text-muted-foreground">
                        <Loader2 className="size-3 animate-spin" />
                        loading
                      </span>
                    )}
                    <button
                      type="button"
                      onClick={() => setFull((v) => !v)}
                      disabled={loading}
                      aria-pressed={full}
                      className="inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-[0.75rem] text-muted-foreground transition-colors hover:bg-muted disabled:opacity-60"
                    >
                      {full ? <Minimize2 className="size-3" /> : <Maximize2 className="size-3" />}
                      {full ? "Clipped view" : "View full transcript"}
                    </button>
                  </div>
                }
              >
                {data.turns.length === 0 ? (
                  <p className="text-sm text-muted-foreground">No readable turns in this session.</p>
                ) : (
                  <>
                    <Transcript turns={data.turns} />
                    {data.turns_capped && (
                      <p className="mt-3 rounded-md border bg-muted px-3 py-2 text-sm text-muted-foreground">
                        {full
                          ? "This conversation is very long; even the full view is capped. Pull the session for everything."
                          : "This conversation is long; the view is truncated. Switch to the full transcript, or pull the session."}
                      </p>
                    )}
                  </>
                )}
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
  ${location.origin}/${owner}/${name}.git
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

function Section({ title, children, action }: { title: string; children: ReactNode; action?: ReactNode }) {
  return (
    <section className="mb-6">
      <div className="mb-2 flex items-center justify-between gap-2">
        <h3 className="eyebrow">{title}</h3>
        {action}
      </div>
      {children}
    </section>
  )
}
