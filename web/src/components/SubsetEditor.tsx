import { useEffect, useState } from 'react'
import {
  createSubset,
  previewMdx,
  type DimensionDto,
  type MemberDto,
  type SubsetDef,
  type Visibility,
} from '../api/client'
import { Tabs, TabPanel } from '../ui'
import MemberSetPicker from './MemberSetPicker'

// Create a subset for one dimension. Two modes: a default tree picker producing a
// static member list, and an opt-in MDX expression with a live resolved-members
// preview. On save it POSTs the subset and calls onSaved with its name.
export default function SubsetEditor({
  cube,
  dimension,
  onSaved,
  onCancel,
}: {
  cube: string
  dimension: DimensionDto
  onSaved: (name: string) => void
  onCancel: () => void
}) {
  const [tab, setTab] = useState<'pick' | 'mdx'>('pick')
  const [name, setName] = useState('')
  const [visibility, setVisibility] = useState<Visibility>('public')
  const [included, setIncluded] = useState<string[]>([])
  const [mdx, setMdx] = useState('')
  const [preview, setPreview] = useState<MemberDto[]>([])
  const [previewError, setPreviewError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  // Debounced live preview of the MDX expression.
  useEffect(() => {
    if (tab !== 'mdx' || mdx.trim() === '') {
      setPreview([])
      setPreviewError(null)
      return
    }
    const handle = setTimeout(() => {
      previewMdx(cube, dimension.name, mdx)
        .then((members) => {
          setPreview(members)
          setPreviewError(null)
        })
        .catch((err: unknown) => {
          setPreview([])
          setPreviewError(err instanceof Error ? err.message : 'Invalid expression')
        })
    }, 300)
    return () => clearTimeout(handle)
  }, [cube, dimension.name, mdx, tab])

  async function save() {
    if (name.trim() === '') {
      setError('Please name the subset.')
      return
    }
    if (tab === 'pick' && included.length === 0) {
      setError('Add at least one member to the set.')
      return
    }
    // The included list is already in the order the user arranged.
    const def: SubsetDef =
      tab === 'mdx'
        ? { name: name.trim(), visibility, kind: 'dynamic', mdx }
        : { name: name.trim(), visibility, kind: 'static', members: included }
    setSaving(true)
    try {
      const saved = await createSubset(cube, dimension.name, def)
      onSaved(saved.name)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not save the subset')
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="editor">
      <h3>New subset of {dimension.name}</h3>
      <div className="field-row">
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Key regions" />
        </label>
        <label>
          Scope
          <select value={visibility} onChange={(e) => setVisibility(e.target.value as Visibility)}>
            <option value="public">Shared</option>
            <option value="private">Only me</option>
          </select>
        </label>
      </div>

      <Tabs
        value={tab}
        onValueChange={(v) => setTab(v as 'pick' | 'mdx')}
        items={[
          { value: 'pick', label: 'Pick members' },
          { value: 'mdx', label: 'Advanced (MDX)' },
        ]}
      >
        <TabPanel value="pick">
          <MemberSetPicker dimension={dimension} value={included} onChange={setIncluded} />
        </TabPanel>
        <TabPanel value="mdx">
          <div className="mdx">
            <textarea
              value={mdx}
              onChange={(e) => setMdx(e.target.value)}
              placeholder="e.g. {[Region].[Total].Children}"
              aria-label="MDX expression"
              rows={3}
            />
            <p className={previewError ? 'error' : 'muted'} role="status" aria-live="polite">
              {previewError
                ? previewError
                : mdx.trim()
                  ? `Resolves to ${preview.length} members`
                  : ''}
            </p>
            <ul className="member-preview">
              {preview.slice(0, 50).map((m) => (
                <li key={m.name}>{m.name}</li>
              ))}
            </ul>
          </div>
        </TabPanel>
      </Tabs>

      {error ? <p className="error" role="alert">{error}</p> : null}
      <div className="actions">
        <button className="primary" disabled={saving} onClick={() => void save()}>
          Save subset
        </button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </div>
  )
}
