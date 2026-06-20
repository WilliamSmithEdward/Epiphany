import * as CM from '@radix-ui/react-context-menu'
import * as DM from '@radix-ui/react-dropdown-menu'
import type { CSSProperties, ReactNode } from 'react'
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
import { Card, Dialog, EmptyState, Input, Select, useConfirm } from '../ui'

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

/** A shared, frozen empty set used as the per-row descendants fallback so a row
 * with no descendants always gets the SAME reference (stable props), instead of
 * a fresh `new Set()` per render. */
const EMPTY_SET: ReadonlySet<string> = new Set<string>()

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
 * / Move down (reorder one step), Copy to a consolidation (additive add_child, the
 * member keeps its other parents), Move to a consolidation or the top level (a
 * reparent, the member loses its other parents, confirmed when it has several),
 * Remove from this consolidation (a single edge), Convert, and Delete. Delete on a
 * child member offers a choice: "Remove from
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
  // Whether the pointer is over the always-present drag-to-top-level drop zone
  // (ADR-0038), so it highlights while a member is dragged onto it.
  const [topOver, setTopOver] = useState(false)
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

  // member name -> whether it is explicitly pinned to the top level (ADR-0038).
  // Drives the row menu's pin/unpin toggle, the "(pinned)" badge on a pinned member
  // that also has a parent, and copyTo's auto-pin (keep a no-parent display root at
  // the top when copying it into a consolidation).
  const pinnedOf = useMemo(
    () => new Set((dimension?.elements ?? []).filter((e) => e.pinned_to_top).map((e) => e.name)),
    [dimension],
  )

  // parent name -> its direct children, for computing the reachable descendants of
  // the dragged member (to suppress the as-child indicator on a cycle-forming
  // target: dropping a member into its own descendant would form a cycle, which the
  // backend rejects). UX only - the backend remains the source of truth.
  const childrenByParent = useMemo(() => {
    const m = new Map<string, string[]>()
    for (const e of dimension?.edges ?? []) {
      const list = m.get(e.parent)
      if (list) list.push(e.child)
      else m.set(e.parent, [e.child])
    }
    return m
  }, [dimension])

  // The set of members reachable below the currently-dragged member (its transitive
  // descendants), recomputed when the drag source changes. An as-child drop onto any
  // of these - or onto the member itself - would form a cycle, so we suppress the
  // as-child indicator there (the self guard covers the member itself).
  const dragDescendants = useMemo(() => {
    const out = new Set<string>()
    if (!drag) return out
    const stack = [drag.name]
    while (stack.length) {
      const cur = stack.pop() as string
      for (const child of childrenByParent.get(cur) ?? []) {
        if (!out.has(child)) {
          out.add(child)
          stack.push(child)
        }
      }
    }
    return out
  }, [drag, childrenByParent])

  // The transitive descendants of EVERY member (each one's whole reachable subtree),
  // computed once per dimension the same way `dragDescendants` does, then indexed per
  // row. Used by the row menu's Copy to / Move to submenus to exclude targets that
  // would form a cycle (adding the member under its own descendant). UX guard only -
  // the backend rejects an actual cycle regardless.
  //
  // Precomputed as a Map rather than a per-row `descendantsOf(r.name)` call because
  // that was evaluated eagerly for every visible row on EVERY render (each drag-over
  // setOver tick, every focus/notice/busy toggle), allocating a fresh BFS + Set each
  // time. The map computes each member's set once; rows index it with a stable
  // EMPTY_SET fallback so a row's `excludeTargets` ref stays stable across renders.
  const descendantsByName = useMemo(() => {
    const m = new Map<string, ReadonlySet<string>>()
    for (const name of childrenByParent.keys()) {
      const out = new Set<string>()
      const stack = [name]
      while (stack.length) {
        const cur = stack.pop() as string
        for (const child of childrenByParent.get(cur) ?? []) {
          if (!out.has(child)) {
            out.add(child)
            stack.push(child)
          }
        }
      }
      m.set(name, out)
    }
    return m
  }, [childrenByParent])

  // The consolidations a member could be added to by keyboard (every member of
  // kind consolidated), for the "Copy to" / "Move to" pickers.
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

  // Every expandable node's path (a consolidation with children), for the toolbar's
  // Expand all / Collapse all. Computed from the editor's own '/'-joined node.path;
  // the pivot's allExpandableKeys uses a different separator and is not reusable here.
  const expandablePaths = useMemo(() => {
    const out: string[] = []
    const walk = (nodes: TreeNode[]) => {
      for (const n of nodes) {
        if (n.children.length > 0) {
          out.push(n.path)
          walk(n.children)
        }
      }
    }
    walk(tree)
    return out
  }, [tree])

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
  // dimension reports which referencing cubes were also updated; `successNote` is
  // a concise confirmation announced via the role=status live region for EVERY
  // target (cube or registry), so a successful edit is announced to assistive tech
  // and not just silently applied (WCAG 4.1.3). The registry fan-out note, when
  // present, supersedes it (it carries the same "updated" meaning plus the cubes).
  const runEdit = useCallback(
    async (edit: DimensionEdit, successNote?: string) => {
      setBusy(true)
      setError(null)
      setNotice(null)
      try {
        if (target.kind === 'cube') {
          await editCubeDimension(target.cube, target.dim, edit)
          if (successNote) setNotice(successNote)
        } else {
          const result = await editDimensionById(target.id, edit)
          if (result.fanned_out_to.length > 0) {
            setNotice(`Updated, and applied to ${result.fanned_out_to.join(', ')}.`)
          } else if (successNote) {
            setNotice(successNote)
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

  // Run a SEQUENCE of structural edits in order, reloading the tree once at the
  // end. This backs a multi-op move (e.g. remove_child then add_child then
  // reorder): the editDimension API is one-op-per-call, so a multi-op move is NOT
  // atomic - if a later op fails, the earlier ops have already committed; we stop
  // at the first failure, surface its error, and still reload so the UI reflects
  // whatever did commit. Empty / single-op lists are handled by the callers.
  const runEdits = useCallback(
    async (edits: DimensionEdit[], successNote?: string): Promise<boolean> => {
      if (edits.length === 0) return true
      setBusy(true)
      setError(null)
      setNotice(null)
      let ok = true
      // How many ops have already committed; on a mid-sequence failure this tells
      // the user the move is in a partial state (the API is one-op-per-call, so the
      // earlier ops are NOT rolled back).
      let committed = 0
      const fannedOut = new Set<string>()
      try {
        for (const edit of edits) {
          if (target.kind === 'cube') {
            await editCubeDimension(target.cube, target.dim, edit)
          } else {
            const result = await editDimensionById(target.id, edit)
            for (const c of result.fanned_out_to) fannedOut.add(c)
          }
          committed += 1
        }
      } catch (e) {
        ok = false
        const detail = e instanceof Error ? e.message : 'Could not apply the change'
        // Be honest that this multi-step move is non-atomic: earlier ops already
        // committed and were not rolled back, so the model is in a partial state.
        const partial =
          committed > 0 && edits.length > 1
            ? ` ${committed} of ${edits.length} steps of this move already applied and were not undone, so the member may be in a partial state.`
            : ''
        setError(`${detail}${partial}`)
      } finally {
        if (ok && fannedOut.size > 0) {
          setNotice(`Updated, and applied to ${[...fannedOut].join(', ')}.`)
        } else if (ok && successNote) {
          // Announce the successful move for a cube target too (no fan-out note).
          setNotice(successNote)
        }
        load()
        onChanged?.()
        setBusy(false)
      }
      return ok
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

  // When `parent` is a leaf/string, adding a child converts it to a consolidation
  // and clears any value stored directly on it, so confirm first; a parent that is
  // already a consolidation is purely additive and needs no confirmation. Resolves
  // false if the user cancels. Shared by addChild and moveMember so an as-child
  // drop onto a leaf gets the same warning whether it is additive or a move.
  const confirmConsolidationConversion = useCallback(
    async (parent: string, child: string): Promise<boolean> => {
      const parentKind = kindOf.get(parent)
      if (parentKind && parentKind !== 'consolidated') {
        return confirm({
          title: `Add "${child}" as a child of "${parent}"`,
          description: `"${parent}" becomes a Consolidation, which is calculated from its children, so any value stored directly on "${parent}" will be cleared. Continue?`,
          confirmLabel: 'Add as child',
          danger: true,
        })
      }
      return true
    },
    [confirm, kindOf],
  )

  // Add `child` to consolidation `parent` ADDITIVELY: the child keeps its existing
  // parents (a member may roll up to several consolidations), unlike a reparent
  // (Move to) which moves it. Backs the "Copy to" menu action and the "add member as
  // child" inline form; the drag as-child gesture is a MOVE and goes through
  // moveMember instead.
  const addChild = useCallback(
    async (parent: string, child: string): Promise<boolean> => {
      if (parent === child) return false
      if (!(await confirmConsolidationConversion(parent, child))) return false
      return runEdit({ op: 'add_child', parent, child }, `Added "${child}" to "${parent}"`)
    },
    [confirmConsolidationConversion, runEdit],
  )

  // Remove just the one `parent -> child` edge (keep the member, its data, and its
  // other parents). A no-op when the member has no parent on the row it came from.
  const removeChild = useCallback(
    (parent: string | null, child: string): Promise<boolean> => {
      if (!parent) return Promise.resolve(false)
      return runEdit({ op: 'remove_child', parent, child }, `Removed "${child}" from "${parent}"`)
    },
    [runEdit],
  )

  // Parent-aware MOVE resolver: emit the minimal, idempotent, duplicate-free op set
  // for moving the dragged occurrence `moved` (dragged out of `fromParent`, null when
  // it was a root) to a destination derived from the target row, by diffing the
  // source parent against the desired target. ALWAYS removes only `fromParent` so the
  // member keeps its OTHER parents (the multi-parent rule). A multi-op move is NOT
  // atomic (one op per API call); runEdits stops at the first failure and reloads once.
  //
  // - as-child onto `target` (MOVE): if fromParent is null -> add_child(target). If
  //   fromParent != target -> add_child(target) then remove_child(fromParent). If
  //   fromParent == target -> no-op. A leaf/string target is converted (confirmed).
  // - before/after a sibling under the SAME parent (toParent === fromParent, both may
  //   be null=root): a pure within-list reorder.
  // - before/after a row under a DIFFERENT parent: add_child(toParent) [skip if
  //   toParent null=root] + remove_child(fromParent) [skip if root] + reorder to position.
  //
  // The ADDITIVE op (add_child) is always emitted BEFORE the destructive remove_child:
  // a multi-op move is not atomic, so on a mid-sequence failure the member is left
  // double-parented (recoverable) rather than orphaned (lost from every parent).
  const moveMember = useCallback(
    async (
      moved: string,
      fromParent: string | null,
      zone: DropZone,
      targetName: string,
      toParent: string | null,
    ): Promise<void> => {
      if (moved === targetName) return

      if (zone === 'as-child') {
        if (fromParent === targetName) return // already a child here: no-op
        if (!(await confirmConsolidationConversion(targetName, moved))) return
        // Add to the target FIRST, then remove from the source: a mid-sequence
        // failure leaves the member double-parented (recoverable), not orphaned.
        const edits: DimensionEdit[] = [{ op: 'add_child', parent: targetName, child: moved }]
        if (fromParent) edits.push({ op: 'remove_child', parent: fromParent, child: moved })
        await runEdits(edits, `Moved "${moved}" into "${targetName}"`)
        return
      }

      // before / after a target row.
      const newOrder = orderMoving(moved, targetName, zone)
      if (toParent === fromParent) {
        // Same parent (or both root): a pure reorder within the element list.
        await runEdits([{ op: 'reorder', new_order: newOrder }], `Moved "${moved}"`)
        return
      }
      // Dropped at a top-level position. A member becomes a standalone root only
      // when fromParent was its LAST parent; if it still rolls up under other
      // consolidations it cannot be both rooted and a child (the model has no
      // separate "top-level" flag), so this just detaches the dragged occurrence -
      // it stays under its other parents, with an honest notice and no reorder
      // (which would only re-sort it under those other parents, not root it).
      if (!toParent) {
        const others = (parentsOf.get(moved) ?? []).filter((p) => p !== fromParent)
        if (fromParent && others.length > 0) {
          await runEdits(
            [{ op: 'remove_child', parent: fromParent, child: moved }],
            `Removed "${moved}" from "${fromParent}" (still rolls up under ${others.join(
              ', ',
            )}, so it is not a top-level member)`,
          )
          return
        }
        const rootEdits: DimensionEdit[] = []
        if (fromParent) rootEdits.push({ op: 'remove_child', parent: fromParent, child: moved })
        rootEdits.push({ op: 'reorder', new_order: newOrder })
        await runEdits(rootEdits, `Moved "${moved}" to the top level`)
        return
      }
      // Into a target consolidation. Add to the new parent FIRST, then remove from
      // the old one, then reorder: a mid-sequence failure leaves the member double-
      // parented (recoverable), not orphaned.
      if (!(await confirmConsolidationConversion(toParent, moved))) return
      const edits: DimensionEdit[] = [{ op: 'add_child', parent: toParent, child: moved }]
      if (fromParent) edits.push({ op: 'remove_child', parent: fromParent, child: moved })
      edits.push({ op: 'reorder', new_order: newOrder })
      await runEdits(edits, `Moved "${moved}" into "${toParent}"`)
    },
    [confirmConsolidationConversion, orderMoving, runEdits, parentsOf],
  )

  // Resolve a drop against the target row actually under the pointer: dispatch to
  // moveMember with the dragged occurrence's source parent (drag.parent) and the
  // target row's parent so the move is parent-aware (MOVE semantics, not additive).
  const doDrop = useCallback(
    (moved: string, fromParent: string | null, target: FlatRow, zone: DropZone) => {
      void moveMember(moved, fromParent, zone, target.name, target.parent)
    },
    [moveMember],
  )

  // Compute the drop zone from the pointer position within a row. The as-child band
  // is the middle ~50% (a large, easy-to-hit container target); before/after are
  // thin ~25% edge bands so an insertion line is still reachable.
  const zoneFor = (e: DragEvent<HTMLElement>): DropZone => {
    const rect = e.currentTarget.getBoundingClientRect()
    const y = e.clientY - rect.top
    if (y < rect.height * 0.25) return 'before'
    if (y > rect.height * 0.75) return 'after'
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
    setTopOver(false)
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
      // Gate on `busy` like the drag and dialog paths: a prior edit reloads the
      // dimension asynchronously, so firing another reorder against the stale
      // snapshot (held arrow key or rapid menu clicks) races and is non-cumulative.
      if (busy) return
      setMenuPath(null)
      const order = (dimension?.elements ?? []).map((e) => e.name)
      const at = order.indexOf(name)
      if (at === -1) return
      const swap = dir === 'up' ? at - 1 : at + 1
      if (swap < 0 || swap >= order.length) return
      ;[order[at], order[swap]] = [order[swap], order[at]]
      // Announce the member's new 1-based position so a keyboard pick-up move (and
      // the row menu's Move up/down) confirms the result, not just silently applies.
      void runEdit(
        { op: 'reorder', new_order: order },
        `Moved "${name}" to position ${swap + 1} of ${order.length}`,
      )
    },
    [busy, dimension, runEdit],
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

  // Open the inline add form for a new member at the end of the list (no ref row).
  // Shared by the toolbar "+ Add member" button and the empty-state action.
  const startAddAtEnd = () => {
    setMenuPath(null)
    setAdding({ at: 'after', ref: null })
    setAddName('')
    setAddKind('numeric')
  }

  const commitAdd = useCallback(async () => {
    // Gate on busy like the button already is: the Enter-key path is otherwise
    // ungated and a double Enter would POST a duplicate insert (and a spurious error).
    if (busy) return
    if (!adding) return
    const name = addName.trim()
    if (name === '') {
      setError('Give the new member a name.')
      return
    }
    // Validate the duplicate locally: the kindOf map already knows every existing
    // member name, so catch a clash here with a friendly message instead of a wasted
    // round-trip that surfaces a raw internal-vocabulary API error.
    if (kindOf.has(name)) {
      setError(`A member named "${name}" already exists.`)
      return
    }
    let ok: boolean
    if (adding.at === 'as-child' && adding.ref) {
      // Insert at the end, then add it as a child of the chosen member additively
      // (which keeps any other members the parent already holds and converts a
      // leaf/string parent to a consolidation). Two ops, NOT atomic. Confirm the
      // leaf->consolidation conversion FIRST (before inserting), so a declined
      // confirm leaves nothing stranded; then run BOTH ops through runEdits so its
      // partial-failure messaging applies and a mid-sequence add_child failure does
      // not strand the inserted member as a silent top-level orphan with the form
      // still open (re-submitting the same name would then hit a duplicate error).
      if (!(await confirmConsolidationConversion(adding.ref, name))) return
      ok = await runEdits(
        [
          { op: 'insert', name, kind: addKind, position: { at: 'end' } },
          { op: 'add_child', parent: adding.ref, child: name },
        ],
        `Added "${name}" to "${adding.ref}"`,
      )
    } else {
      const position: InsertPosition = adding.ref
        ? { at: adding.at as 'before' | 'after', ref: adding.ref }
        : { at: 'end' }
      ok = await runEdit({ op: 'insert', name, kind: addKind, position }, `Added "${name}"`)
    }
    if (ok) setAdding(null)
  }, [busy, adding, addName, addKind, kindOf, confirmConsolidationConversion, runEdit, runEdits])

  const convert = useCallback(
    async (name: string, kind: ElementKind) => {
      if (busy) return
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
          description:
            kind === 'consolidated'
              ? `A Consolidation is calculated from its children, so any value stored directly on "${name}" will be cleared. Continue?`
              : `Changing the kind of "${name}" clears any stored value that does not fit the new kind. Continue?`,
          confirmLabel: 'Convert',
          danger: true,
        })
        if (!ok) return
      }
      void runEdit(
        { op: 'set_kind', element: name, kind },
        `Converted "${name}" to ${KIND_LABEL[kind]}`,
      )
    },
    [busy, confirm, kindOf, runEdit],
  )

  // Remove the member from ONE consolidation (the row's current parent), keeping
  // it under any other parents. Distinct from Detach (all parents) and Delete.
  const removeFromConsolidation = useCallback(
    (parent: string | null, name: string) => {
      if (busy) return
      setMenuPath(null)
      void removeChild(parent, name)
    },
    [busy, removeChild],
  )

  // Pin a member to the top level (ADR-0038): it is shown as a display root in
  // addition to wherever it rolls up. Changes no rollup edge or stored value.
  const pinToTop = useCallback(
    (name: string) => {
      if (busy) return
      setMenuPath(null)
      void runEdit({ op: 'pin_to_top', element: name }, `Pinned "${name}" to the top level`)
    },
    [busy, runEdit],
  )

  // Remove a member's top-level pin (ADR-0038). It drops off the top level unless it
  // also has no parent (a natural root). Changes no rollup edge or stored value.
  const unpinFromTop = useCallback(
    (name: string) => {
      if (busy) return
      setMenuPath(null)
      void runEdit(
        { op: 'unpin_from_top', element: name },
        `Unpinned "${name}" from the top level`,
      )
    },
    [busy, runEdit],
  )

  // "Copy to": add the member to a chosen consolidation from the row menu's submenu
  // (the no-drag equivalent of dropping into the middle zone), ADDITIVE so it keeps
  // all its other parents (a copy, not a move). If the member is currently a display
  // root SOLELY because it has no parent (no parents AND not already pinned), also
  // pin it so the copy KEEPS it at the top level: a user copying a top-level member
  // into a consolidation expects it to stay at the top, but add_child alone would
  // give it a parent and drop it off the top (ADR-0038). A member that already has
  // parents or is already pinned stays a plain add_child.
  const copyTo = useCallback(
    async (parent: string, name: string) => {
      if (busy) return
      setMenuPath(null)
      if (parent === name) return
      const hasNoParents = (parentsOf.get(name) ?? []).length === 0
      if (hasNoParents && !pinnedOf.has(name)) {
        if (!(await confirmConsolidationConversion(parent, name))) return
        await runEdits(
          [
            { op: 'add_child', parent, child: name },
            { op: 'pin_to_top', element: name },
          ],
          `Copied "${name}" to "${parent}" (kept at the top level)`,
        )
        return
      }
      void addChild(parent, name)
    },
    [busy, parentsOf, pinnedOf, confirmConsolidationConversion, runEdits, addChild],
  )

  // "Move to": reparent the member so it ends up under exactly `target` (a
  // consolidation name) or at the top level (`target === null`), losing all its
  // current parents. Because reparent drops EVERY current parent, if the member
  // currently has more than one parent we confirm first (naming the parents it would
  // be removed from), so a destructive collapse of alternate rollups is explicit; a
  // 0- or 1-parent move is the expected, unsurprising case and needs no confirm.
  const moveTo = useCallback(
    async (name: string, target: string | null) => {
      if (busy) return
      setMenuPath(null)
      const parents = parentsOf.get(name) ?? []
      if (parents.length > 1) {
        const dest = target === null ? 'the top level' : `"${target}"`
        const ok = await confirm({
          title: target === null ? `Move "${name}" to the top level` : `Move "${name}" to "${target}"`,
          description: `"${name}" currently rolls up into ${parents.length} consolidations (${parents.join(
            ', ',
          )}). Moving it removes it from all of them so it ends up under only ${dest}. Continue?`,
          confirmLabel: target === null ? 'Move to top level' : 'Move here',
          danger: true,
        })
        if (!ok) return
      }
      void runEdit(
        { op: 'reparent', child: name, new_parent: target },
        target === null ? `Moved "${name}" to the top level` : `Moved "${name}" into "${target}"`,
      )
    },
    [busy, confirm, parentsOf, runEdit],
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
        description: `This removes "${name}" from every consolidation it belongs to and permanently deletes all data stored on it. This cannot be undone here.`,
        confirmLabel: 'Delete from dimension',
        danger: true,
      })
      if (!ok) return
      void runEdit({ op: 'delete', element: name }, `Deleted "${name}"`)
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
      // Announce each unlink (the last success note survives the reload); naming
      // the parent keeps it specific for a single-edge case.
      const ok = await runEdit(
        { op: 'remove_child', parent, child: name },
        `Removed "${name}" from "${parent}"`,
      )
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
        Loading dimension...
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
          to copy it to a consolidation, move it to a consolidation or the top level,
          remove it from one, convert, or delete it.
        </p>
        {picked ? (
          <p className="banner" role="status">
            Picked up "{picked.name}". Use Arrow Up or Arrow Down to move it one
            position; Space drops it, Escape cancels.
          </p>
        ) : null}

        <div className="dimedit__toolbar">
          <span className="muted">
            {count} {count === 1 ? 'member' : 'members'}
          </span>
          <div className="dimedit__toolbar-actions">
            {expandablePaths.length > 0 ? (
              <>
                <button
                  type="button"
                  className="dimedit__addroot"
                  disabled={busy}
                  onClick={() => setExpanded(new Set(expandablePaths))}
                >
                  Expand all
                </button>
                <button
                  type="button"
                  className="dimedit__addroot"
                  disabled={busy}
                  onClick={() => setExpanded(new Set())}
                >
                  Collapse all
                </button>
              </>
            ) : null}
            <button
              type="button"
              className="dimedit__addroot"
              disabled={busy}
              onClick={startAddAtEnd}
            >
              + Add member
            </button>
          </div>
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
                  // Mirror the working PivotFields pattern: set both a payload and
                  // effectAllowed so Firefox actually starts the drag and the cursor
                  // shows a move.
                  e.dataTransfer.setData('text/plain', r.name)
                  e.dataTransfer.effectAllowed = 'move'
                }}
                onDragEnd={endDrag}
                onDragOver={(e) => {
                  if (!drag || drag.name === r.name) return
                  e.preventDefault()
                  e.stopPropagation()
                  e.dataTransfer.dropEffect = 'move'
                  setRootOver(false)
                  let z = zoneFor(e)
                  // Suppress the as-child indicator on a cycle-forming target (the
                  // dragged member's own descendant): nudge it to the nearer edge so
                  // the user sees a valid before/after insertion instead. The backend
                  // is still the source of truth and rejects an actual cycle.
                  if (z === 'as-child' && dragDescendants.has(r.name)) {
                    const rect = e.currentTarget.getBoundingClientRect()
                    z = e.clientY - rect.top < rect.height / 2 ? 'before' : 'after'
                  }
                  scheduleHoverExpand(r)
                  setOver((cur) =>
                    cur?.path === r.path && cur.zone === z ? cur : { path: r.path, zone: z },
                  )
                }}
                onDrop={(e) => {
                  e.preventDefault()
                  e.stopPropagation()
                  // Recompute the zone from the event so the drop lands where the
                  // pointer actually is, not a stale `over` (which can lag the last
                  // dragOver, e.g. after a hover-auto-expand shifted rows).
                  if (drag) {
                    let z = zoneFor(e)
                    if (z === 'as-child' && dragDescendants.has(r.name)) {
                      const rect = e.currentTarget.getBoundingClientRect()
                      z = e.clientY - rect.top < rect.height / 2 ? 'before' : 'after'
                    }
                    doDrop(drag.name, drag.parent, r, z)
                  }
                  endDrag()
                }}
              >
                <RowActions
                  style={{ paddingInlineStart: `${r.depth * 1.2 + 0.25}rem` }}
                  name={r.name}
                  kind={r.kind}
                  isRoot={r.parent === null}
                  pinned={pinnedOf.has(r.name)}
                  currentParent={r.parent}
                  consolidations={consolidations}
                  memberParents={parentsOf.get(r.name) ?? []}
                  excludeTargets={descendantsByName.get(r.name) ?? EMPTY_SET}
                  open={menuPath === r.path}
                  onOpenChange={(o) => setMenuPath(o ? r.path : null)}
                  onMoveUp={() => moveStep(r.name, 'up')}
                  onMoveDown={() => moveStep(r.name, 'down')}
                  onAddBefore={() => startAdd('before', r.name)}
                  onAddAfter={() => startAdd('after', r.name)}
                  onAddChild={() => startAdd('as-child', r.name)}
                  onCopyTo={(parent) => void copyTo(parent, r.name)}
                  onMoveTo={(target) => void moveTo(r.name, target)}
                  onPinToTop={() => pinToTop(r.name)}
                  onUnpinFromTop={() => unpinFromTop(r.name)}
                  onConvert={(k) => void convert(r.name, k)}
                  onRemoveFromConsolidation={() => removeFromConsolidation(r.parent, r.name)}
                  onRemoveFrom={() => openRemoveFrom(r.name)}
                  onDeleteFromDimension={() => void deleteFromDimension(r.name)}
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
                  {/* A pinned member that ALSO has a parent gets a subtle marker so it
                      reads as distinct from a natural no-parent root: this occurrence is
                      shown at the top level by an explicit pin, not because it is
                      parentless (ADR-0038). A pinned no-parent member needs no marker -
                      it would be a root regardless. */}
                  {pinnedOf.has(r.name) && r.parent !== null ? (
                    <span
                      className="dimedit__pinbadge"
                      title="Pinned to the top level"
                      aria-label="Pinned to the top level"
                    >
                      (pinned)
                    </span>
                  ) : null}
                  <span className="dimedit__kind">{KIND_LABEL[r.kind]}</span>
                </RowActions>
              </li>
            )
          })}
          {rows.length === 0 ? (
            <li role="none" className="dimedit__empty">
              <EmptyState
                icon="◇"
                title="No members yet"
                action={
                  <button
                    type="button"
                    className="dimedit__btn dimedit__btn--primary"
                    disabled={busy}
                    onClick={startAddAtEnd}
                  >
                    + Add member
                  </button>
                }
              >
                A dimension is a list of members - the rows and columns a cube is
                sliced by. Add the first member to start building this dimension.
              </EmptyState>
            </li>
          ) : null}
        </ul>

        {/* Drag-to-top-level drop zone (ADR-0038, discoverable target). Always
            present below the list (the bare <ul> area is hard to hit when the list
            is full); idle it shows a hint, while dragging a member it lights up and
            invites a drop. Dropping PINS the member to the top level (it stays under
            its consolidations and keeps its rollups) - the new "to the top level"
            semantics via the explicit zone, distinct from the right-click "Remove
            from <parent>" / "Move to -> Top level" detach/reparent actions. */}
        <div
          className={`dimedit__topzone${drag ? ' is-dragging' : ''}${topOver ? ' is-over' : ''}`}
          role="button"
          aria-label={
            drag
              ? `Drop here to show "${drag.name}" at the top level`
              : 'Drag a member here to also show it at the top level'
          }
          onDragOver={(e) => {
            if (!drag) return
            e.preventDefault()
            e.stopPropagation()
            e.dataTransfer.dropEffect = 'copy'
            setRootOver(false)
            setOver(null)
            clearHoverExpand()
            if (!topOver) setTopOver(true)
          }}
          onDragLeave={(e) => {
            if (e.target === e.currentTarget) setTopOver(false)
          }}
          onDrop={(e) => {
            if (drag) {
              e.preventDefault()
              e.stopPropagation()
              void runEdit(
                { op: 'pin_to_top', element: drag.name },
                `Pinned "${drag.name}" to the top level`,
              )
            }
            endDrag()
          }}
        >
          {drag
            ? `Drop here to show "${drag.name}" at the top level`
            : 'Drag a member here to also show it at the top level'}
        </div>

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
          title={removeFrom ? `Remove "${removeFrom.name}" from consolidations` : ''}
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
interface RowActionProps {
  name: string
  kind: ElementKind
  isRoot: boolean
  /** Whether this member is explicitly pinned to the top level (ADR-0038), so the
   * menu shows the matching pin/unpin toggle. */
  pinned: boolean
  currentParent: string | null
  consolidations: string[]
  /** The consolidations this member is currently a direct child of (its parents).
   * Drives the Copy to "(already contains)" disable and the Move to no-op disables. */
  memberParents: string[]
  /** The member's transitive descendants: excluded from Copy to / Move to because an
   * add there would form a cycle (the backend rejects it regardless). */
  excludeTargets: ReadonlySet<string>
  onMoveUp: () => void
  onMoveDown: () => void
  onAddBefore: () => void
  onAddAfter: () => void
  onAddChild: () => void
  /** Copy to a consolidation: additive add_child, the member keeps its other parents. */
  onCopyTo: (parent: string) => void
  /** Move to a consolidation (target = name) or to the top level (target = null):
   * reparent, the member loses its other parents. */
  onMoveTo: (target: string | null) => void
  /** Pin the member to the top level (ADR-0038): shown as a display root in
   * addition to its rollups; no edge or value changes. */
  onPinToTop: () => void
  /** Remove the member's top-level pin (ADR-0038). */
  onUnpinFromTop: () => void
  onConvert: (kind: ElementKind) => void
  onRemoveFromConsolidation: () => void
  onRemoveFrom: () => void
  onDeleteFromDimension: () => void
}

// Radix dropdown-menu and context-menu expose the same Item/Sub/Separator API, so
// the action list is authored once (`actionItems`) and rendered with whichever set
// of primitives the trigger needs: the ⋯ button uses dropdown-menu, right-click uses
// context-menu (which anchors at the cursor). CM is cast to the shared shape.
type MenuParts = Pick<typeof DM, 'Item' | 'Sub' | 'SubTrigger' | 'SubContent' | 'Portal' | 'Separator'>
const CM_PARTS = CM as unknown as MenuParts

function actionItems(M: MenuParts, p: RowActionProps) {
  // Consolidations this member could be copied/moved into: every consolidation except
  // the member itself and its transitive descendants (an add there would form a cycle).
  const targets = p.consolidations.filter((c) => c !== p.name && !p.excludeTargets.has(c))
  // The member's current direct parents, for the no-op disable logic below.
  const parentSet = new Set(p.memberParents)
  return (
    <>
      <M.Item className="menu__item" onSelect={p.onMoveUp}>
        Move up
      </M.Item>
      <M.Item className="menu__item" onSelect={p.onMoveDown}>
        Move down
      </M.Item>
      <M.Separator className="menu__sep" />
      <M.Item className="menu__item" onSelect={p.onAddBefore}>
        Add member before
      </M.Item>
      <M.Item className="menu__item" onSelect={p.onAddAfter}>
        Add member after
      </M.Item>
      <M.Item className="menu__item" onSelect={p.onAddChild}>
        Add member as child
      </M.Item>
      {targets.length > 0 ? (
        // Copy to: additive add_child, the member KEEPS its existing parents. A target
        // it already sits under is disabled (a duplicate edge is a no-op).
        <M.Sub>
          <M.SubTrigger className="menu__item menu__item--sub">Copy to</M.SubTrigger>
          <M.Portal>
            <M.SubContent className="menu" sideOffset={2} alignOffset={-4}>
              {targets.map((parent) => {
                const already = parentSet.has(parent)
                return (
                  <M.Item
                    key={parent}
                    className="menu__item"
                    disabled={already}
                    onSelect={() => p.onCopyTo(parent)}
                  >
                    {already ? `${parent} (already contains)` : parent}
                  </M.Item>
                )
              })}
            </M.SubContent>
          </M.Portal>
        </M.Sub>
      ) : null}
      <M.Sub>
        {/* Move to: reparent, the member ends up under EXACTLY the chosen target and
            loses its other parents. A target that is already its ONLY parent is a
            no-op; "Top level" is a no-op when the member is already a root. */}
        <M.SubTrigger className="menu__item menu__item--sub">Move to</M.SubTrigger>
        <M.Portal>
          <M.SubContent className="menu" sideOffset={2} alignOffset={-4}>
            {targets.map((parent) => {
              // No-op only when this target is the member's sole current parent.
              const onlyHere = p.memberParents.length === 1 && parentSet.has(parent)
              return (
                <M.Item
                  key={parent}
                  className="menu__item"
                  disabled={onlyHere}
                  onSelect={() => p.onMoveTo(parent)}
                >
                  {onlyHere ? `${parent} (already there)` : parent}
                </M.Item>
              )
            })}
            <M.Separator className="menu__sep" />
            <M.Item className="menu__item" disabled={p.isRoot} onSelect={() => p.onMoveTo(null)}>
              {p.isRoot ? 'Top level (already at top level)' : 'Top level'}
            </M.Item>
          </M.SubContent>
        </M.Portal>
      </M.Sub>
      {/* Pin/unpin to the top level (ADR-0038): show the member as a display root in
          addition to its rollups, without changing any edge or value. A toggle that
          reads the member's current pinned state. */}
      {p.pinned ? (
        <M.Item className="menu__item" onSelect={p.onUnpinFromTop}>
          Unpin from top level
        </M.Item>
      ) : (
        <M.Item className="menu__item" onSelect={p.onPinToTop}>
          Pin to top level
        </M.Item>
      )}
      <M.Separator className="menu__sep" />
      <M.Item
        className="menu__item"
        disabled={p.kind === 'numeric'}
        onSelect={() => p.onConvert('numeric')}
      >
        Convert to Numeric
      </M.Item>
      <M.Item
        className="menu__item"
        disabled={p.kind === 'string'}
        onSelect={() => p.onConvert('string')}
      >
        Convert to String
      </M.Item>
      <M.Item
        className="menu__item"
        disabled={p.kind === 'consolidated'}
        onSelect={() => p.onConvert('consolidated')}
      >
        Convert to Consolidation
      </M.Item>
      <M.Separator className="menu__sep" />
      <M.Item className="menu__item" disabled={p.isRoot} onSelect={p.onRemoveFromConsolidation}>
        {p.isRoot ? 'Remove from this consolidation' : `Remove from "${p.currentParent}"`}
      </M.Item>
      <M.Separator className="menu__sep" />
      {p.isRoot ? (
        // A root has no parents to choose between, so Delete goes straight to the
        // destructive "from the whole dimension" path (user direction).
        <M.Item className="menu__item menu__item--danger" onSelect={p.onDeleteFromDimension}>
          Delete from dimension
        </M.Item>
      ) : (
        // A child can be removed from a consolidation (kept, with its data) or deleted
        // from the dimension entirely (destructive). Offer the choice.
        <M.Sub>
          <M.SubTrigger className="menu__item menu__item--sub menu__item--danger">
            Delete...
          </M.SubTrigger>
          <M.Portal>
            <M.SubContent className="menu" sideOffset={2} alignOffset={-4}>
              <M.Item className="menu__item" onSelect={p.onRemoveFrom}>
                Remove from consolidations...
              </M.Item>
              <M.Separator className="menu__sep" />
              <M.Item className="menu__item menu__item--danger" onSelect={p.onDeleteFromDimension}>
                Delete from dimension
              </M.Item>
            </M.SubContent>
          </M.Portal>
        </M.Sub>
      )}
    </>
  )
}

/** A dimension-editor row's visual content, wrapped so a RIGHT-CLICK opens the action
 * menu at the cursor (context-menu), plus the always-visible ⋯ button that opens the
 * same items anchored to itself (dropdown-menu, controlled by `open` so the keyboard
 * ContextMenu/F10 path can open it too). */
function RowActions({
  style,
  children,
  open,
  onOpenChange,
  ...p
}: RowActionProps & {
  style?: CSSProperties
  children: ReactNode
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  return (
    <CM.Root>
      <CM.Trigger asChild>
        <div className="dimedit__rowinner" style={style}>
          {children}
          <DM.Root open={open} onOpenChange={onOpenChange}>
            <DM.Trigger asChild>
              <button
                type="button"
                className="dimedit__actions"
                aria-label={`Actions for ${p.name}`}
                tabIndex={-1}
                onClick={(e) => e.stopPropagation()}
                onKeyDown={(e) => e.stopPropagation()}
              >
                ⋯
              </button>
            </DM.Trigger>
            <DM.Portal>
              <DM.Content className="menu" align="end" sideOffset={4}>
                {actionItems(DM, p)}
              </DM.Content>
            </DM.Portal>
          </DM.Root>
        </div>
      </CM.Trigger>
      <CM.Portal>
        <CM.Content className="menu">{actionItems(CM_PARTS, p)}</CM.Content>
      </CM.Portal>
    </CM.Root>
  )
}
