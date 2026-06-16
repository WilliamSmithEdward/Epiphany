import { useLayoutEffect, useRef, useState } from 'react'
import { highlight, type CodeLanguage } from './highlight'

/** Shared monospace metrics (kept in lockstep with the CSS in components.css so
 * the highlight overlay and the error strip align exactly with the textarea). */
const LINE_HEIGHT = 20
const PAD = 10

/**
 * A dependency-free code editor (ADR-0026): a syntax-highlighted, aria-hidden
 * overlay rendered directly under a transparent-text textarea. The textarea is
 * the source of truth and the accessible control; the overlay only colors
 * tokens. An optional `errorLine` paints a strip behind that line. Highlighting
 * is best-effort and cosmetic - a tokenizer quirk can only mis-color, never
 * change what is typed or saved.
 */
export function CodeEditor({
  value,
  onChange,
  language,
  placeholder,
  rows = 12,
  errorLine,
  ariaLabel,
}: {
  value: string
  onChange: (next: string) => void
  language: CodeLanguage
  placeholder?: string
  rows?: number
  errorLine?: number | null
  ariaLabel?: string
}) {
  const taRef = useRef<HTMLTextAreaElement>(null)
  const preRef = useRef<HTMLPreElement>(null)
  const [scroll, setScroll] = useState({ top: 0, left: 0 })

  // Keep the highlight overlay scrolled with the textarea.
  useLayoutEffect(() => {
    if (preRef.current) {
      preRef.current.scrollTop = scroll.top
      preRef.current.scrollLeft = scroll.left
    }
  }, [scroll])

  const html = highlight(value, language)
  // Strip top, offset by the current scroll so it tracks while scrolling.
  const errorTop =
    errorLine && errorLine > 0 ? PAD + (errorLine - 1) * LINE_HEIGHT - scroll.top : null

  return (
    <div className="code-editor" style={{ height: rows * LINE_HEIGHT + PAD * 2 }}>
      {errorTop !== null ? (
        <div className="code-editor__errline" style={{ top: errorTop, height: LINE_HEIGHT }} />
      ) : null}
      <pre className="code-editor__hl" aria-hidden="true" ref={preRef}>
        {/* A trailing newline keeps the last line's height when value ends in \n. */}
        <code dangerouslySetInnerHTML={{ __html: `${html}\n` }} />
      </pre>
      <textarea
        ref={taRef}
        className="code-editor__ta"
        value={value}
        spellCheck={false}
        wrap="off"
        aria-label={ariaLabel}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        onScroll={(e) =>
          setScroll({ top: e.currentTarget.scrollTop, left: e.currentTarget.scrollLeft })
        }
      />
    </div>
  )
}
