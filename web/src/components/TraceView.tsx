import type { TraceDto } from '../api/client'

function kindLabel(node: TraceDto): string {
  switch (node.kind) {
    case 'stored':
      return 'stored value'
    case 'rule':
      return `rule #${node.rule ?? 0}`
    case 'consolidation':
      return `total of ${node.contributions ?? node.inputs.length}`
  }
}

/** Renders a calculation provenance ("explain") tree (ADR-0005). Shared by the
 * Rules workspace and the pivot-grid drill-down. */
export function TraceView({ node }: { node: TraceDto }) {
  return (
    <div className="trace-node">
      <div className="trace-row">
        <span className={`trace-kind ${node.kind}`}>{kindLabel(node)}</span>
        <span className="trace-coord">{node.coord.join(' / ')}</span>
        <span className="trace-value">{node.value}</span>
      </div>
      {node.inputs.length > 0 ? (
        <div className="trace-inputs">
          {node.inputs.map((child, i) => (
            <TraceView key={i} node={child} />
          ))}
        </div>
      ) : null}
    </div>
  )
}
