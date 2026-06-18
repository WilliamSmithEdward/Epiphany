import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  addElements,
  createCube,
  defineAttribute,
  getCube,
  listDimensions,
  setAttributeValues,
  type AttributeKind,
  type CubeDetail,
  type DimensionDto,
  type ElementKind,
  type NewDimension,
  type SharedDimensionSummary,
} from '../api/client'
import { Badge, Button, Card, Dialog, EmptyState, Field, Input, Select, Switch, Textarea } from '../ui'
import MemberTable from './MemberTable'

const KIND_OPTIONS = [
  { value: 'numeric', label: 'Number (input cell)' },
  { value: 'string', label: 'Text (input cell)' },
  { value: 'consolidated', label: 'Total (rolls up children)' },
]

const ATTR_KIND_OPTIONS = [
  { value: 'text', label: 'Text' },
  { value: 'numeric', label: 'Number' },
  { value: 'alias', label: 'Alias (alternate name)' },
]

// The Data Model workspace (ADR-0021): see a cube's structure, add members and
// build consolidation hierarchies, and create a brand-new cube. Additive only;
// elements and dimensions are never removed or renamed (an append-only model).
export default function ModelWorkspace({
  cube,
  reloadSignal,
  isAdmin,
  onCubeCreated,
  onOpenDimension,
  initialDim,
  autoNew,
  navSignal,
}: {
  cube: string
  reloadSignal: number
  isAdmin: boolean
  onCubeCreated: (name: string) => void
  /** Open a dimension in the standalone structural editor (ADR-0036). The
   * dimension cards call this so the cube model page no longer edits structure
   * inline; the panel below keeps the append-only member / roll-up / attribute
   * forms for the focused dimension. */
  onOpenDimension?: (dim: string) => void
  /** Focus this dimension on mount / when it changes (from the tree). */
  initialDim?: string
  /** Open the New-cube wizard immediately (the tree's "New cube…" action). */
  autoNew?: boolean
  /** Bumped by the navigator to re-apply initialDim/autoNew when unchanged. */
  navSignal?: number
}) {
  const [detail, setDetail] = useState<CubeDetail | null>(null)
  const [dimName, setDimName] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [showNewCube, setShowNewCube] = useState(false)

  const load = useCallback(() => {
    getCube(cube)
      .then((d) => {
        setDetail(d)
        setDimName((current) => (d.dimensions.some((dim) => dim.name === current) ? current : d.dimensions[0]?.name ?? ''))
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load the model'))
  }, [cube])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  // Focus the dimension the navigator (tree) asked for. navSignal lets re-clicking
  // the same dimension re-focus it.
  useEffect(() => {
    if (initialDim) setDimName(initialDim)
  }, [cube, initialDim, navSignal])

  // Open the New-cube wizard when the tree's "New cube…" action navigates here.
  useEffect(() => {
    if (autoNew) setShowNewCube(true)
  }, [autoNew, navSignal])

  const dimension = useMemo(
    () => detail?.dimensions.find((d) => d.name === dimName) ?? null,
    [detail, dimName],
  )

  if (!detail) {
    return <p className="banner" role="status">Loading {cube}…</p>
  }

  return (
    <div className="model-workspace">
      <Card
        title="Data model"
        subtitle={`Dimensions, members, and hierarchies for ${cube}.`}
        actions={
          isAdmin ? (
            <Button size="sm" variant="primary" onClick={() => setShowNewCube(true)}>
              New cube
            </Button>
          ) : undefined
        }
      >
        {error ? <p className="error" role="alert">{error}</p> : null}
        <div className="model-dims">
          {detail.dimensions.map((d) => (
            <button
              key={d.name}
              type="button"
              className={d.name === dimName ? 'model-dim is-active' : 'model-dim'}
              aria-pressed={d.name === dimName}
              onClick={() => (onOpenDimension ? onOpenDimension(d.name) : setDimName(d.name))}
            >
              <span className="model-dim__name">{d.name}</span>
              <span className="model-dim__count">
                {d.elements.length} {d.elements.length === 1 ? 'member' : 'members'}
              </span>
            </button>
          ))}
        </div>
        {onOpenDimension ? (
          <p className="field__msg field__msg--hint">
            Pick a dimension to open it in the editor, where you can drag members to reorder and
            reshape the hierarchy.
          </p>
        ) : null}
      </Card>

      {dimension ? (
        <DimensionDetailPanel cube={cube} dimension={dimension} onChanged={load} />
      ) : (
        <EmptyState icon="◫" title="No dimension selected">
          Pick a dimension above to view and edit its members.
        </EmptyState>
      )}

      {showNewCube ? (
        <NewCubeDialog
          onClose={() => setShowNewCube(false)}
          onCreated={(name) => {
            setShowNewCube(false)
            onCubeCreated(name)
          }}
        />
      ) : null}
    </div>
  )
}

// ---- append-only member / roll-up / attribute panel for one dimension ----
// Structural editing (reorder, reparent, delete, kind change) lives in the
// standalone DimensionEditor (ADR-0036); this panel keeps the additive member /
// roll-up forms and the attribute management the structural editor does not own.

function DimensionDetailPanel({
  cube,
  dimension,
  onChanged,
}: {
  cube: string
  dimension: DimensionDto
  onChanged: () => void
}) {
  const [memberName, setMemberName] = useState('')
  const [memberKind, setMemberKind] = useState<ElementKind>('numeric')
  const [parent, setParent] = useState('')
  const [child, setChild] = useState('')
  const [weight, setWeight] = useState('1')
  const [attrName, setAttrName] = useState('')
  const [attrKind, setAttrKind] = useState<AttributeKind>('text')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const consolidated = dimension.elements.filter((e) => e.kind === 'consolidated')
  const elementOptions = dimension.elements.map((e) => ({ value: e.name, label: e.name }))
  const parentOptions = consolidated.map((e) => ({ value: e.name, label: e.name }))
  const attributes = dimension.attributes ?? []

  async function addMember() {
    const name = memberName.trim()
    if (name === '') {
      setError('Give the new member a name.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await addElements(cube, [{ dimension: dimension.name, name, kind: memberKind }], [])
      setMemberName('')
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not add the member')
    } finally {
      setBusy(false)
    }
  }

  async function addConsolidation() {
    if (parent === '' || child === '') {
      setError('Pick both a total and a member to roll up.')
      return
    }
    const w = Number(weight)
    if (!Number.isFinite(w) || !Number.isInteger(w)) {
      setError('Weight must be a whole number (often 1, or -1 to subtract).')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await addElements(
        cube,
        [],
        [{ dimension: dimension.name, parent, child, weight: w }],
      )
      setChild('')
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not add the rollup')
    } finally {
      setBusy(false)
    }
  }

  async function addAttribute() {
    const name = attrName.trim()
    if (name === '') {
      setError('Name the attribute.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await defineAttribute(cube, dimension.name, name, attrKind)
      setAttrName('')
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not add the attribute')
    } finally {
      setBusy(false)
    }
  }

  // Commit one inline attribute-cell edit from the member table.
  async function editAttr(element: string, attribute: string, value: string) {
    setError(null)
    try {
      await setAttributeValues(cube, dimension.name, attribute, [{ element, value }])
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not set the value')
    }
  }

  return (
    <Card title={`Dimension: ${dimension.name}`}>
      <div className="model-editor">
        <section>
          <h4 className="model-editor__h">Members</h4>
          <MemberTable dimension={dimension} editable onAttrEdit={editAttr} />
          <div className="model-add-row">
            <Input
              value={memberName}
              onChange={(ev) => setMemberName(ev.target.value)}
              placeholder="New member name"
              aria-label="New member name"
            />
            <Select
              value={memberKind}
              onValueChange={(v) => setMemberKind(v as ElementKind)}
              options={KIND_OPTIONS}
              ariaLabel="Member kind"
            />
            <Button size="sm" variant="secondary" disabled={busy} onClick={() => void addMember()}>
              Add member
            </Button>
          </div>
        </section>

        <section>
          <h4 className="model-editor__h">Roll-ups</h4>
          <p className="field__msg field__msg--hint">
            A total adds up the members beneath it. Pick a total, then the member it should include
            (weight 1 adds, -1 subtracts).
          </p>
          {consolidated.length === 0 ? (
            <p className="muted">
              Add a member with kind &quot;Total&quot; first, then you can roll members up into it.
            </p>
          ) : (
            <>
              {dimension.edges.length > 0 ? (
                <ul className="model-edges">
                  {dimension.edges.map((e, i) => (
                    <li key={`${e.parent}-${e.child}-${i}`}>
                      <strong>{e.parent}</strong> ← {e.child}
                      {e.weight !== 1 ? <span className="model-edge__w"> (x{e.weight})</span> : null}
                    </li>
                  ))}
                </ul>
              ) : null}
              <div className="model-add-row">
                <Select
                  value={parent}
                  onValueChange={setParent}
                  options={parentOptions}
                  placeholder="Total…"
                  ariaLabel="Total element"
                />
                <span className="muted">includes</span>
                <Select
                  value={child}
                  onValueChange={setChild}
                  options={elementOptions}
                  placeholder="Member…"
                  ariaLabel="Member to roll up"
                />
                <Input
                  type="number"
                  className="model-weight"
                  value={weight}
                  onChange={(ev) => setWeight(ev.target.value)}
                  aria-label="Weight"
                />
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={busy}
                  onClick={() => void addConsolidation()}
                >
                  Add roll-up
                </Button>
              </div>
            </>
          )}
        </section>

        <section>
          <h4 className="model-editor__h">Attributes</h4>
          <p className="field__msg field__msg--hint">
            Attributes label members with extra data, like a currency code, a display alias, or a
            numeric weight.
          </p>
          {attributes.length > 0 ? (
            <ul className="model-edges">
              {attributes.map((a) => (
                <li key={a.name}>
                  <strong>{a.name}</strong> <Badge tone="neutral">{a.kind}</Badge>
                  {a.values.length > 0 ? (
                    <span className="model-edge__w"> · {a.values.length} set</span>
                  ) : null}
                </li>
              ))}
            </ul>
          ) : (
            <p className="muted">No attributes yet.</p>
          )}
          <div className="model-add-row">
            <Input
              value={attrName}
              onChange={(e) => setAttrName(e.target.value)}
              placeholder="New attribute name"
              aria-label="New attribute name"
            />
            <Select
              value={attrKind}
              onValueChange={(v) => setAttrKind(v as AttributeKind)}
              options={ATTR_KIND_OPTIONS}
              ariaLabel="Attribute kind"
            />
            <Button size="sm" variant="secondary" disabled={busy} onClick={() => void addAttribute()}>
              Add attribute
            </Button>
          </div>
          {attributes.length > 0 ? (
            <p className="field__msg field__msg--hint">
              Set a value by clicking its cell in the members table above (turn on the attribute
              column from Columns if it is hidden).
            </p>
          ) : null}
        </section>

        {error ? <p className="error" role="alert">{error}</p> : null}
      </div>
    </Card>
  )
}

// ---- new-cube wizard ----

interface DraftDimension {
  /** Inline (define members here) or a reference to a shared dimension. */
  source: 'inline' | 'reference'
  /** Chosen shared-dimension id when `source` is `reference`. */
  ref: number | null
  name: string
  /** Member names, one per line. */
  members: string
  /** Add a consolidated "Total" that sums every member. */
  total: boolean
}

function newDraft(): DraftDimension {
  return { source: 'inline', ref: null, name: '', members: '', total: true }
}

function NewCubeDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void
  onCreated: (name: string) => void
}) {
  const [name, setName] = useState('')
  const [dims, setDims] = useState<DraftDimension[]>([newDraft()])
  const [library, setLibrary] = useState<SharedDimensionSummary[]>([])
  const [error, setError] = useState<string | null>(null)
  const [nameError, setNameError] = useState<string | undefined>(undefined)
  const [dimErrors, setDimErrors] = useState<Record<number, string>>({})
  const [busy, setBusy] = useState(false)

  // Load the shared dimension library so dimensions can be added by reference.
  // A failure (e.g. no Dimension read access) just leaves inline-only.
  useEffect(() => {
    listDimensions()
      .then(setLibrary)
      .catch(() => setLibrary([]))
  }, [])

  const libraryOptions = library.map((d) => ({ value: String(d.id), label: d.name }))

  function update(index: number, patch: Partial<DraftDimension>) {
    setDims((ds) => ds.map((d, i) => (i === index ? { ...d, ...patch } : d)))
  }

  async function create() {
    setNameError(undefined)
    setDimErrors({})
    setError(null)
    const cubeName = name.trim()
    if (cubeName === '') {
      setNameError('Give the cube a name.')
      return
    }
    const dimensions: NewDimension[] = []
    for (let i = 0; i < dims.length; i++) {
      const d = dims[i]
      if (d.source === 'reference') {
        if (d.ref === null) {
          setDimErrors({ [i]: 'Pick a dimension, or switch it to define inline.' })
          return
        }
        const shared = library.find((s) => s.id === d.ref)
        dimensions.push({ name: shared?.name ?? `#${d.ref}`, ref: d.ref })
        continue
      }
      const dn = d.name.trim()
      if (dn === '') {
        setDimErrors({ [i]: 'Every dimension needs a name.' })
        return
      }
      const members = d.members
        .split('\n')
        .map((m) => m.trim())
        .filter((m) => m !== '')
      if (members.length === 0) {
        setDimErrors({ [i]: `Dimension "${dn}" needs at least one member.` })
        return
      }
      const elements = members.map((m) => ({ name: m, kind: 'numeric' as ElementKind }))
      const edges: { parent: string; child: string; weight?: number }[] = []
      if (d.total) {
        elements.push({ name: 'Total', kind: 'consolidated' as ElementKind })
        for (const m of members) edges.push({ parent: 'Total', child: m, weight: 1 })
      }
      dimensions.push({ name: dn, elements, edges })
    }

    setBusy(true)
    setError(null)
    try {
      await createCube(cubeName, dimensions)
      onCreated(cubeName)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not create the cube')
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog
      open
      onOpenChange={(o) => {
        if (!o) onClose()
      }}
      title="Create a cube"
      description="Name the cube and declare its dimensions. You can add more members and roll-ups afterward, but dimensions cannot be added later, so list them all here."
      size="lg"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy} onClick={() => void create()}>
            {busy ? 'Creating…' : 'Create cube'}
          </Button>
        </>
      }
    >
      <div className="new-cube">
        <Field label="Cube name" hint="For example Sales or Budget." error={nameError}>
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Sales"
            />
          )}
        </Field>

        <div className="field">
          <span className="field__label">Dimensions</span>
          {dims.map((d, i) => (
            <div className="new-cube__dim" key={i}>
              <div className="new-cube__dim-head">
                {library.length > 0 ? (
                  <Select
                    value={d.source}
                    onValueChange={(v) =>
                      update(i, { source: v as DraftDimension['source'] })
                    }
                    options={[
                      { value: 'inline', label: 'Define here' },
                      { value: 'reference', label: 'Reuse an existing dimension' },
                    ]}
                    ariaLabel={`Dimension ${i + 1} source`}
                  />
                ) : null}
                {d.source === 'inline' ? (
                  <Input
                    value={d.name}
                    onChange={(e) => update(i, { name: e.target.value })}
                    placeholder={`Dimension ${i + 1} name (e.g. Region)`}
                    aria-label={`Dimension ${i + 1} name`}
                    aria-invalid={dimErrors[i] ? true : undefined}
                    aria-describedby={dimErrors[i] ? `new-cube-dim-${i}-err` : undefined}
                  />
                ) : (
                  <Select
                    value={d.ref !== null ? String(d.ref) : undefined}
                    onValueChange={(v) => update(i, { ref: Number(v) })}
                    options={libraryOptions}
                    placeholder="Pick a dimension…"
                    ariaLabel={`Dimension ${i + 1} reuse an existing dimension`}
                  />
                )}
                {dims.length > 1 ? (
                  <button
                    type="button"
                    className="icon-btn"
                    onClick={() => setDims((ds) => ds.filter((_, j) => j !== i))}
                    title="Remove dimension"
                    aria-label={`Remove dimension ${i + 1}`}
                  >
                    ✕
                  </button>
                ) : null}
              </div>
              {d.source === 'inline' ? (
                <>
                  <Textarea
                    value={d.members}
                    onChange={(e) => update(i, { members: e.target.value })}
                    placeholder={'Members, one per line\nNorth\nSouth'}
                    aria-label={`Dimension ${i + 1} members`}
                    aria-invalid={dimErrors[i] ? true : undefined}
                    aria-describedby={dimErrors[i] ? `new-cube-dim-${i}-err` : undefined}
                    rows={3}
                  />
                  <Switch
                    checked={d.total}
                    onCheckedChange={(v) => update(i, { total: v })}
                    label="Add a Total"
                    description="Creates a Total member that sums every member of this dimension."
                  />
                </>
              ) : (
                <p className="field__msg field__msg--hint">
                  This dimension reuses an existing dimension. Editing it later updates every cube that
                  references it.
                </p>
              )}
              {dimErrors[i] ? (
                <p
                  id={`new-cube-dim-${i}-err`}
                  className="field__msg field__msg--error"
                  role="alert"
                >
                  {dimErrors[i]}
                </p>
              ) : null}
            </div>
          ))}
          <Button
            size="sm"
            variant="ghost"
            icon="+"
            onClick={() => setDims((ds) => [...ds, newDraft()])}
          >
            Add dimension
          </Button>
        </div>

        {error ? <p className="error" role="alert">{error}</p> : null}
      </div>
    </Dialog>
  )
}
