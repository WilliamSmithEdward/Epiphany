import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  createView,
  executeAdhoc,
  executeView,
  getCube,
  getView,
  listSubsets,
  listViews,
  type AxisSpecDef,
  type CellsetDto,
  type ContextEntry,
  type CubeDetail,
  type SubsetDto,
  type ViewDef,
  type ViewDto,
  type Visibility,
} from '../api/client'
import CellsetGrid from './CellsetGrid'
import SubsetEditor from './SubsetEditor'
import ViewBuilder, { ALL_MEMBERS, type DimConfig } from './ViewBuilder'

// The Views workspace: define subsets, build a (nested) view, run it to a
// cellset, save it, and reopen saved views. The binding contract is proven by
// the Rust acceptance suite; this surfaces it for point-and-click use.
export default function ViewWorkspace({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  const [subsetsByDim, setSubsetsByDim] = useState<Record<string, SubsetDto[]>>({})
  const [savedViews, setSavedViews] = useState<ViewDto[]>([])
  const [config, setConfig] = useState<Record<string, DimConfig>>({})
  const [suppress, setSuppress] = useState(false)
  const [name, setName] = useState('')
  const [visibility, setVisibility] = useState<Visibility>('public')
  const [cellset, setCellset] = useState<CellsetDto | null>(null)
  const [editorDim, setEditorDim] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const loadSubsets = useCallback(
    async (loaded: CubeDetail) => {
      const entries = await Promise.all(
        loaded.dimensions.map(async (d) => [d.name, await listSubsets(cube, d.name)] as const),
      )
      setSubsetsByDim(Object.fromEntries(entries))
    },
    [cube],
  )

  // Load structure, default placements, subsets, and saved views per cube.
  useEffect(() => {
    let cancelled = false
    setCellset(null)
    getCube(cube)
      .then(async (loaded) => {
        if (cancelled) return
        setDetail(loaded)
        const defaults: Record<string, DimConfig> = {}
        loaded.dimensions.forEach((dim, i) => {
          const placement = i === 0 ? 'rows' : i === 1 ? 'columns' : 'context'
          defaults[dim.name] = {
            placement,
            source: ALL_MEMBERS,
            contextMember: dim.elements[0]?.name ?? '',
          }
        })
        setConfig(defaults)
        await loadSubsets(loaded)
        const views = await listViews(cube)
        if (!cancelled) setSavedViews(views)
      })
      .catch((err: unknown) => setError(err instanceof Error ? err.message : 'Failed to load cube'))
    return () => {
      cancelled = true
    }
  }, [cube, loadSubsets])

  const buildDef = useCallback((): ViewDef => {
    const rows: AxisSpecDef[] = []
    const columns: AxisSpecDef[] = []
    const context: ContextEntry[] = []
    for (const dim of detail?.dimensions ?? []) {
      const cfg = config[dim.name]
      if (!cfg) continue
      if (cfg.placement === 'context') {
        context.push({ dimension: dim.name, member: cfg.contextMember })
      } else {
        const spec: AxisSpecDef =
          cfg.source === ALL_MEMBERS
            ? { dimension: dim.name, type: 'members', members: dim.elements.map((e) => e.name) }
            : { dimension: dim.name, type: 'subset', subset: cfg.source }
        if (cfg.placement === 'rows') rows.push(spec)
        else columns.push(spec)
      }
    }
    return { rows, columns, context, suppress_zeros: suppress }
  }, [detail, config, suppress])

  const run = useCallback(async () => {
    setBusy(true)
    try {
      setCellset(await executeAdhoc(cube, buildDef()))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not run the view')
    } finally {
      setBusy(false)
    }
  }, [cube, buildDef])

  // Re-run when an external change (WebSocket) bumps the reload signal.
  useEffect(() => {
    if (cellset) void run()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadSignal])

  function updateConfig(dim: string, partial: Partial<DimConfig>) {
    setConfig((current) => ({ ...current, [dim]: { ...current[dim], ...partial } }))
  }

  async function save() {
    if (name.trim() === '') {
      setError('Name the view before saving.')
      return
    }
    setBusy(true)
    try {
      await createView(cube, { ...buildDef(), name: name.trim(), visibility })
      setSavedViews(await listViews(cube))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not save the view')
    } finally {
      setBusy(false)
    }
  }

  async function openView(viewName: string) {
    setBusy(true)
    try {
      const view = await getView(cube, viewName)
      applyView(view)
      setName(view.name)
      setVisibility(view.visibility)
      setSuppress(view.suppress_zeros)
      setCellset(await executeView(cube, viewName))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not open the view')
    } finally {
      setBusy(false)
    }
  }

  function applyView(view: ViewDto) {
    setConfig((current) => {
      const next = { ...current }
      for (const dim of detail?.dimensions ?? []) {
        const base = next[dim.name] ?? {
          placement: 'context' as const,
          source: ALL_MEMBERS,
          contextMember: dim.elements[0]?.name ?? '',
        }
        next[dim.name] = { ...base, placement: 'context', contextMember: base.contextMember }
      }
      const apply = (specs: typeof view.rows, placement: 'rows' | 'columns') => {
        for (const spec of specs) {
          next[spec.dimension] = {
            ...next[spec.dimension],
            placement,
            source: spec.type === 'subset' && spec.subset ? spec.subset : ALL_MEMBERS,
          }
        }
      }
      apply(view.rows, 'rows')
      apply(view.columns, 'columns')
      for (const ctx of view.context) {
        next[ctx.dimension] = { ...next[ctx.dimension], placement: 'context', contextMember: ctx.member }
      }
      return next
    })
  }

  const editorDimension = useMemo(
    () => detail?.dimensions.find((d) => d.name === editorDim) ?? null,
    [detail, editorDim],
  )

  if (!detail) return <p className="banner">Loading {cube}...</p>

  return (
    <div className="workspace">
      <div className="workspace-side">
        <h3>Saved views</h3>
        {savedViews.length === 0 ? (
          <p className="muted">None yet.</p>
        ) : (
          <ul className="saved-views">
            {savedViews.map((v) => (
              <li key={v.name}>
                <button onClick={() => void openView(v.name)}>
                  {v.name} <small>{v.visibility === 'private' ? 'only me' : 'shared'}</small>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="workspace-main">
        {editorDimension ? (
          <SubsetEditor
            cube={cube}
            dimension={editorDimension}
            onCancel={() => setEditorDim(null)}
            onSaved={(saved) => {
              setEditorDim(null)
              void loadSubsets(detail)
              const dimName = editorDimension.name
              updateConfig(dimName, { source: saved })
            }}
          />
        ) : (
          <ViewBuilder
            dimensions={detail.dimensions}
            subsetsByDim={subsetsByDim}
            config={config}
            onConfigChange={updateConfig}
            suppress={suppress}
            onSuppressChange={setSuppress}
            name={name}
            onNameChange={setName}
            visibility={visibility}
            onVisibilityChange={setVisibility}
            onRun={() => void run()}
            onSave={() => void save()}
            onNewSubset={(dim) => setEditorDim(dim)}
            busy={busy}
          />
        )}
        {error ? <p className="error">{error}</p> : null}
        {cellset ? (
          <>
            {cellset.suppressed.row_tuples > 0 || cellset.suppressed.column_tuples > 0 ? (
              <p className="muted">
                Hidden by zero-suppression: {cellset.suppressed.row_tuples} rows,{' '}
                {cellset.suppressed.column_tuples} columns
              </p>
            ) : null}
            <CellsetGrid cube={cube} cellset={cellset} onChanged={() => void run()} />
          </>
        ) : null}
      </div>
    </div>
  )
}
