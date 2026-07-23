import { useLayoutEffect, useRef, useState } from "react"
import { useWindowVirtualizer } from "@tanstack/react-virtual"
import { Brain, ChevronRight, FilePen, Wrench } from "lucide-react"

import type { Block, Turn } from "@/lib/api"
import { Markdown } from "@/components/Markdown"
import { BlockErrorBoundary } from "@/components/ErrorBoundary"
import { cn } from "@/lib/utils"

// The transcript view: a VIRTUALIZED, Claude-Code/Codex-style render of the ordered conversation.
//
// A session can have HUNDREDS of turns, so the list is windowed with @tanstack/react-virtual's
// useWindowVirtualizer: the page scrolls naturally, and only the turns near the viewport are mounted in
// the DOM. Turns vary wildly in height (a one-line prompt vs a long reply with tool output), so heights
// are MEASURED dynamically (`virtualizer.measureElement`) rather than assumed.
export function Transcript({ turns }: { turns: Turn[] }) {
  const listRef = useRef<HTMLDivElement>(null)
  // The list's absolute offset from the top of the document — the window virtualizer needs it to line
  // its coordinate space up with page scroll. Measured after layout (and on resize).
  const [scrollMargin, setScrollMargin] = useState(0)
  useLayoutEffect(() => {
    const el = listRef.current
    if (!el) return
    const measure = () => setScrollMargin(el.getBoundingClientRect().top + window.scrollY)
    measure()
    window.addEventListener("resize", measure)
    // Layout ABOVE the transcript can change after mount (an async provenance badge loading, a wrapping
    // header) and shift the list's document offset without firing a window resize. A ResizeObserver on the
    // body re-measures so the virtualizer's coordinate space stays aligned and turns are not offset.
    const ro = new ResizeObserver(measure)
    ro.observe(document.body)
    return () => {
      window.removeEventListener("resize", measure)
      ro.disconnect()
    }
  }, [])

  const virtualizer = useWindowVirtualizer({
    count: turns.length,
    // A rough first guess; real heights replace it as each turn is measured on mount.
    estimateSize: () => 180,
    overscan: 6,
    scrollMargin,
  })

  const items = virtualizer.getVirtualItems()

  return (
    <div ref={listRef}>
      <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
        <div
          style={{
            position: "absolute",
            top: 0,
            left: 0,
            width: "100%",
            transform: `translateY(${(items[0]?.start ?? 0) - scrollMargin}px)`,
          }}
        >
          {items.map((item) => (
            <div
              key={item.key}
              data-index={item.index}
              ref={virtualizer.measureElement}
              className="pb-3"
            >
              <TurnGroup turn={turns[item.index]} />
            </div>
          ))}
        </div>
      </div>
    </div>
  )
}

// One turn: a labeled block group (user vs assistant, keyed off the kind-prompt / kind-assist accents),
// rendering its blocks IN ORDER, each by kind.
function TurnGroup({ turn }: { turn: Turn }) {
  const isUser = turn.role === "user"
  const accent = isUser ? "border-kind-prompt" : "border-kind-assist"
  return (
    <div className={cn("rounded-r-md border-l-2 bg-card px-3.5 py-2.5", accent)}>
      <div className="mb-1.5 flex items-center justify-between gap-2">
        <span className="eyebrow">{isUser ? "user" : "assistant"}</span>
        {turn.tools > 0 && (
          <span className="font-mono text-[0.68rem] text-muted-foreground">
            {turn.tools} tool {turn.tools === 1 ? "call" : "calls"}
          </span>
        )}
      </div>
      <div className="space-y-2">
        {turn.blocks.map((b, i) => (
          <BlockView key={i} block={b} />
        ))}
      </div>
      {turn.truncated && (
        <p className="mt-2 text-[0.72rem] text-kind-warn">
          truncated — this turn was clipped; pull the session for the full transcript.
        </p>
      )}
    </div>
  )
}

function BlockView({ block }: { block: Block }) {
  switch (block.kind) {
    case "text":
      // RESILIENT: a single malformed markdown block falls back to plain text instead of crashing the
      // page. SANITIZED: the Markdown renderer carries no raw-HTML sink.
      return (
        <BlockErrorBoundary fallbackText={block.text}>
          <Markdown text={block.text} />
        </BlockErrorBoundary>
      )
    case "thinking":
      return <ThinkingRow text={block.text} />
    case "tool_use":
      return <ToolUseRow name={block.name} input={block.input} />
    case "tool_result":
      return <ToolResultRow output={block.output} />
    case "file_edit":
      return <FileEditRow paths={block.paths} more={block.more} />
  }
}

// The assistant's reasoning: a DISTINCT, QUIET, COLLAPSED-by-default block — a dim "thinking" chip that
// expands to the text. Visually separate from normal assistant prose (dashed, muted, no card accent) so
// it reads as a side channel, not the reply. Thinking is ATTACKER-AUTHORED too, so the expanded text goes
// through the SAME sanitized Markdown renderer (no rehype-raw / dangerouslySetInnerHTML) inside a
// per-block error boundary — never an HTML/script sink.
function ThinkingRow({ text }: { text: string }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded-md border border-dashed border-muted-foreground/25 bg-muted/20">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left"
        aria-expanded={open}
      >
        <Brain className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="eyebrow text-muted-foreground">thinking</span>
        {!open && (
          <span className="min-w-0 flex-1 truncate text-[0.72rem] italic text-muted-foreground/80">
            {text}
          </span>
        )}
        <ChevronRight
          className={cn(
            "ml-auto size-3.5 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-90"
          )}
        />
      </button>
      {open && (
        <div className="border-t border-dashed border-muted-foreground/20 px-2.5 py-2 text-muted-foreground">
          <BlockErrorBoundary fallbackText={text}>
            <Markdown text={text} />
          </BlockErrorBoundary>
        </div>
      )}
    </div>
  )
}

// A compact, collapsible tool call: an icon + the monospace tool name, expanding to show the input
// preview in a mono readout box. The input is ATTACKER-AUTHORED, so it is shown as PLAIN TEXT — never
// fed to a markdown/HTML renderer.
function ToolUseRow({ name, input }: { name: string; input: string }) {
  const [open, setOpen] = useState(false)
  const hasInput = input.trim().length > 0
  return (
    <div className="rounded-md border border-kind-tool/25 bg-kind-tool/[0.06]">
      <button
        type="button"
        onClick={() => hasInput && setOpen((v) => !v)}
        className={cn(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-left",
          hasInput ? "cursor-pointer" : "cursor-default"
        )}
        aria-expanded={hasInput ? open : undefined}
      >
        <Wrench className="size-3.5 shrink-0 text-kind-tool" />
        <span className="font-mono text-[0.78rem] font-medium text-kind-tool">{name}</span>
        {hasInput && (
          <>
            {!open && (
              <span className="min-w-0 flex-1 truncate font-mono text-[0.72rem] text-muted-foreground">
                {input}
              </span>
            )}
            <ChevronRight
              className={cn(
                "ml-auto size-3.5 shrink-0 text-muted-foreground transition-transform",
                open && "rotate-90"
              )}
            />
          </>
        )}
      </button>
      {open && hasInput && (
        <pre className="overflow-x-auto whitespace-pre-wrap break-words border-t border-kind-tool/20 px-2.5 py-2 font-mono text-[0.74rem] leading-relaxed text-card-foreground">
          {input}
        </pre>
      )}
    </div>
  )
}

// A tool's output: a collapsible mono readout box, collapsed by default. ATTACKER-AUTHORED — shown as
// PLAIN TEXT.
function ToolResultRow({ output }: { output: string }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded-md border bg-muted/40">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left"
        aria-expanded={open}
      >
        <ChevronRight
          className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", open && "rotate-90")}
        />
        <span className="eyebrow">result</span>
        {!open && (
          <span className="min-w-0 flex-1 truncate font-mono text-[0.72rem] text-muted-foreground">
            {output}
          </span>
        )}
      </button>
      {open && (
        <pre className="overflow-x-auto whitespace-pre-wrap break-words border-t px-2.5 py-2 font-mono text-[0.74rem] leading-relaxed text-card-foreground">
          {output}
        </pre>
      )}
    </div>
  )
}

// A compact "edited: <paths>" line — the files a turn touched, plus a "+N more" when the list overflowed.
function FileEditRow({ paths, more }: { paths: string[]; more?: number }) {
  return (
    <div className="flex flex-wrap items-center gap-1.5 text-[0.74rem]">
      <FilePen className="size-3.5 shrink-0 text-kind-edit" />
      <span className="eyebrow text-kind-edit">edited</span>
      {paths.map((p) => (
        <code
          key={p}
          className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.72rem] text-muted-foreground"
        >
          {p}
        </code>
      ))}
      {more != null && more > 0 && (
        <span className="font-mono text-[0.72rem] text-muted-foreground">+{more} more</span>
      )}
    </div>
  )
}
