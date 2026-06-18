import * as DM from '@radix-ui/react-dropdown-menu'
import {
  useCallback,
  useDeferredValue,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from 'react'
import {
  ApiError,
  getCube,
  getDimension,
  listConnections,
  listCubes,
  listDimensions,
  listFlows,
  listSchedules,
  listViews,
  type DimensionDto,
  type SharedDimensionSummary,
} from '../api/client'
import { buildElementTree, type TreeNode } from '../model/tree'

// What the tree currently has open in the detail pane. Cube-owned objects (its
// dimensions, views, rules+feeders) nest under the cube; the global dimension
// namespace, flows, schedules, and connections are their own server-global
// resource-type roots, listed directly (no cube layer; ADR-0035).
export type Selection =
  | { kind: 'cube'; cube: string } // the cube's data (pivot grid)
  | { kind: 'cube-dimension'; cube: string; dim: string } // a cube's dimension
  | { kind: 'cube-views'; cube: string } // the cube's views manager
  | { kind: 'view'; cube: string; view: string } // one saved view, opened
  | { kind: 'cube-rules'; cube: string } // the cube's rules + feeders
  | { kind: 'dimension'; id: number; name: string } // a global (registry) dimension
  | { kind: 'flow'; flow: string } // a global flow (ADR-0035)
  | { kind: 'schedule'; schedule: string } // a global schedule (ADR-0035)
  | { kind: 'connection'; connection: string } // a global connection (ADR-0035)
  | { kind: 'overview' }
  | { kind: 'security' }

// ---- node actions (context menus) ----

/** The names of the context-menu actions a node can offer. Only backend-supported
 * operations appear; the model is append-only, so there is no rename and no
 * delete of cubes, cube dimensions, or elements. */
export type NodeAction =
  | 'new-cube'
  | 'open-model'
  | 'add-view'
  | 'open-view'
  | 'delete-view'
  | 'add-member'
  | 'add-rollup'
  | 'edit-attributes'
  | 'manage-sets'
  | 'promote-dimension'
  | 'open-rules'
  | 'register-dimension'
  | 'grow-dimension'
  | 'delete-dimension'
  | 'new-flow'
  | 'open-flow'
  | 'run-flow'
  | 'delete-flow'
  | 'new-schedule'
  | 'open-schedule'
  | 'run-schedule'
  | 'delete-schedule'
  | 'open-connections'
  | 'delete-connection'

/** Everything an action handler may need; fields are filled per node kind. */
export interface ActionContext {
  cube?: string
  dim?: string
  view?: string
  flow?: string
  job?: string
  connection?: string
  dimId?: number
}

/** One entry in a node's context menu. */
export interface NodeMenuItem {
  action: NodeAction
  label: string
  danger?: boolean
  disabled?: boolean
}

/** A node in the object-explorer tree. A `loader` makes it expandable (lazy). */
interface Node {
  id: string
  label: string
  icon: string
  /** A short qualifier shown after the label (e.g. a reference count "2", or a
   * provenance cube name). `badgeTitle` is its tooltip / accessible expansion. */
  badge?: string
  badgeTitle?: string
  /** What activating this node opens; absent for pure grouping nodes. */
  selection?: Selection
  /** Present => the node is expandable; called once on first expand. */
  loader?: () => Promise<Node[]>
  /** The context-menu actions for this node, with the context to dispatch. */
  menu?: NodeMenuItem[]
  /** The context passed to `onAction` for every item in `menu`. */
  actionCtx?: ActionContext
  /** A non-interactive status/placeholder row (e.g. "No flows", "Failed to
   * load"): rendered as role="none" and excluded from the roving-tabindex set
   * so arrow navigation and Enter/Space never land on it. */
  info?: true
}

function selectionId(s: Selection): string {
  switch (s.kind) {
    case 'cube':
      return `cube:${s.cube}`
    case 'cube-dimension':
      return `cube:${s.cube}/dim:${s.dim}`
    case 'cube-views':
      return `cube:${s.cube}/views`
    case 'view':
      return `cube:${s.cube}/views/${s.view}`
    case 'cube-rules':
      return `cube:${s.cube}/rules`
    case 'dimension':
      return `dim:${s.id}`
    case 'flow':
      return `flow:${s.flow}`
    case 'schedule':
      return `sched:${s.schedule}`
    case 'connection':
      return `conn:${s.connection}`
    case 'overview':
      return 'overview'
    case 'security':
      return 'security'
  }
}

// ---- node builders (each returns the children for a parent) ----

/** Build the explorer nodes for a dimension's members, following the
 * consolidation hierarchy: a consolidated member (a roll-up like a "Total")
 * expands to the members beneath it; a leaf has no children. Roots are the
 * members with no parent, so a flat dimension shows its members flat and a
 * dimension with roll-ups shows the roll-ups, drillable into their inputs. A
 * member reachable under two roll-ups (alternate hierarchies) appears under
 * each, kept distinct by its path-prefixed id. */
function elementTreeNodes(
  parentId: string,
  tree: TreeNode[],
  menu?: NodeMenuItem[],
  actionCtx?: ActionContext,
): Node[] {
  return tree.map((t) => {
    const id = `${parentId}/el:${t.name}`
    return {
      id,
      label: t.name,
      icon: t.kind === 'consolidated' ? '◇' : t.kind === 'string' ? '"' : '·',
      menu,
      actionCtx,
      loader:
        t.children.length > 0
          ? async () => elementTreeNodes(id, t.children, menu, actionCtx)
          : undefined,
    }
  })
}

/** The shared context-menu action for a cube dimension (the "Dimensions" group
 * node and each individual cube dimension open the same editor). The editor
 * shows the member/roll-up/attribute forms together, so a single "Edit
 * dimension…" item that lands there is honest about the destination rather than
 * three labels that all open the same multi-section pane at its top. */
const CUBE_DIM_MENU: NodeMenuItem[] = [
  { action: 'edit-attributes', label: 'Edit dimension…' },
  { action: 'manage-sets', label: 'Manage sets…' },
]

async function cubeChildren(cube: string): Promise<Node[]> {
  const detail = await getCube(cube)
  const dims: Node = {
    id: `cube:${cube}/dims`,
    label: 'Dimensions',
    icon: '⬡',
    menu: CUBE_DIM_MENU,
    actionCtx: { cube, dim: detail.dimensions[0]?.name },
    loader: async () =>
      detail.dimensions.map((d) => ({
        id: `cube:${cube}/dim:${d.name}`,
        label: d.name,
        icon: '⬡',
        selection: { kind: 'cube-dimension', cube, dim: d.name },
        menu: CUBE_DIM_MENU,
        actionCtx: { cube, dim: d.name },
        loader: async () =>
          elementTreeNodes(`cube:${cube}/dim:${d.name}`, buildElementTree(d), CUBE_DIM_MENU, {
            cube,
            dim: d.name,
          }),
      })),
  }
  const views: Node = {
    id: `cube:${cube}/views`,
    label: 'Views',
    icon: '◫',
    selection: { kind: 'cube-views', cube },
    menu: [{ action: 'add-view', label: 'Add view…' }],
    actionCtx: { cube },
    loader: async () => {
      const vs = await listViews(cube)
      return vs.map((v) => ({
        id: `cube:${cube}/views/${v.name}`,
        label: v.name,
        icon: '◫',
        selection: { kind: 'view', cube, view: v.name },
        menu: [
          { action: 'open-view', label: 'Open' },
          { action: 'delete-view', label: 'Delete…', danger: true },
        ],
        actionCtx: { cube, view: v.name },
      }))
    },
  }
  const rules: Node = {
    id: `cube:${cube}/rules`,
    label: 'Rules & feeders',
    icon: 'Σ',
    selection: { kind: 'cube-rules', cube },
    menu: [{ action: 'open-rules', label: 'Edit rules & feeders' }],
    actionCtx: { cube },
  }
  return [dims, views, rules]
}

function cubeNode(name: string): Node {
  return {
    id: `cube:${name}`,
    label: name,
    icon: '▤',
    selection: { kind: 'cube', cube: name },
    // A superset of the actions its child nodes expose, so the cube row offers
    // a direct path to its structure (consistency / recognition over recall).
    menu: [
      { action: 'open-model', label: 'Edit dimensions…' },
      { action: 'add-view', label: 'Add view…' },
    ],
    actionCtx: { cube: name },
    loader: () => cubeChildren(name),
  }
}

/** Flows are server-global (ADR-0035): listed directly, with no cube layer. */
async function flowNodes(): Promise<Node[]> {
  const flows = await listFlows()
  return flows.length === 0
    ? [{ id: 'flows/none', label: 'No flows yet', icon: ' ', info: true } as Node]
    : flows.map((f) => ({
        id: `flow:${f.name}`,
        label: f.name,
        icon: '⇄',
        selection: { kind: 'flow', flow: f.name } as Selection,
        menu: [
          { action: 'open-flow', label: 'Open' },
          { action: 'run-flow', label: 'Run' },
          { action: 'delete-flow', label: 'Delete…', danger: true },
        ] as NodeMenuItem[],
        actionCtx: { flow: f.name },
      }))
}

/** Schedules are server-global (ADR-0035): listed directly, with no cube layer. */
async function scheduleNodes(): Promise<Node[]> {
  const schedules = await listSchedules()
  return schedules.length === 0
    ? [{ id: 'sched/none', label: 'No schedules yet', icon: ' ', info: true } as Node]
    : schedules.map((j) => ({
        id: `sched:${j.name}`,
        label: j.name,
        icon: '⏱',
        selection: { kind: 'schedule', schedule: j.name } as Selection,
        menu: [
          { action: 'open-schedule', label: 'Edit' },
          { action: 'run-schedule', label: 'Run now' },
          { action: 'delete-schedule', label: 'Delete…', danger: true },
        ] as NodeMenuItem[],
        actionCtx: { job: j.name },
      }))
}

/** Connections are server-global (ADR-0035; admin-only root). Each opens the
 * global connections panel; delete dispatches a confirmed removal. */
async function connectionNodes(): Promise<Node[]> {
  const connections = await listConnections()
  return connections.length === 0
    ? [{ id: 'conn/none', label: 'No connections yet', icon: ' ', info: true } as Node]
    : connections.map((c) => ({
        id: `conn:${c.name}`,
        label: c.name,
        icon: '⇲',
        badge: c.kind,
        badgeTitle: `${c.kind} connection`,
        selection: { kind: 'connection', connection: c.name } as Selection,
        menu: [
          { action: 'open-connections', label: 'Manage' },
          { action: 'delete-connection', label: 'Delete…', danger: true },
        ] as NodeMenuItem[],
        actionCtx: { connection: c.name },
      }))
}

/** The "Dimensions" section: one global namespace (ADR-0031). Lists the registry
 * (global) dimensions, then every cube's embedded-only dimensions, presented
 * together. A registry dimension routes to the dimension editor and is
 * referenceable by any cube; an embedded-only one routes to its cube's model
 * editor and shows that cube as provenance. Cubes the caller cannot read are
 * skipped (their getCube 403s), so the list never leaks a denied cube. */
async function dimensionNamespace(): Promise<Node[]> {
  const registryNode = (d: SharedDimensionSummary): Node => ({
    id: `dim:${d.id}`,
    label: d.name,
    icon: '⬡',
    // The reference count (how many cubes use it), not a shared/local flag.
    badge: d.references.length > 0 ? String(d.references.length) : undefined,
    badgeTitle:
      d.references.length === 1
        ? 'Used by 1 cube'
        : `Used by ${d.references.length} cubes`,
    selection: { kind: 'dimension', id: d.id, name: d.name },
    // Delete is only offered when the dimension is unreferenced; the backend
    // rejects deleting a referenced one (409) anyway.
    menu: [
      { action: 'add-member', label: 'Add member…' },
      { action: 'grow-dimension', label: 'Grow…' },
      {
        action: 'delete-dimension',
        label: 'Delete…',
        danger: true,
        disabled: d.references.length > 0,
      },
    ],
    actionCtx: { dimId: d.id, dim: d.name },
    loader: async () => {
      const detail = await getDimension(d.id)
      return elementTreeNodes(
        `dim:${d.id}`,
        buildElementTree(detail),
        [
          { action: 'add-member', label: 'Add member…' },
          { action: 'grow-dimension', label: 'Grow…' },
        ],
        { dimId: d.id, dim: d.name },
      )
    },
  })

  const embeddedNode = (cube: string, d: DimensionDto): Node => ({
    id: `cube:${cube}/dim:${d.name}`,
    label: d.name,
    icon: '⬡',
    selection: { kind: 'cube-dimension', cube, dim: d.name },
    // An embedded dimension can be promoted into the global registry so other
    // cubes can reference it (ADR-0031 Phase 1), alongside the usual edit/sets.
    menu: [...CUBE_DIM_MENU, { action: 'promote-dimension', label: 'Reuse in other cubes…' }],
    actionCtx: { cube, dim: d.name },
    loader: async () =>
      elementTreeNodes(`cube:${cube}/dim:${d.name}`, buildElementTree(d), CUBE_DIM_MENU, {
        cube,
        dim: d.name,
      }),
  })

  const [registry, cubes] = await Promise.all([listDimensions(), listCubes()])
  // Read each cube's dimensions. A cube the caller cannot read (403) is
  // legitimately skipped so the list never leaks a denied cube; any other failure
  // (500 / network) is rethrown so the tree shows "Failed to load" rather than
  // silently dropping that cube's dimensions and implying the list is complete.
  const details = await Promise.all(
    cubes.map((c) =>
      getCube(c.name).then(
        (detail) => ({ cube: c.name, detail }),
        (err: unknown) => {
          if (err instanceof ApiError && err.status === 403) return null
          throw err
        },
      ),
    ),
  )
  const embedded: Node[] = []
  for (const entry of details) {
    if (!entry) continue
    for (const d of entry.detail.dimensions) {
      if (d.id !== undefined) continue // registry-backed: shown via its registry entry
      embedded.push(embeddedNode(entry.cube, d))
    }
  }
  return [...registry.map(registryNode), ...embedded]
}

/** The static top-level roots (resource-type grouping). */
function rootNodes(isAdmin: boolean): Node[] {
  const roots: Node[] = [
    {
      id: 'root:cubes',
      label: 'Cubes',
      icon: '▤',
      menu: isAdmin ? [{ action: 'new-cube', label: 'New cube…' }] : undefined,
      actionCtx: {},
      loader: async () => (await listCubes()).map((c) => cubeNode(c.name)),
    },
    {
      id: 'root:dimensions',
      label: 'Dimensions',
      icon: '⬡',
      menu: [{ action: 'register-dimension', label: 'New dimension…' }],
      actionCtx: {},
      // Global dimension namespace (ADR-0031): one list = the registry (global)
      // dimensions plus every cube's embedded-only dimensions, presented
      // together with no shared/local distinction. A registry-backed cube
      // dimension carries an id and is shown once via its registry entry; an
      // embedded-only one (no id) is shown with its cube as provenance and opens
      // that cube's model editor.
      loader: dimensionNamespace,
    },
    {
      id: 'root:flows',
      label: 'Flows',
      icon: '⇄',
      menu: [{ action: 'new-flow', label: 'New flow…' }],
      actionCtx: {},
      // Flows are server-global (ADR-0035): listed directly, no cube layer.
      loader: flowNodes,
    },
    {
      id: 'root:schedules',
      label: 'Schedules',
      icon: '⏱',
      menu: [{ action: 'new-schedule', label: 'New schedule…' }],
      actionCtx: {},
      // Schedules are server-global (ADR-0035): listed directly, no cube layer.
      loader: scheduleNodes,
    },
  ]
  // Connections are server-global operator configuration (ADR-0035); the root
  // is admin-only (the non-admin tree never shows connector internals).
  if (isAdmin) {
    roots.push({
      id: 'root:connections',
      label: 'Connections',
      icon: '⇲',
      menu: [{ action: 'open-connections', label: 'Manage connections' }],
      actionCtx: {},
      loader: connectionNodes,
    })
  }
  // Administration is not a model object, so it no longer lives in the tree; it
  // opens from a top-bar button (admin only) into its own view (see CubeApp).
  return roots
}

export default function ModelExplorer({
  selection,
  onSelect,
  isAdmin,
  reloadSignal,
  onAction,
}: {
  selection: Selection | null
  onSelect: (s: Selection) => void
  isAdmin: boolean
  reloadSignal: number
  /** Dispatch a context-menu action; the context names the affected object. */
  onAction?: (action: NodeAction, ctx: ActionContext) => void
}) {
  const roots = useMemo(() => rootNodes(isAdmin), [isAdmin])
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set(['root:cubes']))
  const [childrenById, setChildren] = useState<Record<string, Node[]>>({})
  const [loading, setLoading] = useState<Set<string>>(() => new Set())
  // Per-node load failures, kept separate from childrenById so a collapse/
  // re-expand (or a Retry) re-runs the loader instead of caching a dead row.
  const [errorById, setErrors] = useState<Record<string, string>>({})
  // A single global search across every object in the tree (cubes, dimensions,
  // members, views, flows, schedules), not a cube-only filter.
  const [query, setQuery] = useState('')
  const selectedId = selection ? selectionId(selection) : null
  const [focusId, setFocusId] = useState<string>('root:cubes')
  // The id of the row whose context menu is currently open (only one at a time).
  const [menuOpenId, setMenuOpenId] = useState<string | null>(null)
  // A polite live-region message announcing lazy-load progress / failures.
  const [liveMsg, setLiveMsg] = useState('')
  const treeRef = useRef<HTMLUListElement>(null)
  // Type-ahead buffer (printable chars) with a ~500ms expiry, held in a ref so
  // it survives renders without retriggering the key handler's useCallback.
  const typeahead = useRef<{ keys: string; t: number }>({ keys: '', t: 0 })

  const runAction = useCallback(
    (action: NodeAction, ctx: ActionContext) => {
      setMenuOpenId(null)
      onAction?.(action, ctx)
    },
    [onAction],
  )

  // Return DOM focus to a tree row by id (used after a menu closes, so focus
  // never lands on the invisible "⋯" trigger).
  const focusRow = useCallback((id: string) => {
    const el = treeRef.current?.querySelector<HTMLElement>(`[data-node-id="${cssEscape(id)}"]`)
    el?.focus()
  }, [])

  const load = useCallback((node: Node) => {
    if (!node.loader) return
    setLoading((s) => new Set(s).add(node.id))
    setErrors((m) => {
      if (!(node.id in m)) return m
      const n = { ...m }
      delete n[node.id]
      return n
    })
    setLiveMsg(`Loading ${node.label}…`)
    node
      .loader()
      .then((kids) => {
        setChildren((m) => ({ ...m, [node.id]: kids }))
        setLiveMsg(`${node.label} loaded`)
      })
      .catch((err) => {
        // Keep the failure out of childrenById so the line-392 re-expand guard
        // (`node.loader && !childrenById[node.id]`) re-fetches on collapse+expand.
        const msg = err instanceof Error ? err.message : String(err)
        setErrors((m) => ({ ...m, [node.id]: msg }))
        setLiveMsg(`Failed to load ${node.label}: ${msg}`)
      })
      .finally(() => setLoading((s) => { const n = new Set(s); n.delete(node.id); return n }))
  }, [])

  // Defer the heavy filter work off the keystroke so typing stays responsive: the
  // input shows `query` immediately while the (potentially large) tree filter and
  // re-render track the deferred value.
  const deferredQuery = useDeferredValue(query)
  const trimmed = deferredQuery.trim()
  const searching = trimmed !== ''
  const q = trimmed.toLowerCase()

  // While a search is active, eagerly load every still-unloaded expandable node
  // so the filter sees the whole model (the tree is otherwise lazy). It cascades:
  // as a node's children arrive, childrenById changes and the next level loads,
  // up to convergence. A ref of already-scheduled ids keeps it idempotent, so the
  // effect doesn't re-dispatch on every loading/error toggle (only on new
  // children arriving) and can't double-fetch; it resets when the search clears.
  const scheduledRef = useRef<Set<string>>(new Set())
  useEffect(() => {
    if (!searching) {
      scheduledRef.current.clear()
      return
    }
    for (const n of collectLoaders(roots, childrenById)) {
      if (n.id in childrenById || scheduledRef.current.has(n.id)) continue
      scheduledRef.current.add(n.id)
      load(n)
    }
  }, [searching, roots, childrenById, load])

  // The search result set over the loaded tree: every node whose label matches,
  // plus the ancestor chain leading to it (so a match shows under its rollups and
  // section). `searchExpand` is the set of ancestors to force-open.
  const { keepIds, searchExpand } = useMemo(() => {
    const keep = new Set<string>()
    const expand = new Set<string>()
    if (!searching) return { keepIds: keep, searchExpand: expand }
    const walk = (nodes: Node[]): boolean => {
      let any = false
      for (const n of nodes) {
        if (n.info) continue
        const selfMatch = n.label.toLowerCase().includes(q)
        const kids = childrenById[n.id]
        const childMatch = kids ? walk(kids) : false
        if (selfMatch || childMatch) {
          keep.add(n.id)
          if (childMatch) expand.add(n.id)
          any = true
        }
      }
      return any
    }
    walk(roots)
    return { keepIds: keep, searchExpand: expand }
  }, [searching, q, roots, childrenById])

  // Reload the children of any currently-expanded node when the model changes
  // (a write bumps reloadSignal), so the tree stays in sync after create/delete.
  useEffect(() => {
    if (reloadSignal === 0) return
    const reloadable = collectLoaders(roots, childrenById).filter((n) => expanded.has(n.id))
    for (const n of reloadable) load(n)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadSignal])

  const toggle = useCallback(
    (node: Node) => {
      setExpanded((s) => {
        const n = new Set(s)
        if (n.has(node.id)) {
          n.delete(node.id)
          // If a descendant of the collapsing node holds focus, move it up to
          // the node being collapsed (ids are path-prefixed, e.g.
          // `cube:Sales/dim:Region`) so the roving tabindex stays visible.
          setFocusId((f) => (f.startsWith(node.id + '/') ? node.id : f))
          // Drop any stale load error so re-expand re-runs the loader.
          setErrors((m) => {
            if (!(node.id in m)) return m
            const c = { ...m }
            delete c[node.id]
            return c
          })
        } else {
          n.add(node.id)
          if (node.loader && !childrenById[node.id]) load(node)
        }
        return n
      })
    },
    [childrenById, load],
  )

  const activate = useCallback(
    (node: Node) => {
      if (node.selection) onSelect(node.selection)
      if (node.loader) toggle(node)
    },
    [onSelect, toggle],
  )

  // The flat list of currently-visible, navigable nodes (for roving-tabindex
  // keyboard nav). Non-interactive status rows (`info`) are excluded so arrows
  // and Enter/Space never land on them.
  const visible = useMemo(
    () => flatten(roots, searching ? searchExpand : expanded, childrenById, searching, keepIds),
    [roots, expanded, childrenById, searching, searchExpand, keepIds],
  )

  // Self-healing roving tabindex: if the focused node leaves the visible set
  // (after a collapse or a filter change), re-home focus to the first visible
  // node so the tree always has exactly one tab stop (WCAG 2.4.3).
  useEffect(() => {
    if (visible.length === 0) return
    if (!visible.some((v) => v.node.id === focusId)) setFocusId(visible[0].node.id)
  }, [visible, focusId])

  const onKeyDown = useCallback(
    (e: ReactKeyboardEvent, node: Node) => {
      const idx = visible.findIndex((v) => v.node.id === node.id)
      // Match the rendered open state: during a search, expansion is driven by
      // searchExpand, not the user's manual `expanded` set, and is not manually
      // toggleable (the search controls it), so arrows only move focus then.
      const isOpen = searching ? searchExpand.has(node.id) : expanded.has(node.id)
      const hasMenu = Boolean(onAction && node.menu && node.menu.length > 0)
      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault()
          if (idx < visible.length - 1) setFocusId(visible[idx + 1].node.id)
          break
        case 'ArrowUp':
          e.preventDefault()
          if (idx > 0) setFocusId(visible[idx - 1].node.id)
          break
        case 'ArrowRight':
          e.preventDefault()
          if (!searching && node.loader && !isOpen) toggle(node)
          else if (isOpen && idx < visible.length - 1) setFocusId(visible[idx + 1].node.id)
          break
        case 'ArrowLeft':
          e.preventDefault()
          if (!searching && node.loader && isOpen) toggle(node)
          else {
            const parent = visible[idx]?.parentId
            if (parent) setFocusId(parent)
          }
          break
        case 'Home':
          e.preventDefault()
          if (visible.length > 0) setFocusId(visible[0].node.id)
          break
        case 'End':
          e.preventDefault()
          if (visible.length > 0) setFocusId(visible[visible.length - 1].node.id)
          break
        case 'Enter':
        case ' ':
          e.preventDefault()
          activate(node)
          break
        case 'ContextMenu':
          if (hasMenu) {
            e.preventDefault()
            setFocusId(node.id)
            setMenuOpenId(node.id)
          }
          break
        case 'F10':
          if (e.shiftKey && hasMenu) {
            e.preventDefault()
            setFocusId(node.id)
            setMenuOpenId(node.id)
          }
          break
        default: {
          // Type-ahead: a printable char jumps to the next visible node whose
          // label starts with the accumulated buffer (~500ms window).
          if (e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
            e.preventDefault()
            const now = Date.now()
            const buf = typeahead.current
            buf.keys = now - buf.t > 500 ? e.key : buf.keys + e.key
            buf.t = now
            const needle = buf.keys.toLowerCase()
            const order = [
              ...visible.slice(idx + 1),
              ...visible.slice(0, idx + 1),
            ]
            const hit = order.find((v) => v.node.label.toLowerCase().startsWith(needle))
            if (hit) setFocusId(hit.node.id)
          }
        }
      }
    },
    [visible, expanded, searching, searchExpand, toggle, activate, onAction],
  )

  // Keep DOM focus on the roving-tabindex node after keyboard navigation.
  useEffect(() => {
    const el = treeRef.current?.querySelector<HTMLElement>(`[data-node-id="${cssEscape(focusId)}"]`)
    if (el && treeRef.current?.contains(document.activeElement)) el.focus()
  }, [focusId])

  function renderNodes(nodes: Node[], depth: number, parentId: string | null): ReactNode {
    const list = searching ? searchVisibleChildren(nodes, parentId, keepIds) : nodes
    // Zero-results at the root: the search matched nothing across the whole tree.
    if (searching && parentId === null && list.length === 0) {
      return (
        <li role="none" className="tree__empty" style={{ paddingInlineStart: `${depth * 14 + 8}px` }}>
          No objects match &ldquo;{trimmed}&rdquo;
        </li>
      )
    }
    return list.map((node) => {
      // Non-interactive status placeholders ("No flows yet", etc.): a plain
      // role="none" row, never a focusable/selectable treeitem.
      if (node.info) {
        return (
          <li role="none" key={node.id} className="tree__empty" style={{ paddingInlineStart: `${depth * 14 + 8}px` }}>
            {node.label}
          </li>
        )
      }
      const isOpen = searching ? searchExpand.has(node.id) : expanded.has(node.id)
      const isSel = node.id === selectedId
      // While searching, only the ancestors of matches are expandable (they hold
      // matching descendants); a matched dead-end row shows no twisty so it never
      // presents an expand affordance that, under the search filter, reveals
      // nothing. Outside search, anything with a loader is expandable.
      const expandable = Boolean(node.loader) && (!searching || searchExpand.has(node.id))
      const selectable = Boolean(node.selection)
      const hasMenu = Boolean(onAction && node.menu && node.menu.length > 0)
      const isLoading = loading.has(node.id)
      const loadError = errorById[node.id]
      const accLabel = node.badge ? `${node.label}, ${node.badgeTitle ?? node.badge}` : node.label
      return (
        <li
          key={node.id}
          role="treeitem"
          aria-expanded={expandable ? isOpen : undefined}
          aria-selected={selectable ? isSel : undefined}
          aria-level={depth + 1}
          aria-busy={expandable && isOpen && isLoading ? true : undefined}
          aria-label={node.badge ? accLabel : undefined}
          data-node-id={node.id}
          tabIndex={focusId === node.id ? 0 : -1}
          onClick={(e) => { e.stopPropagation(); setFocusId(node.id); activate(node) }}
          onKeyDown={(e) => onKeyDown(e, node)}
          onContextMenu={
            hasMenu
              ? (e) => {
                  e.preventDefault()
                  setFocusId(node.id)
                  setMenuOpenId(node.id)
                }
              : undefined
          }
        >
          <div
            className={`tree__row${isSel ? ' is-selected' : ''}`}
            style={{ paddingInlineStart: `${depth * 14 + 8}px` }}
          >
            <span className="tree__twisty" aria-hidden="true" onClick={(e) => { e.stopPropagation(); setFocusId(node.id); toggle(node) }}>
              {expandable ? (isOpen ? '▾' : '▸') : ''}
            </span>
            <span className="tree__icon" aria-hidden="true">{node.icon}</span>
            <span className="tree__label">{node.label}</span>
            {node.badge ? (
              <span className="tree__badge" title={node.badgeTitle ?? node.badge} aria-hidden="true">
                {node.badge}
              </span>
            ) : null}
            {isLoading ? <span className="tree__spinner" aria-hidden="true">…</span> : null}
            {hasMenu ? (
              <RowMenu
                node={node}
                open={menuOpenId === node.id}
                onOpenChange={(o) => setMenuOpenId(o ? node.id : null)}
                onAction={runAction}
                onCloseAutoFocus={() => focusRow(node.id)}
              />
            ) : null}
          </div>
          {expandable && isOpen ? (
            <ul role="group">
              {loadError ? (
                <li role="none" className="tree__empty tree__error" style={{ paddingInlineStart: `${(depth + 1) * 14 + 8}px` }}>
                  <span className="tree__error-text" title={loadError}>
                    Failed to load {node.label}: {loadError}
                  </span>
                  <button
                    type="button"
                    className="tree__retry"
                    onClick={(e) => { e.stopPropagation(); load(node) }}
                  >
                    Retry
                  </button>
                </li>
              ) : childrenById[node.id] ? (
                renderNodes(childrenById[node.id], depth + 1, node.id)
              ) : null}
            </ul>
          ) : null}
        </li>
      )
    })
  }

  return (
    <nav className="tree" aria-label="Model explorer">
      <div className="tree__toolbar" role="search">
        <input
          type="search"
          className="tree__search-input"
          value={query}
          placeholder="Search…"
          aria-label="Search objects"
          aria-controls="model-explorer-tree"
          onChange={(e) => setQuery(e.target.value)}
        />
      </div>
      <ul id="model-explorer-tree" role="tree" aria-label="Model explorer" ref={treeRef} className="tree__root">
        {renderNodes(roots, 0, null)}
      </ul>
      <div className="sr-only" role="status" aria-live="polite">
        {liveMsg}
      </div>
    </nav>
  )
}

// ---- per-row context menu ----

/**
 * A row's actions menu: a keyboard-reachable "⋯" button (so it works without a
 * mouse) plus right-click on the row, both opening the same controlled Radix
 * dropdown anchored to the row. Items dispatch through `onAction`.
 */
function RowMenu({
  node,
  open,
  onOpenChange,
  onAction,
  onCloseAutoFocus,
}: {
  node: Node
  open: boolean
  onOpenChange: (open: boolean) => void
  onAction: (action: NodeAction, ctx: ActionContext) => void
  /** Return focus to the owning tree row when the menu closes (so focus never
   * lands on the visually-hidden "⋯" trigger). */
  onCloseAutoFocus: () => void
}) {
  const ctx = node.actionCtx ?? {}
  return (
    <DM.Root open={open} onOpenChange={onOpenChange}>
      <DM.Trigger asChild>
        <button
          type="button"
          className="tree__actions"
          aria-label={`Actions for ${node.label}`}
          // opacity:0 does NOT remove a button from the tab order, so keep it
          // out of the roving tree's single tab stop. It stays mouse-clickable
          // and openable from the row via Shift+F10 / the ContextMenu key.
          tabIndex={-1}
          // Don't let the trigger's click also select/expand the row.
          onClick={(e) => e.stopPropagation()}
          onKeyDown={(e) => e.stopPropagation()}
        >
          ⋯
        </button>
      </DM.Trigger>
      <DM.Portal>
        <DM.Content
          className="menu"
          align="start"
          sideOffset={4}
          onCloseAutoFocus={(e) => {
            e.preventDefault()
            onCloseAutoFocus()
          }}
        >
          {(node.menu ?? []).map((item) => (
            <DM.Item
              key={item.action}
              className={item.danger ? 'menu__item menu__item--danger' : 'menu__item'}
              disabled={item.disabled}
              onSelect={() => onAction(item.action, ctx)}
            >
              {item.label}
            </DM.Item>
          ))}
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  )
}

// ---- helpers ----

/** All nodes (roots + already-loaded descendants) that have a loader. */
function collectLoaders(roots: Node[], childrenById: Record<string, Node[]>): Node[] {
  const out: Node[] = []
  const walk = (nodes: Node[]) => {
    for (const n of nodes) {
      if (n.loader) out.push(n)
      const kids = childrenById[n.id]
      if (kids) walk(kids)
    }
  }
  walk(roots)
  return out
}

/** Max matching children rendered per parent during a search, so a broad query
 * (e.g. one matching thousands of members) can never flood the tree with DOM. */
const CHILD_CAP = 50

/** The children of one parent to show during a search: those kept by the filter
 * (a match, or an ancestor of one), capped with a trailing "+N more" info row. */
function searchVisibleChildren(
  nodes: Node[],
  parentId: string | null,
  keepIds: Set<string>,
): Node[] {
  const kept = nodes.filter((n) => !n.info && keepIds.has(n.id))
  if (kept.length <= CHILD_CAP) return kept
  const overflow = kept.length - CHILD_CAP
  return [
    ...kept.slice(0, CHILD_CAP),
    {
      id: `${parentId ?? 'root'}/__more`,
      label: `${overflow} more match${overflow === 1 ? '' : 'es'} not shown; refine your search`,
      icon: ' ',
      info: true,
    },
  ]
}

/** Flatten the visible tree (respecting expand state and the active search). */
function flatten(
  roots: Node[],
  openIds: Set<string>,
  childrenById: Record<string, Node[]>,
  searching: boolean,
  keepIds: Set<string>,
): { node: Node; depth: number; parentId: string | null }[] {
  const out: { node: Node; depth: number; parentId: string | null }[] = []
  const walk = (nodes: Node[], depth: number, parentId: string | null) => {
    const list = searching ? searchVisibleChildren(nodes, parentId, keepIds) : nodes
    for (const n of list) {
      // Non-interactive status rows ("No flows yet", "+N more", etc.) are never
      // keyboard navigation targets, so they stay out of the roving-tabindex set.
      if (n.info) continue
      out.push({ node: n, depth, parentId })
      if (n.loader && openIds.has(n.id) && childrenById[n.id]) {
        walk(childrenById[n.id], depth + 1, n.id)
      }
    }
  }
  walk(roots, 0, null)
  return out
}

function cssEscape(s: string): string {
  return s.replace(/["\\]/g, '\\$&')
}
