import * as React from "react"
import { createPortal } from "react-dom"
import { X } from "lucide-react"

import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"

// A controlled modal dialog, hand-written to match the no-radix rule the other primitives follow
// (see ui/select). It is deliberately small but keeps the accessibility a modal must have:
//   • role="dialog" + aria-modal, labelled by its title via aria-labelledby;
//   • Escape closes; a backdrop click closes; focus moves into the panel on open and returns to the
//     trigger on close; Tab is trapped inside the panel while it is open;
//   • the body scroll is locked so the page behind doesn't move.
// The API mirrors shadcn's controlled shape: <Dialog open onOpenChange><DialogContent/></Dialog>.

interface DialogCtx {
  onOpenChange: (open: boolean) => void
  titleId: string
}
const Ctx = React.createContext<DialogCtx | null>(null)

function Dialog({
  open,
  onOpenChange,
  children,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  children: React.ReactNode
}) {
  const titleId = React.useId()
  if (!open) return null
  return <Ctx.Provider value={{ onOpenChange, titleId }}>{children}</Ctx.Provider>
}

function useDialog() {
  const ctx = React.useContext(Ctx)
  if (!ctx) throw new Error("Dialog subcomponents must be used inside <Dialog>")
  return ctx
}

const FOCUSABLE =
  'a[href],button:not([disabled]),textarea:not([disabled]),input:not([disabled]),select:not([disabled]),[tabindex]:not([tabindex="-1"])'

function DialogContent({ className, children, ...props }: React.ComponentProps<"div">) {
  const { onOpenChange, titleId } = useDialog()
  const panelRef = React.useRef<HTMLDivElement>(null)
  const restoreRef = React.useRef<HTMLElement | null>(null)

  React.useEffect(() => {
    restoreRef.current = document.activeElement as HTMLElement | null
    const prevOverflow = document.body.style.overflow
    document.body.style.overflow = "hidden"
    // Focus the panel (or its first focusable child) once mounted.
    const panel = panelRef.current
    const first = panel?.querySelector<HTMLElement>(FOCUSABLE)
    ;(first ?? panel)?.focus()

    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.stopPropagation()
        onOpenChange(false)
        return
      }
      if (e.key === "Tab" && panel) {
        const items = Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
          (el) => el.offsetParent !== null
        )
        if (items.length === 0) {
          e.preventDefault()
          panel.focus()
          return
        }
        const first = items[0]
        const last = items[items.length - 1]
        const active = document.activeElement
        if (e.shiftKey && active === first) {
          e.preventDefault()
          last.focus()
        } else if (!e.shiftKey && active === last) {
          e.preventDefault()
          first.focus()
        }
      }
    }
    document.addEventListener("keydown", onKey, true)
    return () => {
      document.removeEventListener("keydown", onKey, true)
      document.body.style.overflow = prevOverflow
      restoreRef.current?.focus?.()
    }
  }, [onOpenChange])

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/55 p-4 pt-[10vh] backdrop-blur-sm"
      onMouseDown={(e) => {
        // Only a click that starts AND lands on the backdrop closes — a drag that ends outside
        // an input shouldn't dismiss the dialog.
        if (e.target === e.currentTarget) onOpenChange(false)
      }}
    >
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className={cn(
          "relative w-full max-w-lg rounded-xl border bg-card text-card-foreground shadow-lg outline-none",
          className
        )}
        {...props}
      >
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="absolute right-2.5 top-2.5 size-8 text-muted-foreground"
          onClick={() => onOpenChange(false)}
          aria-label="Close"
        >
          <X />
        </Button>
        {children}
      </div>
    </div>,
    document.body
  )
}

function DialogHeader({ className, ...props }: React.ComponentProps<"div">) {
  return <div className={cn("flex flex-col gap-1.5 p-5 pb-0", className)} {...props} />
}

function DialogTitle({ className, ...props }: React.ComponentProps<"h2">) {
  const { titleId } = useDialog()
  return (
    <h2
      id={titleId}
      className={cn("text-lg font-semibold leading-none tracking-tight", className)}
      {...props}
    />
  )
}

function DialogDescription({ className, ...props }: React.ComponentProps<"p">) {
  return <p className={cn("text-sm text-muted-foreground", className)} {...props} />
}

function DialogBody({ className, ...props }: React.ComponentProps<"div">) {
  return <div className={cn("p-5", className)} {...props} />
}

function DialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn("flex flex-wrap items-center justify-end gap-2 p-5 pt-0", className)}
      {...props}
    />
  )
}

export {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogBody,
  DialogFooter,
}
