// Quick-start snippets for the rules and flow editors (W3). Plain data: each
// template has a label, a one-line description, and a body inserted at the end of
// the current editor source. Deliberately small and dependency-free.

export interface Template {
  label: string
  description: string
  body: string
}

export const RULE_TEMPLATES: Template[] = [
  {
    label: 'Sum of two measures',
    description: 'A measure computed from two others (e.g. Margin = Sales - Cost).',
    body: "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];",
  },
  {
    label: 'Ratio (percent)',
    description: 'A percentage of one measure over another.',
    body: "['Measure':'MarginPct'] = value['Measure':'Margin'] / value['Measure':'Sales'] * 100;",
  },
  {
    label: 'Conditional (IF / THEN / ELSE)',
    description: 'Pick a value based on a condition.',
    body:
      "['Measure':'Flag'] = IF value['Measure':'Sales'] > 0 THEN 1 ELSE 0;",
  },
  {
    label: 'Cross-cube reference',
    description: 'Read a fully-addressed cell from another cube.',
    body:
      "['Measure':'Plan'] = 'OtherCube'!['Measure':'Amount', 'Version':'Budget'];",
  },
]

export const FLOW_TEMPLATES: Template[] = [
  {
    label: 'Load cells from CSV',
    description: 'Map each input row to a cell write.',
    body: `function rows(ctx) {
  const data = ctx.input();
  ctx.writeCells(data.map(function (r) {
    return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
  }));
}
`,
  },
  {
    label: 'Add members to a dimension',
    description: 'Create dimension members from the input, then roll them up.',
    body: `function rows(ctx) {
  const data = ctx.input();
  const names = data.map(function (r) { return r.Name; });
  ctx.ensureElements('Region', names);
  names.forEach(function (n) { ctx.addChild('Region', 'Total', n, 1); });
}
`,
  },
  {
    label: 'Conditional assignment',
    description: 'Write a value only when a condition holds.',
    body: `function rows(ctx) {
  const data = ctx.input();
  ctx.writeCells(
    data
      .filter(function (r) { return Number(r.Value) > 0; })
      .map(function (r) {
        return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
      })
  );
}
`,
  },
]

/** Append a template body to existing source (with a blank line between). */
export function appendTemplate(source: string, body: string): string {
  if (source.trim() === '') return body
  return `${source.replace(/\s*$/, '')}\n\n${body}`
}
