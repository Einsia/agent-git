import { Link } from "react-router-dom"

export function Crumb({ name, session }: { name: string; session?: string }) {
  return (
    <nav className="mb-3 text-sm text-muted-foreground">
      <Link to="/" className="hover:text-foreground">
        AgentGitHub
      </Link>
      <span className="mx-1.5">/</span>
      <Link to={`/agent/${name}`} className="font-mono hover:text-foreground">
        {name}
      </Link>
      {session && (
        <>
          <span className="mx-1.5">/</span>
          <span className="font-mono">{session}</span>
        </>
      )}
    </nav>
  )
}
