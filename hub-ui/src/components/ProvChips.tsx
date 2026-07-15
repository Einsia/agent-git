import { GitBranch, Clock, User, Cpu } from "lucide-react"

import { Badge } from "@/components/ui/badge"

interface Prov {
  runtime: string
  model?: string
  branch?: string
  author?: string
  when?: string
}

// The trust-at-a-glance row: which runtime/model produced this, on what branch,
// by whom, how long ago. This is what a reviewer reads before pulling a session in.
export function ProvChips({ runtime, model, branch, author, when }: Prov) {
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <Badge>{runtime}</Badge>
      {model && (
        <Badge variant="muted">
          <Cpu className="size-3" /> {model}
        </Badge>
      )}
      {branch && (
        <Badge variant="muted">
          <GitBranch className="size-3" /> {branch}
        </Badge>
      )}
      {author && (
        <Badge variant="muted">
          <User className="size-3" /> {author}
        </Badge>
      )}
      {when && (
        <Badge variant="muted">
          <Clock className="size-3" /> {when}
        </Badge>
      )}
    </div>
  )
}
