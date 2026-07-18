import { useParams, useSearchParams } from "react-router-dom"

import { api } from "@/lib/api"
import { useGuarded } from "@/lib/useGuarded"
import { Crumb } from "@/components/Crumb"
import { Forbidden, LoadError } from "@/components/States"

export function Diff() {
  const { owner = "", name = "", id = "" } = useParams()
  const [params] = useSearchParams()
  const from = params.get("from") ?? ""
  const to = params.get("to") ?? ""
  const missing = !from || !to
  const { data, loading, error, status, forbidden } = useGuarded(
    // Don't fire a doomed request when a revision is missing — show a clear message instead.
    () => (missing ? Promise.resolve(null) : api.diff(owner, name, id, from, to)),
    [owner, name, id, from, to]
  )

  const empty =
    data &&
    !data.added_prompts.length &&
    !data.removed_prompts.length &&
    !data.added_files.length &&
    !data.removed_files.length &&
    data.conclusion_before === data.conclusion_after

  if (forbidden) return <Forbidden what={`${owner}/${name}`} />

  return (
    <div>
      <Crumb owner={owner} name={name} session={id} />
      <span className="eyebrow">revision diff</span>
      <h1 className="mt-1 break-all font-mono text-xl font-bold tracking-tight">{id}</h1>

      {missing && (
        <p className="mt-4 rounded-md border bg-muted px-3 py-2 text-sm text-muted-foreground">
          Pick two revisions to compare. Use “diff vs previous” in a session’s revision list.
        </p>
      )}
      {!missing && loading && <p className="py-6 text-muted-foreground">Loading…</p>}
      {/* 401 is already redirecting to the login form; don't flash an error behind it. */}
      {!missing && error && status !== 401 && (
        <div className="mt-4">
          <LoadError message={error} />
        </div>
      )}

      {data && (
        <>
          <p className="mt-3 rounded-md border bg-muted px-3 py-2 text-sm text-muted-foreground">
            Comparing <code className="font-mono">{data.from}</code> →{" "}
            <code className="font-mono">{data.to}</code> · semantic (prompts, files, conclusion)
          </p>

          <DiffBlock title="instructions" added={data.added_prompts} removed={data.removed_prompts} />
          <DiffBlock title="files changed" added={data.added_files} removed={data.removed_files} />
          {data.conclusion_before !== data.conclusion_after && (
            <DiffBlock
              title="conclusion"
              added={data.conclusion_after ? [data.conclusion_after] : []}
              removed={data.conclusion_before ? [data.conclusion_before] : []}
            />
          )}

          {empty && (
            <p className="mt-6 text-muted-foreground">No differences in prompts, files, or conclusion.</p>
          )}
        </>
      )}
    </div>
  )
}

function DiffBlock({ title, added, removed }: { title: string; added: string[]; removed: string[] }) {
  if (!added.length && !removed.length) return null
  return (
    <section className="mt-5">
      <h3 className="eyebrow mb-2">{title}</h3>
      <ul className="space-y-1">
        {removed.map((x, i) => (
          <li
            key={`r${i}`}
            className="whitespace-pre-wrap break-words rounded-md bg-kind-warn/15 px-3 py-1.5 font-mono text-[0.82rem] text-kind-warn"
          >
            − {x.split("\n")[0]}
          </li>
        ))}
        {added.map((x, i) => (
          <li
            key={`a${i}`}
            className="whitespace-pre-wrap break-words rounded-md bg-kind-edit/15 px-3 py-1.5 font-mono text-[0.82rem] text-kind-edit"
          >
            ＋ {x.split("\n")[0]}
          </li>
        ))}
      </ul>
    </section>
  )
}
