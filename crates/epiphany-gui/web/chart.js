// Charts of a query's result, drawn as SVG: grouped, stacked or ranked bars, lines, areas, or
// a ring of shares. A chart fills its width, and its height too where asked, and is drawn
// again as either changes. Its marks say what they are when pointed at, or gone through with
// the arrow keys, and may be picked.

import { counted, extent, formatNumber, h, icon, textWidths, widest } from './ui.js';

/** How many colours there are before they come round again, lighter. */
const COLOURS = 10;

/**
 * The colour of a series, or a slice, by its place, as classes: one of the ten, and past
 * them the same again lighter, and past twenty lighter still, so that no two look alike.
 */
const colourOf = (index) => {
  const tint = Math.min(Math.floor(index / COLOURS), 2);
  return `c${index % COLOURS}${tint ? ` tint-${tint}` : ''}`;
};
/** How many slices a ring has at most: past that, the smallest are one slice, Other. */
const MOST_SLICES = 12;
/** How near a bar, in pixels, the pointer is taken to be on it, as thin as bars may be. */
const NEAR_BAR = 4;
/** How far to either side of a line's only column of dots, in pixels, the pointer finds them. */
const NEAR_LONE = 48;

const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });
const percent = new Intl.NumberFormat(undefined, { style: 'percent', maximumFractionDigits: 1 });
const still = matchMedia('(prefers-reduced-motion: reduce)');

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
 * The colour of a bar of a series coloured `colour`: that of the tone `tone(value)` gives,
 * the tone of the first of the view's rules the value meets, or else the series'.
 */
const paint = (colour, value, tone) => {
  const rule = tone?.(value);
  return rule ? `tone-${rule}` : colour;
};

/**
 * How wide text is as a chart writes it, 11 pixels high: its steps and names, or `bold` its
 * values. Its figures are tabular, each as wide as a nought, which a canvas cannot draw: each
 * is measured as one.
 */
const measured = (bold = false) => {
  const measure = textWidths(document.body, { size: '11px', weight: bold ? 600 : 400 });
  return (text) => measure(String(text).replace(/\d/g, '0'));
};

/** A name cut to `most` characters, marked where it was cut. */
const short = (name, most = 18) => (name.length > most ? `${name.slice(0, most - 1)}…` : name);

/**
 * The series and categories of a result: each row is a series across the columns, or each
 * column across the rows when `byColumns`. Each keeps its tuple, the members it stands for,
 * and where the query writes its cells out, as a calculated member's format does, its
 * `texts`, each value as written.
 */
export function shape(result, byColumns) {
  const [columns, rows] = result.axes;
  const label = (tuple) => tuple.map((m) => m.name).join(' · ') || 'Value';
  const columnTuples = columns?.tuples ?? [];
  const rowTuples = rows ? rows.tuples : [[]];
  const count = columnTuples.length;
  const value = (r, c) => {
    const cell = result.cells[r * count + c];
    return typeof cell === 'number' ? cell : null;
  };
  const written = (r, c) => result.formatted?.[r * count + c] ?? null;
  const [across, along] = byColumns ? [rowTuples, columnTuples] : [columnTuples, rowTuples];
  const each = (read, s) => across.map((_, c) => (byColumns ? read(c, s) : read(s, c)));
  return {
    categories: across.map(label),
    tuples: across,
    series: along.map((tuple, s) => ({
      name: label(tuple),
      tuple,
      values: each(value, s),
      texts: result.formatted?.length ? each(written, s) : null,
    })),
  };
}

/** The names of the series of `data` that a chart draws. */
export const seriesNames = (data) => data.series.map((entry) => entry.name);

/**
 * A line of values, small and without axes, to show which way they go: gaps where a value
 * is missing, and a dot on the last.
 */
export function sparkline(values, { width = 180, height = 40 } = {}) {
  const known = values.filter((value) => value !== null);
  const [low, high] = extent(known);
  const x = (index) => 2 + (index / Math.max(values.length - 1, 1)) * (width - 4);
  const y = (value) => 2 + (high === low ? (height - 4) / 2 : (1 - (value - low) / (high - low)) * (height - 4));
  const last = values.length - 1 - [...values].reverse().findIndex((value) => value !== null);
  return h('svg', { class: 'sparkline', width, height, viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': `Trend over ${counted(values.length, 'value')}` },
    h('path', { class: 'line c0', d: linePath(values.map((value, index) => (value === null ? null : [x(index), y(value)]))) }),
    h('circle', { class: 'dot c0', cx: x(last), cy: y(values[last]), r: 3 }));
}

/** A path through the points, broken where one is missing. */
function linePath(points) {
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
  return path;
}

/**
 * The slices of a ring: the first series across the categories, or where there is one
 * category, each series at it. Only what is more than nothing has a share; past MOST_SLICES,
 * the smallest are one slice, which is no one member to pick.
 */
function ringSlices(data) {
  const across = data.categories.length > 1;
  const slices = across
    ? data.categories.map((name, c) => ({ name, value: data.series[0]?.values[c], s: 0, c }))
    : data.series.map((entry, s) => ({ name: entry.name, value: entry.values[0], s, c: 0 }));
  const shown = slices.filter((slice) => slice.value > 0);
  if (shown.length <= MOST_SLICES) return shown;
  const largest = [...shown].sort((a, b) => b.value - a.value);
  const rest = largest.slice(MOST_SLICES - 1);
  return [...largest.slice(0, MOST_SLICES - 1), { name: `Other (${rest.length})`, value: rest.reduce((sum, slice) => sum + slice.value, 0), s: null, c: null }];
}

/**
 * A chart of `data` ({ categories, series }) as `kind`: 'bars', 'stacked', 'shares' (each
 * category's bar its whole, a hundred per cent), 'lines', 'area', 'ranked' or 'donut',
 * `height` pixels high, or as high as its place where `fill`. Where it
 * has axes, the series `lines` names are drawn as lines over the rest, and those `second`
 * names as lines against a scale of their own on the right. `labels`
 * writes each value on its mark, and `format` writes a value as the view has it. `tone(value)`
 * gives the tone a value takes by the view's rules, if any, which each bar of the only series
 * drawn as bars takes for its colour. `pick` is
 * called with the series and category of a mark clicked, or chosen with Enter, and `picked`
 * names the one picked, the others fading. `fresh`: the values are new, and come in moving.
 */
export function chart(data, kind, { height = 380, fill = false, labels = false, format = formatNumber, tone = null, pick = null, picked = null, fresh = true, lines = [], second = [] } = {}) {
  const ring = kind === 'donut';
  const axes = !ring && kind !== 'ranked';
  const beside = new Set(axes ? second : []);
  // Lines over shares of a whole would be against no scale of theirs: only those at the right.
  const over = new Set(axes && kind !== 'lines' && kind !== 'shares' ? lines.filter((name) => !beside.has(name)) : []);
  const slices = ring ? ringSlices(data) : null;
  // What the legend lists and the colours tell apart: the series, or a ring's slices.
  const parts = (ring ? slices.map((slice) => slice.name) : data.series.map((entry) => entry.name))
    .map((name, index) => ({ name, colour: colourOf(index), index }));
  const hidden = new Set();
  const plot = h('div', { class: 'chart-plot', tabindex: '0', role: 'group', 'aria-roledescription': 'chart' });
  const tip = h('div', { class: 'chart-tip', role: 'status', hidden: true });
  const legend = h('div', { class: 'chart-legend', role: 'group', 'aria-label': ring ? 'Slices' : 'Series' });
  // A legend kept to one line, as a dashboard's tile keeps it, is gone along by arrows, as
  // Power BI's is: each shown where there is more that way. The keys reach every item.
  const along = (way, name, shape) => h('button', {
    class: 'button small quiet icon legend-along',
    type: 'button',
    tabindex: '-1',
    hidden: true,
    'aria-label': name,
    title: name,
    onclick: () => legend.scrollBy({ left: way * legend.clientWidth * 0.8, behavior: still.matches ? 'auto' : 'smooth' }),
  }, icon(shape, 14));
  const earlier = along(-1, ring ? 'Earlier slices' : 'Earlier series', 'back');
  const later = along(1, ring ? 'Later slices' : 'Later series', 'chevron');
  const ends = () => {
    earlier.hidden = legend.scrollLeft <= 1;
    later.hidden = legend.scrollLeft >= legend.scrollWidth - legend.clientWidth - 1;
    legend.classList.toggle('more-before', !earlier.hidden);
    legend.classList.toggle('more-after', !later.hidden);
  };
  legend.addEventListener('scroll', ends, { passive: true });
  const element = h('figure', { class: `chart${fill ? ' fill' : ''}` },
    h('div', { class: 'legend-row' }, earlier, legend, later),
    h('div', { class: 'chart-area' }, plot, tip));
  let marks = [];
  // Where the marks drawn stand, worked out once the pointer moves over them, and the one it
  // is on or nearest.
  let near = null;
  let hovered = null;
  let focused = -1;
  let entering = fresh && !still.matches;

  const drawLegend = () => {
    // A series drawn as a line over the rest is shown so, its swatch a line.
    legend.replaceChildren(...parts.map((part) => h('button', {
      class: beside.has(part.name) || over.has(part.name) ? 'legend-item line' : 'legend-item',
      dataset: { s: part.index },
      'aria-pressed': String(!hidden.has(part.index)),
      title: beside.has(part.name) ? `${part.name}, against the axis at the right` : null,
      onclick: () => {
        if (hidden.has(part.index)) hidden.delete(part.index);
        else if (hidden.size < parts.length - 1) hidden.add(part.index);
        drawLegend();
        draw();
      },
    }, h('span', { class: `swatch ${part.colour}` }), part.name)));
    ends();
  };
  // A series whose bars take its view's rules' colours shows those colours in its swatch.
  const tintLegend = (svg) => {
    const paints = new Map();
    for (const bar of svg.querySelectorAll('.bar[data-s]')) {
      const paint = [...bar.classList].find((each) => /^(c\d+|tone-\w+)$/.test(each));
      paints.set(bar.dataset.s, (paints.get(bar.dataset.s) ?? new Set()).add(paint));
    }
    for (const item of legend.children) {
      const used = [...(paints.get(item.dataset.s) ?? [])];
      const colours = used.some((paint) => paint.startsWith('tone-')) ? used.map((paint) => `var(--${paint.replace(/^tone-/, '')})`) : [];
      item.querySelector('.swatch').style.background = colours.length
        ? `linear-gradient(90deg, ${colours.map((colour, at) => `${colour} ${(at * 100) / colours.length}% ${((at + 1) * 100) / colours.length}%`).join(', ')})`
        : '';
    }
  };

  // The size the chart is drawn at, and the size it was drawn at last.
  const size = () => [Math.max(plot.clientWidth, 100), fill ? Math.max(plot.clientHeight, 120) : height];
  let drawnAt = '';
  const draw = () => {
    const [width, tall] = size();
    drawnAt = `${width}x${tall}`;
    const svg = h('svg', { width, height: tall, viewBox: `0 0 ${width} ${tall}`, role: 'img', class: [entering ? 'entering' : '', picked ? 'picking' : ''].join(' ').trim() || null });
    marks = [];
    // Drawn again, at a new size or a series shown or hidden, nothing is pointed at until the
    // pointer moves, and no tip tells of a mark gone; the keys' is shown again below.
    near = null;
    hover(null);
    tip.hidden = true;
    // A mark of series `s` at category `c`: what it says when pointed at, and whether it is the one picked.
    const mark = (shapeElement, { s, c, lines }) => {
      shapeElement.classList.add('mark');
      shapeElement.dataset.tip = lines.join('\n');
      if (s !== null) Object.assign(shapeElement.dataset, { s, c });
      if (picked && picked.series === s && picked.category === c) shapeElement.classList.add('picked');
      marks.push(shapeElement);
      return shapeElement;
    };
    const context = { svg, width, height: tall, data, labels, format, mark, colour: (index) => parts[index]?.colour ?? colourOf(index), shown: (index) => !hidden.has(index), beside, over, tone };
    let said;
    if (ring) said = drawRing(context, slices);
    else if (kind === 'ranked') said = drawRanked(context);
    else said = drawAxes(context, kind);
    svg.setAttribute('aria-label', said);
    tintLegend(svg);
    plot.setAttribute('aria-label', `${said}. The arrow keys go from one value to the next${pick ? ', and Enter picks it' : ''}.`);
    plot.replaceChildren(svg);
    // The legend is as wide as the chart: drawn at a new size, it may hold more or less.
    ends();
    entering = false;
    if (focused >= marks.length) focused = marks.length - 1;
    if (focused >= 0 && plot.contains(document.activeElement)) showFocus();
  };

  const say = (target, x, y) => {
    const [value, name, detail] = target.dataset.tip.split('\n');
    // The series named beside its colour, as the legend has it.
    const colour = [...target.classList].filter((each) => /^(c\d+|tint-\d|tone-\w+)$/.test(each)).join(' ');
    tip.replaceChildren(h('strong', {}, value),
      h('span', { class: 'tip-series' }, colour ? h('span', { class: `swatch ${colour}`, 'aria-hidden': 'true' }) : null, name),
      detail ? h('span', { class: 'muted' }, detail) : null);
    tip.hidden = false;
    // Beside the point, after it and below it, or before it and above it where there is no
    // room, and within the chart.
    const box = plot.getBoundingClientRect();
    const beside = (at, size, room) => Math.max(0, Math.min(at + 12 + size <= room ? at + 12 : at - 12 - size, room - size));
    tip.style.left = `${beside(x, tip.offsetWidth, box.width)}px`;
    tip.style.top = `${beside(y, tip.offsetHeight, box.height)}px`;
  };
  const showFocus = () => {
    marks.forEach((each, index) => each.classList.toggle('focused', index === focused));
    const target = marks[focused];
    if (!target) return;
    const box = target.getBoundingClientRect();
    const area = plot.getBoundingClientRect();
    say(target, box.left - area.left + box.width / 2, box.top - area.top);
  };
  const choose = (target) => {
    if (pick && target?.dataset.s !== undefined) pick({ series: Number(target.dataset.s), category: Number(target.dataset.c) });
  };

  /**
   * The mark nearest the pointer at `x`, `y` across the chart drawn, where it is on none: a
   * bar within a few pixels, as thin as bars may be; or of a line's dots, small to point at,
   * the nearest of those in the column nearest it, as a line is read down from its
   * categories. Slices are pointed at only.
   */
  const nearest = (x, y) => {
    if (!near) {
      const dots = marks.filter((each) => each.localName === 'circle').map((each) => ({ each, x: Number(each.getAttribute('cx')), y: Number(each.getAttribute('cy')) }));
      const across = [...new Set(dots.map((dot) => dot.x))].sort((a, b) => a - b);
      const apart = across.slice(1).reduce((least, at, index) => Math.min(least, at - across[index]), Infinity);
      const bars = marks.filter((each) => each.localName === 'rect').map((each) => {
        const [left, top, wide, high] = ['x', 'y', 'width', 'height'].map((name) => Number(each.getAttribute(name)));
        return { each, left, top, right: left + wide, bottom: top + high };
      });
      near = { dots, bars, reach: Number.isFinite(apart) ? apart / 2 : NEAR_LONE };
    }
    let found = null;
    let least = NEAR_BAR;
    for (const bar of near.bars) {
      const far = Math.hypot(Math.max(bar.left - x, 0, x - bar.right), Math.max(bar.top - y, 0, y - bar.bottom));
      if (far <= least) {
        found = bar.each;
        least = far;
      }
    }
    if (found) return found;
    least = Infinity;
    for (const dot of near.dots) {
      const far = Math.abs(dot.y - y);
      if (Math.abs(dot.x - x) <= near.reach && far < least) {
        found = dot.each;
        least = far;
      }
    }
    return found;
  };
  /** Marks the mark the pointer is on or nearest, or none. */
  const hover = (target) => {
    if (target === hovered) return;
    hovered?.classList.remove('hovered');
    hovered = target;
    hovered?.classList.add('hovered');
    plot.classList.toggle('near', Boolean(hovered));
  };

  plot.addEventListener('pointermove', (event) => {
    const area = plot.firstElementChild?.getBoundingClientRect();
    const target = event.target.closest?.('[data-tip]') ?? (area ? nearest(event.clientX - area.left, event.clientY - area.top) : null);
    hover(target);
    if (!target) {
      tip.hidden = true;
      return;
    }
    const box = plot.getBoundingClientRect();
    say(target, event.clientX - box.left, event.clientY - box.top);
  });
  plot.addEventListener('pointerleave', () => {
    hover(null);
    tip.hidden = true;
  });
  // A click picks the mark it is on, or the one whose tip is shown.
  plot.addEventListener('click', (event) => choose(event.target.closest?.('.mark') ?? hovered));
  plot.addEventListener('keydown', (event) => {
    const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
    if (step || event.key === 'Home' || event.key === 'End') {
      if (!marks.length) return;
      event.preventDefault();
      focused = event.key === 'Home' ? 0 : event.key === 'End' ? marks.length - 1 : Math.min(Math.max(focused + step, 0), marks.length - 1);
      showFocus();
    } else if ((event.key === 'Enter' || event.key === ' ') && marks[focused] && pick) {
      event.preventDefault();
      choose(marks[focused]);
    }
  });
  plot.addEventListener('blur', () => {
    tip.hidden = true;
    marks.forEach((each) => each.classList.remove('focused'));
  });

  drawLegend();
  let queued = false;
  const resize = new ResizeObserver(() => {
    if (!plot.isConnected) {
      resize.disconnect();
      return;
    }
    // Told of the size it was drawn at, as it is the moment it is first watched, it is not
    // drawn again: a chart of thousands of marks was drawn twice for nothing.
    if (queued || size().join('x') === drawnAt) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      draw();
    });
  });
  // Drawn once in the page, then again whenever its size changes.
  requestAnimationFrame(() => {
    draw();
    resize.observe(plot);
  });
  return element;
}

/** The steps of a scale that holds `values`, nought among them. */
function reach(values) {
  let low = 0;
  let high = 0;
  for (const value of values) {
    if (value === null) continue;
    high = Math.max(high, value);
    low = Math.min(low, value);
  }
  return ticks(low, high);
}

/**
 * How a scale writes the steps of `series`: in percent where they write each of their values
 * so, a hundred times over unless the format's percent sign is in quotes, only written.
 */
function stepWriter(series) {
  const written = series.flatMap((entry) => entry.values.flatMap((value, c) => (value === null ? [] : [[value, entry.texts?.[c] ?? '']])));
  if (!written.length || !written.every(([, text]) => /%\s*$/.test(text))) return (step) => compact.format(step);
  const [value, text] = written.find(([number]) => number !== 0) ?? written[0];
  const shown = Math.abs(Number.parseFloat(text.replace(/[^\d.]/g, '')));
  const by = Math.abs(shown - Math.abs(value) * 100) < Math.abs(shown - Math.abs(value)) ? 100 : 1;
  return (step) => `${compact.format(step * by)}%`;
}

/**
 * Bars side by side, stacked, or stacked as shares of each category's whole, lines or areas,
 * over the categories along the bottom; the
 * series `over` names as lines over them; and those `beside` names as lines against a scale
 * of their own on the right, unless they are all there are.
 */
function drawAxes({ svg, width, height, data, labels, format, mark, colour, shown, beside, over, tone }, kind) {
  const all = data.series.map((entry, index) => ({ ...entry, index })).filter((entry) => shown(entry.index));
  const right = all.filter((entry) => beside.has(entry.name));
  const apart = right.length > 0 && right.length < all.length;
  const left = apart ? all.filter((entry) => !beside.has(entry.name)) : all;
  const lined = left.filter((entry) => over.has(entry.name));
  const series = left.filter((entry) => !over.has(entry.name));
  // The only series drawn as bars, or stacked, takes its view's rules' colours bar by bar:
  // with more, colour tells the series apart.
  const shares = kind === 'shares';
  const toned = series.length === 1 && !shares ? tone : null;
  const categories = data.categories;

  // The range the values reach: stacks add up their positives and negatives apart.
  const stack = (c) => {
    let up = 0;
    let down = 0;
    for (const entry of series) {
      const value = entry.values[c] ?? 0;
      if (value > 0) up += value;
      else down += value;
    }
    return [up, down];
  };
  // Shares of each category's whole, its positive values, reach from nought to all of it.
  const steps = shares ? ticks(0, 1) : reach([
    ...(kind === 'stacked' ? categories.flatMap((_, c) => stack(c)) : series.flatMap((entry) => entry.values)),
    ...lined.flatMap((entry) => entry.values),
  ]);
  // The scale at the right, of the series apart, takes as many steps as the one at the left,
  // the one with fewer reaching higher, so each of its steps lies on a gridline.
  const rightSteps = apart ? reach(right.flatMap((entry) => entry.values)) : [];
  for (const list of apart ? [steps, rightSteps] : []) {
    const step = list[1] - list[0];
    while (list.length < Math.max(steps.length, rightSteps.length)) list.push(Number((list.at(-1) + step).toFixed(10)));
  }
  const leftTicks = steps.map((step) => (shares ? percent : compact).format(step));
  const rightTicks = apart ? rightSteps.map(stepWriter(right)) : [];

  // Beside the plot, as much room as the steps written there take.
  const tickWidth = measured();
  const room = (texts) => Math.ceil(texts.reduce((most, text) => Math.max(most, tickWidth(text)), 0)) + 12;
  const margin = { top: labels ? 24 : 16, right: apart ? room(rightTicks) : 16, bottom: 40, left: Math.max(room(leftTicks), 32) };
  const bandOf = () => Math.max(width - margin.left - margin.right, 1) / Math.max(categories.length, 1);
  // The names along the bottom go aslant where they would not fit side by side: beneath them
  // as much room as the longest takes, aslant, and before them as much as the first reaches
  // to the left of its place, its length and its height turned forty degrees. Only every so
  // many are written where even aslant they would run into one another.
  const longest = categories.reduce((most, name) => Math.max(most, tickWidth(short(name))), 0);
  const crowded = bandOf() < longest + 8;
  if (crowded) {
    margin.bottom = Math.min(Math.max(Math.ceil(longest * 0.65) + 26, 40), 84);
    margin.left = Math.max(margin.left, Math.ceil(longest * 0.77 + 8 - bandOf() / 2) + 4);
  }
  const band = bandOf();
  const every = crowded ? Math.ceil(18 / band) : 1;
  const inner = { width: Math.max(width - margin.left - margin.right, 1), height: Math.max(height - margin.top - margin.bottom, 20) };
  const scale = (list) => (value) => margin.top + inner.height - ((value - list[0]) / (list.at(-1) - list[0])) * inner.height;
  const y = scale(steps);
  const yRight = apart ? scale(rightSteps) : y;
  const x = (c) => margin.left + band * c;
  // Marks narrower than a pixel stand over one another: of those in each pixel's column only
  // the highest and the lowest can be seen, and only they are drawn, so that a chart of
  // 100,000 categories draws as many marks as it has pixels across. A bar from nought hides
  // the others its way: of bars, the highest above nought and the lowest below. Null where
  // every mark is seen.
  const seen = (values, bars = true) => {
    if (band >= 1) return null;
    const [highest, lowest] = [new Map(), new Map()];
    values.forEach((value, c) => {
      if (value === null || value === undefined) return;
      const column = Math.floor(x(c));
      const [most, least] = [highest.get(column), lowest.get(column)];
      if (most === undefined || value > values[most]) highest.set(column, c);
      if (least === undefined || value < values[least]) lowest.set(column, c);
    });
    return new Set([...[...highest.values()].filter((c) => !bars || values[c] > 0), ...[...lowest.values()].filter((c) => !bars || values[c] < 0)]);
  };
  // A value as the query writes it, where it does, as a calculated member's format has it; as
  // the view's format has it in a tip, and briefly on its mark, otherwise.
  const tipOf = (entry, c, value) => [entry.texts?.[c] ?? format(value), entry.name, categories[c]];
  const said = (entry, c) => entry.texts?.[c] ?? compact.format(entry.values[c]);
  // A value written on its mark, unless it would run into one written already: those first
  // written stand, as Power BI hides the labels that collide.
  const valueWidth = measured(true);
  const written = [];
  const label = (text, cx, cy, anchor = 'middle') => {
    const wide = valueWidth(text);
    const left = anchor === 'middle' ? cx - wide / 2 : anchor === 'end' ? cx - wide : cx;
    const box = { left, right: left + wide, top: cy - 10, bottom: cy + 2 };
    if (written.some((other) => box.left < other.right + 2 && other.left < box.right + 2 && box.top < other.bottom && other.top < box.bottom)) return;
    written.push(box);
    svg.append(h('text', { class: 'value', x: cx, y: cy, 'text-anchor': anchor }, text));
  };

  steps.forEach((step, s) => {
    svg.append(h('line', { class: step === 0 ? 'axis' : 'gridline', x1: margin.left, x2: width - margin.right, y1: y(step), y2: y(step) }),
      h('text', { class: 'tick', x: margin.left - 8, y: y(step) + 4, 'text-anchor': 'end' }, leftTicks[s]));
    if (apart) svg.append(h('text', { class: 'tick right', x: width - margin.right + 8, y: y(step) + 4, 'text-anchor': 'start' }, rightTicks[s]));
  });
  categories.forEach((name, c) => {
    if (c % every) return;
    const cx = x(c) + band / 2;
    svg.append(crowded
      ? h('text', { class: 'tick', x: cx, y: height - margin.bottom + 14, 'text-anchor': 'end', transform: `rotate(-40 ${cx} ${height - margin.bottom + 14})` }, short(name))
      : h('text', { class: 'tick', x: cx, y: height - margin.bottom + 18, 'text-anchor': 'middle' }, short(name)));
  });

  /** A series as a line through its values against the scale `of`, a dot at each; filled down to nought for an area. */
  const drawLine = (entry, of, filled) => {
    const points = entry.values.map((value, c) => (value === null ? null : [x(c) + band / 2, of(value)]));
    // Dense, the line and its dots go through those seen alone, as one through every point
    // looks; it is broken only where a value is missing.
    const kept = seen(entry.values, false);
    const traced = kept ? points.filter((point, c) => point === null || kept.has(c)) : points;
    if (filled) {
      // Each unbroken run of values fills down to nought.
      let run = [];
      const fillRun = () => {
        if (run.length > 1) svg.append(h('path', { class: `area ${colour(entry.index)}`, d: `M${run[0][0]},${of(0)}L${run.map((point) => point.join(',')).join('L')}L${run[run.length - 1][0]},${of(0)}Z` }));
        run = [];
      };
      traced.forEach((point) => (point ? run.push(point) : fillRun()));
      fillRun();
    }
    svg.append(h('path', { class: `line ${colour(entry.index)}`, d: linePath(traced), pathLength: 1 }));
    points.forEach((point, c) => {
      if (!point || (kept && !kept.has(c))) return;
      svg.append(mark(h('circle', { class: `dot ${colour(entry.index)}`, cx: point[0], cy: point[1], r: 4 }), { s: entry.index, c, lines: tipOf(entry, c, entry.values[c]) }));
      if (labels) label(said(entry, c), point[0], point[1] - 9);
    });
  };

  if (kind === 'lines' || kind === 'area') {
    for (const entry of series) drawLine(entry, y, kind === 'area');
  } else if (kind === 'stacked' || shares) {
    const thick = Math.max(Math.min(band * 0.64, 56), 2);
    const stacks = categories.map((_, c) => stack(c));
    const kept = band < 1 ? new Set([...seen(stacks.map(([up]) => up)), ...seen(stacks.map(([, down]) => down))]) : null;
    categories.forEach((_, c) => {
      if (kept && !kept.has(c)) return;
      // As shares, each positive value is drawn as its part of the category's whole.
      const [whole] = stacks[c];
      let up = 0;
      let down = 0;
      for (const entry of series) {
        const value = entry.values[c] ?? 0;
        const amount = shares ? (value > 0 ? value / whole : 0) : value;
        if (!amount) continue;
        const from = amount > 0 ? up : down;
        const to = from + amount;
        if (amount > 0) up = to;
        else down = to;
        const box = { x: x(c) + (band - thick) / 2, y: Math.min(y(from), y(to)), width: thick, height: Math.max(Math.abs(y(to) - y(from)), 1) };
        const lines = shares ? [percent.format(amount), entry.name, `${categories[c]} · ${entry.texts?.[c] ?? format(value)}`] : tipOf(entry, c, value);
        svg.append(mark(h('rect', { class: `bar ${paint(colour(entry.index), value, toned)}`, ...box }), { s: entry.index, c, lines }));
        if (labels && box.height > 14) label(shares ? percent.format(amount) : said(entry, c), box.x + thick / 2, box.y + box.height / 2 + 4);
      }
    });
  } else {
    const group = Math.min(band * 0.8, 24 * series.length + 8);
    const each = Math.max(group / Math.max(series.length, 1), 2);
    const kept = series.map((entry) => seen(entry.values));
    categories.forEach((_, c) => {
      series.forEach((entry, s) => {
        const value = entry.values[c];
        if (!value || (kept[s] && !kept[s].has(c))) return;
        const left = x(c) + (band - group) / 2 + s * each + 1;
        svg.append(mark(h('rect', {
          class: `bar ${paint(colour(entry.index), value, toned)}`,
          x: left,
          y: Math.min(y(0), y(value)),
          width: Math.max(each - 2, 1),
          height: Math.max(Math.abs(y(value) - y(0)), 1),
          rx: 2,
        }), { s: entry.index, c, lines: tipOf(entry, c, value) }));
        if (labels) label(said(entry, c), left + Math.max(each - 2, 1) / 2, value < 0 ? y(value) + 13 : y(value) - 5);
      });
    });
  }
  // The series drawn as lines over the rest: against the same scale, and those apart against
  // the scale at the right.
  for (const entry of lined) drawLine(entry, y, false);
  if (apart) for (const entry of right) drawLine(entry, yRight, false);
  const kindSaid = { lines: 'Line', area: 'Area', stacked: 'Stacked bar', shares: '100% stacked bar' }[kind] ?? 'Bar';
  const named = (list) => list.map((entry) => entry.name).join(', ');
  const listed = [
    series.length ? named(series) : null,
    lined.length ? `${named(lined)} as lines` : null,
    apart ? `${named(right)} against an axis at the right` : null,
  ].filter(Boolean).join('; ');
  return `${kindSaid} chart of ${listed}, across ${categories.length} categories`;
}

/** The categories as bars across, the largest of the first series shown at the top. */
function drawRanked({ svg, width, height, data, labels, format, mark, colour, shown, tone }) {
  const entry = data.series.map((each, index) => ({ ...each, index })).find((each) => shown(each.index));
  const ranked = data.categories.map((name, c) => ({ name, c, value: entry?.values[c] })).filter((item) => item.value !== null && item.value !== undefined)
    .sort((a, b) => b.value - a.value);
  const margin = { top: 8, right: 16, bottom: 28, left: 16 };
  const tall = Math.max(height - margin.top - margin.bottom, 20);
  const band = tall / Math.max(ranked.length, 1);
  // Names and values are written where a bar is tall enough to be told by them: before the
  // bars as much room as the longest name takes, up to two fifths of the chart, and after
  // them as much as the widest value. Only those are measured: a million are not.
  const named = band >= 11;
  const written = (item) => entry.texts?.[item.c] ?? compact.format(item.value);
  if (named) {
    margin.left = Math.min(widest(ranked, (item) => short(item.name, 28), () => 0, measured()) + 16, width * 0.4);
    if (labels) margin.right = Math.ceil(widest(ranked, written, () => 0, measured(true))) + 14;
  }
  const inner = { width: Math.max(width - margin.left - margin.right, 20), height: tall };
  const [least, most] = extent(ranked.map((item) => item.value));
  const steps = ticks(Math.min(0, least), Math.max(0, most), 4);
  const low = steps[0];
  const high = steps[steps.length - 1];
  const x = (value) => margin.left + ((value - low) / (high - low)) * inner.width;
  const thick = Math.max(Math.min(band * 0.7, 28), 1);
  for (const step of steps) {
    svg.append(h('line', { class: step === 0 ? 'axis' : 'gridline', x1: x(step), x2: x(step), y1: margin.top, y2: margin.top + inner.height }),
      h('text', { class: 'tick', x: x(step), y: height - 8, 'text-anchor': 'middle' }, compact.format(step)));
  }
  // Bars thinner than a pixel stand over one another: of each pixel's row only the longest
  // can be seen, and only it is drawn.
  const longest = new Map();
  if (band < 1) {
    ranked.forEach((item, rank) => {
      const row = Math.floor(band * rank);
      const was = longest.get(row);
      if (was === undefined || Math.abs(item.value) > Math.abs(ranked[was].value)) longest.set(row, rank);
    });
  }
  const seen = new Set(longest.values());
  ranked.forEach((item, rank) => {
    if (band < 1 && !seen.has(rank)) return;
    const top = margin.top + band * rank + (band - thick) / 2;
    if (named) svg.append(h('text', { class: 'tick', x: margin.left - 8, y: top + thick / 2 + 4, 'text-anchor': 'end' }, short(item.name, 28)));
    svg.append(mark(h('rect', { class: `bar ${paint(colour(entry.index), item.value, tone)}`, x: Math.min(x(0), x(item.value)), y: top, width: Math.max(Math.abs(x(item.value) - x(0)), 1), height: thick, rx: 2 }),
      { s: entry.index, c: item.c, lines: [entry.texts?.[item.c] ?? format(item.value), item.name, `${rank + 1} of ${ranked.length}`] }));
    if (labels && named) svg.append(h('text', { class: 'value', x: x(Math.max(item.value, 0)) + 6, y: top + thick / 2 + 4 }, written(item)));
  });
  return `Bars of ${entry?.name ?? 'nothing'} across ${ranked.length} categories, the largest first`;
}

/** A ring of each slice's share of their whole, the whole written in the middle. */
function drawRing({ svg, width, height, labels, format, mark, colour, shown }, slices) {
  const drawn = slices.map((slice, index) => ({ ...slice, index })).filter((slice) => shown(slice.index));
  const whole = drawn.reduce((sum, slice) => sum + slice.value, 0);
  const cx = width / 2;
  const cy = height / 2;
  const outer = Math.max(Math.min(width, height) / 2 - 8, 10);
  const inner = outer * 0.62;
  const point = (angle, radius) => [cx + radius * Math.sin(angle), cy - radius * Math.cos(angle)];
  const shareWidth = measured(true);
  let angle = 0;
  for (const slice of drawn) {
    const sweep = whole ? (slice.value / whole) * Math.PI * 2 : 0;
    // A whole ring is two halves: an arc cannot end where it begins.
    const end = angle + Math.min(sweep, Math.PI * 2 - 1e-4);
    const large = end - angle > Math.PI ? 1 : 0;
    const [x1, y1] = point(angle, outer);
    const [x2, y2] = point(end, outer);
    const [x3, y3] = point(end, inner);
    const [x4, y4] = point(angle, inner);
    svg.append(mark(h('path', { class: `slice ${colour(slice.index)}`, d: `M${x1},${y1}A${outer},${outer} 0 ${large} 1 ${x2},${y2}L${x3},${y3}A${inner},${inner} 0 ${large} 0 ${x4},${y4}Z` }),
      { s: slice.s, c: slice.c, lines: [format(slice.value), slice.name, `${percent.format(slice.value / whole)} of ${format(whole)}`] }));
    // A slice's share written on it where its arc, through the middle of the ring, holds it.
    const share = percent.format(slice.value / whole);
    if (labels && sweep * ((outer + inner) / 2) >= shareWidth(share) + 6) {
      const [lx, ly] = point(angle + sweep / 2, (outer + inner) / 2);
      svg.append(h('text', { class: 'value on-slice', x: lx, y: ly + 4, 'text-anchor': 'middle' }, share));
    }
    angle += sweep;
  }
  svg.append(h('text', { class: 'ring-whole', x: cx, y: cy + 2, 'text-anchor': 'middle' }, compact.format(whole)),
    h('text', { class: 'tick', x: cx, y: cy + 20, 'text-anchor': 'middle' }, 'Total'));
  return `Ring of ${drawn.length} shares of ${format(whole)}`;
}
