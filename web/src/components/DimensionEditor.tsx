import * as DM from '@radix-ui/react-dropdown-menu'
import { useCallback, useEffect, useMemo, useState, type DragEvent } from 'react'
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
import { Card, Input, Select, useConfirm } from '../ui'

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
 * keys the row while `name` drives every edit. */
interface FlatRow {
  name: string
  kind: ElementKind
  depth: number
  hasChildren: boolean
  expanded: boolean
  path: string
}

/**
 * The standalone, cube-agnostic, hierarchy-only dimension editor (ADR-0036).
 * Members are rows in a tree: each row is draggable and drives structural edits
 * (reorder / reparent / set kind / delete / insert) through the new endpoints.
 *
 * Drag-and-drop drop zones: while dragging a member over a row, the row splits
 * into thirds. The top third places the dragged member BEFORE the target, the
 * bottom third places it AFTER (both compute the full new member order and POST
 * a `reorder`), and the middle third places it AS A CHILD of the target (POST a
 * `reparent`, which the backend turns the target into a consolidation for).
 * Dropping onto the empty area below the list detaches the member to a root.
 *
 * Right-click (or the row's "..." button) opens a menu: Add member before / after
 * / as child, Convert to Numeric / String / Consolidation, Detach, and Delete.
 * The add actions take a name + kind inline. A delete, and any convert that would
 * drop stored values, confirm first.
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
  // Drag state: the member being dragged, plus the row + zone under the pointer.
  const [dragName, setDragName] = useState<string | null>(null)
  const [over, setOver] = useState<{ path: string; zone: DropZone } | null>(null)
  const [rootOver, setRootOver] = useState(false)
  // The row whose context menu is open, and an in-progress inline add form.
  const [menuPath, setMenuPath] = useState<string | null>(null)
  const [adding, setAdding] = useState<{
    at: 'before' | 'after' | 'as-child'
    ref: string | null
  } | null>(null)
  const [addName, setAddName] = useState('')
  const [addKind, setAddKind] = useState<ElementKind>('numeric')

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

  // Resolve a drop: before/after -> reorder; as-child -> reparent onto target.
  const doDrop = useCallback(
    (moved: string, targetName: string, zone: DropZone) => {
      if (moved === targetName) return
      if (zone === 'as-child') {
        void runEdit({ op: 'reparent', child: moved, new_parent: targetName })
      } else {
        void runEdit({ op: 'reorder', new_order: orderMoving(moved, targetName, zone) })
      }
    },
    [runEdit, orderMoving],
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
      // Insert at the end, then reparent it under the chosen member (which the
      // backend converts to a consolidation). Two committed edits.
      const inserted = await runEdit({
        op: 'insert',
        name,
        kind: addKind,
        position: { at: 'end' },
      })
      ok = inserted
        ? await runEdit({ op: 'reparent', child: name, new_parent: adding.ref })
        : false
    } else {
      const position: InsertPosition = adding.ref
        ? { at: adding.at as 'before' | 'after', ref: adding.ref }
        : { at: 'end' }
      ok = await runEdit({ op: 'insert', name, kind: addKind, position })
    }
    if (ok) setAdding(null)
  }, [adding, addName, addKind, runEdit])

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

  const detach = useCallback(
    (name: string) => {
      setMenuPath(null)
      void runEdit({ op: 'reparent', child: name, new_parent: null })
    },
    [runEdit],
  )

  const remove = useCallback(
    async (name: string) => {
      setMenuPath(null)
      if ((childCountOf.get(name) ?? 0) > 0) {
        setError(
          `"${name}" has members under it. Detach or delete those first, then delete "${name}".`,
        )
        return
      }
      const ok = await confirm({
        title: `Delete "${name}"`,
        body: `Delete the member "${name}"? Any values stored on it are removed. This cannot be undone here.`,
        confirmLabel: 'Delete',
        danger: true,
      })
      if (!ok) return
      void runEdit({ op: 'delete', element: name })
    },
    [confirm, childCountOf, runEdit],
  )

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

  return (
    <Card
      title={dimension.name}
      subtitle="Drag a member onto another to place it before, after, or inside. Right-click a member for more actions."
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
          className={`dimedit__tree${rootOver ? ' is-rootover' : ''}`}
          role="tree"
          aria-label={`Members of ${dimension.name}`}
          aria-busy={busy || undefined}
          onDragOver={(e) => {
            // Dropping in the list's own (non-row) area detaches to a root.
            if (dragName && e.target === e.currentTarget) {
              e.preventDefault()
              setRootOver(true)
              setOver(null)
            }
          }}
          onDragLeave={(e) => {
            if (e.target === e.currentTarget) setRootOver(false)
          }}
          onDrop={(e) => {
            if (dragName && e.target === e.currentTarget) {
              e.preventDefault()
              detach(dragName)
            }
            setRootOver(false)
            setOver(null)
            setDragName(null)
          }}
        >
          {rows.map((r) => {
            const isOver = over?.path === r.path
            const zone = isOver ? over?.zone : undefined
            return (
              <li
                key={r.path}
                role="treeitem"
                aria-level={r.depth + 1}
                aria-expanded={r.hasChildren ? r.expanded : undefined}
                className={`dimedit__row${dragName === r.name ? ' is-dragging' : ''}${
                  isOver ? ` is-over is-over--${zone}` : ''
                }`}
                draggable={!busy}
                onDragStart={(e) => {
                  setDragName(r.name)
                  e.dataTransfer.effectAllowed = 'move'
                }}
                onDragEnd={() => {
                  setDragName(null)
                  setOver(null)
                  setRootOver(false)
                }}
                onDragOver={(e) => {
                  if (!dragName || dragName === r.name) return
                  e.preventDefault()
                  e.stopPropagation()
                  setRootOver(false)
                  const z = zoneFor(e)
                  setOver((cur) =>
                    cur?.path === r.path && cur.zone === z ? cur : { path: r.path, zone: z },
                  )
                }}
                onDrop={(e) => {
                  e.preventDefault()
                  e.stopPropagation()
                  if (dragName && over) doDrop(dragName, r.name, over.zone)
                  setDragName(null)
                  setOver(null)
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
                      aria-label={r.expanded ? `Collapse ${r.name}` : `Expand ${r.name}`}
                      aria-expanded={r.expanded}
                      onClick={() => toggleExpand(r.path)}
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
                    open={menuPath === r.path}
                    onOpenChange={(o) => setMenuPath(o ? r.path : null)}
                    onAddBefore={() => startAdd('before', r.name)}
                    onAddAfter={() => startAdd('after', r.name)}
                    onAddChild={() => startAdd('as-child', r.name)}
                    onConvert={(k) => void convert(r.name, k)}
                    onDetach={() => detach(r.name)}
                    onDelete={() => void remove(r.name)}
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
      </div>
    </Card>
  )
}

// ---- per-row context / actions menu ----

/**
 * One member row's actions, as a controlled Radix dropdown anchored to a
 * keyboard-reachable "..." trigger. Right-clicking the row opens the same menu
 * (the editor sets `open`), so every structural action has both a pointer and a
 * keyboard/no-drag path (ADR-0036). The convert items are disabled for the
 * current kind so the menu reads as a clear state toggle.
 */
function RowMenu({
  name,
  kind,
  open,
  onOpenChange,
  onAddBefore,
  onAddAfter,
  onAddChild,
  onConvert,
  onDetach,
  onDelete,
}: {
  name: string
  kind: ElementKind
  open: boolean
  onOpenChange: (open: boolean) => void
  onAddBefore: () => void
  onAddAfter: () => void
  onAddChild: () => void
  onConvert: (kind: ElementKind) => void
  onDetach: () => void
  onDelete: () => void
}) {
  return (
    <DM.Root open={open} onOpenChange={onOpenChange}>
      <DM.Trigger asChild>
        <button
          type="button"
          className="dimedit__actions"
          aria-label={`Actions for ${name}`}
          onClick={(e) => e.stopPropagation()}
        >
          ⋯
        </button>
      </DM.Trigger>
      <DM.Portal>
        <DM.Content className="menu" align="end" sideOffset={4}>
          <DM.Item className="menu__item" onSelect={onAddBefore}>
            Add member before
          </DM.Item>
          <DM.Item className="menu__item" onSelect={onAddAfter}>
            Add member after
          </DM.Item>
          <DM.Item className="menu__item" onSelect={onAddChild}>
            Add member as child
          </DM.Item>
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
          <DM.Item className="menu__item" onSelect={onDetach}>
            Detach to top level
          </DM.Item>
          <DM.Item className="menu__item menu__item--danger" onSelect={onDelete}>
            Delete
          </DM.Item>
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  )
}
