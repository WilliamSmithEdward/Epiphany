import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  connectWs,
  deleteDimension,
  deleteFlow,
  deleteJob,
  deleteView,
  listCubes,
  logout,
  promoteDimension,
  runJob,
  type CubeSummary,
} from '../api/client'
import DimensionsWorkspace from './DimensionsWorkspace'
import FlowsWorkspace from './FlowsWorkspace'
import JobsWorkspace from './JobsWorkspace'
import ModelWorkspace from './ModelWorkspace'
import ModelExplorer, {
  type ActionContext,
  type NodeAction,
  type Selection,
} from './ModelExplorer'
import PivotGrid from './PivotGrid'
import RulesWorkspace from './RulesWorkspace'
import SetsManager from './SetsManager'
import SandboxBar from './SandboxBar'
import SecurityWorkspace from './SecurityWorkspace'
import ServerOverview from './ServerOverview'
import WelcomeCard from './WelcomeCard'
import ViewWorkspace from './ViewWorkspace'
import ChangePassword from './ChangePassword'
import {
  Badge,
  Button,
  CommandPalette,
  Dialog,
  EmptyState,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  ThemeToggle,
  Tooltip,
  useCommandPalette,
  useConfirm,
  type Command,
} from '../ui'

// The command-palette shortcut hint, shown platform-appropriately (the binding
// itself accepts Cmd or Ctrl; only the label differs). Avoids the Mac ⌘ symbol
// on Windows/Linux.
const IS_MAC =
  typeof navigator !== 'undefined' && /Mac|iPhone|iPad/i.test(navigator.platform || navigator.userAgent || '')
const PALETTE_HINT = IS_MAC ? '⌘K' : 'Ctrl K'

/** The per-tab navigation intent: the "open this specific item / start a new
 * one" hint a tree action carries into the destination workspace. `signal`
 * bumps on every navigation so a workspace re-applies the intent even when the
 * cube (and thus the component) is unchanged. */
interface NavIntent {
  signal: number
  autoNew?: boolean
  flow?: string
  view?: string
  job?: string
  dim?: string
  dimId?: number
}

/** One open tab: a selection plus the latest nav intent for its pane. */
interface Tab {
  id: string
  selection: Selection
  nav: NavIntent
}

/** A stable id for a selection, used as the tab key. Mirrors ModelExplorer's
 * selectionId so the tree's selected-row highlight tracks the active tab. */
function tabId(s: Selection): string {
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

/** The cube a selection targets, or null for cube-independent surfaces. */
function cubeOf(s: Selection | null): string | null {
  if (!s) return null
  switch (s.kind) {
    case 'cube':
    case 'cube-dimension':
    case 'cube-views':
    case 'view':
    case 'cube-rules':
    case 'flow':
    case 'schedule':
      return s.cube
    default:
      return null
  }
}

/** One breadcrumb step: a label plus the Selection to navigate to when an
 * ancestor crumb is clicked. `to` is null for the current page (last crumb)
 * and for non-navigable roots (e.g. the bare "Cubes" / "Administration" word).*/
interface Crumb {
  label: string
  to: Selection | null
}

/** Breadcrumb steps for the current selection (last one is the current page).
 * Ancestors carry a navigation target; the create/new states get a real label
 * (driven by the nav intent) so the current crumb is never blank. */
function crumbs(s: Selection | null, opts: { autoNew?: boolean } = {}): Crumb[] {
  const cubesRoot: Crumb = { label: 'Cubes', to: null }
  const dimsRoot: Crumb = { label: 'Dimensions', to: null }
  const flowsRoot: Crumb = { label: 'Flows', to: null }
  const schedRoot: Crumb = { label: 'Schedules', to: null }
  const adminRoot: Crumb = { label: 'Administration', to: null }
  if (!s) return [{ label: 'Home', to: null }]
  switch (s.kind) {
    case 'cube':
      return [cubesRoot, { label: s.cube, to: null }]
    case 'cube-dimension':
      // A named dim is a specific dimension under the cube. An empty dim means
      // either the new-cube wizard (autoNew) or the cube's model editor focused
      // on its first dimension ("Edit dimensions…").
      return s.dim
        ? [cubesRoot, { label: s.cube, to: { kind: 'cube', cube: s.cube } }, { label: s.dim, to: null }]
        : opts.autoNew
          ? [cubesRoot, { label: 'New cube', to: null }]
          : [cubesRoot, { label: s.cube, to: { kind: 'cube', cube: s.cube } }, { label: 'Dimensions', to: null }]
    case 'cube-views':
      return [cubesRoot, { label: s.cube, to: { kind: 'cube', cube: s.cube } }, { label: 'Views', to: null }]
    case 'view':
      return [
        cubesRoot,
        { label: s.cube, to: { kind: 'cube', cube: s.cube } },
        { label: 'Views', to: { kind: 'cube-views', cube: s.cube } },
        { label: s.view, to: null },
      ]
    case 'cube-rules':
      return [cubesRoot, { label: s.cube, to: { kind: 'cube', cube: s.cube } }, { label: 'Rules & feeders', to: null }]
    case 'dimension':
      return [dimsRoot, { label: s.name || (opts.autoNew ? 'New dimension' : 'Dimension'), to: null }]
    case 'flow':
      return [flowsRoot, { label: s.cube, to: { kind: 'cube', cube: s.cube } }, { label: s.flow || 'New flow', to: null }]
    case 'schedule':
      return [schedRoot, { label: s.cube, to: { kind: 'cube', cube: s.cube } }, { label: s.job || 'New schedule', to: null }]
    case 'overview':
      return [adminRoot, { label: 'Server overview', to: null }]
    case 'security':
      return [adminRoot, { label: 'Security & audit', to: null }]
  }
}

export default function CubeApp({
  username,
  isAdmin,
  onLogout,
}: {
  username: string
  isAdmin: boolean
  onLogout: () => void
}) {
  const [cubes, setCubes] = useState<CubeSummary[]>([])
  // Each opened object is a tab; the active tab's selection + nav drive the
  // content routing. Start with zero tabs (object-centric empty state).
  const [tabs, setTabs] = useState<Tab[]>([])
  const [activeId, setActiveId] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Three-state so a still-connecting socket is never reported as "Offline".
  const [conn, setConn] = useState<'connecting' | 'live' | 'offline'>('connecting')
  const [reload, setReload] = useState(0)
  const [pwOpen, setPwOpen] = useState(false)
  // Administration is its own view (admin only), opened from the top bar rather
  // than the model tree; null means the normal model workspace is shown.
  const [adminView, setAdminView] = useState<null | 'overview' | 'security'>(null)
  // The (cube, dimension) whose member-sets manager dialog is open, if any.
  const [setsFor, setSetsFor] = useState<{ cube: string; dim: string } | null>(null)
  const palette = useCommandPalette()
  const confirm = useConfirm()
  // The active detail pane reports unsaved edits here; navigating away (tree,
  // breadcrumb, palette, all funnel through navigate()) confirms first so a
  // one-click selection cannot silently discard work.
  const paneDirty = useRef(false)

  // The active tab (its selection + nav intent drive the content routing).
  const activeTab = useMemo(
    () => tabs.find((t) => t.id === activeId) ?? null,
    [tabs, activeId],
  )
  const selection = activeTab?.selection ?? null

  // Open-or-activate a tab and hand its pane a fresh intent. If a tab for this
  // selection already exists, make it active and bump its nav signal (merging
  // the new intent); otherwise push a new tab. Always bumps the signal so the
  // destination re-applies the intent (open the item / open the new-form).
  // Re-targeting the SAME already-active tab re-applies the intent without a
  // dirty prompt; any other switch/open confirms first when the current pane
  // has unsaved edits, so a one-click navigation cannot silently discard work.
  const navigate = useCallback(
    (next: Selection, intent: Omit<NavIntent, 'signal'> = {}) => {
      const id = tabId(next)
      const go = (clearDirty = true) => {
        // Reset only when we actually leave the current pane; re-targeting the
        // active tab keeps its unsaved-edit flag so a later switch still confirms.
        if (clearDirty) paneDirty.current = false
        setTabs((list) => {
          const existing = list.find((t) => t.id === id)
          if (existing) {
            return list.map((t) =>
              t.id === id
                ? { ...t, selection: next, nav: { signal: t.nav.signal + 1, ...intent } }
                : t,
            )
          }
          return [...list, { id, selection: next, nav: { signal: 1, ...intent } }]
        })
        setActiveId(id)
      }
      // Re-targeting the already-active tab re-applies the intent in place and
      // must NOT clear the dirty flag (the same pane stays mounted with its edits).
      if (id === activeId) {
        go(false)
        return
      }
      // Switching to another tab with no unsaved edits proceeds immediately.
      if (!paneDirty.current) {
        go()
        return
      }
      void (async () => {
        const ok = await confirm({
          title: 'Discard unsaved changes',
          body: 'You have unsaved edits in this pane. Discard them and continue?',
          confirmLabel: 'Discard',
          danger: true,
        })
        if (ok) go()
      })()
    },
    [confirm, activeId],
  )

  // Close a tab. If it is removed: when it was the active tab, activate the
  // neighbor (prefer the one to the left, else the right, else null -> empty
  // state). Closing the active tab while its pane is dirty confirms first.
  const closeTab = useCallback(
    (id: string) => {
      const remove = () => {
        setTabs((list) => {
          const idx = list.findIndex((t) => t.id === id)
          if (idx === -1) return list
          const next = list.filter((t) => t.id !== id)
          setActiveId((cur) => {
            if (cur !== id) return cur
            // Reactivating means a fresh mount, so the (now closed) dirty pane
            // cannot leak edits forward.
            paneDirty.current = false
            const neighbor = next[idx - 1] ?? next[idx] ?? null
            return neighbor ? neighbor.id : null
          })
          return next
        })
      }
      if (id !== activeId || !paneDirty.current) {
        remove()
        return
      }
      void (async () => {
        const ok = await confirm({
          title: 'Discard unsaved changes',
          body: 'You have unsaved edits in this pane. Discard them and close the tab?',
          confirmLabel: 'Discard',
          danger: true,
        })
        if (ok) remove()
      })()
    },
    [confirm, activeId],
  )

  // Object-centric launch: never auto-open a cube/tab on load. The cube list
  // backs the explorer + command palette; opening is always an explicit action.
  const loadCubes = useCallback(() => {
    listCubes()
      .then((list) => setCubes(list))
      .catch((err: unknown) =>
        setError(err instanceof Error ? err.message : 'Failed to load cubes'),
      )
  }, [])

  useEffect(() => {
    loadCubes()
  }, [loadCubes])

  const bumpReload = useCallback(() => setReload((n) => n + 1), [])

  // Stable so it does not retrigger the pane's onDirtyChange effect each render.
  const setPaneDirty = useCallback((dirty: boolean) => {
    paneDirty.current = dirty
  }, [])

  // After a cube is created, refresh the cube list and open its tab.
  const onCubeCreated = useCallback(
    (name: string) => {
      loadCubes()
      bumpReload()
      navigate({ kind: 'cube', cube: name }, {})
    },
    [loadCubes, bumpReload, navigate],
  )

  // Dispatch a tree context-menu action. Two kinds of action:
  //   - DIRECT: a single backend call (delete/run) confirmed when destructive,
  //     then bumpReload() so the tree + open detail refresh.
  //   - NAVIGATE: open the workspace that hosts the relevant form and hand it an
  //     intent (open this item / start a new one). The append-only create/edit
  //     forms already live inside those workspaces, so we reuse them.
  const onAction = useCallback(
    (action: NodeAction, ctx: ActionContext) => {
      // Flows/schedules belong to a cube; a root-level "New…" resolves one from
      // the context, the current selection, then the first available cube.
      const resolveCube = () => ctx.cube ?? cubeOf(selection) ?? cubes[0]?.name ?? null

      switch (action) {
        // ---- direct, destructive (confirm first) ----
        case 'delete-view': {
          if (!ctx.cube || !ctx.view) return
          const { cube: c, view: v } = ctx
          void (async () => {
            const ok = await confirm({
              title: 'Delete view',
              body: `Delete view "${v}" from ${c}? This cannot be undone.`,
              confirmLabel: 'Delete',
              danger: true,
            })
            if (!ok) return
            try {
              await deleteView(c, v)
              bumpReload()
            } catch (e) {
              setError(e instanceof Error ? e.message : 'Could not delete the view')
            }
          })()
          return
        }
        case 'delete-flow': {
          if (!ctx.cube || !ctx.flow) return
          const { cube: c, flow: f } = ctx
          void (async () => {
            const ok = await confirm({
              title: 'Delete flow',
              body: `Delete flow "${f}" from ${c}? This cannot be undone, and any schedule that runs it will fail.`,
              confirmLabel: 'Delete',
              danger: true,
            })
            if (!ok) return
            try {
              await deleteFlow(c, f)
              bumpReload()
            } catch (e) {
              setError(e instanceof Error ? e.message : 'Could not delete the flow')
            }
          })()
          return
        }
        case 'delete-schedule': {
          if (!ctx.cube || !ctx.job) return
          const { cube: c, job: j } = ctx
          void (async () => {
            const ok = await confirm({
              title: 'Delete schedule',
              body: `Delete schedule "${j}" from ${c}? This permanently removes the schedule and cannot be undone.`,
              confirmLabel: 'Delete',
              danger: true,
            })
            if (!ok) return
            try {
              await deleteJob(c, j)
              bumpReload()
            } catch (e) {
              setError(e instanceof Error ? e.message : 'Could not delete the schedule')
            }
          })()
          return
        }
        case 'delete-dimension': {
          if (ctx.dimId === undefined) return
          const id = ctx.dimId
          const label = ctx.dim ?? `#${id}`
          void (async () => {
            const ok = await confirm({
              title: 'Delete dimension',
              body: `Delete dimension "${label}"? This permanently removes it and cannot be undone.`,
              confirmLabel: 'Delete',
              danger: true,
            })
            if (!ok) return
            try {
              await deleteDimension(id)
              bumpReload()
            } catch (e) {
              setError(e instanceof Error ? e.message : 'Could not delete the dimension')
            }
          })()
          return
        }

        // ---- direct, non-destructive ----
        case 'run-schedule': {
          if (!ctx.cube || !ctx.job) return
          const { cube: c, job: j } = ctx
          void (async () => {
            try {
              await runJob(c, j)
              bumpReload()
            } catch (e) {
              setError(e instanceof Error ? e.message : 'Could not run the schedule')
            }
          })()
          return
        }
        // Running a flow needs a CSV/connection payload, so navigate to its run
        // panel rather than guessing one (the run panel is in FlowsWorkspace).
        case 'run-flow':
        case 'open-flow':
          if (ctx.cube && ctx.flow) navigate({ kind: 'flow', cube: ctx.cube, flow: ctx.flow }, { flow: ctx.flow })
          return

        // ---- navigate to an existing create/edit form ----
        case 'new-cube':
          // The New-cube wizard lives in ModelWorkspace (a cube-dimension view).
          // Use the current/first cube only as the host to render it. On a fresh
          // install with zero cubes there is no host to render against; tell the
          // admin rather than silently doing nothing.
          {
            const host = resolveCube()
            if (host) navigate({ kind: 'cube-dimension', cube: host, dim: '' }, { autoNew: true })
            else setError('Creating the first cube requires an existing cube to host the wizard. This will be available once at least one cube exists.')
          }
          return
        case 'open-model':
          // Open the cube's data model (ModelWorkspace defaults to its first
          // dimension when none is named).
          if (ctx.cube) navigate({ kind: 'cube-dimension', cube: ctx.cube, dim: '' }, {})
          return
        case 'open-rules':
          if (ctx.cube) navigate({ kind: 'cube-rules', cube: ctx.cube }, {})
          return
        case 'manage-sets':
          // The member-sets CRUD dialog for a cube dimension.
          if (ctx.cube && ctx.dim) setSetsFor({ cube: ctx.cube, dim: ctx.dim })
          return
        case 'promote-dimension': {
          // Promote a cube's embedded dimension into the global registry so other
          // cubes can reference it (ADR-0031). Confirms first since it changes the
          // dimension from cube-local to global (a one-way, append-only step).
          if (!ctx.cube || !ctx.dim) return
          const { cube: c, dim: d } = ctx
          void (async () => {
            const ok = await confirm({
              title: 'Reuse in other cubes',
              body: `Make "${d}" available to other cubes? It stays in ${c} with the same members and hierarchy, and other cubes can then reuse it; editing it later updates every cube that uses it.`,
              confirmLabel: 'Make reusable',
            })
            if (!ok) return
            try {
              const { id } = await promoteDimension(c, d)
              bumpReload()
              // The dimension is now a global object: open it as the global
              // dimension and close the now-orphaned cube-local tab so there is a
              // single tab pointing at the right (registry) editor.
              navigate({ kind: 'dimension', id, name: d }, { dimId: id })
              closeTab(tabId({ kind: 'cube-dimension', cube: c, dim: d }))
            } catch (e) {
              setError(e instanceof Error ? e.message : 'Could not make the dimension reusable')
            }
          })()
          return
        }
        case 'add-view':
          if (ctx.cube) navigate({ kind: 'cube-views', cube: ctx.cube }, { autoNew: true })
          return
        case 'open-view':
          if (ctx.cube && ctx.view) navigate({ kind: 'view', cube: ctx.cube, view: ctx.view }, { view: ctx.view })
          return
        case 'add-member':
        case 'add-rollup':
        case 'edit-attributes':
          // The cube-dimension editor always shows the member/roll-up/attribute
          // forms; focus the chosen dimension. For a shared dimension, focus it
          // in the library (its editor shows the same add forms).
          if (ctx.cube && ctx.dim) {
            navigate({ kind: 'cube-dimension', cube: ctx.cube, dim: ctx.dim }, { dim: ctx.dim })
          } else if (ctx.dimId !== undefined) {
            navigate(
              { kind: 'dimension', id: ctx.dimId, name: ctx.dim ?? '' },
              { dimId: ctx.dimId },
            )
          }
          return
        case 'grow-dimension':
          if (ctx.dimId !== undefined) {
            navigate({ kind: 'dimension', id: ctx.dimId, name: ctx.dim ?? '' }, { dimId: ctx.dimId })
          }
          return
        case 'register-dimension':
          navigate({ kind: 'dimension', id: -1, name: '' }, { autoNew: true })
          return
        case 'new-flow': {
          const host = resolveCube()
          if (host) navigate({ kind: 'flow', cube: host, flow: '' }, { autoNew: true })
          else setError('Create a cube first, then add flows to it.')
          return
        }
        case 'new-schedule': {
          const host = resolveCube()
          if (host) navigate({ kind: 'schedule', cube: host, job: '' }, { autoNew: true })
          else setError('Create a cube first, then add schedules to it.')
          return
        }
        case 'open-schedule':
          if (ctx.cube && ctx.job) navigate({ kind: 'schedule', cube: ctx.cube, job: ctx.job }, { job: ctx.job })
          return
      }
    },
    [confirm, navigate, closeTab, bumpReload, selection, cubes],
  )

  useEffect(() => {
    const socket = connectWs((event) => {
      if (event.type === 'cells_changed' || event.type === 'objects_changed') {
        setReload((n) => n + 1)
      }
    })
    socket.onopen = () => setConn('live')
    socket.onclose = () => setConn('offline')
    socket.onerror = () => setConn('offline')
    return () => socket.close()
  }, [])

  const signOut = useCallback(() => {
    logout()
      .catch(() => undefined)
      .finally(onLogout)
  }, [onLogout])

  // Command palette: keep parity with the tree IA. Open any cube, jump to the
  // resource-type roots, invoke the same create actions, reach admin surfaces.
  // Plain selections funnel through navigate() so stale nav intent is cleared.
  const commands = useMemo<Command[]>(() => {
    const list: Command[] = []
    for (const cube of cubes) {
      list.push({
        id: `cube:${cube.name}`,
        label: `Open cube: ${cube.name}`,
        group: 'Cube',
        keywords: 'switch select data',
        run: () => navigate({ kind: 'cube', cube: cube.name }, {}),
      })
    }
    // Resource-type roots (mirror the tree's Dimensions / Flows / Schedules).
    list.push({
      id: 'go:dimensions',
      label: 'Go to Dimensions',
      group: 'Go to',
      keywords: 'reusable across cubes library',
      run: () => navigate({ kind: 'dimension', id: -1, name: '' }, {}),
    })
    list.push({
      id: 'go:flows',
      label: 'Go to Flows',
      group: 'Go to',
      run: () => onAction('new-flow', {}),
    })
    list.push({
      id: 'go:schedules',
      label: 'Go to Schedules',
      group: 'Go to',
      run: () => onAction('new-schedule', {}),
    })
    // Create actions, dispatched through the same handler the tree uses.
    if (isAdmin) {
      list.push({ id: 'new:cube', label: 'New cube…', group: 'Create', run: () => onAction('new-cube', {}) })
    }
    list.push({ id: 'new:dimension', label: 'New dimension…', group: 'Create', run: () => onAction('register-dimension', {}) })
    list.push({ id: 'new:flow', label: 'New flow…', group: 'Create', run: () => onAction('new-flow', {}) })
    list.push({ id: 'new:schedule', label: 'New schedule…', group: 'Create', run: () => onAction('new-schedule', {}) })
    if (isAdmin) {
      list.push({ id: 'go:overview', label: 'Go to Server overview', group: 'Admin', run: () => setAdminView('overview') })
      list.push({ id: 'go:security', label: 'Go to Security & audit', group: 'Admin', run: () => setAdminView('security') })
    }
    list.push({ id: 'pw', label: 'Change password', group: 'Account', run: () => setPwOpen(true) })
    list.push({ id: 'signout', label: 'Sign out', group: 'Account', run: signOut })
    return list
  }, [cubes, isAdmin, signOut, navigate, onAction])

  const segs = crumbs(selection, { autoNew: activeTab?.nav.autoNew })
  const cube = cubeOf(selection)
  const showSandbox =
    selection?.kind === 'cube' ||
    selection?.kind === 'cube-views' ||
    selection?.kind === 'view' ||
    selection?.kind === 'cube-dimension' ||
    selection?.kind === 'cube-rules'

  return (
    <div className="shell">
      <header className="appbar">
        <div className="appbar__brand">
          <span className="appbar__logo" aria-hidden="true">
            ◆
          </span>
          <span className="appbar__name">Epiphany</span>
        </div>
        <nav className="crumbs" aria-label="Breadcrumb">
          {segs.map((seg, i) => {
            const isLast = i === segs.length - 1
            return (
              <span key={i}>
                {isLast ? (
                  <span className="crumbs__seg crumbs__seg--current" aria-current="page">
                    {seg.label}
                  </span>
                ) : seg.to ? (
                  <button
                    type="button"
                    className="crumbs__seg crumbs__link"
                    onClick={() => navigate(seg.to as Selection, {})}
                  >
                    {seg.label}
                  </button>
                ) : (
                  <span className="crumbs__seg">{seg.label}</span>
                )}
                {isLast ? null : (
                  <span className="crumbs__sep" aria-hidden="true">
                    ›
                  </span>
                )}
              </span>
            )
          })}
        </nav>
        <span className="appbar__spacer" />
        {isAdmin ? (
          <Button
            variant="ghost"
            size="sm"
            className="appbar__admin"
            onClick={() => setAdminView('overview')}
            aria-pressed={adminView !== null}
          >
            Administration
          </Button>
        ) : null}
        <Button
          variant="ghost"
          size="sm"
          className="appbar__search"
          onClick={() => palette.setOpen(true)}
        >
          Search<kbd className="kbd">{PALETTE_HINT}</kbd>
        </Button>
        <span role="status" aria-live="polite">
          <Tooltip
            content={
              conn === 'live'
                ? 'Live updates connected'
                : conn === 'connecting'
                  ? 'Connecting to live updates…'
                  : 'Offline - reconnecting'
            }
          >
            <span>
              <Badge tone={conn === 'live' ? 'success' : 'neutral'} dot>
                {conn === 'live' ? 'Live' : conn === 'connecting' ? 'Connecting…' : 'Offline'}
              </Badge>
            </span>
          </Tooltip>
        </span>
        <ThemeToggle />
        <Menu
          trigger={
            <button type="button" className="appbar__user">
              <span className="appbar__avatar" aria-hidden="true">
                {username.slice(0, 1).toUpperCase()}
              </span>
              {username}
            </button>
          }
        >
          <MenuLabel>Signed in as {username}</MenuLabel>
          {isAdmin ? <MenuLabel>Administrator</MenuLabel> : null}
          <MenuSeparator />
          <MenuItem onSelect={() => setPwOpen(true)}>Change password</MenuItem>
          <MenuItem danger onSelect={signOut}>
            Sign out
          </MenuItem>
        </Menu>
      </header>

      <div className="shell__body">
        {adminView ? (
          <main className="content admin-view">
            <div className="admin-view__bar">
              <Button variant="ghost" size="sm" onClick={() => setAdminView(null)}>
                &larr; Back
              </Button>
              <div className="admin-view__nav" role="tablist" aria-label="Administration">
                <button
                  type="button"
                  role="tab"
                  aria-selected={adminView === 'overview'}
                  className={`seg${adminView === 'overview' ? ' is-active' : ''}`}
                  onClick={() => setAdminView('overview')}
                >
                  Server overview
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={adminView === 'security'}
                  className={`seg${adminView === 'security' ? ' is-active' : ''}`}
                  onClick={() => setAdminView('security')}
                >
                  Security &amp; audit
                </button>
              </div>
            </div>
            {error ? (
              <p className="error" role="alert">
                {error}
              </p>
            ) : null}
            {adminView === 'overview' ? <ServerOverview /> : <SecurityWorkspace />}
          </main>
        ) : (
          <>
            <ModelExplorer
              selection={selection}
              onSelect={(s) => navigate(s, {})}
              isAdmin={isAdmin}
              reloadSignal={reload}
              onAction={onAction}
            />

            <main className="content">
              {tabs.length > 0 ? (
                // A labelled group of switch-buttons, not an APG tabs widget: each
                // is independently closable and opens a different object kind, so
                // role=group with aria-current on the active button is the honest
                // semantic (no tabpanel/roving-tabindex contract to fulfil).
                <div className="objtabs" role="group" aria-label="Open objects">
                  {tabs.map((t) => {
                    const segsForTab = crumbs(t.selection, { autoNew: t.nav.autoNew })
                    const label = segsForTab[segsForTab.length - 1]?.label ?? 'Untitled'
                    // The full breadcrumb path disambiguates tabs whose last
                    // segment collides (e.g. a flow and a view both named "Budget")
                    // and recovers text the chip truncates with an ellipsis.
                    const fullPath = segsForTab.map((s) => s.label).join(' / ')
                    const isActive = t.id === activeId
                    return (
                      <div key={t.id} className={`objtab${isActive ? ' is-active' : ''}`}>
                        <button
                          type="button"
                          aria-current={isActive ? 'true' : undefined}
                          className="objtab__label"
                          title={fullPath}
                          onClick={() => navigate(t.selection, {})}
                        >
                          {label}
                        </button>
                        <button
                          type="button"
                          className="objtab__close"
                          aria-label={`Close ${fullPath}`}
                          onClick={() => closeTab(t.id)}
                        >
                          ×
                        </button>
                      </div>
                    )
                  })}
                </div>
              ) : null}

              {error ? (
                <p className="error" role="alert">
                  {error}
                </p>
              ) : null}
              <WelcomeCard username={username} isAdmin={isAdmin} hasCubes={cubes.length > 0} />

              {showSandbox && cube ? (
                <SandboxBar key={cube} cube={cube} onChange={bumpReload} />
              ) : null}

              {activeTab === null ? (
                <EmptyState icon="▤" title="Pick an object to open">
                  {cubes.length === 0
                    ? isAdmin
                      ? 'No cubes yet. Create one from the Cubes section to get started.'
                      : 'No objects are available to you yet. Ask an administrator for access.'
                    : 'Choose a cube, dimension, flow, or schedule from the explorer on the left to open it.'}
                </EmptyState>
              ) : activeTab.selection.kind === 'dimension' ? (
                <DimensionsWorkspace
                  reloadSignal={reload}
                  // A plain tree row-click carries no nav intent, so fall back to
                  // the tab's own selection id (id -1 is the new/placeholder draft,
                  // which has no dimension to focus) -- otherwise the pane would
                  // show the workspace's default dimension, not the clicked one.
                  initialDimId={
                    activeTab.nav.dimId ??
                    (activeTab.selection.id >= 0 ? activeTab.selection.id : undefined)
                  }
                  autoNew={activeTab.nav.autoNew}
                  navSignal={activeTab.nav.signal}
                />
              ) : activeTab.selection.kind === 'cube' && cube ? (
                <PivotGrid cube={cube} reloadSignal={reload} onModelChange={bumpReload} />
              ) : activeTab.selection.kind === 'cube-dimension' && cube ? (
                <ModelWorkspace
                  cube={cube}
                  reloadSignal={reload}
                  isAdmin={isAdmin}
                  onCubeCreated={onCubeCreated}
                  initialDim={activeTab.selection.dim || activeTab.nav.dim}
                  autoNew={activeTab.nav.autoNew}
                  navSignal={activeTab.nav.signal}
                />
              ) : (activeTab.selection.kind === 'cube-views' || activeTab.selection.kind === 'view') && cube ? (
                <ViewWorkspace
                  cube={cube}
                  reloadSignal={reload}
                  initialView={activeTab.selection.kind === 'view' ? activeTab.selection.view : activeTab.nav.view}
                  autoNew={activeTab.nav.autoNew}
                  navSignal={activeTab.nav.signal}
                  onDirtyChange={setPaneDirty}
                />
              ) : activeTab.selection.kind === 'cube-rules' && cube ? (
                <RulesWorkspace cube={cube} reloadSignal={reload} onDirtyChange={setPaneDirty} />
              ) : activeTab.selection.kind === 'flow' && cube ? (
                <FlowsWorkspace
                  cube={cube}
                  reloadSignal={reload}
                  isAdmin={isAdmin}
                  initialFlow={activeTab.selection.flow || activeTab.nav.flow}
                  autoNew={activeTab.nav.autoNew}
                  navSignal={activeTab.nav.signal}
                  onDirtyChange={setPaneDirty}
                />
              ) : activeTab.selection.kind === 'schedule' && cube ? (
                <JobsWorkspace
                  cube={cube}
                  reloadSignal={reload}
                  initialJob={activeTab.selection.job || activeTab.nav.job}
                  autoNew={activeTab.nav.autoNew}
                  navSignal={activeTab.nav.signal}
                  onDirtyChange={setPaneDirty}
                />
              ) : null}
            </main>
          </>
        )}
      </div>

      <CommandPalette open={palette.open} onOpenChange={palette.setOpen} commands={commands} />

      <Dialog open={pwOpen} onOpenChange={setPwOpen} title="Change password" size="sm">
        <ChangePassword
          submitLabel="Update password"
          onCancel={() => setPwOpen(false)}
          onDone={() => setPwOpen(false)}
        />
      </Dialog>

      <Dialog
        open={setsFor !== null}
        onOpenChange={(open) => {
          if (!open) setSetsFor(null)
        }}
        title={setsFor ? `Sets in ${setsFor.dim}` : 'Sets'}
        description="Saved member selections you can apply to a cube view axis."
        size="lg"
      >
        {setsFor ? (
          <SetsManager
            cube={setsFor.cube}
            dimName={setsFor.dim}
            onClose={() => setSetsFor(null)}
            onChanged={bumpReload}
          />
        ) : null}
      </Dialog>
    </div>
  )
}
