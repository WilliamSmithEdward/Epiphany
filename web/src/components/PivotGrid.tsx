import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  createView,
  executeMdx,
  explainCell,
  getCube,
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
import CellsetGrid from './CellsetGrid'
import PivotFields, { type AxisRole, type AxisSet } from './PivotFields'
import SubsetEditor from './SubsetEditor'
import { TraceView } from './TraceView'

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
}: {
  cube: string
  reloadSignal: number
  /** Called after the layout is saved as a View, so the explorer can refresh. */
  onModelChange?: () => void
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  // The dimensions nested on each axis, outer to inner. Each axis always holds
  // at least one dimension.
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
  // "Save view" dialog: persist the current layout as a named, shared/private View.
  const [saveOpen, setSaveOpen] = useState(false)
  const [saveName, setSaveName] = useState('')
  const [saveVis, setSaveVis] = useState<Visibility>('private')
  // Independent zero-suppression: drop all-zero rows / all-zero columns. Captured
  // into the saved view def (see buildViewDef); off by default.
  const [saveSuppressRows, setSaveSuppressRows] = useState(false)
  const [saveSuppressCols, setSaveSuppressCols] = useState(false)
  const [saveBusy, setSaveBusy] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  // "Show MDX" dialog: previews the query the current layout generates.
  const [mdxOpen, setMdxOpen] = useState(false)
  // The MDX dialog's editable query text, executed result, error, and run state.
  const [mdxText, setMdxText] = useState('')
  const [mdxResult, setMdxResult] = useState<CellsetDto | null>(null)
  const [mdxError, setMdxError] = useState<string | null>(null)
  const [mdxBusy, setMdxBusy] = useState(false)
  const gridRef = useRef<HTMLDivElement>(null)
  // Monotonic refresh generation: each refresh() bumps it and only applies its
  // own response if it is still the latest, so a slow readCells that resolves
  // after a newer refresh cannot overwrite the current cellset (request race).
  const refreshGen = useRef(0)

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
      .catch((err: unknown) =>
        setError(err instanceof Error ? err.message : 'Failed to load cube'),
      )
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

  // Default filter member for a dimension (its first element).
  const defaultMember = useCallback(
    (dimName: string) => detail?.dimensions.find((d) => d.name === dimName)?.elements[0]?.name ?? '',
    [detail],
  )

  // Re-pivot: move a dimension onto Rows, Columns, Filters, or Unused. Dropping
  // on Rows or Columns appends the dimension to that axis (nesting); the
  // dimension is first removed from wherever it currently is. Moving the last
  // dimension off an axis promotes an off-axis dimension so each axis keeps at
  // least one. Filters and Unused are both off-axis (member-pinned); the only
  // difference is whether the dimension is parked in the Unused set.
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
        // Moving the sole dimension off one axis straight onto the other would
        // empty the source axis; with no off-axis dimension free to promote (a
        // fully-on-axis cube, e.g. a plain 2-D cube), swap the two axes instead.
        const sourceIsRows = inRows
        const sourceAxis = sourceIsRows ? rowDims : inCols ? colDims : null
        const onAxisNames = new Set([...rowDims, ...colDims])
        const freeDim = detail.dimensions.map((d) => d.name).find((n) => !onAxisNames.has(n))
        if (sourceAxis && sourceAxis.length === 1 && freeDim === undefined) {
          // Pure swap: the lone source dimension trades places with the target
          // axis (whose dimensions move to the source axis).
          const newSource = target ? colDims : rowDims
          if (target) {
            setColDims(rowDims.filter((d) => d !== dim))
            setRowDims([...newSource, dim])
          } else {
            setRowDims(colDims.filter((d) => d !== dim))
            setColDims([...newSource, dim])
          }
          setUnused((u) => deleteFrom(u, dim))
          return
        }
        // Otherwise: remove it from its current home, then append to the target.
        setUnused((u) => deleteFrom(u, dim))
        const removeFromAxis = (axis: string[], setAxis: (a: string[]) => void) => {
          if (!axis.includes(dim)) return
          if (axis.length > 1) {
            setAxis(axis.filter((d) => d !== dim))
            return
          }
          // It is the only dimension on its axis: promote a free off-axis
          // dimension so the source axis stays non-empty.
          if (freeDim === undefined) return // unreachable here (swap handled above)
          setAxis([freeDim])
          setUnused((u) => deleteFrom(u, freeDim))
          setContext((c) => {
            const n = { ...c }
            delete n[freeDim]
            return n
          })
        }
        if (inRows) removeFromAxis(rowDims, setRowDims)
        if (inCols) removeFromAxis(colDims, setColDims)
        // It was off-axis: it is leaving the filters, so drop its pinned member.
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
        // Leaving an axis. If it is the last dimension on that axis, promote an
        // off-axis dimension onto the vacated axis so it stays non-empty.
        const fromRows = inRows
        const axis = fromRows ? rowDims : colDims
        if (axis.length === 1) {
          const onAxis = new Set([...rowDims, ...colDims])
          const promote = detail.dimensions.map((d) => d.name).find((n) => !onAxis.has(n))
          if (promote === undefined) return // a fully-on-axis cube needs every axis filled
          if (fromRows) setRowDims([promote])
          else setColDims([promote])
          setContext((c) => {
            const n = { ...c }
            delete n[promote]
            n[dim] = defaultMember(dim)
            return n
          })
          setUnused((u) => {
            const n = deleteFrom(u, promote)
            return role === 'unused' ? new Set(n).add(dim) : deleteFrom(n, dim)
          })
          return
        }
        // More than one dimension on the axis: just remove this one.
        if (fromRows) setRowDims((a) => a.filter((d) => d !== dim))
        else setColDims((a) => a.filter((d) => d !== dim))
        setContext((c) => ({ ...c, [dim]: defaultMember(dim) }))
        setUnused((u) => (role === 'unused' ? new Set(u).add(dim) : deleteFrom(u, dim)))
        return
      }
      // dim is already off-axis: just park or un-park it.
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
      suppress_zero_rows: saveSuppressRows,
      suppress_zero_columns: saveSuppressCols,
    }
  }, [detail, rowDims, colDims, context, axisSet, saveSuppressRows, saveSuppressCols])

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

  const refresh = useCallback(async () => {
    if (!detail || rowDims.length === 0 || colDims.length === 0) return
    if (rowTuples.length === 0 || colTuples.length === 0) {
      // Nothing to fetch (an empty axis): clear any prior slice's cells and end any
      // in-flight busy state so the "No data" empty state is not shown dimmed.
      setCells(new Map())
      setRefreshing(false)
      return
    }
    const coords: Coord[] = []
    for (const rt of rowTuples) {
      for (const ct of colTuples) {
        coords.push(coordFor(rt, ct))
      }
    }
    const gen = (refreshGen.current += 1)
    setRefreshing(true)
    try {
      const fetched = await readCells(cube, coords)
      // Ignore a stale response: a newer refresh started while this one was in
      // flight, so applying these cells would paint older data over the current
      // layout (the row/col keys can still match, e.g. on a context change).
      if (gen !== refreshGen.current) return
      const next = new Map<string, CellDto>()
      let i = 0
      for (const rt of rowTuples) {
        const rk = tupleKey(rt)
        for (const ct of colTuples) {
          next.set(`${rk}||${tupleKey(ct)}`, fetched[i])
          i += 1
        }
      }
      setCells(next)
      setError(null)
      setRefreshing(false)
    } catch (err) {
      if (gen !== refreshGen.current) return
      setError(err instanceof Error ? err.message : 'Failed to read cells')
      setRefreshing(false)
    }
  }, [cube, detail, rowDims, colDims, coordFor, rowTuples, colTuples])

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
    return <p className="banner" role="status">Loading {cube}...</p>
  }

  const editorDimDto = subsetEditorDim
    ? (detail.dimensions.find((d) => d.name === subsetEditorDim) ?? null)
    : null

  // computeHeaderSpans reads only {dimension, name, key}; map each tuple to that
  // shape once, for both axes.
  const headerTuples = (tuples: Tuple[]) =>
    tuples.map((t) => t.map((m) => ({ dimension: m.dim, name: m.name, key: m.key })))
  // Nested column headers: one row per column-axis level, run-length merged.
  const colHeader = computeHeaderSpans(headerTuples(colTuples))
  // For each body row, the row-header cells that start a run at that row, per
  // row-axis level (mirrors the CellsetGrid rowSpan technique).
  const rowSpans = computeHeaderSpans(headerTuples(rowTuples))
  const rowHeaderAt: { dim: string; name: string; key?: string; rowSpan: number; startIndex: number }[][] =
    rowTuples.map(() => [])
  for (let level = 0; level < rowDims.length; level++) {
    let r = 0
    for (const run of rowSpans[level] ?? []) {
      rowHeaderAt[r].push({ dim: run.dimension, name: run.name, key: run.key, rowSpan: run.span, startIndex: r })
      r += run.span
    }
  }

  // Whether a header run's member can be drilled into within its dimension.
  const runExpandable = (dim: string, name: string) =>
    isHierarchical(dim) && (forests.get(dim)?.childrenOf.has(name) ?? false)

  const cornerCols = Math.max(1, rowDims.length)
  const colLevels = colDims.length
  const cornerLabel = `${rowDims.join(' / ')} / ${colDims.join(' / ')}`

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
        <span className="grid-toolbar__spacer" />
        <Button
          variant="ghost"
          size="sm"
          icon="◫"
          onClick={() => {
            setSaveError(null)
            setSaveSuppressRows(false)
            setSaveSuppressCols(false)
            setSaveOpen(true)
          }}
        >
          Save view
        </Button>
        <Button
          variant="ghost"
          size="sm"
          icon="∑"
          onClick={() => {
            setMdxText(buildMdx())
            setMdxResult(null)
            setMdxError(null)
            setMdxOpen(true)
          }}
        >
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
      {/* Polite live status so a re-query (any refresh past the first load) is
          announced to assistive tech, not just visually. */}
      <div className="sr-only" role="status" aria-live="polite">
        {refreshing ? 'Refreshing values...' : ''}
      </div>
      <div
        className="grid-wrap"
        ref={gridRef}
        aria-busy={refreshing || undefined}
        // Dim the currently-painted cells while a refresh is in flight so a user
        // never reads a previous-slice number AS the current slice's value (the
        // tuple keys are unchanged on a context/filter change, so the stale numbers
        // would otherwise stay crisply painted until readCells resolves).
        style={refreshing ? { opacity: 0.5, transition: 'opacity 120ms' } : undefined}
      >
        <table className="pivot">
          <thead>
            {colLevels === 0 ? (
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
            {rowTuples.length === 0 ? (
              // An empty row axis (e.g. an applied set resolved to no members)
              // would otherwise render a bare header-only table; show an explicit
              // "No data" row so the state reads as intentional, not broken.
              <tr>
                <td className="pivot__empty muted" colSpan={cornerCols + Math.max(1, colTuples.length)}>
                  No data to show. Adjust the filters or member sets on the rows axis.
                </td>
              </tr>
            ) : null}
            {rowTuples.map((rt, ri) => (
              <tr key={tupleKey(rt)}>
                {rowHeaderAt[ri].map((h, hi) => {
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
                {colTuples.map((ct, ci) => {
                  const cell = cells.get(`${tupleKey(rt)}||${tupleKey(ct)}`)
                  return (
                    <CellView
                      key={tupleKey(ct)}
                      cell={cell}
                      r={ri}
                      c={ci}
                      rowName={rt.map((m) => m.name).join(' / ')}
                      colName={ct.map((m) => m.name).join(' / ')}
                      spreadMode={spreadMode}
                      onCommit={(next) => void commit(rt, ct, cell?.value ?? '', next)}
                      onSpread={(next) => void spread(rt, ct, next)}
                      onNav={focusCell}
                      onDrill={() => void drillInto(rt, ct)}
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
          <label className="check">
            <input
              type="checkbox"
              checked={saveSuppressRows}
              onChange={(e) => setSaveSuppressRows(e.target.checked)}
            />
            <span>Suppress zero rows</span>
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={saveSuppressCols}
              onChange={(e) => setSaveSuppressCols(e.target.checked)}
            />
            <span>Suppress zero columns</span>
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
        description="The query the current layout generates. Edit it and Run to execute against this cube."
        size="lg"
      >
        <textarea
          className="mdx-preview"
          style={{ width: '100%', resize: 'vertical' }}
          value={mdxText}
          onChange={(e) => setMdxText(e.target.value)}
          spellCheck={false}
          aria-label="MDX query"
          rows={8}
        />
        {mdxError ? (
          <p className="error" role="alert">
            {mdxError}
          </p>
        ) : null}
        <div className="pw-form__actions">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void navigator.clipboard?.writeText(mdxText)}
          >
            Copy
          </Button>
          <Button
            size="sm"
            disabled={mdxBusy}
            onClick={() => {
              setMdxBusy(true)
              executeMdx(cube, mdxText)
                .then((cs) => {
                  setMdxResult(cs)
                  setMdxError(null)
                })
                .catch((e) => {
                  setMdxResult(null)
                  setMdxError(e instanceof Error ? e.message : 'Could not run the query')
                })
                .finally(() => setMdxBusy(false))
            }}
          >
            {mdxBusy ? 'Running...' : 'Run'}
          </Button>
          <Button variant="ghost" size="sm" onClick={() => setMdxOpen(false)}>
            Close
          </Button>
        </div>
        {mdxResult ? (
          <CellsetGrid
            cube={cube}
            cellset={mdxResult}
            onChanged={() => {
              executeMdx(cube, mdxText)
                .then((cs) => setMdxResult(cs))
                .catch(() => {})
            }}
          />
        ) : null}
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
