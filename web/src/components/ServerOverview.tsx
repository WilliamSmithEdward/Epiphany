import { useCallback, useEffect, useState } from 'react'
import { listAllRuns, listCubes, queryAudit, type AuditRecordDto, type CubeSummary, type RunDto } from '../api/client'
import { Badge, Button, Card, EmptyState } from '../ui'

// The admin server-overview dashboard (W4): a cross-cube snapshot of recent
// activity. Server-gated to admins (the nav entry is hidden for everyone else
// and /api/v1/runs + /api/v1/audit are admin-only regardless). Polls every 30s.
export default function ServerOverview() {
  const [cubes, setCubes] = useState<CubeSummary[] | null>(null)
  const [runs, setRuns] = useState<RunDto[]>([])
  const [denials, setDenials] = useState<AuditRecordDto[]>([])
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(() => {
    Promise.all([listCubes(), listAllRuns(20), queryAudit({ outcome: 'denied', limit: 10 })])
      .then(([c, r, d]) => {
        setCubes(c)
        setRuns(r)
        setDenials(d)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load the overview'))
  }, [])

  useEffect(() => {
    refresh()
    const handle = setInterval(refresh, 30_000)
    return () => clearInterval(handle)
  }, [refresh])

  if (error) {
    return (
      <EmptyState icon="⚿" title="Could not load the overview">
        {error}
      </EmptyState>
    )
  }
  if (!cubes) {
    return <p className="banner">Loading the overview…</p>
  }

  const cellTotal = cubes.reduce((sum, c) => sum + c.cell_count, 0)
  const failed = runs.filter((r) => r.state === 'failed').length

  return (
    <div className="overview">
      <Card title="Server overview" subtitle="A live snapshot across all cubes." actions={
        <Button size="sm" variant="ghost" onClick={refresh}>Refresh</Button>
      }>
        <div className="overview-stats">
          <Stat label="Cubes" value={String(cubes.length)} />
          <Stat label="Stored cells" value={cellTotal.toLocaleString()} />
          <Stat label="Recent runs" value={String(runs.length)} />
          <Stat label="Recent failures" value={String(failed)} tone={failed > 0 ? 'danger' : 'neutral'} />
          <Stat label="Recent denials" value={String(denials.length)} tone={denials.length > 0 ? 'warn' : 'neutral'} />
        </div>
      </Card>

      <Card title="Recent runs" subtitle="Scheduled and manual flow runs across every cube.">
        {runs.length === 0 ? (
          <p className="muted">No runs yet.</p>
        ) : (
          <table className="overview-table">
            <thead>
              <tr>
                <th>State</th>
                <th>Cube</th>
                <th>Target</th>
                <th>When</th>
                <th>By</th>
                <th>Result</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((r) => (
                <tr key={r.id}>
                  <td>{runBadge(r.state)}</td>
                  <td>{r.cube}</td>
                  <td>
                    {r.target}
                    {r.is_job ? ' (job)' : ''}
                  </td>
                  <td>{formatTime(r.fire_millis)}</td>
                  <td>{r.principal}</td>
                  <td>
                    {r.error ? (
                      <span className="error">{r.error}</span>
                    ) : (
                      `${r.cells_written} cells`
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      <Card title="Recent access denials" subtitle="The latest denied requests from the audit log.">
        {denials.length === 0 ? (
          <p className="ok">No recent denials.</p>
        ) : (
          <ul className="coord-list">
            {denials.map((d) => (
              <li key={d.seq}>
                <strong>{d.actor}</strong> — {d.action} on {d.object_kind}
                {d.target ? ` (${d.target})` : ''} · {formatTime(d.timestamp_millis)}
              </li>
            ))}
          </ul>
        )}
      </Card>
    </div>
  )
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: 'danger' | 'warn' | 'neutral' }) {
  return (
    <div className="overview-stat">
      <div className={`overview-stat__value${tone && tone !== 'neutral' ? ` is-${tone}` : ''}`}>{value}</div>
      <div className="overview-stat__label">{label}</div>
    </div>
  )
}

function runBadge(state: string) {
  switch (state) {
    case 'succeeded':
      return <Badge tone="success">ok</Badge>
    case 'failed':
      return <Badge tone="danger">failed</Badge>
    case 'running':
    case 'pending':
      return <Badge tone="info">{state}</Badge>
    default:
      return <Badge tone="neutral">{state}</Badge>
  }
}

function formatTime(millis: number): string {
  if (!millis) return ''
  try {
    return new Date(millis).toLocaleString()
  } catch {
    return String(millis)
  }
}
