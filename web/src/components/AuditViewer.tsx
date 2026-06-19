import { useCallback, useEffect, useRef, useState } from 'react'
import { queryAudit, type AuditRecordDto } from '../api/client'

// The audit action tokens the server emits (ADR-0010), for the filter dropdown.
const ACTIONS = [
  'login',
  'logout',
  'access_denied',
  'user_change',
  'group_change',
  'object_create',
  'object_update',
  'object_delete',
  'flow_exec',
  'job_exec',
  'sandbox_commit',
  'sandbox_discard',
  'checkpoint',
]

function formatTime(millis: number): string {
  const date = new Date(millis)
  return Number.isNaN(date.getTime()) ? String(millis) : date.toLocaleString()
}

// The audit-log viewer (ADR-0010): a filtered, read-only view of security-
// relevant and model-changing actions. Admin only. Records carry no secrets or
// cell payloads (RG-13), only object identities.
export default function AuditViewer() {
  const [records, setRecords] = useState<AuditRecordDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [actor, setActor] = useState('')
  const [action, setAction] = useState('')
  const [outcome, setOutcome] = useState<'' | 'allowed' | 'denied'>('')
  const [loading, setLoading] = useState(false)
  // Read the actor at query time so the load effect does not re-run on every
  // keystroke; the actor query is run explicitly on blur, Enter, and Refresh.
  const actorRef = useRef(actor)
  actorRef.current = actor

  const load = useCallback(() => {
    setError(null)
    setLoading(true)
    queryAudit({
      actor: actorRef.current.trim() || undefined,
      action: action || undefined,
      outcome: outcome || undefined,
      limit: 500,
    })
      .then(setRecords)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load audit log'))
      .finally(() => setLoading(false))
  }, [action, outcome])

  // The selects fire immediately; the actor text field does not (see above).
  useEffect(load, [load])

  return (
    <div>
      <h3>Audit log</h3>
      {error ? <p className="error" role="alert">{error}</p> : null}
      <div className="field-row">
        <label>
          Actor
          <input
            value={actor}
            onChange={(e) => setActor(e.target.value)}
            onBlur={load}
            onKeyDown={(e) => {
              if (e.key === 'Enter') load()
            }}
            placeholder="any"
          />
        </label>
        <label>
          Action
          <select value={action} onChange={(e) => setAction(e.target.value)}>
            <option value="">any</option>
            {ACTIONS.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
        </label>
        <label>
          Outcome
          <select
            value={outcome}
            onChange={(e) => setOutcome(e.target.value as '' | 'allowed' | 'denied')}
          >
            <option value="">any</option>
            <option value="allowed">allowed</option>
            <option value="denied">denied</option>
          </select>
        </label>
      </div>
      <div className="actions">
        <button className="primary" onClick={load}>
          Refresh
        </button>
      </div>
      {loading ? (
        <p className="banner" role="status" aria-live="polite">
          Loading audit records...
        </p>
      ) : records.length === 0 ? (
        <p className="muted">No matching records.</p>
      ) : null}
      <p role="status" aria-live="polite" className="muted">
        {loading || records.length === 0
          ? ''
          : `${records.length} record${records.length === 1 ? '' : 's'}`}
      </p>
      <table className="placements">
        <caption className="sr-only">Audit records</caption>
        <thead>
          <tr>
            <th scope="col" className="num">Seq</th>
            <th scope="col">Time</th>
            <th scope="col">Actor</th>
            <th scope="col">Action</th>
            <th scope="col">Target</th>
            <th scope="col">Outcome</th>
          </tr>
        </thead>
        <tbody>
          {records.map((r) => (
            <tr key={r.seq}>
              <td className="num">{r.seq}</td>
              <td>{formatTime(r.timestamp_millis)}</td>
              <td>{r.actor}</td>
              <td>{r.action}</td>
              <td>{r.object_kind ? `${r.object_kind}: ${r.target}` : ''}</td>
              <td className={r.allowed ? 'ok' : 'error'}>{r.allowed ? 'allowed' : 'denied'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
