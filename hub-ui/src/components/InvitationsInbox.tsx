import { useState } from "react"
import { Check, Inbox, X } from "lucide-react"

import { api } from "@/lib/api"
import { useAsync } from "@/lib/useAsync"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import {
  Dialog,
  DialogBody,
  DialogDescription,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"

// The signed-in caller's pending org invitations, reachable from the header. A plain useAsync (not
// useGuarded): a 401 here just means "signed out", and the header only mounts this for a signed-in
// caller anyway — a redirect would be wrong.
export function InvitationsInbox() {
  const [open, setOpen] = useState(false)
  const { data, error, reload } = useAsync(() => api.myInvitations(), [])
  const [busyId, setBusyId] = useState<string | null>(null)
  const [actionError, setActionError] = useState("")

  const invites = data ?? []
  const count = invites.length

  async function respond(org: string, id: string, accept: boolean) {
    setBusyId(id)
    setActionError("")
    try {
      if (accept) await api.acceptInvitation(org, id)
      else await api.declineInvitation(org, id)
      reload()
    } catch (err) {
      setActionError(String((err as Error)?.message ?? err))
    } finally {
      setBusyId(null)
    }
  }

  return (
    <>
      <Button
        variant="ghost"
        size="icon"
        className="relative"
        onClick={() => {
          setActionError("")
          reload()
          setOpen(true)
        }}
        aria-label={count > 0 ? `Invitations (${count} pending)` : "Invitations"}
        title="Invitations"
      >
        <Inbox />
        {count > 0 && (
          <span
            className="absolute -right-0.5 -top-0.5 flex min-w-4 items-center justify-center rounded-full bg-primary px-1 font-mono text-[0.6rem] font-semibold leading-4 text-primary-foreground"
            aria-hidden
          >
            {count}
          </span>
        )}
      </Button>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Invitations</DialogTitle>
            <DialogDescription>Pending invitations to join an organization.</DialogDescription>
          </DialogHeader>
          <DialogBody className="flex flex-col gap-3">
            {error && (
              <p role="alert" className="text-sm text-destructive">
                {error}
              </p>
            )}
            {count === 0 && !error && (
              <p className="rounded-lg border bg-muted/40 px-4 py-8 text-center text-sm text-muted-foreground">
                No pending invitations.
              </p>
            )}
            {invites.map((inv) => (
              <div
                key={inv.id}
                className="flex flex-wrap items-center justify-between gap-3 rounded-lg border bg-card px-4 py-3"
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="truncate font-mono text-sm font-semibold">{inv.org}</span>
                    <Badge variant="muted" className="text-[0.6rem]">
                      {inv.role}
                    </Badge>
                  </div>
                  <p className="mt-0.5 text-[0.72rem] text-muted-foreground">
                    invited by <span className="font-mono">{inv.created_by}</span>
                  </p>
                </div>
                <div className="flex items-center gap-2">
                  <Button
                    size="sm"
                    disabled={busyId === inv.id}
                    onClick={() => respond(inv.org, inv.id, true)}
                  >
                    <Check />
                    Accept
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={busyId === inv.id}
                    onClick={() => respond(inv.org, inv.id, false)}
                  >
                    <X />
                    Decline
                  </Button>
                </div>
              </div>
            ))}
            {actionError && (
              <p role="alert" className="text-sm text-destructive">
                {actionError}
              </p>
            )}
          </DialogBody>
        </DialogContent>
      </Dialog>
    </>
  )
}
