import { useEffect, useState, type ReactNode } from "react"
import { QRCodeSVG } from "qrcode.react"
import {
  KeyRound,
  MailCheck,
  MailWarning,
  Send,
  ShieldCheck,
  ShieldOff,
  ShieldPlus,
  Trash2,
  TriangleAlert,
} from "lucide-react"

import { ApiError, api, type SigningKey } from "@/lib/api"
import { useSession } from "@/lib/session"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Badge } from "@/components/ui/badge"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { InputOTP } from "@/components/ui/input-otp"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { CopyButton } from "@/components/CopyButton"
import { Eyebrow } from "@/components/States"

export function Account() {
  const { me, loading } = useSession()

  if (loading) return <p className="py-10 text-muted-foreground">Loading…</p>
  // Account settings are personal; a signed-out caller has no account to configure.
  if (!me)
    return (
      <div className="readout rounded-lg border px-6 py-12 text-center">
        <p className="mb-1 font-semibold">Sign in to manage your account</p>
        <p className="mx-auto max-w-[46ch] text-sm text-muted-foreground">
          Two-factor authentication and your other account settings live here once you're signed in.
        </p>
      </div>
    )

  return (
    <div className="flex flex-col gap-8">
      <header>
        <span className="eyebrow">account</span>
        <h1 className="mt-1 flex flex-wrap items-baseline gap-2.5 text-2xl font-bold tracking-tight">
          Account
          <span className="font-mono text-base font-normal text-muted-foreground">{me.username}</span>
        </h1>
        <p className="mt-1 max-w-[62ch] text-sm text-muted-foreground">
          Your sign-in security. Tokens for git and scripts live under{" "}
          <a href="/tokens" className="text-primary hover:underline">
            tokens
          </a>
          .
        </p>
      </header>

      <EmailCard />
      <SigningKeysCard username={me.username} />
      <TwoFactorCard />
    </div>
  )
}

// A shortened, monospace fingerprint for display — the full value is on the clipboard / in `agit identity
// keys`. A device key's fingerprint is already short (32 hex chars); we group it for legibility.
function shortFpr(fpr: string): string {
  return fpr.length > 20 ? `${fpr.slice(0, 20)}…` : fpr
}

// The signing-keys card: the account's registered device keys (SSH-keys style, many per account), each
// with its label, fingerprint, and when it was added, plus a per-key Revoke. New keys are added from the
// CLI (`agit identity enroll`) — the private half never leaves the machine — so this page only lists and
// revokes. A revoked key stops verifying the owner's sessions and loses encryption access on the next
// rewrap. Mirrors GitHub's "SSH and GPG keys" screen.
function SigningKeysCard({ username }: { username: string }) {
  const [keys, setKeys] = useState<SigningKey[] | null>(null)
  const [error, setError] = useState("")
  const [revoking, setRevoking] = useState<string | null>(null)

  async function load() {
    setError("")
    try {
      const set = await api.signingKeys(username)
      setKeys(set.keys)
    } catch (err) {
      // 404 = the account has no non-revoked device key yet. That's not an error — show the empty state.
      if (err instanceof ApiError && err.status === 404) {
        setKeys([])
      } else {
        setError(String((err as Error)?.message ?? err))
        setKeys([])
      }
    }
  }

  useEffect(() => {
    void load()
    // Reload when the account changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [username])

  async function revoke(fpr: string) {
    setRevoking(fpr)
    setError("")
    try {
      await api.revokeSigningKey(fpr)
      await load()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setRevoking(null)
    }
  }

  return (
    <section>
      <Eyebrow className="mb-1.5">signing keys</Eyebrow>
      <div className="rounded-lg border bg-card p-5">
        <div className="flex items-start gap-3">
          <KeyRound className="mt-0.5 size-5 shrink-0 text-primary" />
          <div className="min-w-0 flex-1">
            <p className="font-semibold">Device keys</p>
            <p className="mt-0.5 max-w-[58ch] text-sm text-muted-foreground">
              One key per machine you enroll. A session is attributed to you when it's signed by{" "}
              <em>any</em> of these keys, so a second laptop no longer clobbers the first. Add one from a
              machine with <code className="font-mono">agit identity enroll</code> — the private half never
              leaves it.
            </p>
          </div>
        </div>

        <div className="mt-5">
          {keys === null ? (
            <p className="text-sm text-muted-foreground">Loading…</p>
          ) : keys.length === 0 ? (
            <div className="rounded-lg border bg-muted/30 p-5">
              <p className="text-sm font-medium">No device keys yet</p>
              <p className="mt-1 max-w-[54ch] text-sm text-muted-foreground">
                Run <code className="font-mono">agit identity enroll</code> on a machine to publish its
                signing key here. Until then, your signed sessions show as unregistered.
              </p>
            </div>
          ) : (
            <ul className="flex flex-col divide-y rounded-lg border">
              {keys.map((k) => (
                <li key={k.key_fpr} className="flex flex-wrap items-center justify-between gap-3 p-4">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-semibold">{k.label || "unlabelled device"}</span>
                      <Badge variant="outline" className="font-mono text-xs">
                        {shortFpr(k.key_fpr)}
                      </Badge>
                    </div>
                    <p className="mt-1 text-xs text-muted-foreground">
                      Added {k.created || "unknown"}
                      {k.epoch > 0 ? ` · rotated to epoch ${k.epoch}` : ""}
                    </p>
                  </div>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => revoke(k.key_fpr)}
                    disabled={revoking !== null}
                    className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                  >
                    <Trash2 />
                    {revoking === k.key_fpr ? "Revoking…" : "Revoke"}
                  </Button>
                </li>
              ))}
            </ul>
          )}
        </div>

        {error && (
          <p role="alert" className="mt-3 text-sm text-destructive">
            {error}
          </p>
        )}
      </div>
    </section>
  )
}

// A pasted verification value may be the whole emailed link or the bare token. Pull the token out of a
// `...?token=<t>` URL; otherwise take the trimmed input as-is.
function extractToken(pasted: string): string {
  const s = pasted.trim()
  const m = s.match(/[?&]token=([^&\s]+)/)
  return m ? decodeURIComponent(m[1]) : s
}

// The email card: shows the account's registered email with a Verified / Unverified badge (from
// GET /api/me), and — while unverified — a Resend button plus a field to paste the token/link. This is
// the account-facing half of the email-squatting defense: only a VERIFIED email is attributed in
// provenance verification (see src/hub/store.rs::get_identity_key_by_email).
function EmailCard() {
  const { me, refresh } = useSession()
  const [pasted, setPasted] = useState("")
  const [busy, setBusy] = useState<"resend" | "verify" | null>(null)
  const [error, setError] = useState("")
  const [notice, setNotice] = useState("")

  const email = me?.email ?? ""
  const verified = !!me?.email_verified

  async function resend() {
    setBusy("resend")
    setError("")
    setNotice("")
    try {
      await api.resendEmailVerification()
      setNotice(
        "A fresh verification link was generated. Your hub operator forwards it to your email address — paste the token or link below once you have it."
      )
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(null)
    }
  }

  async function verify() {
    const token = extractToken(pasted)
    if (!token || busy) return
    setBusy("verify")
    setError("")
    setNotice("")
    try {
      await api.verifyEmail(token)
      setPasted("")
      await refresh() // flip the badge to Verified
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(null)
    }
  }

  return (
    <section>
      <Eyebrow className="mb-1.5">email</Eyebrow>
      <div className="rounded-lg border bg-card p-5">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            {verified ? (
              <MailCheck className="mt-0.5 size-5 shrink-0 text-primary" />
            ) : (
              <MailWarning className="mt-0.5 size-5 shrink-0 text-kind-warn" />
            )}
            <div>
              <p className="font-semibold">Committer email</p>
              <p className="mt-0.5 max-w-[54ch] text-sm text-muted-foreground">
                {email ? (
                  <>
                    <span className="font-mono text-foreground">{email}</span> is the address your signed
                    sessions are attributed under. It must be verified before the hub will attribute pushes
                    to you.
                  </>
                ) : (
                  <>
                    No email on file yet. Enroll a signing identity with an email (
                    <code className="font-mono">agit identity enroll</code>) to attribute your pushes to
                    this account.
                  </>
                )}
              </p>
            </div>
          </div>
          {email &&
            (verified ? (
              <Badge variant="default" className="gap-1">
                <MailCheck className="size-3" />
                Verified
              </Badge>
            ) : (
              <Badge variant="outline" className="gap-1 border-kind-warn/40 text-kind-warn">
                <MailWarning className="size-3" />
                Unverified
              </Badge>
            ))}
        </div>

        {email && !verified && (
          <div className="mt-5 flex flex-col gap-4 rounded-lg border bg-muted/30 p-5">
            <Alert variant="warn">
              <TriangleAlert />
              <AlertTitle>Verify your email to be attributable</AlertTitle>
              <AlertDescription>
                Until it's verified, an unrecognized (possibly squatted) email is not trusted: your signed
                sessions show as unregistered rather than verified-as {me?.username}.
              </AlertDescription>
            </Alert>

            <div className="flex flex-wrap items-center gap-3">
              <Button variant="outline" size="sm" onClick={resend} disabled={busy !== null}>
                <Send />
                {busy === "resend" ? "Sending…" : "Resend verification link"}
              </Button>
              <p className="text-xs text-muted-foreground">
                The link is delivered by your hub operator (hermetic hubs don't send mail directly).
              </p>
            </div>

            <div className="flex flex-col gap-2">
              <Label htmlFor="verify-token">Paste the token or link from your email</Label>
              <div className="flex flex-wrap items-center gap-2">
                <Input
                  id="verify-token"
                  value={pasted}
                  onChange={(e) => setPasted(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && verify()}
                  placeholder="evt_… or https://…/verify-email?token=evt_…"
                  autoComplete="off"
                  className="min-w-[18rem] flex-1 font-mono"
                />
                <Button onClick={verify} disabled={busy !== null || !pasted.trim()}>
                  <MailCheck />
                  {busy === "verify" ? "Verifying…" : "Verify"}
                </Button>
              </div>
            </div>

            {notice && <p className="text-sm text-muted-foreground">{notice}</p>}
            {error && (
              <p role="alert" className="text-sm text-destructive">
                {error}
              </p>
            )}
          </div>
        )}
      </div>
    </section>
  )
}

// The card drives its own state off the enroll/confirm/disable flow, because the hub exposes no
// "is 2FA on?" read (GET /api/me is only {username, is_admin}). "unknown" is the honest opening
// state: pressing Set up either returns a QR (it was off) or 409 (it was on), which resolves it.
type Phase =
  | { kind: "unknown" }
  | { kind: "enrolling"; secret: string; uri: string }
  | { kind: "enabled" }

function TwoFactorCard() {
  const [phase, setPhase] = useState<Phase>({ kind: "unknown" })
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")
  const [backupCodes, setBackupCodes] = useState<string[] | null>(null)
  const [disableOpen, setDisableOpen] = useState(false)

  async function startEnroll() {
    setBusy(true)
    setError("")
    try {
      const r = await api.enroll2fa()
      setPhase({ kind: "enrolling", secret: r.secret, uri: r.otpauth_uri })
    } catch (err) {
      // 409 = 2FA is already active. That's not a failure — it tells us the real state.
      if (err instanceof ApiError && err.status === 409) {
        setPhase({ kind: "enabled" })
      } else {
        setError(String((err as Error)?.message ?? err))
      }
    } finally {
      setBusy(false)
    }
  }

  return (
    <section>
      <Eyebrow className="mb-1.5">two-factor authentication</Eyebrow>
      <div className="rounded-lg border bg-card p-5">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <ShieldCheck className="mt-0.5 size-5 shrink-0 text-primary" />
            <div>
              <p className="font-semibold">Authenticator app (TOTP)</p>
              <p className="mt-0.5 max-w-[52ch] text-sm text-muted-foreground">
                Add a second step to every sign-in: your password, then a 6-digit code from an
                authenticator app. You'll also get one-time backup codes for when you don't have the app.
              </p>
            </div>
          </div>
          {phase.kind === "enabled" && (
            <Badge variant="default" className="gap-1">
              <ShieldCheck className="size-3" />
              on
            </Badge>
          )}
        </div>

        <div className="mt-5">
          {phase.kind === "unknown" && (
            <Button onClick={startEnroll} disabled={busy}>
              <ShieldPlus />
              {busy ? "Starting…" : "Set up two-factor authentication"}
            </Button>
          )}

          {phase.kind === "enrolling" && (
            <EnrollSteps
              secret={phase.secret}
              uri={phase.uri}
              onConfirmed={(codes) => {
                setBackupCodes(codes)
                setPhase({ kind: "enabled" })
              }}
              onCancel={() => setPhase({ kind: "unknown" })}
            />
          )}

          {phase.kind === "enabled" && (
            <div className="flex flex-wrap items-center gap-3">
              <p className="text-sm text-muted-foreground">
                Two-factor authentication is on for your account.
              </p>
              <Button variant="outline" size="sm" onClick={() => setDisableOpen(true)}>
                <ShieldOff />
                Disable
              </Button>
            </div>
          )}
        </div>

        {error && (
          <p role="alert" className="mt-3 text-sm text-destructive">
            {error}
          </p>
        )}
      </div>

      {/* Backup codes are shown exactly once, right after confirm. */}
      <BackupCodesDialog codes={backupCodes} onClose={() => setBackupCodes(null)} />

      <DisableDialog
        open={disableOpen}
        onOpenChange={setDisableOpen}
        onDisabled={() => {
          setDisableOpen(false)
          setPhase({ kind: "unknown" })
        }}
      />
    </section>
  )
}

function Step({ n, title, children }: { n: number; title: string; children: ReactNode }) {
  return (
    <div className="flex gap-3">
      <span className="flex size-6 shrink-0 items-center justify-center rounded-full bg-muted font-mono text-xs font-semibold text-muted-foreground">
        {n}
      </span>
      <div className="min-w-0 flex-1">
        <p className="mb-2 text-sm font-medium">{title}</p>
        {children}
      </div>
    </div>
  )
}

function EnrollSteps({
  secret,
  uri,
  onConfirmed,
  onCancel,
}: {
  secret: string
  uri: string
  onConfirmed: (codes: string[]) => void
  onCancel: () => void
}) {
  const [code, setCode] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function confirm(submitted = code) {
    if (submitted.length !== 6 || busy) return
    setBusy(true)
    setError("")
    try {
      const r = await api.confirm2fa(submitted)
      onConfirmed(r.backup_codes)
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
      setCode("")
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex flex-col gap-6 rounded-lg border bg-muted/30 p-5">
      <Step n={1} title="Scan the QR code with your authenticator app">
        <div className="flex flex-wrap items-center gap-4">
          {/* QR needs a light quiet-zone to scan, in either theme — keep the white plate. */}
          <div className="rounded-lg bg-white p-3">
            <QRCodeSVG value={uri} size={148} level="M" />
          </div>
          <div className="min-w-0">
            <p className="mb-1.5 text-xs text-muted-foreground">
              Can't scan? Enter this secret key manually:
            </p>
            <div className="flex items-center gap-2">
              <code className="truncate rounded-md bg-card px-2.5 py-1.5 font-mono text-sm">{secret}</code>
              <CopyButton value={secret} size="icon" variant="ghost" label="Copy secret" />
            </div>
          </div>
        </div>
      </Step>

      <Step n={2} title="Enter the 6-digit code it shows">
        <InputOTP
          value={code}
          onChange={setCode}
          onComplete={(c) => void confirm(c)}
          disabled={busy}
          autoFocus
          aria-label="6-digit code from your authenticator app"
        />
        {error && (
          <p role="alert" className="mt-2 text-sm text-destructive">
            {error}
          </p>
        )}
        <div className="mt-4 flex items-center gap-2">
          <Button onClick={() => void confirm()} disabled={busy || code.length !== 6}>
            {busy ? "Verifying…" : "Verify and enable"}
          </Button>
          <Button variant="ghost" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
        </div>
      </Step>
    </div>
  )
}

function BackupCodesDialog({ codes, onClose }: { codes: string[] | null; onClose: () => void }) {
  if (!codes) return null
  const allCodes = codes.join("\n")
  return (
    <Dialog open={!!codes} onOpenChange={(o) => !o && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Save your backup codes</DialogTitle>
          <DialogDescription>
            Two-factor authentication is now on. Each code works once if you lose your authenticator.
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="flex flex-col gap-4">
          <Alert variant="warn">
            <TriangleAlert />
            <AlertTitle>Save these now — they won't be shown again</AlertTitle>
            <AlertDescription>
              The hub stores only a hash of each code. Once you close this dialog they're gone for good.
            </AlertDescription>
          </Alert>
          <div className="grid grid-cols-2 gap-2 rounded-lg border bg-muted/40 p-3">
            {codes.map((c) => (
              <code key={c} className="font-mono text-sm tabular-nums">
                {c}
              </code>
            ))}
          </div>
        </DialogBody>
        <DialogFooter>
          <CopyButton value={allCodes} label="Copy all codes" variant="outline" />
          <Button onClick={onClose}>I've saved them</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function DisableDialog({
  open,
  onOpenChange,
  onDisabled,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onDisabled: () => void
}) {
  const [proof, setProof] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function submit() {
    if (!proof.trim() || busy) return
    setBusy(true)
    setError("")
    try {
      await api.disable2fa(proof.trim())
      setProof("")
      onDisabled()
    } catch (err) {
      setError(String((err as Error)?.message ?? err))
    } finally {
      setBusy(false)
    }
  }

  if (!open) return null
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Disable two-factor authentication</DialogTitle>
          <DialogDescription>
            Confirm it's you. A current authenticator code, an unused backup code, or your account
            password all work.
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="flex flex-col gap-2">
          <Label htmlFor="disable-proof">code or password</Label>
          <Input
            id="disable-proof"
            value={proof}
            onChange={(e) => setProof(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
            placeholder="123456 or a backup code or your password"
            autoComplete="off"
            autoFocus
          />
          {error && (
            <p role="alert" className="mt-1 text-sm text-destructive">
              {error}
            </p>
          )}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button
            variant="default"
            className="bg-destructive text-white hover:bg-destructive/90"
            onClick={submit}
            disabled={busy || !proof.trim()}
          >
            <KeyRound />
            {busy ? "Disabling…" : "Disable"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
