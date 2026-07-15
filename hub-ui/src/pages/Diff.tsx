import { useParams, useSearchParams } from "react-router-dom"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { Crumb } from "@/components/Crumb"

export function Diff() {
  const { name = "", id = "" } = useParams()
  const [params] = useSearchParams()
  const from = params.get("from") ?? ""
  const to = params.get("to") ?? ""
  const missing = !from || !to
  const { data, loading, error } = useAsync(
    // Don't fire a doomed request when a revision is missing — show a clear message instead.
    () => (missing ? Promise.resolve(null) : api.diff(name, id, from, to)),
    [name, id, from, to]
  )

  const empty =
    data &&
    !data.added_prompts.length &&
    !data.removed_prompts.length &&
    !data.added_files.length &&
    !data.removed_files.length &&
    data.conclusion_before === data.conclusion_after

  return (
    <div>
      <Crumb name={name} session={id} />
      <h1 className="text-2xl font-bold tracking-tight">
        revision diff <span className="font-mono text-xl">{id}</span>
      </h1>

      {missing && (
        <p className="mt-4 rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground">
          缺少要比较的版本。请从某条 session 的 revision 列表里选「与上一版 diff」。
        </p>
      )}
      {!missing && loading && <p className="py-6 text-muted-foreground">加载中…</p>}
      {!missing && error && <p className="py-6 text-destructive">加载失败：{error}</p>}

      {data && (
        <>
          <p className="mt-3 rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground">
            比较 <code className="font-mono">{data.from}</code> → <code className="font-mono">{data.to}</code>
            （语义级：只看 prompt / 文件 / 结论的增减）。
          </p>

          <DiffBlock title="新增 / 移除的指令" added={data.added_prompts} removed={data.removed_prompts} />
          <DiffBlock title="改动文件" added={data.added_files} removed={data.removed_files} />
          {data.conclusion_before !== data.conclusion_after && (
            <DiffBlock
              title="结论 / 进展"
              added={data.conclusion_after ? [data.conclusion_after] : []}
              removed={data.conclusion_before ? [data.conclusion_before] : []}
            />
          )}

          {empty && (
            <p className="mt-6 text-muted-foreground">两版在指令 / 文件 / 结论层面没有差异。</p>
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
      <h3 className="mb-2 font-mono text-[0.72rem] uppercase tracking-[0.08em] text-muted-foreground">{title}</h3>
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
