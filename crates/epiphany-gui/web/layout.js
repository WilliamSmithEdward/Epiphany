// The layout of an exploration: which dimensions stand on rows, on columns and as filters,
// with the members chosen and opened, and the MDX query it asks.

/** Names as MDX writes them, in brackets. */
export const quote = (name) => `[${String(name).replaceAll(']', ']]')}]`;
export const member = (dimension, name) => `${quote(dimension)}.${quote(name)}`;

/* ---- The layout ---- */

/**
 * A layout is `{ cube, rows, columns, filters, nonEmpty, sort, mdx }`. Each entry of rows
 * and columns is `{ dimension, members, expanded }`, the members chosen in order and those
 * opened to show their children; each filter is `{ dimension, member }`. `nonEmpty` says,
 * for `rows` and for `columns`, whether those with no values are left out. `top`, where set,
 * keeps only the `count` largest rows, or smallest with `bottom`, by the column `tuple`.
 * `calculations` add
 * members worked out from others, as `CALCULATIONS` says. `mdx` is set when the query has
 * been written by hand.
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
    nonEmpty: { rows: false, columns: false },
    sort: null,
    mdx: null,
  };
}

/**
 * A query's clauses as read at its top level, outside brackets, braces, parentheses and
 * quotes, so that names and sets may hold commas and words of their own: the axes its
 * SELECT lists, and what its FROM names, as written, up to any WHERE.
 */
function clauses(text) {
  const source = String(text ?? '');
  const letters = /[A-Za-z]+/y;
  let depth = 0;
  let quote = null;
  let start = -1;
  let from = -1;
  const axes = [];
  for (let at = 0; at < source.length; at += 1) {
    const char = source[at];
    if (quote) {
      if (char === quote && source[at + 1] === quote) at += 1;
      else if (char === quote) quote = null;
      continue;
    }
    if (char === '[') quote = ']';
    else if (char === '"' || char === "'") quote = char;
    else if ('({'.includes(char)) depth += 1;
    else if (')}'.includes(char)) depth -= 1;
    else if (depth === 0) {
      letters.lastIndex = at;
      const word = letters.exec(source)?.[0]?.toUpperCase();
      const boundary = !/[A-Za-z0-9_]/.test(source[at - 1] ?? '');
      if (boundary && word === 'SELECT' && start < 0) start = at + word.length;
      else if (boundary && word === 'FROM' && start >= 0 && from < 0) {
        axes.push(source.slice(start, at));
        from = at + word.length;
      } else if (boundary && word === 'WHERE' && from >= 0) {
        return { axes, from: source.slice(from, at) };
      } else if (char === ',' && start >= 0 && from < 0) {
        axes.push(source.slice(start, at));
        start = at + 1;
      }
      if (word && boundary) at += word.length - 1;
    }
  }
  return { axes, from: from >= 0 ? source.slice(from) : '' };
}

/** The cube a query asks of, as its FROM names it; null where it names none plainly. */
export function cubeOf(text) {
  const from = clauses(text).from.trim();
  const bracketed = /^\[((?:[^\]]|\]\])*)\]\s*$/.exec(from);
  if (bracketed) return bracketed[1].replaceAll(']]', ']');
  return /^[A-Za-z_]\w*$/.exec(from)?.[0] ?? null;
}

/** Which axes of a query leave out what has no values: those its SELECT puts NON EMPTY before. */
export function emptiness(text) {
  const found = { rows: false, columns: false };
  for (const axis of clauses(text).axes) {
    const place = /\bON\s+(COLUMNS|ROWS|0|1|AXIS\s*\(\s*([01])\s*\))\s*$/i.exec(axis);
    if (!place) continue;
    const name = place[1].toUpperCase();
    const which = name === 'COLUMNS' || name === '0' || place[2] === '0' ? 'columns' : 'rows';
    found[which] = /^\s*NON\s+EMPTY\b/i.test(axis);
  }
  return found;
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

/**
 * The calculations a layout can add beside a dimension's members, each `{ dimension, name,
 * kind, a, b }`: what they are called when offered, and the MDX that works them out.
 */
export const CALCULATIONS = {
  difference: ['The difference from another member', (x, y) => `${x} - ${y}`],
  ratio: ['Its ratio to another member', (x, y) => `${x} / ${y}`],
  change: ['Its change from another member, in %', (x, y) => `100 * (${x} - ${y}) / ${y}`],
  share: ['Its share of its parent, in %', (x) => `100 * ${x} / ${x}.Parent`],
};

/** The MDX expression of a calculation over its dimension's members. */
export function calculation({ dimension, kind, a, b }) {
  return CALCULATIONS[kind][1](member(dimension, a), b === undefined ? null : member(dimension, b));
}

/** The MDX a layout asks. */
export function mdx(layout, dims) {
  if (layout.mdx) return layout.mdx;
  // Calculations stand after the members of their dimension, where it is on an axis.
  const placed = new Set([...layout.rows, ...layout.columns].map((entry) => entry.dimension));
  const calculations = (layout.calculations ?? []).filter((each) => placed.has(each.dimension));
  const set = (entries) => entries
    .map((entry) => `{${[
      ...shown(entry, dims.get(entry.dimension)).map(({ name }) => member(entry.dimension, name)),
      ...calculations.filter((each) => each.dimension === entry.dimension).map((each) => member(each.dimension, each.name)),
    ].join(', ')}}`)
    .join(' * ');
  const nonEmpty = (axis) => (layout.nonEmpty?.[axis] ? 'NON EMPTY ' : '');
  let rows = set(layout.rows);
  // Only the largest or smallest rows by a column, where the layout keeps some.
  if (layout.top && rows) {
    const column = layout.top.tuple.map(([dim, name]) => member(dim, name)).join(', ');
    rows = `${layout.top.bottom ? 'BOTTOMCOUNT' : 'TOPCOUNT'}(${rows}, ${layout.top.count}, (${column}))`;
  }
  if (layout.sort && rows) {
    const column = layout.sort.tuple.map(([dim, name]) => member(dim, name)).join(', ');
    rows = `ORDER(${rows}, (${column}), ${layout.sort.descending ? 'BDESC' : 'BASC'})`;
  }
  const axes = [];
  if (layout.columns.length) axes.push(`${nonEmpty('columns')}${set(layout.columns)} ON COLUMNS`);
  if (layout.rows.length) axes.push(`${nonEmpty('rows')}${rows} ON ROWS`);
  const slicer = layout.filters.map((filter) => member(filter.dimension, filter.member));
  const where = slicer.length ? `\nWHERE (${slicer.join(', ')})` : '';
  const members = calculations.map((each) => `MEMBER ${member(each.dimension, each.name)} AS ${calculation(each)}`);
  const head = members.length ? `WITH\n  ${members.join('\n  ')}\n` : '';
  return `${head}SELECT\n  ${axes.join(',\n  ')}\nFROM ${quote(layout.cube)}${where}`;
}

/** Where the last layout of a cube is kept, in this browser. */
export const saved = (cube) => `epiphany.explore.${cube}`;
