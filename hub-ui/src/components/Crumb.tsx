import { Link } from "react-router-dom"

export function Crumb({ owner, name, session }: { owner: string; name: string; session?: string }) {
  return (
    <nav className="mb-4 font-mono text-[0.78rem] text-muted-foreground">
      <Link to="/" className="hover:text-foreground">
        hub
      </Link>
      <span className="mx-1.5 text-muted-foreground/50">/</span>
      {/* Identity is owner/name; the whole scoped id is one link back to the agent. */}
      <Link to={`/agent/${owner}/${name}`} className="hover:text-foreground">
        {owner}/{name}
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
