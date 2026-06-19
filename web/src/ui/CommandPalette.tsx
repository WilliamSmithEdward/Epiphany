import * as RD from '@radix-ui/react-dialog'
import { useEffect, useMemo, useRef, useState } from 'react'

export interface Command {
  id: string
  label: string
  /** Group label shown on the right (e.g. "Cube", "Go to", "Action"). */
  group: string
  /** Optional keywords to widen matching. */
  keywords?: string
  run: () => void
}

/** A fuzzy command/navigation palette (ADR-0020): the expert fast-path. */
export function CommandPalette({
  open,
  onOpenChange,
  commands,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  commands: Command[]
}) {
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)

  const results = useMemo(() => {
    const needle = query.trim().toLowerCase()
    if (!needle) return commands
    return commands.filter((c) =>
      `${c.label} ${c.group} ${c.keywords ?? ''}`.toLowerCase().includes(needle),
    )
  }, [query, commands])

  useEffect(() => {
    if (open) setQuery('')
  }, [open])
  useEffect(() => {
    setActive(0)
  }, [query])

  function choose(index: number) {
    const cmd = results[index]
    if (cmd) {
      onOpenChange(false)
      cmd.run()
    }
  }

  return (
    <RD.Root open={open} onOpenChange={onOpenChange}>
      <RD.Portal>
        <RD.Overlay className="dialog__overlay" />
        <RD.Content
          className="cmdk"
          aria-label="Command palette"
          onOpenAutoFocus={(e) => {
            e.preventDefault()
            inputRef.current?.focus()
          }}
        >
          <RD.Title className="sr-only">Command palette</RD.Title>
          <input
            ref={inputRef}
            className="cmdk__input"
            aria-label="Search cubes, sections, and actions"
            placeholder="Search cubes, sections, actions..."
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'ArrowDown') {
                e.preventDefault()
                setActive((a) => Math.min(a + 1, results.length - 1))
              } else if (e.key === 'ArrowUp') {
                e.preventDefault()
                setActive((a) => Math.max(a - 1, 0))
              } else if (e.key === 'Enter') {
                e.preventDefault()
                choose(active)
              }
            }}
          />
          <div className="sr-only" role="status" aria-live="polite">
            {results.length === 0
              ? 'No matches'
              : `${results.length} result${results.length === 1 ? '' : 's'}`}
          </div>
          <ul className="cmdk__list" role="listbox">
            {results.length === 0 ? (
              <li className="cmdk__empty">No matches</li>
            ) : (
              results.map((c, i) => (
                <li key={c.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={i === active}
                    className={i === active ? 'cmdk__item is-active' : 'cmdk__item'}
                    onMouseMove={() => setActive(i)}
                    onClick={() => choose(i)}
                  >
                    <span className="cmdk__label">{c.label}</span>
                    <span className="cmdk__group">{c.group}</span>
                  </button>
                </li>
              ))
            )}
          </ul>
        </RD.Content>
      </RD.Portal>
    </RD.Root>
  )
}
