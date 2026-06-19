// The object-explorer's model-to-Node tree: the node/selection types and the
// pure builder functions that turn the server's cubes/dimensions/flows/etc. into
// the lazy `Node` forest the ModelExplorer component renders. Kept framework-free
// (no React) so the tree shape is typecheck-covered and unit-testable on its own,
// like web/src/model/tree.ts. ModelExplorer.tsx owns the React state, effects, and
// rendering and composes these builders.

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
export interface Node {
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

export function selectionId(s: Selection): string {
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
export function elementTreeNodes(
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

export async function cubeChildren(cube: string): Promise<Node[]> {
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

export function cubeNode(name: string): Node {
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
export async function flowNodes(): Promise<Node[]> {
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
export async function scheduleNodes(): Promise<Node[]> {
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
export async function connectionNodes(): Promise<Node[]> {
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
export async function dimensionNamespace(): Promise<Node[]> {
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

  const embeddedNode = (cube: string, d: DimensionDto): Node => {
    // A `dimlib:` prefix keeps this id distinct from the under-cube node
    // (`cube:${cube}/dim:...`). The same embedded dimension shows in BOTH the cube
    // tree and this global list (the ADR-0031 union), and the tree's expand /
    // context-menu / focus state is keyed by node id, so identical ids would
    // couple the two locations (expanding or opening a menu in one fires in both).
    // Selection still carries the same `cube-dimension` target, so either row
    // opens the same dimension.
    const id = `dimlib:cube:${cube}/dim:${d.name}`
    return {
      id,
      label: d.name,
      icon: '⬡',
      selection: { kind: 'cube-dimension', cube, dim: d.name },
      // An embedded dimension can be promoted into the global registry so other
      // cubes can reference it (ADR-0031 Phase 1), alongside the usual edit/sets.
      menu: [...CUBE_DIM_MENU, { action: 'promote-dimension', label: 'Reuse in other cubes…' }],
      actionCtx: { cube, dim: d.name },
      loader: async () =>
        elementTreeNodes(id, buildElementTree(d), CUBE_DIM_MENU, { cube, dim: d.name }),
    }
  }

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
export function rootNodes(isAdmin: boolean): Node[] {
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

// ---- tree-walk helpers (pure) ----

/** All nodes (roots + already-loaded descendants) that have a loader. */
export function collectLoaders(roots: Node[], childrenById: Record<string, Node[]>): Node[] {
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
export const CHILD_CAP = 50

/** The children of one parent to show during a search: those kept by the filter
 * (a match, or an ancestor of one), capped with a trailing "+N more" info row. */
export function searchVisibleChildren(
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
export function flatten(
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

export function cssEscape(s: string): string {
  return s.replace(/["\\]/g, '\\$&')
}
