import { useCallback, useEffect, useMemo, useState } from 'react'
import { connectWs, listCubes, logout, type CubeSummary } from '../api/client'
import FlowsWorkspace from './FlowsWorkspace'
import JobsWorkspace from './JobsWorkspace'
import ModelWorkspace from './ModelWorkspace'
import PivotGrid from './PivotGrid'
import RulesWorkspace from './RulesWorkspace'
import SandboxBar from './SandboxBar'
import SecurityWorkspace from './SecurityWorkspace'
import ViewWorkspace from './ViewWorkspace'
import {
  Badge,
  Button,
  CommandPalette,
  EmptyState,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  Select,
  ThemeToggle,
  Tooltip,
  useCommandPalette,
  type Command,
} from '../ui'

type Section = 'data' | 'model' | 'views' | 'rules' | 'flows' | 'jobs' | 'admin'

interface NavItem {
  id: Section
  label: string
  glyph: string
  group: string
  admin?: boolean
}

const NAV: NavItem[] = [
  { id: 'data', label: 'Data', glyph: '▦', group: 'Workspace' },
  { id: 'model', label: 'Model', glyph: '◳', group: 'Workspace' },
  { id: 'views', label: 'Views', glyph: '◫', group: 'Workspace' },
  { id: 'rules', label: 'Rules', glyph: 'Σ', group: 'Workspace' },
  { id: 'flows', label: 'Flows', glyph: '⇄', group: 'Workspace' },
  { id: 'jobs', label: 'Schedules', glyph: '⏱', group: 'Workspace' },
  { id: 'admin', label: 'Security & audit', glyph: '⚿', group: 'Administration', admin: true },
]

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
  const [selected, setSelected] = useState<string | null>(null)
  const [section, setSection] = useState<Section>('data')
  const [error, setError] = useState<string | null>(null)
  const [live, setLive] = useState(false)
  const [reload, setReload] = useState(0)
  const palette = useCommandPalette()

  const loadCubes = useCallback((select?: string) => {
    listCubes()
      .then((list) => {
        setCubes(list)
        setSelected((current) => select ?? current ?? list[0]?.name ?? null)
      })
      .catch((err: unknown) =>
        setError(err instanceof Error ? err.message : 'Failed to load cubes'),
      )
  }, [])

  useEffect(() => {
    loadCubes()
  }, [loadCubes])

  // After a cube is created, refresh the list and jump to its model.
  const onCubeCreated = useCallback((name: string) => {
    loadCubes(name)
    setSection('model')
  }, [loadCubes])

  useEffect(() => {
    const socket = connectWs((event) => {
      if (event.type === 'cells_changed' || event.type === 'objects_changed') {
        setReload((n) => n + 1)
      }
    })
    socket.onopen = () => setLive(true)
    socket.onclose = () => setLive(false)
    return () => socket.close()
  }, [])

  const signOut = useCallback(() => {
    logout()
      .catch(() => undefined)
      .finally(onLogout)
  }, [onLogout])

  const bumpReload = useCallback(() => setReload((n) => n + 1), [])

  const visibleNav = NAV.filter((n) => !n.admin || isAdmin)

  // Command palette: switch cube, jump to a section, sign out.
  const commands = useMemo<Command[]>(() => {
    const list: Command[] = []
    for (const cube of cubes) {
      list.push({
        id: `cube:${cube.name}`,
        label: `Open cube: ${cube.name}`,
        group: 'Cube',
        keywords: 'switch select',
        run: () => {
          setSelected(cube.name)
          if (section === 'admin') setSection('data')
        },
      })
    }
    for (const n of visibleNav) {
      list.push({
        id: `go:${n.id}`,
        label: `Go to ${n.label}`,
        group: 'Navigate',
        run: () => setSection(n.id),
      })
    }
    list.push({ id: 'signout', label: 'Sign out', group: 'Account', run: signOut })
    return list
    // visibleNav is derived from isAdmin (stable); cubes/section drive the set.
  }, [cubes, section, signOut, visibleNav])

  const sectionLabel = NAV.find((n) => n.id === section)?.label ?? ''

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
          {section === 'admin' ? (
            <span className="crumbs__seg">Administration</span>
          ) : selected ? (
            <span className="crumbs__seg">{selected}</span>
          ) : null}
          <span className="crumbs__sep" aria-hidden="true">
            ›
          </span>
          <span className="crumbs__seg crumbs__seg--current">{sectionLabel}</span>
        </nav>
        <span className="appbar__spacer" />
        <Button
          variant="ghost"
          size="sm"
          className="appbar__search"
          onClick={() => palette.setOpen(true)}
        >
          Search<kbd className="kbd">⌘K</kbd>
        </Button>
        <Tooltip content={live ? 'Live updates connected' : 'Offline - reconnecting'}>
          <span>
            <Badge tone={live ? 'success' : 'neutral'} dot>
              {live ? 'Live' : 'Offline'}
            </Badge>
          </span>
        </Tooltip>
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
          <MenuItem danger onSelect={signOut}>
            Sign out
          </MenuItem>
        </Menu>
      </header>

      <div className="shell__body">
        <nav className="nav" aria-label="Sections">
          <div className="nav__cube">
            <span className="nav__cube-label">Cube</span>
            {cubes.length > 0 ? (
              <Select
                value={selected ?? undefined}
                onValueChange={(v) => {
                  setSelected(v)
                  if (section === 'admin') setSection('data')
                }}
                options={cubes.map((c) => ({ value: c.name, label: c.name }))}
                ariaLabel="Select cube"
                className="nav__cube-select"
              />
            ) : (
              <span className="muted">No cubes</span>
            )}
          </div>
          {['Workspace', 'Administration'].map((group) => {
            const items = visibleNav.filter((n) => n.group === group)
            if (items.length === 0) return null
            return (
              <div className="nav__group" key={group}>
                <div className="nav__group-title">{group}</div>
                {items.map((n) => (
                  <button
                    key={n.id}
                    type="button"
                    className={section === n.id ? 'nav__item is-active' : 'nav__item'}
                    onClick={() => setSection(n.id)}
                  >
                    <span className="nav__glyph" aria-hidden="true">
                      {n.glyph}
                    </span>
                    {n.label}
                  </button>
                ))}
              </div>
            )
          })}
        </nav>

        <main className="content">
          {error ? <p className="error">{error}</p> : null}
          {section === 'admin' && isAdmin ? (
            <SecurityWorkspace />
          ) : selected ? (
            <>
              {section === 'data' || section === 'views' ? (
                <SandboxBar key={selected} cube={selected} onChange={bumpReload} />
              ) : null}
              {section === 'data' ? (
                <PivotGrid cube={selected} reloadSignal={reload} />
              ) : section === 'model' ? (
                <ModelWorkspace
                  cube={selected}
                  reloadSignal={reload}
                  isAdmin={isAdmin}
                  onCubeCreated={onCubeCreated}
                />
              ) : section === 'views' ? (
                <ViewWorkspace cube={selected} reloadSignal={reload} />
              ) : section === 'rules' ? (
                <RulesWorkspace cube={selected} reloadSignal={reload} />
              ) : section === 'flows' ? (
                <FlowsWorkspace cube={selected} reloadSignal={reload} />
              ) : (
                <JobsWorkspace cube={selected} reloadSignal={reload} />
              )}
            </>
          ) : (
            <EmptyState icon="▦" title="No cube selected">
              {cubes.length === 0
                ? 'No cubes are available yet. Define a model or load the demo data to get started.'
                : 'Pick a cube from the sidebar to begin.'}
            </EmptyState>
          )}
        </main>
      </div>

      <CommandPalette
        open={palette.open}
        onOpenChange={palette.setOpen}
        commands={commands}
      />
    </div>
  )
}
