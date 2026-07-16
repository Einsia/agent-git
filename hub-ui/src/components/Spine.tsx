import { cn } from "@/lib/utils"

// The session spine: the app's signature, read as an instrument trace. Each event is one
// tick; height + kind-color encode what happened (prompt / reply / tool / edit). You read
// the rhythm at a glance — a burst of edits, a long back-and-forth, a tool-heavy run.
// Heights are px (fraction of the row height) — percentage heights collapse on flex children.
const KIND: Record<string, { f: number; text: string; label: string }> = {
  p: { f: 1, text: "text-kind-prompt", label: "prompt" },
  a: { f: 0.6, text: "text-kind-assist", label: "reply" },
  t: { f: 0.34, text: "text-kind-tool", label: "tool" },
  e: { f: 0.82, text: "text-kind-edit", label: "edit" },
}

export function Spine({
  spine,
  className,
  cap = 200,
  px = 20,
  barW = 3,
  glow = false,
  sweep = false,
}: {
  spine: string
  className?: string
  cap?: number
  px?: number
  barW?: number
  glow?: boolean
  sweep?: boolean
}) {
  if (!spine) return null
  const ticks = [...spine]
  const shown = ticks.slice(0, cap)
  const more = ticks.length - shown.length
  return (
    <div className={cn("flex items-center gap-2", className)}>
      <div
        className={cn("flex items-end gap-[2px]", sweep && "sweep")}
        style={{ height: px }}
        role="img"
        aria-label={`session trace, ${ticks.length} events`}
      >
        {shown.map((c, i) => {
          const k = KIND[c] ?? KIND.t
          return (
            <span
              key={i}
              className={cn("shrink-0 rounded-t-[1px] bg-current", k.text, glow && "glow")}
              style={{
                width: barW,
                height: Math.max(2, Math.round(px * k.f)),
                ...(sweep ? { animationDelay: `${Math.min(i, 90) * 5}ms` } : null),
              }}
            />
          )
        })}
      </div>
      {more > 0 && <span className="font-mono text-[0.62rem] text-muted-foreground">+{more}</span>}
    </div>
  )
}

// The hero readout on a session page: the trace on a graticule, with a power-on sweep and
// a per-kind event tally. This is where the signature is loudest; everything else stays quiet.
export function SpineReadout({ spine, className }: { spine: string; className?: string }) {
  const total = [...spine].length
  return (
    <div className={cn("readout rounded-md border p-4", className)}>
      <div className="mb-3 flex items-center justify-between">
        <span className="eyebrow">trace</span>
        <span className="font-mono text-[0.66rem] tabular-nums text-muted-foreground">
          {total} events
        </span>
      </div>
      <Spine spine={spine} glow sweep cap={520} px={52} barW={4} className="min-h-[52px]" />
      <div className="mt-4 border-t pt-3">
        <SpineLegend spine={spine} />
      </div>
    </div>
  )
}

// Legend of the kind palette. Pass `spine` to also show each kind's count in this session.
export function SpineLegend({ spine }: { spine?: string }) {
  const counts = spine
    ? [...spine].reduce<Record<string, number>>((m, c) => ((m[c] = (m[c] ?? 0) + 1), m), {})
    : null
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-[0.68rem] text-muted-foreground">
      {Object.entries(KIND).map(([key, k]) => (
        <span key={k.label} className="inline-flex items-center gap-1.5">
          <span className={cn("h-2.5 w-[3px] rounded-sm bg-current", k.text)} />
          <span>{k.label}</span>
          {counts && <span className="font-mono tabular-nums text-foreground/70">{counts[key] ?? 0}</span>}
        </span>
      ))}
    </div>
  )
}
