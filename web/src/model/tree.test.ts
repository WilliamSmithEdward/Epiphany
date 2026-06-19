import { describe, it, expect } from 'vitest'
import { computeHeaderSpans, subsetVisibleMembers } from './tree'

describe('subsetVisibleMembers (saved static subset)', () => {
  it('de-dups a repeated member name so axis keys stay unique', () => {
    // Regression: a subset whose stored member list repeats a name (a hand-edited
    // model TOML or a raw API POST that bypasses the picker's de-dup) must not mint
    // two members with the same key — that gave sibling <tr>/<CellView> the same
    // React key, duplicating rows and stranding cells on that axis.
    const members = subsetVisibleMembers(['North', 'East', 'North'])
    expect(members.map((m) => m.name)).toEqual(['North', 'East']) // first occurrence wins
    const keys = members.map((m) => m.key)
    expect(new Set(keys).size).toBe(keys.length) // every key is unique
  })

  it('leaves a duplicate-free subset unchanged, preserving order', () => {
    expect(subsetVisibleMembers(['North', 'South', 'East']).map((m) => m.name)).toEqual([
      'North',
      'South',
      'East',
    ])
  })

  it('collapses adjacent duplicates and handles an empty list', () => {
    expect(subsetVisibleMembers(['A', 'A']).map((m) => m.name)).toEqual(['A'])
    expect(subsetVisibleMembers([])).toEqual([])
  })

  it('shows subset members flat: depth 0, not expandable', () => {
    expect(subsetVisibleMembers(['A'])[0]).toMatchObject({
      name: 'A',
      key: 'A',
      depth: 0,
      expandable: false,
    })
  })
})

describe('computeHeaderSpans (path-key grouping)', () => {
  const cell = (name: string, key: string) => ({ dimension: 'Region', name, key })

  it('does NOT merge same-named members with different drill-path keys', () => {
    // Diamond rollup: "South" appears adjacently via two paths. The path-keyed body
    // renders two rows; the header must split to match, not over-merge by name.
    const header = computeHeaderSpans([
      [cell('South', 'TotalNorthSouth')],
      [cell('South', 'TotalSouth')],
    ])
    expect(header[0].map((run) => run.span)).toEqual([1, 1])
  })

  it('merges members sharing the same key (same member, same path)', () => {
    const header = computeHeaderSpans([[cell('Q1', 'Q1')], [cell('Q1', 'Q1')]])
    expect(header[0]).toHaveLength(1)
    expect(header[0][0].span).toBe(2)
  })

  it('falls back to grouping by name when no key is supplied (cellset grid)', () => {
    const header = computeHeaderSpans([
      [{ dimension: 'P', name: 'Q1' }],
      [{ dimension: 'P', name: 'Q1' }],
    ])
    expect(header[0][0].span).toBe(2)
  })
})
