import * as DM from '@radix-ui/react-dropdown-menu'
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from 'react'
import {
  getCube,
  getDimension,
  listCubes,
  listDimensions,
  listFlows,
  listJobs,
  listViews,
} from '../api/client'

// What the tree currently has open in the detail pane. The IA is cube-centric
// (research-validated): cube-owned objects (its dimensions, views, rules+feeders)
// nest under the cube; shared dimensions, flows, and schedules are their own
// resource-type roots (flows/schedules grouped by their owning cube).
export type Selection =
  | { kind: 'cube'; cube: string } // the cube's data (pivot grid)
  | { kind: 'cube-dimension'; cube: string; dim: string } // a cube's dimension
  | { kind: 'cube-views'; cube: string } // the cube's views manager
  | { kind: 'view'; cube: string; view: string } // one saved view, opened
  | { kind: 'cube-rules'; cube: string } // the cube's rules + feeders
  | { kind: 'dimension'; id: number; name: string } // a shared library dimension
  | { kind: 'flow'; cube: string; flow: string }
  | { kind: 'schedule'; cube: string; job: string }
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

/** Everything an action handler may need; fields are filled per node kind. */
export interface ActionContext {
  cube?: string
  dim?: string
  view?: string
  flow?: string
  job?: string
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
  /** A short qualifier shown after the label (e.g. "shared", "12"). */
  badge?: string
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
      return `flow:${s.cube}/${s.flow}`
    case 'schedule':
      return `sched:${s.cube}/${s.job}`
    case 'overview':
      return 'overview'
    case 'security':
      return 'security'
  }
}

// ---- node builders (each returns the children for a parent) ----

function elementNodes(
  prefix: string,
  elements: { name: string; kind: string }[],
  menu?: NodeMenuItem[],
  actionCtx?: ActionContext,
): Node[] {
  return elements.map((el) => ({
    id: `${prefix}/el:${el.name}`,
    label: el.name,
    icon: el.kind === 'consolidated' ? '◇' : el.kind === 'string' ? '"' : '·',
    menu,
    actionCtx,
  }))
}

/** The shared context-menu action for a cube dimension (the "Dimensions" group
 * node and each individual cube dimension open the same editor). The editor
 * shows the member/roll-up/attribute forms together, so a single "Edit
 * dimension…" item that lands there is honest about the destination rather than
 * three labels that all open the same multi-section pane at its top. */
const CUBE_DIM_MENU: NodeMenuItem[] = [
  { action: 'edit-attributes', label: 'Edit dimension…' },
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
          elementNodes(`cube:${cube}/dim:${d.name}`, d.elements, CUBE_DIM_MENU, {
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

async function flowsByCube(): Promise<Node[]> {
  const cubes = await listCubes()
  return cubes.map((c) => ({
    id: `flows:${c.name}`,
    label: c.name,
    icon: '▤',
    menu: [{ action: 'new-flow', label: 'New flow…' }],
    actionCtx: { cube: c.name },
    loader: async () => {
      const flows = await listFlows(c.name)
      return flows.length === 0
        ? [{ id: `flows:${c.name}/none`, label: 'No flows yet', icon: ' ', info: true } as Node]
        : flows.map((f) => ({
            id: `flow:${c.name}/${f.name}`,
            label: f.name,
            icon: '⇄',
            selection: { kind: 'flow', cube: c.name, flow: f.name } as Selection,
            menu: [
              { action: 'open-flow', label: 'Open' },
              { action: 'run-flow', label: 'Run' },
              { action: 'delete-flow', label: 'Delete…', danger: true },
            ] as NodeMenuItem[],
            actionCtx: { cube: c.name, flow: f.name },
          }))
    },
  }))
}

async function schedulesByCube(): Promise<Node[]> {
  const cubes = await listCubes()
  return cubes.map((c) => ({
    id: `sched:${c.name}`,
    label: c.name,
    icon: '▤',
    menu: [{ action: 'new-schedule', label: 'New schedule…' }],
    actionCtx: { cube: c.name },
    loader: async () => {
      const jobs = await listJobs(c.name)
      return jobs.length === 0
        ? [{ id: `sched:${c.name}/none`, label: 'No schedules yet', icon: ' ', info: true } as Node]
        : jobs.map((j) => ({
            id: `sched:${c.name}/${j.name}`,
            label: j.name,
            icon: '⏱',
            selection: { kind: 'schedule', cube: c.name, job: j.name } as Selection,
            menu: [
              { action: 'open-schedule', label: 'Edit' },
              { action: 'run-schedule', label: 'Run now' },
              { action: 'delete-schedule', label: 'Delete…', danger: true },
            ] as NodeMenuItem[],
            actionCtx: { cube: c.name, job: j.name },
          }))
    },
  }))
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
      menu: [{ action: 'register-dimension', label: 'Register shared dimension…' }],
      actionCtx: {},
      loader: async () =>
        (await listDimensions()).map((d) => ({
          id: `dim:${d.id}`,
          label: d.name,
          icon: '⬡',
          badge: 'shared',
          selection: { kind: 'dimension', id: d.id, name: d.name } as Selection,
          // Delete is only offered when the shared dimension is unreferenced;
          // the backend rejects deleting a referenced one (409) anyway.
          menu: [
            { action: 'add-member', label: 'Add member…' },
            { action: 'grow-dimension', label: 'Grow…' },
            {
              action: 'delete-dimension',
              label: 'Delete…',
              danger: true,
              disabled: d.references.length > 0,
            },
          ] as NodeMenuItem[],
          actionCtx: { dimId: d.id, dim: d.name },
          loader: async () => {
            const detail = await getDimension(d.id)
            return elementNodes(
              `dim:${d.id}`,
              detail.elements,
              [
                { action: 'add-member', label: 'Add member…' },
                { action: 'grow-dimension', label: 'Grow…' },
              ],
              { dimId: d.id, dim: d.name },
            )
          },
        })),
    },
    {
      id: 'root:flows',
      label: 'Flows',
      icon: '⇄',
      menu: [{ action: 'new-flow', label: 'New flow…' }],
      actionCtx: {},
      loader: flowsByCube,
    },
    {
      id: 'root:schedules',
      label: 'Schedules',
      icon: '⏱',
      menu: [{ action: 'new-schedule', label: 'New schedule…' }],
      actionCtx: {},
      loader: schedulesByCube,
    },
  ]
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
  const [cubeFilter, setCubeFilter] = useState('')
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
    () => flatten(roots, expanded, childrenById, cubeFilter),
    [roots, expanded, childrenById, cubeFilter],
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
      const isOpen = expanded.has(node.id)
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
          if (node.loader && !isOpen) toggle(node)
          else if (isOpen && idx < visible.length - 1) setFocusId(visible[idx + 1].node.id)
          break
        case 'ArrowLeft':
          e.preventDefault()
          if (node.loader && isOpen) toggle(node)
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
    [visible, expanded, toggle, activate, onAction],
  )

  // Keep DOM focus on the roving-tabindex node after keyboard navigation.
  useEffect(() => {
    const el = treeRef.current?.querySelector<HTMLElement>(`[data-node-id="${cssEscape(focusId)}"]`)
    if (el && treeRef.current?.contains(document.activeElement)) el.focus()
  }, [focusId])

  function renderNodes(nodes: Node[], depth: number, parentId: string | null): ReactNode {
    const isCubeRoot = parentId === 'root:cubes' && cubeFilter.trim() !== ''
    const filtered = isCubeRoot
      ? nodes.filter((n) => n.label.toLowerCase().includes(cubeFilter.trim().toLowerCase()))
      : nodes
    // Zero-results: when the cube search filters out every cube, show a single
    // non-interactive status row (role="none") instead of a blank group.
    if (isCubeRoot && filtered.length === 0) {
      return (
        <li role="none" className="tree__empty" style={{ paddingInlineStart: `${depth * 14 + 8}px` }}>
          No cubes match &ldquo;{cubeFilter.trim()}&rdquo;
        </li>
      )
    }
    return filtered.map((node) => {
      // Non-interactive status placeholders ("No flows yet", etc.): a plain
      // role="none" row, never a focusable/selectable treeitem.
      if (node.info) {
        return (
          <li role="none" key={node.id} className="tree__empty" style={{ paddingInlineStart: `${depth * 14 + 8}px` }}>
            {node.label}
          </li>
        )
      }
      const isOpen = expanded.has(node.id)
      const isSel = node.id === selectedId
      const expandable = Boolean(node.loader)
      const selectable = Boolean(node.selection)
      const hasMenu = Boolean(onAction && node.menu && node.menu.length > 0)
      const isLoading = loading.has(node.id)
      const loadError = errorById[node.id]
      const accLabel = node.badge ? `${node.label}, ${node.badge} dimension` : node.label
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
          onClick={() => { setFocusId(node.id); activate(node) }}
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
              <span className="tree__badge" title="Shared dimension" aria-hidden="true">
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
          value={cubeFilter}
          placeholder="Search cubes"
          aria-label="Search cubes"
          aria-controls="model-explorer-tree"
          onChange={(e) => setCubeFilter(e.target.value)}
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

/** Flatten the visible tree (respecting expand state + the cube filter). */
function flatten(
  roots: Node[],
  expanded: Set<string>,
  childrenById: Record<string, Node[]>,
  cubeFilter: string,
): { node: Node; depth: number; parentId: string | null }[] {
  const out: { node: Node; depth: number; parentId: string | null }[] = []
  const walk = (nodes: Node[], depth: number, parentId: string | null) => {
    const filtered = parentId === 'root:cubes' && cubeFilter.trim() !== ''
      ? nodes.filter((n) => n.label.toLowerCase().includes(cubeFilter.trim().toLowerCase()))
      : nodes
    for (const n of filtered) {
      // Non-interactive status rows ("No flows yet", etc.) are never keyboard
      // navigation targets, so they stay out of the roving-tabindex set.
      if (n.info) continue
      out.push({ node: n, depth, parentId })
      if (n.loader && expanded.has(n.id) && childrenById[n.id]) {
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
