import { useState } from 'react'
import { writeCell, type CellsetDto, type Coord } from '../api/client'
import { computeHeaderSpans } from '../model/tree'

// Render an executed cellset: nested column headers (colSpan) and row headers
// (rowSpan) via computeHeaderSpans, with editable leaf cells writing back through
// the existing single-cell write. The server's `editable` flag is trusted, never
// inferred, so consolidated cells stay read-only.
export default function CellsetGrid({
  cube,
  cellset,
  onChanged,
}: {
  cube: string
  cellset: CellsetDto
  onChanged: () => void
}) {
  const [error, setError] = useState<string | null>(null)
  const rowDims = cellset.row_dimensions.length
  const colLevels = cellset.column_dimensions.length
  const ncols = Math.max(1, cellset.column_tuples.length)
  const cornerCols = Math.max(1, rowDims)

  const colHeader = computeHeaderSpans(cellset.column_tuples)

  // For each body row, the row-header cells that start a run at that row.
  const rowHeaderAt: { name: string; rowSpan: number }[][] = cellset.row_tuples.map(() => [])
  for (let level = 0; level < rowDims; level++) {
    let r = 0
    for (const run of computeHeaderSpans(cellset.row_tuples)[level] ?? []) {
      rowHeaderAt[r].push({ name: run.name, rowSpan: run.span })
      r += run.span
    }
  }

  function coordFor(r: number, c: number): Coord {
    const coord: Coord = {}
    for (const m of cellset.row_tuples[r] ?? []) coord[m.dimension] = m.name
    for (const m of cellset.column_tuples[c] ?? []) coord[m.dimension] = m.name
    for (const ctx of cellset.context) coord[ctx.dimension] = ctx.member
    return coord
  }

  function cellLabel(r: number, c: number): string {
    const rowName = (cellset.row_tuples[r] ?? []).map((m) => m.name).join(' / ')
    const colName = (cellset.column_tuples[c] ?? []).map((m) => m.name).join(' / ')
    return [rowName, colName].filter(Boolean).join(' × ') || 'Value'
  }

  // A stable, collision-free key for a tuple: its member names joined with a
  // control character that cannot appear in an element name (matching the
  // pivot's tupleKey convention). Used to key rows/cells by identity so a live
  // refetch that reorders tuples remounts the affected inputs (discarding a
  // stranded in-progress edit) instead of silently preserving a value-keyed DOM
  // input over a now-different coordinate.
  const tupleKey = (tuple: { name: string }[]): string => tuple.map((m) => m.name).join('')

  // Commit against the coordinate CAPTURED at render (bound into the cell's
  // closure), not one re-resolved from (r,c) against whatever cellset is current
  // at blur time - so a refetch between focus and blur cannot redirect the write
  // to a different cell.
  async function commit(coord: Coord, previous: string, next: string) {
    if (next === previous) return
    try {
      setError(null)
      await writeCell(cube, coord, next)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the cell')
    } finally {
      onChanged()
    }
  }

  return (
    <div className="grid-wrap">
      {error ? <p className="error" role="alert">{error}</p> : null}
      <table className="pivot cellset">
        <caption className="sr-only">{`Cells for ${cube}`}</caption>
        <thead>
          {colLevels === 0 ? (
            <tr>
              <th className="corner" colSpan={cornerCols} />
              <th scope="col">Value</th>
            </tr>
          ) : (
            colHeader.map((row, level) => (
              <tr key={level}>
                {level === 0 ? (
                  <th className="corner" colSpan={cornerCols} rowSpan={colLevels} />
                ) : null}
                {row.map((span, i) => (
                  <th key={i} scope="col" colSpan={span.span}>
                    {span.name}
                  </th>
                ))}
              </tr>
            ))
          )}
        </thead>
        <tbody>
          {cellset.row_tuples.map((rowTuple, r) => {
            const rowKey = tupleKey(rowTuple)
            return (
            <tr key={rowKey}>
              {rowHeaderAt[r].map((h, i) => (
                <th key={i} scope="row" className="rowhead" rowSpan={h.rowSpan}>
                  {h.name}
                </th>
              ))}
              {Array.from({ length: ncols }, (_, c) => {
                const colKey = tupleKey(cellset.column_tuples[c] ?? [])
                const cell = cellset.cells[r * ncols + c]
                if (!cell) return <td key={colKey} className="cell" />
                if (!cell.editable) {
                  return (
                    <td
                      key={colKey}
                      className={cell.overlaid ? 'cell consolidated overlaid' : 'cell consolidated'}
                    >
                      {cell.value ?? ''}
                    </td>
                  )
                }
                // Capture this cell's coordinate now, at render, so a live
                // refetch between focus and blur cannot redirect the write.
                const coord = coordFor(r, c)
                const previous = cell.value ?? ''
                return (
                  <td
                    key={colKey}
                    className={cell.overlaid ? 'cell overlaid' : 'cell'}
                    title={cell.overlaid ? 'Uncommitted what-if value' : undefined}
                  >
                    <input
                      // Key by tuple identity + value so a remote reorder or value
                      // change remounts the input (rather than preserving a
                      // value-keyed DOM node over a now-different coordinate).
                      key={`${rowKey}||${colKey}||${previous}`}
                      aria-label={cellLabel(r, c)}
                      defaultValue={previous}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') e.currentTarget.blur()
                      }}
                      onBlur={(e) => void commit(coord, previous, e.currentTarget.value.trim())}
                    />
                  </td>
                )
              })}
            </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
