import { useState } from 'react'
import type { TreeNode } from '../model/tree'

// A recursive element picker: consolidated nodes expand/collapse; every node has
// a checkbox that toggles membership by element name. Leaves cannot expand.
// Expansion is controlled when `expanded`/`onToggleExpand` are supplied (keyed by
// node path, so a member under two parents expands independently per branch),
// which lets a parent drive expand-all / collapse-all / level controls; otherwise
// each node keeps its own open state.
export default function ElementTree({
  nodes,
  selected,
  onToggle,
  expanded,
  onToggleExpand,
}: {
  nodes: TreeNode[]
  selected: Set<string>
  onToggle: (name: string) => void
  expanded?: Set<string>
  onToggleExpand?: (path: string) => void
}) {
  return (
    <ul className="tree">
      {nodes.map((node) => (
        <TreeItem
          key={node.path}
          node={node}
          selected={selected}
          onToggle={onToggle}
          expanded={expanded}
          onToggleExpand={onToggleExpand}
        />
      ))}
    </ul>
  )
}

function TreeItem({
  node,
  selected,
  onToggle,
  expanded,
  onToggleExpand,
}: {
  node: TreeNode
  selected: Set<string>
  onToggle: (name: string) => void
  expanded?: Set<string>
  onToggleExpand?: (path: string) => void
}) {
  const [localOpen, setLocalOpen] = useState(false)
  const open = expanded ? expanded.has(node.path) : localOpen
  const toggle = () => (expanded ? onToggleExpand?.(node.path) : setLocalOpen((o) => !o))
  const expandable = node.children.length > 0
  return (
    <li>
      <div className="tree-row">
        {expandable ? (
          <button
            type="button"
            className="twisty"
            aria-expanded={open}
            aria-label={open ? 'Collapse' : 'Expand'}
            onClick={toggle}
          >
            {open ? '-' : '+'}
          </button>
        ) : (
          <span className="twisty-spacer" />
        )}
        <label>
          <input
            type="checkbox"
            checked={selected.has(node.name)}
            onChange={() => onToggle(node.name)}
          />
          {node.name}
          {node.kind === 'consolidated' ? <small> (rollup)</small> : null}
        </label>
      </div>
      {expandable && open ? (
        <ul className="tree">
          {node.children.map((child) => (
            <TreeItem
              key={child.path}
              node={child}
              selected={selected}
              onToggle={onToggle}
              expanded={expanded}
              onToggleExpand={onToggleExpand}
            />
          ))}
        </ul>
      ) : null}
    </li>
  )
}
