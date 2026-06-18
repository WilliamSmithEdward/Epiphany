import { useMemo, useState, type DragEvent } from 'react'
import type { DimensionDto } from '../api/client'
import MemberTable from './MemberTable'

/**
 * A two-pane member set builder. The left pane is a scalable member table
 * (search, attribute columns, flat/hierarchy, virtualization; ADR-0032) with a
 * checkbox per row plus relationship presets (roots / leaves / all); selected
 * members are sent to the right pane as an addition or a replacement. The right
 * pane is the ordered, included set: sortable, reorderable (drag or keyboard),
 * and trimmable. The ordered member list is the value, so a set keeps the order
 * the user arranges here.
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
  const [leftSelected, setLeftSelected] = useState<Set<string>>(() => new Set())
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

  // Parent -> children and child -> parents, for the relationship operators.
  const { childrenOf, parentsOf } = useMemo(() => {
    const childrenOf = new Map<string, string[]>()
    const parentsOf = new Map<string, string[]>()
    for (const e of dimension.edges) {
      const kids = childrenOf.get(e.parent) ?? []
      kids.push(e.child)
      childrenOf.set(e.parent, kids)
      const parents = parentsOf.get(e.child) ?? []
      parents.push(e.parent)
      parentsOf.set(e.child, parents)
    }
    return { childrenOf, parentsOf }
  }, [dimension])
  const hasHierarchy = dimension.edges.length > 0

  // Replace the left selection with a relationship over the current selection.
  // These mirror the standard OLAP set operators: direct children, all
  // descendants, parents (ancestors), siblings (same parent), and the leaf
  // descendants of the selection.
  type Relation = 'children' | 'descendants' | 'parents' | 'ancestors' | 'siblings' | 'leaves'
  const relate = (op: Relation) => {
    const base = [...leftSelected]
    if (base.length === 0) return
    const out = new Set<string>()
    if (op === 'children') {
      base.forEach((n) => (childrenOf.get(n) ?? []).forEach((c) => out.add(c)))
    } else if (op === 'parents') {
      base.forEach((n) => (parentsOf.get(n) ?? []).forEach((p) => out.add(p)))
    } else if (op === 'siblings') {
      base.forEach((n) =>
        (parentsOf.get(n) ?? []).forEach((p) =>
          (childrenOf.get(p) ?? []).forEach((s) => out.add(s)),
        ),
      )
    } else if (op === 'descendants' || op === 'leaves') {
      const seen = new Set<string>()
      const stack = [...base]
      while (stack.length) {
        const n = stack.pop() as string
        const kids = childrenOf.get(n) ?? []
        if (op === 'leaves' && kids.length === 0) out.add(n)
        for (const c of kids) {
          if (op === 'descendants') out.add(c)
          if (!seen.has(c)) {
            seen.add(c)
            stack.push(c)
          }
        }
      }
    } else {
      // ancestors: walk parents transitively
      const seen = new Set<string>()
      const stack = [...base]
      while (stack.length) {
        const n = stack.pop() as string
        for (const p of parentsOf.get(n) ?? []) {
          out.add(p)
          if (!seen.has(p)) {
            seen.add(p)
            stack.push(p)
          }
        }
      }
    }
    setLeftSelected(out)
  }

  const includedSet = useMemo(() => new Set(value), [value])

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

  return (
    <div className="set-picker">
      <section className="set-picker__pane" aria-label="Available members">
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
          <span className="set-picker__selcount muted">{leftSelected.size} selected</span>
        </div>
        {hasHierarchy ? (
          <div className="set-picker__presets" role="group" aria-label="Relate to selection">
            <span className="set-picker__relabel muted">Relate:</span>
            <button type="button" disabled={leftSelected.size === 0} onClick={() => relate('children')} title="Replace with the direct children of the selection">
              Children
            </button>
            <button type="button" disabled={leftSelected.size === 0} onClick={() => relate('descendants')} title="Replace with all descendants of the selection">
              Descendants
            </button>
            <button type="button" disabled={leftSelected.size === 0} onClick={() => relate('parents')} title="Replace with the direct parents of the selection">
              Parents
            </button>
            <button type="button" disabled={leftSelected.size === 0} onClick={() => relate('ancestors')} title="Replace with all ancestors of the selection">
              Ancestors
            </button>
            <button type="button" disabled={leftSelected.size === 0} onClick={() => relate('siblings')} title="Replace with members sharing a parent with the selection">
              Siblings
            </button>
            <button type="button" disabled={leftSelected.size === 0} onClick={() => relate('leaves')} title="Replace with the leaf descendants of the selection">
              Leaves of
            </button>
          </div>
        ) : null}
        <MemberTable
          dimension={dimension}
          selectable
          selected={leftSelected}
          onSelectedChange={setLeftSelected}
        />
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
            A-Z
          </button>
          <button type="button" onClick={() => sortBy('za')}>
            Z-A
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
