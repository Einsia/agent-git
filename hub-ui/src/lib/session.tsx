import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react"

import { ApiError, api, type Me } from "@/lib/api"

interface SessionState {
  me: Me | null
  loading: boolean
  refresh: () => Promise<void>
  logout: () => Promise<void>
}

const Ctx = createContext<SessionState>({
  me: null,
  loading: true,
  refresh: async () => {},
  logout: async () => {},
})

// A 401 from /api/me is the normal "signed out" state, not an error: public agents read fine
// anonymously. Only a 401 from a *data* endpoint means "this page needs a sign-in" — that's
// where the redirect belongs, see useGuarded.
export function SessionProvider({ children }: { children: ReactNode }) {
  const [me, setMe] = useState<Me | null>(null)
  const [loading, setLoading] = useState(true)

  const refresh = useCallback(async () => {
    try {
      setMe(await api.me())
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) setMe(null)
      else setMe(null)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const logout = useCallback(async () => {
    try {
      await api.logout()
    } finally {
      setMe(null)
    }
  }, [])

  return <Ctx.Provider value={{ me, loading, refresh, logout }}>{children}</Ctx.Provider>
}

export function useSession() {
  return useContext(Ctx)
}
