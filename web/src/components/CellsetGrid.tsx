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

  async function commit(r: number, c: number, previous: string, next: string) {
    if (next === previous) return
    try {
      setError(null)
      await writeCell(cube, coordFor(r, c), next)
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
          {cellset.row_tuples.map((_, r) => (
            <tr key={r}>
              {rowHeaderAt[r].map((h, i) => (
                <th key={i} scope="row" className="rowhead" rowSpan={h.rowSpan}>
                  {h.name}
                </th>
              ))}
              {Array.from({ length: ncols }, (_, c) => {
                const cell = cellset.cells[r * ncols + c]
                if (!cell) return <td key={c} className="cell" />
                if (!cell.editable) {
                  return (
                    <td
                      key={c}
                      className={cell.overlaid ? 'cell consolidated overlaid' : 'cell consolidated'}
                    >
                      {cell.value ?? ''}
                    </td>
                  )
                }
                return (
                  <td
                    key={c}
                    className={cell.overlaid ? 'cell overlaid' : 'cell'}
                    title={cell.overlaid ? 'Uncommitted what-if value' : undefined}
                  >
                    <input
                      key={cell.value ?? ''}
                      aria-label={cellLabel(r, c)}
                      defaultValue={cell.value ?? ''}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') e.currentTarget.blur()
                      }}
                      onBlur={(e) =>
                        void commit(r, c, cell.value ?? '', e.currentTarget.value.trim())
                      }
                    />
                  </td>
                )
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
