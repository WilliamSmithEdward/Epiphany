import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  deleteJob,
  listFlows,
  listJobs,
  listRuns,
  putJob,
  runJob,
  type FlowDto,
  type JobDto,
  type RunDto,
} from '../api/client'
import { Badge, Button, Card, EmptyState, Field, Input, Select, Switch } from '../ui'

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

function humanizeInterval(ms: number): string {
  const { count, unit } = splitInterval(ms)
  const noun = count === 1 ? unit.replace(/s$/, '') : unit
  return `every ${count} ${noun}`
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

// The scheduler workspace for one cube (Phase 8, ADR-0013): list, create, and
// edit scheduled jobs (ordered flow steps on a fixed interval), kick a job by
// hand, and read the recent run history. Built on the shared design system.
export default function JobsWorkspace({
  cube,
  reloadSignal,
}: {
  cube: string
  reloadSignal: number
}) {
  const [jobs, setJobs] = useState<JobDto[]>([])
  const [flows, setFlows] = useState<FlowDto[]>([])
  const [runs, setRuns] = useState<RunDto[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [draft, setDraft] = useState({ ...BLANK })
  const [stepPick, setStepPick] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(() => {
    Promise.all([listJobs(cube), listFlows(cube), listRuns(cube)])
      .then(([j, f, r]) => {
        setJobs(j)
        setFlows(f)
        setRuns(r)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load jobs'))
  }, [cube])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  function newJob() {
    setSelected(null)
    setDraft({ ...BLANK })
    setStepPick('')
    setError(null)
  }

  function openJob(job: JobDto) {
    const { count, unit } = splitInterval(job.every_millis)
    setSelected(job.name)
    setDraft({ name: job.name, steps: [...job.steps], count, unit, enabled: job.enabled })
    setStepPick('')
    setError(null)
  }

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
      await putJob(cube, {
        name,
        steps: draft.steps,
        every_millis: everyMillis,
        enabled: draft.enabled,
      })
      setSelected(name)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the schedule')
    } finally {
      setBusy(false)
    }
  }

  async function remove(name: string) {
    setError(null)
    try {
      await deleteJob(cube, name)
      if (selected === name) newJob()
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not delete the schedule')
    }
  }

  async function kick(name: string) {
    setBusy(true)
    setError(null)
    try {
      await runJob(cube, name)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not run the job')
    } finally {
      setBusy(false)
    }
  }

  const noFlows = flows.length === 0

  return (
    <div className="jobs-workspace">
      <Card
        title="Schedules"
        subtitle="Run a sequence of flows automatically on a fixed interval."
        actions={
          <Button size="sm" variant="primary" onClick={newJob}>
            New schedule
          </Button>
        }
      >
        {jobs.length === 0 ? (
          <EmptyState icon="⏱" title="No schedules yet">
            A schedule runs one or more of this cube&apos;s flows on a repeating interval, for
            example to refresh data every morning. Create one to get started.
          </EmptyState>
        ) : (
          <ul className="job-list">
            {jobs.map((job) => (
              <li
                key={job.name}
                className={job.name === selected ? 'job-row is-active' : 'job-row'}
              >
                <button type="button" className="job-row__main" onClick={() => openJob(job)}>
                  <span className="job-row__name">{job.name}</span>
                  <span className="job-row__meta">
                    {humanizeInterval(job.every_millis)} &middot; {job.steps.length}{' '}
                    {job.steps.length === 1 ? 'step' : 'steps'}
                  </span>
                </button>
                <Badge tone={job.enabled ? 'success' : 'neutral'} dot>
                  {job.enabled ? 'Active' : 'Paused'}
                </Badge>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => void kick(job.name)}
                  title="Run this schedule now"
                >
                  Run now
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => void remove(job.name)}
                  title="Delete this schedule"
                >
                  Delete
                </Button>
              </li>
            ))}
          </ul>
        )}
      </Card>

      <Card title={selected ? `Edit "${selected}"` : 'New schedule'}>
        {noFlows ? (
          <p className="muted">
            This cube has no flows yet. Create a flow in the Flows section first, then schedule it
            here.
          </p>
        ) : (
          <div className="job-editor">
            <Field label="Name" hint="A short label, for example refresh_actuals.">
              {(id) => (
                <Input
                  id={id}
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
                        >
                          ↑
                        </button>
                        <button
                          type="button"
                          className="icon-btn"
                          disabled={i === draft.steps.length - 1}
                          onClick={() => moveStep(i, 1)}
                          title="Move down"
                        >
                          ↓
                        </button>
                        <button
                          type="button"
                          className="icon-btn"
                          onClick={() => removeStep(i)}
                          title="Remove step"
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
        subtitle="The latest scheduled and manual runs for this cube, newest first."
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
              <thead>
                <tr>
                  <th>Status</th>
                  <th>Target</th>
                  <th>When</th>
                  <th className="num">Rows</th>
                  <th className="num">Cells</th>
                  <th className="num">Elements</th>
                  <th>By</th>
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
