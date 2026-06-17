import type { TestReportDto } from '../api/client'

// Shared rendering of a rule/flow test report: a pass/fail summary, the per-test
// list, and any assertion failures (coordinate, expected, actual). Used by both
// the Rules and Flows workspaces so the two stay identical.
export function TestReport({ report }: { report: TestReportDto }) {
  return (
    <div className="test-report">
      <p role="status" className={report.all_passed ? 'ok' : 'error'}>
        {report.all_passed
          ? `All ${report.outcomes.length} tests passed`
          : `${report.outcomes.filter((o) => !o.passed).length} of ${report.outcomes.length} failed`}
      </p>
      <ul className="test-list">
        {report.outcomes.map((o) => (
          <li key={o.name} className={o.passed ? 'ok' : 'error'}>
            <span className="test-status">{o.passed ? 'PASS' : 'FAIL'}</span> {o.name}
            {o.failures.length > 0 ? (
              <ul className="failure-list">
                {o.failures.map((f, i) => (
                  <li key={i}>
                    {Object.entries(f.coord)
                      .map(([d, m]) => `${d}:${m}`)
                      .join(' / ')}{' '}
                    expected {f.expected}, got {f.actual}
                  </li>
                ))}
              </ul>
            ) : null}
          </li>
        ))}
      </ul>
    </div>
  )
}
