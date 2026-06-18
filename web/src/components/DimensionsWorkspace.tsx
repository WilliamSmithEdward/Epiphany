import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  deleteDimension,
  getDimension,
  growDimension,
  listDimensions,
  registerDimension,
  type ElementKind,
  type SharedDimensionDetail,
  type SharedDimensionSummary,
} from '../api/client'
import {
  Badge,
  Button,
  Card,
  Dialog,
  EmptyState,
  Field,
  Input,
  Select,
  Switch,
  Textarea,
  useConfirm,
} from '../ui'

const KIND_OPTIONS = [
  { value: 'numeric', label: 'Number (input cell)' },
  { value: 'string', label: 'Text (input cell)' },
  { value: 'consolidated', label: 'Total (rolls up children)' },
]

function kindBadge(kind: ElementKind) {
  switch (kind) {
    case 'consolidated':
      return <Badge tone="info">total</Badge>
    case 'string':
      return <Badge tone="neutral">text</Badge>
    default:
      return <Badge tone="neutral">number</Badge>
  }
}

// The shared Dimension Library (ADR-0024): register reusable dimensions once and
// reference them from many cubes. Editing a shared dimension here fans the change
// out to every cube that references it, so they never drift. Append-only:
// members and edges are added, never removed. Gated server-side on the global
// Dimension permission; a user without it sees the access notice.
export default function DimensionsWorkspace({
  reloadSignal,
  initialDimId,
  autoNew,
  navSignal,
}: {
  reloadSignal: number
  /** Focus this shared dimension on mount / when it changes (from the tree). */
  initialDimId?: number
  /** Open the register-dimension wizard immediately (the tree's action). */
  autoNew?: boolean
  /** Bumped by the navigator to re-apply initialDimId/autoNew when unchanged. */
  navSignal?: number
}) {
  const [list, setList] = useState<SharedDimensionSummary[] | null>(null)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [forbidden, setForbidden] = useState(false)
  const [showNew, setShowNew] = useState(false)

  const load = useCallback(() => {
    listDimensions()
      .then((items) => {
        setForbidden(false)
        setError(null)
        setList(items)
        setSelectedId((current) =>
          current !== null && items.some((d) => d.id === current) ? current : items[0]?.id ?? null,
        )
      })
      .catch((e: unknown) => {
        const message = e instanceof Error ? e.message : 'Failed to load the dimension library'
        // A 403 surfaces as a friendly access notice rather than a raw error.
        if (/access/i.test(message)) setForbidden(true)
        else setError(message)
        setList([])
      })
  }, [])

  useEffect(() => {
    load()
  }, [load, reloadSignal])

  // Focus the shared dimension the navigator (tree) asked for. navSignal lets
  // re-clicking the same one re-focus it.
  useEffect(() => {
    if (initialDimId !== undefined) setSelectedId(initialDimId)
  }, [initialDimId, navSignal])

  // Open the register wizard when the tree's "Register shared dimension…" action
  // navigates here.
  useEffect(() => {
    if (autoNew) setShowNew(true)
  }, [autoNew, navSignal])

  const selected = useMemo(
    () => (selectedId !== null ? list?.find((d) => d.id === selectedId) ?? null : null),
    [list, selectedId],
  )

  if (forbidden) {
    return (
      <EmptyState icon="⬡" title="No access to dimensions">
        You do not have access to dimensions yet. Ask an administrator to grant you access.
      </EmptyState>
    )
  }

  if (!list) {
    return <p className="banner" role="status">Loading dimensions…</p>
  }

  return (
    <div className="model-workspace">
      <Card
        title="Dimensions"
        subtitle="Dimensions you can reuse across cubes. Editing one updates every cube that references it."
        actions={
          <Button size="sm" variant="primary" onClick={() => setShowNew(true)}>
            New dimension
          </Button>
        }
      >
        {error ? <p className="error" role="alert">{error}</p> : null}
        {list.length === 0 ? (
          <EmptyState icon="⬡" title="No dimensions yet">
            Register a dimension here, then reference it when you create a cube. One edit keeps every
            cube that uses it consistent.
          </EmptyState>
        ) : (
          <div className="model-dims">
            {list.map((d) => (
              <button
                key={d.id}
                type="button"
                className={d.id === selectedId ? 'model-dim is-active' : 'model-dim'}
                aria-pressed={d.id === selectedId}
                onClick={() => setSelectedId(d.id)}
              >
                <span className="model-dim__name">{d.name}</span>
                <span className="model-dim__count">
                  {d.element_count} {d.element_count === 1 ? 'member' : 'members'} ·{' '}
                  {d.references.length} {d.references.length === 1 ? 'cube' : 'cubes'}
                </span>
              </button>
            ))}
          </div>
        )}
      </Card>

      {selected ? <SharedDimensionEditor id={selected.id} onChanged={load} /> : null}

      {showNew ? (
        <NewDimensionDialog
          onClose={() => setShowNew(false)}
          onCreated={() => {
            setShowNew(false)
            load()
          }}
        />
      ) : null}
    </div>
  )
}

// ---- editor for one shared dimension ----

function SharedDimensionEditor({ id, onChanged }: { id: number; onChanged: () => void }) {
  const confirm = useConfirm()
  const [detail, setDetail] = useState<SharedDimensionDetail | null>(null)
  const [memberName, setMemberName] = useState('')
  const [memberKind, setMemberKind] = useState<ElementKind>('numeric')
  const [parent, setParent] = useState('')
  const [child, setChild] = useState('')
  const [weight, setWeight] = useState('1')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(() => {
    getDimension(id)
      .then(setDetail)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load the dimension'))
  }, [id])

  useEffect(() => {
    load()
  }, [load])

  if (!detail) {
    return null
  }

  const consolidated = detail.elements.filter((e) => e.kind === 'consolidated')
  const elementOptions = detail.elements.map((e) => ({ value: e.name, label: e.name }))
  const parentOptions = consolidated.map((e) => ({ value: e.name, label: e.name }))

  async function addMember() {
    const name = memberName.trim()
    if (name === '') {
      setError('Give the new member a name.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await growDimension(id, [{ name, kind: memberKind }], [])
      setMemberName('')
      load()
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
      await growDimension(id, [], [{ parent, child, weight: w }])
      setChild('')
      load()
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not add the rollup')
    } finally {
      setBusy(false)
    }
  }

  async function remove() {
    const ok = await confirm({
      title: 'Delete dimension',
      body: `Delete dimension "${detail?.name ?? ''}"? This permanently removes the dimension and cannot be undone.`,
      confirmLabel: 'Delete',
      danger: true,
    })
    if (!ok) return
    setBusy(true)
    setError(null)
    try {
      await deleteDimension(id)
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not delete the dimension')
      setBusy(false)
    }
  }

  const unreferenced = detail.references.length === 0

  return (
    <Card
      title={`Dimension: ${detail.name}`}
      subtitle={
        detail.references.length > 0
          ? `Generation ${detail.generation}. Used by ${detail.references.join(', ')}. Changes fan out to all of them.`
          : `Generation ${detail.generation}. Not referenced by any cube yet.`
      }
      actions={
        <Button
          size="sm"
          variant="ghost"
          disabled={busy || !unreferenced}
          title={
            unreferenced
              ? 'Delete this dimension'
              : 'Referenced by one or more cubes; cannot delete'
          }
          onClick={() => void remove()}
        >
          Delete
        </Button>
      }
    >
      <div className="model-editor">
        <section>
          <h4 className="model-editor__h">Members</h4>
          {detail.elements.length === 0 ? (
            <p className="muted">No members yet.</p>
          ) : (
            <ul className="model-members">
              {detail.elements.map((e) => (
                <li key={e.name} className="model-member">
                  <span className="model-member__name">{e.name}</span>
                  {kindBadge(e.kind)}
                </li>
              ))}
            </ul>
          )}
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
              {detail.edges.length > 0 ? (
                <ul className="model-edges">
                  {detail.edges.map((e, i) => (
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

        {error ? <p className="error" role="alert">{error}</p> : null}
      </div>
    </Card>
  )
}

// ---- new shared-dimension wizard ----

function NewDimensionDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void
  onCreated: () => void
}) {
  const [name, setName] = useState('')
  const [members, setMembers] = useState('')
  const [total, setTotal] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [fieldErrors, setFieldErrors] = useState<{ name?: string; members?: string }>({})
  const [busy, setBusy] = useState(false)

  async function create() {
    setFieldErrors({})
    setError(null)
    const dn = name.trim()
    if (dn === '') {
      setFieldErrors({ name: 'Give the dimension a name.' })
      return
    }
    const memberList = members
      .split('\n')
      .map((m) => m.trim())
      .filter((m) => m !== '')
    if (memberList.length === 0) {
      setFieldErrors({ members: 'Add at least one member, one per line.' })
      return
    }
    const elements = memberList.map((m) => ({ name: m, kind: 'numeric' as ElementKind }))
    const edges: { parent: string; child: string; weight?: number }[] = []
    if (total) {
      elements.push({ name: 'Total', kind: 'consolidated' as ElementKind })
      for (const m of memberList) edges.push({ parent: 'Total', child: m, weight: 1 })
    }

    setBusy(true)
    setError(null)
    try {
      await registerDimension({ name: dn, elements, edges })
      onCreated()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not register the dimension')
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
      title="New dimension"
      description="Define a dimension once here, then reference it from any cube. Editing it later updates every cube that uses it."
      size="md"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy} onClick={() => void create()}>
            {busy ? 'Registering…' : 'Register'}
          </Button>
        </>
      }
    >
      <div className="new-cube">
        <Field
          label="Dimension name"
          hint="For example Product or Region."
          error={fieldErrors.name}
        >
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Product"
            />
          )}
        </Field>
        <Field label="Members" hint="One per line." error={fieldErrors.members}>
          {(id, a11y) => (
            <Textarea
              id={id}
              {...a11y}
              value={members}
              onChange={(e) => setMembers(e.target.value)}
              placeholder={'Widget\nGadget'}
              rows={4}
            />
          )}
        </Field>
        <Switch
          checked={total}
          onCheckedChange={setTotal}
          label="Add a Total"
          description="Creates a Total member that sums every member of this dimension."
        />
        {error ? <p className="error" role="alert">{error}</p> : null}
      </div>
    </Dialog>
  )
}
