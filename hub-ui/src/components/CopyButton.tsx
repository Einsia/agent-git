import { useEffect, useState } from "react"
import { Check, Copy } from "lucide-react"

import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"

// clone_url / tokens / the .agit.toml snippet all have to be one click away.
// navigator.clipboard doesn't exist on insecure non-localhost origins (the hub is 127.0.0.1
// by default, where it does — but once --host exposes it over http, it's gone), so keep an
// execCommand fallback rather than let the button go dead.
async function copy(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    // fall through to the fallback below
  }
  try {
    const ta = document.createElement("textarea")
    ta.value = text
    ta.setAttribute("readonly", "")
    ta.style.position = "fixed"
    ta.style.opacity = "0"
    document.body.appendChild(ta)
    ta.select()
    const ok = document.execCommand("copy")
    document.body.removeChild(ta)
    return ok
  } catch {
    return false
  }
}

export function CopyButton({
  value,
  label = "Copy",
  className,
  size = "sm",
  variant = "outline",
}: {
  value: string
  label?: string
  className?: string
  size?: "sm" | "icon" | "default"
  variant?: "outline" | "ghost" | "secondary" | "default"
}) {
  const [state, setState] = useState<"idle" | "ok" | "fail">("idle")

  useEffect(() => {
    if (state === "idle") return
    const t = setTimeout(() => setState("idle"), 1600)
    return () => clearTimeout(t)
  }, [state])

  const icon = state === "ok" ? <Check className="text-kind-edit" /> : <Copy />
  const text = state === "ok" ? "Copied" : state === "fail" ? "Copy failed" : label

  return (
    <Button
      type="button"
      variant={variant}
      size={size}
      className={cn(className)}
      onClick={async () => setState((await copy(value)) ? "ok" : "fail")}
      aria-label={label}
      title={value}
    >
      {icon}
      {size !== "icon" && text}
    </Button>
  )
}
