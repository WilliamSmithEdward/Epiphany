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
  collectLoaders,
  cssEscape,
  flatten,
  rootNodes,
  searchVisibleChildren,
  selectionId,
  type ActionContext,
  type Node,
  type NodeAction,
  type Selection,
} from './modelExplorerTree'

// The node/selection types and the model-to-Node tree builders live in
// modelExplorerTree.ts (pure, unit-testable, no React). This component owns the
// React state/effects and the rendered tree, composing those builders. Re-export
// the public types so existing importers (CubeApp) keep their import path.
export type { ActionContext, NodeAction, Selection } from './modelExplorerTree'

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
  // Per-node load generation: every load() dispatch for a node id bumps its
  // counter and captures the value; the .then/.catch only commits if its capture
  // is still the latest. This makes concurrent dispatches for the same id (a
  // reload + a Retry, a double reload bump per delete, mount-vs-reload, or a
  // collapse mid-load) last-DISPATCH-wins rather than last-RESPONSE-wins, so an
  // older-but-faster response cannot clobber a newer one's children.
  const loadGen = useRef<Map<string, number>>(new Map())

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
    // Bump this node id's generation and capture it; a later dispatch for the same
    // id supersedes this one, and the guards below discard this response if so.
    const g = (loadGen.current.get(node.id) ?? 0) + 1
    loadGen.current.set(node.id, g)
    setLoading((s) => new Set(s).add(node.id))
    setErrors((m) => {
      if (!(node.id in m)) return m
      const n = { ...m }
      delete n[node.id]
      return n
    })
    setLiveMsg(`Loading ${node.label}...`)
    node
      .loader()
      .then((kids) => {
        if (loadGen.current.get(node.id) !== g) return // superseded by a newer load
        setChildren((m) => ({ ...m, [node.id]: kids }))
        setLiveMsg(`${node.label} loaded`)
      })
      .catch((err) => {
        if (loadGen.current.get(node.id) !== g) return // superseded by a newer load
        // Keep the failure out of childrenById so `toggle`'s re-expand guard
        // (`node.loader && !childrenById[node.id]`) re-fetches on collapse+expand.
        const msg = err instanceof Error ? err.message : String(err)
        setErrors((m) => ({ ...m, [node.id]: msg }))
        setLiveMsg(`Failed to load ${node.label}: ${msg}`)
      })
      .finally(() => {
        // Only the latest dispatch clears the spinner; a superseded one leaving it
        // off would hide the in-flight newer load.
        if (loadGen.current.get(node.id) !== g) return
        setLoading((s) => { const n = new Set(s); n.delete(node.id); return n })
      })
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

  // Reload the children of the relevant nodes when the model changes (a write
  // bumps reloadSignal), so the tree stays in sync after create/delete.
  //
  // Outside search, that is the nodes the user has manually expanded. DURING a
  // search, though, subtrees are opened via `searchExpand` and populated by the
  // eager-load effect, NOT by the manual `expanded` set - so they would be missed
  // here, leaving a gone/renamed object visible until the search box is cleared.
  // While searching, reload every actually-loaded node (the childrenById keys) so
  // the search result set re-derives over fresh children.
  useEffect(() => {
    if (reloadSignal === 0) return
    const inScope = searching
      ? (n: Node) => n.id in childrenById
      : (n: Node) => expanded.has(n.id)
    const reloadable = collectLoaders(roots, childrenById).filter(inScope)
    for (const n of reloadable) load(n)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadSignal])

  // On first mount, load the children of any initially-expanded node (the Cubes
  // root). Otherwise it renders expanded-but-empty until the user toggles it,
  // since `toggle` loads only on user expand and the reload effect above is gated
  // to model changes (reloadSignal > 0).
  useEffect(() => {
    for (const n of collectLoaders(roots, childrenById)) {
      if (expanded.has(n.id)) load(n)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

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
      // Opening a tab is decoupled from expanding: a row that opens a tab just
      // opens it and does NOT also toggle its children (expand with the twisty,
      // or the Right/Left arrow keys). A pure container row (no tab of its own,
      // e.g. the "Cubes"/"Flows" groups) still expands on click, since that is
      // its only action.
      if (node.selection) onSelect(node.selection)
      else if (node.loader) toggle(node)
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
      // A key on the focused row is handled once by that row; stop it bubbling to
      // ancestor <li> onKeyDown handlers (the same bubbling class as onContextMenu
      // and onClick), which would otherwise re-handle it for the wrong node.
      e.stopPropagation()
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
      // Highlight by the underlying selection, not the node id: a dimension shown
      // in two tree locations (the cube tree and the global list) has distinct ids
      // but the same selection, so selecting it highlights wherever it appears.
      const isSel = node.selection
        ? selectionId(node.selection) === selectedId
        : node.id === selectedId
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
                  // Stop the right-click bubbling to ancestor <li>s, or the last
                  // (root) handler would win and open the wrong (e.g. Cubes) menu,
                  // mirroring the onClick stopPropagation below.
                  e.stopPropagation()
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
            {isLoading ? <span className="tree__spinner" aria-hidden="true">...</span> : null}
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
          placeholder="Search..."
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
