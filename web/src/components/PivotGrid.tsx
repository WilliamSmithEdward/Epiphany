import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  createView,
  executeMdx,
  explainCell,
  getCube,
  isAbortError,
  listSubsets,
  previewMdx,
  readCells,
  spreadCells,
  writeCell,
  type AxisSpecDef,
  type CellDto,
  type CellsetDto,
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
import {
  allExpandableKeys,
  buildForest,
  computeHeaderSpans,
  flattenForest,
  subsetVisibleMembers,
  type Forest,
  type VisibleMember,
} from '../model/tree'
import { Button, Dialog, Select } from '../ui'
import { useVirtualRows } from '../ui/useVirtualRows'
import CellsetGrid from './CellsetGrid'
import PivotFields, { type AxisRole, type AxisSet } from './PivotFields'
import SubsetEditor from './SubsetEditor'
import { TraceView } from './TraceView'

// The pivot body virtualizes above this many rendered rows: below it a large
// model is not a concern and a plain <tbody> keeps rowSpan headers simplest;
// above it we window the rows (and fetch only the visible window's cells) so a
// several-thousand-row cellset stays responsive (ADR-0020 performance mandate).
const VIRTUAL_ROW_THRESHOLD = 150
// The fixed body-row height (px) the row windowing math assumes. Pivot data rows
// are single-line (cells never wrap - see .grid-wrap table.pivot td), so a fixed
// height is accurate; it must match the CSS row box (padding + line-height).
const PIVOT_ROW_H = 33
// Overscan rows above/below the viewport, and the extra window margin (in rows)
// added when fetching cells so a small scroll reveals already-loaded values
// rather than briefly-blank cells while the next windowed read lands.
const ROW_OVERSCAN = 6
const FETCH_MARGIN = 40

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

/** One member of an axis tuple: a dimension member with its nesting depth (in
 * its own dimension's drill-down forest) and whether it can be expanded. */
interface TupleMember {
  dim: string
  name: string
  /** Unique within this dimension's visible list: the member's drill path, so an
   * alternate-rollup member (reachable under two parents) is distinct per parent. */
  key: string
  depth: number
  expandable: boolean
}

/** A full tuple on an axis: one member per dimension on that axis, outer first. */
type Tuple = TupleMember[]

/** A separator that cannot appear in an element name, so a tuple's member names
 * join to a stable, collision-free key. */
const TUPLE_SEP = ''

/** A stable, UNIQUE string key for a tuple, joining each member's drill-path key
 * (not its bare name) so an alternate-rollup member, reachable under two parents
 * (e.g. a region rolling up to both Total and Coastal), yields a DISTINCT key per
 * occurrence. Bare names collide there, giving sibling rows/cells the same React
 * key and breaking reconciliation (rows duplicate and cells linger on toggle). */
function tupleKey(tuple: Tuple): string {
  return tuple.map((m) => m.key).join(TUPLE_SEP)
}

/** The cartesian product of each dimension's visible-member list, in dim order
 * (outermost dimension varies slowest). Each result is one axis tuple. */
function cartesian(perDim: { dim: string; members: VisibleMember[] }[]): Tuple[] {
  if (perDim.length === 0) return []
  let acc: Tuple[] = [[]]
  for (const { dim, members } of perDim) {
    const next: Tuple[] = []
    for (const prefix of acc) {
      for (const m of members) {
        next.push([...prefix, { dim, name: m.name, key: m.key, depth: m.depth, expandable: m.expandable }])
      }
    }
    acc = next
  }
  return acc
}

/** Build the MDX query the current layout represents: the visible column tuples
 * on COLUMNS (a CrossJoin when columns nest more than one dimension), the
 * visible row tuples on ROWS, and every off-axis dimension as a single-member
 * slicer in WHERE. */
function buildMdxQuery(opts: {
  cube: string
  rowDims: string[]
  colDims: string[]
  rowMembers: Record<string, string[]>
  colMembers: Record<string, string[]>
  slicers: { dim: string; member: string }[]
}): string {
  const member = (dim: string, m: string) => `${mdxId(dim)}.${mdxId(m)}`
  // A single dimension is a plain set { a, b }; nested dimensions cross-join
  // their per-dimension sets so each tuple is the cartesian of the levels.
  const axis = (dims: string[], membersByDim: Record<string, string[]>): string => {
    const sets = dims.map(
      (d) => `{ ${(membersByDim[d] ?? []).map((m) => member(d, m)).join(', ')} }`,
    )
    if (sets.length === 0) return '{ }'
    if (sets.length === 1) return sets[0]
    return `CrossJoin(${sets.join(', ')})`
  }
  const lines = [
    'SELECT',
    `  ${axis(opts.colDims, opts.colMembers)} ON COLUMNS,`,
    `  ${axis(opts.rowDims, opts.rowMembers)} ON ROWS`,
    `FROM ${mdxId(opts.cube)}`,
  ]
  if (opts.slicers.length > 0) {
    lines.push(`WHERE ( ${opts.slicers.map((s) => member(s.dim, s.member)).join(', ')} )`)
  }
  return lines.join('\n')
}

export default function PivotGrid({
  cube,
  reloadSignal,
  onModelChange,
  showMdx = true,
}: {
  cube: string
  reloadSignal: number
  /** Called after the layout is saved as a View, so the explorer can refresh. */
  onModelChange?: () => void
  /** Whether to offer the "Show MDX" affordance. Hidden for a pure business-user
   * persona (ADR-0020 progressive disclosure): raw MDX is modeler/admin machinery.
   * Defaults to shown so non-persona callers keep the existing behavior. */
  showMdx?: boolean
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  // The dimensions nested on each axis, outer to inner. An axis may be empty (the
  // grid then shows a placeholder) - moving a chip never auto-promotes another
  // dimension to keep an axis filled.
  const [rowDims, setRowDims] = useState<string[]>([])
  const [colDims, setColDims] = useState<string[]>([])
  const [context, setContext] = useState<Record<string, string>>({})
  const [cells, setCells] = useState<Map<string, CellDto>>(new Map())
  // True while a refresh() is in flight (after the first load too). Drives the
  // grid's aria-busy + a polite live status and dims the currently-painted cells
  // so a user never reads a previous-slice number AS the current slice's value:
  // on a context/filter change the row/col tuple KEYS are unchanged, so the old
  // numbers would otherwise stay crisply painted under the new slice until the
  // readCells resolves.
  const [refreshing, setRefreshing] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [drill, setDrill] = useState<{ label: string; trace: TraceDto | null } | null>(null)
  // Bumped to re-run the initial load after an error (the Retry affordance).
  const [retryKey, setRetryKey] = useState(0)
  // 'off' is the disabled sentinel; a Radix Select.Item value may never be the
  // empty string, so the "off" option carries a real value.
  const [spreadMode, setSpreadMode] = useState<'off' | SpreadMethod>('off')
  // Drill-down expansion per dimension: the expanded occurrences within that
  // dimension's hierarchy, each held by its DRILL-PATH KEY (not its bare name)
  // so an alternate-rollup member reachable under two parents can be expanded or
  // collapsed independently per parent. The outer map is keyed by dimension name
  // so a dimension drills the same way whether it stands alone or nests.
  const [expanded, setExpanded] = useState<Record<string, Set<string>>>({})
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
  // "Save view" dialog open flag. The dialog's own form state (name, visibility,
  // busy, error) lives inside SaveViewDialog, not here, so a keystroke in its name
  // field re-renders only the small dialog and never the (potentially huge) grid
  // body - the pivot is the perf-critical surface (ADR-0020).
  const [saveOpen, setSaveOpen] = useState(false)
  // Independent zero-suppression: hide all-zero rows / all-zero columns. These are
  // LIVE toolbar toggles - they filter the displayed grid immediately (see
  // displayRowTuples/displayColTuples) - and are also captured into a saved view
  // (see buildViewDef). Off by default.
  const [suppressRows, setSuppressRows] = useState(false)
  const [suppressCols, setSuppressCols] = useState(false)
  // "Show MDX" dialog open flag. Like the Save dialog, its editable query text,
  // result, and run state live inside MdxDialog so typing in the MDX textarea
  // never re-renders the grid.
  const [mdxOpen, setMdxOpen] = useState(false)
  const gridRef = useRef<HTMLDivElement>(null)
  // Monotonic refresh generation: each refresh() bumps it and only applies its
  // own response if it is still the latest, so a slow readCells that resolves
  // after a newer refresh cannot overwrite the current cellset (request race).
  // The AbortController below is the systemic fix (it cancels the superseded
  // fetch); the counter stays as a same-tick backstop.
  const refreshGen = useRef(0)
  // The in-flight readCells AbortController, aborted when a newer refresh starts
  // or the grid unmounts, so a superseded/abandoned read is cancelled rather than
  // left to resolve and be discarded (ADR-0020 performance mandate).
  const refreshAbort = useRef<AbortController | null>(null)
  // The half-open row range [from, to) whose cells are currently loaded into
  // `cells`, so a scroll only re-reads when the needed window escapes it (a small
  // scroll stays within the FETCH_MARGIN already fetched). Reset to empty whenever
  // the layout/context changes and the cells map is cleared.
  const loadedRows = useRef<{ from: number; to: number }>({ from: 0, to: 0 })

  // Abort any in-flight windowed read when the grid unmounts.
  useEffect(() => () => refreshAbort.current?.abort(), [])

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
    // Clear a prior cube's error / layout so switching cubes / retrying isn't blocked.
    setError(null)
    setAxisSet({})
    setUnused(new Set())
    setExpanded({})
    getCube(cube)
      .then((loaded) => {
        if (cancelled) return
        setDetail(loaded)
        const dims = loaded.dimensions
        const row = dims[0]?.name
        // Default: first dimension on rows, second on columns (or the first
        // again if the cube is one-dimensional). The rest become filters.
        const initialRows = row ? [row] : []
        const initialCols = dims[1]?.name ? [dims[1].name] : initialRows
        setRowDims(initialRows)
        setColDims(initialCols)
        const onAxis = new Set([...initialRows, ...initialCols])
        const ctx: Record<string, string> = {}
        for (const dim of dims) {
          if (!onAxis.has(dim.name)) ctx[dim.name] = dim.elements[0]?.name ?? ''
        }
        setContext(ctx)
        void loadSubsets(dims).then((m) => {
          if (!cancelled) setSubsetsByDim(m)
        })
      })
      .catch((err: unknown) => {
        // Guard the catch too (the .then already does): a slow getCube for a
        // previous cube can reject after we switched cubes and loaded the new
        // one, painting a stale wrong-cube error over a healthy grid.
        if (cancelled) return
        setError(err instanceof Error ? err.message : 'Failed to load cube')
      })
    return () => {
      cancelled = true
    }
  }, [cube, retryKey, loadSubsets])

  // One consolidation forest per dimension, built once per cube load. Used to
  // flatten each axis dimension's visible members and to decide which header
  // runs get a drill-down twisty.
  const forests = useMemo(() => {
    const m = new Map<string, Forest>()
    for (const d of detail?.dimensions ?? []) m.set(d.name, buildForest(d))
    return m
  }, [detail])

  // The visible members of a single dimension: a saved set is an explicit member
  // list shown flat (depth 0, no drill-down); otherwise the full dimension as a
  // drill-down forest honoring its expansion set.
  const visibleMembersOf = useCallback(
    (dim: string): VisibleMember[] => {
      const set = axisSet[dim]
      if (set) return subsetVisibleMembers(set.members)
      const forest = forests.get(dim)
      if (!forest) return []
      return flattenForest(forest.roots, forest.childrenOf, expanded[dim] ?? new Set())
    },
    [axisSet, forests, expanded],
  )

  // The axis tuples: the cartesian product of each axis dimension's visible
  // members, outer dimension first.
  const rowTuples = useMemo(
    () => cartesian(rowDims.map((dim) => ({ dim, members: visibleMembersOf(dim) }))),
    [rowDims, visibleMembersOf],
  )
  const colTuples = useMemo(
    () => cartesian(colDims.map((dim) => ({ dim, members: visibleMembersOf(dim) }))),
    [colDims, visibleMembersOf],
  )

  // A displayed cell counts as non-zero when it is a NUMERIC cell holding a value
  // that is neither blank nor numeric zero. String cells carry no numeric value
  // and the engine treats them as zero for suppression (Cube::get returns zero for
  // a string element), so they are zero here too - matching what a saved
  // suppressed view of the same layout would show.
  const cellIsNonZero = useCallback(
    (rowTuple: Tuple, colTuple: Tuple): boolean => {
      const cell = cells.get(`${tupleKey(rowTuple)}||${tupleKey(colTuple)}`)
      if (!cell || cell.kind === 'string') return false
      const v = cell.value
      return v != null && v !== '' && Number(v) !== 0
    },
    [cells],
  )

  // Live zero-suppression: the row/column tuples actually rendered. Mirrors the
  // core (execute_view_with) - drop all-zero rows first (judged across every
  // column), then all-zero columns judged across the SURVIVING rows; each axis is
  // gated by its own toggle. readCells still fetches the FULL grid, so toggling is
  // instant and reversible with no re-query - this only filters what is shown.
  //
  // Two guards keep a refresh from briefly hiding everything: while a refresh is in
  // flight the cells map is stale relative to the (possibly new) tuples, so we
  // don't suppress at all; and a tuple whose cells have not been fetched yet (just
  // revealed by a drill-down) is kept rather than judged zero on missing data.
  const displayRowTuples = useMemo(() => {
    if (!suppressRows || refreshing || colTuples.length === 0) return rowTuples
    return rowTuples.filter((rt) => {
      let sawCell = false
      for (const ct of colTuples) {
        if (cellIsNonZero(rt, ct)) return true
        if (cells.has(`${tupleKey(rt)}||${tupleKey(ct)}`)) sawCell = true
      }
      return !sawCell
    })
  }, [suppressRows, refreshing, rowTuples, colTuples, cellIsNonZero, cells])
  const displayColTuples = useMemo(() => {
    if (!suppressCols || refreshing || displayRowTuples.length === 0) return colTuples
    return colTuples.filter((ct) => {
      let sawCell = false
      for (const rt of displayRowTuples) {
        if (cellIsNonZero(rt, ct)) return true
        if (cells.has(`${tupleKey(rt)}||${tupleKey(ct)}`)) sawCell = true
      }
      return !sawCell
    })
  }, [suppressCols, refreshing, displayRowTuples, colTuples, cellIsNonZero, cells])

  // The coordinate for a (row tuple, column tuple) cell: off-axis filters first,
  // then the row tuple's members, then the column tuple's members.
  const coordFor = useCallback(
    (rowTuple: Tuple, colTuple: Tuple): Coord => {
      const onAxis = new Set([...rowDims, ...colDims])
      const coord: Coord = {}
      for (const d of detail?.dimensions ?? []) {
        if (!onAxis.has(d.name)) coord[d.name] = context[d.name] ?? d.elements[0]?.name ?? ''
      }
      for (const m of rowTuple) coord[m.dim] = m.name
      for (const m of colTuple) coord[m.dim] = m.name
      return coord
    },
    [detail, context, rowDims, colDims],
  )

  // Whether zero-suppression is active on either axis. Suppression must judge
  // EVERY cell of a row/column to decide it is all-zero, so it needs the full grid
  // fetched (a windowed read cannot tell whether an unfetched row is all-zero); in
  // that mode windowing is disabled and the whole cross-product is read, matching
  // the original behavior. With suppression off (the common case) reads are
  // windowed to the visible rows.
  const suppressing = suppressRows || suppressCols
  // Virtualize the body (and window the reads) only above the threshold and only
  // when not suppressing. `rowTuples` (not the suppressed display list) sizes the
  // window in the common non-suppressing path, where the two are identical.
  const virtualizeRows = !suppressing && rowTuples.length > VIRTUAL_ROW_THRESHOLD

  // Fetch cells for row window [rowFrom, rowTo) x all column tuples and store them.
  // `replace` (a layout/context change, a manual/WS refresh) swaps the whole cells
  // map and resets the loaded-row range; a scroll-triggered window extension merges
  // the new rows into the existing map so already-loaded rows stay painted. Only
  // the visible window's coords are POSTed (unless suppressing, when the caller
  // passes the full range), so a several-thousand-row cellset is never materialized
  // as one giant request.
  const fetchRows = useCallback(
    async (rowFrom: number, rowTo: number, replace: boolean) => {
      if (!detail) return
      if (rowDims.length === 0 || colDims.length === 0 || rowTuples.length === 0 || colTuples.length === 0) {
        // No dimension on an axis (or an empty axis): nothing to fetch. Clear any
        // prior slice's cells and end any in-flight busy state so the placeholder /
        // "No data" empty state is not shown dimmed and AT does not keep announcing.
        refreshAbort.current?.abort()
        setCells(new Map())
        loadedRows.current = { from: 0, to: 0 }
        setRefreshing(false)
        return
      }
      const from = Math.max(0, rowFrom)
      const to = Math.min(rowTuples.length, rowTo)
      const coords: Coord[] = []
      for (let r = from; r < to; r++) {
        const rt = rowTuples[r]
        for (const ct of colTuples) coords.push(coordFor(rt, ct))
      }
      // Abort a still-in-flight read (superseded) before starting a newer one, then
      // bump the generation as a same-tick backstop against an out-of-order resolve.
      refreshAbort.current?.abort()
      const controller = new AbortController()
      refreshAbort.current = controller
      const gen = (refreshGen.current += 1)
      setRefreshing(true)
      try {
        const fetched = await readCells(cube, coords, { signal: controller.signal })
        if (gen !== refreshGen.current) return
        setCells((prev) => {
          const next = replace ? new Map<string, CellDto>() : new Map(prev)
          let i = 0
          for (let r = from; r < to; r++) {
            const rk = tupleKey(rowTuples[r])
            for (const ct of colTuples) {
              next.set(`${rk}||${tupleKey(ct)}`, fetched[i])
              i += 1
            }
          }
          return next
        })
        loadedRows.current = replace
          ? { from, to }
          : { from: Math.min(loadedRows.current.from, from), to: Math.max(loadedRows.current.to, to) }
        setError(null)
        setRefreshing(false)
      } catch (err) {
        // A cancelled read (superseded or unmounted) is benign; do not surface it.
        if (isAbortError(err) || gen !== refreshGen.current) return
        setError(err instanceof Error ? err.message : 'Failed to read cells')
        setRefreshing(false)
      }
    },
    [cube, detail, rowDims, colDims, coordFor, rowTuples, colTuples],
  )

  // Row windowing (ADR-0032's useVirtualRows, the same hook the member table uses):
  // above the threshold, render only the rows in (and a small overscan around) the
  // viewport, so the DOM node count stays constant no matter how many rows the
  // layout produces. `enabled=false` below the threshold renders every row (short
  // grids stay plain DOM). Disabled while suppressing, where every row must be in
  // the DOM to be judged for all-zero anyway. The scroll container is the shared
  // grid-wrap (its ref is merged with gridRef so focusCell can still query inputs).
  const virtual = useVirtualRows({
    rowCount: displayRowTuples.length,
    rowHeight: PIVOT_ROW_H,
    overscan: ROW_OVERSCAN,
    enabled: virtualizeRows,
  })
  // Merge the virtualization container ref with gridRef (both target .grid-wrap):
  // the hook measures/scrolls through its ref, focusCell queries inputs through
  // gridRef, and the windowed-read seed reads scrollTop through gridRef.
  const setGridEl = useCallback(
    (el: HTMLDivElement | null) => {
      gridRef.current = el
      virtual.containerRef.current = el
    },
    [virtual.containerRef],
  )

  // Extend the fetched row window as the user scrolls: when the visible window
  // (plus margin) escapes the already-loaded range, read the missing rows and merge
  // them in. Only fires while virtualizing (windowed reads); the full-fetch paths
  // load everything up front. Debounced implicitly by React batching + the loaded-
  // range guard, so a fast scroll issues few reads.
  useEffect(() => {
    if (!virtualizeRows) return
    const wantFrom = Math.max(0, virtual.start - FETCH_MARGIN)
    const wantTo = Math.min(rowTuples.length, virtual.end + FETCH_MARGIN)
    const { from, to } = loadedRows.current
    if (wantFrom < from || wantTo > to) void fetchRows(wantFrom, wantTo, false)
    // rowTuples.length pins the effect to the current layout; start/end drive it.
  }, [virtualizeRows, virtual.start, virtual.end, fetchRows, rowTuples.length])

  // Toggle one occurrence's drill-down expansion, by its drill-path key.
  const toggleExpanded = useCallback((dim: string, key: string) => {
    setExpanded((cur) => {
      const set = cur[dim] ?? new Set<string>()
      const n = new Set(set)
      if (n.has(key)) n.delete(key)
      else n.add(key)
      return { ...cur, [dim]: n }
    })
  }, [])

  // A dimension is a drill-down hierarchy (twisties + level controls) when it
  // has no explicit set applied AND its forest has at least one parent.
  const isHierarchical = useCallback(
    (dim: string) => !axisSet[dim] && (forests.get(dim)?.childrenOf.size ?? 0) > 0,
    [axisSet, forests],
  )

  // ---- per-axis drill-down level controls ----
  // Each axis's "Expand all" / "Collapse all" / "+ level" / "- level" act across
  // every drill-down dimension on that axis.

  const axisHierarchical = useCallback(
    (dims: string[]) => dims.some(isHierarchical),
    [isHierarchical],
  )

  const expandAll = useCallback(
    (dims: string[]) => {
      setExpanded((cur) => {
        const next = { ...cur }
        for (const dim of dims) {
          if (!isHierarchical(dim)) continue
          const forest = forests.get(dim)
          if (!forest) continue
          next[dim] = allExpandableKeys(forest.roots, forest.childrenOf)
        }
        return next
      })
    },
    [forests, isHierarchical],
  )

  const collapseAll = useCallback(
    (dims: string[]) => {
      setExpanded((cur) => {
        const next = { ...cur }
        for (const dim of dims) if (isHierarchical(dim)) next[dim] = new Set()
        return next
      })
    },
    [isHierarchical],
  )

  // Expand to the next level: open every currently-visible collapsed parent
  // (the frontier) on each drill-down dimension of the axis.
  const expandNext = useCallback(
    (dims: string[]) => {
      setExpanded((cur) => {
        const next = { ...cur }
        for (const dim of dims) {
          if (!isHierarchical(dim)) continue
          const forest = forests.get(dim)
          if (!forest) continue
          const set = new Set(cur[dim] ?? new Set<string>())
          for (const m of flattenForest(forest.roots, forest.childrenOf, set)) {
            if (m.expandable && !set.has(m.key)) set.add(m.key)
          }
          next[dim] = set
        }
        return next
      })
    },
    [forests, isHierarchical],
  )

  // Collapse to the previous level: close the deepest currently-expanded parents
  // on each drill-down dimension of the axis.
  const collapsePrev = useCallback(
    (dims: string[]) => {
      setExpanded((cur) => {
        const next = { ...cur }
        for (const dim of dims) {
          if (!isHierarchical(dim)) continue
          const forest = forests.get(dim)
          if (!forest) continue
          const set = new Set(cur[dim] ?? new Set<string>())
          const visible = flattenForest(forest.roots, forest.childrenOf, set)
          let maxDepth = -1
          for (const m of visible) if (set.has(m.key)) maxDepth = Math.max(maxDepth, m.depth)
          if (maxDepth < 0) continue
          for (const m of visible) if (m.depth === maxDepth && set.has(m.key)) set.delete(m.key)
          next[dim] = set
        }
        return next
      })
    },
    [forests, isHierarchical],
  )

  const rowHierarchical = axisHierarchical(rowDims)
  const colHierarchical = axisHierarchical(colDims)

  // The member an off-axis dimension resolves to by default: its top
  // consolidation (the aggregate "all" member) when it has one, else its first
  // root, else its first element. An Unused dimension pins to this so it does NOT
  // narrow the slice (see placeDimension); a Filters dimension starts here and is
  // then user-editable.
  const defaultMember = useCallback(
    (dimName: string): string => {
      const d = detail?.dimensions.find((x) => x.name === dimName)
      if (!d) return ''
      const isChild = new Set(d.edges.map((e) => e.child))
      const isParent = new Set(d.edges.map((e) => e.parent))
      // A top consolidation: a root (never a child) that aggregates (is a parent).
      const topConsol = d.elements.find((e) => !isChild.has(e.name) && isParent.has(e.name))
      if (topConsol) return topConsol.name
      const firstRoot = d.elements.find((e) => !isChild.has(e.name))
      return (firstRoot ?? d.elements[0])?.name ?? ''
    },
    [detail],
  )

  // Re-pivot: move a dimension onto Rows, Columns, Filters, or Unused. The move
  // affects ONLY the dragged dimension - an axis is allowed to become empty
  // rather than auto-promoting or swapping another dimension to keep it filled
  // (an empty axis renders a friendly placeholder instead). Dropping on
  // Rows/Columns appends the dimension as the innermost nesting level. Filters
  // and Unused are both off-axis: Filters pins to a user-chosen member (an active
  // slice); Unused pins to the dimension's default/aggregate member and is NOT a
  // filter (so setting a dimension aside removes its filtering effect).
  const placeDimension = useCallback(
    (dim: string, role: AxisRole) => {
      if (!detail) return
      const inRows = rowDims.includes(dim)
      const inCols = colDims.includes(dim)

      if (role === 'rows' || role === 'columns') {
        const target = role === 'rows'
        // Re-dropping onto the axis it already sits on is a no-op (reordering is
        // not handled this pass).
        if (target ? inRows : inCols) return
        // Remove it from its current home; the source axis may end up empty.
        if (inRows) setRowDims((a) => a.filter((d) => d !== dim))
        if (inCols) setColDims((a) => a.filter((d) => d !== dim))
        setUnused((u) => deleteFrom(u, dim))
        // Coming from an off-axis role: drop its pinned slicer member.
        if (!inRows && !inCols) {
          setContext((c) => {
            const n = { ...c }
            delete n[dim]
            return n
          })
        }
        // Append to the target axis (nesting it as the innermost dimension).
        if (target) setRowDims((a) => (a.includes(dim) ? a : [...a, dim]))
        else setColDims((a) => (a.includes(dim) ? a : [...a, dim]))
        return
      }

      // 'filters' or 'unused': off-axis, member-pinned roles.
      if (inRows || inCols) {
        // Leaving an axis for an off-axis role: drop any member set applied to it,
        // so returning it to an axis starts from "all members" rather than silently
        // re-applying the old set (which the off-axis chip never showed).
        setAxisSet((s) => {
          if (!(dim in s)) return s
          const n = { ...s }
          delete n[dim]
          return n
        })
        // Remove it from its axis; the vacated axis may end up empty.
        if (inRows) setRowDims((a) => a.filter((d) => d !== dim))
        if (inCols) setColDims((a) => a.filter((d) => d !== dim))
      }
      // Unused pins to the default/aggregate member (not a filter); Filters keeps
      // its current member if it already had one, else starts at the default.
      setContext((c) => ({
        ...c,
        [dim]: role === 'unused' ? defaultMember(dim) : (c[dim] ?? defaultMember(dim)),
      }))
      setUnused((u) => (role === 'unused' ? new Set(u).add(dim) : deleteFrom(u, dim)))
    },
    [detail, rowDims, colDims, defaultMember],
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
        } catch (err) {
          // Surface the failure instead of applying an empty set, which would
          // silently blank the axis (indistinguishable from a genuinely empty
          // set). Leave the current set in place so the grid stays readable.
          setError(
            err instanceof Error
              ? `Could not resolve the set "${subset.name}": ${err.message}`
              : `Could not resolve the set "${subset.name}".`,
          )
          return
        }
      }
      setError(null)
      setAxisSet((s) => ({ ...s, [dim]: { name: subset.name, members } }))
    },
    [cube],
  )

  // Capture the current layout as a saved View definition: each axis dimension
  // is the chosen member set (a named subset) or all members; every off-axis
  // dimension is a single-member context (filter). Mirrors the Views builder.
  const buildViewDef = useCallback((): ViewDef => {
    const axisSpec = (dimName: string): AxisSpecDef => {
      const set = axisSet[dimName]
      if (set) return { dimension: dimName, type: 'subset', subset: set.name }
      const members =
        detail?.dimensions.find((d) => d.name === dimName)?.elements.map((e) => e.name) ?? []
      return { dimension: dimName, type: 'members', members }
    }
    const onAxis = new Set([...rowDims, ...colDims])
    const ctx: ContextEntry[] = (detail?.dimensions ?? [])
      .filter((d) => !onAxis.has(d.name))
      .map((d) => ({ dimension: d.name, member: context[d.name] ?? d.elements[0]?.name ?? '' }))
    return {
      rows: rowDims.map(axisSpec),
      columns: colDims.map(axisSpec),
      context: ctx,
      suppress_zero_rows: suppressRows,
      suppress_zero_columns: suppressCols,
    }
  }, [detail, rowDims, colDims, context, axisSet, suppressRows, suppressCols])

  // Persist the current layout as a named view. Owned by the parent (it holds the
  // layout + buildViewDef); SaveViewDialog calls it with the name/visibility it
  // collected and surfaces any thrown error inline. Resolves on success so the
  // dialog can close and reset; rethrows so the dialog shows the failure.
  const createSavedView = useCallback(
    async (name: string, visibility: Visibility) => {
      await createView(cube, { ...buildViewDef(), name, visibility })
      onModelChange?.()
    },
    [cube, buildViewDef, onModelChange],
  )

  // Build the MDX the current layout represents. Computed lazily (only when the
  // "Show MDX" dialog opens) rather than on every render, since visibleMembersOf
  // runs per axis dimension and the string is rarely viewed.
  const buildMdx = useCallback((): string => {
    const onAxis = new Set([...rowDims, ...colDims])
    const slicers = (detail?.dimensions ?? [])
      .filter((d) => !onAxis.has(d.name))
      .map((d) => ({ dim: d.name, member: context[d.name] ?? d.elements[0]?.name ?? '' }))
    const rowMembersByDim: Record<string, string[]> = {}
    for (const dim of rowDims) rowMembersByDim[dim] = visibleMembersOf(dim).map((m) => m.name)
    const colMembersByDim: Record<string, string[]> = {}
    for (const dim of colDims) colMembersByDim[dim] = visibleMembersOf(dim).map((m) => m.name)
    return buildMdxQuery({
      cube,
      rowDims,
      colDims,
      rowMembers: rowMembersByDim,
      colMembers: colMembersByDim,
      slicers,
    })
  }, [cube, detail, rowDims, colDims, context, visibleMembersOf])

  // A full refresh of the current window: the visible window (+margin) when
  // virtualizing, else the whole row list. Replaces the cells map. Used on layout /
  // context / cube changes, WS reloads, and after a write. Reads the live scroll
  // window from `loadedRows` is not possible here (it is being reset), so it starts
  // from the top window; a subsequent scroll extends it via ensureWindow.
  const refresh = useCallback(async () => {
    if (!virtualizeRows) {
      await fetchRows(0, rowTuples.length, true)
      return
    }
    // Seed the initial visible window from the top; the scroll effect widens it as
    // the user moves. A margin is included so an immediate small scroll is covered.
    const container = gridRef.current
    const first = container ? Math.floor(container.scrollTop / PIVOT_ROW_H) : 0
    const visible = container ? Math.ceil(container.clientHeight / PIVOT_ROW_H) : VIRTUAL_ROW_THRESHOLD
    await fetchRows(first - FETCH_MARGIN, first + visible + FETCH_MARGIN, true)
  }, [virtualizeRows, fetchRows, rowTuples.length])

  useEffect(() => {
    void refresh()
  }, [refresh, reloadSignal])

  const commit = useCallback(
    async (rowTuple: Tuple, colTuple: Tuple, previous: string, next: string) => {
      if (next === previous) return
      try {
        await writeCell(cube, coordFor(rowTuple, colTuple), next)
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Write failed')
      }
      await refresh()
    },
    [cube, coordFor, refresh],
  )

  /** Spread a value entered at a (possibly consolidated) cell across its leaves. */
  const spread = useCallback(
    async (rowTuple: Tuple, colTuple: Tuple, typed: string) => {
      if (spreadMode === 'off') return
      // Clear ignores the typed value; the others need a number.
      const value = spreadMode === 'clear' ? '0' : typed.trim()
      if (spreadMode !== 'clear' && value === '') return
      try {
        await spreadCells(cube, coordFor(rowTuple, colTuple), value, spreadMode)
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Spread failed')
      }
      await refresh()
    },
    [cube, coordFor, refresh, spreadMode],
  )

  /** Open the provenance drill-down for a calculated cell. */
  const drillInto = useCallback(
    async (rowTuple: Tuple, colTuple: Tuple) => {
      const label = `${rowTuple.map((m) => m.name).join(' / ')} / ${colTuple
        .map((m) => m.name)
        .join(' / ')}`
      setDrill({ label, trace: null })
      try {
        const trace = await explainCell(cube, coordFor(rowTuple, colTuple), 'full')
        setDrill({ label, trace })
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Could not explain this cell')
        setDrill(null)
      }
    },
    [cube, coordFor],
  )

  /** Move focus to the editable cell input at absolute (r, c). When virtualizing,
   * the target row may be outside the rendered window, so scroll it into view and
   * focus on the next frame once it has mounted; otherwise focus it directly. */
  const focusCell = useCallback(
    (r: number, c: number) => {
      const grid = gridRef.current
      const find = () =>
        grid?.querySelector<HTMLInputElement>(`input[data-r="${r}"][data-c="${c}"]`) ?? null
      const focus = (el: HTMLInputElement | null) => {
        el?.focus()
        el?.select()
      }
      const target = find()
      if (target || !virtualizeRows || !grid) {
        focus(target)
        return
      }
      // Row not in the window: scroll so it lands in view, then focus once the
      // windowed render has placed the input (a rAF is enough after the scroll).
      grid.scrollTop = Math.max(0, r * PIVOT_ROW_H - grid.clientHeight / 2)
      requestAnimationFrame(() => requestAnimationFrame(() => focus(find())))
    },
    [virtualizeRows],
  )

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
    return <p className="banner" role="status">Loading {cube}...</p>
  }

  const editorDimDto = subsetEditorDim
    ? (detail.dimensions.find((d) => d.name === subsetEditorDim) ?? null)
    : null

  // computeHeaderSpans reads only {dimension, name, key}; map each tuple to that
  // shape once, for both axes.
  const headerTuples = (tuples: Tuple[]) =>
    tuples.map((t) => t.map((m) => ({ dimension: m.dim, name: m.name, key: m.key })))
  // Nested column headers: one row per column-axis level, run-length merged. Built
  // from the DISPLAYED tuples so zero-suppression collapses header spans too.
  const colHeader = computeHeaderSpans(headerTuples(displayColTuples))
  // The visible row window [winStart, winEnd): the whole list when not virtualizing,
  // else just the windowed slice the DOM actually renders. Everything below is
  // computed against this window so only visible rows are built.
  const winStart = virtualizeRows ? virtual.start : 0
  const winEnd = virtualizeRows ? virtual.end : displayRowTuples.length
  const windowRowTuples = virtualizeRows ? displayRowTuples.slice(winStart, winEnd) : displayRowTuples
  // For each VISIBLE body row, the row-header cells that begin (or, for a run that
  // started above the window, first surface) at that row, per row-axis level. A
  // rowSpan run is clipped to the window: a run [r, r+span) intersecting the window
  // is emitted at max(r, winStart) with its span clamped to the window, so an outer
  // dimension's member that spans rows starting off-screen still labels the first
  // visible row (with a correct, clipped rowSpan) instead of vanishing. Indexed by
  // window-relative row so the render maps it alongside windowRowTuples.
  const rowSpansAll = computeHeaderSpans(headerTuples(displayRowTuples))
  const rowHeaderAt: { dim: string; name: string; key?: string; rowSpan: number; startIndex: number }[][] =
    windowRowTuples.map(() => [])
  for (let level = 0; level < rowDims.length; level++) {
    let r = 0
    for (const run of rowSpansAll[level] ?? []) {
      const runStart = r
      const runEnd = r + run.span
      r = runEnd
      // Skip a run entirely outside the window; clip one that straddles it.
      if (runEnd <= winStart || runStart >= winEnd) continue
      const from = Math.max(runStart, winStart)
      const to = Math.min(runEnd, winEnd)
      rowHeaderAt[from - winStart].push({
        dim: run.dimension,
        name: run.name,
        key: run.key,
        rowSpan: to - from,
        startIndex: from,
      })
    }
  }
  // Spacer heights so the scrollbar reflects the full row count while only the
  // window is in the DOM: a leading <tr> of the rows above, a trailing <tr> of the
  // rows below. Zero when not virtualizing.
  const topSpacer = virtualizeRows ? virtual.offsetTop : 0
  const bottomSpacer = virtualizeRows
    ? Math.max(0, virtual.totalHeight - virtual.offsetTop - windowRowTuples.length * PIVOT_ROW_H)
    : 0

  // Whether a header run's member can be drilled into within its dimension.
  const runExpandable = (dim: string, name: string) =>
    isHierarchical(dim) && (forests.get(dim)?.childrenOf.has(name) ?? false)

  const cornerCols = Math.max(1, rowDims.length)
  const colLevels = colDims.length
  const cornerLabel = `${rowDims.join(' / ')} / ${colDims.join(' / ')}`
  // An axis with no dimension cannot address cells: the grid shows a placeholder
  // instead of a table, and a layout missing an axis is not a saveable view.
  const axisEmpty = rowDims.length === 0 || colDims.length === 0

  // Level-control button groups for an axis (rendered for rows and columns).
  const levelControls = (dims: string[], label: string) => (
    <div className="grid-levels" role="group" aria-label={`${label} levels`}>
      <span className="grid-levels__label">{label}</span>
      <Button variant="ghost" size="sm" onClick={() => expandNext(dims)} title="Expand to the next level">
        + level
      </Button>
      <Button variant="ghost" size="sm" onClick={() => collapsePrev(dims)} title="Collapse to the previous level">
        - level
      </Button>
      <Button variant="ghost" size="sm" onClick={() => expandAll(dims)} title={`Expand all ${label.toLowerCase()}`}>
        Expand all
      </Button>
      <Button variant="ghost" size="sm" onClick={() => collapseAll(dims)} title={`Collapse all ${label.toLowerCase()}`}>
        Collapse all
      </Button>
    </div>
  )

  return (
    <div>
      <PivotFields
        dimensions={detail.dimensions}
        rowDims={rowDims}
        colDims={colDims}
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
        {rowHierarchical ? levelControls(rowDims, 'Rows') : null}
        {colHierarchical ? levelControls(colDims, 'Columns') : null}
        {!axisEmpty ? (
          <div className="grid-suppress" role="group" aria-label="Zero suppression">
            <Button
              variant="ghost"
              size="sm"
              className="grid-toggle"
              aria-pressed={suppressRows}
              onClick={() => setSuppressRows((v) => !v)}
              title="Hide rows whose every cell is zero or blank"
            >
              Suppress zero rows
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className="grid-toggle"
              aria-pressed={suppressCols}
              onClick={() => setSuppressCols((v) => !v)}
              title="Hide columns whose every cell is zero or blank"
            >
              Suppress zero columns
            </Button>
          </div>
        ) : null}
        <span className="grid-toolbar__spacer" />
        <Button
          variant="ghost"
          size="sm"
          icon="◫"
          disabled={axisEmpty}
          title={axisEmpty ? 'Add a dimension to both Rows and Columns before saving a view.' : undefined}
          onClick={() => setSaveOpen(true)}
        >
          Save view
        </Button>
        {showMdx ? (
          <Button variant="ghost" size="sm" icon="∑" onClick={() => setMdxOpen(true)}>
            Show MDX
          </Button>
        ) : null}
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
      {/* Polite live status so a re-query (any refresh past the first load) is
          announced to assistive tech, not just visually. */}
      <div className="sr-only" role="status" aria-live="polite">
        {refreshing ? 'Refreshing values...' : ''}
      </div>
      {axisEmpty ? (
        // An axis with no dimension cannot address cells, so render a single
        // merged cell explaining what to do instead of a broken header-only grid.
        <div className="grid-wrap">
          <table className="pivot">
            <tbody>
              <tr>
                <td className="pivot__empty muted">
                  No data can be shown without a dimension on both Rows and Columns.
                  Drag a dimension onto each axis to build the grid.
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      ) : (
      <div
        className="grid-wrap"
        ref={setGridEl}
        onScroll={virtualizeRows ? virtual.onScroll : undefined}
        aria-busy={refreshing || undefined}
        // Dim the currently-painted cells while a refresh is in flight so a user
        // never reads a previous-slice number AS the current slice's value (the
        // tuple keys are unchanged on a context/filter change, so the stale numbers
        // would otherwise stay crisply painted until readCells resolves).
        style={refreshing ? { opacity: 0.5, transition: 'opacity 120ms' } : undefined}
      >
        <table className="pivot">
          <thead>
            {colLevels === 0 || colHeader.length === 0 ? (
              // No column levels, or every column was suppressed away: keep just
              // the corner label so the header is not blank above the empty-state.
              <tr>
                <th className="corner" colSpan={cornerCols}>
                  {cornerLabel}
                </th>
              </tr>
            ) : (
              colHeader.map((runs, level) => (
                <tr key={level}>
                  {level === 0 ? (
                    <th className="corner" colSpan={cornerCols} rowSpan={colLevels}>
                      {cornerLabel}
                    </th>
                  ) : null}
                  {runs.map((run, i) => {
                    const expandable = runExpandable(run.dimension, run.name)
                    // Address expansion by the occurrence's drill-path key (not its
                    // bare name) so a member under two parents toggles per occurrence.
                    const runKey = run.key ?? run.name
                    const isOpen = expanded[run.dimension]?.has(runKey) ?? false
                    return (
                      <th key={`${run.key ?? run.name}#${i}`} scope="col" colSpan={run.span}>
                        <span className="pivot__colhead">
                          {expandable ? (
                            <button
                              type="button"
                              className="pivot__twisty"
                              aria-expanded={isOpen}
                              aria-label={`${isOpen ? 'Collapse' : 'Expand'} ${run.name}`}
                              onClick={() => toggleExpanded(run.dimension, runKey)}
                            >
                              {isOpen ? '▾' : '▸'}
                            </button>
                          ) : null}
                          <span className="pivot__colhead-label">{run.name}</span>
                        </span>
                      </th>
                    )
                  })}
                </tr>
              ))
            )}
          </thead>
          <tbody>
            {displayRowTuples.length === 0 || displayColTuples.length === 0 ? (
              // Nothing to show: either a member set resolved to no members, or
              // zero-suppression hid every row/column. Show an explicit row so the
              // state reads as intentional, not a broken header-only table.
              <tr>
                <td
                  className="pivot__empty muted"
                  colSpan={cornerCols + Math.max(1, displayColTuples.length)}
                >
                  {(suppressRows || suppressCols) && rowTuples.length > 0 && colTuples.length > 0
                    ? 'Everything in view was hidden by zero-suppression. Turn off the Suppress zero toggles to show the hidden rows or columns.'
                    : 'No data to show. Adjust the filters or member sets on the rows axis.'}
                </td>
              </tr>
            ) : null}
            {/* Leading spacer: the total height of the rows above the window, so
                the scrollbar reflects the full row count while those rows stay out
                of the DOM. A single full-width cell keeps the column layout intact. */}
            {topSpacer > 0 ? (
              <tr aria-hidden="true" className="pivot__spacer">
                <td colSpan={cornerCols + displayColTuples.length} style={{ height: topSpacer, padding: 0, border: 0 }} />
              </tr>
            ) : null}
            {displayRowTuples.length > 0 &&
              displayColTuples.length > 0 &&
              windowRowTuples.map((rt, wi) => {
              // wi indexes the rendered window; ri is the absolute row index used
              // for cell data-r / keyboard nav so focus moves across the full grid.
              const ri = winStart + wi
              return (
              <tr key={tupleKey(rt)} style={virtualizeRows ? { height: PIVOT_ROW_H } : undefined}>
                {rowHeaderAt[wi].map((h, hi) => {
                  const member = rt.find((m) => m.dim === h.dim)
                  const expandable = runExpandable(h.dim, h.name)
                  // Address expansion by the occurrence's drill-path key (not its
                  // bare name) so a member under two parents toggles per occurrence.
                  const hKey = h.key ?? h.name
                  const isOpen = expanded[h.dim]?.has(hKey) ?? false
                  return (
                    <th
                      key={`${h.dim}#${hi}`}
                      className="rowhead"
                      scope="row"
                      rowSpan={h.rowSpan}
                    >
                      <span
                        className="pivot__rowhead-inner"
                        style={{ paddingInlineStart: `${(member?.depth ?? 0) * 16}px` }}
                      >
                        {expandable ? (
                          <button
                            type="button"
                            className="pivot__twisty"
                            aria-expanded={isOpen}
                            aria-label={`${isOpen ? 'Collapse' : 'Expand'} ${h.name}`}
                            onClick={() => toggleExpanded(h.dim, hKey)}
                          >
                            {isOpen ? '▾' : '▸'}
                          </button>
                        ) : (
                          <span className="pivot__twisty pivot__twisty--leaf" aria-hidden="true" />
                        )}
                        <span className="pivot__rowhead-label">{h.name}</span>
                      </span>
                    </th>
                  )
                })}
                {displayColTuples.map((ct, ci) => {
                  const cell = cells.get(`${tupleKey(rt)}||${tupleKey(ct)}`)
                  return (
                    <CellView
                      key={tupleKey(ct)}
                      cell={cell}
                      r={ri}
                      c={ci}
                      rowTuple={rt}
                      colTuple={ct}
                      spreadMode={spreadMode}
                      onCommit={commit}
                      onSpread={spread}
                      onNav={focusCell}
                      onDrill={drillInto}
                    />
                  )
                })}
              </tr>
              )
            })}
            {/* Trailing spacer: the total height of the rows below the window. */}
            {bottomSpacer > 0 ? (
              <tr aria-hidden="true" className="pivot__spacer">
                <td colSpan={cornerCols + displayColTuples.length} style={{ height: bottomSpacer, padding: 0, border: 0 }} />
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
      )}
      {drill ? (
        <Dialog
          open
          onOpenChange={(open) => {
            if (!open) setDrill(null)
          }}
          title={`How "${drill.label}" is calculated`}
          description="The value, and the stored inputs, rules, and totals it comes from."
          size="md"
        >
          {drill.trace ? (
            <div className="trace">
              <TraceView node={drill.trace} />
            </div>
          ) : (
            <p className="muted">Loading provenance...</p>
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
      <SaveViewDialog
        open={saveOpen}
        onOpenChange={setSaveOpen}
        suppressRows={suppressRows}
        suppressCols={suppressCols}
        onSave={createSavedView}
      />

      {mdxOpen ? (
        <MdxDialog cube={cube} initialMdx={buildMdx} onClose={() => setMdxOpen(false)} />
      ) : null}
    </div>
  )
}

// The "Save view" dialog, split out of PivotGrid so its controlled name field and
// visibility select hold their OWN state: a keystroke here re-renders only this
// small dialog, never the (potentially several-thousand-cell) grid body. `onSave`
// persists the current layout (owned by the parent) and rejects on failure so the
// error surfaces inline.
const SaveViewDialog = memo(function SaveViewDialog({
  open,
  onOpenChange,
  suppressRows,
  suppressCols,
  onSave,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  suppressRows: boolean
  suppressCols: boolean
  onSave: (name: string, visibility: Visibility) => Promise<void>
}) {
  const [name, setName] = useState('')
  const [visibility, setVisibility] = useState<Visibility>('private')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Reset the form each time the dialog opens so a prior attempt's name/error does
  // not linger on reopen.
  useEffect(() => {
    if (open) {
      setName('')
      setError(null)
      setBusy(false)
    }
  }, [open])

  const save = async () => {
    if (name.trim() === '') {
      setError('Name the view before saving.')
      return
    }
    setBusy(true)
    try {
      await onSave(name.trim(), visibility)
      onOpenChange(false)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not save the view')
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="Save view"
      description="Save the current rows, columns, filters, and member sets as a reusable view."
      size="sm"
    >
      <div className="pw-form">
        <label className="field">
          <span className="field__label">View name</span>
          <input
            value={name}
            placeholder="e.g. Q1 by region"
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        <label className="field">
          <span className="field__label">Who can see it</span>
          <Select
            value={visibility}
            onValueChange={(v) => setVisibility(v as Visibility)}
            options={[
              { value: 'private', label: 'Only me' },
              { value: 'public', label: 'Everyone' },
            ]}
            ariaLabel="View visibility"
          />
        </label>
        {suppressRows || suppressCols ? (
          // Zero-suppression is set from the grid toolbar (a live toggle); the
          // saved view simply captures whatever is active now.
          <p className="muted" role="note">
            This view will be saved with zero-suppression on for{' '}
            {suppressRows && suppressCols
              ? 'rows and columns'
              : suppressRows
                ? 'rows'
                : 'columns'}
            .
          </p>
        ) : null}
        {error ? (
          <p className="error" role="alert">
            {error}
          </p>
        ) : null}
        <div className="pw-form__actions">
          <Button variant="ghost" size="sm" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button size="sm" disabled={busy} onClick={() => void save()}>
            Save view
          </Button>
        </div>
      </div>
    </Dialog>
  )
})

// The "Show MDX" dialog, split out of PivotGrid so its editable query textarea and
// executed result hold their OWN state - typing in the textarea re-renders only
// this dialog, not the grid. Mounted only while open (the parent gates it), so the
// initial text is built once from the current layout via `initialMdx`.
const MdxDialog = memo(function MdxDialog({
  cube,
  initialMdx,
  onClose,
}: {
  cube: string
  initialMdx: () => string
  onClose: () => void
}) {
  const [text, setText] = useState(() => initialMdx())
  const [result, setResult] = useState<CellsetDto | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const runMdx = () => {
    setBusy(true)
    executeMdx(cube, text)
      .then((cs) => {
        setResult(cs)
        setError(null)
      })
      .catch((e) => {
        setResult(null)
        setError(e instanceof Error ? e.message : 'Could not run the query')
      })
      .finally(() => setBusy(false))
  }

  return (
    <Dialog
      open
      onOpenChange={(o) => {
        if (!o) onClose()
      }}
      title="MDX for this view"
      description="The query the current layout generates. Edit it and Run to execute against this cube."
      size="lg"
    >
      <textarea
        className="mdx-preview"
        style={{ width: '100%', resize: 'vertical' }}
        value={text}
        onChange={(e) => setText(e.target.value)}
        spellCheck={false}
        aria-label="MDX query"
        rows={8}
      />
      {error ? (
        <p className="error" role="alert">
          {error}
        </p>
      ) : null}
      <div className="pw-form__actions">
        <Button variant="ghost" size="sm" onClick={() => void navigator.clipboard?.writeText(text)}>
          Copy
        </Button>
        <Button size="sm" disabled={busy} onClick={runMdx}>
          {busy ? 'Running...' : 'Run'}
        </Button>
        <Button variant="ghost" size="sm" onClick={onClose}>
          Close
        </Button>
      </div>
      {result ? (
        <CellsetGrid
          cube={cube}
          cellset={result}
          onChanged={() => {
            executeMdx(cube, text)
              .then((cs) => setResult(cs))
              .catch(() => {})
          }}
        />
      ) : null}
    </Dialog>
  )
})

// Memoized so a PivotGrid state change unrelated to this cell (e.g. a keystroke
// in the Save-view name input or the MDX dialog textarea, both state in the
// parent) does not re-render every cell. All props are render-stable: rowTuple /
// colTuple come from memoized axis tuples, the four callbacks are useCallback'd
// in the parent, and `cell` only changes on an actual data refresh. The callbacks
// take the tuples (rather than a per-cell closure) precisely to stay stable.
const CellView = memo(function CellView({
  cell,
  r,
  c,
  rowTuple,
  colTuple,
  spreadMode,
  onCommit,
  onSpread,
  onNav,
  onDrill,
}: {
  cell: CellDto | undefined
  r: number
  c: number
  rowTuple: Tuple
  colTuple: Tuple
  spreadMode: 'off' | SpreadMethod
  onCommit: (rowTuple: Tuple, colTuple: Tuple, previous: string, next: string) => void
  onSpread: (rowTuple: Tuple, colTuple: Tuple, typed: string) => void
  onNav: (r: number, c: number) => void
  onDrill: (rowTuple: Tuple, colTuple: Tuple) => void
}) {
  const rowName = rowTuple.map((m) => m.name).join(' / ')
  const colName = colTuple.map((m) => m.name).join(' / ')
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
                onSpread(rowTuple, colTuple, e.currentTarget.value)
                e.currentTarget.value = ''
              } else if (e.key === 'Escape') {
                e.currentTarget.value = ''
                e.currentTarget.blur()
              }
            }}
            onBlur={(e) => {
              if (e.currentTarget.value.trim() !== '') onSpread(rowTuple, colTuple, e.currentTarget.value)
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
          <button type="button" className="cell-drill" onClick={() => onDrill(rowTuple, colTuple)}>
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
        onBlur={(e) => onCommit(rowTuple, colTuple, cell.value ?? '', e.currentTarget.value.trim())}
      />
    </td>
  )
})
