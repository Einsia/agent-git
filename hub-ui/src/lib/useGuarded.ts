import { useEffect } from "react"
import { useLocation, useNavigate } from "react-router-dom"

import { useAsync } from "@/lib/useAsync"

// Permission routing layered on useAsync:
//   401 → send them to sign in, carrying `next` so they land back here;
//   403 → let the page render "you don't have access" — do NOT redirect. Bouncing an
//         already-signed-in person to the login form dresses "you aren't authorized" up as
//         "you aren't signed in", and they'd sign in again to the same wall.
export function useGuarded<T>(fn: () => Promise<T>, deps: unknown[]) {
  const state = useAsync(fn, deps)
  const nav = useNavigate()
  const loc = useLocation()

  useEffect(() => {
    if (state.status === 401) {
      nav(`/login?next=${encodeURIComponent(loc.pathname + loc.search)}`, { replace: true })
    }
  }, [state.status, nav, loc.pathname, loc.search])

  return { ...state, forbidden: state.status === 403, unauthorized: state.status === 401 }
}
