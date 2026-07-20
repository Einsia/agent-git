import * as React from "react"

import { cn } from "@/lib/utils"

// A plain form label. Kept as thin as the other primitives (no radix), styled to sit above an
// Input the same way the hand-rolled `.eyebrow` labels do elsewhere, but as a real <label> so a
// click focuses its control and screen readers announce the pairing.
function Label({ className, ...props }: React.ComponentProps<"label">) {
  return (
    <label
      className={cn(
        "eyebrow select-none peer-disabled:cursor-not-allowed peer-disabled:opacity-50",
        className
      )}
      {...props}
    />
  )
}

export { Label }
