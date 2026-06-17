import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  explainCell,
  getCube,
  listSubsets,
  previewMdx,
  readCells,
  spreadCells,
  writeCell,
  type CellDto,
  type Coord,
  type CubeDetail,
  type DimensionDto,
  type SpreadMethod,
  type SubsetDto,
  type TraceDto,
} from '../api/client'
import { Button, Dialog, Select } from '../ui'
import PivotFields, { type AxisRole, type AxisSet } from './PivotFields'
import SubsetEditor from './SubsetEditor'
import { TraceView } from './TraceView'

function cellKey(row: string, col: string): string {
  return `${row} ${col}`
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

export default function PivotGrid({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
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
  // Which consolidation members on the row axis are expanded (drill-down).
  const [expandedRows, setExpandedRows] = useState<Set<string>>(() => new Set())
  // Saved subsets per dimension (for the "select a set" menu on each axis chip).
  const [subsetsByDim, setSubsetsByDim] = useState<Record<string, SubsetDto[]>>({})
  // The member set applied to an axis dimension, resolved to a member list; a
  // missing/null entry means "all members" (the default, with drill-down).
  const [axisSet, setAxisSet] = useState<Record<string, AxisSet | null>>({})
  // The dimension whose set editor (SubsetEditor) dialog is open, if any.
  const [subsetEditorDim, setSubsetEditorDim] = useState<string | null>(null)
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

  // Default filter member for a dimension (its first element).
  const defaultMember = useCallback(
    (dimName: string) => detail?.dimensions.find((d) => d.name === dimName)?.elements[0]?.name ?? '',
    [detail],
  )

  // Re-pivot: move a dimension onto Rows, Columns, or Filters. Rows and Columns
  // always hold exactly one dimension, so a drop swaps with the current occupant
  // (axis<->axis) or trades places with a filter; moving an axis dimension to
  // Filters promotes the first filter onto the vacated axis.
  const placeDimension = useCallback(
    (dim: string, role: AxisRole) => {
      if (!detail) return
      if (role === 'rows') {
        if (dim === rowDim) return
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
      } else if (role === 'columns') {
        if (dim === colDim) return
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
      } else {
        // role === 'filters'
        if (dim !== rowDim && dim !== colDim) return // already a filter
        const filters = detail.dimensions
          .map((d) => d.name)
          .filter((n) => n !== rowDim && n !== colDim)
        if (filters.length === 0) return // a 2-D cube needs both axes filled
        const promote = filters[0]
        if (dim === rowDim) setRowDim(promote)
        else setColDim(promote)
        setContext((c) => {
          const n = { ...c }
          delete n[promote]
          n[dim] = defaultMember(dim)
          return n
        })
      }
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

  const refresh = useCallback(async () => {
    if (!detail || !rowDim || !colDim) return
    const rows = visibleRows
    const colSet = axisSet[colDim] ?? null
    const cols = (
      colSet
        ? colSet.members
        : (detail.dimensions.find((d) => d.name === colDim)?.elements ?? []).map((e) => e.name)
    ).map((name) => ({ name }))
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
  }, [cube, detail, rowDim, colDim, coordFor, visibleRows, axisSet])

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

  const colSet = axisSet[colDim] ?? null
  const colMembers: { name: string }[] = colSet
    ? colSet.members.map((name) => ({ name }))
    : (detail.dimensions.find((d) => d.name === colDim)?.elements ?? [])
  const editorDimDto = subsetEditorDim
    ? (detail.dimensions.find((d) => d.name === subsetEditorDim) ?? null)
    : null

  return (
    <div>
      <PivotFields
        dimensions={detail.dimensions}
        rowDim={rowDim}
        colDim={colDim}
        context={context}
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
        <span className="grid-toolbar__spacer" />
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
              {colMembers.map((c) => (
                <th key={c.name} scope="col">
                  {c.name}
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
                {colMembers.map((c, ci) => {
                  const cell = cells.get(cellKey(r.name, c.name))
                  return (
                    <CellView
                      key={c.name}
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
        title="Calculated value — click to see how it is calculated"
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
      title={cell.overlaid ? 'Uncommitted what-if value' : 'Editable — type a value, Enter to save'}
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
