import * as React from "react"

import { cn } from "@/lib/utils"

// A native select: every primitive in ui/ is hand-written with no radix dependency, and this
// one doesn't introduce it either.
function Select({ className, ...props }: React.ComponentProps<"select">) {
  return (
    <select
      className={cn(
        "flex h-9 w-full cursor-pointer appearance-none rounded-md border bg-transparent px-3 py-1 text-sm shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50",
        // The native dropdown inherits the system's white background in dark mode; state the
        // option colors explicitly.
        "[&>option]:bg-card [&>option]:text-card-foreground",
        className
      )}
      {...props}
    />
  )
}

export { Select }
