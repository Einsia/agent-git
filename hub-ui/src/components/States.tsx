import type { ReactNode } from "react"
import { Link, useLocation } from "react-router-dom"
import { Lock } from "lucide-react"

import { buttonVariants } from "@/components/ui/button"
import { cn } from "@/lib/utils"

// 403: say plainly that access is the problem. Don't bounce to the login form — someone
// already signed in would sign in again and hit the same wall; the redirect would only
// disguise "you aren't authorized" as "you aren't signed in".
//
// `reason` carries the server's own explanation when it has one worth reading; the default
// text is the ordinary case (a private agent you aren't a member of).
export function Forbidden({ what = "this agent", reason }: { what?: string; reason?: string }) {
  const loc = useLocation()
  return (
    <div className="readout rounded-lg border px-6 py-12 text-center">
      <Lock className="mx-auto mb-3 size-6 text-muted-foreground" />
      <p className="mb-1 font-semibold">No access to {what}</p>
      <p className="mx-auto max-w-[46ch] text-sm text-muted-foreground">
        {reason ??
          "It is private and you are not a member. Ask its owner to add you, or sign in as an account that has access."}
      </p>
      <div className="mt-5 flex items-center justify-center gap-2">
        <Link to="/" className={cn(buttonVariants({ variant: "outline", size: "sm" }))}>
          All agents
        </Link>
        <Link
          to={`/login?next=${encodeURIComponent(loc.pathname + loc.search)}`}
          className={cn(buttonVariants({ variant: "ghost", size: "sm" }))}
        >
          Switch account
        </Link>
      </div>
    </div>
  )
}

export function LoadError({ message }: { message: string }) {
  return (
    <div className="rounded-lg border border-destructive/30 bg-destructive/5 px-4 py-6">
      <p className="eyebrow text-destructive">load failed</p>
      <p className="mt-1.5 text-sm text-foreground">{message}</p>
    </div>
  )
}

// The instrument's section label. `.eyebrow` (index.css) is the single source of the spec —
// tracked uppercase mono — so this only adds the element and any per-use color.
export function Eyebrow({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cn("eyebrow", className)}>{children}</p>
}
