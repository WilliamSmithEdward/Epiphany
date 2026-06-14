import { useState } from 'react'
import type { TreeNode } from '../model/tree'

// A recursive element picker: consolidated nodes expand/collapse; every node has
// a checkbox that toggles membership by element name. Leaves cannot expand.
export default function ElementTree({
  nodes,
  selected,
  onToggle,
}: {
  nodes: TreeNode[]
  selected: Set<string>
  onToggle: (name: string) => void
}) {
  return (
    <ul className="tree">
      {nodes.map((node) => (
        <TreeItem key={node.path} node={node} selected={selected} onToggle={onToggle} />
      ))}
    </ul>
  )
}

function TreeItem({
  node,
  selected,
  onToggle,
}: {
  node: TreeNode
  selected: Set<string>
  onToggle: (name: string) => void
}) {
  const [open, setOpen] = useState(false)
  const expandable = node.children.length > 0
  return (
    <li>
      <div className="tree-row">
        {expandable ? (
          <button
            type="button"
            className="twisty"
            aria-label={open ? 'Collapse' : 'Expand'}
            onClick={() => setOpen((o) => !o)}
          >
            {open ? '−' : '+'}
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
            <TreeItem key={child.path} node={child} selected={selected} onToggle={onToggle} />
          ))}
        </ul>
      ) : null}
    </li>
  )
}
