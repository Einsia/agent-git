import { useCallback, useEffect, useState } from "react"

import { ApiError } from "@/lib/api"

export interface AsyncState<T> {
  data: T | null
  error: string | null
  loading: boolean
  /// The HTTP status of a failed request, when there was one. Callers key their permission
  /// states off this (401 = sign in, 403 = refused), so it has to survive the throw.
  status: number | null
  reload: () => void
}

// Minimal data-fetching hook. Re-runs when any dep changes, or on reload().
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]): AsyncState<T> {
  const [state, setState] = useState<Omit<AsyncState<T>, "reload">>({
    data: null,
    error: null,
    loading: true,
    status: null,
  })
  const [nonce, setNonce] = useState(0)
  const reload = useCallback(() => setNonce((n) => n + 1), [])

  useEffect(() => {
    let alive = true
    setState((s) => ({ ...s, loading: true, error: null, status: null }))
    fn()
      .then((data) => alive && setState({ data, error: null, loading: false, status: null }))
      .catch(
        (e) =>
          alive &&
          setState({
            data: null,
            error: String(e?.message ?? e),
            loading: false,
            status: e instanceof ApiError ? e.status : null,
          })
      )
    return () => {
      alive = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, nonce])

  return { ...state, reload }
}
