import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { DimensionDto, ElementKind } from '../api/client'
import { buildElementTree, type TreeNode } from '../model/tree'
import { useVirtualRows } from '../ui/useVirtualRows'

const ROW_H = 33
const VIRTUAL_THRESHOLD = 200

/** Build a substring/wildcard matcher. `*` and `?` are wildcards; every other
 * regex metacharacter is escaped, so a member name containing regex characters
 * matches literally and the pattern can never be catastrophic. */
function matcher(query: string): (s: string) => boolean {
  const q = query.trim()
  if (!q) return () => true
  if (q.includes('*') || q.includes('?')) {
    const pattern = q
      .replace(/[.+^${}()|[\]\\]/g, '\\$&')
      .replace(/\*/g, '.*')
      .replace(/\?/g, '.')
    try {
      const re = new RegExp(pattern, 'i')
      return (s) => re.test(s)
    } catch {
      /* fall through to substring */
    }
  }
  const lower = q.toLowerCase()
  return (s) => s.toLowerCase().includes(lower)
}

const KIND_LABEL: Record<ElementKind, string> = {
  numeric: 'Number',
  string: 'Text',
  consolidated: 'Total',
}

interface Row {
  name: string
  kind: ElementKind
  depth: number
  hasChildren: boolean
  expanded: boolean
  path: string
}

type SortDir = 'asc' | 'desc'

/**
 * A scalable member table (ADR-0032): the members of a dimension as rows, with
 * the member name as a pinned leading column, a kind column, and any toggled-on
 * attribute columns. Search (wildcard + alias-aware), sortable headers, a
 * Flat/Hierarchy toggle, and in-house row virtualization let it handle thousands
 * of members at a constant DOM cost. In `selectable` mode it is the Available
 * pane of the set builder: each row has a checkbox and reports selection changes.
 *
 * v1 uses static table semantics (role=table) because cells are read-only here;
 * the interactive controls (checkboxes, twisties, sort headers) are normal tab
 * stops. Full grid cell-navigation + inline cell editing is deferred (ADR-0032).
 */
export default function MemberTable({
  dimension,
  selectable = false,
  selected,
  onSelectedChange,
}: {
  dimension: DimensionDto
  selectable?: boolean
  selected?: Set<string>
  onSelectedChange?: (next: Set<string>) => void
}) {
  const attributes = useMemo(() => dimension.attributes ?? [], [dimension])
  const aliasAttrs = useMemo(() => attributes.filter((a) => a.kind === 'alias'), [attributes])

  const [search, setSearch] = useState('')
  const [sortKey, setSortKey] = useState<string>('model') // 'model' | 'member' | 'kind' | attr name
  const [sortDir, setSortDir] = useState<SortDir>('asc')
  const [visibleAttrs, setVisibleAttrs] = useState<Set<string>>(() => new Set())
  const [aliasLabel, setAliasLabel] = useState<string | null>(null)
  const [mode, setMode] = useState<'flat' | 'hierarchy'>('flat')
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set())
  const [columnsOpen, setColumnsOpen] = useState(false)
  const lastClicked = useRef<number | null>(null)
  const columnsRef = useRef<HTMLDivElement | null>(null)

  // member -> value, per attribute, for cell lookup and attribute sort.
  const attrValues = useMemo(() => {
    const m = new Map<string, Map<string, string>>()
    for (const a of attributes) {
      const inner = new Map<string, string>()
      for (const v of a.values) inner.set(v.element, v.value)
      m.set(a.name, inner)
    }
    return m
  }, [attributes])

  const order = useMemo(
    () => new Map(dimension.elements.map((e, i) => [e.name, i] as const)),
    [dimension],
  )
  const kindOf = useMemo(
    () => new Map(dimension.elements.map((e) => [e.name, e.kind] as const)),
    [dimension],
  )
  const tree = useMemo(() => buildElementTree(dimension), [dimension])
  const hasHierarchy = useMemo(() => dimension.edges.length > 0, [dimension])

  const labelOf = useCallback(
    (name: string) => (aliasLabel ? attrValues.get(aliasLabel)?.get(name) || name : name),
    [aliasLabel, attrValues],
  )

  const match = useMemo(() => matcher(search), [search])
  const searching = search.trim() !== ''

  // Close the columns popover on an outside click.
  useEffect(() => {
    if (!columnsOpen) return
    const onDown = (e: MouseEvent) => {
      if (columnsRef.current && !columnsRef.current.contains(e.target as Node)) setColumnsOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [columnsOpen])

  // The visible row list. Hierarchy mode (no search) flattens the expanded tree;
  // otherwise a flat, filtered, sorted member list. Searching always flattens.
  const rows: Row[] = useMemo(() => {
    if (mode === 'hierarchy' && !searching) {
      const out: Row[] = []
      const walk = (nodes: TreeNode[], depth: number) => {
        for (const n of nodes) {
          const isOpen = expanded.has(n.path)
          out.push({
            name: n.name,
            kind: n.kind,
            depth,
            hasChildren: n.children.length > 0,
            expanded: isOpen,
            path: n.path,
          })
          if (isOpen && n.children.length) walk(n.children, depth + 1)
        }
      }
      walk(tree, 0)
      return out
    }
    const filtered = dimension.elements.filter((e) => match(e.name) || match(labelOf(e.name)))
    const sorted = [...filtered]
    const cmp = (a: string, b: string) => {
      let r: number
      if (sortKey === 'model') r = (order.get(a) ?? 0) - (order.get(b) ?? 0)
      else if (sortKey === 'kind') r = (kindOf.get(a) ?? '').localeCompare(kindOf.get(b) ?? '')
      else if (sortKey === 'member') r = labelOf(a).localeCompare(labelOf(b))
      else {
        const va = attrValues.get(sortKey)?.get(a) ?? ''
        const vb = attrValues.get(sortKey)?.get(b) ?? ''
        const na = Number(va)
        const nb = Number(vb)
        r = va !== '' && vb !== '' && !Number.isNaN(na) && !Number.isNaN(nb) ? na - nb : va.localeCompare(vb)
      }
      return sortKey !== 'model' && sortDir === 'desc' ? -r : r
    }
    sorted.sort((ea, eb) => cmp(ea.name, eb.name))
    return sorted.map((e) => ({
      name: e.name,
      kind: e.kind,
      depth: 0,
      hasChildren: false,
      expanded: false,
      path: e.name,
    }))
  }, [mode, searching, expanded, tree, dimension, match, labelOf, sortKey, sortDir, order, kindOf, attrValues])

  const virtual = useVirtualRows({
    rowCount: rows.length,
    rowHeight: ROW_H,
    enabled: rows.length > VIRTUAL_THRESHOLD,
  })

  const shownAttrs = attributes.filter((a) => visibleAttrs.has(a.name))
  const cols = [
    selectable ? '2.25rem' : null,
    'minmax(12rem, 1.6fr)', // member
    '5.5rem', // kind
    ...shownAttrs.map(() => 'minmax(7rem, 1fr)'),
  ]
    .filter(Boolean)
    .join(' ')

  const toggleSort = (key: string) => {
    if (sortKey === key) {
      if (sortDir === 'asc') setSortDir('desc')
      else {
        setSortKey('model') // third click clears back to the dimension's own order
        setSortDir('asc')
      }
    } else {
      setSortKey(key)
      setSortDir('asc')
    }
  }
  const ariaSort = (key: string): 'ascending' | 'descending' | 'none' =>
    sortKey === key ? (sortDir === 'asc' ? 'ascending' : 'descending') : 'none'
  const sortMark = (key: string) => (sortKey === key ? (sortDir === 'asc' ? ' ▲' : ' ▼') : '')

  const toggleAttr = (name: string) =>
    setVisibleAttrs((s) => {
      const n = new Set(s)
      if (n.has(name)) n.delete(name)
      else n.add(name)
      return n
    })
  const toggleExpand = (path: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(path)) n.delete(path)
      else n.add(path)
      return n
    })
  const expandAll = () => {
    const all = new Set<string>()
    const walk = (nodes: TreeNode[]) =>
      nodes.forEach((n) => {
        if (n.children.length) {
          all.add(n.path)
          walk(n.children)
        }
      })
    walk(tree)
    setExpanded(all)
  }

  // Selection (set-builder mode): toggle, Shift-range over the current row order.
  const onRowSelect = (index: number, name: string, shift: boolean) => {
    if (!selectable || !onSelectedChange) return
    const next = new Set(selected ?? [])
    if (shift && lastClicked.current !== null) {
      const [lo, hi] = [lastClicked.current, index].sort((a, b) => a - b)
      const turnOn = !next.has(name)
      for (let i = lo; i <= hi; i++) {
        const nm = rows[i]?.name
        if (!nm) continue
        if (turnOn) next.add(nm)
        else next.delete(nm)
      }
    } else {
      if (next.has(name)) next.delete(name)
      else next.add(name)
    }
    lastClicked.current = index
    onSelectedChange(next)
  }
  const allShownSelected =
    selectable && rows.length > 0 && rows.every((r) => selected?.has(r.name))
  const toggleSelectAll = () => {
    if (!onSelectedChange) return
    const next = new Set(selected ?? [])
    if (allShownSelected) rows.forEach((r) => next.delete(r.name))
    else rows.forEach((r) => next.add(r.name))
    onSelectedChange(next)
  }

  const total = dimension.elements.length
  const slice = rows.slice(virtual.start, virtual.end)
  const colCount = (selectable ? 1 : 0) + 2 + shownAttrs.length

  return (
    <div className="mtable">
      <div className="mtable__toolbar">
        <input
          type="search"
          className="mtable__search"
          value={search}
          placeholder="Search members"
          aria-label="Search members"
          onChange={(e) => setSearch(e.target.value)}
        />
        {attributes.length > 0 ? (
          <div className="mtable__cols" ref={columnsRef}>
            <button
              type="button"
              className="mtable__colsbtn"
              aria-haspopup="true"
              aria-expanded={columnsOpen}
              onClick={() => setColumnsOpen((o) => !o)}
            >
              Columns{shownAttrs.length < attributes.length ? ` (${attributes.length - shownAttrs.length} hidden)` : ''}
            </button>
            {columnsOpen ? (
              <div className="mtable__colsmenu" role="group" aria-label="Attribute columns">
                {attributes.map((a) => (
                  <label key={a.name} className="mtable__colsitem">
                    <input
                      type="checkbox"
                      checked={visibleAttrs.has(a.name)}
                      onChange={() => toggleAttr(a.name)}
                    />
                    {a.name}
                  </label>
                ))}
              </div>
            ) : null}
          </div>
        ) : null}
        {aliasAttrs.length > 0 ? (
          <label className="mtable__alias">
            Show by{' '}
            <select
              value={aliasLabel ?? ''}
              onChange={(e) => setAliasLabel(e.target.value || null)}
              aria-label="Show members by name or alias"
            >
              <option value="">Name</option>
              {aliasAttrs.map((a) => (
                <option key={a.name} value={a.name}>
                  {a.name}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        {hasHierarchy ? (
          <div className="mtable__modes" role="group" aria-label="View">
            <button
              type="button"
              aria-pressed={mode === 'flat'}
              className={mode === 'flat' ? 'is-active' : ''}
              onClick={() => setMode('flat')}
            >
              Flat
            </button>
            <button
              type="button"
              aria-pressed={mode === 'hierarchy'}
              className={mode === 'hierarchy' ? 'is-active' : ''}
              onClick={() => setMode('hierarchy')}
            >
              Hierarchy
            </button>
            {mode === 'hierarchy' && !searching ? (
              <>
                <button type="button" onClick={expandAll} title="Expand all">
                  Expand all
                </button>
                <button type="button" onClick={() => setExpanded(new Set())} title="Collapse all">
                  Collapse all
                </button>
              </>
            ) : null}
          </div>
        ) : null}
        <span className="mtable__count" role="status" aria-live="polite">
          {searching ? `${rows.length} of ${total}` : `${total}`} {total === 1 ? 'member' : 'members'}
        </span>
      </div>

      <div
        className="mtable__scroll"
        ref={virtual.containerRef}
        onScroll={virtual.onScroll}
        role="table"
        aria-label={`Members of ${dimension.name}`}
        aria-rowcount={rows.length + 1}
        aria-colcount={colCount}
        style={{ ['--mtable-cols' as string]: cols }}
      >
        <div className="mtable__head" role="row" aria-rowindex={1}>
          {selectable ? (
            <span className="mtable__cell mtable__cell--check" role="columnheader">
              <input
                type="checkbox"
                aria-label="Select all shown members"
                checked={allShownSelected}
                ref={(el) => {
                  if (el) el.indeterminate = !allShownSelected && rows.some((r) => selected?.has(r.name))
                }}
                onChange={toggleSelectAll}
              />
            </span>
          ) : null}
          <button
            type="button"
            className="mtable__cell mtable__cell--member mtable__sortbtn"
            role="columnheader"
            aria-sort={ariaSort('member')}
            onClick={() => toggleSort('member')}
          >
            {aliasLabel ?? 'Member'}
            {sortMark('member')}
          </button>
          <button
            type="button"
            className="mtable__cell mtable__sortbtn"
            role="columnheader"
            aria-sort={ariaSort('kind')}
            onClick={() => toggleSort('kind')}
          >
            Kind{sortMark('kind')}
          </button>
          {shownAttrs.map((a) => (
            <button
              key={a.name}
              type="button"
              className="mtable__cell mtable__sortbtn"
              role="columnheader"
              aria-sort={ariaSort(a.name)}
              onClick={() => toggleSort(a.name)}
            >
              {a.name}
              {sortMark(a.name)}
            </button>
          ))}
        </div>

        <div className="mtable__body" style={{ height: virtual.totalHeight }}>
          <div style={{ transform: `translateY(${virtual.offsetTop}px)` }}>
            {rows.length === 0 ? (
              <p className="mtable__empty muted">
                {searching ? `No members match "${search.trim()}"` : 'No members yet.'}
              </p>
            ) : (
              slice.map((r, i) => {
                const absIndex = virtual.start + i
                const isSel = selectable && selected?.has(r.name)
                return (
                  <div
                    key={r.path}
                    className={`mtable__row${isSel ? ' is-selected' : ''}`}
                    role="row"
                    aria-rowindex={absIndex + 2}
                    aria-selected={selectable ? !!isSel : undefined}
                    style={{ height: ROW_H }}
                  >
                    {selectable ? (
                      <span className="mtable__cell mtable__cell--check" role="cell">
                        <input
                          type="checkbox"
                          aria-label={`Select ${r.name}`}
                          checked={!!isSel}
                          onChange={() => onRowSelect(absIndex, r.name, false)}
                          onClick={(e) => {
                            if (e.shiftKey) onRowSelect(absIndex, r.name, true)
                          }}
                        />
                      </span>
                    ) : null}
                    <span
                      className="mtable__cell mtable__cell--member"
                      role="rowheader"
                      style={mode === 'hierarchy' && !searching ? { paddingLeft: `${r.depth * 1.1 + 0.5}rem` } : undefined}
                    >
                      {mode === 'hierarchy' && !searching && r.hasChildren ? (
                        <button
                          type="button"
                          className="mtable__twisty"
                          aria-label={r.expanded ? `Collapse ${r.name}` : `Expand ${r.name}`}
                          aria-expanded={r.expanded}
                          onClick={() => toggleExpand(r.path)}
                        >
                          {r.expanded ? '▾' : '▸'}
                        </button>
                      ) : mode === 'hierarchy' && !searching ? (
                        <span className="mtable__twisty mtable__twisty--leaf" aria-hidden="true" />
                      ) : null}
                      <span className="mtable__name">{labelOf(r.name)}</span>
                    </span>
                    <span className="mtable__cell mtable__kind" role="cell">
                      {KIND_LABEL[r.kind]}
                    </span>
                    {shownAttrs.map((a) => (
                      <span
                        key={a.name}
                        className={`mtable__cell mtable__attr${a.kind === 'numeric' ? ' mtable__attr--num' : ''}`}
                        role="cell"
                      >
                        {attrValues.get(a.name)?.get(r.name) ?? ''}
                      </span>
                    ))}
                  </div>
                )
              })
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
