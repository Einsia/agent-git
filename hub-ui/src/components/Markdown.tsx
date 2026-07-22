import ReactMarkdown from "react-markdown"
import remarkGfm from "remark-gfm"

// A reusable markdown renderer, scoped to the session conversation view.
//
// SECURITY — transcripts are ATTACKER-AUTHORED. react-markdown is used WITHOUT rehype-raw and without
// dangerouslySetInnerHTML, so any raw HTML embedded in a transcript (a `<script>`, an `<img onerror=…>`)
// is rendered as literal text, never parsed into DOM — it cannot execute. Links go through
// react-markdown's default urlTransform, which strips dangerous protocols (`javascript:`, `vbscript:`,
// unsafe `data:`); we keep that default and additionally open links in a new tab with a hardened `rel`.
// remark-gfm adds GitHub-flavored code fences, tables, and task lists. Typography lives in `.md` in
// index.css; wide code/tables scroll inside their own container so the page body never scrolls sideways.
export function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ node: _node, ...props }) => (
            <a {...props} target="_blank" rel="noreferrer nofollow" />
          ),
          // GFM tables can be wider than the column; give each its own horizontal scroll.
          table: ({ node: _node, ...props }) => (
            <div className="md-scroll">
              <table {...props} />
            </div>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  )
}
