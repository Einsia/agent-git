import type { ComponentType } from "react"
import { ShieldCheck, ShieldAlert, ShieldQuestion, Shield, FileWarning, PenLine } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"
import type { ProvenanceStatus, ProvenanceVerdict } from "@/lib/api"

// The cryptographic-provenance verdict, rendered as a single trust-at-a-glance badge. This is the
// REGISTRY-CLASSIFIED verdict (GET .../session/<id>/provenance), so `verified_as` means the session was
// signed by the registered, email-VERIFIED key of a real account — a positive attribution, not just a
// self-check. The one rule that governs the visual treatment: a `key_mismatch` (a possible forgery) is
// alarming and NEVER green; nothing but a real attribution earns the positive/success colour.

type Treatment = {
  label: (v: ProvenanceVerdict) => string
  icon: ComponentType<{ className?: string }>
  // Full class string for the badge. `verified_as` is the only green; danger states are red and
  // distinct; the rest are muted/neutral so they never compete with a real verdict.
  className: string
  // A muted verdict uses the Badge's own `muted` variant; the coloured ones override via className.
  variant?: "muted"
}

const TREATMENTS: Record<ProvenanceStatus, Treatment> = {
  verified_as: {
    label: (v) => `verified · ${v.username ?? "?"}`,
    icon: ShieldCheck,
    className: "border-kind-edit/30 bg-kind-edit/12 text-kind-edit",
  },
  key_mismatch: {
    label: () => "key mismatch",
    icon: ShieldAlert,
    className: "border-destructive/40 bg-destructive/15 text-destructive font-semibold",
  },
  content_tampered: {
    label: () => "content tampered",
    icon: FileWarning,
    className: "border-destructive/40 bg-destructive/15 text-destructive font-semibold",
  },
  bad_signature: {
    label: () => "bad signature",
    icon: FileWarning,
    className: "border-destructive/40 bg-destructive/15 text-destructive font-semibold",
  },
  signed_unregistered: {
    label: () => "signed (unregistered)",
    icon: ShieldQuestion,
    className: "",
    variant: "muted",
  },
  verified: {
    label: () => "signed",
    icon: PenLine,
    className: "",
    variant: "muted",
  },
  unsigned: {
    label: () => "unsigned",
    icon: Shield,
    className: "",
    variant: "muted",
  },
}

export function ProvenanceBadge({ verdict }: { verdict: ProvenanceVerdict }) {
  // An unknown status the client doesn't model degrades to the neutral unsigned treatment rather than
  // crashing — the same graceful-degradation contract the read path holds to.
  const t = TREATMENTS[verdict.status] ?? TREATMENTS.unsigned
  const Icon = t.icon
  return (
    <Badge variant={t.variant} className={cn(t.className)} title={verdict.summary}>
      <Icon className="size-3" />
      {t.label(verdict)}
    </Badge>
  )
}
