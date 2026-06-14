# Architecture Decision Records

Significant decisions are recorded here (RG-16), one numbered file each. Use [`template.md`](template.md) for new ADRs. Supersede an earlier decision rather than silently contradicting it.

| ADR | Title | Status | Phase |
|---|---|---|---|
| [0001](0001-concurrency-model.md) | Concurrency model | Proposed | 0/1 |
| [0002](0002-runtime-persistence-format.md) | Runtime persistence format | Proposed | 0/1 |
| [0003](0003-model-as-code-serialization.md) | Model-as-code serialization format | Proposed | 1 |
| [0004](0004-embedded-javascript-engine.md) | Embedded JavaScript engine and TypeScript flows | Accepted | 5 |
| [0005](0005-automatic-feeder-inference.md) | Automatic feeder inference, validation, and under/over-feed detection | Accepted | 4 |
| [0006](0006-cell-storage-and-memory-layout.md) | Cell storage and memory layout | Accepted | 1 |
| [0007](0007-rule-evaluation-strategy.md) | Rule evaluation strategy (compiled rules, on-demand eval, memoization) | Accepted | 4 |
| [0008](0008-numeric-model-and-precision.md) | Numeric model and precision | Proposed | 1 |
| [0009](0009-determinism-strategy.md) | Determinism strategy | Accepted | 0 |
| [0010](0010-audit-logging.md) | Audit / user-action logging | Proposed | 7/8 |
| [0011](0011-mdx-seam-and-execution.md) | MDX evaluator seam, execute-time resolution, and zero-suppression | Accepted | 3 |
| [0012](0012-data-source-connectors.md) | Data-source connectors (command, HTTP) and the fetch/transform split | Accepted | 5+ |

> All originally-reserved ADR numbers are allocated; later decisions continue from 0012.
