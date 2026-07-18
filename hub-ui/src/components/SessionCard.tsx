import { Link } from "react-router-dom"

import type { SessionSummary } from "@/lib/api"
import { Card } from "@/components/ui/card"
import { Spine } from "@/components/Spine"
import { ProvChips } from "@/components/ProvChips"

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

export function SessionCard({ owner, name, s }: { owner: string; name: string; s: SessionSummary }) {
  return (
    <Card className="p-4 transition-colors hover:border-primary/50">
      <div className="flex items-center justify-between gap-4">
        <Link
          to={`/agent/${owner}/${name}/session/${s.id}`}
          className="break-all font-mono text-[0.82rem] text-primary hover:underline"
        >
          {s.id}
        </Link>
        <Spine spine={s.spine} />
      </div>

      <div className="mt-2.5 text-[1.05rem] font-semibold leading-snug">
        {s.title || <span className="text-muted-foreground">Untitled session</span>}
      </div>
      {s.conclusion && (
        <p className="mt-1 line-clamp-2 text-[0.92rem] text-muted-foreground">{s.conclusion}</p>
      )}

      {s.files.length > 0 && (
        <div className="mt-2.5 flex flex-wrap gap-1.5">
          {s.files.slice(0, 8).map((f) => (
            <code key={f} className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.72rem] text-muted-foreground">
              {f}
            </code>
          ))}
        </div>
      )}

      <div className="mt-3 flex flex-wrap items-center gap-2 border-t pt-2.5">
        <ProvChips
          runtime={s.runtime}
          model={s.model}
          branch={s.branch}
          author={s.author}
          when={s.when}
        />
        <span className="ml-auto font-mono text-[0.72rem] tabular-nums text-muted-foreground">
          {plural(s.n_prompts, "prompt")} · {plural(s.n_texts, "reply", "replies")} ·{" "}
          {plural(s.tools, "tool")}
        </span>
      </div>
    </Card>
  )
}
