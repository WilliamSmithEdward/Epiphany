import { describe, it, expect } from 'vitest'
import {
  allExpandableKeys,
  buildForest,
  computeHeaderSpans,
  flattenForest,
  pathKey,
  subsetVisibleMembers,
} from './tree'
import type { DimensionDto } from '../api/client'

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

describe('flattenForest (drill-path expanded keys, BUG 9)', () => {
  // An alternate-rollup DAG: "South" rolls up to BOTH "Total" and "Coastal", so
  // it appears once under each parent with a distinct drill-path key.
  const dim: DimensionDto = {
    name: 'Region',
    elements: [
      { name: 'Total', kind: 'consolidated' },
      { name: 'Coastal', kind: 'consolidated' },
      { name: 'South', kind: 'consolidated' },
      { name: 'Miami', kind: 'numeric' },
    ],
    edges: [
      { parent: 'Total', child: 'Coastal', weight: 1 },
      { parent: 'Total', child: 'South', weight: 1 },
      { parent: 'Coastal', child: 'South', weight: 1 },
      { parent: 'South', child: 'Miami', weight: 1 },
    ],
  }
  const { roots, childrenOf } = buildForest(dim)

  it('builds the forest with the single root', () => {
    expect(roots).toEqual(['Total'])
  })

  it('gives each occurrence of a multi-parent member a distinct key', () => {
    // Collapsed: only the root "Total" is visible.
    expect(flattenForest(roots, childrenOf, new Set()).map((m) => m.name)).toEqual(['Total'])
    // Expand Total and Coastal so both "South" occurrences surface.
    const totalKey = pathKey('', 'Total')
    const expandTotalCoastal = new Set([totalKey, pathKey(totalKey, 'Coastal')])
    const souths = flattenForest(roots, childrenOf, expandTotalCoastal).filter(
      (m) => m.name === 'South',
    )
    expect(souths.length).toBe(2) // Total/South and Total/Coastal/South
    const keys = souths.map((m) => m.key)
    expect(new Set(keys).size).toBe(2) // the two occurrences have distinct keys
  })

  it('expands one occurrence of a multi-parent member without expanding the other', () => {
    // Open Total and Coastal so both "South" occurrences are visible.
    const totalKey = pathKey('', 'Total')
    const coastalKey = pathKey(totalKey, 'Coastal')
    const southUnderTotal = pathKey(totalKey, 'South')
    const southUnderCoastal = pathKey(coastalKey, 'South')

    // Expand ONLY the South reached via Total (not the one under Coastal).
    const expanded = new Set([totalKey, coastalKey, southUnderTotal])
    const visible = flattenForest(roots, childrenOf, expanded)

    // Miami appears exactly once: under Total/South (expanded), not under
    // Total/Coastal/South (still collapsed). Keying by bare name would expand both.
    const miami = visible.filter((m) => m.name === 'Miami')
    expect(miami).toHaveLength(1)
    expect(miami[0].key).toBe(pathKey(southUnderTotal, 'Miami'))
    expect(miami[0].key).not.toBe(pathKey(southUnderCoastal, 'Miami'))
  })

  it('allExpandableKeys lists every expandable occurrence (per path)', () => {
    const keys = allExpandableKeys(roots, childrenOf)
    const totalKey = pathKey('', 'Total')
    const coastalKey = pathKey(totalKey, 'Coastal')
    expect(keys.has(totalKey)).toBe(true)
    expect(keys.has(coastalKey)).toBe(true)
    expect(keys.has(pathKey(totalKey, 'South'))).toBe(true)
    expect(keys.has(pathKey(coastalKey, 'South'))).toBe(true)
    // Putting them all in the expanded set expands every occurrence: Miami shows
    // up twice (once per South occurrence).
    const visible = flattenForest(roots, childrenOf, keys)
    expect(visible.filter((m) => m.name === 'Miami')).toHaveLength(2)
  })
})
