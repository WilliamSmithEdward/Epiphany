import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  listFlows,
  listRuns,
  listSchedules,
  putSchedule,
  runSchedule,
  type FlowDto,
  type RunDto,
} from '../api/client'
import { Badge, Button, Card, Field, Input, Select, Switch } from '../ui'

// Friendly interval units. every_millis on the wire is always milliseconds; the
// editor lets a planner think in seconds / minutes / hours / days instead.
const UNITS = [
  { value: 'seconds', label: 'seconds', ms: 1_000 },
  { value: 'minutes', label: 'minutes', ms: 60_000 },
  { value: 'hours', label: 'hours', ms: 3_600_000 },
  { value: 'days', label: 'days', ms: 86_400_000 },
] as const

type UnitKey = (typeof UNITS)[number]['value']

/** Split a millisecond interval into the largest whole unit that divides it. */
function splitInterval(ms: number): { count: number; unit: UnitKey } {
  for (const u of [...UNITS].reverse()) {
    if (ms >= u.ms && ms % u.ms === 0) {
      return { count: ms / u.ms, unit: u.value }
    }
  }
  return { count: Math.max(1, Math.round(ms / 1000)), unit: 'seconds' }
}

function humanizeTime(ms: number): string {
  if (!ms) return ''
  return new Date(ms).toLocaleString()
}

function runTone(state: string): 'success' | 'danger' | 'info' | 'neutral' {
  switch (state) {
    case 'succeeded':
      return 'success'
    case 'failed':
      return 'danger'
    case 'running':
    case 'pending':
      return 'info'
    default:
      return 'neutral'
  }
}

const BLANK = { name: '', steps: [] as string[], count: 5, unit: 'minutes' as UnitKey, enabled: true }

// The scheduler workspace (Phase 8, ADR-0013; server-global, ADR-0035): list,
// create, and edit schedules (ordered flow steps on a fixed interval), kick one
// by hand, and read the recent run history. Schedules are global, not owned by a
// cube; the cubes a schedule writes are whatever its flows' bodies address.
export default function JobsWorkspace({
  reloadSignal,
  initialJob,
  autoNew,
  navSignal,
  onDirtyChange,
}: {
  reloadSignal: number
  /** Open this schedule in the editor on mount / when it changes (from the tree). */
  initialJob?: string
  /** Start with a blank "new schedule" form (the tree's "New schedule…" action). */
  autoNew?: boolean
  /** Bumped by the navigator to re-apply initialJob/autoNew (e.g. re-clicking the
   * same schedule). */
  navSignal?: number
  /** Reports unsaved-edit state up so the navigator can guard against silently
   * discarding an in-progress schedule when the user clicks away in the tree. */
  onDirtyChange?: (dirty: boolean) => void
}) {
  const [flows, setFlows] = useState<FlowDto[]>([])
  const [runs, setRuns] = useState<RunDto[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [draft, setDraft] = useState({ ...BLANK })
  // The last loaded/saved draft, so we can tell whether the form is dirty.
  const [savedDraft, setSavedDraft] = useState({ ...BLANK })
  const [stepPick, setStepPick] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(() => {
    // Flows (and schedules) drive the editor and gate on Flow/Job access, so a
    // non-admin schedule author can load them. listRuns() is admin-only (GET
    // /runs -> require_admin), so load it separately and tolerate a failure (403)
    // instead of letting it reject the whole load and break the editor (ADR-0023).
    listFlows()
      .then((f) => setFlows(f))
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load schedules'))
    listRuns()
      .then((r) => setRuns(r))
      .catch(() => setRuns([]))
  }, [])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  // Open the schedule the navigator (tree) asked for, or a blank "new schedule"
  // form. Driven by initialJob/autoNew/navSignal so re-clicking re-applies.
  useEffect(() => {
    if (autoNew) {
      setSelected(null)
      setDraft({ ...BLANK })
      setSavedDraft({ ...BLANK })
      setStepPick('')
      setError(null)
      return
    }
    if (!initialJob) return
    let live = true
    listSchedules()
      .then((js) => {
        if (!live) return
        const job = js.find((j) => j.name === initialJob)
        if (job) {
          const { count, unit } = splitInterval(job.every_millis)
          const loaded = { name: job.name, steps: [...job.steps], count, unit, enabled: job.enabled }
          setSelected(job.name)
          setDraft(loaded)
          setSavedDraft({ ...loaded, steps: [...loaded.steps] })
          setStepPick('')
          setError(null)
        }
      })
      .catch(() => undefined)
    return () => {
      live = false
    }
  }, [initialJob, autoNew, navSignal])

  // The form is dirty when any edited field diverges from the loaded/saved draft.
  const dirty =
    draft.name !== savedDraft.name ||
    draft.count !== savedDraft.count ||
    draft.unit !== savedDraft.unit ||
    draft.enabled !== savedDraft.enabled ||
    draft.steps.join('\n') !== savedDraft.steps.join('\n')

  // Report dirtiness up so the navigator can confirm before discarding edits;
  // clear it on unmount so a stale "dirty" never blocks the next navigation.
  useEffect(() => {
    onDirtyChange?.(dirty)
    return () => onDirtyChange?.(false)
  }, [dirty, onDirtyChange])

  const flowOptions = useMemo(
    () => flows.map((f) => ({ value: f.name, label: f.name })),
    [flows],
  )
  const everyMillis = useMemo(() => {
    const unit = UNITS.find((u) => u.value === draft.unit) ?? UNITS[1]
    return Math.max(1, draft.count) * unit.ms
  }, [draft.count, draft.unit])

  function addStep() {
    if (stepPick === '') return
    setDraft((d) => ({ ...d, steps: [...d.steps, stepPick] }))
    setStepPick('')
  }

  function moveStep(index: number, delta: number) {
    setDraft((d) => {
      const next = [...d.steps]
      const target = index + delta
      if (target < 0 || target >= next.length) return d
      ;[next[index], next[target]] = [next[target], next[index]]
      return { ...d, steps: next }
    })
  }

  function removeStep(index: number) {
    setDraft((d) => ({ ...d, steps: d.steps.filter((_, i) => i !== index) }))
  }

  async function save() {
    const name = draft.name.trim()
    if (name === '') {
      setError('Please give the schedule a name.')
      return
    }
    if (draft.steps.length === 0) {
      setError('Add at least one flow step to run.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await putSchedule({
        name,
        steps: draft.steps,
        every_millis: everyMillis,
        enabled: draft.enabled,
      })
      setSelected(name)
      // The saved draft becomes the new clean baseline.
      setSavedDraft({ name: draft.name, steps: [...draft.steps], count: draft.count, unit: draft.unit, enabled: draft.enabled })
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the schedule')
    } finally {
      setBusy(false)
    }
  }

  async function kick(name: string) {
    setBusy(true)
    setError(null)
    try {
      await runSchedule(name)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not run the schedule')
    } finally {
      setBusy(false)
    }
  }

  const noFlows = flows.length === 0

  return (
    <div className="jobs-workspace">
      {/* The object explorer (tree) is the navigator: pick, create, run, and
          delete schedules from its context menus. This pane edits the schedule
          the tree opened (or a blank "new schedule" form). */}
      <Card title={selected ? `Edit "${selected}"` : 'New schedule'}>
        {noFlows ? (
          <p className="muted">
            There are no flows yet. Create a flow in the Flows section first, then schedule it here.
          </p>
        ) : (
          <div className="job-editor">
            <Field label="Name" hint="A short label, for example refresh_actuals.">
              {(id, a11y) => (
                <Input
                  id={id}
                  {...a11y}
                  value={draft.name}
                  onChange={(e) => setDraft((d) => ({ ...d, name: e.target.value }))}
                  placeholder="refresh_actuals"
                />
              )}
            </Field>

            <div className="field">
              <span className="field__label">Run</span>
              <div className="job-interval">
                <span className="muted">every</span>
                <Input
                  type="number"
                  min={1}
                  className="job-interval__count"
                  aria-label="Run interval count"
                  value={String(draft.count)}
                  onChange={(e) =>
                    setDraft((d) => ({ ...d, count: Math.max(1, Number(e.target.value) || 1) }))
                  }
                />
                <Select
                  value={draft.unit}
                  onValueChange={(v) => setDraft((d) => ({ ...d, unit: v as UnitKey }))}
                  options={UNITS.map((u) => ({ value: u.value, label: u.label }))}
                  ariaLabel="Interval unit"
                />
              </div>
            </div>

            <div className="field">
              <span className="field__label">Steps</span>
              <p className="field__msg field__msg--hint">
                Flows run top to bottom each time the schedule fires.
              </p>
              {draft.steps.length === 0 ? (
                <p className="muted">No steps yet. Add a flow below.</p>
              ) : (
                <ol className="job-steps">
                  {draft.steps.map((step, i) => (
                    <li key={`${step}-${i}`} className="job-step">
                      <span className="job-step__index">{i + 1}</span>
                      <span className="job-step__name">{step}</span>
                      <span className="job-step__controls">
                        <button
                          type="button"
                          className="icon-btn"
                          disabled={i === 0}
                          onClick={() => moveStep(i, -1)}
                          title="Move up"
                          aria-label="Move step up"
                        >
                          ↑
                        </button>
                        <button
                          type="button"
                          className="icon-btn"
                          disabled={i === draft.steps.length - 1}
                          onClick={() => moveStep(i, 1)}
                          title="Move down"
                          aria-label="Move step down"
                        >
                          ↓
                        </button>
                        <button
                          type="button"
                          className="icon-btn"
                          onClick={() => removeStep(i)}
                          title="Remove step"
                          aria-label="Remove step"
                        >
                          ✕
                        </button>
                      </span>
                    </li>
                  ))}
                </ol>
              )}
              <div className="job-add-step">
                <Select
                  value={stepPick}
                  onValueChange={setStepPick}
                  options={flowOptions}
                  placeholder="Pick a flow…"
                  ariaLabel="Flow to add as a step"
                />
                <Button size="sm" variant="secondary" disabled={stepPick === ''} onClick={addStep}>
                  Add step
                </Button>
              </div>
            </div>

            <Switch
              checked={draft.enabled}
              onCheckedChange={(v) => setDraft((d) => ({ ...d, enabled: v }))}
              label="Active"
              description="When off, the schedule is kept but will not fire automatically."
            />

            {error ? <p className="error" role="alert">{error}</p> : null}
            <div className="actions">
              <Button variant="primary" disabled={busy} onClick={() => void save()}>
                {busy ? 'Saving…' : selected ? 'Save changes' : 'Create schedule'}
              </Button>
              {selected ? (
                <Button variant="ghost" disabled={busy} onClick={() => void kick(selected)}>
                  Run now
                </Button>
              ) : null}
            </div>
          </div>
        )}
      </Card>

      <Card
        title="Recent runs"
        subtitle="The latest scheduled and manual runs across the server, newest first."
        actions={
          <Button size="sm" variant="ghost" icon="↻" onClick={load}>
            Refresh
          </Button>
        }
      >
        {runs.length === 0 ? (
          <p className="muted">No runs recorded yet.</p>
        ) : (
          <div className="run-table-wrap">
            <table className="run-table">
              <caption className="sr-only">Recent runs</caption>
              <thead>
                <tr>
                  <th scope="col">Status</th>
                  <th scope="col">Target</th>
                  <th scope="col">Cube</th>
                  <th scope="col">When</th>
                  <th scope="col" className="num">Rows</th>
                  <th scope="col" className="num">Cells</th>
                  <th scope="col" className="num">Elements</th>
                  <th scope="col">By</th>
                </tr>
              </thead>
              <tbody>
                {runs.map((run) => (
                  <tr key={run.id}>
                    <td>
                      <Badge tone={runTone(run.state)}>{run.state}</Badge>
                    </td>
                    <td>
                      {run.target}
                      {run.is_job ? <span className="run-table__tag">schedule</span> : null}
                      {run.error ? <div className="run-table__err">{run.error}</div> : null}
                    </td>
                    <td>{run.cube || '-'}</td>
                    <td className="run-table__when">{humanizeTime(run.fire_millis)}</td>
                    <td className="num">{run.rows_read}</td>
                    <td className="num">{run.cells_written}</td>
                    <td className="num">{run.elements_added}</td>
                    <td className="run-table__by">{run.principal}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  )
}
