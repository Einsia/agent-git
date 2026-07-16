import { useEffect, useState, type ReactNode } from "react"
import { Link, NavLink, useLocation, useNavigate } from "react-router-dom"
import { LogOut, Moon, Sun } from "lucide-react"

import { Button } from "@/components/ui/button"
import { useSession } from "@/lib/session"
import { cn } from "@/lib/utils"

function useTheme() {
  const [dark, setDark] = useState(() => {
    const saved = localStorage.getItem("agit-theme")
    if (saved) return saved === "dark"
    return document.documentElement.classList.contains("dark")
  })
  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark)
    localStorage.setItem("agit-theme", dark ? "dark" : "light")
  }, [dark])
  return { dark, toggle: () => setDark((d) => !d) }
}

// Nav that only exists for a signed-in caller. The site-wide audit log is admin-only on the
// server (403 otherwise), so only show it to an admin rather than offer a link into a wall.
function Nav({ isAdmin }: { isAdmin: boolean }) {
  return (
    <nav className="flex items-center gap-1">
      <Tab to="/tokens">tokens</Tab>
      {isAdmin && <Tab to="/audit">audit</Tab>}
    </nav>
  )
}

function Tab({ to, children }: { to: string; children: ReactNode }) {
  return (
    <NavLink
      to={to}
      className={({ isActive }) =>
        cn(
          "eyebrow rounded-md px-2 py-1 transition-colors hover:text-foreground",
          isActive && "bg-accent/60 text-foreground"
        )
      }
    >
      {children}
    </NavLink>
  )
}

export function Layout({ children }: { children: ReactNode }) {
  const { dark, toggle } = useTheme()
  const { me, loading, logout } = useSession()
  const nav = useNavigate()
  const loc = useLocation()

  async function signOut() {
    await logout()
    nav("/", { replace: true })
  }

  return (
    <div className="min-h-screen">
      <header className="sticky top-0 z-10 border-b bg-card/85 backdrop-blur-md">
        <div className="mx-auto flex max-w-5xl items-center gap-3 px-5 py-2.5">
          <Link to="/" className="flex items-baseline gap-1 text-[1.05rem] font-bold tracking-tight">
            <span className="text-primary">◆</span>
            <span className="font-mono">agit</span>
            <span className="font-mono text-muted-foreground">·hub</span>
          </Link>
          <span className="eyebrow hidden sm:inline">agent session registry</span>

          <div className="ml-auto flex items-center gap-2">
            {me && <Nav isAdmin={me.is_admin} />}

            {/* Don't flash "sign in" before /api/me answers — it reads as being signed out. */}
            {!loading &&
              (me ? (
                <>
                  <span className="hidden items-baseline gap-1.5 font-mono text-[0.72rem] sm:flex">
                    <span className="text-foreground/80">{me.username}</span>
                    {me.is_admin && <span className="eyebrow">admin</span>}
                  </span>
                  <Button variant="ghost" size="icon" onClick={signOut} aria-label="Sign out" title="Sign out">
                    <LogOut />
                  </Button>
                </>
              ) : (
                loc.pathname !== "/login" && (
                  <Link
                    to={`/login?next=${encodeURIComponent(loc.pathname + loc.search)}`}
                    className="eyebrow rounded-md px-2 py-1 hover:text-foreground"
                  >
                    sign in
                  </Link>
                )
              ))}

            <Button variant="ghost" size="icon" onClick={toggle} aria-label="Toggle theme">
              {dark ? <Sun /> : <Moon />}
            </Button>
          </div>
        </div>
      </header>
      <main className="mx-auto max-w-5xl px-5 pb-24 pt-7">{children}</main>
    </div>
  )
}
