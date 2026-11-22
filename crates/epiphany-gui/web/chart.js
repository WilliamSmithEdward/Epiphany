// Charts of a query's result, drawn as SVG: grouped bars, stacked bars or lines.

import { formatNumber, h } from './ui.js';

/** How many series are drawn at most; the rest are left out and said to be. */
const MOST_SERIES = 12;
/** How many colours there are before they repeat. */
const COLOURS = 10;

const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });

/** Round steps for an axis from `low` to `high`. */
function ticks(low, high, count = 5) {
  if (low === high) {
    high = low + 1;
  }
  const rough = (high - low) / count;
  const magnitude = 10 ** Math.floor(Math.log10(rough));
  const ratio = rough / magnitude;
  const step = (ratio >= 7.5 ? 10 : ratio >= 3.5 ? 5 : ratio >= 1.5 ? 2 : 1) * magnitude;
  const out = [];
  for (let value = Math.floor(low / step) * step; value <= Math.ceil(high / step) * step + step / 2; value += step) {
    out.push(Number(value.toFixed(10)));
  }
  return out;
}

/**
 * The series and categories of a result: each row is a series across the columns, or each
 * column across the rows when `byColumns`.
 */
export function shape(result, byColumns) {
  const [columns, rows] = result.axes;
  const label = (tuple) => tuple.map((m) => m.name).join(' · ');
  const columnLabels = (columns?.tuples ?? []).map(label);
  const rowLabels = rows ? rows.tuples.map(label) : ['Value'];
  const count = columnLabels.length;
  const value = (r, c) => {
    const cell = result.cells[r * count + c];
    return typeof cell === 'number' ? cell : null;
  };
  const series = byColumns
    ? columnLabels.map((name, c) => ({ name, values: rowLabels.map((_, r) => value(r, c)) }))
    : rowLabels.map((name, r) => ({ name, values: columnLabels.map((_, c) => value(r, c)) }));
  return { categories: byColumns ? rowLabels : columnLabels, series };
}

/**
 * A line of values, small and without axes, to show which way they go: gaps where a value
 * is missing, and a dot on the last.
 */
export function sparkline(values, { width = 180, height = 40 } = {}) {
  const known = values.filter((value) => value !== null);
  const low = Math.min(...known);
  const high = Math.max(...known);
  const x = (index) => 2 + (index / Math.max(values.length - 1, 1)) * (width - 4);
  const y = (value) => 2 + (high === low ? (height - 4) / 2 : (1 - (value - low) / (high - low)) * (height - 4));
  let path = '';
  let drawing = false;
  values.forEach((value, index) => {
    if (value === null) {
      drawing = false;
      return;
    }
    path += `${drawing ? 'L' : 'M'}${x(index).toFixed(1)},${y(value).toFixed(1)}`;
    drawing = true;
  });
  const last = values.length - 1 - [...values].reverse().findIndex((value) => value !== null);
  return h('svg', { class: 'sparkline', width, height, viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': `Trend over ${values.length} values` },
    h('path', { class: 'line c0', d: path }),
    h('circle', { class: 'dot c0', cx: x(last), cy: y(values[last]), r: 3 }));
}

/**
 * A chart of `data` ({ categories, series }) as `kind`: 'bars', 'stacked' or 'lines',
 * `height` pixels high. It fills its container's width and redraws as that changes.
 */
export function chart(data, kind, { height = 380 } = {}) {
  const hidden = new Set();
  const total = data.series.length;
  const series = data.series.slice(0, MOST_SERIES).map((entry, index) => ({ ...entry, colour: index % COLOURS }));
  const plot = h('div', { class: 'chart-plot' });
  const tip = h('div', { class: 'chart-tip', role: 'status', hidden: true });
  const legend = h('div', { class: 'chart-legend', role: 'group', 'aria-label': 'Series' });
  const element = h('figure', { class: 'chart' },
    legend,
    h('div', { class: 'chart-area' }, plot, tip),
    total > MOST_SERIES ? h('figcaption', { class: 'hint' }, `Showing the first ${MOST_SERIES} of ${total} series.`) : null);

  const drawLegend = () => {
    legend.replaceChildren(...series.map((entry) => h('button', {
      class: 'legend-item',
      'aria-pressed': String(!hidden.has(entry.name)),
      onclick: () => {
        if (hidden.has(entry.name)) hidden.delete(entry.name);
        else if (hidden.size < series.length - 1) hidden.add(entry.name);
        drawLegend();
        draw();
      },
    }, h('span', { class: `swatch c${entry.colour}` }), entry.name)));
  };

  const draw = () => {
    const width = Math.max(plot.clientWidth, 280);
    const shown = series.filter((entry) => !hidden.has(entry.name));
    const categories = data.categories;
    const crowded = categories.length > Math.floor(width / 70);
    const margin = { top: 16, right: 16, bottom: crowded ? 84 : 40, left: 64 };
    const inner = { width: width - margin.left - margin.right, height: height - margin.top - margin.bottom };

    // The range the values reach: stacks add up their positives and negatives apart.
    let low = 0;
    let high = 0;
    categories.forEach((_, c) => {
      if (kind === 'stacked') {
        let up = 0;
        let down = 0;
        for (const entry of shown) {
          const value = entry.values[c] ?? 0;
          if (value > 0) up += value;
          else down += value;
        }
        high = Math.max(high, up);
        low = Math.min(low, down);
      } else {
        for (const entry of shown) {
          const value = entry.values[c];
          if (value === null) continue;
          high = Math.max(high, value);
          low = Math.min(low, value);
        }
      }
    });
    const steps = ticks(low, high);
    const bottom = steps[0];
    const top = steps[steps.length - 1];
    const y = (value) => margin.top + inner.height - ((value - bottom) / (top - bottom)) * inner.height;
    const band = inner.width / Math.max(categories.length, 1);
    const x = (c) => margin.left + band * c;

    const svg = h('svg', {
      width,
      height,
      viewBox: `0 0 ${width} ${height}`,
      role: 'img',
      'aria-label': `${kind === 'lines' ? 'Line' : 'Bar'} chart of ${shown.map((entry) => entry.name).join(', ')} across ${categories.length} categories`,
    });
    for (const step of steps) {
      svg.append(h('line', { class: step === 0 ? 'axis' : 'gridline', x1: margin.left, x2: width - margin.right, y1: y(step), y2: y(step) }),
        h('text', { class: 'tick', x: margin.left - 8, y: y(step) + 4, 'text-anchor': 'end' }, compact.format(step)));
    }
    categories.forEach((name, c) => {
      const cx = x(c) + band / 2;
      const short = name.length > 18 ? `${name.slice(0, 17)}…` : name;
      svg.append(crowded
        ? h('text', { class: 'tick', x: cx, y: height - margin.bottom + 14, 'text-anchor': 'end', transform: `rotate(-40 ${cx} ${height - margin.bottom + 14})` }, short)
        : h('text', { class: 'tick', x: cx, y: height - margin.bottom + 18, 'text-anchor': 'middle' }, short));
    });

    const marks = [];
    const mark = (shapeElement, entry, c, value) => {
      shapeElement.dataset.tip = `${categories[c]}\n${entry.name}\n${formatNumber(value)}`;
      marks.push(shapeElement);
      return shapeElement;
    };
    if (kind === 'lines') {
      for (const entry of shown) {
        const points = entry.values.map((value, c) => (value === null ? null : [x(c) + band / 2, y(value)]));
        // A gap in the data is a gap in the line.
        let path = '';
        let drawing = false;
        for (const point of points) {
          if (!point) {
            drawing = false;
            continue;
          }
          path += `${drawing ? 'L' : 'M'}${point[0].toFixed(1)},${point[1].toFixed(1)}`;
          drawing = true;
        }
        svg.append(h('path', { class: `line c${entry.colour}`, d: path }));
        points.forEach((point, c) => {
          if (point) svg.append(mark(h('circle', { class: `dot c${entry.colour}`, cx: point[0], cy: point[1], r: 4 }), entry, c, entry.values[c]));
        });
      }
    } else if (kind === 'stacked') {
      const width0 = Math.max(Math.min(band * 0.64, 56), 2);
      categories.forEach((_, c) => {
        let up = 0;
        let down = 0;
        for (const entry of shown) {
          const value = entry.values[c] ?? 0;
          if (!value) continue;
          const from = value > 0 ? up : down;
          const to = from + value;
          if (value > 0) up = to;
          else down = to;
          svg.append(mark(h('rect', {
            class: `bar c${entry.colour}`,
            x: x(c) + (band - width0) / 2,
            y: Math.min(y(from), y(to)),
            width: width0,
            height: Math.max(Math.abs(y(to) - y(from)), 1),
          }), entry, c, value));
        }
      });
    } else {
      const group = Math.min(band * 0.8, 24 * shown.length + 8);
      const each = Math.max(group / Math.max(shown.length, 1), 2);
      categories.forEach((_, c) => {
        shown.forEach((entry, s) => {
          const value = entry.values[c];
          if (!value) return;
          svg.append(mark(h('rect', {
            class: `bar c${entry.colour}`,
            x: x(c) + (band - group) / 2 + s * each + 1,
            y: Math.min(y(0), y(value)),
            width: Math.max(each - 2, 1),
            height: Math.max(Math.abs(y(value) - y(0)), 1),
            rx: 2,
          }), entry, c, value));
        });
      });
    }
    svg.addEventListener('pointermove', (event) => {
      const target = event.target.closest('[data-tip]');
      if (!target) {
        tip.hidden = true;
        return;
      }
      const [category, name, value] = target.dataset.tip.split('\n');
      tip.replaceChildren(h('strong', {}, value), h('span', {}, name), h('span', { class: 'muted' }, category));
      tip.hidden = false;
      const box = plot.getBoundingClientRect();
      tip.style.left = `${Math.min(event.clientX - box.left + 12, box.width - 180)}px`;
      tip.style.top = `${event.clientY - box.top + 12}px`;
    });
    svg.addEventListener('pointerleave', () => {
      tip.hidden = true;
    });
    plot.replaceChildren(svg);
  };

  drawLegend();
  const resize = new ResizeObserver(() => {
    if (plot.isConnected) draw();
    else resize.disconnect();
  });
  // Drawn once in the page, then again whenever its width changes.
  requestAnimationFrame(() => {
    draw();
    resize.observe(plot);
  });
  return element;
}
