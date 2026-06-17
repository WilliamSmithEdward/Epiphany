import { useCallback, useEffect, useState } from 'react'
import {
  deleteSubset,
  getCube,
  listSubsets,
  type DimensionDto,
  type SubsetDto,
} from '../api/client'
import { Button, useConfirm } from '../ui'
import SubsetEditor from './SubsetEditor'

/**
 * Manage the saved member sets (subsets) of one cube dimension: list, create,
 * edit, and delete. Reached from the dimension's context menu in the explorer.
 * Fetches the dimension structure (for the member picker) and its sets itself.
 */
export default function SetsManager({
  cube,
  dimName,
  onClose,
  onChanged,
}: {
  cube: string
  dimName: string
  onClose: () => void
  /** Called after a set is created, edited, or deleted, so the rest of the app
   * (e.g. an open pivot's set menus) can refresh. */
  onChanged?: () => void
}) {
  const [dimension, setDimension] = useState<DimensionDto | null>(null)
  const [subsets, setSubsets] = useState<SubsetDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  // null = list view; { existing? } = the editor (new when existing is absent).
  const [editing, setEditing] = useState<{ existing?: SubsetDto } | null>(null)
  const confirm = useConfirm()

  const reloadSubsets = useCallback(() => {
    return listSubsets(cube, dimName)
      .then((s) => {
        setSubsets(s)
        setError(null)
      })
      .catch((e) => setError(e instanceof Error ? e.message : 'Could not load sets'))
  }, [cube, dimName])

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    Promise.all([getCube(cube), listSubsets(cube, dimName)])
      .then(([detail, s]) => {
        if (cancelled) return
        setDimension(detail.dimensions.find((d) => d.name === dimName) ?? null)
        setSubsets(s)
        setError(null)
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : 'Could not load the dimension')
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [cube, dimName])

  const remove = (s: SubsetDto) =>
    void (async () => {
      const ok = await confirm({
        title: 'Delete set',
        body: `Delete the set "${s.name}" from ${dimName}? This cannot be undone.`,
        confirmLabel: 'Delete',
        danger: true,
      })
      if (!ok) return
      try {
        await deleteSubset(cube, dimName, s.name)
        await reloadSubsets()
        onChanged?.()
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Could not delete the set')
      }
    })()

  if (editing && dimension) {
    return (
      <SubsetEditor
        key={editing.existing?.name ?? '__new__'}
        cube={cube}
        dimension={dimension}
        existing={editing.existing}
        onCancel={() => setEditing(null)}
        onSaved={() => {
          setEditing(null)
          void reloadSubsets()
          onChanged?.()
        }}
      />
    )
  }

  return (
    <div className="sets-manager">
      <div className="sets-manager__bar">
        <Button size="sm" disabled={!dimension} onClick={() => setEditing({})}>
          New set
        </Button>
      </div>
      {error ? (
        <p className="error" role="alert">
          {error}
        </p>
      ) : null}
      {loading ? (
        <p className="muted">Loading sets...</p>
      ) : subsets.length === 0 ? (
        <p className="muted">No sets yet for {dimName}. Create one to reuse a member selection.</p>
      ) : (
        <table className="sets-table">
          <thead>
            <tr>
              <th scope="col">Set</th>
              <th scope="col">Visible to</th>
              <th scope="col">Members</th>
              <th scope="col" aria-label="Actions" />
            </tr>
          </thead>
          <tbody>
            {subsets.map((s) => (
              <tr key={s.name}>
                <th scope="row">{s.name}</th>
                <td>{s.visibility === 'public' ? 'Everyone' : 'Only me'}</td>
                <td>{s.kind === 'dynamic' ? 'Expression' : String(s.members.length)}</td>
                <td className="sets-table__actions">
                  <Button variant="ghost" size="sm" onClick={() => setEditing({ existing: s })}>
                    Edit
                  </Button>
                  <Button variant="ghost" size="sm" onClick={() => remove(s)}>
                    Delete
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <div className="actions">
        <Button variant="ghost" size="sm" onClick={onClose}>
          Close
        </Button>
      </div>
    </div>
  )
}
