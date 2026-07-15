import { useEffect, useState, type ReactNode } from "react"
import { Link } from "react-router-dom"
import { Moon, Sun } from "lucide-react"

import { Button } from "@/components/ui/button"

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

export function Layout({ children }: { children: ReactNode }) {
  const { dark, toggle } = useTheme()
  return (
    <div className="min-h-screen">
      <header className="sticky top-0 z-10 border-b bg-card/80 backdrop-blur-md">
        <div className="mx-auto flex max-w-5xl items-baseline gap-4 px-5 py-3">
          <Link to="/" className="text-[1.05rem] font-bold tracking-tight">
            <span className="text-primary">◆</span> agit
            <span className="text-muted-foreground">·hub</span>
          </Link>
          <span className="font-mono text-[0.68rem] uppercase tracking-[0.14em] text-muted-foreground">
            agent session registry
          </span>
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto self-center"
            onClick={toggle}
            aria-label="切换主题"
          >
            {dark ? <Sun /> : <Moon />}
          </Button>
        </div>
      </header>
      <main className="mx-auto max-w-5xl px-5 pb-20 pt-6">{children}</main>
    </div>
  )
}
