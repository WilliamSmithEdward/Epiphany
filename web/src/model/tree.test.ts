import { describe, it, expect } from 'vitest'
import {
  allExpandableKeys,
  buildElementTree,
  buildForest,
  computeHeaderSpans,
  flattenForest,
  pathKey,
  subsetVisibleMembers,
  type TreeNode,
} from './tree'
import type { DimensionDto } from '../api/client'

describe('subsetVisibleMembers (saved static subset)', () => {
  it('de-dups a repeated member name so axis keys stay unique', () => {
    // Regression: a subset whose stored member list repeats a name (a hand-edited
    // model TOML or a raw API POST that bypasses the picker's de-dup) must not mint
    // two members with the same key: that gave sibling <tr>/<CellView> the same
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

describe('buildElementTree (element-order sorted children)', () => {
  it('orders each node\'s children by element order, not raw edge order', () => {
    // The edges below list children out of element order (Charlie before Bravo).
    // buildElementTree must sort each parent's children by the dimension's element
    // order so the editor matches buildForest (the pivot's reference) and a global
    // `reorder` reorders siblings consistently in both views.
    const dim: DimensionDto = {
      name: 'Region',
      elements: [
        { name: 'Total', kind: 'consolidated' },
        { name: 'Alpha', kind: 'numeric' },
        { name: 'Bravo', kind: 'numeric' },
        { name: 'Charlie', kind: 'numeric' },
      ],
      edges: [
        { parent: 'Total', child: 'Charlie', weight: 1 },
        { parent: 'Total', child: 'Alpha', weight: 1 },
        { parent: 'Total', child: 'Bravo', weight: 1 },
      ],
    }
    const tree = buildElementTree(dim)
    expect(tree.map((n) => n.name)).toEqual(['Total'])
    expect(tree[0].children.map((n) => n.name)).toEqual(['Alpha', 'Bravo', 'Charlie'])
  })

  it('sorts children at every depth to agree with buildForest', () => {
    const dim: DimensionDto = {
      name: 'Region',
      elements: [
        { name: 'Total', kind: 'consolidated' },
        { name: 'West', kind: 'consolidated' },
        { name: 'East', kind: 'consolidated' },
        { name: 'NY', kind: 'numeric' },
        { name: 'NJ', kind: 'numeric' },
      ],
      edges: [
        { parent: 'Total', child: 'West', weight: 1 },
        { parent: 'Total', child: 'East', weight: 1 },
        // East's children listed reversed vs. element order (NJ before NY).
        { parent: 'East', child: 'NJ', weight: 1 },
        { parent: 'East', child: 'NY', weight: 1 },
      ],
    }
    const tree = buildElementTree(dim)
    const total = tree[0]
    expect(total.children.map((n) => n.name)).toEqual(['West', 'East'])
    const east = total.children.find((n) => n.name === 'East') as TreeNode
    // Element order is NY (index 3) then NJ (index 4), so the sorted children must
    // be [NY, NJ], the same order buildForest produces.
    expect(east.children.map((n) => n.name)).toEqual(['NY', 'NJ'])
    const forest = buildForest(dim)
    expect(forest.childrenOf.get('East')).toEqual(['NY', 'NJ'])
  })

  it('terminates on a cyclic edge set instead of overflowing the stack', () => {
    // A->B->C->A is a cycle (no element is parent-free, so the recursion would
    // never bottom out without the ancestry guard). buildElementTree must mirror
    // flattenForest's per-path ancestry guard and cut the back-edge so it returns.
    const dim: DimensionDto = {
      name: 'Cyclic',
      elements: [
        { name: 'A', kind: 'consolidated' },
        { name: 'B', kind: 'consolidated' },
        { name: 'C', kind: 'consolidated' },
      ],
      edges: [
        { parent: 'A', child: 'B', weight: 1 },
        { parent: 'B', child: 'C', weight: 1 },
        { parent: 'C', child: 'A', weight: 1 }, // back-edge closing the cycle
      ],
    }
    // Every element has an incoming edge, so there is no parent-free root; the
    // tree is empty but the call must still return (not recurse forever).
    const tree = buildElementTree(dim)
    expect(tree).toEqual([])
  })

  it('cuts only the back-edge of a cycle, keeping the reachable prefix when a root exists', () => {
    // Root -> A -> B -> A : "A" reappears on its own path (a cycle via B). The
    // ancestry guard cuts B->A (the back-edge) but keeps Root/A/B, so the
    // non-cyclic prefix still renders.
    const dim: DimensionDto = {
      name: 'CyclicWithRoot',
      elements: [
        { name: 'Root', kind: 'consolidated' },
        { name: 'A', kind: 'consolidated' },
        { name: 'B', kind: 'consolidated' },
      ],
      edges: [
        { parent: 'Root', child: 'A', weight: 1 },
        { parent: 'A', child: 'B', weight: 1 },
        { parent: 'B', child: 'A', weight: 1 }, // back-edge: A already on the path
      ],
    }
    const tree = buildElementTree(dim)
    expect(tree.map((n) => n.name)).toEqual(['Root'])
    const a = tree[0].children[0]
    expect(a.name).toBe('A')
    const b = a.children[0]
    expect(b.name).toBe('B')
    // B's edge back to A is the cycle's back-edge and is cut, so B has no children.
    expect(b.children).toEqual([])
  })

  it('preserves per-occurrence multiplication of a shared rollup (not a cycle)', () => {
    // A diamond (Total -> {West, East}, both -> Shared) is NOT a cycle: "Shared"
    // is reachable by two distinct paths and must still appear once per path. The
    // ancestry guard (keyed by the path, fresh per branch) must not collapse this.
    const dim: DimensionDto = {
      name: 'Diamond',
      elements: [
        { name: 'Total', kind: 'consolidated' },
        { name: 'West', kind: 'consolidated' },
        { name: 'East', kind: 'consolidated' },
        { name: 'Shared', kind: 'numeric' },
      ],
      edges: [
        { parent: 'Total', child: 'West', weight: 1 },
        { parent: 'Total', child: 'East', weight: 1 },
        { parent: 'West', child: 'Shared', weight: 1 },
        { parent: 'East', child: 'Shared', weight: 1 },
      ],
    }
    const tree = buildElementTree(dim)
    const total = tree[0]
    const west = total.children.find((n) => n.name === 'West') as TreeNode
    const east = total.children.find((n) => n.name === 'East') as TreeNode
    // "Shared" appears once under each parent (distinct paths), unchanged.
    expect(west.children.map((n) => n.name)).toEqual(['Shared'])
    expect(east.children.map((n) => n.name)).toEqual(['Shared'])
    expect(west.children[0].path).toBe('Total/West/Shared')
    expect(east.children[0].path).toBe('Total/East/Shared')
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
