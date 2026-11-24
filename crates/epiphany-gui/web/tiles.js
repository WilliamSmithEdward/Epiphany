// How a dashboard's tile shows its view's answer: a key figure, a chart, a table of every row
// or a heatmap; or the tile's own few lines of text. Each fills its tile and is drawn again as
// the tile changes size. What a reader clicks on, a mark or a row, is picked: the dashboard
// shows every other tile at its members.

import { empty } from './app.js';
import { chart, shape, sparkline } from './chart.js';
import { highlighter, toneOf } from './grid.js';
import { Rows } from './tree.js';
import { extent, formatValue, h, shownText, widest } from './ui.js';

/** The ways a tile shows its view, by name: what each is called, and its icon. */
export const KINDS = {
  figure: ['Key figure', 'hash'],
  bars: ['Bars', 'chart'],
  stacked: ['Stacked bars', 'bars'],
  lines: ['Lines', 'explore'],
  area: ['Area', 'area'],
  ranked: ['Ranked bars', 'ranked'],
  donut: ['Donut', 'ring'],
  grid: ['Grid', 'grid'],
  heatmap: ['Heatmap', 'heat'],
  text: ['Text', 'text'],
};

const label = (tuple) => tuple.map((m) => m.name).join(' · ');

/**
 * A view's answer as rows of cells, to copy or download: its rows' dimensions over their
 * names, its columns' names across the top, and each value as it is, not as it is shown.
 */
export function numbersOf(result) {
  const [columns, rows] = result.axes;
  const columnTuples = columns?.tuples ?? [];
  const rowTuples = rows?.tuples ?? [[]];
  const count = columnTuples.length;
  const lead = (rowTuples[0] ?? []).map((m) => m.dimension);
  return [
    [...(lead.length ? lead : ['']), ...columnTuples.map(label)],
    ...rowTuples.map((tuple, r) => [...(tuple.length ? tuple.map((m) => m.name) : ['']), ...Array.from({ length: count }, (_, c) => result.cells[r * count + c])]),
  ];
}

/**
 * A view's answer as `tile` shows it, in the view's number format and with its highlights.
 * `pick(members, key)` is called with the members of what is clicked, and `picked` is the key
 * of the one picked, to be marked. `fresh`: the answer is new, rather than shown again.
 */
export function present(result, tile, view, { pick = null, picked = null, fresh = true } = {}) {
  const how = tile.show ?? 'bars';
  const format = view?.format;
  if (how === 'figure') return figure(result, format, view?.highlights);
  if (how === 'grid' || how === 'heatmap') return table(result, { format, highlights: view?.highlights, heat: how === 'heatmap', pick, picked });
  const data = shape(result, Boolean(tile.by_columns));
  if (!data.series.some((entry) => entry.values.some((value) => value !== null))) {
    return empty('chart', 'Nothing to chart', 'The view has no numbers.');
  }
  const at = /^s:(\d+)\|c:(\d+)$/.exec(picked ?? '');
  return chart(data, how, {
    fill: true,
    labels: Boolean(tile.labels),
    lines: tile.lines ?? [],
    second: tile.second_axis ?? [],
    format: (value) => formatValue(value, format),
    pick: pick && (({ series, category }) => pick([...data.series[series].tuple, ...data.tuples[category]], `s:${series}|c:${category}`)),
    picked: at ? { series: Number(at[1]), category: Number(at[2]) } : null,
    fresh,
  });
}

/**
 * The latest value of the first row, large, with its change from the value before it and the
 * row's trend beneath: the row is taken to run through time, as a key figure's mostly does.
 * It takes the colour of the first of the view's highlight rules it meets, as its cell would
 * in the grid: green at or above a target, say.
 */
function figure(result, format, highlights) {
  const [columns, rows] = result.axes;
  const columnTuples = columns?.tuples ?? [];
  const row = columnTuples.map((_, c) => (typeof result.cells[c] === 'number' ? result.cells[c] : null));
  const known = row.map((value, c) => ({ value, c })).filter((each) => each.value !== null);
  const latest = known.at(-1);
  const before = known.at(-2);
  const value = latest ? latest.value : result.cells[0];
  const at = latest ? latest.c : 0;
  const name = [...(rows?.tuples[0] ?? []), ...(columnTuples[at] ?? [])].map((m) => m.name).join(' · ');
  let change = null;
  if (latest && before) {
    const by = latest.value - before.value;
    const share = before.value ? by / Math.abs(before.value) : null;
    const said = share === null ? formatValue(by, format) : share.toLocaleString(undefined, { style: 'percent', maximumFractionDigits: 1, signDisplay: 'exceptZero' });
    change = h('div', { class: `figure-change ${by > 0 ? 'up' : by < 0 ? 'down' : ''}` },
      h('span', { 'aria-hidden': 'true' }, by > 0 ? '▲' : by < 0 ? '▼' : '='),
      h('span', {}, `${said} from ${label(columnTuples[before.c])}`));
  }
  const tone = toneOf(highlights, value);
  return h('div', { class: 'figure' },
    h('div', { class: `figure-value${tone ? ` tone-${tone}` : typeof value === 'number' && value < 0 ? ' negative' : ''}` }, value === null || value === undefined ? '–' : latest ? formatValue(value, format) : shownText(result, 0, format)),
    h('div', { class: 'muted figure-name' }, name),
    change,
    known.length > 1 ? h('div', { class: 'figure-trend' }, sparkline(row), h('span', { class: 'muted' }, `${label(columnTuples[0])} to ${label(columnTuples.at(-1))}`)) : null);
}

/**
 * The answer as a table of every row, drawn only where it can be seen: its rows' names down
 * the left and its columns' across the top, both kept in sight as it scrolls. A grid's cells
 * take the view's highlights; a heatmap's are shaded from the lowest value to the highest.
 * A row's name picks its members, and a cell those of its row and column.
 */
function table(result, { format, highlights, heat, pick, picked }) {
  const [columns, rows] = result.axes;
  const columnTuples = columns?.tuples ?? [];
  const rowTuples = rows?.tuples ?? [[]];
  const count = columnTuples.length;
  const value = (r, c) => result.cells[r * count + c];
  const total = (tuple) => tuple.some((m) => m.type === 'consolidated');
  const mark = heat ? heatShades(rowTuples, count, value) : highlighter(highlights, rowTuples, columnTuples, value, total);
  const scroller = h('div', { class: `tile-table${heat ? ' heat' : ''}${pick ? ' pickable' : ''}`, role: 'table', 'aria-rowcount': rowTuples.length + 1, tabindex: '0' });
  // The names' column as wide as the widest name needs.
  const names = Math.max(48, widest(rowTuples, label));
  scroller.style.setProperty('--heading', `${Math.ceil(Math.min(names + 28, 320))}px`);
  scroller.style.setProperty('--count', String(Math.max(count, 1)));
  const choose = (members, key) => pick?.(members, key);
  const head = h('div', { class: 'tile-row head', role: 'row', 'aria-rowindex': 1 },
    h('span', { role: 'columnheader', class: 'corner' }),
    columnTuples.map((tuple) => h('span', { role: 'columnheader', class: 'n' }, label(tuple))));
  const drawn = new Rows(scroller, (r) => {
    const tuple = rowTuples[r];
    const key = `r:${r}`;
    const row = h('div', { class: `tile-row${total(tuple) ? ' total' : ''}${picked === key ? ' picked' : ''}`, role: 'row', 'aria-rowindex': r + 2 },
      h('span', { role: 'rowheader' },
        pick ? h('button', { class: 'row-pick', type: 'button', title: `Show every other tile at ${label(tuple)}`, onclick: () => choose(tuple, key) }, label(tuple)) : label(tuple)));
    for (let c = 0; c < count; c += 1) {
      const number = value(r, c);
      const cell = h('span', {
        role: 'cell',
        class: `n${typeof number === 'number' && number < 0 ? ' negative' : ''}${picked === `${key}|c:${c}` ? ' picked' : ''}`,
        onclick: pick ? () => choose([...tuple, ...columnTuples[c]], `${key}|c:${c}`) : null,
      }, shownText(result, r * count + c, format));
      mark?.(cell, r, c, number);
      row.append(cell);
    }
    return row;
  }, { covered: () => head.offsetHeight });
  scroller.append(head, drawn.element);
  drawn.show(rowTuples.length);
  return scroller;
}

/**
 * Shades each number of a heatmap by where it lies from its row's lowest to its highest: a
 * row is one measure across its columns, often of a unit of its own, and a total's row is
 * shaded on its own scale like any other.
 */
function heatShades(rowTuples, count, value) {
  const ranges = new Map();
  const range = (r) => {
    if (!ranges.has(r)) {
      const numbers = Array.from({ length: count }, (_, c) => value(r, c)).filter((number) => typeof number === 'number');
      ranges.set(r, extent(numbers));
    }
    return ranges.get(r);
  };
  return (cell, r, c, number) => {
    const [low, high] = range(r);
    if (typeof number !== 'number' || !(high > low)) return;
    cell.classList.add('shade');
    cell.style.setProperty('--shade', ((number - low) / (high - low)).toFixed(3));
  };
}

/**
 * A tile's own text: a line that begins `# ` a heading, one that begins `- ` an item of a list,
 * an empty line the end of a paragraph, and words between `**` strong. Nothing in it is taken
 * for markup.
 */
export function prose(text) {
  const blocks = [];
  let paragraph = [];
  let list = null;
  const strong = (line) => line.split(/\*\*(.+?)\*\*/).map((part, index) => (index % 2 ? h('strong', {}, part) : part));
  const end = () => {
    if (paragraph.length) blocks.push(h('p', {}, strong(paragraph.join(' '))));
    if (list) blocks.push(list);
    paragraph = [];
    list = null;
  };
  for (const line of String(text).split('\n').map((each) => each.trim())) {
    if (!line) end();
    else if (line.startsWith('# ')) {
      end();
      blocks.push(h('h3', {}, strong(line.slice(2))));
    } else if (line.startsWith('- ')) {
      if (paragraph.length) end();
      (list ??= h('ul')).append(h('li', {}, strong(line.slice(2))));
    } else {
      if (list) end();
      paragraph.push(line);
    }
  }
  end();
  return h('div', { class: 'prose' }, blocks);
}
