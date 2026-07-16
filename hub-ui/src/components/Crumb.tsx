import { Link } from "react-router-dom"

export function Crumb({ name, session }: { name: string; session?: string }) {
  return (
    <nav className="mb-4 font-mono text-[0.78rem] text-muted-foreground">
      <Link to="/" className="hover:text-foreground">
        hub
      </Link>
      <span className="mx-1.5 text-muted-foreground/50">/</span>
      <Link to={`/agent/${name}`} className="hover:text-foreground">
        {name}
      </Link>
      {session && (
        <>
          <span className="mx-1.5 text-muted-foreground/50">/</span>
          <span className="text-foreground/80">{session}</span>
        </>
      )}
    </nav>
  )
}
