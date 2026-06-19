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
  numeric: 'Numeric',
  string: 'String',
  consolidated: 'Consolidation',
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
/** The member view modes the table can offer. `flat` lists every member;
 * `hierarchy` follows the consolidation rollups; `leaves` lists only the leaf
 * members (those with no children). The set editor offers all three; other
 * callers default to flat + hierarchy. */
type ViewMode = 'flat' | 'hierarchy' | 'leaves'

/**
 * A scalable member table (ADR-0032): the members of a dimension as rows, with
 * the member name as a pinned leading column, a kind column, and any toggled-on
 * attribute columns. Search (wildcard + alias-aware), sortable headers, a
 * Flat/Hierarchy toggle, per-attribute column filters, and in-house row
 * virtualization let it handle thousands of members at a constant DOM cost.
 *
 * Two interactive modes layer on top of the read-only view:
 *  - `selectable` makes it the Available pane of the set builder: each row has a
 *    checkbox, reports selection changes, and gains Keep/Hide filters that scope
 *    the list to (or away from) the current selection.
 *  - `editable` (ignored when selectable) makes the attribute cells inline
 *    editable: click a cell to type a value, Enter/blur to commit, Escape to
 *    cancel; commits flow up through `onAttrEdit`.
 *
 * It uses static table semantics (role=table); the interactive controls
 * (checkboxes, twisties, sort headers, cell editors) are normal tab stops.
 */
export default function MemberTable({
  dimension,
  selectable = false,
  selected,
  onSelectedChange,
  editable = false,
  onAttrEdit,
  leavesMode = false,
}: {
  dimension: DimensionDto
  selectable?: boolean
  selected?: Set<string>
  onSelectedChange?: (next: Set<string>) => void
  /** Editor mode: render attribute cells as inline-editable. Ignored when
   * `selectable` (the set builder needs read-only cells under its checkboxes). */
  editable?: boolean
  /** Commit an edited attribute value (only called in `editable` mode). */
  onAttrEdit?: (element: string, attribute: string, value: string) => void | Promise<void>
  /** Offer a third "Leaves" view mode (only leaf members). The set editor turns
   * this on; the dimension model surfaces leave it off (ADR-0036). */
  leavesMode?: boolean
}) {
  const attributes = useMemo(() => dimension.attributes ?? [], [dimension])
  const aliasAttrs = useMemo(() => attributes.filter((a) => a.kind === 'alias'), [attributes])
  const editing0 = editable && !selectable

  const [search, setSearch] = useState('')
  const [sortKey, setSortKey] = useState<string>('model') // 'model' | 'member' | 'kind' | attr name
  const [sortDir, setSortDir] = useState<SortDir>('asc')
  // Editor mode opens with every attribute column shown so editing is
  // discoverable; the set builder opens with none (the picker is about members).
  const [visibleAttrs, setVisibleAttrs] = useState<Set<string>>(() =>
    editing0 ? new Set(attributes.map((a) => a.name)) : new Set(),
  )
  const [aliasLabel, setAliasLabel] = useState<string | null>(null)
  const [mode, setMode] = useState<ViewMode>('flat')
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set())
  const [columnsOpen, setColumnsOpen] = useState(false)
  // Per-attribute column filters (attr name -> wildcard/substring query) and
  // whether the filter bar is shown.
  const [colFilters, setColFilters] = useState<Record<string, string>>({})
  const [filtersOpen, setFiltersOpen] = useState(false)
  // Set-builder view filter: scope the list to / away from the current selection.
  const [keepHide, setKeepHide] = useState<'all' | 'keep' | 'hide'>('all')
  // The attribute cell currently being edited (editor mode), and its draft text.
  const [editing, setEditing] = useState<{ element: string; attr: string } | null>(null)
  const [editText, setEditText] = useState('')
  // Anchor for shift-range selection: the stable `path` of the last-clicked row,
  // NOT a positional index. The row list reorders/refilters on sort/search/filter
  // changes, so a stored index would go stale; a path is resolved against the
  // CURRENT `rows` at shift-click time so the range matches what the user sees.
  const lastClicked = useRef<string | null>(null)
  const columnsRef = useRef<HTMLDivElement | null>(null)

  // Re-show all attribute columns when the edited dimension changes, so each
  // dimension's editor opens with its own attributes visible.
  useEffect(() => {
    if (editing0) setVisibleAttrs(new Set(attributes.map((a) => a.name)))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dimension.name, editing0])

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
  // The leaf members (no outgoing consolidation edge), for the "Leaves" view.
  const leaves = useMemo(() => {
    const hasChild = new Set<string>()
    for (const e of dimension.edges) hasChild.add(e.parent)
    return new Set(dimension.elements.filter((e) => !hasChild.has(e.name)).map((e) => e.name))
  }, [dimension])

  const labelOf = useCallback(
    (name: string) => (aliasLabel ? attrValues.get(aliasLabel)?.get(name) || name : name),
    [aliasLabel, attrValues],
  )

  const match = useMemo(() => matcher(search), [search])
  const searching = search.trim() !== ''

  // Per-attribute column filters: a member passes only if its value for each
  // filtered attribute matches that column's wildcard/substring query.
  const colFilterFns = useMemo(() => {
    const fns: Array<(name: string) => boolean> = []
    for (const a of attributes) {
      const q = colFilters[a.name]
      if (!q || q.trim() === '') continue
      const m = matcher(q)
      const vals = attrValues.get(a.name)
      fns.push((name) => m(vals?.get(name) ?? ''))
    }
    return fns
  }, [attributes, colFilters, attrValues])
  const hasColFilter = colFilterFns.length > 0

  // Set-builder Keep/Hide scopes the visible list to / away from the selection.
  const keepHideActive = selectable && keepHide !== 'all'
  const passesKeepHide = useCallback(
    (name: string) => {
      if (!keepHideActive) return true
      const inSel = selected?.has(name) ?? false
      return keepHide === 'keep' ? inSel : !inSel
    },
    [keepHideActive, keepHide, selected],
  )

  // Any active filter forces the flat (filtered) row path, even in hierarchy
  // mode. Leaves mode is also a flat (filtered-to-leaves) listing.
  const filtering = searching || hasColFilter || keepHideActive || mode === 'leaves'

  // Commit the in-progress attribute edit (editor mode). Skips a no-op edit and
  // lets the parent surface any save error; Escape clears `editing` first so the
  // resulting blur is a no-op.
  const commitEdit = () => {
    if (!editing) return
    const current = attrValues.get(editing.attr)?.get(editing.element) ?? ''
    if (editText !== current) void Promise.resolve(onAttrEdit?.(editing.element, editing.attr, editText))
    setEditing(null)
  }

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
    if (mode === 'hierarchy' && !filtering) {
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
    const filtered = dimension.elements.filter(
      (e) =>
        (mode !== 'leaves' || leaves.has(e.name)) &&
        (match(e.name) || match(labelOf(e.name))) &&
        colFilterFns.every((f) => f(e.name)) &&
        passesKeepHide(e.name),
    )
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
  }, [mode, filtering, expanded, tree, dimension, leaves, match, labelOf, sortKey, sortDir, order, kindOf, attrValues, colFilterFns, passesKeepHide])

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
  // The anchor is the last-clicked row's stable `path`; the range is resolved
  // against the CURRENT `rows` so a sort/filter change between the two clicks
  // does not select an unintended block (the stored index would have gone stale).
  const onRowSelect = (path: string, name: string, shift: boolean) => {
    if (!selectable || !onSelectedChange) return
    const next = new Set(selected ?? [])
    const anchor = lastClicked.current
    const anchorIndex = anchor !== null ? rows.findIndex((r) => r.path === anchor) : -1
    const index = rows.findIndex((r) => r.path === path)
    if (shift && anchorIndex !== -1 && index !== -1) {
      const [lo, hi] = [anchorIndex, index].sort((a, b) => a - b)
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
    lastClicked.current = path
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
            {leavesMode ? (
              <button
                type="button"
                aria-pressed={mode === 'leaves'}
                className={mode === 'leaves' ? 'is-active' : ''}
                onClick={() => setMode('leaves')}
                title="Show only leaf members (those with no children)"
              >
                Leaves
              </button>
            ) : null}
            {mode === 'hierarchy' && !filtering ? (
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
        {attributes.length > 0 ? (
          <button
            type="button"
            className="mtable__colsbtn"
            aria-pressed={filtersOpen}
            onClick={() => setFiltersOpen((o) => !o)}
            title="Filter by attribute value"
          >
            Filters{hasColFilter ? ` (${colFilterFns.length})` : ''}
          </button>
        ) : null}
        {selectable ? (
          <div className="mtable__modes" role="group" aria-label="Filter by selection">
            <button
              type="button"
              aria-pressed={keepHide === 'keep'}
              className={keepHide === 'keep' ? 'is-active' : ''}
              onClick={() => setKeepHide((k) => (k === 'keep' ? 'all' : 'keep'))}
              title="Show only the selected members"
            >
              Keep
            </button>
            <button
              type="button"
              aria-pressed={keepHide === 'hide'}
              className={keepHide === 'hide' ? 'is-active' : ''}
              onClick={() => setKeepHide((k) => (k === 'hide' ? 'all' : 'hide'))}
              title="Hide the selected members"
            >
              Hide
            </button>
          </div>
        ) : null}
        <span className="mtable__count" role="status" aria-live="polite">
          {filtering ? `${rows.length} of ${total}` : `${total}`} {total === 1 ? 'member' : 'members'}
        </span>
      </div>

      {filtersOpen && shownAttrs.length > 0 ? (
        <div className="mtable__filterbar" role="group" aria-label="Attribute column filters">
          {shownAttrs.map((a) => (
            <label key={a.name} className="mtable__filterfield">
              <span className="mtable__filterlabel">{a.name}</span>
              <input
                type="search"
                value={colFilters[a.name] ?? ''}
                placeholder={`Filter ${a.name}`}
                aria-label={`Filter by ${a.name}`}
                onChange={(e) =>
                  setColFilters((f) => ({ ...f, [a.name]: e.target.value }))
                }
              />
            </label>
          ))}
          {hasColFilter ? (
            <button type="button" className="mtable__colsbtn" onClick={() => setColFilters({})}>
              Clear filters
            </button>
          ) : null}
        </div>
      ) : filtersOpen ? (
        <div className="mtable__filterbar mtable__filterbar--hint">
          <span className="muted">Add an attribute column to filter by its value.</span>
        </div>
      ) : null}

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
                {filtering
                  ? searching
                    ? `No members match "${search.trim()}"`
                    : 'No members match the current filters.'
                  : 'No members yet.'}
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
                          onChange={() => onRowSelect(r.path, r.name, false)}
                          onClick={(e) => {
                            if (e.shiftKey) onRowSelect(r.path, r.name, true)
                          }}
                        />
                      </span>
                    ) : null}
                    <span
                      className="mtable__cell mtable__cell--member"
                      role="rowheader"
                      style={mode === 'hierarchy' && !filtering ? { paddingLeft: `${r.depth * 1.1 + 0.5}rem` } : undefined}
                    >
                      {mode === 'hierarchy' && !filtering && r.hasChildren ? (
                        <button
                          type="button"
                          className="mtable__twisty"
                          aria-label={r.expanded ? `Collapse ${r.name}` : `Expand ${r.name}`}
                          aria-expanded={r.expanded}
                          onClick={() => toggleExpand(r.path)}
                        >
                          {r.expanded ? '▾' : '▸'}
                        </button>
                      ) : mode === 'hierarchy' && !filtering ? (
                        <span className="mtable__twisty mtable__twisty--leaf" aria-hidden="true" />
                      ) : null}
                      <span className="mtable__name">{labelOf(r.name)}</span>
                    </span>
                    <span className="mtable__cell mtable__kind" role="cell">
                      {KIND_LABEL[r.kind]}
                    </span>
                    {shownAttrs.map((a) => {
                      const val = attrValues.get(a.name)?.get(r.name) ?? ''
                      const numCls = a.kind === 'numeric' ? ' mtable__attr--num' : ''
                      if (editing0 && editing?.element === r.name && editing.attr === a.name) {
                        return (
                          <span key={a.name} className={`mtable__cell mtable__attr${numCls}`} role="cell">
                            <input
                              className="mtable__celledit"
                              autoFocus
                              value={editText}
                              inputMode={a.kind === 'numeric' ? 'decimal' : undefined}
                              aria-label={`${a.name} for ${r.name}`}
                              onChange={(e) => setEditText(e.target.value)}
                              onBlur={commitEdit}
                              onKeyDown={(e) => {
                                if (e.key === 'Enter') {
                                  e.preventDefault()
                                  commitEdit()
                                } else if (e.key === 'Escape') {
                                  e.preventDefault()
                                  setEditing(null)
                                }
                              }}
                            />
                          </span>
                        )
                      }
                      if (editing0) {
                        return (
                          <button
                            key={a.name}
                            type="button"
                            className={`mtable__cell mtable__attr mtable__celltrigger${numCls}`}
                            role="cell"
                            aria-label={`Edit ${a.name} for ${r.name}, currently ${val || 'empty'}`}
                            onClick={() => {
                              setEditing({ element: r.name, attr: a.name })
                              setEditText(val)
                            }}
                          >
                            {val !== '' ? val : <span className="mtable__cellempty">Set...</span>}
                          </button>
                        )
                      }
                      return (
                        <span key={a.name} className={`mtable__cell mtable__attr${numCls}`} role="cell">
                          {val}
                        </span>
                      )
                    })}
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
