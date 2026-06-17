import * as DM from '@radix-ui/react-dropdown-menu'
import { useState, type DragEvent, type ReactNode } from 'react'
import type { DimensionDto, SubsetDto } from '../api/client'
import { Select } from '../ui'

/** The drag-over/leave/drop handlers a zone needs as a drop target. */
interface DropHandlers {
  onDragOver: (e: DragEvent) => void
  onDragLeave: () => void
  onDrop: (e: DragEvent) => void
}

// Where a dimension sits in the pivot layout. The editable grid puts exactly one
// dimension on rows and one on columns; every other dimension is a filter pinned
// to a single member (the cube is always fully covered).
export type AxisRole = 'rows' | 'columns' | 'filters'

/** The set applied to a row/column axis: a named saved subset resolved to its
 * member list, or null for every member of the dimension. */
export interface AxisSet {
  name: string
  members: string[]
}

const DT = 'application/x-epiphany-dim'

/**
 * The pivot field tray: three drop zones (Rows, Columns, Filters) holding a chip
 * per dimension. Chips are dragged between zones to re-pivot; each chip also
 * carries a menu with the same moves (keyboard parity) plus, for a row/column
 * dimension, the member set to show (all members, a saved subset, or a new one).
 */
export default function PivotFields({
  dimensions,
  rowDim,
  colDim,
  context,
  subsetsByDim,
  axisSet,
  onPlace,
  onContextMember,
  onPickSet,
  onNewSet,
}: {
  dimensions: DimensionDto[]
  rowDim: string
  colDim: string
  context: Record<string, string>
  subsetsByDim: Record<string, SubsetDto[]>
  axisSet: Record<string, AxisSet | null>
  onPlace: (dim: string, role: AxisRole) => void
  onContextMember: (dim: string, member: string) => void
  onPickSet: (dim: string, subset: SubsetDto | null) => void
  onNewSet: (dim: string) => void
}) {
  const [over, setOver] = useState<AxisRole | null>(null)
  const filterDims = dimensions.filter((d) => d.name !== rowDim && d.name !== colDim)

  const dropProps = (role: AxisRole): DropHandlers => ({
    onDragOver: (e: DragEvent) => {
      if (e.dataTransfer.types.includes(DT)) {
        e.preventDefault()
        setOver(role)
      }
    },
    onDragLeave: () => setOver((o) => (o === role ? null : o)),
    onDrop: (e: DragEvent) => {
      e.preventDefault()
      setOver(null)
      const dim = e.dataTransfer.getData(DT)
      if (dim) onPlace(dim, role)
    },
  })

  return (
    <div className="pivot-fields">
      <Zone role="rows" label="Rows" over={over === 'rows'} dropProps={dropProps('rows')}>
        <AxisChip
          dim={rowDim}
          role="rows"
          set={axisSet[rowDim] ?? null}
          subsets={subsetsByDim[rowDim] ?? []}
          onPlace={onPlace}
          onPickSet={onPickSet}
          onNewSet={onNewSet}
        />
      </Zone>
      <Zone role="columns" label="Columns" over={over === 'columns'} dropProps={dropProps('columns')}>
        <AxisChip
          dim={colDim}
          role="columns"
          set={axisSet[colDim] ?? null}
          subsets={subsetsByDim[colDim] ?? []}
          onPlace={onPlace}
          onPickSet={onPickSet}
          onNewSet={onNewSet}
        />
      </Zone>
      <Zone role="filters" label="Filters" over={over === 'filters'} dropProps={dropProps('filters')}>
        {filterDims.length === 0 ? (
          <span className="pivot-zone__empty">Drag a dimension here to filter</span>
        ) : (
          filterDims.map((d) => (
            <FilterChip
              key={d.name}
              dim={d}
              member={context[d.name] ?? ''}
              onPlace={onPlace}
              onContextMember={onContextMember}
            />
          ))
        )}
      </Zone>
    </div>
  )
}

function Zone({
  role,
  label,
  over,
  dropProps,
  children,
}: {
  role: AxisRole
  label: string
  over: boolean
  dropProps: DropHandlers
  children: ReactNode
}) {
  return (
    <div className={`pivot-zone${over ? ' pivot-zone--over' : ''}`} {...dropProps}>
      <span className="pivot-zone__label" id={`pivot-zone-${role}`}>
        {label}
      </span>
      <div className="pivot-zone__chips" role="group" aria-labelledby={`pivot-zone-${role}`}>
        {children}
      </div>
    </div>
  )
}

/** Shared drag-source wiring for a chip. */
function dragProps(dim: string) {
  return {
    draggable: true,
    onDragStart: (e: DragEvent) => {
      e.dataTransfer.setData(DT, dim)
      e.dataTransfer.effectAllowed = 'move'
    },
  }
}

function AxisChip({
  dim,
  role,
  set,
  subsets,
  onPlace,
  onPickSet,
  onNewSet,
}: {
  dim: string
  role: 'rows' | 'columns'
  set: AxisSet | null
  subsets: SubsetDto[]
  onPlace: (dim: string, role: AxisRole) => void
  onPickSet: (dim: string, subset: SubsetDto | null) => void
  onNewSet: (dim: string) => void
}) {
  const moveTo: AxisRole = role === 'rows' ? 'columns' : 'rows'
  return (
    <div className="pivot-chip" {...dragProps(dim)}>
      <span className="pivot-chip__handle" aria-hidden="true">
        ⠿
      </span>
      <span className="pivot-chip__name">{dim}</span>
      <span className="pivot-chip__set">{set ? set.name : 'All members'}</span>
      <DM.Root>
        <DM.Trigger asChild>
          <button type="button" className="pivot-chip__menu" aria-label={`Options for ${dim}`}>
            ▾
          </button>
        </DM.Trigger>
        <DM.Portal>
          <DM.Content className="menu" align="end" sideOffset={4}>
            <DM.Label className="menu__label">Members</DM.Label>
            <DM.CheckboxItem
              className="menu__item"
              checked={!set}
              onSelect={() => onPickSet(dim, null)}
            >
              All members
            </DM.CheckboxItem>
            {subsets.map((s) => (
              <DM.CheckboxItem
                key={s.name}
                className="menu__item"
                checked={set?.name === s.name}
                onSelect={() => onPickSet(dim, s)}
              >
                {s.name}
              </DM.CheckboxItem>
            ))}
            <DM.Item className="menu__item" onSelect={() => onNewSet(dim)}>
              New set…
            </DM.Item>
            <DM.Separator className="menu__sep" />
            <DM.Label className="menu__label">Move to</DM.Label>
            <DM.Item className="menu__item" onSelect={() => onPlace(dim, moveTo)}>
              {moveTo === 'rows' ? 'Rows' : 'Columns'}
            </DM.Item>
            <DM.Item className="menu__item" onSelect={() => onPlace(dim, 'filters')}>
              Filters
            </DM.Item>
          </DM.Content>
        </DM.Portal>
      </DM.Root>
    </div>
  )
}

function FilterChip({
  dim,
  member,
  onPlace,
  onContextMember,
}: {
  dim: DimensionDto
  member: string
  onPlace: (dim: string, role: AxisRole) => void
  onContextMember: (dim: string, member: string) => void
}) {
  return (
    <div className="pivot-chip pivot-chip--filter" {...dragProps(dim.name)}>
      <span className="pivot-chip__handle" aria-hidden="true">
        ⠿
      </span>
      <span className="pivot-chip__name">{dim.name}</span>
      <Select
        value={member}
        onValueChange={(v) => onContextMember(dim.name, v)}
        options={dim.elements.map((el) => ({ value: el.name, label: el.name }))}
        ariaLabel={`${dim.name} member`}
      />
      <DM.Root>
        <DM.Trigger asChild>
          <button type="button" className="pivot-chip__menu" aria-label={`Move ${dim.name}`}>
            ▾
          </button>
        </DM.Trigger>
        <DM.Portal>
          <DM.Content className="menu" align="end" sideOffset={4}>
            <DM.Label className="menu__label">Move to</DM.Label>
            <DM.Item className="menu__item" onSelect={() => onPlace(dim.name, 'rows')}>
              Rows
            </DM.Item>
            <DM.Item className="menu__item" onSelect={() => onPlace(dim.name, 'columns')}>
              Columns
            </DM.Item>
          </DM.Content>
        </DM.Portal>
      </DM.Root>
    </div>
  )
}
