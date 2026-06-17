import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  createView,
  explainCell,
  getCube,
  listSubsets,
  previewMdx,
  readCells,
  spreadCells,
  writeCell,
  type AxisSpecDef,
  type CellDto,
  type ContextEntry,
  type Coord,
  type CubeDetail,
  type DimensionDto,
  type SpreadMethod,
  type SubsetDto,
  type TraceDto,
  type ViewDef,
  type Visibility,
} from '../api/client'
import { Button, Dialog, Select } from '../ui'
import PivotFields, { type AxisRole, type AxisSet } from './PivotFields'
import SubsetEditor from './SubsetEditor'
import { TraceView } from './TraceView'

function cellKey(row: string, col: string): string {
  return `${row} ${col}`
}

/** Return a copy of `s` without `key` (or `s` unchanged if it was absent). */
function deleteFrom(s: Set<string>, key: string): Set<string> {
  if (!s.has(key)) return s
  const n = new Set(s)
  n.delete(key)
  return n
}

/** A bracket-quoted MDX identifier ( ] is escaped as ]] ). */
function mdxId(name: string): string {
  return `[${name.replace(/]/g, ']]')}]`
}

/** Build the MDX query the current layout represents: the visible column members
 * on COLUMNS, the visible row members on ROWS, and every off-axis dimension as a
 * single-member slicer in WHERE. */
function buildMdxQuery(opts: {
  cube: string
  rowDim: string
  colDim: string
  rowMembers: string[]
  colMembers: string[]
  slicers: { dim: string; member: string }[]
}): string {
  const member = (dim: string, m: string) => `${mdxId(dim)}.${mdxId(m)}`
  const cols = opts.colMembers.map((m) => member(opts.colDim, m)).join(', ')
  const rows = opts.rowMembers.map((m) => member(opts.rowDim, m)).join(', ')
  const lines = [
    'SELECT',
    `  { ${cols} } ON COLUMNS,`,
    `  { ${rows} } ON ROWS`,
    `FROM ${mdxId(opts.cube)}`,
  ]
  if (opts.slicers.length > 0) {
    lines.push(`WHERE ( ${opts.slicers.map((s) => member(s.dim, s.member)).join(', ')} )`)
  }
  return lines.join('\n')
}

/** One row in the (possibly drilled-down) row axis: a dimension member with its
 * nesting depth and whether it can be expanded to reveal children. */
interface VisibleRow {
  name: string
  depth: number
  expandable: boolean
}

/** Build the consolidation forest for a dimension: an ordered child list per
 * parent, and the set of roots (members that are no one's child). Children keep
 * the dimension's own element order so the grid is deterministic. */
function buildForest(dim: DimensionDto | undefined): {
  roots: string[]
  childrenOf: Map<string, string[]>
} {
  const childrenOf = new Map<string, string[]>()
  const childSet = new Set<string>()
  if (dim) {
    const order = new Map(dim.elements.map((el, i) => [el.name, i] as const))
    for (const e of dim.edges) {
      const arr = childrenOf.get(e.parent)
      if (arr) arr.push(e.child)
      else childrenOf.set(e.parent, [e.child])
      childSet.add(e.child)
    }
    for (const arr of childrenOf.values()) {
      arr.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0))
    }
  }
  const roots = dim ? dim.elements.filter((el) => !childSet.has(el.name)).map((el) => el.name) : []
  return { roots, childrenOf }
}

/** Flatten the forest into the rows that are currently visible, honoring the
 * expanded set. A member with children gets a twisty; expanding it inserts its
 * children one level deeper. An `ancestry` guard makes alternate-rollup DAGs
 * (a member reachable from two parents) safe against cycles. */
function flattenForest(
  roots: string[],
  childrenOf: Map<string, string[]>,
  expanded: Set<string>,
): VisibleRow[] {
  const out: VisibleRow[] = []
  const visit = (name: string, depth: number, ancestry: Set<string>) => {
    const kids = childrenOf.get(name)
    const expandable = !!kids && kids.length > 0
    out.push({ name, depth, expandable })
    if (expandable && expanded.has(name) && !ancestry.has(name)) {
      const next = new Set(ancestry).add(name)
      for (const child of kids) visit(child, depth + 1, next)
    }
  }
  for (const r of roots) visit(r, 0, new Set())
  return out
}

export default function PivotGrid({
  cube,
  reloadSignal,
  onModelChange,
}: {
  cube: string
  reloadSignal: number
  /** Called after the layout is saved as a View, so the explorer can refresh. */
  onModelChange?: () => void
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  const [rowDim, setRowDim] = useState('')
  const [colDim, setColDim] = useState('')
  const [context, setContext] = useState<Record<string, string>>({})
  const [cells, setCells] = useState<Map<string, CellDto>>(new Map())
  const [error, setError] = useState<string | null>(null)
  const [drill, setDrill] = useState<{ label: string; trace: TraceDto | null } | null>(null)
  // Bumped to re-run the initial load after an error (the Retry affordance).
  const [retryKey, setRetryKey] = useState(0)
  // 'off' is the disabled sentinel; a Radix Select.Item value may never be the
  // empty string, so the "off" option carries a real value.
  const [spreadMode, setSpreadMode] = useState<'off' | SpreadMethod>('off')
  // Which consolidation members on the row / column axes are expanded (drill-down).
  const [expandedRows, setExpandedRows] = useState<Set<string>>(() => new Set())
  const [expandedCols, setExpandedCols] = useState<Set<string>>(() => new Set())
  // Dimensions parked in the "Unused" zone (still pinned to a member via context,
  // just kept out of the active Filters list). Purely an organizational split.
  const [unused, setUnused] = useState<Set<string>>(() => new Set())
  // Saved subsets per dimension (for the "select a set" menu on each axis chip).
  const [subsetsByDim, setSubsetsByDim] = useState<Record<string, SubsetDto[]>>({})
  // The member set applied to an axis dimension, resolved to a member list; a
  // missing/null entry means "all members" (the default, with drill-down).
  const [axisSet, setAxisSet] = useState<Record<string, AxisSet | null>>({})
  // The dimension whose set editor (SubsetEditor) dialog is open, if any.
  const [subsetEditorDim, setSubsetEditorDim] = useState<string | null>(null)
  // "Save view" dialog: persist the current layout as a named, shared/private View.
  const [saveOpen, setSaveOpen] = useState(false)
  const [saveName, setSaveName] = useState('')
  const [saveVis, setSaveVis] = useState<Visibility>('private')
  const [saveBusy, setSaveBusy] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  // "Show MDX" dialog: previews the query the current layout generates.
  const [mdxOpen, setMdxOpen] = useState(false)
  const gridRef = useRef<HTMLDivElement>(null)

  // Load (or reload) the saved subsets for every dimension, so each axis chip's
  // "select a set" menu is current (used on first load and after a new set saves).
  const loadSubsets = useCallback(async (dims: DimensionDto[]) => {
    const pairs = await Promise.all(
      dims.map((d) =>
        listSubsets(cube, d.name)
          .then((ss) => [d.name, ss] as const)
          .catch(() => [d.name, [] as SubsetDto[]] as const),
      ),
    )
    return Object.fromEntries(pairs)
  }, [cube])

  useEffect(() => {
    let cancelled = false
    // Clear a prior cube's error so switching cubes / retrying isn't blocked.
    setError(null)
    setAxisSet({})
    setUnused(new Set())
    getCube(cube)
      .then((loaded) => {
        if (cancelled) return
        setDetail(loaded)
        const dims = loaded.dimensions
        const row = dims[0]?.name ?? ''
        const col = dims[1]?.name ?? row
        setRowDim(row)
        setColDim(col)
        const ctx: Record<string, string> = {}
        for (const dim of dims) {
          if (dim.name !== row && dim.name !== col) {
            ctx[dim.name] = dim.elements[0]?.name ?? ''
          }
        }
        setContext(ctx)
        void loadSubsets(dims).then((m) => {
          if (!cancelled) setSubsetsByDim(m)
        })
      })
      .catch((err: unknown) =>
        setError(err instanceof Error ? err.message : 'Failed to load cube'),
      )
    return () => {
      cancelled = true
    }
  }, [cube, retryKey, loadSubsets])

  const coordFor = useCallback(
    (rowMember: string, colMember: string): Coord => ({
      ...context,
      [rowDim]: rowMember,
      [colDim]: colMember,
    }),
    [context, rowDim, colDim],
  )

  // Drill-down state belongs to a single (cube, row dimension) view; reset it
  // when either changes so a stale expansion never carries over.
  useEffect(() => {
    setExpandedRows(new Set())
  }, [cube, rowDim])
  useEffect(() => {
    setExpandedCols(new Set())
  }, [cube, colDim])

  const rowSet = axisSet[rowDim] ?? null
  const rowDimDto = detail?.dimensions.find((d) => d.name === rowDim)
  const { roots, childrenOf } = useMemo(() => buildForest(rowDimDto), [rowDimDto])
  const visibleRows = useMemo(
    () =>
      // A set is an explicit member list, shown flat; with no set, the row axis is
      // the full dimension as a drill-down forest.
      rowSet
        ? rowSet.members.map((name) => ({ name, depth: 0, expandable: false }))
        : flattenForest(roots, childrenOf, expandedRows),
    [rowSet, roots, childrenOf, expandedRows],
  )

  const toggleRow = useCallback((name: string) => {
    setExpandedRows((s) => {
      const n = new Set(s)
      if (n.has(name)) n.delete(name)
      else n.add(name)
      return n
    })
  }, [])

  // Row drill-down level controls. Only meaningful when the row axis is a
  // consolidation hierarchy (no explicit member set applied).
  const rowHierarchical = !rowSet && childrenOf.size > 0
  const rowExpandAll = () => setExpandedRows(new Set(childrenOf.keys()))
  const rowCollapseAll = () => setExpandedRows(new Set())
  // Expand to the next level: open every currently-visible collapsed parent (the
  // frontier), revealing one more level each click.
  const rowExpandNext = () =>
    setExpandedRows((cur) => {
      const next = new Set(cur)
      for (const r of visibleRows) if (r.expandable && !next.has(r.name)) next.add(r.name)
      return next
    })
  // Collapse to the previous level: close the deepest currently-expanded parents.
  const rowCollapsePrev = () => {
    let maxDepth = -1
    for (const r of visibleRows) if (expandedRows.has(r.name)) maxDepth = Math.max(maxDepth, r.depth)
    if (maxDepth < 0) return
    setExpandedRows((cur) => {
      const next = new Set(cur)
      for (const r of visibleRows) if (r.depth === maxDepth && next.has(r.name)) next.delete(r.name)
      return next
    })
  }

  // Column axis drill-down: the mirror of the row hierarchy. Expanding a
  // consolidation inserts its children as further columns to the right.
  const colSet = axisSet[colDim] ?? null
  const colDimDto = detail?.dimensions.find((d) => d.name === colDim)
  const { roots: colRoots, childrenOf: colChildrenOf } = useMemo(
    () => buildForest(colDimDto),
    [colDimDto],
  )
  const visibleCols = useMemo(
    () =>
      colSet
        ? colSet.members.map((name) => ({ name, depth: 0, expandable: false }))
        : flattenForest(colRoots, colChildrenOf, expandedCols),
    [colSet, colRoots, colChildrenOf, expandedCols],
  )

  const toggleCol = useCallback((name: string) => {
    setExpandedCols((s) => {
      const n = new Set(s)
      if (n.has(name)) n.delete(name)
      else n.add(name)
      return n
    })
  }, [])

  const colHierarchical = !colSet && colChildrenOf.size > 0
  const colExpandAll = () => setExpandedCols(new Set(colChildrenOf.keys()))
  const colCollapseAll = () => setExpandedCols(new Set())
  const colExpandNext = () =>
    setExpandedCols((cur) => {
      const next = new Set(cur)
      for (const c of visibleCols) if (c.expandable && !next.has(c.name)) next.add(c.name)
      return next
    })
  const colCollapsePrev = () => {
    let maxDepth = -1
    for (const c of visibleCols) if (expandedCols.has(c.name)) maxDepth = Math.max(maxDepth, c.depth)
    if (maxDepth < 0) return
    setExpandedCols((cur) => {
      const next = new Set(cur)
      for (const c of visibleCols) if (c.depth === maxDepth && next.has(c.name)) next.delete(c.name)
      return next
    })
  }

  // Default filter member for a dimension (its first element).
  const defaultMember = useCallback(
    (dimName: string) => detail?.dimensions.find((d) => d.name === dimName)?.elements[0]?.name ?? '',
    [detail],
  )

  // Re-pivot: move a dimension onto Rows, Columns, Filters, or Unused. Rows and
  // Columns always hold exactly one dimension, so a drop swaps with the current
  // occupant (axis<->axis) or trades places with an off-axis dimension; moving an
  // axis dimension off promotes the first off-axis dimension onto the vacated
  // axis. Filters and Unused are both off-axis (member-pinned); the only
  // difference is whether the dimension is parked in the Unused set.
  const placeDimension = useCallback(
    (dim: string, role: AxisRole) => {
      if (!detail) return
      if (role === 'rows') {
        if (dim === rowDim) return
        setUnused((u) => deleteFrom(u, dim))
        if (dim === colDim) {
          setColDim(rowDim)
          setRowDim(dim)
          return
        }
        setRowDim(dim)
        setContext((c) => {
          const n = { ...c }
          delete n[dim]
          n[rowDim] = defaultMember(rowDim)
          return n
        })
        return
      }
      if (role === 'columns') {
        if (dim === colDim) return
        setUnused((u) => deleteFrom(u, dim))
        if (dim === rowDim) {
          setRowDim(colDim)
          setColDim(dim)
          return
        }
        setColDim(dim)
        setContext((c) => {
          const n = { ...c }
          delete n[dim]
          n[colDim] = defaultMember(colDim)
          return n
        })
        return
      }
      // 'filters' or 'unused': off-axis, member-pinned roles.
      if (dim === rowDim || dim === colDim) {
        const offAxis = detail.dimensions
          .map((d) => d.name)
          .filter((n) => n !== rowDim && n !== colDim)
        if (offAxis.length === 0) return // a 2-D cube needs both axes filled
        const promote = offAxis[0]
        if (dim === rowDim) setRowDim(promote)
        else setColDim(promote)
        setContext((c) => {
          const n = { ...c }
          delete n[promote]
          n[dim] = defaultMember(dim)
          return n
        })
        // The promoted dimension is now on an axis, so it can no longer be parked.
        setUnused((u) => {
          const n = deleteFrom(u, promote)
          return role === 'unused' ? new Set(n).add(dim) : deleteFrom(n, dim)
        })
        return
      }
      // dim is already off-axis: just park or un-park it.
      setUnused((u) => (role === 'unused' ? new Set(u).add(dim) : deleteFrom(u, dim)))
    },
    [detail, rowDim, colDim, defaultMember],
  )

  // Apply a member set to an axis dimension (null clears it back to all members).
  // Dynamic (MDX) subsets are resolved to a concrete member list on selection.
  const pickSet = useCallback(
    async (dim: string, subset: SubsetDto | null) => {
      if (!subset) {
        setAxisSet((s) => ({ ...s, [dim]: null }))
        return
      }
      let members = subset.members
      if ((!members || members.length === 0) && subset.mdx) {
        try {
          members = (await previewMdx(cube, dim, subset.mdx)).map((m) => m.name)
        } catch {
          members = []
        }
      }
      setAxisSet((s) => ({ ...s, [dim]: { name: subset.name, members } }))
    },
    [cube],
  )

  // Capture the current layout as a saved View definition: each axis is the
  // chosen member set (a named subset) or all members; every other dimension is
  // a single-member context (filter). Mirrors the Views builder's contract.
  const buildViewDef = useCallback((): ViewDef => {
    const axisSpec = (dimName: string): AxisSpecDef => {
      const set = axisSet[dimName]
      if (set) return { dimension: dimName, type: 'subset', subset: set.name }
      const members = detail?.dimensions.find((d) => d.name === dimName)?.elements.map((e) => e.name) ?? []
      return { dimension: dimName, type: 'members', members }
    }
    const ctx: ContextEntry[] = (detail?.dimensions ?? [])
      .filter((d) => d.name !== rowDim && d.name !== colDim)
      .map((d) => ({ dimension: d.name, member: context[d.name] ?? d.elements[0]?.name ?? '' }))
    return { rows: [axisSpec(rowDim)], columns: [axisSpec(colDim)], context: ctx, suppress_zeros: false }
  }, [detail, rowDim, colDim, context, axisSet])

  const saveView = useCallback(async () => {
    if (saveName.trim() === '') {
      setSaveError('Name the view before saving.')
      return
    }
    setSaveBusy(true)
    try {
      await createView(cube, { ...buildViewDef(), name: saveName.trim(), visibility: saveVis })
      setSaveOpen(false)
      setSaveName('')
      setSaveError(null)
      onModelChange?.()
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : 'Could not save the view')
    } finally {
      setSaveBusy(false)
    }
  }, [cube, buildViewDef, saveName, saveVis, onModelChange])

  const refresh = useCallback(async () => {
    if (!detail || !rowDim || !colDim) return
    const rows = visibleRows
    const cols = visibleCols
    const coords: Coord[] = []
    for (const r of rows) {
      for (const c of cols) {
        coords.push(coordFor(r.name, c.name))
      }
    }
    try {
      const fetched = await readCells(cube, coords)
      const next = new Map<string, CellDto>()
      let i = 0
      for (const r of rows) {
        for (const c of cols) {
          next.set(cellKey(r.name, c.name), fetched[i])
          i += 1
        }
      }
      setCells(next)
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to read cells')
    }
  }, [cube, detail, rowDim, colDim, coordFor, visibleRows, visibleCols])

  useEffect(() => {
    void refresh()
  }, [refresh, reloadSignal])

  const commit = useCallback(
    async (rowMember: string, colMember: string, previous: string, next: string) => {
      if (next === previous) return
      try {
        await writeCell(cube, coordFor(rowMember, colMember), next)
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Write failed')
      }
      await refresh()
    },
    [cube, coordFor, refresh],
  )

  /** Spread a value entered at a (possibly consolidated) cell across its leaves. */
  const spread = useCallback(
    async (rowMember: string, colMember: string, typed: string) => {
      if (spreadMode === 'off') return
      // Clear ignores the typed value; the others need a number.
      const value = spreadMode === 'clear' ? '0' : typed.trim()
      if (spreadMode !== 'clear' && value === '') return
      try {
        await spreadCells(cube, coordFor(rowMember, colMember), value, spreadMode)
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Spread failed')
      }
      await refresh()
    },
    [cube, coordFor, refresh, spreadMode],
  )

  /** Open the provenance drill-down for a calculated cell. */
  const drillInto = useCallback(
    async (rowMember: string, colMember: string) => {
      const label = `${rowMember} / ${colMember}`
      setDrill({ label, trace: null })
      try {
        const trace = await explainCell(cube, coordFor(rowMember, colMember), 'full')
        setDrill({ label, trace })
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Could not explain this cell')
        setDrill(null)
      }
    },
    [cube, coordFor],
  )

  /** Move focus to the editable cell input at (r, c), if one exists. */
  const focusCell = useCallback((r: number, c: number) => {
    const target = gridRef.current?.querySelector<HTMLInputElement>(
      `input[data-r="${r}"][data-c="${c}"]`,
    )
    target?.focus()
    target?.select()
  }, [])

  // Surface an initial-load failure instead of an endless loading banner; the
  // error <p> further down is unreachable while detail is null. Recoverable.
  if (error && !detail) {
    return (
      <p className="error" role="alert">
        {error}{' '}
        <Button variant="ghost" size="sm" onClick={() => setRetryKey((k) => k + 1)}>
          Retry
        </Button>
      </p>
    )
  }

  if (!detail) {
    return <p className="banner" role="status">Loading {cube}…</p>
  }

  const editorDimDto = subsetEditorDim
    ? (detail.dimensions.find((d) => d.name === subsetEditorDim) ?? null)
    : null

  const mdxQuery = buildMdxQuery({
    cube,
    rowDim,
    colDim,
    rowMembers: visibleRows.map((r) => r.name),
    colMembers: visibleCols.map((c) => c.name),
    slicers: detail.dimensions
      .filter((d) => d.name !== rowDim && d.name !== colDim)
      .map((d) => ({ dim: d.name, member: context[d.name] ?? d.elements[0]?.name ?? '' })),
  })

  return (
    <div>
      <PivotFields
        dimensions={detail.dimensions}
        rowDim={rowDim}
        colDim={colDim}
        context={context}
        unused={unused}
        subsetsByDim={subsetsByDim}
        axisSet={axisSet}
        onPlace={placeDimension}
        onContextMember={(dim, v) => setContext((c) => ({ ...c, [dim]: v }))}
        onPickSet={(dim, subset) => void pickSet(dim, subset)}
        onNewSet={(dim) => setSubsetEditorDim(dim)}
      />
      <div className="grid-toolbar">
        <label className="grid-axis">
          <span>Spread</span>
          <Select
            value={spreadMode}
            onValueChange={(v) => setSpreadMode(v as 'off' | SpreadMethod)}
            options={[
              { value: 'off', label: 'Off' },
              { value: 'equal', label: 'Equal' },
              { value: 'proportional', label: 'Proportional' },
              { value: 'repeat', label: 'Repeat' },
              { value: 'clear', label: 'Clear' },
            ]}
            ariaLabel="Spread mode"
          />
        </label>
        {rowHierarchical ? (
          <div className="grid-levels" role="group" aria-label="Row levels">
            <span className="grid-levels__label">Rows</span>
            <Button variant="ghost" size="sm" onClick={rowExpandNext} title="Expand to the next level">
              + level
            </Button>
            <Button variant="ghost" size="sm" onClick={rowCollapsePrev} title="Collapse to the previous level">
              - level
            </Button>
            <Button variant="ghost" size="sm" onClick={rowExpandAll} title="Expand all rows">
              Expand all
            </Button>
            <Button variant="ghost" size="sm" onClick={rowCollapseAll} title="Collapse all rows">
              Collapse all
            </Button>
          </div>
        ) : null}
        {colHierarchical ? (
          <div className="grid-levels" role="group" aria-label="Column levels">
            <span className="grid-levels__label">Columns</span>
            <Button variant="ghost" size="sm" onClick={colExpandNext} title="Expand to the next level">
              + level
            </Button>
            <Button variant="ghost" size="sm" onClick={colCollapsePrev} title="Collapse to the previous level">
              - level
            </Button>
            <Button variant="ghost" size="sm" onClick={colExpandAll} title="Expand all columns">
              Expand all
            </Button>
            <Button variant="ghost" size="sm" onClick={colCollapseAll} title="Collapse all columns">
              Collapse all
            </Button>
          </div>
        ) : null}
        <span className="grid-toolbar__spacer" />
        <Button variant="ghost" size="sm" icon="◫" onClick={() => { setSaveError(null); setSaveOpen(true) }}>
          Save view
        </Button>
        <Button variant="ghost" size="sm" icon="∑" onClick={() => setMdxOpen(true)}>
          Show MDX
        </Button>
        <Button variant="ghost" size="sm" icon="↻" onClick={() => void refresh()}>
          Refresh
        </Button>
      </div>
      {spreadMode !== 'off' ? (
        <p className="banner" role="status">
          Spreading is on ({spreadMode}). Type a value into a total cell to distribute it across the
          leaves underneath. Turn it off to edit single cells again.
        </p>
      ) : null}
      {error ? (
        <p className="error" role="alert">
          {error}
        </p>
      ) : null}
      <div className="grid-wrap" ref={gridRef}>
        <table className="pivot">
          <thead>
            <tr>
              <th className="corner">
                {rowDim} / {colDim}
              </th>
              {visibleCols.map((c, ci) => (
                <th key={`${c.name}#${ci}`} scope="col">
                  <span className="pivot__colhead">
                    {c.expandable ? (
                      <button
                        type="button"
                        className="pivot__twisty"
                        aria-expanded={expandedCols.has(c.name)}
                        aria-label={`${expandedCols.has(c.name) ? 'Collapse' : 'Expand'} ${c.name}`}
                        onClick={() => toggleCol(c.name)}
                      >
                        {expandedCols.has(c.name) ? '▾' : '▸'}
                      </button>
                    ) : null}
                    <span className="pivot__colhead-label">{c.name}</span>
                  </span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {visibleRows.map((r, ri) => (
              <tr key={`${r.name}#${ri}`}>
                <th className="rowhead" scope="row">
                  <span
                    className="pivot__rowhead-inner"
                    style={{ paddingInlineStart: `${r.depth * 16}px` }}
                  >
                    {r.expandable ? (
                      <button
                        type="button"
                        className="pivot__twisty"
                        aria-expanded={expandedRows.has(r.name)}
                        aria-label={`${expandedRows.has(r.name) ? 'Collapse' : 'Expand'} ${r.name}`}
                        onClick={() => toggleRow(r.name)}
                      >
                        {expandedRows.has(r.name) ? '▾' : '▸'}
                      </button>
                    ) : (
                      <span className="pivot__twisty pivot__twisty--leaf" aria-hidden="true" />
                    )}
                    <span className="pivot__rowhead-label">{r.name}</span>
                  </span>
                </th>
                {visibleCols.map((c, ci) => {
                  const cell = cells.get(cellKey(r.name, c.name))
                  return (
                    <CellView
                      key={`${c.name}#${ci}`}
                      cell={cell}
                      r={ri}
                      c={ci}
                      rowName={r.name}
                      colName={c.name}
                      spreadMode={spreadMode}
                      onCommit={(next) => void commit(r.name, c.name, cell?.value ?? '', next)}
                      onSpread={(next) => void spread(r.name, c.name, next)}
                      onNav={focusCell}
                      onDrill={() => void drillInto(r.name, c.name)}
                    />
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {drill ? (
        <Dialog
          open
          onOpenChange={(open) => {
            if (!open) setDrill(null)
          }}
          title={`How “${drill.label}” is calculated`}
          description="The value, and the stored inputs, rules, and totals it comes from."
          size="md"
        >
          {drill.trace ? (
            <div className="trace">
              <TraceView node={drill.trace} />
            </div>
          ) : (
            <p className="muted">Loading provenance…</p>
          )}
        </Dialog>
      ) : null}
      {editorDimDto ? (
        <Dialog
          open
          onOpenChange={(open) => {
            if (!open) setSubsetEditorDim(null)
          }}
          title={`Member set for ${editorDimDto.name}`}
          description="Pick the members this axis should show, then save the set to reuse it."
          size="xl"
        >
          <SubsetEditor
            cube={cube}
            dimension={editorDimDto}
            onSaved={(name) => {
              const dim = editorDimDto.name
              setSubsetEditorDim(null)
              void loadSubsets(detail.dimensions).then((m) => {
                setSubsetsByDim(m)
                const created = m[dim]?.find((s) => s.name === name) ?? null
                if (created) void pickSet(dim, created)
              })
            }}
            onCancel={() => setSubsetEditorDim(null)}
          />
        </Dialog>
      ) : null}
      <Dialog
        open={saveOpen}
        onOpenChange={setSaveOpen}
        title="Save view"
        description="Save the current rows, columns, filters, and member sets as a reusable view."
        size="sm"
      >
        <div className="pw-form">
          <label className="field">
            <span className="field__label">View name</span>
            <input
              value={saveName}
              placeholder="e.g. Q1 by region"
              onChange={(e) => setSaveName(e.target.value)}
            />
          </label>
          <label className="field">
            <span className="field__label">Who can see it</span>
            <Select
              value={saveVis}
              onValueChange={(v) => setSaveVis(v as Visibility)}
              options={[
                { value: 'private', label: 'Only me' },
                { value: 'public', label: 'Everyone' },
              ]}
              ariaLabel="View visibility"
            />
          </label>
          {saveError ? (
            <p className="error" role="alert">
              {saveError}
            </p>
          ) : null}
          <div className="pw-form__actions">
            <Button variant="ghost" size="sm" onClick={() => setSaveOpen(false)}>
              Cancel
            </Button>
            <Button size="sm" disabled={saveBusy} onClick={() => void saveView()}>
              Save view
            </Button>
          </div>
        </div>
      </Dialog>

      <Dialog
        open={mdxOpen}
        onOpenChange={setMdxOpen}
        title="MDX for this view"
        description="The query the current rows, columns, filters, and sets generate."
        size="md"
      >
        <pre className="mdx-preview">{mdxQuery}</pre>
        <div className="pw-form__actions">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void navigator.clipboard?.writeText(mdxQuery)}
          >
            Copy
          </Button>
          <Button size="sm" onClick={() => setMdxOpen(false)}>
            Close
          </Button>
        </div>
      </Dialog>
    </div>
  )
}

function CellView({
  cell,
  r,
  c,
  rowName,
  colName,
  spreadMode,
  onCommit,
  onSpread,
  onNav,
  onDrill,
}: {
  cell: CellDto | undefined
  r: number
  c: number
  rowName: string
  colName: string
  spreadMode: 'off' | SpreadMethod
  onCommit: (next: string) => void
  onSpread: (next: string) => void
  onNav: (r: number, c: number) => void
  onDrill: () => void
}) {
  const cellLabel = `${rowName} ${colName}`
  if (!cell || !cell.editable) {
    // With spreading on, a calculated (total) cell accepts a value to distribute
    // across its leaves; otherwise it stays a click-to-explain calculated value.
    if (cell && spreadMode !== 'off') {
      return (
        <td className={cell.overlaid ? 'cell editable overlaid' : 'cell editable'} title={`Spread (${spreadMode}) across the leaves under this total`}>
          <input
            key={`spread-${cell.value ?? ''}`}
            data-r={r}
            data-c={c}
            aria-label={`Spread ${cellLabel}`}
            defaultValue=""
            placeholder={spreadMode === 'clear' ? '↵ clear' : cell.value ?? ''}
            inputMode="decimal"
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault()
                onSpread(e.currentTarget.value)
                e.currentTarget.value = ''
              } else if (e.key === 'Escape') {
                e.currentTarget.value = ''
                e.currentTarget.blur()
              }
            }}
            onBlur={(e) => {
              if (e.currentTarget.value.trim() !== '') onSpread(e.currentTarget.value)
              e.currentTarget.value = ''
            }}
          />
        </td>
      )
    }
    const hasValue = cell?.value != null && cell.value !== ''
    return (
      <td
        className={cell?.overlaid ? 'cell calc overlaid' : 'cell calc'}
        title="Calculated value. Click to see how it is calculated."
      >
        {hasValue ? (
          <button type="button" className="cell-drill" onClick={onDrill}>
            {cell?.value}
          </button>
        ) : (
          (cell?.value ?? '')
        )}
      </td>
    )
  }
  return (
    <td
      className={cell.overlaid ? 'cell editable overlaid' : 'cell editable'}
      title={cell.overlaid ? 'Uncommitted what-if value' : 'Editable. Type a value, then Enter to save.'}
    >
      <input
        key={cell.value ?? ''}
        data-r={r}
        data-c={c}
        aria-label={cellLabel}
        defaultValue={cell.value ?? ''}
        inputMode="decimal"
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === 'ArrowDown') {
            e.preventDefault()
            e.currentTarget.blur()
            onNav(r + 1, c)
          } else if (e.key === 'ArrowUp') {
            e.preventDefault()
            e.currentTarget.blur()
            onNav(r - 1, c)
          } else if (e.key === 'Escape') {
            e.currentTarget.value = cell.value ?? ''
            e.currentTarget.blur()
          }
        }}
        onBlur={(e) => onCommit(e.currentTarget.value.trim())}
      />
    </td>
  )
}
