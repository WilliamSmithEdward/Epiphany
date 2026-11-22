// The layout of an exploration: which dimensions stand on rows, on columns and as filters,
// with the members chosen and opened, and the MDX query it asks.

/** Names as MDX writes them, in brackets. */
export const quote = (name) => `[${String(name).replaceAll(']', ']]')}]`;
export const member = (dimension, name) => `${quote(dimension)}.${quote(name)}`;

/* ---- The layout ---- */

/**
 * A layout is `{ cube, rows, columns, filters, nonEmpty, sort, mdx }`. Each entry of rows
 * and columns is `{ dimension, members, expanded }`, the members chosen in order and those
 * opened to show their children; each filter is `{ dimension, member }`. `mdx` is set when
 * the query has been written by hand.
 */
export function defaultLayout(cube, dims) {
  const time = cube.dimensions.find((name) => dims.get(name).type === 'time');
  const rows = cube.dimensions[0];
  const columns = time && time !== rows ? time : cube.dimensions[1];
  const axis = (name) => name ? [{ dimension: name, members: [...dims.get(name).roots], expanded: [] }] : [];
  return {
    cube: cube.name,
    rows: axis(rows),
    columns: axis(columns),
    filters: cube.dimensions.filter((name) => name !== rows && name !== columns)
      .map((name) => ({ dimension: name, member: dims.get(name).roots[0] })),
    nonEmpty: false,
    sort: null,
    mdx: null,
  };
}

/**
 * The members an axis entry shows: each chosen member, and its children where it is open;
 * the children after it, or before it where the entry puts totals last.
 */
export function shown(entry, dim) {
  const open = new Set(entry.expanded);
  const out = [];
  const visit = (name, depth) => {
    const after = entry.totalsLast && open.has(name);
    if (!after) out.push({ name, depth });
    if (open.has(name)) {
      for (const child of dim.byName.get(name)?.children ?? []) visit(child.name, depth + 1);
    }
    if (after) out.push({ name, depth });
  };
  for (const name of entry.members) visit(name, 0);
  return out;
}

/** The MDX a layout asks. */
export function mdx(layout, dims) {
  if (layout.mdx) return layout.mdx;
  const set = (entries) => entries
    .map((entry) => `{${shown(entry, dims.get(entry.dimension)).map(({ name }) => member(entry.dimension, name)).join(', ')}}`)
    .join(' * ');
  const nonEmpty = layout.nonEmpty ? 'NON EMPTY ' : '';
  let rows = set(layout.rows);
  if (layout.sort && rows) {
    const column = layout.sort.tuple.map(([dim, name]) => member(dim, name)).join(', ');
    rows = `ORDER(${rows}, (${column}), ${layout.sort.descending ? 'BDESC' : 'BASC'})`;
  }
  const axes = [];
  if (layout.columns.length) axes.push(`${nonEmpty}${set(layout.columns)} ON COLUMNS`);
  if (layout.rows.length) axes.push(`${nonEmpty}${rows} ON ROWS`);
  const slicer = layout.filters.map((filter) => member(filter.dimension, filter.member));
  const where = slicer.length ? `\nWHERE (${slicer.join(', ')})` : '';
  return `SELECT\n  ${axes.join(',\n  ')}\nFROM ${quote(layout.cube)}${where}`;
}

/** Where the last layout of a cube is kept, in this browser. */
export const saved = (cube) => `epiphany.explore.${cube}`;
