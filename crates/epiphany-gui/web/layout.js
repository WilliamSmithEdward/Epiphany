// The layout of an exploration: which dimensions stand on rows, on columns and as filters,
// with the members chosen and opened, and the MDX query it asks.

/** Names as MDX writes them, in brackets. */
export const quote = (name) => `[${String(name).replaceAll(']', ']]')}]`;
export const member = (dimension, name) => `${quote(dimension)}.${quote(name)}`;

/* ---- The layout ---- */

/**
 * A layout is `{ cube, rows, columns, filters, nonEmpty, sort, mdx }`. Each entry of rows
 * and columns is `{ dimension, members, expanded }`, the members chosen in order and the
 * paths of those opened to show their children; each filter is `{ dimension, member }`. A
 * path is the names from a member chosen down to the one opened, as `pathKey` joins them, so
 * that a member shown in two places is opened in one alone; a member chosen is its own path,
 * as the name alone that a layout of before kept. `nonEmpty` says,
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

/** What joins a path's names: a character no name holds. */
const JOIN = '\u001f';
/** A path as an entry's `expanded` keeps it, from its names. */
export const pathKey = (names) => names.join(JOIN);
/** A path's names, from the member chosen down. */
export const namesOf = (path) => String(path).split(JOIN);

/**
 * What a layout shows of each dimension, for a page holding one in parts to fetch before
 * drawing it: by dimension, the members it names, chosen, opened on the way, standing as
 * filters or calculated with, and those whose children it shows, as `fill` takes them.
 */
export function wanted(layout) {
  const asks = new Map();
  const of = (dimension) => {
    if (!asks.has(dimension)) asks.set(dimension, { names: [], childrenOf: [] });
    return asks.get(dimension);
  };
  for (const entry of [...(layout?.rows ?? []), ...(layout?.columns ?? [])]) {
    const ask = of(entry.dimension);
    ask.names.push(...entry.members);
    for (const path of entry.expanded ?? []) {
      const names = namesOf(path);
      ask.names.push(...names);
      ask.childrenOf.push(names.at(-1));
    }
  }
  for (const filter of layout?.filters ?? []) of(filter.dimension).names.push(filter.member);
  for (const each of layout?.calculations ?? []) of(each.dimension).names.push(...[each.a, each.b].filter((name) => name !== undefined));
  return asks;
}

/**
 * A dimension's members, in the order a query gave them, read as an outline where they are
 * one: members chosen, each followed by the whole of its children, in order, where it is
 * open, as Descendants gives them. `{ members, expanded }`, which `shown` gives back in the
 * same order; null where nothing in them opens, or where a member is followed by some of
 * its children and not all.
 */
export function folded(names, dim) {
  let at = 0;
  const expanded = [];
  // The members after the one at the end of `path`: its children, if they follow it.
  const read = (path) => {
    const children = dim.byName.get(path.at(-1))?.children ?? [];
    if (!children.length || names[at] !== children[0].name) return true;
    for (const child of children) {
      if (names[at] !== child.name) return false;
      at += 1;
      if (!read([...path, child.name])) return false;
    }
    expanded.push(pathKey(path));
    return true;
  };
  const members = [];
  while (at < names.length) {
    members.push(names[at]);
    at += 1;
    if (!read([members.at(-1)])) return null;
  }
  return expanded.length ? { members, expanded } : null;
}

/**
 * The members an axis entry shows, as `{ name, depth, path, opened }`: each chosen member,
 * and its children where it is open, after it, or before it where the entry puts totals last.
 */
export function shown(entry, dim) {
  const open = new Set(entry.expanded);
  const out = [];
  const visit = (names) => {
    const path = pathKey(names);
    const row = { name: names.at(-1), depth: names.length - 1, path, opened: open.has(path) };
    const after = entry.totalsLast && row.opened;
    if (!after) out.push(row);
    if (row.opened) {
      for (const child of dim.byName.get(row.name)?.children ?? []) visit([...names, child.name]);
    }
    if (after) out.push(row);
  };
  for (const name of entry.members) visit([name]);
  return out;
}

/**
 * The paths of the members that have children at or beneath each path of `starts` (given as
 * names), that `keep` keeps: those to open to show everything there, or down to a level.
 */
export function pathsBeneath(dim, starts, keep = () => true) {
  const out = [];
  const visit = (names) => {
    const element = dim.byName.get(names.at(-1));
    if (!element?.children.length) return;
    if (keep(element)) out.push(pathKey(names));
    for (const child of element.children) visit([...names, child.name]);
  };
  for (const names of starts) visit(names);
  return out;
}

/**
 * Where each tuple of an axis of a query's answer stands in the outline its layout opens: for
 * each of its members, its row of `shown`, or null for one the layout does not show, such as a
 * calculation. The answer keeps the outline's order, leaving out what is empty, so each member
 * is the next of its name, or the same as the row before; and a member past the outer one
 * before it starts its own outline again. One found nowhere after, as in rows put in order by
 * their values, is taken where its name stands first.
 */
export function outlined(entries, dims, tuples) {
  const outlines = (tuples[0] ?? []).map((member) => {
    const entry = entries.find((each) => each.dimension === member.dimension);
    const dim = dims.get(member.dimension);
    return entry && dim ? shown(entry, dim) : null;
  });
  const at = outlines.map(() => -1);
  let before = null;
  return tuples.map((tuple) => {
    let moved = false;
    const found = tuple.map((member, level) => {
      const outline = outlines[level];
      if (!outline) return null;
      if (moved) at[level] = -1;
      else if (at[level] >= 0 && before?.[level]?.name === member.name) return outline[at[level]];
      let next = at[level] + 1;
      while (next < outline.length && outline[next].name !== member.name) next += 1;
      moved = true;
      if (next >= outline.length) return outline.find((row) => row.name === member.name) ?? null;
      at[level] = next;
      return outline[next];
    });
    before = tuple;
    return found;
  });
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

/**
 * A link to a cube's page that opens `view` there, its query and how it is shown, for anyone
 * who may read the cube: the view travels in the address, as base64url of its JSON.
 */
export function linkOf(cube, view) {
  let binary = '';
  for (const byte of new TextEncoder().encode(JSON.stringify(view))) binary += String.fromCharCode(byte);
  const shared = btoa(binary).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
  return `${location.origin}/explore?cube=${encodeURIComponent(cube)}&shared=${shared}`;
}

/**
 * The view a link carries, kept to what a view may say, as the server holds views to it;
 * null for a link damaged or made up.
 */
export function linkedView(shared) {
  let read;
  try {
    const binary = atob(shared.replaceAll('-', '+').replaceAll('_', '/'));
    read = JSON.parse(new TextDecoder().decode(Uint8Array.from(binary, (letter) => letter.charCodeAt(0))));
  } catch {
    return null;
  }
  if (typeof read?.mdx !== 'string') return null;
  const whole = (value, most) => (Number.isInteger(value) && value >= 0 && value <= most ? value : undefined);
  const among = (value, ...allowed) => (allowed.includes(value) ? value : undefined);
  const { format, highlights, geometry } = read;
  return {
    mdx: read.mdx,
    format: format && {
      decimals: whole(format.decimals, 20),
      scale: among(format.scale, 'thousands', 'millions', 'billions'),
      parentheses: format.parentheses === true,
      dash_zeros: format.dash_zeros === true,
    },
    highlights: highlights && {
      bars: highlights.bars === true,
      shades: highlights.shades === true,
      rules: (Array.isArray(highlights.rules) ? highlights.rules : [])
        .filter((rule) => among(rule?.test, 'above', 'at_least', 'below', 'at_most', 'equals') && Number.isFinite(rule.value) && among(rule.tone, 'good', 'bad', 'warn', 'accent'))
        .map(({ test, value, tone }) => ({ test, value, tone })),
    },
    geometry: geometry && {
      row_height: whole(geometry.row_height, 65_535),
      widths: Object.fromEntries(Object.entries(geometry.widths ?? {}).filter(([, width]) => whole(width, 65_535) !== undefined)),
    },
  };
}

/** Where a query waits for the page of the cube it asks of. */
const HANDED = 'epiphany.explore.handed';

/** Leaves a query for the page of a cube, which runs it as it opens, within a few seconds. */
export function handOver(cube, text) {
  sessionStorage.setItem(HANDED, JSON.stringify({ cube, mdx: text, at: Date.now() }));
}

/**
 * The query left for a cube's page, if one was left for it just now: one left when going
 * there was called off is no longer wanted. Taken, it is gone.
 */
export function takeOver(cube) {
  const handed = JSON.parse(sessionStorage.getItem(HANDED) ?? 'null');
  sessionStorage.removeItem(HANDED);
  return handed?.cube === cube && Date.now() - handed.at < 10000 ? handed.mdx : null;
}

/** A query of one cell: its first dimension's member on columns, its second's on rows, the rest standing in the slicer. */
export function cellQuery(cube, dimensions, at) {
  const [columns, rows, ...rest] = dimensions.map((dimension, index) => member(dimension, at[index]));
  const axes = [`{${columns}} ON COLUMNS`, rows ? `{${rows}} ON ROWS` : null].filter(Boolean);
  return `SELECT\n  ${axes.join(',\n  ')}\nFROM ${quote(cube)}${rest.length ? `\nWHERE (${rest.join(', ')})` : ''}`;
}
