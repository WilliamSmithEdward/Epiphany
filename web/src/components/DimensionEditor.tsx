import * as DM from '@radix-ui/react-dropdown-menu'
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react'
import {
  editCubeDimension,
  editDimensionById,
  getCube,
  getDimension,
  type DimensionDto,
  type DimensionEdit,
  type ElementKind,
  type InsertPosition,
} from '../api/client'
import { buildElementTree, type TreeNode } from '../model/tree'
import { Card, Dialog, Input, Select, useConfirm } from '../ui'

// The target dimension: either a cube-embedded dimension (edited through the
// cube route) or a registry (global) dimension (edited by id, fanning out to
// every referencing cube). The editor itself is cube-agnostic: it never labels
// or surfaces the cube, beyond the post-edit "also updated" note for a registry
// dimension's fan-out.
export type DimensionTarget =
  | { kind: 'cube'; cube: string; dim: string }
  | { kind: 'registry'; id: number; name: string }

const KIND_LABEL: Record<ElementKind, string> = {
  numeric: 'Numeric',
  string: 'String',
  consolidated: 'Consolidation',
}

const KIND_OPTIONS = [
  { value: 'numeric', label: 'Numeric' },
  { value: 'string', label: 'String' },
  { value: 'consolidated', label: 'Consolidation' },
]

const KIND_ICON: Record<ElementKind, string> = {
  numeric: '·',
  string: '"',
  consolidated: '◇',
}

/** Where, relative to a target row, a drop lands: place the dragged member
 * before it, after it, or as a child of it (which turns the target into a
 * consolidation). */
type DropZone = 'before' | 'as-child' | 'after'

/** A flattened, indented hierarchy row for rendering. `name` may repeat when a
 * member sits under more than one parent (alternate hierarchies), so `path`
 * keys the row while `name` drives every edit. `parent` is the member's current
 * parent on THIS row (the second-to-last path segment), or `null` when the row
 * is a top-level root, so a drag-out / "remove from this consolidation" unlinks
 * the right edge. */
interface FlatRow {
  name: string
  kind: ElementKind
  depth: number
  hasChildren: boolean
  expanded: boolean
  path: string
  parent: string | null
}

/** The source of an in-flight move (drag or keyboard pick-up): the member name
 * and the parent it is being moved out of on its origin row. */
interface MoveSource {
  name: string
  parent: string | null
}

/** The current parent of a tree row is the second-to-last segment of its
 * `/`-joined path (`Total/East` -> `Total`); a root row (`East`) has none. */
function parentOfPath(path: string): string | null {
  const at = path.lastIndexOf('/')
  return at === -1 ? null : path.slice(0, at).split('/').pop() ?? null
}

/**
 * The standalone, cube-agnostic, hierarchy-only dimension editor (ADR-0036).
 * Members are rows in a tree: each row is draggable and drives structural edits
 * (reorder / reparent / add child / remove from a consolidation / set kind /
 * delete / insert) through the new endpoints.
 *
 * Drag-and-drop drop zones (research-grounded, ADR-0036): while dragging a member
 * over a row, the row splits into thirds. The top third places the dragged member
 * BEFORE the target and the bottom third AFTER (both an insertion LINE; they
 * compute the full new member order and POST a `reorder`), and the middle third
 * adds the dragged member AS A CHILD of the target (a container HIGHLIGHT; POST an
 * `add_child`, additive: the member keeps its existing parents and the backend
 * turns a leaf/string target into a consolidation). Hovering a collapsed
 * consolidation expands it after a short delay so its children become drop
 * targets. Dropping onto the empty area below the list removes the member from
 * the consolidation it was dragged out of (a `remove_child`), leaving it under
 * its other parents; a member dragged from the root area is a no-op.
 *
 * Keyboard parity (WCAG 2.2 SC 2.5.7, mandatory): the tree is a single tab stop
 * with a roving tabindex; ArrowUp/Down move focus, ArrowRight expands or steps
 * in, ArrowLeft collapses or steps to the parent. Space picks the focused member
 * up; with a member picked up, ArrowUp/Down drop it before/after the focused row,
 * and Escape cancels. Every drag gesture also has a row-menu equivalent: Move up
 * / Move down (reorder one step), Add to a consolidation (additive add_child via
 * an inline picker), Remove from this consolidation, Detach to top level, Convert,
 * and Delete. Delete on a child member offers a choice: "Remove from
 * consolidations" (a popup of its parents with checkboxes, removing only the
 * checked edges and keeping the member and its data) or the destructive "Delete
 * from dimension" (removes the member, every membership, and all its data, behind
 * a confirm); a root member, having no parents, deletes from the dimension
 * directly.
 */
export default function DimensionEditor({
  target,
  onChanged,
}: {
  target: DimensionTarget
  /** Notify the host after a committed edit so the tree / other panes refresh. */
  onChanged?: () => void
}) {
  const confirm = useConfirm()
  const [dimension, setDimension] = useState<DimensionDto | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set())
  // Drag state: the source member + the parent it is dragged out of, plus the row
  // and zone under the pointer.
  const [drag, setDrag] = useState<MoveSource | null>(null)
  const [over, setOver] = useState<{ path: string; zone: DropZone } | null>(null)
  const [rootOver, setRootOver] = useState(false)
  // Keyboard roving tabindex + pick-up state. `focusPath` is the single tab stop;
  // `picked` is the member picked up by Space (keyboard drag) awaiting a drop.
  const [focusPath, setFocusPath] = useState<string | null>(null)
  const [picked, setPicked] = useState<MoveSource | null>(null)
  const treeRef = useRef<HTMLUListElement>(null)
  // A pending "hover a collapsed consolidation -> expand it" timer.
  const hoverExpand = useRef<{ path: string; timer: number } | null>(null)
  // The row whose context menu is open, an in-progress inline add form, and the
  // row whose "add to consolidation" picker is open.
  const [menuPath, setMenuPath] = useState<string | null>(null)
  const [adding, setAdding] = useState<{
    at: 'before' | 'after' | 'as-child'
    ref: string | null
  } | null>(null)
  const [addName, setAddName] = useState('')
  const [addKind, setAddKind] = useState<ElementKind>('numeric')
  // The "Remove from consolidations" popup: the member being removed and the set of
  // its parent consolidations the user has checked to unlink it from. Reached from a
  // child member's Delete menu ("from parent"); the destructive "from dimension"
  // path is a confirm, not this popup.
  const [removeFrom, setRemoveFrom] = useState<{ name: string; checked: Set<string> } | null>(null)

  // A stable key for the load effect: re-fetch when the target identity changes.
  const targetKey =
    target.kind === 'cube' ? `cube:${target.cube}/${target.dim}` : `reg:${target.id}`

  const load = useCallback(() => {
    const apply = (dto: DimensionDto) => {
      setDimension(dto)
      setError(null)
    }
    if (target.kind === 'cube') {
      getCube(target.cube)
        .then((detail) => {
          const dim = detail.dimensions.find((d) => d.name === target.dim)
          if (dim) apply(dim)
          else setError(`Dimension "${target.dim}" was not found.`)
        })
        .catch((e: unknown) =>
          setError(e instanceof Error ? e.message : 'Failed to load the dimension'),
        )
    } else {
      getDimension(target.id)
        .then((detail) =>
          apply({ name: detail.name, elements: detail.elements, edges: detail.edges, attributes: detail.attributes }),
        )
        .catch((e: unknown) =>
          setError(e instanceof Error ? e.message : 'Failed to load the dimension'),
        )
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [targetKey])

  useEffect(() => {
    load()
  }, [load])

  const tree = useMemo(() => (dimension ? buildElementTree(dimension) : []), [dimension])
  const kindOf = useMemo(
    () => new Map((dimension?.elements ?? []).map((e) => [e.name, e.kind] as const)),
    [dimension],
  )
  // child name -> whether it has any incoming edge (used to know roots), and
  // parent name -> children count (used to guard delete of a non-empty parent).
  const childCountOf = useMemo(() => {
    const m = new Map<string, number>()
    for (const e of dimension?.edges ?? []) m.set(e.parent, (m.get(e.parent) ?? 0) + 1)
    return m
  }, [dimension])

  // member name -> every consolidation it is a child of (its parents), in element
  // order. Drives the "Remove from consolidations" popup's checkbox list and tells
  // a Delete whether the member is a root (no parents -> delete straight from the
  // dimension) or a child (offer remove-from-parents vs delete-from-dimension).
  const parentsOf = useMemo(() => {
    const m = new Map<string, string[]>()
    for (const e of dimension?.edges ?? []) {
      const list = m.get(e.child)
      if (list) list.push(e.parent)
      else m.set(e.child, [e.parent])
    }
    return m
  }, [dimension])

  // The consolidations a member could be added to by keyboard (every member of
  // kind consolidated), for the "Add to consolidation" picker.
  const consolidations = useMemo(
    () => (dimension?.elements ?? []).filter((e) => e.kind === 'consolidated').map((e) => e.name),
    [dimension],
  )

  // The flattened, indented visible rows (expanded subtrees only).
  const rows: FlatRow[] = useMemo(() => {
    const out: FlatRow[] = []
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
          parent: parentOfPath(n.path),
        })
        if (isOpen && n.children.length) walk(n.children, depth + 1)
      }
    }
    walk(tree, 0)
    return out
  }, [tree, expanded])

  const toggleExpand = (path: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(path)) n.delete(path)
      else n.add(path)
      return n
    })

  // Self-healing roving tabindex: keep exactly one focusable row. If the focused
  // row leaves the visible set (a collapse, a reload, a structural edit), re-home
  // focus to the first row so the tree always has one tab stop (WCAG 2.4.3).
  useEffect(() => {
    if (rows.length === 0) {
      if (focusPath !== null) setFocusPath(null)
      return
    }
    if (!focusPath || !rows.some((r) => r.path === focusPath)) {
      setFocusPath(rows[0].path)
    }
  }, [rows, focusPath])

  // Keep DOM focus on the roving-tabindex row after keyboard navigation, but only
  // while focus is already inside the tree (so we never steal focus on load).
  useEffect(() => {
    if (!focusPath) return
    const root = treeRef.current
    if (!root || !root.contains(document.activeElement)) return
    const el = root.querySelector<HTMLElement>(`[data-row-path="${cssEscapePath(focusPath)}"]`)
    el?.focus()
  }, [focusPath, rows])

  // Run one structural edit, then reload and surface the result. A registry
  // dimension reports which referencing cubes were also updated.
  const runEdit = useCallback(
    async (edit: DimensionEdit) => {
      setBusy(true)
      setError(null)
      setNotice(null)
      try {
        if (target.kind === 'cube') {
          await editCubeDimension(target.cube, target.dim, edit)
        } else {
          const result = await editDimensionById(target.id, edit)
          if (result.fanned_out_to.length > 0) {
            setNotice(`Updated, and applied to ${result.fanned_out_to.join(', ')}.`)
          }
        }
        load()
        onChanged?.()
        return true
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Could not apply the change')
        return false
      } finally {
        setBusy(false)
      }
    },
    [target, load, onChanged],
  )

  // The new full member order placing `moved` immediately before/after `ref`.
  const orderMoving = useCallback(
    (moved: string, ref: string, side: 'before' | 'after'): string[] => {
      const base = (dimension?.elements ?? []).map((e) => e.name).filter((n) => n !== moved)
      const at = base.indexOf(ref)
      if (at === -1) return base
      const insertAt = side === 'before' ? at : at + 1
      base.splice(insertAt, 0, moved)
      return base
    },
    [dimension],
  )

  // Add `child` to consolidation `parent` ADDITIVELY: the child keeps its existing
  // parents (a member may roll up to several consolidations), unlike Detach +
  // reparent which moves it. When the target is currently a leaf/string it gets
  // converted to a consolidation and any value stored directly on it is cleared,
  // so confirm first; a target that is already a consolidation is purely additive
  // and needs no confirmation.
  const addChild = useCallback(
    async (parent: string, child: string): Promise<boolean> => {
      if (parent === child) return false
      const parentKind = kindOf.get(parent)
      if (parentKind && parentKind !== 'consolidated') {
        const ok = await confirm({
          title: `Add "${child}" as a child of "${parent}"`,
          body: `"${parent}" becomes a Consolidation, which is calculated from its children, so any value stored directly on "${parent}" will be cleared. Continue?`,
          confirmLabel: 'Add as child',
          danger: true,
        })
        if (!ok) return false
      }
      return runEdit({ op: 'add_child', parent, child })
    },
    [confirm, kindOf, runEdit],
  )

  // Remove just the one `parent -> child` edge (keep the member, its data, and its
  // other parents). A no-op when the member has no parent on the row it came from.
  const removeChild = useCallback(
    (parent: string | null, child: string): Promise<boolean> => {
      if (!parent) return Promise.resolve(false)
      return runEdit({ op: 'remove_child', parent, child })
    },
    [runEdit],
  )

  // Resolve a drop: before/after -> reorder; as-child -> add the moved member as a
  // child of the target additively (it keeps its existing parents).
  const doDrop = useCallback(
    (moved: string, targetName: string, zone: DropZone) => {
      if (moved === targetName) return
      if (zone === 'as-child') {
        void addChild(targetName, moved)
      } else {
        void runEdit({ op: 'reorder', new_order: orderMoving(moved, targetName, zone) })
      }
    },
    [runEdit, orderMoving, addChild],
  )

  // Compute the drop zone (top third / middle / bottom third) from the pointer
  // position within a row.
  const zoneFor = (e: DragEvent<HTMLElement>): DropZone => {
    const rect = e.currentTarget.getBoundingClientRect()
    const y = e.clientY - rect.top
    if (y < rect.height / 3) return 'before'
    if (y > (rect.height * 2) / 3) return 'after'
    return 'as-child'
  }

  // Cancel any pending hover-to-expand timer.
  const clearHoverExpand = useCallback(() => {
    if (hoverExpand.current) {
      window.clearTimeout(hoverExpand.current.timer)
      hoverExpand.current = null
    }
  }, [])

  // Clear all transient drag state (used by drop, dragend, and a failed drop) so a
  // rejected or finished drag never leaves a stuck indicator.
  const endDrag = useCallback(() => {
    setDrag(null)
    setOver(null)
    setRootOver(false)
    clearHoverExpand()
  }, [clearHoverExpand])

  // While dragging over a collapsed consolidation, expand it after a short hover
  // so its children become drop targets (research-grounded, ADR-0036).
  const scheduleHoverExpand = useCallback(
    (row: FlatRow) => {
      if (!row.hasChildren || row.expanded) {
        clearHoverExpand()
        return
      }
      if (hoverExpand.current?.path === row.path) return
      clearHoverExpand()
      const timer = window.setTimeout(() => {
        setExpanded((s) => new Set(s).add(row.path))
        hoverExpand.current = null
      }, 600)
      hoverExpand.current = { path: row.path, timer }
    },
    [clearHoverExpand],
  )

  // Move a member one step among the FULL member order (swap it with the member
  // immediately before/after it). This is the non-drag reorder path, so keyboard
  // users have parity with drag before/after: it backs both the row menu's Move
  // up / Move down and an arrow key on a picked-up member (ADR-0036).
  const moveStep = useCallback(
    (name: string, dir: 'up' | 'down') => {
      setMenuPath(null)
      const order = (dimension?.elements ?? []).map((e) => e.name)
      const at = order.indexOf(name)
      if (at === -1) return
      const swap = dir === 'up' ? at - 1 : at + 1
      if (swap < 0 || swap >= order.length) return
      ;[order[at], order[swap]] = [order[swap], order[at]]
      void runEdit({ op: 'reorder', new_order: order })
    },
    [dimension, runEdit],
  )

  // ---- keyboard tree navigation + pick-up/drop (WCAG 2.2 SC 2.5.7) ----

  const onRowKeyDown = useCallback(
    (e: ReactKeyboardEvent, row: FlatRow) => {
      // Handle once on the focused row; stop it bubbling to ancestor <li>s.
      e.stopPropagation()
      const idx = rows.findIndex((r) => r.path === row.path)
      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault()
          if (picked) {
            // Move the picked member one step DOWN in the full order; it stays
            // picked so repeated arrows walk it to its destination.
            moveStep(picked.name, 'down')
          } else if (idx < rows.length - 1) {
            setFocusPath(rows[idx + 1].path)
          }
          break
        case 'ArrowUp':
          e.preventDefault()
          if (picked) {
            // Move the picked member one step UP in the full order.
            moveStep(picked.name, 'up')
          } else if (idx > 0) {
            setFocusPath(rows[idx - 1].path)
          }
          break
        case 'ArrowRight':
          e.preventDefault()
          if (picked) break
          if (row.hasChildren && !row.expanded) toggleExpand(row.path)
          else if (row.expanded && idx < rows.length - 1) setFocusPath(rows[idx + 1].path)
          break
        case 'ArrowLeft':
          e.preventDefault()
          if (picked) break
          if (row.hasChildren && row.expanded) toggleExpand(row.path)
          else {
            // Step to the parent row (the nearest earlier row at a lower depth).
            for (let j = idx - 1; j >= 0; j--) {
              if (rows[j].depth < row.depth) {
                setFocusPath(rows[j].path)
                break
              }
            }
          }
          break
        case 'Home':
          e.preventDefault()
          if (!picked && rows.length > 0) setFocusPath(rows[0].path)
          break
        case 'End':
          e.preventDefault()
          if (!picked && rows.length > 0) setFocusPath(rows[rows.length - 1].path)
          break
        case ' ':
        case 'Enter':
          e.preventDefault()
          // Toggle pick-up: Space/Enter grabs the focused member; pressing it again
          // drops it in place (releases). Arrows move a picked member one step.
          setPicked((p) => (p && p.name === row.name ? null : { name: row.name, parent: row.parent }))
          break
        case 'Escape':
          if (picked) {
            e.preventDefault()
            setPicked(null)
          }
          break
        case 'ContextMenu':
          e.preventDefault()
          setMenuPath(row.path)
          break
        case 'F10':
          if (e.shiftKey) {
            e.preventDefault()
            setMenuPath(row.path)
          }
          break
        default:
          break
      }
    },
    [rows, picked, moveStep],
  )

  // ---- context-menu actions ----

  const startAdd = (at: 'before' | 'after' | 'as-child', ref: string) => {
    setMenuPath(null)
    setAdding({ at, ref })
    setAddName('')
    setAddKind('numeric')
  }

  const commitAdd = useCallback(async () => {
    if (!adding) return
    const name = addName.trim()
    if (name === '') {
      setError('Give the new member a name.')
      return
    }
    let ok: boolean
    if (adding.at === 'as-child' && adding.ref) {
      // Insert at the end, then add it as a child of the chosen member
      // additively (which keeps any other members the parent already holds and
      // converts a leaf/string parent to a consolidation). Two committed edits.
      const inserted = await runEdit({
        op: 'insert',
        name,
        kind: addKind,
        position: { at: 'end' },
      })
      ok = inserted ? await addChild(adding.ref, name) : false
    } else {
      const position: InsertPosition = adding.ref
        ? { at: adding.at as 'before' | 'after', ref: adding.ref }
        : { at: 'end' }
      ok = await runEdit({ op: 'insert', name, kind: addKind, position })
    }
    if (ok) setAdding(null)
  }, [adding, addName, addKind, runEdit, addChild])

  const convert = useCallback(
    async (name: string, kind: ElementKind) => {
      setMenuPath(null)
      const current = kindOf.get(name)
      if (current === kind) return
      // Converting a leaf (numeric/string) that may hold stored values into a
      // consolidation, or switching between numeric and string, can clear stored
      // values. State the rule plainly and confirm first (ADR-0036).
      const dropsValues =
        (current !== 'consolidated' && kind === 'consolidated') ||
        (current === 'numeric' && kind === 'string') ||
        (current === 'string' && kind === 'numeric')
      if (dropsValues) {
        const ok = await confirm({
          title: `Convert "${name}" to ${KIND_LABEL[kind]}`,
          body:
            kind === 'consolidated'
              ? `A Consolidation is calculated from its children, so any value stored directly on "${name}" will be cleared. Continue?`
              : `Changing the kind of "${name}" clears any stored value that does not fit the new kind. Continue?`,
          confirmLabel: 'Convert',
          danger: true,
        })
        if (!ok) return
      }
      void runEdit({ op: 'set_kind', element: name, kind })
    },
    [confirm, kindOf, runEdit],
  )

  // Detach: remove the member from EVERY parent (it becomes a root), keeping it.
  const detach = useCallback(
    (name: string) => {
      setMenuPath(null)
      void runEdit({ op: 'reparent', child: name, new_parent: null })
    },
    [runEdit],
  )

  // Remove the member from ONE consolidation (the row's current parent), keeping
  // it under any other parents. Distinct from Detach (all parents) and Delete.
  const removeFromConsolidation = useCallback(
    (parent: string | null, name: string) => {
      setMenuPath(null)
      void removeChild(parent, name)
    },
    [removeChild],
  )

  // Add the member to a chosen consolidation from the row menu's submenu (the
  // no-drag equivalent of dropping into the middle zone), additive so it keeps
  // its other parents.
  const addToConsolidation = useCallback(
    (parent: string, name: string) => {
      setMenuPath(null)
      void addChild(parent, name)
    },
    [addChild],
  )

  // Delete the member from the WHOLE dimension: removes it from every consolidation
  // it belongs to AND deletes all data stored on it (the `delete` op). Destructive
  // and irreversible here, so it confirms with an explicit "all data will be lost"
  // warning. A root member's Delete comes straight here (it has no parents to choose
  // between); a child member reaches it as the "from dimension" branch.
  const deleteFromDimension = useCallback(
    async (name: string) => {
      setMenuPath(null)
      if ((childCountOf.get(name) ?? 0) > 0) {
        setError(
          `"${name}" has members under it. Detach or delete those first, then delete "${name}".`,
        )
        return
      }
      const ok = await confirm({
        title: `Delete "${name}" from the dimension`,
        body: `This removes "${name}" from every consolidation it belongs to and permanently deletes all data stored on it. This cannot be undone here.`,
        confirmLabel: 'Delete from dimension',
        danger: true,
      })
      if (!ok) return
      void runEdit({ op: 'delete', element: name })
    },
    [confirm, childCountOf, runEdit],
  )

  // Open the "remove from consolidations" popup for a child member: the user picks
  // which of its parent consolidations to unlink it from. Non-destructive (the
  // member and its data stay), so no confirm here, the popup's button is the commit.
  const openRemoveFrom = useCallback((name: string) => {
    setMenuPath(null)
    setRemoveFrom({ name, checked: new Set() })
  }, [])

  // Apply the popup: unlink the member from each checked consolidation (one
  // `remove_child` edge per parent), keeping the member, its data, and any parents
  // left unchecked. Sequential so a mid-run rejection stops and surfaces.
  const confirmRemoveFrom = useCallback(async () => {
    if (!removeFrom) return
    const { name, checked } = removeFrom
    for (const parent of checked) {
      const ok = await runEdit({ op: 'remove_child', parent, child: name })
      if (!ok) return
    }
    setRemoveFrom(null)
  }, [removeFrom, runEdit])

  if (!dimension) {
    return error ? (
      <Card title="Dimension">
        <p className="error" role="alert">
          {error}
        </p>
      </Card>
    ) : (
      <p className="banner" role="status">
        Loading dimension…
      </p>
    )
  }

  const count = dimension.elements.length
  // The parent consolidations offered as checkboxes in the "remove from" popup.
  const removeParents = removeFrom ? parentsOf.get(removeFrom.name) ?? [] : []

  return (
    <Card
      title={dimension.name}
      subtitle="Drag a member onto another to place it before, after, or inside; drag it out to remove it from that consolidation. Or focus a member and press Space to pick it up, then use the arrow keys. Right-click a member for more actions."
    >
      <div className="dimedit">
        {error ? (
          <p className="error" role="alert">
            {error}
          </p>
        ) : null}
        {notice ? (
          <p className="banner banner--ok" role="status">
            {notice}
          </p>
        ) : null}
        {/* Short keyboard instructions, referenced by every draggable row via
            aria-describedby so a screen reader announces the no-drag gesture. */}
        <p id="dimedit-dnd-help" className="sr-only">
          Press Space to pick this member up, then Arrow Up or Arrow Down to move it
          one position; press Space again or Escape to drop it. Use the actions menu
          to add it to a consolidation, remove it from one, detach, convert, or
          delete it.
        </p>
        {picked ? (
          <p className="banner" role="status">
            Picked up “{picked.name}”. Use Arrow Up or Arrow Down to move it one
            position; Space drops it, Escape cancels.
          </p>
        ) : null}

        <div className="dimedit__toolbar">
          <span className="muted">
            {count} {count === 1 ? 'member' : 'members'}
          </span>
          <button
            type="button"
            className="dimedit__addroot"
            disabled={busy}
            onClick={() => {
              setAdding({ at: 'after', ref: null })
              setAddName('')
              setAddKind('numeric')
            }}
          >
            + Add member
          </button>
        </div>

        <ul
          ref={treeRef}
          className={`dimedit__tree${rootOver ? ' is-rootover' : ''}`}
          role="tree"
          aria-label={`Members of ${dimension.name}`}
          aria-busy={busy || undefined}
          onDragOver={(e) => {
            // Dragging into the list's own (non-row) area is the "out of a
            // consolidation" target: remove the member from the parent it came
            // from. A root member dragged here is a no-op (no parent to remove).
            if (drag && e.target === e.currentTarget) {
              e.preventDefault()
              setRootOver(true)
              setOver(null)
              clearHoverExpand()
            }
          }}
          onDragLeave={(e) => {
            if (e.target === e.currentTarget) setRootOver(false)
          }}
          onDrop={(e) => {
            if (drag && e.target === e.currentTarget) {
              e.preventDefault()
              // Remove from the one consolidation it was dragged out of.
              removeFromConsolidation(drag.parent, drag.name)
            }
            endDrag()
          }}
        >
          {rows.map((r) => {
            const isOver = over?.path === r.path
            const zone = isOver ? over?.zone : undefined
            const isPicked = picked?.name === r.name
            return (
              <li
                key={r.path}
                role="treeitem"
                aria-level={r.depth + 1}
                aria-expanded={r.hasChildren ? r.expanded : undefined}
                aria-roledescription="draggable member"
                aria-describedby="dimedit-dnd-help"
                aria-grabbed={isPicked || undefined}
                data-row-path={r.path}
                tabIndex={focusPath === r.path ? 0 : -1}
                className={`dimedit__row${drag?.name === r.name ? ' is-dragging' : ''}${
                  isOver ? ` is-over is-over--${zone}` : ''
                }${isPicked ? ' is-picked' : ''}`}
                draggable={!busy}
                onFocus={() => setFocusPath(r.path)}
                onKeyDown={(e) => onRowKeyDown(e, r)}
                onDragStart={(e) => {
                  setDrag({ name: r.name, parent: r.parent })
                  setPicked(null)
                  e.dataTransfer.effectAllowed = 'move'
                }}
                onDragEnd={endDrag}
                onDragOver={(e) => {
                  if (!drag || drag.name === r.name) return
                  e.preventDefault()
                  e.stopPropagation()
                  setRootOver(false)
                  const z = zoneFor(e)
                  scheduleHoverExpand(r)
                  setOver((cur) =>
                    cur?.path === r.path && cur.zone === z ? cur : { path: r.path, zone: z },
                  )
                }}
                onDrop={(e) => {
                  e.preventDefault()
                  e.stopPropagation()
                  if (drag && over) doDrop(drag.name, r.name, over.zone)
                  endDrag()
                }}
                onContextMenu={(e) => {
                  e.preventDefault()
                  setMenuPath(r.path)
                }}
              >
                <div
                  className="dimedit__rowinner"
                  style={{ paddingInlineStart: `${r.depth * 1.2 + 0.25}rem` }}
                >
                  {r.hasChildren ? (
                    <button
                      type="button"
                      className="dimedit__twisty"
                      tabIndex={-1}
                      aria-label={r.expanded ? `Collapse ${r.name}` : `Expand ${r.name}`}
                      aria-expanded={r.expanded}
                      onClick={(e) => {
                        e.stopPropagation()
                        toggleExpand(r.path)
                      }}
                    >
                      {r.expanded ? '▾' : '▸'}
                    </button>
                  ) : (
                    <span className="dimedit__twisty dimedit__twisty--leaf" aria-hidden="true" />
                  )}
                  <span className="dimedit__handle" aria-hidden="true">
                    ⠿
                  </span>
                  <span className="dimedit__icon" aria-hidden="true">
                    {KIND_ICON[r.kind]}
                  </span>
                  <span className="dimedit__name">{r.name}</span>
                  <span className="dimedit__kind">{KIND_LABEL[r.kind]}</span>
                  <RowMenu
                    name={r.name}
                    kind={r.kind}
                    isRoot={r.parent === null}
                    currentParent={r.parent}
                    consolidations={consolidations}
                    open={menuPath === r.path}
                    onOpenChange={(o) => setMenuPath(o ? r.path : null)}
                    onMoveUp={() => moveStep(r.name, 'up')}
                    onMoveDown={() => moveStep(r.name, 'down')}
                    onAddBefore={() => startAdd('before', r.name)}
                    onAddAfter={() => startAdd('after', r.name)}
                    onAddChild={() => startAdd('as-child', r.name)}
                    onAddToConsolidation={(parent) => addToConsolidation(parent, r.name)}
                    onConvert={(k) => void convert(r.name, k)}
                    onRemoveFromConsolidation={() => removeFromConsolidation(r.parent, r.name)}
                    onDetach={() => detach(r.name)}
                    onRemoveFrom={() => openRemoveFrom(r.name)}
                    onDeleteFromDimension={() => void deleteFromDimension(r.name)}
                  />
                </div>
              </li>
            )
          })}
          {rows.length === 0 ? (
            <li role="none" className="dimedit__empty muted">
              No members yet. Use Add member to start.
            </li>
          ) : null}
        </ul>

        {adding ? (
          <div className="dimedit__add" role="group" aria-label="Add member">
            <span className="muted">
              {adding.ref
                ? adding.at === 'as-child'
                  ? `New member inside ${adding.ref}`
                  : `New member ${adding.at} ${adding.ref}`
                : 'New member at the end'}
            </span>
            <Input
              autoFocus
              value={addName}
              placeholder="Member name"
              aria-label="New member name"
              onChange={(e) => setAddName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  void commitAdd()
                } else if (e.key === 'Escape') {
                  e.preventDefault()
                  setAdding(null)
                }
              }}
            />
            <Select
              value={addKind}
              onValueChange={(v) => setAddKind(v as ElementKind)}
              options={KIND_OPTIONS}
              ariaLabel="New member kind"
            />
            <button
              type="button"
              className="dimedit__btn dimedit__btn--primary"
              disabled={busy}
              onClick={() => void commitAdd()}
            >
              Add
            </button>
            <button type="button" className="dimedit__btn" onClick={() => setAdding(null)}>
              Cancel
            </button>
          </div>
        ) : null}

        {/* "Remove from consolidations" popup: pick which parent consolidations to
            unlink a child member from. Non-destructive (keeps the member + data). */}
        <Dialog
          open={removeFrom !== null}
          onOpenChange={(o) => {
            if (!o) setRemoveFrom(null)
          }}
          size="sm"
          title={removeFrom ? `Remove “${removeFrom.name}” from consolidations` : ''}
          description="Removing the member from a consolidation keeps the member and all its data; it only stops rolling up into that consolidation. Choose which to remove it from."
          footer={
            <>
              <button
                type="button"
                className="dimedit__btn"
                onClick={() => setRemoveFrom(null)}
              >
                Cancel
              </button>
              <button
                type="button"
                className="dimedit__btn dimedit__btn--primary"
                disabled={busy || !removeFrom || removeFrom.checked.size === 0}
                onClick={() => void confirmRemoveFrom()}
              >
                {removeFrom && removeFrom.checked.size > 0
                  ? `Remove from ${removeFrom.checked.size} consolidation${
                      removeFrom.checked.size === 1 ? '' : 's'
                    }`
                  : 'Remove'}
              </button>
            </>
          }
        >
          <ul
            className="dimedit__removelist"
            role="group"
            aria-label="Consolidations to remove from"
          >
            {removeParents.map((parent) => {
              const id = `dimedit-rm-${parent}`
              const isChecked = removeFrom?.checked.has(parent) ?? false
              return (
                <li key={parent} className="dimedit__removeitem">
                  <label htmlFor={id} className="dimedit__removelabel">
                    <input
                      id={id}
                      type="checkbox"
                      checked={isChecked}
                      onChange={(e) =>
                        setRemoveFrom((cur) => {
                          if (!cur) return cur
                          const checked = new Set(cur.checked)
                          if (e.target.checked) checked.add(parent)
                          else checked.delete(parent)
                          return { ...cur, checked }
                        })
                      }
                    />
                    <span>{parent}</span>
                  </label>
                </li>
              )
            })}
            {removeParents.length === 0 ? (
              <li className="muted">This member is not in any consolidation.</li>
            ) : null}
          </ul>
        </Dialog>
      </div>
    </Card>
  )
}

/** Escape a row path for use in a `[data-row-path="..."]` attribute selector
 * (paths can contain `/` and arbitrary member names). Prefer the platform
 * `CSS.escape` and fall back to escaping the quote/backslash chars. */
function cssEscapePath(path: string): string {
  if (typeof CSS !== 'undefined' && typeof CSS.escape === 'function') return CSS.escape(path)
  return path.replace(/["\\]/g, '\\$&')
}

// ---- per-row context / actions menu ----

/**
 * One member row's actions, as a controlled Radix dropdown anchored to a
 * keyboard-reachable "..." trigger. Right-clicking the row opens the same menu
 * (the editor sets `open`), so every structural action has both a pointer and a
 * keyboard/no-drag path (ADR-0036). The convert items are disabled for the
 * current kind so the menu reads as a clear state toggle; "Remove from this
 * consolidation" is disabled for a top-level root (it has no parent edge).
 */
function RowMenu({
  name,
  kind,
  isRoot,
  currentParent,
  consolidations,
  open,
  onOpenChange,
  onMoveUp,
  onMoveDown,
  onAddBefore,
  onAddAfter,
  onAddChild,
  onAddToConsolidation,
  onConvert,
  onRemoveFromConsolidation,
  onDetach,
  onRemoveFrom,
  onDeleteFromDimension,
}: {
  name: string
  kind: ElementKind
  isRoot: boolean
  currentParent: string | null
  consolidations: string[]
  open: boolean
  onOpenChange: (open: boolean) => void
  onMoveUp: () => void
  onMoveDown: () => void
  onAddBefore: () => void
  onAddAfter: () => void
  onAddChild: () => void
  onAddToConsolidation: (parent: string) => void
  onConvert: (kind: ElementKind) => void
  onRemoveFromConsolidation: () => void
  onDetach: () => void
  onRemoveFrom: () => void
  onDeleteFromDimension: () => void
}) {
  // Consolidations this member could be added to (every consolidation except
  // itself and the one it already sits under on this row).
  const targets = consolidations.filter((c) => c !== name && c !== currentParent)
  return (
    <DM.Root open={open} onOpenChange={onOpenChange}>
      <DM.Trigger asChild>
        <button
          type="button"
          className="dimedit__actions"
          aria-label={`Actions for ${name}`}
          tabIndex={-1}
          onClick={(e) => e.stopPropagation()}
          onKeyDown={(e) => e.stopPropagation()}
        >
          ⋯
        </button>
      </DM.Trigger>
      <DM.Portal>
        <DM.Content className="menu" align="end" sideOffset={4}>
          <DM.Item className="menu__item" onSelect={onMoveUp}>
            Move up
          </DM.Item>
          <DM.Item className="menu__item" onSelect={onMoveDown}>
            Move down
          </DM.Item>
          <DM.Separator className="menu__sep" />
          <DM.Item className="menu__item" onSelect={onAddBefore}>
            Add member before
          </DM.Item>
          <DM.Item className="menu__item" onSelect={onAddAfter}>
            Add member after
          </DM.Item>
          <DM.Item className="menu__item" onSelect={onAddChild}>
            Add member as child
          </DM.Item>
          {targets.length > 0 ? (
            <DM.Sub>
              <DM.SubTrigger className="menu__item menu__item--sub">
                Add to consolidation
              </DM.SubTrigger>
              <DM.Portal>
                <DM.SubContent className="menu" sideOffset={2} alignOffset={-4}>
                  {targets.map((parent) => (
                    <DM.Item
                      key={parent}
                      className="menu__item"
                      onSelect={() => onAddToConsolidation(parent)}
                    >
                      {parent}
                    </DM.Item>
                  ))}
                </DM.SubContent>
              </DM.Portal>
            </DM.Sub>
          ) : null}
          <DM.Separator className="menu__sep" />
          <DM.Item
            className="menu__item"
            disabled={kind === 'numeric'}
            onSelect={() => onConvert('numeric')}
          >
            Convert to Numeric
          </DM.Item>
          <DM.Item
            className="menu__item"
            disabled={kind === 'string'}
            onSelect={() => onConvert('string')}
          >
            Convert to String
          </DM.Item>
          <DM.Item
            className="menu__item"
            disabled={kind === 'consolidated'}
            onSelect={() => onConvert('consolidated')}
          >
            Convert to Consolidation
          </DM.Item>
          <DM.Separator className="menu__sep" />
          <DM.Item
            className="menu__item"
            disabled={isRoot}
            onSelect={onRemoveFromConsolidation}
          >
            {isRoot ? 'Remove from this consolidation' : `Remove from "${currentParent}"`}
          </DM.Item>
          <DM.Item className="menu__item" onSelect={onDetach}>
            Detach to top level
          </DM.Item>
          <DM.Separator className="menu__sep" />
          {isRoot ? (
            // A root has no parents to choose between, so Delete goes straight to
            // the destructive "from the whole dimension" path (user direction).
            <DM.Item
              className="menu__item menu__item--danger"
              onSelect={onDeleteFromDimension}
            >
              Delete from dimension
            </DM.Item>
          ) : (
            // A child can be removed from a consolidation (kept, with its data) or
            // deleted from the dimension entirely (destructive). Offer the choice.
            <DM.Sub>
              <DM.SubTrigger className="menu__item menu__item--sub menu__item--danger">
                Delete…
              </DM.SubTrigger>
              <DM.Portal>
                <DM.SubContent className="menu" sideOffset={2} alignOffset={-4}>
                  <DM.Item className="menu__item" onSelect={onRemoveFrom}>
                    Remove from consolidations…
                  </DM.Item>
                  <DM.Separator className="menu__sep" />
                  <DM.Item
                    className="menu__item menu__item--danger"
                    onSelect={onDeleteFromDimension}
                  >
                    Delete from dimension
                  </DM.Item>
                </DM.SubContent>
              </DM.Portal>
            </DM.Sub>
          )}
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  )
}
