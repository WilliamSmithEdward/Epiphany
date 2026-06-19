import { describe, it, expect } from 'vitest'
import { elementTreeNodes, selectionId, type NodeMenuItem } from './modelExplorerTree'
import type { TreeNode } from '../model/tree'

describe('selectionId', () => {
  it('encodes each selection kind to a stable, distinct id', () => {
    expect(selectionId({ kind: 'cube', cube: 'Sales' })).toBe('cube:Sales')
    expect(selectionId({ kind: 'cube-dimension', cube: 'Sales', dim: 'Region' })).toBe(
      'cube:Sales/dim:Region',
    )
    expect(selectionId({ kind: 'cube-views', cube: 'Sales' })).toBe('cube:Sales/views')
    expect(selectionId({ kind: 'view', cube: 'Sales', view: 'Q1' })).toBe('cube:Sales/views/Q1')
    expect(selectionId({ kind: 'cube-rules', cube: 'Sales' })).toBe('cube:Sales/rules')
    expect(selectionId({ kind: 'dimension', id: 7, name: 'Region' })).toBe('dim:7')
    expect(selectionId({ kind: 'flow', flow: 'Load' })).toBe('flow:Load')
    expect(selectionId({ kind: 'schedule', schedule: 'Nightly' })).toBe('sched:Nightly')
    expect(selectionId({ kind: 'connection', connection: 'Pg' })).toBe('conn:Pg')
    expect(selectionId({ kind: 'overview' })).toBe('overview')
    expect(selectionId({ kind: 'security' })).toBe('security')
  })

  it('keeps the cube-dimension id independent of the global registry dimension id', () => {
    // A cube dimension and a registry dimension of the same name must not collide:
    // their ids are namespaced differently so the two tree rows stay distinct.
    expect(selectionId({ kind: 'cube-dimension', cube: 'Sales', dim: 'Region' })).not.toBe(
      selectionId({ kind: 'dimension', id: 1, name: 'Region' }),
    )
  })
})

describe('elementTreeNodes', () => {
  // A small consolidation hierarchy: Total -> {North (leaf), South (leaf)}.
  const tree: TreeNode[] = [
    {
      name: 'Total',
      kind: 'consolidated',
      path: 'Total',
      children: [
        { name: 'North', kind: 'numeric', path: 'Total/North', children: [] },
        { name: 'South', kind: 'string', path: 'Total/South', children: [] },
      ],
    },
  ]

  it('path-prefixes ids by parent so an occurrence stays unique', () => {
    const nodes = elementTreeNodes('dim:7', tree)
    expect(nodes).toHaveLength(1)
    expect(nodes[0].id).toBe('dim:7/el:Total')
  })

  it('makes a member with children expandable (loader) and a leaf not', async () => {
    const [total] = elementTreeNodes('dim:7', tree)
    expect(typeof total.loader).toBe('function')
    const kids = await total.loader!()
    expect(kids.map((k) => k.id)).toEqual(['dim:7/el:Total/el:North', 'dim:7/el:Total/el:South'])
    // A leaf carries no loader, so the tree shows no expand affordance for it.
    expect(kids[0].loader).toBeUndefined()
  })

  it('maps element kind to the icon glyph (consolidated / string / numeric)', async () => {
    const [total] = elementTreeNodes('dim:7', tree)
    expect(total.icon).toBe('◇') // consolidated
    const [north, south] = await total.loader!()
    expect(north.icon).toBe('·') // numeric
    expect(south.icon).toBe('"') // string
  })

  it('propagates the shared menu and action context down every level', async () => {
    const menu: NodeMenuItem[] = [{ action: 'add-member', label: 'Add member…' }]
    const ctx = { dimId: 7, dim: 'Region' }
    const [total] = elementTreeNodes('dim:7', tree, menu, ctx)
    expect(total.menu).toBe(menu)
    expect(total.actionCtx).toBe(ctx)
    const [north] = await total.loader!()
    expect(north.menu).toBe(menu)
    expect(north.actionCtx).toBe(ctx)
  })
})
