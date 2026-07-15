import { cn } from "@/lib/utils"

// The session spine: the app's signature. Each event in the session becomes one tick,
// its height and color set by kind (prompt / assistant / tool / edit). Read the rhythm
// of a session at a glance — a burst of edits, a long back-and-forth, a tool-heavy run.
const KIND: Record<string, { h: string; bg: string; label: string }> = {
  p: { h: "h-full", bg: "bg-kind-prompt", label: "prompt" },
  a: { h: "h-3/5", bg: "bg-kind-assist", label: "reply" },
  t: { h: "h-1/3", bg: "bg-kind-tool", label: "tool" },
  e: { h: "h-4/5", bg: "bg-kind-edit", label: "edit" },
}

export function Spine({
  spine,
  className,
  cap = 240,
  height = "h-[18px]",
}: {
  spine: string
  className?: string
  cap?: number
  height?: string
}) {
  if (!spine) return null
  const ticks = [...spine]
  const shown = ticks.slice(0, cap)
  const more = ticks.length - shown.length
  return (
    <div className={cn("flex items-center gap-2", className)}>
      <div
        className={cn("flex items-end gap-px", height)}
        title={`${ticks.length} 个事件：prompt / 回复 / 工具 / 编辑`}
        role="img"
        aria-label={`session spine, ${ticks.length} events`}
      >
        {shown.map((c, i) => {
          const k = KIND[c] ?? KIND.t
          return <span key={i} className={cn("w-[3px] rounded-t-[1px]", k.h, k.bg)} />
        })}
      </div>
      {more > 0 && <span className="font-mono text-[0.62rem] text-muted-foreground">+{more}</span>}
    </div>
  )
}

export function SpineLegend() {
  return (
    <div className="flex flex-wrap items-center gap-3 text-[0.68rem] text-muted-foreground">
      {Object.values(KIND).map((k) => (
        <span key={k.label} className="inline-flex items-center gap-1.5">
          <span className={cn("h-2.5 w-[3px] rounded-sm", k.bg)} />
          {k.label}
        </span>
      ))}
    </div>
  )
}
