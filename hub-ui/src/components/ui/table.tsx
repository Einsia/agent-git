import * as React from "react"

import { cn } from "@/lib/utils"

// The shadcn table set, hand-written (no radix — nothing here needs it). Wrapped in a horizontal
// scroller so a wide table never pushes the page body sideways.
function Table({ className, ...props }: React.ComponentProps<"table">) {
  return (
    <div className="relative w-full overflow-x-auto">
      <table className={cn("w-full caption-bottom text-sm", className)} {...props} />
    </div>
  )
}

function TableHeader({ className, ...props }: React.ComponentProps<"thead">) {
  return <thead className={cn("[&_tr]:border-b", className)} {...props} />
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return <tbody className={cn("[&_tr:last-child]:border-0", className)} {...props} />
}

function TableRow({ className, ...props }: React.ComponentProps<"tr">) {
  return <tr className={cn("border-t transition-colors hover:bg-muted/40", className)} {...props} />
}

function TableHead({ className, ...props }: React.ComponentProps<"th">) {
  return (
    <th
      className={cn(
        "eyebrow h-9 px-4 text-left align-middle font-medium [&:has([role=checkbox])]:pr-0",
        className
      )}
      {...props}
    />
  )
}

function TableCell({ className, ...props }: React.ComponentProps<"td">) {
  return <td className={cn("px-4 py-2.5 align-middle", className)} {...props} />
}

export { Table, TableHeader, TableBody, TableRow, TableHead, TableCell }
