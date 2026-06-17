import { useMemo, useState, type DragEvent } from 'react'
import type { DimensionDto } from '../api/client'
import { buildElementTree } from '../model/tree'
import ElementTree from './ElementTree'

/**
 * A two-pane member set builder. The left pane lists every element of the
 * dimension as a searchable, expandable hierarchy with selection helpers
 * (roots / leaves / all); selected elements are sent to the right pane as an
 * addition or a replacement. The right pane is the ordered, included set: it can
 * be sorted, reordered (drag or keyboard), and trimmed. The ordered member list
 * is the value, so a set keeps the order the user arranges here.
 */
export default function MemberSetPicker({
  dimension,
  value,
  onChange,
}: {
  dimension: DimensionDto
  value: string[]
  onChange: (members: string[]) => void
}) {
  const tree = useMemo(() => buildElementTree(dimension), [dimension])
  const [leftSelected, setLeftSelected] = useState<Set<string>>(() => new Set())
  const [search, setSearch] = useState('')
  const [dragIndex, setDragIndex] = useState<number | null>(null)

  // Dimension order: for stable add ordering and the "model order" sort.
  const order = useMemo(
    () => new Map(dimension.elements.map((e, i) => [e.name, i] as const)),
    [dimension],
  )

  // Roots (no parent) and leaves (no children), derived from the edges.
  const { roots, leaves, all } = useMemo(() => {
    const hasParent = new Set<string>()
    const hasChild = new Set<string>()
    for (const e of dimension.edges) {
      hasParent.add(e.child)
      hasChild.add(e.parent)
    }
    const all = dimension.elements.map((e) => e.name)
    return {
      all,
      roots: all.filter((n) => !hasParent.has(n)),
      leaves: all.filter((n) => !hasChild.has(n)),
    }
  }, [dimension])

  const includedSet = useMemo(() => new Set(value), [value])

  const toggleLeft = (name: string) =>
    setLeftSelected((s) => {
      const n = new Set(s)
      if (n.has(name)) n.delete(name)
      else n.add(name)
      return n
    })

  const transfer = (replace: boolean) => {
    const picked = [...leftSelected].sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0))
    if (replace) {
      onChange(picked)
      return
    }
    const next = [...value]
    for (const n of picked) if (!includedSet.has(n)) next.push(n)
    onChange(next)
  }

  const remove = (name: string) => onChange(value.filter((n) => n !== name))
  const moveBy = (i: number, dir: -1 | 1) => {
    const j = i + dir
    if (j < 0 || j >= value.length) return
    const next = [...value]
    ;[next[i], next[j]] = [next[j], next[i]]
    onChange(next)
  }
  const sortBy = (mode: 'model' | 'az' | 'za') => {
    const next = [...value]
    if (mode === 'az') next.sort((a, b) => a.localeCompare(b))
    else if (mode === 'za') next.sort((a, b) => b.localeCompare(a))
    else next.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0))
    onChange(next)
  }
  const onDropAt = (i: number) => {
    if (dragIndex === null || dragIndex === i) return
    const next = [...value]
    const [moved] = next.splice(dragIndex, 1)
    next.splice(i, 0, moved)
    onChange(next)
    setDragIndex(null)
  }

  const q = search.trim().toLowerCase()
  const matches = q ? all.filter((n) => n.toLowerCase().includes(q)) : []

  return (
    <div className="set-picker">
      <section className="set-picker__pane" aria-label="Available members">
        <header className="set-picker__head">
          <span>Available</span>
          <span className="muted">{all.length}</span>
        </header>
        <input
          type="search"
          className="set-picker__search"
          value={search}
          placeholder="Search members"
          aria-label="Search available members"
          onChange={(e) => setSearch(e.target.value)}
        />
        <div className="set-picker__presets" role="group" aria-label="Select">
          <button type="button" onClick={() => setLeftSelected(new Set(roots))}>
            Roots
          </button>
          <button type="button" onClick={() => setLeftSelected(new Set(leaves))}>
            Leaves
          </button>
          <button type="button" onClick={() => setLeftSelected(new Set(all))}>
            All
          </button>
          {leftSelected.size > 0 ? (
            <button type="button" onClick={() => setLeftSelected(new Set())}>
              Clear
            </button>
          ) : null}
        </div>
        <div className="set-picker__list">
          {q ? (
            matches.length === 0 ? (
              <p className="muted">No members match &ldquo;{search.trim()}&rdquo;</p>
            ) : (
              <ul className="set-picker__flat">
                {matches.map((n) => (
                  <li key={n}>
                    <label>
                      <input
                        type="checkbox"
                        checked={leftSelected.has(n)}
                        onChange={() => toggleLeft(n)}
                      />
                      {n}
                    </label>
                  </li>
                ))}
              </ul>
            )
          ) : (
            <ElementTree nodes={tree} selected={leftSelected} onToggle={toggleLeft} />
          )}
        </div>
        <p className="muted">{leftSelected.size} selected</p>
      </section>

      <div className="set-picker__controls" role="group" aria-label="Transfer">
        <button
          type="button"
          className="primary"
          disabled={leftSelected.size === 0}
          onClick={() => transfer(false)}
          title="Add the selected members to the set"
        >
          Add &rarr;
        </button>
        <button
          type="button"
          disabled={leftSelected.size === 0}
          onClick={() => transfer(true)}
          title="Replace the set with the selected members"
        >
          Replace &rarr;
        </button>
      </div>

      <section className="set-picker__pane" aria-label="Included members">
        <header className="set-picker__head">
          <span>Included</span>
          <span className="muted">{value.length}</span>
        </header>
        <div className="set-picker__sortbar" role="group" aria-label="Sort included">
          <button type="button" onClick={() => sortBy('model')} title="Sort by the dimension's own order">
            Model order
          </button>
          <button type="button" onClick={() => sortBy('az')}>
            A&ndash;Z
          </button>
          <button type="button" onClick={() => sortBy('za')}>
            Z&ndash;A
          </button>
          {value.length > 0 ? (
            <button type="button" onClick={() => onChange([])}>
              Clear all
            </button>
          ) : null}
        </div>
        <div className="set-picker__list">
          {value.length === 0 ? (
            <p className="muted">No members yet. Select on the left, then Add.</p>
          ) : (
            <ul className="set-picker__ordered">
              {value.map((n, i) => (
                <li
                  key={n}
                  draggable
                  onDragStart={() => setDragIndex(i)}
                  onDragOver={(e: DragEvent) => e.preventDefault()}
                  onDrop={() => onDropAt(i)}
                  onDragEnd={() => setDragIndex(null)}
                >
                  <span className="set-picker__handle" aria-hidden="true">
                    &#x2837;
                  </span>
                  <span className="set-picker__member">{n}</span>
                  <span className="set-picker__rowactions">
                    <button type="button" aria-label={`Move ${n} up`} disabled={i === 0} onClick={() => moveBy(i, -1)}>
                      &uarr;
                    </button>
                    <button
                      type="button"
                      aria-label={`Move ${n} down`}
                      disabled={i === value.length - 1}
                      onClick={() => moveBy(i, 1)}
                    >
                      &darr;
                    </button>
                    <button type="button" aria-label={`Remove ${n}`} onClick={() => remove(n)}>
                      &times;
                    </button>
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  )
}
