import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"

// An inline notice block. cva-driven variants, same authoring pattern as badge/button: the
// destructive + warn tints reuse the instrument palette (--destructive, --kind-warn) so a warning
// reads as part of the same signal system, not a bolt-on.
const alertVariants = cva(
  "relative w-full rounded-lg border px-4 py-3 text-sm [&>svg]:absolute [&>svg]:left-4 [&>svg]:top-4 [&>svg]:size-4 [&>svg]:text-current [&>svg~*]:pl-6",
  {
    variants: {
      variant: {
        default: "bg-card text-card-foreground",
        destructive: "border-destructive/40 bg-destructive/5 text-destructive",
        warn: "border-kind-warn/40 bg-kind-warn/5 text-kind-warn",
      },
    },
    defaultVariants: { variant: "default" },
  }
)

function Alert({
  className,
  variant,
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof alertVariants>) {
  return <div role="alert" className={cn(alertVariants({ variant }), className)} {...props} />
}

function AlertTitle({ className, ...props }: React.ComponentProps<"div">) {
  return <div className={cn("mb-1 font-semibold leading-none tracking-tight", className)} {...props} />
}

function AlertDescription({ className, ...props }: React.ComponentProps<"div">) {
  return <div className={cn("text-sm [&_p]:leading-relaxed", className)} {...props} />
}

export { Alert, AlertTitle, AlertDescription, alertVariants }
