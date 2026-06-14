import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  deleteConnection,
  deleteFlow,
  getCube,
  importCsv,
  listConnections,
  listFlows,
  listFlowTests,
  previewFlow,
  putConnection,
  putFlow,
  runFlow,
  runFlowTests,
  type ConnectionDto,
  type CubeDetail,
  type FlowDto,
  type FlowPreview,
  type RunReport,
  type TestReportDto,
} from '../api/client'

const STARTER = `// A flow reads ctx.input() (the data rows) and stages changes.
function rows(ctx) {
  const data = ctx.input()
  // ctx.ensureElements('Dim', data.map(r => r.Column))
  // ctx.writeCells(data.map(r => ({ coord: { Dim: r.Column }, value: r.Value })))
}
`

// The modeler's flow workspace for one cube (Phase 5): write and validate
// TypeScript flows, run them over CSV, load data with a guided import wizard, and
// run flow unit tests. Editing follows the dependency-free textarea +
// debounced-validation pattern (no Monaco); located error markers are delivered.
export default function FlowsWorkspace({
  cube,
  reloadSignal,
}: {
  cube: string
  reloadSignal: number
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  const [flows, setFlows] = useState<FlowDto[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [source, setSource] = useState(STARTER)
  const [preview, setPreview] = useState<FlowPreview | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const load = useCallback(() => {
    Promise.all([getCube(cube), listFlows(cube)])
      .then(([d, fs]) => {
        setDetail(d)
        setFlows(fs)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load'))
  }, [cube])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  // Debounced validation of the edited source.
  useEffect(() => {
    if (source.trim() === '') {
      setPreview({ ok: true })
      return
    }
    const handle = setTimeout(() => {
      previewFlow(cube, source)
        .then(setPreview)
        .catch((e: unknown) =>
          setPreview({ ok: false, message: e instanceof Error ? e.message : 'Invalid' }),
        )
    }, 300)
    return () => clearTimeout(handle)
  }, [cube, source])

  function openFlow(f: FlowDto) {
    setSelected(f.name)
    setName(f.name)
    setSource(f.source)
    setError(null)
  }

  function newFlow() {
    setSelected(null)
    setName('')
    setSource(STARTER)
    setError(null)
  }

  async function save() {
    if (name.trim() === '') {
      setError('Please name the flow.')
      return
    }
    setSaving(true)
    setError(null)
    try {
      await putFlow(cube, name.trim(), source)
      setSelected(name.trim())
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the flow')
    } finally {
      setSaving(false)
    }
  }

  async function remove(flowName: string) {
    try {
      await deleteFlow(cube, flowName)
      if (selected === flowName) newFlow()
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not delete the flow')
    }
  }

  return (
    <div className="flows-workspace">
      <section className="flow-list">
        <div className="rules-editor-head">
          <h3>Flows</h3>
          <button onClick={newFlow}>New</button>
        </div>
        <ul className="saved-views">
          {flows.map((f) => (
            <li key={f.name}>
              <button className={f.name === selected ? 'active' : ''} onClick={() => openFlow(f)}>
                {f.name}
              </button>
              <button className="link" onClick={() => void remove(f.name)} title="Delete">
                x
              </button>
            </li>
          ))}
          {flows.length === 0 ? <li className="muted">No flows yet</li> : null}
        </ul>
      </section>

      <section className="flow-editor">
        <div className="field-row">
          <label>
            Name
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. load_sales" />
          </label>
          {preview?.ok === false ? (
            <span className="error">
              {preview.line ? `Line ${preview.line}, col ${preview.column}: ` : ''}
              {preview.message}
            </span>
          ) : source.trim() === '' ? (
            <span className="muted">Empty</span>
          ) : (
            <span className="ok">Valid</span>
          )}
        </div>
        <textarea
          className="rules-source"
          value={source}
          spellCheck={false}
          onChange={(e) => setSource(e.target.value)}
          rows={14}
        />
        {error ? <p className="error">{error}</p> : null}
        <div className="actions">
          <button className="primary" disabled={saving || preview?.ok === false} onClick={() => void save()}>
            {saving ? 'Saving...' : 'Save flow'}
          </button>
        </div>
      </section>

      {selected ? <RunPanel cube={cube} flow={selected} reloadSignal={reloadSignal} /> : null}
      {detail ? <ImportPanel cube={cube} detail={detail} /> : null}
      <FlowTestPanel cube={cube} reloadSignal={reloadSignal} />
      <ConnectionsPanel cube={cube} reloadSignal={reloadSignal} />
    </div>
  )
}

// ---- run a flow ----

function RunPanel({
  cube,
  flow,
  reloadSignal,
}: {
  cube: string
  flow: string
  reloadSignal: number
}) {
  const [csv, setCsv] = useState('')
  // The data source: '' = inline CSV, otherwise a connection name.
  const [source, setSource] = useState('')
  const [connections, setConnections] = useState<ConnectionDto[]>([])
  const [report, setReport] = useState<RunReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)

  useEffect(() => {
    listConnections(cube)
      .then(setConnections)
      .catch(() => undefined)
  }, [cube, reloadSignal])

  async function run() {
    setRunning(true)
    setError(null)
    try {
      const body = source === '' ? { input: csv } : { connection: source }
      setReport(await runFlow(cube, flow, body))
    } catch (e) {
      setReport(null)
      setError(e instanceof Error ? e.message : 'Run failed')
    } finally {
      setRunning(false)
    }
  }

  return (
    <section className="flow-run">
      <div className="rules-editor-head">
        <h3>
          Run <small>{flow}</small>
        </h3>
        <button onClick={() => void run()} disabled={running}>
          {running ? 'Running...' : 'Run'}
        </button>
      </div>
      <label className="muted">
        Source{' '}
        <select value={source} onChange={(e) => setSource(e.target.value)}>
          <option value="">Inline CSV</option>
          {connections.map((c) => (
            <option key={c.name} value={c.name}>
              {c.name} ({c.kind})
            </option>
          ))}
        </select>
      </label>
      {source === '' ? (
        <textarea
          className="rules-source"
          value={csv}
          spellCheck={false}
          placeholder={'Paste CSV input (leave empty for a source-less flow)\nRegion,Value\nNorth,100'}
          onChange={(e) => setCsv(e.target.value)}
          rows={5}
        />
      ) : (
        <p className="muted">Rows are fetched from the &quot;{source}&quot; connection.</p>
      )}
      {error ? <p className="error">{error}</p> : null}
      {report ? <RunReportView report={report} /> : null}
    </section>
  )
}

// ---- connection admin (command connectors) ----

function ConnectionsPanel({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
  const [connections, setConnections] = useState<ConnectionDto[]>([])
  const [name, setName] = useState('')
  const [program, setProgram] = useState('')
  const [args, setArgs] = useState('')
  const [format, setFormat] = useState('csv')
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const load = useCallback(() => {
    listConnections(cube)
      .then(setConnections)
      .catch(() => undefined)
  }, [cube])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  async function add() {
    if (name.trim() === '' || program.trim() === '') {
      setError('A connection needs a name and a program.')
      return
    }
    setSaving(true)
    setError(null)
    try {
      await putConnection(cube, {
        name: name.trim(),
        kind: 'command',
        program: program.trim(),
        // One argument per line.
        args: args.split('\n').map((a) => a.trim()).filter((a) => a !== ''),
        format,
        timeout_ms: 30000,
      })
      setName('')
      setProgram('')
      setArgs('')
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the connection')
    } finally {
      setSaving(false)
    }
  }

  async function remove(connName: string) {
    try {
      await deleteConnection(cube, connName)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not delete the connection')
    }
  }

  return (
    <section className="connections-panel">
      <div className="rules-editor-head">
        <h3>Connections</h3>
      </div>
      <ul className="coord-list">
        {connections.map((c) => (
          <li key={c.name}>
            <strong>{c.name}</strong> [{c.kind}] {c.program} {c.args.join(' ')}{' '}
            <button className="link" onClick={() => void remove(c.name)} title="Delete">
              x
            </button>
          </li>
        ))}
        {connections.length === 0 ? <li className="muted">No connections</li> : null}
      </ul>
      <p className="muted">
        Add a command connection (admin only; the server must enable command connectors):
      </p>
      <div className="conn-form">
        <input value={name} placeholder="name" onChange={(e) => setName(e.target.value)} />
        <input value={program} placeholder="program (e.g. python)" onChange={(e) => setProgram(e.target.value)} />
        <textarea
          value={args}
          placeholder={'one argument per line\nscripts/extract.py\n--region=North'}
          onChange={(e) => setArgs(e.target.value)}
          rows={3}
        />
        <select value={format} onChange={(e) => setFormat(e.target.value)}>
          <option value="csv">csv</option>
          <option value="json">json</option>
        </select>
        <button className="primary" disabled={saving} onClick={() => void add()}>
          {saving ? 'Saving...' : 'Add connection'}
        </button>
      </div>
      {error ? <p className="error">{error}</p> : null}
    </section>
  )
}

function RunReportView({ report }: { report: RunReport }) {
  return (
    <div className="run-report">
      <p className="ok">
        {report.rows_read} rows read, {report.cells_written} cells written, {report.elements_added}{' '}
        elements added
      </p>
      {report.logs.length > 0 ? (
        <ul className="coord-list">
          {report.logs.map((l, i) => (
            <li key={i}>{l}</li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}

// ---- guided CSV import ----

function ImportPanel({ cube, detail }: { cube: string; detail: CubeDetail }) {
  const [csv, setCsv] = useState('')
  const [valueColumn, setValueColumn] = useState('')
  const [mapping, setMapping] = useState<Record<string, string>>({})
  const [fixed, setFixed] = useState<Record<string, string>>({})
  const [report, setReport] = useState<RunReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)

  // The CSV header columns (naive: the first line split on commas).
  const columns = useMemo(() => {
    const first = csv.split('\n')[0]?.trim()
    return first ? first.split(',').map((c) => c.trim()) : []
  }, [csv])

  const dimNames = detail.dimensions.map((d) => d.name)
  const mappedDims = new Set(Object.values(mapping))

  async function run() {
    if (valueColumn === '') {
      setError('Pick the value column.')
      return
    }
    setRunning(true)
    setError(null)
    try {
      setReport(
        await importCsv(cube, {
          csv,
          columns: mapping,
          value_column: valueColumn,
          fixed,
        }),
      )
    } catch (e) {
      setReport(null)
      setError(e instanceof Error ? e.message : 'Import failed')
    } finally {
      setRunning(false)
    }
  }

  return (
    <section className="flow-import">
      <div className="rules-editor-head">
        <h3>Guided CSV import</h3>
        <button onClick={() => void run()} disabled={running || columns.length === 0}>
          {running ? 'Importing...' : 'Import'}
        </button>
      </div>
      <textarea
        className="rules-source"
        value={csv}
        spellCheck={false}
        placeholder={'Paste CSV with a header row\nRegion,Product,Value\nNorth,Widget,100'}
        onChange={(e) => setCsv(e.target.value)}
        rows={5}
      />
      {columns.length > 0 ? (
        <div className="import-map">
          <p className="muted">Map each column:</p>
          {columns.map((col) => (
            <label key={col} className="import-row">
              <span>{col}</span>
              <select
                value={valueColumn === col ? '__value' : (mapping[col] ?? '__ignore')}
                onChange={(e) => {
                  const v = e.target.value
                  setMapping((m) => {
                    const next = { ...m }
                    delete next[col]
                    if (v !== '__ignore' && v !== '__value') next[col] = v
                    return next
                  })
                  if (v === '__value') setValueColumn(col)
                  else if (valueColumn === col) setValueColumn('')
                }}
              >
                <option value="__ignore">(ignore)</option>
                <option value="__value">value</option>
                {dimNames.map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
            </label>
          ))}
          <p className="muted">Fixed member for unmapped dimensions:</p>
          {dimNames
            .filter((d) => !mappedDims.has(d))
            .map((d) => (
              <label key={d} className="import-row">
                <span>{d}</span>
                <input
                  value={fixed[d] ?? ''}
                  placeholder="member"
                  onChange={(e) => setFixed((f) => ({ ...f, [d]: e.target.value }))}
                />
              </label>
            ))}
        </div>
      ) : null}
      {error ? <p className="error">{error}</p> : null}
      {report ? <RunReportView report={report} /> : null}
    </section>
  )
}

// ---- flow tests ----

function FlowTestPanel({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
  const [count, setCount] = useState<number | null>(null)
  const [report, setReport] = useState<TestReportDto | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)

  useEffect(() => {
    let live = true
    listFlowTests(cube)
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
      setReport(await runFlowTests(cube))
    } catch (e) {
      setReport(null)
      setError(e instanceof Error ? e.message : 'Could not run the tests')
    } finally {
      setRunning(false)
    }
  }

  return (
    <section className="test-panel">
      <div className="rules-editor-head">
        <h3>Flow tests {count !== null ? <small>({count})</small> : null}</h3>
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
