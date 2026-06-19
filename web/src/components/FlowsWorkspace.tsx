import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  deleteConnection,
  deleteSecret,
  getCube,
  importCsv,
  listConnections,
  listCubes,
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
  type FlowDto,
  type FlowInputDto,
  type FlowPreview,
  type RunReport,
  type TestReportDto,
} from '../api/client'
import { Badge, CodeEditor, Field, Input, Textarea, useConfirm } from '../ui'
import { appendTemplate, FLOW_TEMPLATES } from '../templates'
import { TestReport } from './TestReport'

const STARTER = `// A flow reads ctx.input() (the data rows) and stages changes.
// Address a cube explicitly with ctx.cube('Name'); read a declared source by
// name (ctx.input('source') for a global source, ctx.input('local.source') for
// a flow-scoped one).
function rows(ctx) {
  const data = ctx.input()
  // ctx.cube('Sales').ensureElements('Region', data.map(r => r.Column))
  // ctx.cube('Sales').writeCells(data.map(r => ({ coord: { Region: r.Column }, value: r.Value })))
}
`

// The modeler's flow workspace (ADR-0035): flows are server-global, not owned by
// any cube. Author the TypeScript body in the in-house CodeEditor (outputs are
// named in code via ctx.cube(...)), declare the flow's data sources in the
// UI-driven Data sources panel, run it, and run flow unit tests. A cube context
// (when present) additionally exposes the cube-scoped guided CSV import.
export default function FlowsWorkspace({
  cube,
  reloadSignal,
  isAdmin,
  initialFlow,
  autoNew,
  navSignal,
  onDirtyChange,
}: {
  /** A cube context, present only to host the cube-scoped guided CSV import.
   * Flow authoring itself is global and never depends on a selected cube. */
  cube?: string | null
  reloadSignal: number
  isAdmin: boolean
  /** Open this flow in the editor on mount / when it changes (from the tree). */
  initialFlow?: string
  /** Start with a blank "new flow" form (the tree's "New flow..." action). */
  autoNew?: boolean
  /** Bumped by the navigator to re-apply initialFlow/autoNew (e.g. clicking the
   * same flow twice). */
  navSignal?: number
  /** Reports unsaved-edit state up so the navigator can guard against silently
   * discarding flow source when the user clicks away in the tree. */
  onDirtyChange?: (dirty: boolean) => void
}) {
  const [selected, setSelected] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [source, setSource] = useState(STARTER)
  const [inputs, setInputs] = useState<FlowInputDto[]>([])
  const [owner, setOwner] = useState<string | null>(null)
  const [defaultCube, setDefaultCube] = useState<string | null>(null)
  // The last loaded/saved name+source+inputs, so we can tell whether it is dirty.
  const [savedName, setSavedName] = useState('')
  const [savedSource, setSavedSource] = useState(STARTER)
  const [savedInputs, setSavedInputs] = useState('[]')
  const [preview, setPreview] = useState<FlowPreview | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  // Set when the open flow is deleted out from under us (e.g. another tab/user).
  // putFlow is an idempotent PUT-by-name upsert, so a still-enabled Save would
  // silently re-create the just-deleted flow (lost-update). Disable Save and show
  // the not-found notice until the user starts a fresh flow / opens another.
  const [deleted, setDeleted] = useState(false)
  // The guided CSV import is cube-scoped (ADR-0035), but flows are global, so the
  // import target is chosen here rather than inherited from an ambient cube. The
  // picked cube's detail backs the column-to-dimension mapping.
  const [cubeNames, setCubeNames] = useState<string[]>([])
  const [importCube, setImportCube] = useState<string>(cube ?? '')
  const [detail, setDetail] = useState<CubeDetail | null>(null)

  useEffect(() => {
    let live = true
    listCubes()
      .then((cs) => {
        if (live) setCubeNames(cs.map((c) => c.name))
      })
      .catch(() => {
        if (live) setCubeNames([])
      })
    return () => {
      live = false
    }
  }, [reloadSignal])

  useEffect(() => {
    if (!importCube) {
      setDetail(null)
      return
    }
    getCube(importCube)
      .then(setDetail)
      .catch(() => setDetail(null))
  }, [importCube, reloadSignal])

  // Open the flow the navigator (tree) asked for, or a blank form for "New flow".
  useEffect(() => {
    if (autoNew) {
      setSelected(null)
      setName('')
      setSource(STARTER)
      setInputs([])
      setOwner(null)
      setDefaultCube(null)
      setSavedName('')
      setSavedSource(STARTER)
      setSavedInputs('[]')
      setError(null)
      setDeleted(false)
      return
    }
    if (!initialFlow) return
    let live = true
    listFlows()
      .then((fs) => {
        if (!live) return
        const f = fs.find((x) => x.name === initialFlow)
        if (f) {
          const fi = f.inputs ?? []
          setSelected(f.name)
          setName(f.name)
          setSource(f.source)
          setInputs(fi)
          setOwner(f.owner ?? null)
          setDefaultCube(f.default_cube ?? null)
          setSavedName(f.name)
          setSavedSource(f.source)
          setSavedInputs(JSON.stringify(fi))
          setError(null)
          setDeleted(false)
        } else {
          // The requested flow is gone (e.g. deleted in another tab). Reset.
          setSelected(null)
          setName('')
          setSource(STARTER)
          setInputs([])
          setOwner(null)
          setDefaultCube(null)
          setSavedName('')
          setSavedSource(STARTER)
          setSavedInputs('[]')
          setError(`Flow "${initialFlow}" was not found; it may have been deleted.`)
          setDeleted(false)
        }
      })
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : `Could not open flow "${initialFlow}".`),
      )
    return () => {
      live = false
    }
  }, [initialFlow, autoNew, navSignal])

  // While a flow is open, react to live reloadSignal bumps (a remote
  // objects_changed) by re-listing flows: if the open flow has been deleted out
  // from under us, surface the not-found notice and disable Save (putFlow is an
  // idempotent upsert that would otherwise silently re-create it). The editor
  // buffer is left intact so unsaved edits are not clobbered beyond reflecting
  // the deletion. Skipped while authoring a brand-new (unsaved) flow.
  useEffect(() => {
    if (!selected) return
    let live = true
    listFlows()
      .then((fs) => {
        if (!live) return
        const stillExists = fs.some((x) => x.name === selected)
        if (stillExists) {
          // Re-appeared (e.g. recreated) after a prior deletion notice: clear it.
          setDeleted(false)
        } else {
          setDeleted(true)
          setError(`Flow "${selected}" was deleted elsewhere. Your edits are kept, but Save is disabled so it is not re-created. Start a new flow or rename to save a copy.`)
        }
      })
      .catch(() => undefined)
    return () => {
      live = false
    }
    // Keyed on reloadSignal (+ selected) only: re-checking on every keystroke is
    // unnecessary, and `selected` only changes via the open effect above.
  }, [reloadSignal, selected])

  // Debounced validation of the edited source.
  useEffect(() => {
    if (source.trim() === '') {
      setPreview({ ok: true })
      return
    }
    const handle = setTimeout(() => {
      previewFlow(source)
        .then(setPreview)
        .catch((e: unknown) =>
          setPreview({ ok: false, message: e instanceof Error ? e.message : 'Invalid' }),
        )
    }, 300)
    return () => clearTimeout(handle)
  }, [source])

  const inputsJson = useMemo(() => JSON.stringify(inputs), [inputs])
  const dirty = name !== savedName || source !== savedSource || inputsJson !== savedInputs
  // Block Save only when it would resurrect the deleted flow under its own name.
  // Renaming turns it into a genuinely new flow, which is allowed to save.
  const wouldResurrect = deleted && name.trim() === (selected ?? '')

  // Report dirtiness up so the navigator can confirm before discarding edits;
  // clear it on unmount so a stale "dirty" never blocks the next navigation.
  useEffect(() => {
    onDirtyChange?.(dirty)
    return () => onDirtyChange?.(false)
  }, [dirty, onDirtyChange])

  async function save() {
    if (name.trim() === '') {
      setError('Please name the flow.')
      return
    }
    setSaving(true)
    setError(null)
    try {
      const flow: FlowDto = {
        name: name.trim(),
        source,
        inputs,
        owner,
        default_cube: defaultCube,
      }
      const saved = await putFlow(flow)
      const savedFi = saved.inputs ?? inputs
      setSelected(saved.name)
      setOwner(saved.owner ?? owner)
      setInputs(savedFi)
      // The saved name+source+inputs become the new clean baseline.
      setSavedName(saved.name)
      setSavedSource(source)
      setSavedInputs(JSON.stringify(savedFi))
      // A successful save (e.g. under a new name) means it exists again.
      setDeleted(false)
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
            <option value="">Insert template...</option>
            {FLOW_TEMPLATES.map((t, i) => (
              <option key={i} value={i} title={t.description}>
                {t.label}
              </option>
            ))}
          </select>
        </div>
        {owner ? <p className="muted">Runs as: {owner}</p> : null}
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
          <button
            className="primary"
            disabled={saving || preview?.ok === false || wouldResurrect}
            onClick={() => void save()}
          >
            {saving ? 'Saving...' : 'Save flow'}
          </button>
        </div>
      </section>

      <DataSourcesPanel inputs={inputs} onChange={setInputs} isAdmin={isAdmin} reloadSignal={reloadSignal} />

      {selected ? <RunPanel flow={selected} inputs={inputs} /> : null}
      {/* The guided CSV import is cube-scoped (ADR-0035): pick the target cube,
          then map columns to its dimensions. */}
      {cubeNames.length > 0 ? (
        <section className="flows-import">
          <h3>Import a CSV into a cube</h3>
          <Field label="Target cube">
            {(id, a11y) => (
              <select
                id={id}
                {...a11y}
                value={importCube}
                onChange={(e) => setImportCube(e.target.value)}
              >
                <option value="">Choose a cube...</option>
                {cubeNames.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
            )}
          </Field>
          {importCube && detail ? <ImportPanel cube={importCube} detail={detail} /> : null}
        </section>
      ) : null}
      <FlowTestPanel reloadSignal={reloadSignal} />
      {/* Global connections + HTTP secrets are operator configuration (admin
          only); a non-admin never sees the connector internals (ADR-0012/0030). */}
      {isAdmin ? <ConnectionsPanel reloadSignal={reloadSignal} /> : null}
    </div>
  )
}

// ---- data sources (the flow's declared inputs, ADR-0035) ----

/** The literal address a flow body uses to read a source: a global source by its
 * bare name, a flow-scoped one with a `local.` prefix. */
function inputAddress(input: FlowInputDto): string {
  return input.scope === 'global' ? `ctx.input('${input.name}')` : `ctx.input('local.${input.name}')`
}

/** The UI-driven editor for a flow's data sources. Outputs are named in code, but
 * inputs are configured here: add a reference to a global connection, or define a
 * flow-scoped (local) connection inline. */
function DataSourcesPanel({
  inputs,
  onChange,
  isAdmin,
  reloadSignal,
}: {
  inputs: FlowInputDto[]
  onChange: (next: FlowInputDto[]) => void
  isAdmin: boolean
  reloadSignal: number
}) {
  const [globals, setGlobals] = useState<ConnectionDto[]>([])
  const [globalPick, setGlobalPick] = useState('')
  const [addingLocal, setAddingLocal] = useState(false)
  const [copied, setCopied] = useState<string | null>(null)

  useEffect(() => {
    listConnections()
      .then(setGlobals)
      .catch(() => setGlobals([]))
  }, [reloadSignal])

  const usedGlobalNames = useMemo(
    () => new Set(inputs.filter((i) => i.scope === 'global').map((i) => i.name)),
    [inputs],
  )
  const localNames = useMemo(
    () => new Set(inputs.filter((i) => i.scope === 'local').map((i) => i.name)),
    [inputs],
  )
  // Global connections not yet referenced by this flow (no duplicates).
  const available = useMemo(
    () => globals.filter((c) => !usedGlobalNames.has(c.name)),
    [globals, usedGlobalNames],
  )

  function addGlobal() {
    if (globalPick === '') return
    if (usedGlobalNames.has(globalPick)) return
    onChange([...inputs, { name: globalPick, scope: 'global' }])
    setGlobalPick('')
  }

  function addLocal(conn: ConnectionDto) {
    onChange([...inputs, { name: conn.name, scope: 'local', connection: conn }])
    setAddingLocal(false)
  }

  function remove(index: number) {
    onChange(inputs.filter((_, i) => i !== index))
  }

  function copyAddress(addr: string) {
    void navigator.clipboard?.writeText(addr).then(
      () => {
        setCopied(addr)
        setTimeout(() => setCopied((c) => (c === addr ? null : c)), 1500)
      },
      () => undefined,
    )
  }

  return (
    <section className="flow-sources">
      <div className="rules-editor-head">
        <h3>Data sources</h3>
      </div>
      <p className="muted">
        Declare the data this flow reads. Global sources reference a shared connection; a flow-scoped
        source is defined just for this flow. Outputs are named in the flow code (ctx.cube(...)), so
        only inputs are configured here. When you run the flow, every declared source is fetched
        automatically.
      </p>
      <ul className="coord-list">
        {inputs.map((input, i) => {
          const addr = inputAddress(input)
          return (
            <li key={`${input.scope}:${input.name}`}>
              <strong>{input.name}</strong>{' '}
              <Badge tone={input.scope === 'global' ? 'info' : 'neutral'}>
                {input.scope === 'global' ? 'Global' : 'Local'}
              </Badge>{' '}
              {input.scope === 'local' && input.connection ? (
                <span className="muted">[{input.connection.kind}]</span>
              ) : null}{' '}
              <code className="flow-source__addr">{addr}</code>{' '}
              <button
                type="button"
                className="link"
                onClick={() => copyAddress(addr)}
                title="Copy the code address to the clipboard"
              >
                {copied === addr ? 'copied' : 'copy'}
              </button>{' '}
              <button
                type="button"
                className="link"
                onClick={() => remove(i)}
                title="Remove this data source"
                aria-label={`Remove data source ${input.name}`}
              >
                x
              </button>
            </li>
          )
        })}
        {inputs.length === 0 ? <li className="muted">No data sources declared</li> : null}
      </ul>

      <div className="flow-source__add">
        <div className="flow-source__add-global">
          <Field label="Add global source">
            {(id, a11y) => (
              <select
                id={id}
                {...a11y}
                value={globalPick}
                onChange={(e) => setGlobalPick(e.target.value)}
                disabled={available.length === 0}
              >
                <option value="">
                  {globals.length === 0
                    ? 'No global connections defined'
                    : available.length === 0
                      ? 'All global connections already added'
                      : 'Pick a connection...'}
                </option>
                {available.map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name} ({c.kind})
                  </option>
                ))}
              </select>
            )}
          </Field>
          <button type="button" disabled={globalPick === ''} onClick={addGlobal}>
            Add global source
          </button>
          {!isAdmin ? (
            <p className="muted">Global connections are managed by an administrator.</p>
          ) : null}
        </div>

        {addingLocal ? (
          <ConnectionForm
            title="Define a flow-scoped source"
            submitLabel="Add local source"
            existingNames={localNames}
            onCancel={() => setAddingLocal(false)}
            onBuilt={addLocal}
          />
        ) : (
          <button type="button" onClick={() => setAddingLocal(true)}>
            Add local source...
          </button>
        )}
      </div>
    </section>
  )
}

// ---- run a flow ----

function RunPanel({ flow, inputs }: { flow: string; inputs: FlowInputDto[] }) {
  const [csv, setCsv] = useState('')
  // The ad-hoc override target: '' = none (run declared sources), otherwise a
  // declared source address to feed inline content to.
  const [target, setTarget] = useState('')
  const [report, setReport] = useState<RunReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)

  const hasDeclared = inputs.length > 0
  // The addressable source names for the inline-override picker.
  const addresses = useMemo(
    () => inputs.map((i) => (i.scope === 'global' ? i.name : `local.${i.name}`)),
    [inputs],
  )

  async function run() {
    setRunning(true)
    setError(null)
    try {
      let body: Parameters<typeof runFlow>[1]
      if (target !== '') {
        // Feed inline content to a single named declared source.
        body = { inputs: { [target]: csv } }
      } else if (!hasDeclared) {
        // No declared sources: the ad-hoc CSV is the sole input.
        body = { input: csv }
      } else {
        // Declared sources are fetched automatically by the backend.
        body = {}
      }
      setReport(await runFlow(flow, body))
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
      {hasDeclared ? (
        <p className="muted">
          Declared sources are fetched automatically when the flow runs. To try ad-hoc data, pick a
          source below and paste rows for it.
        </p>
      ) : (
        <p className="muted">
          This flow has no declared sources. Paste CSV below to feed ctx.input() for a quick test.
        </p>
      )}
      {hasDeclared ? (
        <label className="muted">
          Override source with inline data{' '}
          <select value={target} onChange={(e) => setTarget(e.target.value)}>
            <option value="">none (fetch declared sources)</option>
            {addresses.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      {!hasDeclared || target !== '' ? (
        <textarea
          className="rules-source"
          value={csv}
          spellCheck={false}
          placeholder={'Paste CSV input (leave empty to run with no input rows)\nRegion,Value\nNorth,100'}
          onChange={(e) => setCsv(e.target.value)}
          rows={5}
        />
      ) : null}
      {error ? <p className="error" role="alert">{error}</p> : null}
      {report ? <RunReportView report={report} /> : null}
    </section>
  )
}

// ---- a reusable connection-building form (command / http / sql) ----

/** Builds a ConnectionDto from a small form. Used both to add a global connection
 * (ConnectionsPanel) and to define a flow-scoped source (DataSourcesPanel). The
 * caller decides what to do with the built connection via `onBuilt`. */
function ConnectionForm({
  title,
  submitLabel,
  existingNames,
  onBuilt,
  onCancel,
}: {
  title?: string
  submitLabel: string
  /** Names already taken in the caller's namespace (rejected as duplicates). */
  existingNames?: Set<string>
  onBuilt: (conn: ConnectionDto) => void
  onCancel?: () => void
}) {
  const [kind, setKind] = useState<'command' | 'http' | 'sql'>('command')
  const [name, setName] = useState('')
  const [program, setProgram] = useState('')
  const [args, setArgs] = useState('')
  const [url, setUrl] = useState('')
  const [authSecret, setAuthSecret] = useState('')
  const [format, setFormat] = useState('csv')
  const [workingDir, setWorkingDir] = useState('')
  const [sqlEngine, setSqlEngine] = useState('postgres')
  const [host, setHost] = useState('')
  const [port, setPort] = useState('')
  const [database, setDatabase] = useState('')
  const [dbUser, setDbUser] = useState('')
  const [query, setQuery] = useState('')
  const [sslMode, setSslMode] = useState('verify-full')
  const [error, setError] = useState<string | null>(null)

  function build() {
    const trimmed = name.trim()
    if (trimmed === '') {
      setError('A data source needs a name.')
      return
    }
    if (existingNames?.has(trimmed)) {
      setError(`A source named "${trimmed}" already exists here. Pick a unique name.`)
      return
    }
    if (kind === 'command') {
      if (program.trim() === '') {
        setError('A command data source needs a program.')
        return
      }
      onBuilt({
        name: trimmed,
        kind: 'command',
        program: program.trim(),
        // One argument per line.
        args: args.split('\n').map((a) => a.trim()).filter((a) => a !== ''),
        format,
        timeout_ms: 30000,
        working_dir: workingDir.trim() === '' ? null : workingDir.trim(),
      })
    } else if (kind === 'http') {
      if (url.trim() === '') {
        setError('An HTTP data source needs a url.')
        return
      }
      onBuilt({
        name: trimmed,
        kind: 'http',
        program: '',
        args: [],
        format,
        timeout_ms: 30000,
        url: url.trim(),
        auth: authSecret.trim() === '' ? null : { kind: 'bearer', secret: authSecret.trim() },
      })
    } else {
      if (host.trim() === '' || database.trim() === '' || query.trim() === '') {
        setError('A SQL data source needs a host, database, and query.')
        return
      }
      onBuilt({
        name: trimmed,
        kind: 'sql',
        program: '',
        args: [],
        format: 'csv',
        timeout_ms: 30000,
        engine: sqlEngine,
        host: host.trim(),
        port: Number(port) || (sqlEngine === 'mysql' ? 3306 : 5432),
        database: database.trim(),
        user: dbUser.trim(),
        query: query.trim(),
        ssl_mode: sslMode,
        password_secret: authSecret.trim() === '' ? null : authSecret.trim(),
      })
    }
  }

  return (
    <div className="conn-form">
      {title ? <h4>{title}</h4> : null}
      <Field label="Kind">
        {(id, a11y) => (
          <select
            id={id}
            {...a11y}
            value={kind}
            onChange={(e) => setKind(e.target.value as 'command' | 'http' | 'sql')}
          >
            <option value="command">command</option>
            <option value="http">http</option>
            <option value="sql">sql (database)</option>
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
      ) : kind === 'http' ? (
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
      ) : (
        <>
          <Field label="Engine">
            {(id, a11y) => (
              <select id={id} {...a11y} value={sqlEngine} onChange={(e) => setSqlEngine(e.target.value)}>
                <option value="postgres">PostgreSQL</option>
                <option value="mysql">MySQL / MariaDB</option>
              </select>
            )}
          </Field>
          <Field label="Host">
            {(id, a11y) => (
              <Input
                id={id}
                {...a11y}
                value={host}
                placeholder="db.internal (host must be allowlisted)"
                onChange={(e) => setHost(e.target.value)}
              />
            )}
          </Field>
          <Field label="Port">
            {(id, a11y) => (
              <Input
                id={id}
                {...a11y}
                type="number"
                value={port}
                placeholder={sqlEngine === 'mysql' ? '3306' : '5432'}
                onChange={(e) => setPort(e.target.value)}
              />
            )}
          </Field>
          <Field label="Database">
            {(id, a11y) => (
              <Input
                id={id}
                {...a11y}
                value={database}
                placeholder="analytics"
                onChange={(e) => setDatabase(e.target.value)}
              />
            )}
          </Field>
          <Field label="User">
            {(id, a11y) => (
              <Input
                id={id}
                {...a11y}
                value={dbUser}
                placeholder="reporting"
                onChange={(e) => setDbUser(e.target.value)}
              />
            )}
          </Field>
          <Field label="Password secret name">
            {(id, a11y) => (
              <Input
                id={id}
                {...a11y}
                value={authSecret}
                placeholder="optional, references a managed secret"
                onChange={(e) => setAuthSecret(e.target.value)}
              />
            )}
          </Field>
          <Field label="TLS mode">
            {(id, a11y) => (
              <select id={id} {...a11y} value={sslMode} onChange={(e) => setSslMode(e.target.value)}>
                <option value="verify-full">verify-full (default)</option>
                <option value="require">require (encrypt, no cert check)</option>
                <option value="disable">disable</option>
              </select>
            )}
          </Field>
          <Field label="Query">
            {(id, a11y) => (
              <Textarea
                id={id}
                {...a11y}
                value={query}
                placeholder={'SELECT region, amount::text FROM sales'}
                onChange={(e) => setQuery(e.target.value)}
                rows={3}
              />
            )}
          </Field>
        </>
      )}
      {kind !== 'sql' ? (
        <Field label="Format">
          {(id, a11y) => (
            <select id={id} {...a11y} value={format} onChange={(e) => setFormat(e.target.value)}>
              <option value="csv">csv</option>
              <option value="json">json</option>
            </select>
          )}
        </Field>
      ) : null}
      {error ? <p className="error" role="alert">{error}</p> : null}
      <div className="actions">
        <button className="primary" onClick={build}>
          {submitLabel}
        </button>
        {onCancel ? (
          <button type="button" onClick={onCancel}>
            Cancel
          </button>
        ) : null}
      </div>
    </div>
  )
}

// ---- global connection admin ----

function ConnectionsPanel({ reloadSignal }: { reloadSignal: number }) {
  const confirm = useConfirm()
  const [connections, setConnections] = useState<ConnectionDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  // The most recent "Test connection" result, keyed by connection name.
  const [preview, setPreview] = useState<{ name: string; data: ConnectionPreview } | null>(null)
  const [testing, setTesting] = useState<string | null>(null)

  const load = useCallback(() => {
    listConnections()
      .then(setConnections)
      .catch(() => undefined)
  }, [])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  const existing = useMemo(() => new Set(connections.map((c) => c.name)), [connections])

  async function add(conn: ConnectionDto) {
    setSaving(true)
    setError(null)
    try {
      await putConnection(conn)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the data source')
    } finally {
      setSaving(false)
    }
  }

  async function remove(connName: string) {
    const ok = await confirm({
      title: 'Delete connection',
      body: `Delete connection "${connName}"? Flows or schedules that read from it will fail until you re-create it. This cannot be undone.`,
      confirmLabel: 'Delete',
      danger: true,
    })
    if (!ok) return
    try {
      await deleteConnection(connName)
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
      const data = await previewConnection(connName)
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
        <h3>Connections</h3>
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
              {testing === c.name ? 'testing...' : 'test'}
            </button>{' '}
            <button
              className="link"
              onClick={() => void remove(c.name)}
              title="Delete"
              aria-label={`Delete connection ${c.name}`}
            >
              x
            </button>
            {preview?.name === c.name ? <PreviewTable data={preview.data} /> : null}
          </li>
        ))}
        {connections.length === 0 ? <li className="muted">No connections</li> : null}
      </ul>
      <p className="muted">
        Global connections are shared across flows and schedules (admin only; the server must enable
        the matching connector kind). An HTTP source can reference a named secret for its credential
        (managed below). Test a connection before using it in a flow.
      </p>
      <ConnectionForm
        submitLabel={saving ? 'Saving...' : 'Add connection'}
        existingNames={existing}
        onBuilt={(conn) => void add(conn)}
      />
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

// ---- guided CSV import (stays cube-scoped, ADR-0035) ----

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
        <h3>Guided CSV import <small>into {cube}</small></h3>
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

// ---- flow tests (server-global, ADR-0035) ----

function FlowTestPanel({ reloadSignal }: { reloadSignal: number }) {
  const [count, setCount] = useState<number | null>(null)
  const [report, setReport] = useState<TestReportDto | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)

  useEffect(() => {
    let live = true
    listFlowTests()
      .then((tests) => {
        if (live) setCount(tests.length)
      })
      .catch(() => undefined)
    return () => {
      live = false
    }
  }, [reloadSignal])

  async function run() {
    setRunning(true)
    setError(null)
    try {
      setReport(await runFlowTests())
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
