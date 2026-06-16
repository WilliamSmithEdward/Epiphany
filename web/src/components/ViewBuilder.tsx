import type { DimensionDto, SubsetDto, Visibility } from '../api/client'
import { Tooltip } from '../ui'

export type Placement = 'rows' | 'columns' | 'context'

/** Per-dimension placement: which axis, and the source (a saved subset name or
 * the `__all__` sentinel for every member) or the fixed context member. */
export interface DimConfig {
  placement: Placement
  source: string
  contextMember: string
}

export const ALL_MEMBERS = '__all__'

// A point-and-click view builder: each dimension is placed on Rows, Columns, or
// Context; rows/columns choose a subset (or all members), context picks a member.
// Every dimension is always placed, so the cube is fully covered.
export default function ViewBuilder({
  dimensions,
  subsetsByDim,
  config,
  onConfigChange,
  suppress,
  onSuppressChange,
  name,
  onNameChange,
  visibility,
  onVisibilityChange,
  onRun,
  onSave,
  onNewSubset,
  busy,
}: {
  dimensions: DimensionDto[]
  subsetsByDim: Record<string, SubsetDto[]>
  config: Record<string, DimConfig>
  onConfigChange: (dim: string, partial: Partial<DimConfig>) => void
  suppress: boolean
  onSuppressChange: (value: boolean) => void
  name: string
  onNameChange: (value: string) => void
  visibility: Visibility
  onVisibilityChange: (value: Visibility) => void
  onRun: () => void
  onSave: () => void
  onNewSubset: (dim: string) => void
  busy: boolean
}) {
  return (
    <div className="builder">
      <table className="placements">
        <thead>
          <tr>
            <th>Dimension</th>
            <th>Place on</th>
            <th>Members</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {dimensions.map((dim) => {
            const cfg = config[dim.name]
            if (!cfg) return null
            const subsets = subsetsByDim[dim.name] ?? []
            return (
              <tr key={dim.name}>
                <td>{dim.name}</td>
                <td>
                  <select
                    value={cfg.placement}
                    onChange={(e) =>
                      onConfigChange(dim.name, { placement: e.target.value as Placement })
                    }
                  >
                    <option value="rows">Rows</option>
                    <option value="columns">Columns</option>
                    <option value="context">Context</option>
                  </select>
                </td>
                <td>
                  {cfg.placement === 'context' ? (
                    <select
                      value={cfg.contextMember}
                      onChange={(e) => onConfigChange(dim.name, { contextMember: e.target.value })}
                    >
                      {dim.elements.map((el) => (
                        <option key={el.name} value={el.name}>
                          {el.name}
                        </option>
                      ))}
                    </select>
                  ) : (
                    <select
                      value={cfg.source}
                      onChange={(e) => onConfigChange(dim.name, { source: e.target.value })}
                    >
                      <option value={ALL_MEMBERS}>All members</option>
                      {subsets.map((s) => (
                        <option key={s.name} value={s.name}>
                          {s.name}
                        </option>
                      ))}
                    </select>
                  )}
                </td>
                <td>
                  <button onClick={() => onNewSubset(dim.name)}>New subset</button>
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>

      <div className="field-row">
        <label className="check">
          <input
            type="checkbox"
            checked={suppress}
            onChange={(e) => onSuppressChange(e.target.checked)}
          />
          <Tooltip content="Hide rows and columns that are entirely blank or zero, so only meaningful numbers show.">
            <span>Suppress zeros</span>
          </Tooltip>
        </label>
        <label>
          Name
          <input value={name} onChange={(e) => onNameChange(e.target.value)} placeholder="Save as..." />
        </label>
        <label>
          Scope
          <select value={visibility} onChange={(e) => onVisibilityChange(e.target.value as Visibility)}>
            <option value="public">Shared</option>
            <option value="private">Only me</option>
          </select>
        </label>
      </div>

      <div className="actions">
        <button className="primary" disabled={busy} onClick={onRun}>
          Run
        </button>
        <button disabled={busy} onClick={onSave}>
          Save view
        </button>
      </div>
    </div>
  )
}
