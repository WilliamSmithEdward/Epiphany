import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  explainCell,
  feederDiagnostics,
  getCube,
  getRules,
  listRuleTests,
  previewRules,
  putRules,
  runRuleTests,
  type Coord,
  type CubeDetail,
  type FeederReportDto,
  type RulePreview,
  type TestReportDto,
  type TraceDto,
} from '../api/client'

// The modeler's calculation workspace for one cube (Phase 4): edit and validate
// rules, see auto-inferred feeders and under/over-feed diagnostics, trace any
// cell's provenance ("explain"), and run the model's rule unit tests. Editing
// follows the M3 pattern - a plain textarea with debounced server-side
// validation - so the app stays dependency-free; syntax highlighting is a
// deferred enhancement, but located error markers are delivered here.
export default function RulesWorkspace({
  cube,
  reloadSignal,
}: {
  cube: string
  reloadSignal: number
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  const [source, setSource] = useState('')
  const [saved, setSaved] = useState('')
  const [preview, setPreview] = useState<RulePreview | null>(null)
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [savedNote, setSavedNote] = useState(false)

  // Load the cube structure (for the explain picker) and its current rules.
  useEffect(() => {
    let live = true
    Promise.all([getCube(cube), getRules(cube)])
      .then(([d, rules]) => {
        if (!live) return
        setDetail(d)
        setSource(rules.source)
        setSaved(rules.source)
      })
      .catch(() => undefined)
    return () => {
      live = false
    }
  }, [cube, reloadSignal])

  // Debounced validation of the edited source (parse + compile, no save).
  useEffect(() => {
    if (source.trim() === '') {
      setPreview({ ok: true })
      return
    }
    const handle = setTimeout(() => {
      previewRules(cube, source)
        .then(setPreview)
        .catch((err: unknown) =>
          setPreview({ ok: false, message: err instanceof Error ? err.message : 'Invalid' }),
        )
    }, 300)
    return () => clearTimeout(handle)
  }, [cube, source])

  const dirty = source !== saved

  async function save() {
    setSaving(true)
    setSaveError(null)
    try {
      const result = await putRules(cube, source)
      setSaved(result.source)
      setSource(result.source)
      setSavedNote(true)
      setTimeout(() => setSavedNote(false), 2000)
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : 'Could not save the rules')
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="rules-workspace">
      <section className="rules-editor">
        <div className="rules-editor-head">
          <h3>Rules</h3>
          {preview?.ok === false ? (
            <span className="error">
              {preview.line ? `Line ${preview.line}, col ${preview.column}: ` : ''}
              {preview.message}
            </span>
          ) : source.trim() === '' ? (
            <span className="muted">No rules yet</span>
          ) : (
            <span className="ok">Valid</span>
          )}
        </div>
        <textarea
          className="rules-source"
          value={source}
          spellCheck={false}
          onChange={(e) => setSource(e.target.value)}
          placeholder={"['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"}
          rows={12}
        />
        {saveError ? <p className="error">{saveError}</p> : null}
        <div className="actions">
          <button
            className="primary"
            disabled={saving || !dirty || preview?.ok === false}
            onClick={() => void save()}
          >
            {saving ? 'Saving...' : 'Save rules'}
          </button>
          <button disabled={!dirty} onClick={() => setSource(saved)}>
            Revert
          </button>
          {savedNote ? <span className="ok">Saved</span> : null}
        </div>
      </section>

      <FeederPanel cube={cube} reloadSignal={reloadSignal} />
      {detail ? <ExplainPanel cube={cube} detail={detail} reloadSignal={reloadSignal} /> : null}
      <TestPanel cube={cube} reloadSignal={reloadSignal} />
    </div>
  )
}

// ---- feeder diagnostics ----

function FeederPanel({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
  const [report, setReport] = useState<FeederReportDto | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const refresh = useCallback(() => {
    setLoading(true)
    setError(null)
    feederDiagnostics(cube)
      .then(setReport)
      .catch((err: unknown) => setError(err instanceof Error ? err.message : 'Failed'))
      .finally(() => setLoading(false))
  }, [cube])

  useEffect(() => {
    refresh()
  }, [refresh, reloadSignal])

  return (
    <section className="feeder-panel">
      <div className="rules-editor-head">
        <h3>Feeders</h3>
        <button onClick={refresh} disabled={loading}>
          {loading ? 'Checking...' : 'Recheck'}
        </button>
      </div>
      {error ? <p className="error">{error}</p> : null}
      {report ? (
        <div className="feeder-report">
          <p>
            <strong>{report.fed_cell_count}</strong> fed cell
            {report.fed_cell_count === 1 ? '' : 's'}
            {report.under_fed.length === 0 ? (
              <span className="ok"> - no under-feed</span>
            ) : (
              <span className="error"> - {report.under_fed.length} under-fed</span>
            )}
            {report.over_fed.length > 0 ? (
              <span className="warn"> - {report.over_fed.length} over-fed</span>
            ) : null}
          </p>
          {report.under_fed.length > 0 ? (
            <div>
              <p className="error">Under-fed (would read as a wrong zero in roll-ups):</p>
              <ul className="coord-list">
                {report.under_fed.map((c, i) => (
                  <li key={i}>{c.join(' / ')}</li>
                ))}
              </ul>
            </div>
          ) : null}
          {report.over_fed.length > 0 ? (
            <div>
              <p className="warn">
                Over-fed (wasted scan/RAM, ~{report.estimated_over_fed_bytes} bytes):
              </p>
              <ul className="coord-list">
                {report.over_fed.map((c, i) => (
                  <li key={i}>{c.join(' / ')}</li>
                ))}
              </ul>
            </div>
          ) : null}
          {report.opaque_rules.length > 0 ? (
            <div>
              <p className="muted">
                Rules feeders could not be auto-inferred (feed manually or review):
              </p>
              <ul className="coord-list">
                {report.opaque_rules.map((o) => (
                  <li key={o.rule}>
                    rule #{o.rule}: {o.reason}
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
        </div>
      ) : null}
    </section>
  )
}

// ---- explain (provenance) ----

function ExplainPanel({
  cube,
  detail,
  reloadSignal,
}: {
  cube: string
  detail: CubeDetail
  reloadSignal: number
}) {
  const initial = useMemo(() => {
    const coord: Coord = {}
    for (const dim of detail.dimensions) coord[dim.name] = dim.elements[0]?.name ?? ''
    return coord
  }, [detail])

  const [coord, setCoord] = useState<Coord>(initial)
  const [trace, setTrace] = useState<TraceDto | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  // Reset the picker when the cube (and thus its dimensions) changes.
  useEffect(() => {
    setCoord(initial)
    setTrace(null)
    setError(null)
  }, [initial, reloadSignal])

  async function explain() {
    setLoading(true)
    setError(null)
    try {
      setTrace(await explainCell(cube, coord, 'full'))
    } catch (err) {
      setTrace(null)
      setError(err instanceof Error ? err.message : 'Could not explain the cell')
    } finally {
      setLoading(false)
    }
  }

  return (
    <section className="explain-panel">
      <div className="rules-editor-head">
        <h3>Explain</h3>
        <button onClick={() => void explain()} disabled={loading}>
          {loading ? 'Tracing...' : 'Explain cell'}
        </button>
      </div>
      <div className="explain-picker">
        {detail.dimensions.map((dim) => (
          <label key={dim.name}>
            {dim.name}
            <select
              value={coord[dim.name] ?? ''}
              onChange={(e) => setCoord((c) => ({ ...c, [dim.name]: e.target.value }))}
            >
              {dim.elements.map((el) => (
                <option key={el.name} value={el.name}>
                  {el.name}
                </option>
              ))}
            </select>
          </label>
        ))}
      </div>
      {error ? <p className="error">{error}</p> : null}
      {trace ? (
        <div className="trace">
          <TraceNode node={trace} />
        </div>
      ) : null}
    </section>
  )
}

function kindLabel(node: TraceDto): string {
  switch (node.kind) {
    case 'stored':
      return 'stored leaf'
    case 'rule':
      return `rule #${node.rule ?? 0}`
    case 'consolidation':
      return `consolidation of ${node.contributions ?? node.inputs.length}`
  }
}

function TraceNode({ node }: { node: TraceDto }) {
  return (
    <div className="trace-node">
      <div className="trace-row">
        <span className={`trace-kind ${node.kind}`}>{kindLabel(node)}</span>
        <span className="trace-coord">{node.coord.join(' / ')}</span>
        <span className="trace-value">{node.value}</span>
      </div>
      {node.inputs.length > 0 ? (
        <div className="trace-inputs">
          {node.inputs.map((child, i) => (
            <TraceNode key={i} node={child} />
          ))}
        </div>
      ) : null}
    </div>
  )
}

// ---- rule test runner ----

function TestPanel({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
  const [report, setReport] = useState<TestReportDto | null>(null)
  const [count, setCount] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)

  // Show how many tests are defined (the run reports per-test outcomes).
  useEffect(() => {
    let live = true
    listRuleTests(cube)
      .then((tests) => {
        if (live) setCount(tests.length)
      })
      .catch(() => undefined)
    return () => {
      live = false
    }
  }, [cube, reloadSignal])

  async function run() {
    setRunning(true)
    setError(null)
    try {
      setReport(await runRuleTests(cube))
    } catch (err) {
      setReport(null)
      setError(err instanceof Error ? err.message : 'Could not run the tests')
    } finally {
      setRunning(false)
    }
  }

  return (
    <section className="test-panel">
      <div className="rules-editor-head">
        <h3>Rule tests {count !== null ? <small>({count})</small> : null}</h3>
        <button onClick={() => void run()} disabled={running || count === 0}>
          {running ? 'Running...' : 'Run tests'}
        </button>
      </div>
      {error ? <p className="error">{error}</p> : null}
      {report ? (
        <div className="test-report">
          <p className={report.all_passed ? 'ok' : 'error'}>
            {report.all_passed
              ? `All ${report.outcomes.length} tests passed`
              : `${report.outcomes.filter((o) => !o.passed).length} of ${report.outcomes.length} failed`}
          </p>
          <ul className="test-list">
            {report.outcomes.map((o) => (
              <li key={o.name} className={o.passed ? 'ok' : 'error'}>
                <span className="test-status">{o.passed ? 'PASS' : 'FAIL'}</span> {o.name}
                {o.failures.length > 0 ? (
                  <ul className="failure-list">
                    {o.failures.map((f, i) => (
                      <li key={i}>
                        {Object.entries(f.coord)
                          .map(([d, m]) => `${d}:${m}`)
                          .join(' / ')}{' '}
                        expected {f.expected}, got {f.actual}
                      </li>
                    ))}
                  </ul>
                ) : null}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </section>
  )
}
