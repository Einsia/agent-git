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
      <header className="sticky top-0 z-10 border-b bg-card/85 backdrop-blur-md">
        <div className="mx-auto flex max-w-5xl items-center gap-3 px-5 py-2.5">
          <Link to="/" className="flex items-baseline gap-1 text-[1.05rem] font-bold tracking-tight">
            <span className="text-primary">◆</span>
            <span className="font-mono">agit</span>
            <span className="font-mono text-muted-foreground">·hub</span>
          </Link>
          <span className="eyebrow hidden sm:inline">agent session registry</span>
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto"
            onClick={toggle}
            aria-label="Toggle theme"
          >
            {dark ? <Sun /> : <Moon />}
          </Button>
        </div>
      </header>
      <main className="mx-auto max-w-5xl px-5 pb-24 pt-7">{children}</main>
    </div>
  )
}
