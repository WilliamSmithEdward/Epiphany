import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  deleteConnection,
  deleteSecret,
  getCube,
  importCsv,
  listConnections,
  listFlows,
  listFlowTests,
  listSecrets,
  previewConnection,
  putSecret,
  previewFlow,
  putConnection,
  putFlow,
  runFlow,
  runFlowTests,
  type ConnectionDto,
  type ConnectionPreview,
  type CubeDetail,
  type FlowPreview,
  type RunReport,
  type TestReportDto,
} from '../api/client'
import { CodeEditor, Field, Input, Textarea, useConfirm } from '../ui'
import { appendTemplate, FLOW_TEMPLATES } from '../templates'
import { TestReport } from './TestReport'

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
  isAdmin,
  initialFlow,
  autoNew,
  navSignal,
}: {
  cube: string
  reloadSignal: number
  isAdmin: boolean
  /** Open this flow in the editor on mount / when it changes (from the tree). */
  initialFlow?: string
  /** Start with a blank "new flow" form (the tree's "New flow…" action). */
  autoNew?: boolean
  /** Bumped by the navigator to re-apply initialFlow/autoNew even when the
   * cube is unchanged (e.g. clicking the same flow twice). */
  navSignal?: number
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  const [selected, setSelected] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [source, setSource] = useState(STARTER)
  const [preview, setPreview] = useState<FlowPreview | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const load = useCallback(() => {
    getCube(cube)
      .then(setDetail)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load'))
  }, [cube])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  // Open the flow the navigator (tree) asked for, or a blank form for "New flow".
  // Driven by cube/initialFlow/autoNew/navSignal so re-clicking re-applies it.
  useEffect(() => {
    if (autoNew) {
      setSelected(null)
      setName('')
      setSource(STARTER)
      setError(null)
      return
    }
    if (!initialFlow) return
    let live = true
    listFlows(cube)
      .then((fs) => {
        if (!live) return
        const f = fs.find((x) => x.name === initialFlow)
        if (f) {
          setSelected(f.name)
          setName(f.name)
          setSource(f.source)
          setError(null)
        } else {
          // The requested flow is gone (e.g. deleted in another tab). Don't
          // leave a stale editor implying it is open; reset and say so.
          setSelected(null)
          setName('')
          setSource(STARTER)
          setError(`Flow "${initialFlow}" was not found; it may have been deleted.`)
        }
      })
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : `Could not open flow "${initialFlow}".`),
      )
    return () => {
      live = false
    }
  }, [cube, initialFlow, autoNew, navSignal])

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

  return (
    <div className="flows-workspace">
      {/* The object explorer (tree) is the navigator: pick, open, create, run, and
          delete flows from its context menus. This pane is the editor + runner for
          the flow the tree opened (or a blank "new flow" form). */}
      <section className="flow-editor">
        <div className="rules-editor-head">
          <h3>{selected ? `Flow: ${selected}` : 'New flow'}</h3>
        </div>
        <div className="field-row">
          <label>
            Name
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. load_sales" />
          </label>
          <span
            role="status"
            aria-live="polite"
            className={
              preview?.ok === false ? 'error' : source.trim() === '' ? 'muted' : 'ok'
            }
          >
            {preview?.ok === false
              ? `${preview.line ? `Line ${preview.line}, col ${preview.column}: ` : ''}${preview.message}`
              : source.trim() === ''
                ? 'Empty'
                : 'Valid'}
          </span>
          <select
            className="template-pick"
            value=""
            aria-label="Insert a flow template"
            onChange={(e) => {
              const t = FLOW_TEMPLATES[Number(e.target.value)]
              if (t) setSource((s) => appendTemplate(s, t.body))
              e.target.value = ''
            }}
          >
            <option value="">Insert template…</option>
            {FLOW_TEMPLATES.map((t, i) => (
              <option key={i} value={i} title={t.description}>
                {t.label}
              </option>
            ))}
          </select>
        </div>
        <CodeEditor
          language="flow"
          value={source}
          onChange={setSource}
          ariaLabel="Flow source"
          rows={14}
          errorLine={preview?.ok === false ? preview.line : null}
        />
        {error ? <p className="error" role="alert">{error}</p> : null}
        <div className="actions">
          <button className="primary" disabled={saving || preview?.ok === false} onClick={() => void save()}>
            {saving ? 'Saving...' : 'Save flow'}
          </button>
        </div>
      </section>

      {selected ? <RunPanel cube={cube} flow={selected} reloadSignal={reloadSignal} /> : null}
      {detail ? <ImportPanel cube={cube} detail={detail} /> : null}
      <FlowTestPanel cube={cube} reloadSignal={reloadSignal} />
      {/* Data sources + HTTP secrets are operator configuration (admin only); a
          non-admin never sees the connector internals (ADR-0012/0030). */}
      {isAdmin ? <ConnectionsPanel cube={cube} reloadSignal={reloadSignal} /> : null}
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
          <option value="">Paste data (CSV)</option>
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
          placeholder={'Paste CSV input (leave empty to run with no input rows)\nRegion,Value\nNorth,100'}
          onChange={(e) => setCsv(e.target.value)}
          rows={5}
        />
      ) : (
        <p className="muted">Rows are fetched from the &quot;{source}&quot; data source.</p>
      )}
      {error ? <p className="error" role="alert">{error}</p> : null}
      {report ? <RunReportView report={report} /> : null}
    </section>
  )
}

// ---- connection admin (command connectors) ----

function ConnectionsPanel({ cube, reloadSignal }: { cube: string; reloadSignal: number }) {
  const confirm = useConfirm()
  const [connections, setConnections] = useState<ConnectionDto[]>([])
  const [kind, setKind] = useState<'command' | 'http'>('command')
  const [name, setName] = useState('')
  const [program, setProgram] = useState('')
  const [args, setArgs] = useState('')
  const [url, setUrl] = useState('')
  const [authSecret, setAuthSecret] = useState('')
  const [format, setFormat] = useState('csv')
  const [workingDir, setWorkingDir] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  // The most recent "Test connection" result, keyed by connection name.
  const [preview, setPreview] = useState<{ name: string; data: ConnectionPreview } | null>(null)
  const [testing, setTesting] = useState<string | null>(null)

  const load = useCallback(() => {
    listConnections(cube)
      .then(setConnections)
      .catch(() => undefined)
  }, [cube])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  function reset() {
    setName('')
    setProgram('')
    setArgs('')
    setWorkingDir('')
    setUrl('')
    setAuthSecret('')
  }

  async function add() {
    if (name.trim() === '') {
      setError('A data source needs a name.')
      return
    }
    setSaving(true)
    setError(null)
    try {
      if (kind === 'command') {
        if (program.trim() === '') {
          setError('A command data source needs a program.')
          setSaving(false)
          return
        }
        await putConnection(cube, {
          name: name.trim(),
          kind: 'command',
          program: program.trim(),
          // One argument per line.
          args: args.split('\n').map((a) => a.trim()).filter((a) => a !== ''),
          format,
          timeout_ms: 30000,
          working_dir: workingDir.trim() === '' ? null : workingDir.trim(),
        })
      } else {
        if (url.trim() === '') {
          setError('An HTTP data source needs a url.')
          setSaving(false)
          return
        }
        await putConnection(cube, {
          name: name.trim(),
          kind: 'http',
          program: '',
          args: [],
          format,
          timeout_ms: 30000,
          url: url.trim(),
          auth: authSecret.trim() === '' ? null : { kind: 'bearer', secret: authSecret.trim() },
        })
      }
      reset()
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the data source')
    } finally {
      setSaving(false)
    }
  }

  async function remove(connName: string) {
    const ok = await confirm({
      title: 'Delete data source',
      body: `Delete data source "${connName}"? Flows or scheduled jobs that read from it will fail until you re-create it. This cannot be undone.`,
      confirmLabel: 'Delete',
      danger: true,
    })
    if (!ok) return
    try {
      await deleteConnection(cube, connName)
      if (preview?.name === connName) setPreview(null)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not delete the connection')
    }
  }

  async function test(connName: string) {
    setTesting(connName)
    setError(null)
    setPreview(null)
    try {
      const data = await previewConnection(cube, connName)
      setPreview({ name: connName, data })
    } catch (e) {
      setError(e instanceof Error ? e.message : 'The connection test failed')
    } finally {
      setTesting(null)
    }
  }

  return (
    <section className="connections-panel">
      <div className="rules-editor-head">
        <h3>Data sources</h3>
      </div>
      <ul className="coord-list">
        {connections.map((c) => (
          <li key={c.name}>
            <strong>{c.name}</strong> [{c.kind}]{' '}
            {c.kind === 'http' ? (c.url ?? '') : `${c.program} ${c.args.join(' ')}`}{' '}
            <button
              className="link"
              onClick={() => void test(c.name)}
              disabled={testing === c.name}
              title="Run the connection and preview its output"
            >
              {testing === c.name ? 'testing…' : 'test'}
            </button>{' '}
            <button
              className="link"
              onClick={() => void remove(c.name)}
              title="Delete"
              aria-label={`Delete data source ${c.name}`}
            >
              x
            </button>
            {preview?.name === c.name ? <PreviewTable data={preview.data} /> : null}
          </li>
        ))}
        {connections.length === 0 ? <li className="muted">No data sources</li> : null}
      </ul>
      <p className="muted">
        Add a data source (admin only; the server must enable the matching connector kind). An HTTP
        source can reference a named secret for its credential (managed below). Test a source before
        using it in a flow.
      </p>
      <div className="conn-form">
        <Field label="Kind">
          {(id, a11y) => (
            <select
              id={id}
              {...a11y}
              value={kind}
              onChange={(e) => setKind(e.target.value as 'command' | 'http')}
            >
              <option value="command">command</option>
              <option value="http">http</option>
            </select>
          )}
        </Field>
        <Field label="Name">
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              value={name}
              placeholder="orders_csv"
              onChange={(e) => setName(e.target.value)}
            />
          )}
        </Field>
        {kind === 'command' ? (
          <>
            <Field label="Program">
              {(id, a11y) => (
                <Input
                  id={id}
                  {...a11y}
                  value={program}
                  placeholder="program (e.g. python)"
                  onChange={(e) => setProgram(e.target.value)}
                />
              )}
            </Field>
            <Field label="Arguments">
              {(id, a11y) => (
                <Textarea
                  id={id}
                  {...a11y}
                  value={args}
                  placeholder={'one argument per line\nscripts/extract.py\n--region=North'}
                  onChange={(e) => setArgs(e.target.value)}
                  rows={3}
                />
              )}
            </Field>
            <Field label="Working directory">
              {(id, a11y) => (
                <Input
                  id={id}
                  {...a11y}
                  value={workingDir}
                  placeholder="optional, absolute path"
                  onChange={(e) => setWorkingDir(e.target.value)}
                />
              )}
            </Field>
          </>
        ) : (
          <>
            <Field label="URL">
              {(id, a11y) => (
                <Input
                  id={id}
                  {...a11y}
                  value={url}
                  placeholder="https://api.example.com/data.csv (host must be allowlisted)"
                  onChange={(e) => setUrl(e.target.value)}
                />
              )}
            </Field>
            <Field label="Bearer-token secret name">
              {(id, a11y) => (
                <Input
                  id={id}
                  {...a11y}
                  value={authSecret}
                  placeholder="optional"
                  onChange={(e) => setAuthSecret(e.target.value)}
                />
              )}
            </Field>
          </>
        )}
        <Field label="Format">
          {(id, a11y) => (
            <select id={id} {...a11y} value={format} onChange={(e) => setFormat(e.target.value)}>
              <option value="csv">csv</option>
              <option value="json">json</option>
            </select>
          )}
        </Field>
        <button className="primary" disabled={saving} onClick={() => void add()}>
          {saving ? 'Saving...' : 'Add data source'}
        </button>
      </div>
      {error ? <p className="error" role="alert">{error}</p> : null}
      <SecretsPanel />
    </section>
  )
}

/** Manage the named HTTP credentials (ADR-0030; admin). Values are write-only:
 * the list shows names only, and a value is never returned after saving. */
function SecretsPanel() {
  const confirm = useConfirm()
  const [names, setNames] = useState<string[]>([])
  const [name, setName] = useState('')
  const [value, setValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(() => {
    listSecrets()
      .then(setNames)
      .catch(() => undefined)
  }, [])

  useEffect(() => {
    load()
  }, [load])

  async function add() {
    if (name.trim() === '' || value === '') {
      setError('A secret needs a name and a value.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await putSecret(name.trim(), value)
      setName('')
      setValue('')
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the secret')
    } finally {
      setBusy(false)
    }
  }

  async function remove(secretName: string) {
    const ok = await confirm({
      title: 'Delete secret',
      body: `Delete secret "${secretName}"? The value cannot be recovered, and any HTTP connection that references it will stop working.`,
      confirmLabel: 'Delete',
      danger: true,
    })
    if (!ok) return
    try {
      await deleteSecret(secretName)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not delete the secret')
    }
  }

  return (
    <div className="secrets-panel">
      <h4>HTTP secrets</h4>
      <p className="muted">
        Named credentials for HTTP data sources (admin only). Values are write-only: a secret is
        never shown again after you save it.
      </p>
      <ul className="coord-list">
        {names.map((n) => (
          <li key={n}>
            <strong>{n}</strong>{' '}
            <button
              className="link"
              onClick={() => void remove(n)}
              title="Delete"
              aria-label={`Delete secret ${n}`}
            >
              x
            </button>
          </li>
        ))}
        {names.length === 0 ? <li className="muted">No secrets</li> : null}
      </ul>
      <div className="conn-form">
        <Field label="Secret name">
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              value={name}
              placeholder="e.g. rates_token"
              onChange={(e) => setName(e.target.value)}
            />
          )}
        </Field>
        <Field label="Value">
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              type="password"
              value={value}
              placeholder="bearer token, or user:password for basic"
              onChange={(e) => setValue(e.target.value)}
            />
          )}
        </Field>
        <button disabled={busy} onClick={() => void add()}>
          {busy ? 'Saving...' : 'Add secret'}
        </button>
      </div>
      {error ? <p className="error" role="alert">{error}</p> : null}
    </div>
  )
}

/** Render a connection preview as a small table (first rows + total count). */
function PreviewTable({ data }: { data: ConnectionPreview }) {
  if (data.row_count === 0) {
    return <p className="muted">The connection ran but returned no rows.</p>
  }
  return (
    <div className="conn-preview">
      <p className="muted">
        {data.row_count} row{data.row_count === 1 ? '' : 's'}
        {data.rows.length < data.row_count ? ` (showing first ${data.rows.length})` : ''}
      </p>
      <table className="conn-preview-table">
        <thead>
          <tr>
            {data.columns.map((col) => (
              <th key={col}>{col}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {data.rows.map((row, i) => (
            <tr key={i}>
              {row.map((cell, j) => (
                <td key={j}>{cell}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
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
      {error ? <p className="error" role="alert">{error}</p> : null}
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
      {error ? <p className="error" role="alert">{error}</p> : null}
      {report ? <TestReport report={report} /> : null}
    </section>
  )
}
