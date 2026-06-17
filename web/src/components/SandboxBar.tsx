import { useCallback, useEffect, useState } from 'react'
import {
  commitSandbox,
  createSandbox,
  deleteSandbox,
  listSandboxes,
  setActiveSandbox,
  type SandboxDto,
} from '../api/client'
import { useConfirm } from '../ui'

// The what-if sandbox switcher (ADR-0014). Selecting a sandbox sets the global
// active sandbox (so every data request carries the X-Epiphany-Sandbox header)
// and signals a reload, so the grid and views recompute over the overlay. The
// selection persists per cube, and is reset when the cube changes (this is
// remounted with key={cube}).
const BASE = ''

export default function SandboxBar({ cube, onChange }: { cube: string; onChange: () => void }) {
  const confirm = useConfirm()
  const [sandboxes, setSandboxes] = useState<SandboxDto[]>([])
  const [active, setActive] = useState<string>(BASE)
  const [newName, setNewName] = useState('')
  const [error, setError] = useState<string | null>(null)

  const storageKey = `epiphany.sandbox.${cube}`

  const apply = useCallback(
    (name: string) => {
      setActive(name)
      setActiveSandbox(name === BASE ? null : name)
      if (name === BASE) localStorage.removeItem(storageKey)
      else localStorage.setItem(storageKey, name)
      onChange()
    },
    [onChange, storageKey],
  )

  const load = useCallback(
    async (select?: string) => {
      try {
        const list = await listSandboxes(cube)
        setSandboxes(list)
        const want = select ?? localStorage.getItem(storageKey) ?? BASE
        apply(want !== BASE && list.some((s) => s.name === want) ? want : BASE)
        setError(null)
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load sandboxes')
      }
    },
    [cube, storageKey, apply],
  )

  useEffect(() => {
    setActiveSandbox(null)
    setActive(BASE)
    void load()
    // Clear the global on unmount so another cube does not inherit the header.
    return () => setActiveSandbox(null)
  }, [load])

  async function onCreate() {
    const name = newName.trim()
    if (!name) return
    try {
      await createSandbox(cube, name)
      setNewName('')
      await load(name)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not create the sandbox')
    }
  }

  async function onCommit() {
    if (active === BASE) return
    const ok = await confirm({
      title: 'Commit sandbox',
      body: `Commit "${active}" into base data? This merges every uncommitted what-if override into the cube and cannot be undone.`,
      confirmLabel: 'Commit',
      danger: true,
    })
    if (!ok) return
    try {
      await commitSandbox(cube, active)
      await load(BASE)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Commit failed')
    }
  }

  async function onDiscard() {
    if (active === BASE) return
    const ok = await confirm({
      title: 'Discard sandbox',
      body: `Discard all uncommitted changes in "${active}"? This cannot be undone.`,
      confirmLabel: 'Discard',
      danger: true,
    })
    if (!ok) return
    try {
      await deleteSandbox(cube, active)
      await load(BASE)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Discard failed')
    }
  }

  return (
    <div className={`sandbox-bar ${active !== BASE ? 'whatif' : ''}`}>
      <label>
        What-if
        <select value={active} onChange={(e) => apply(e.target.value)}>
          <option value={BASE}>Base data</option>
          {sandboxes.map((s) => (
            <option key={s.name} value={s.name}>
              {s.name} ({s.cell_count})
            </option>
          ))}
        </select>
      </label>
      <input
        aria-label="New sandbox name"
        placeholder="new sandbox"
        value={newName}
        onChange={(e) => setNewName(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') void onCreate()
        }}
      />
      <button onClick={() => void onCreate()} disabled={!newName.trim()}>
        Create
      </button>
      <button onClick={() => void onCommit()} disabled={active === BASE}>
        Commit
      </button>
      <button onClick={() => void onDiscard()} disabled={active === BASE}>
        Discard
      </button>
      {active !== BASE ? (
        <span className="whatif-badge" title="Values shown are uncommitted what-if overrides">
          Editing what-if: {active} (uncommitted)
        </span>
      ) : null}
      {error ? <span className="error" role="alert">{error}</span> : null}
    </div>
  )
}
