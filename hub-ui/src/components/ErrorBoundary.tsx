import { Component, type ErrorInfo, type ReactNode } from "react"

// A render-error firewall for a single transcript block. Transcripts are ATTACKER-AUTHORED, and a
// single malformed markdown segment (a pathological table, a broken fence) must not take down the whole
// session page. When a child throws during render, this catches it and shows the raw text as plain,
// inert content instead. Scoped per block, so one bad block degrades to plain text while every other
// block renders normally.
interface Props {
  // The plain-text fallback shown when the child fails to render.
  fallbackText: string
  children: ReactNode
}

interface State {
  failed: boolean
}

export class BlockErrorBoundary extends Component<Props, State> {
  state: State = { failed: false }

  static getDerivedStateFromError(): State {
    return { failed: true }
  }

  componentDidCatch(_error: Error, _info: ErrorInfo) {
    // Swallow — the fallback UI is the whole response. No console noise in production builds.
  }

  render() {
    if (this.state.failed) {
      return (
        <div>
          <p className="mb-1 text-[0.72rem] text-kind-warn">
            This block could not be rendered; showing raw text.
          </p>
          <pre className="overflow-x-auto whitespace-pre-wrap break-words rounded-md border bg-muted p-3 font-mono text-[0.78rem] leading-relaxed">
            {this.props.fallbackText}
          </pre>
        </div>
      )
    }
    return this.props.children
  }
}
