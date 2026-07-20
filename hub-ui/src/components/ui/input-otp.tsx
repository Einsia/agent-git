import * as React from "react"

import { cn } from "@/lib/utils"

// A segmented one-time-code input: `length` single-character cells that together hold one numeric
// code (6 digits for a TOTP). No radix / input-otp dependency — hand-written like the other
// primitives. It stays accessible and forgiving:
//   • type a digit → advances; Backspace on an empty cell → steps back and clears the previous;
//   • ← / → move between cells; a paste of the whole code fills every cell at once;
//   • each cell is a real <input inputMode="numeric"> with an aria-label, inside a group labelled
//     for assistive tech by `aria-label`.
// `onComplete` fires when every cell is filled — handy for auto-submitting the confirm step.
function InputOTP({
  value,
  onChange,
  length = 6,
  disabled,
  autoFocus,
  onComplete,
  "aria-label": ariaLabel = "One-time code",
  className,
}: {
  value: string
  onChange: (next: string) => void
  length?: number
  disabled?: boolean
  autoFocus?: boolean
  onComplete?: (code: string) => void
  "aria-label"?: string
  className?: string
}) {
  const refs = React.useRef<Array<HTMLInputElement | null>>([])
  const chars = React.useMemo(() => {
    const out: string[] = []
    for (let i = 0; i < length; i++) out.push(value[i] ?? "")
    return out
  }, [value, length])

  function set(next: string) {
    const cleaned = next.replace(/\D/g, "").slice(0, length)
    onChange(cleaned)
    if (cleaned.length === length) onComplete?.(cleaned)
  }

  function onCellChange(i: number, raw: string) {
    const digit = raw.replace(/\D/g, "")
    if (!digit) return
    // Take the last typed character (handles the case where the cell already held one).
    const d = digit[digit.length - 1]
    const arr = value.split("")
    arr[i] = d
    const joined = arr.join("").slice(0, length)
    set(joined)
    if (i < length - 1) refs.current[i + 1]?.focus()
  }

  function onKeyDown(i: number, e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Backspace") {
      e.preventDefault()
      const arr = value.split("")
      if (arr[i]) {
        arr[i] = ""
        onChange(arr.join(""))
      } else if (i > 0) {
        arr[i - 1] = ""
        onChange(arr.join(""))
        refs.current[i - 1]?.focus()
      }
    } else if (e.key === "ArrowLeft" && i > 0) {
      e.preventDefault()
      refs.current[i - 1]?.focus()
    } else if (e.key === "ArrowRight" && i < length - 1) {
      e.preventDefault()
      refs.current[i + 1]?.focus()
    }
  }

  function onPaste(e: React.ClipboardEvent<HTMLInputElement>) {
    e.preventDefault()
    const text = e.clipboardData.getData("text")
    set(text)
    const filled = text.replace(/\D/g, "").slice(0, length).length
    refs.current[Math.min(filled, length - 1)]?.focus()
  }

  return (
    <div role="group" aria-label={ariaLabel} className={cn("flex items-center gap-2", className)}>
      {chars.map((c, i) => (
        <input
          key={i}
          ref={(el) => {
            refs.current[i] = el
          }}
          value={c}
          onChange={(e) => onCellChange(i, e.target.value)}
          onKeyDown={(e) => onKeyDown(i, e)}
          onPaste={onPaste}
          onFocus={(e) => e.target.select()}
          inputMode="numeric"
          autoComplete={i === 0 ? "one-time-code" : "off"}
          pattern="[0-9]*"
          maxLength={1}
          disabled={disabled}
          autoFocus={autoFocus && i === 0}
          aria-label={`Digit ${i + 1} of ${length}`}
          className={cn(
            "h-11 w-10 rounded-md border bg-transparent text-center font-mono text-lg shadow-sm transition-colors",
            "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
            "disabled:cursor-not-allowed disabled:opacity-50"
          )}
        />
      ))}
    </div>
  )
}

export { InputOTP }
