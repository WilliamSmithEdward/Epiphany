import { useCallback, useEffect, useState } from 'react'
import { connectWs, listCubes, logout, type CubeSummary } from '../api/client'
import PivotGrid from './PivotGrid'
import RulesWorkspace from './RulesWorkspace'
import ViewWorkspace from './ViewWorkspace'

type Mode = 'grid' | 'views' | 'rules'

export default function CubeApp({
  username,
  onLogout,
}: {
  username: string
  onLogout: () => void
}) {
  const [cubes, setCubes] = useState<CubeSummary[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [live, setLive] = useState(false)
  const [reload, setReload] = useState(0)
  const [mode, setMode] = useState<Mode>('grid')

  useEffect(() => {
    listCubes()
      .then((list) => {
        setCubes(list)
        setSelected((current) => current ?? list[0]?.name ?? null)
      })
      .catch((err: unknown) =>
        setError(err instanceof Error ? err.message : 'Failed to load cubes'),
      )
  }, [])

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

  return (
    <div className="app">
      <header className="topbar">
        <strong>Epiphany</strong>
        <span
          className={`dot ${live ? 'on' : 'off'}`}
          title={live ? 'Live updates on' : 'Offline'}
        />
        <span className="spacer" />
        <span className="user">{username}</span>
        <button onClick={signOut}>Sign out</button>
      </header>
      <div className="body">
        <nav className="sidebar">
          <h2>Cubes</h2>
          <ul>
            {cubes.map((cube) => (
              <li key={cube.name}>
                <button
                  className={cube.name === selected ? 'active' : ''}
                  onClick={() => setSelected(cube.name)}
                >
                  {cube.name} <small>{cube.cell_count} cells</small>
                </button>
              </li>
            ))}
          </ul>
        </nav>
        <main className="content">
          {error ? <p className="error">{error}</p> : null}
          {selected ? (
            <>
              <div className="modes">
                <button className={mode === 'grid' ? 'active' : ''} onClick={() => setMode('grid')}>
                  Grid
                </button>
                <button className={mode === 'views' ? 'active' : ''} onClick={() => setMode('views')}>
                  Views
                </button>
                <button className={mode === 'rules' ? 'active' : ''} onClick={() => setMode('rules')}>
                  Rules
                </button>
              </div>
              {mode === 'grid' ? (
                <PivotGrid cube={selected} reloadSignal={reload} />
              ) : mode === 'views' ? (
                <ViewWorkspace cube={selected} reloadSignal={reload} />
              ) : (
                <RulesWorkspace cube={selected} reloadSignal={reload} />
              )}
            </>
          ) : (
            <p>No cube selected.</p>
          )}
        </main>
      </div>
    </div>
  )
}
