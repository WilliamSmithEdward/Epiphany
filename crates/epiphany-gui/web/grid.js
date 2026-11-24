// The pivot grid: drawing and sizing it, moving about its headings, finding in it,
// highlighting it, and its menus. Methods of the explorer, kept apart to keep each part readable.

import { post } from './api.js';
import { empty } from './app.js';
import { checkFeeders } from './feeders.js';
import { member, namesOf, outlined, pathsBeneath, saved } from './layout.js';
import { cellText, Selection } from './selection.js';
import { beneath } from './tree.js';
import { copyText, counted, dialog, formatNumber, formatValue, h, icon, menu, mount, notify, printsWhole, SCALES, shownText, when } from './ui.js';

/** The most cells a grid draws whole for printing: every row draws all its columns. */
const PRINTED = 200_000;

/** How a highlight rule tests a number against its value. */
const TESTS = { above: (a, b) => a > b, at_least: (a, b) => a >= b, below: (a, b) => a < b, at_most: (a, b) => a <= b, equals: (a, b) => a === b };

/** The tone of the first of a view's highlight rules a number meets, as 'good' or 'bad'; null where it meets none. */
export function toneOf(highlights, number) {
  if (typeof number !== 'number') return null;
  return highlights?.rules?.find((rule) => TESTS[rule.test]?.(number, rule.value))?.tone ?? null;
}

/**
 * What a view's highlights do to a cell of its grid, or null when it has none: a bar as
 * long as the number's share of the largest in its column, a shade from the column's lowest
 * to its highest, and the colour of the first rule the number meets. Totals take no bar or
 * shade, being out of scale with what they add up.
 */
export function highlighter(highlights, rows, columns, value, total) {
  if (!highlights || (!highlights.bars && !highlights.shades && !highlights.rules?.length)) return null;
  // The range of each column's numbers, totals left out.
  const ranges = columns.map((columnTuple, c) => {
    let low = Infinity;
    let high = -Infinity;
    let widest = 0;
    if (!total(columnTuple)) {
      rows.forEach((rowTuple, r) => {
        const number = value(r, c);
        if (typeof number !== 'number' || total(rowTuple)) return;
        low = Math.min(low, number);
        high = Math.max(high, number);
        widest = Math.max(widest, Math.abs(number));
      });
    }
    return { low, high, widest };
  });
  return (td, r, c, number) => {
    if (typeof number !== 'number') return;
    const { low, high, widest } = ranges[c];
    const measured = Number.isFinite(low) && !total(rows[r]) && !total(columns[c]);
    if (highlights.bars && measured && widest > 0) {
      td.classList.add('data-bar');
      td.style.setProperty('--bar', `${((Math.abs(number) / widest) * 100).toFixed(1)}%`);
    }
    if (highlights.shades && measured && high > low) {
      td.classList.add('shade');
      td.style.setProperty('--shade', ((number - low) / (high - low)).toFixed(3));
    }
    const tone = toneOf(highlights, number);
    if (tone) td.classList.add(`tone-${tone}`);
  };
}

/** The pivot grid: drawing and sizing it, moving about its headings, finding in it, highlighting it, and its menus. */
export const grid = {
  drawGrid() {
    const result = this.result;
    const [columns = { tuples: [[]] }, rows = { tuples: [[]] }] = result.axes;
    const columnTuples = columns.tuples.length ? columns.tuples : [];
    const rowTuples = rows.tuples.length ? rows.tuples : [];
    const columnDepth = columnTuples[0]?.length ?? 0;
    const rowDepth = rowTuples[0]?.length ?? 0;
    if (!columnTuples.length || (result.axes.length > 1 && !rowTuples.length)) {
      const hidden = ['rows', 'columns'].filter((axis) => this.layout.nonEmpty?.[axis]);
      mount(this.output, empty('grid', 'Nothing to show', hidden.length ? `Every cell is empty. Show the empty ${hidden.join(' and ')} to see them.` : 'The query chose no members.'));
      mount(this.statusLine);
      this.grid = null;
      return;
    }
    const total = (tuple) => tuple.some((m) => m.type === 'consolidated');
    // Where each row's and column's members stand in the outline the layout opens, each
    // opened there alone.
    const rowPlaces = outlined(this.layout.rows, this.dims, rowTuples);
    const columnPlaces = outlined(this.layout.columns, this.dims, columnTuples);
    // The attributes shown beside each dimension's members, where the layout asks for them.
    const shownAttributes = (place, tuple) => (tuple ?? []).map((m) => this.entryOf(place, m.dimension)?.attributes ?? []);
    const rowAttributes = shownAttributes('rows', rowTuples[0]);
    const columnAttributes = shownAttributes('columns', columnTuples[0]);
    const rowWidth = rowAttributes.reduce((sum, attributes) => sum + 1 + attributes.length, 0);
    const headRows = columnAttributes.reduce((sum, attributes) => sum + 1 + attributes.length, 0);
    const table = h('table', { class: 'pivot', role: 'grid', 'aria-rowcount': rowTuples.length + headRows, 'aria-colcount': columnTuples.length + rowWidth });
    const head = h('thead');
    // The column the rows are put in order by: its heading says so to a screen reader, and
    // shows it by an arrow.
    const sort = this.layout.sort;
    const sortedBy = sort ? columnTuples.findIndex((tuple) => tuple.length === sort.tuple.length
      && tuple.every((m, i) => m.dimension === sort.tuple[i][0] && m.name === sort.tuple[i][1])) : -1;
    const sorted = (level, span, index) => level === columnDepth - 1 && span === 1 && index === sortedBy;
    for (let level = 0; level < columnDepth; level += 1) {
      const groups = [];
      // Neighbouring columns that share this member and those above it share one heading.
      for (let index = 0; index < columnTuples.length;) {
        const key = (i) => columnTuples[i].slice(0, level + 1).map((m) => m.unique_name).join('|');
        let span = 1;
        while (index + span < columnTuples.length && key(index + span) === key(index)) span += 1;
        groups.push([columnTuples[index][level], span, index]);
        index += span;
      }
      const tr = h('tr');
      if (level === 0) {
        // A heading over each column of row headings: its dimension, or an attribute shown.
        const labels = rowTuples[0]?.flatMap((m, index) => [m.dimension, ...rowAttributes[index]]) ?? [];
        tr.append(...(labels.length ? labels : ['']).map((label) => h('th', { class: 'corner', rowspan: headRows, scope: 'col' }, label)));
      }
      // Each heading knows the columns beneath it, to choose them when it is clicked.
      const across = (span, index) => ({ c1: index, c2: index + span - 1 });
      for (const [m, span, index] of groups) {
        const by = sorted(level, span, index);
        tr.append(h('th', {
          scope: 'col',
          colspan: span,
          tabindex: '-1',
          class: `col-head ${m.type === 'consolidated' ? 'total' : ''}`,
          'aria-sort': by ? (sort.descending ? 'descending' : 'ascending') : null,
          dataset: { ...across(span, index), ...this.headingOf('columns', m, columnPlaces[index]?.[level]) },
        }, this.memberLabel('columns', m, columnPlaces[index]?.[level]), by ? h('span', { class: 'sort-mark', 'aria-hidden': 'true' }, icon(sort.descending ? 'down' : 'up', 12)) : null));
      }
      head.append(tr);
      for (const attribute of columnAttributes[level]) {
        head.append(h('tr', { class: 'attribute-row-head' }, groups.map(([m, span, index]) => h('th', {
          scope: 'col',
          colspan: span,
          class: 'col-attribute',
          title: attribute,
          dataset: across(span, index),
        }, this.attributeValue(m, attribute)))));
      }
    }
    const body = h('tbody');
    const columnCount = columnTuples.length;
    const value = (r, c) => result.cells[r * columnCount + c];
    // Only the rows in view are drawn, between two rows as tall as those above and below them.
    // Each is made afresh at its height: Chrome holds a table row to the height it was laid
    // out at when only its cell's height changes.
    const spacer = (height = 0) => {
      const cell = h('td', { colspan: rowWidth + columnCount });
      cell.style.height = `${height}px`;
      return h('tr', { class: 'spacer', 'aria-hidden': 'true' }, cell);
    };
    const grid = {
      table,
      body,
      rowTuples,
      columnTuples,
      rowPlaces,
      columnCount,
      rowWidth,
      headRows,
      rowAttributes,
      total,
      value,
      // What a cell shows, as a calculated member's format writes it or as the view's does.
      text: (r, c) => shownText(result, r * columnCount + c, this.layout.format),
      // Totals take a value to spread where the user may write and nothing in the slicer is calculated.
      spreads: this.mayWrite() && !(result.slicer ?? []).some((m) => m.type === 'calculated'),
      highlight: highlighter(this.layout.highlights, rowTuples, columnTuples, value, total),
      spacer,
      top: spacer(),
      bottom: spacer(),
      rows: new Map(),
      from: 0,
      to: 0,
      height: 32,
      lefts: [],
    };
    this.grid = grid;
    body.append(grid.top, grid.bottom);
    const keys = [
      ...(rowTuples[0] ?? []).flatMap((m, index) => [`row:${m.dimension}`, ...rowAttributes[index].map((attribute) => `row:${m.dimension}:${attribute}`)]),
      ...columnTuples.map((tuple) => `col:${tuple.map((m) => m.unique_name).join('|')}`),
    ];
    table.append(h('colgroup', {}, keys.map((key) => h('col', { dataset: { key } }))), head, body);
    const summary = h('span', { class: 'selection-summary', 'aria-live': 'polite' });
    const previous = this.selection;
    this.selection = new Selection(table, {
      rows: rowTuples.length,
      columns: columnCount,
      value,
      changed: (chosen) => mount(summary, ...this.summarize(chosen)),
      cell: (r, c) => grid.rows.get(r)?.children[rowWidth + c],
      reveal: (r, c) => {
        const cell = this.revealRow(r)?.children[rowWidth + c];
        cell?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
        return cell;
      },
      drawn: () => grid.rows,
    });
    table.addEventListener('contextmenu', (event) => {
      const heading = event.target.closest('th[data-member]');
      if (heading) {
        event.preventDefault();
        this.headingMenu(heading, { x: event.clientX, y: event.clientY });
        return;
      }
      const cell = event.target.closest('td');
      if (!cell) return;
      event.preventDefault();
      const [r, c] = [Number(cell.dataset.r), Number(cell.dataset.c)];
      if (!this.selection.has(r, c)) this.selection.choose({ r1: r, r2: r, c1: c, c2: c });
      this.cellMenu(r, c, { x: event.clientX, y: event.clientY });
    });
    table.addEventListener('keydown', (event) => {
      if (event.target.closest('th')) this.headingKey(event, table);
      else this.moveFocus(event, table);
    });
    table.addEventListener('paste', (event) => {
      const text = event.clipboardData?.getData('text/plain');
      if (!text || event.target.closest('input, textarea, select')) return;
      event.preventDefault();
      this.paste(text);
    });
    table.addEventListener('dblclick', (event) => {
      // A heading opens or closes; a cell that may be written is edited.
      const heading = event.target.closest('th[data-member]');
      if (heading && !event.target.closest('button')) {
        this.openHeading(heading);
        return;
      }
      const cell = event.target.closest('td.editable');
      if (cell) this.edit(cell);
    });
    // Focus on the grid itself, its cell scrolled away and let go, goes back to that cell at a key.
    const scroller = h('div', { class: 'pivot-scroll', tabindex: '-1' }, table);
    scroller.addEventListener('keydown', (event) => {
      if (event.target !== scroller || !['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'PageUp', 'PageDown', 'Home', 'End', 'Enter', 'F2'].includes(event.key)) return;
      event.preventDefault();
      const { r, c } = this.selection.current ?? { r: 0, c: 0 };
      this.selection.activate(r, c);
    });
    let queued = false;
    scroller.addEventListener('scroll', () => {
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        this.updateRows();
      });
    });
    new ResizeObserver(() => this.updateRows()).observe(scroller);
    printsWhole(scroller, () => this.drawWhole(), () => this.updateRows(true));
    grid.scroller = scroller;
    // Drawn again, the grid stays where it was scrolled to.
    const before = this.output.querySelector(':scope > .pivot-scroll');
    const scrolled = before ? [before.scrollTop, before.scrollLeft] : [0, 0];
    mount(this.output, scroller);
    // The first rows are drawn, and, collapsed, the widest of each column's content in any
    // row, for the columns to fit; then the rows as tall as they are drawn.
    this.drawRows(0, Math.min(rowTuples.length, 40));
    const measure = this.measureRow();
    if (measure) body.append(measure);
    this.size(table);
    measure?.remove();
    this.pinHeads(table);
    grid.height = grid.rows.get(0)?.offsetHeight || grid.height;
    [scroller.scrollTop, scroller.scrollLeft] = scrolled;
    this.updateRows(true);
    if (previous?.rows === rowTuples.length && previous.columns === columnCount) this.selection.restore(previous);
    this.focusCell();
    this.refocusHeading(table);
    this.markFound();
    this.drawStatus(summary);
    this.explainRows(this.drawnRows());
  },

  /**
   * Every row, for paper, which has no scrolling: up to PRINTED cells, past which drawing
   * them would hold the page up, and the printout says that only the rows in view are on it.
   */
  drawWhole() {
    const grid = this.grid;
    const whole = grid.rowTuples.length * grid.columnCount <= PRINTED;
    grid.scroller.toggleAttribute('data-part', !whole);
    grid.scroller.dataset.rows = grid.rowTuples.length.toLocaleString();
    if (whole) this.drawRows(0, grid.rowTuples.length);
  },

  /** The places of the rows drawn. */
  drawnRows() {
    const { from, to } = this.grid;
    return Array.from({ length: to - from }, (_, index) => from + index);
  },

  /**
   * Draws the rows in view and some beyond, when scrolling has moved past those drawn; with
   * `again`, whatever is drawn.
   */
  updateRows(again = false) {
    const grid = this.grid;
    if (!grid?.scroller.isConnected) return;
    const { scroller, height } = grid;
    const first = Math.floor(scroller.scrollTop / height);
    const last = Math.ceil((scroller.scrollTop + scroller.clientHeight - grid.table.tHead.offsetHeight) / height);
    if (!again && first >= grid.from && last <= grid.to) return;
    // Beyond what is seen, enough rows that a little scrolling draws nothing new.
    this.drawRows(Math.max(0, first - 30), Math.min(grid.rowTuples.length, Math.max(last, first + 1) + 30));
    this.explainSoon();
  },

  /**
   * Draws the rows from `from` up to `to`: those already drawn stay as they are, the focus
   * with them; the others go, the focus on one passing to the grid itself.
   */
  drawRows(from, to) {
    const grid = this.grid;
    for (const [r, tr] of grid.rows) {
      if (r >= from && r < to) continue;
      if (tr.contains(document.activeElement)) grid.scroller.focus({ preventScroll: true });
      tr.remove();
      grid.rows.delete(r);
    }
    const kept = [...grid.rows.keys()];
    const low = kept.length ? Math.min(...kept) : to;
    const high = kept.length ? Math.max(...kept) + 1 : to;
    for (const [end, height] of [['top', from * grid.height], ['bottom', (grid.rowTuples.length - to) * grid.height]]) {
      const fresh = grid.spacer(height);
      grid[end].replaceWith(fresh);
      grid[end] = fresh;
    }
    const add = (start, end, place) => {
      const rows = document.createDocumentFragment();
      for (let r = start; r < end; r += 1) {
        const tr = this.buildRow(r);
        grid.rows.set(r, tr);
        rows.append(tr);
      }
      place(rows);
    };
    add(from, Math.min(low, to), (rows) => grid.top.after(rows));
    add(Math.max(high, from), to, (rows) => grid.bottom.before(rows));
    grid.from = from;
    grid.to = to;
    // With the cell the keys act on beyond those drawn, Tab still comes into the grid.
    if (!grid.body.querySelector('td[tabindex="0"]')) grid.rows.get(from)?.children[grid.rowWidth]?.setAttribute('tabindex', '0');
    this.selection.paint();
    this.paintFound();
  },

  /** A row of the grid, scrolled into view below the headings and drawn; null past the ends. */
  revealRow(r) {
    const grid = this.grid;
    if (!grid || r < 0 || r >= grid.rowTuples.length) return null;
    const { scroller, height } = grid;
    const top = r * height;
    const below = scroller.clientHeight - grid.table.tHead.offsetHeight;
    if (top < scroller.scrollTop) scroller.scrollTop = top;
    else if (top + height > scroller.scrollTop + below) scroller.scrollTop = top + height - below;
    this.updateRows();
    return grid.rows.get(r) ?? null;
  },

  /**
   * A row of the grid: its headings, pinned beside the cells and blank where they repeat the
   * row above, so that groups read clearly; then its cells.
   */
  buildRow(r) {
    const grid = this.grid;
    const { rowTuples, rowAttributes, total, lefts } = grid;
    const tuple = rowTuples[r];
    const tr = h('tr', { class: total(tuple) ? 'total-row' : '', 'aria-rowindex': grid.headRows + r + 1, dataset: { r } });
    const pin = (th) => {
      const place = tr.children.length;
      if (lefts.length) {
        th.style.left = `${lefts[place] ?? 0}px`;
        th.classList.add('pinned');
      }
      tr.append(th);
      return th;
    };
    tuple.forEach((m, level) => {
      const at = grid.rowPlaces[r]?.[level];
      const shares = (other, index) => other && other.slice(0, level + 1).every((o, i) => o.unique_name === tuple[i].unique_name && grid.rowPlaces[index]?.[i]?.path === grid.rowPlaces[r]?.[i]?.path);
      const same = shares(rowTuples[r - 1], r - 1);
      // A heading stands for its row and the rows beneath that repeat it.
      let last = r;
      while (!same && shares(rowTuples[last + 1], last + 1)) last += 1;
      const down = { r1: r, r2: last };
      const heading = pin(this.indent(h('th', {
        scope: 'row',
        tabindex: '-1',
        class: `row-head ${m.type === 'consolidated' ? 'total' : ''}`,
        dataset: same ? down : { ...down, ...this.headingOf('rows', m, at), level },
      }, same ? '' : this.memberLabel('rows', m, at)), at));
      // Beneath each row's first heading, the handle that sets the height of every row.
      if (level === 0) heading.append(h('span', { class: 'row-resize', role: 'separator', 'aria-orientation': 'horizontal', 'aria-label': 'Drag to change the height of every row', title: 'Drag to change the height of every row' }));
      for (const attribute of rowAttributes[level] ?? []) {
        pin(h('th', { scope: 'row', tabindex: '-1', class: 'row-attribute', title: attribute, dataset: down }, same ? '' : this.attributeValue(m, attribute)));
      }
    });
    for (let c = 0; c < grid.columnCount; c += 1) tr.append(this.buildCell(r, c));
    return tr;
  },

  /** A cell of the grid: its value as the query or the view formats it, highlighted, and marked for writing and comments. */
  buildCell(r, c) {
    const { value, text, total, rowTuples, columnTuples, highlight } = this.grid;
    const shown = value(r, c);
    const number = typeof shown === 'number';
    const current = this.selection?.current ?? { r: 0, c: 0 };
    const td = h('td', {
      class: `${total(rowTuples[r]) || total(columnTuples[c]) ? 'total' : ''} ${number ? 'n' : 's'} ${number && shown < 0 ? 'negative' : ''}`,
      tabindex: current.r === r && current.c === c ? '0' : '-1',
      dataset: { r, c },
    }, text(r, c));
    this.markWriting(td, r, c);
    // Compared with the stored values, a value a sandbox changes has the stored one before it.
    const stored = this.compare ? this.storedText(r, c) : null;
    if (stored) td.prepend(h('del', { class: 'was' }, stored));
    highlight?.(td, r, c, shown);
    if (this.commented?.size) this.markComments(td, r, c);
    return td;
  },

  /**
   * A row holding, in each column, the widest of what the column shows in any row, for the
   * columns to fit to though most rows are not drawn: collapsed, it takes no room. None when
   * every row is drawn.
   */
  measureRow() {
    const { rowTuples, columnTuples, columnCount, rowAttributes, total, value, rows } = this.grid;
    if (rows.size >= rowTuples.length) return null;
    const widest = (length) => {
      let best = 0;
      let most = -1;
      rowTuples.forEach((_, r) => {
        const size = length(r);
        if (size > most) [best, most] = [r, size];
      });
      return best;
    };
    const cells = [];
    (rowTuples[0] ?? []).forEach((first, level) => {
      const entry = this.entryOf('rows', first.dimension);
      const dim = this.dims.get(first.dimension);
      const label = (m) => (entry?.alias ? dim?.byName.get(m.name)?.attributes?.[entry.alias] : null) || m.name;
      // A name is as wide as its letters and its indent, at about six pixels a letter and three
      // letters a step in.
      const r = widest((row) => String(label(rowTuples[row][level])).length + this.depthOf(this.grid.rowPlaces[row]?.[level]) * 3);
      const at = this.grid.rowPlaces[r]?.[level];
      cells.push(this.indent(h('th', { class: `row-head ${rowTuples[r][level].type === 'consolidated' ? 'total' : ''}` }, this.memberLabel('rows', rowTuples[r][level], at)), at));
      for (const attribute of rowAttributes[level] ?? []) {
        const longest = widest((row) => String(this.attributeValue(rowTuples[row][level], attribute)).length);
        cells.push(h('th', { class: 'row-attribute' }, this.attributeValue(rowTuples[longest][level], attribute)));
      }
    });
    for (let c = 0; c < columnCount; c += 1) {
      // The widest number is the largest, or the most negative; the widest text the longest.
      let [large, low, long] = [0, 0, 0];
      let [most, least, longest] = [-1, 0, -1];
      rowTuples.forEach((_, r) => {
        const each = value(r, c);
        if (typeof each === 'number') {
          if (Math.abs(each) > most) [large, most] = [r, Math.abs(each)];
          if (each < least) [low, least] = [r, each];
        } else if (each !== null && each !== undefined && String(each).length > longest) {
          [long, longest] = [r, String(each).length];
        }
      });
      const own = (row) => String(this.grid.text(row, c));
      const [r, text] = [large, low, long]
        .map((row) => [row, own(row)])
        .reduce((a, b) => (b[1].length > a[1].length ? b : a));
      // Stored values shown beside a sandbox's widen their column by as much.
      let [beside, was] = [r, ''];
      if (this.compare && this.stored) {
        rowTuples.forEach((_, row) => {
          const stored = this.storedText(row, c);
          if (stored && stored.length + own(row).length > was.length + own(beside).length) [beside, was] = [row, stored];
        });
      }
      const wider = was.length + own(beside).length > text.length;
      const widest = wider ? [h('del', { class: 'was' }, was), own(beside)] : [text];
      cells.push(h('td', { class: total(rowTuples[wider ? beside : r]) || total(columnTuples[c]) ? 'total' : '' }, ...widest));
    }
    return h('tr', { class: 'measure', 'aria-hidden': 'true' }, cells);
  },

  /** The line beneath the grid: its size, how long the query took, and how it is shown. */
  drawStatus(summary) {
    const grid = this.grid;
    const cells = grid.rowTuples.length * grid.columnCount;
    grid.legend = h('span', { class: 'legend-input muted', title: 'Tinted cells take a value: type into one, or paste', hidden: !this.writable.size }, 'Can be written');
    grid.invalidLegend = h('span', { class: 'legend-invalid muted', title: 'Underlined values break a check of the cube. Point at one to read what it says.', hidden: !this.invalid.size }, 'Breaks a check');
    mount(this.statusLine,
      h('span', {}, `${counted(grid.rowTuples.length, 'row')} × ${counted(grid.columnCount, 'column')}`),
      h('span', { class: 'muted' }, `${counted(cells, 'cell')} in ${Math.round(this.took)} ms`),
      grid.legend,
      grid.invalidLegend,
      this.differing ? h('span', {
        class: 'legend-sandboxed muted',
        title: `Marked cells hold values the sandbox ${this.sandbox.name} changes. ${this.compare ? 'The stored value is struck through beside each.' : 'Show the stored values beside them from the sandbox menu.'}`,
      }, `${this.differing.toLocaleString()} changed in ${this.sandbox.name}`) : null,
      this.layout.format?.scale ? h('span', { class: 'badge' }, SCALES[this.layout.format.scale][1]) : null,
      this.holds.size ? h('span', { class: 'badge held-badge', title: 'Spreads leave held cells as they are. Release them from a cell\'s menu.' }, `${this.holds.size.toLocaleString()} held`) : null,
      this.layout.sort ? h('span', { class: 'badge accent' }, `Sorted ${this.layout.sort.descending ? 'largest' : 'smallest'} first`) : null,
      this.layout.top ? h('span', { class: 'badge accent' }, `The ${this.layout.top.count.toLocaleString()} ${this.layout.top.bottom ? 'smallest' : 'largest'} by ${this.layout.top.tuple.map(([, name]) => name).join(' · ')}`) : null,
      summary);
  },

  /** Says, for more than one cell chosen, how many there are and what their numbers add to. */
  summarize(chosen) {
    if (chosen.count < 2) return [];
    const figure = (label, value) => h('span', {}, h('span', { class: 'muted' }, `${label} `), formatValue(value, this.layout.format));
    return [
      h('span', {}, h('span', { class: 'muted' }, 'Count '), chosen.count.toLocaleString()),
      ...(chosen.numbers ? [
        figure('Sum', chosen.sum),
        figure('Average', chosen.average),
        figure('Min', chosen.min),
        figure('Max', chosen.max),
      ] : []),
    ];
  },

  /**
   * How far a row's heading stands in, by where its member stands in the outline: a step for
   * each member opened above it, the styles drawing a line down from each to the last member
   * beneath it. Rows put in order by their values stand in line.
   */
  indent(heading, at) {
    const depth = this.depthOf(at);
    // Set as a property: written into the style attribute, it would go when the heading is pinned.
    if (depth) heading.style.setProperty('--depth', String(depth));
    return heading;
  },

  depthOf(at) {
    return at && !this.layout.sort && !this.layout.top ? Math.min(at.depth, 12) : 0;
  },

  /**
   * A member's heading: with a button that opens or closes it where it has children, there
   * alone, `at` being where it stands in the outline. Down the rows, its heading stands it in
   * by its depth, its leaves in line with the members that open. Across the columns, an indent
   * would only widen its column and crowd its name: the buttons on the members that open show
   * the outline there.
   */
  memberLabel(place, m, at) {
    const entry = this.layout[place]?.find((candidate) => candidate.dimension === m.dimension);
    const element = this.dims.get(m.dimension)?.byName.get(m.name);
    const label = h('span', { class: 'member-label' });
    if (entry && at && element?.children.length) {
      // Not a stop for Tab: the heading itself takes the keys that open and close it.
      label.append(h('button', {
        class: 'twisty',
        tabindex: '-1',
        'aria-expanded': String(at.opened),
        'aria-label': `${at.opened ? 'Close' : 'Open'} ${m.name}`,
        onclick: () => this.toggle(place, m.dimension, at.path),
      }, icon(at.opened ? 'minus' : 'plus', 12)));
    } else if (place === 'rows' && this.depthOf(at)) {
      label.append(h('span', { class: 'twisty' }));
    }
    const alias = entry?.alias ? element?.attributes?.[entry.alias] : null;
    label.append(h('span', { title: alias ? m.name : null }, alias || m.name));
    return label;
  },

  /** An attribute of a member, as text. */
  attributeValue(m, attribute) {
    const value = this.dims.get(m.dimension)?.byName.get(m.name)?.attributes?.[attribute];
    if (value === undefined || value === null) return '';
    return typeof value === 'number' ? formatNumber(value) : String(value);
  },

  /**
   * Sizes the grid: every column to its content and headings, unless given a width by hand,
   * and every row to the height given by hand, if one is. Then puts the handles that change
   * them on the headings.
   */
  size(table) {
    const geometry = this.layout.geometry ?? {};
    const cols = [...table.querySelectorAll('col')];
    if (geometry.row_height) table.style.setProperty('--row-height', `${geometry.row_height}px`);
    // Measured as laid out by their content, then held, so that changing one moves no other.
    const fitted = cols.map((col) => Math.ceil(col.getBoundingClientRect().width));
    const widths = cols.map((col, index) => geometry.widths?.[col.dataset.key] ?? fitted[index]);
    const apply = () => {
      cols.forEach((col, index) => {
        col.style.width = `${widths[index]}px`;
      });
      table.style.width = `${widths.reduce((sum, width) => sum + width, 0)}px`;
    };
    apply();
    table.classList.add('sized');
    const keep = (change) => {
      this.layout.geometry = { ...(this.layout.geometry ?? {}), ...change(this.layout.geometry ?? {}) };
      localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
      this.drawToolbar();
    };
    // Follows the pointer wherever it goes until it is let go.
    const follow = (event, value, move, end) => {
      event.preventDefault();
      event.stopPropagation();
      const origin = { x: event.clientX, y: event.clientY, value };
      document.body.classList.add('resizing');
      const moved = (next) => move(origin, next);
      const done = () => {
        window.removeEventListener('pointermove', moved);
        window.removeEventListener('pointerup', done);
        window.removeEventListener('pointercancel', done);
        document.body.classList.remove('resizing');
        end();
      };
      window.addEventListener('pointermove', moved);
      window.addEventListener('pointerup', done);
      window.addEventListener('pointercancel', done);
    };
    // A handle on the right edge of the heading of each column.
    const headRows = [...table.tHead.rows];
    const dataHeads = [...headRows[headRows.length - 1].cells].filter((cell) => !cell.classList.contains('corner'));
    const corners = [...table.tHead.querySelectorAll('th.corner')];
    const columnHeads = [...(this.grid.rowWidth ? corners : []), ...dataHeads];
    if (columnHeads.length === cols.length) {
      columnHeads.forEach((cell, index) => {
        const handle = h('span', { class: 'col-resize', role: 'separator', 'aria-orientation': 'vertical', 'aria-label': 'Drag to change the width of the column', title: 'Drag to change the width; double-click to fit it' });
        cell.append(handle);
        handle.addEventListener('pointerdown', (event) => {
          if (event.button !== 0) return;
          follow(event, widths[index], (origin, next) => {
            widths[index] = Math.max(40, Math.round(origin.value + next.clientX - origin.x));
            apply();
          }, () => {
            keep((geometry) => ({ widths: { ...(geometry.widths ?? {}), [cols[index].dataset.key]: widths[index] } }));
            this.pinHeads(table);
          });
        });
        handle.addEventListener('dblclick', (event) => {
          event.stopPropagation();
          widths[index] = fitted[index];
          apply();
          keep((geometry) => {
            const kept = { ...(geometry.widths ?? {}) };
            delete kept[cols[index].dataset.key];
            return { widths: kept };
          });
          this.pinHeads(table);
        });
      });
    }
    // The handle beneath each row's first heading sets the height of every row.
    table.addEventListener('pointerdown', (event) => {
      if (event.button !== 0 || !event.target.closest('.row-resize')) return;
      let height = this.grid.height;
      follow(event, height, (origin, next) => {
        height = Math.max(22, Math.min(120, Math.round(origin.value + next.clientY - origin.y)));
        table.style.setProperty('--row-height', `${height}px`);
      }, () => {
        keep(() => ({ row_height: height }));
        this.grid.height = this.grid.rows.get(this.grid.from)?.offsetHeight || height;
        this.pinHeads(table);
        this.updateRows(true);
      });
    });
  },

  /**
   * Keeps the heading rows and the row headings in view as the grid scrolls: each heading
   * row below the one above it, and each row heading column beside those before it.
   */
  pinHeads(table) {
    const grid = this.grid;
    let top = 0;
    for (const row of table.tHead.rows) {
      for (const cell of row.cells) {
        if (!cell.classList.contains('corner')) cell.style.top = `${top}px`;
      }
      top += row.offsetHeight;
    }
    // Each column of row headings stands as far from the left as those before it are wide.
    const widths = [...table.querySelectorAll('col')].slice(0, grid.rowWidth).map((col) => col.getBoundingClientRect().width);
    grid.lefts = widths.map((_, index) => widths.slice(0, index).reduce((sum, width) => sum + width, 0));
    table.tHead.querySelectorAll('th.corner').forEach((cell, index) => {
      cell.style.left = `${grid.lefts[index] ?? 0}px`;
    });
    for (const row of grid.rows.values()) {
      [...row.cells].slice(0, grid.rowWidth).forEach((cell, index) => {
        cell.style.left = `${grid.lefts[index]}px`;
        cell.classList.add('pinned');
      });
    }
    // A cell scrolled to, by the keys or the focus, stays clear of the headings pinned above and beside it.
    grid.scroller.style.scrollPaddingTop = `${top}px`;
    grid.scroller.style.scrollPaddingLeft = `${widths.reduce((sum, width) => sum + width, 0)}px`;
  },

  moveFocus(event, table) {
    const cell = event.target.closest('td');
    if (!cell || event.defaultPrevented) return;
    const r = Number(cell.dataset.r);
    const c = Number(cell.dataset.c);
    const moves = { ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
    if (event.key === 'F2' && event.shiftKey) {
      event.preventDefault();
      this.openComments(r, c);
      return;
    }
    if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
      event.preventDefault();
      this.cellMenu(r, c, cell);
      return;
    }
    const takes = cell.classList.contains('editable') || cell.classList.contains('spreadable');
    const typing = !event.ctrlKey && !event.metaKey && !event.altKey && (event.key.length === 1 || event.key === 'F2');
    if (!takes && typing && event.key !== ' ' && this.result) {
      event.preventDefault();
      const { key } = event;
      // A row not yet asked about is asked about first; the key then does what it would have.
      this.explainRows([r]).then(() => {
        if (cell.classList.contains('editable') || cell.classList.contains('spreadable')) {
          this.edit(cell, key === 'F2' ? undefined : key);
        } else if (this.toldWhy !== cell) {
          // Once for each cell, not at every key.
          notify(this.whyNot(r, c));
          this.toldWhy = cell;
        }
      });
      return;
    }
    if (takes && !event.ctrlKey && !event.metaKey && !event.altKey) {
      if (event.key === 'Enter' || event.key === 'F2') {
        event.preventDefault();
        this.edit(cell);
        return;
      }
      if ((event.key === 'Delete' || event.key === 'Backspace') && cell.classList.contains('editable')) {
        event.preventDefault();
        const chosen = this.selection.cells();
        this.clear(chosen.length ? chosen : [[r, c]]);
        return;
      }
      if (event.key.length === 1) {
        event.preventDefault();
        this.edit(cell, event.key);
        return;
      }
    }
    // Page Up and Down move by the rows in view; Home and End to the row's ends, and with
    // Ctrl to the grid's.
    const grid = this.grid;
    const page = Math.max(1, Math.floor((grid.scroller.clientHeight - table.tHead.offsetHeight) / grid.height) - 1);
    const [lastRow, lastColumn] = [grid.rowTuples.length - 1, grid.columnCount - 1];
    const command = event.ctrlKey || event.metaKey;
    const move = moves[event.key];
    const jump = move ? [r + move[0], c + move[1]] : {
      PageUp: [Math.max(0, r - page), c],
      PageDown: [Math.min(lastRow, r + page), c],
      Home: command ? [0, 0] : [r, 0],
      End: command ? [lastRow, lastColumn] : [r, lastColumn],
    }[event.key];
    if (!jump || event.altKey) return;
    const next = this.selection.reveal(...jump);
    // Past the first column or row, the keys go on into the headings.
    const heading = next ? null
      : event.key === 'ArrowLeft' ? [...cell.parentElement.cells].filter((other) => other.tagName === 'TH').pop()
        : event.key === 'ArrowUp' ? this.columnHeading(table, table.tHead.rows.length - 1, c) : null;
    const target = next ?? heading;
    if (!target) return;
    event.preventDefault();
    if (next) {
      cell.tabIndex = -1;
      next.tabIndex = 0;
    }
    target.focus();
  },

  /** The heading, in one of the heading rows, over a column of cells. */
  columnHeading(table, row, c) {
    return [...(table.tHead.rows[row]?.cells ?? [])].find((cell) => cell.dataset.c1 !== undefined && Number(cell.dataset.c1) <= c && Number(cell.dataset.c2) >= c);
  },

  /** What a heading stands for, kept on it to act on: its member, dimension and axis, and where it stands in the outline. */
  headingOf(place, m, at) {
    return { member: m.name, dimension: m.dimension, place, path: at?.path ?? '' };
  },

  /**
   * The keys of a heading. The arrows move among the headings and back into the cells;
   * Enter, Space, + and - open and close its member, * opens everything beneath it, and the
   * menu key or Shift F10 opens its menu.
   */
  headingKey(event, table) {
    const heading = event.target.closest('th');
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const member = heading.dataset.member;
    const row = heading.parentElement;
    const inHead = row.parentElement === table.tHead;
    const opened = () => this.entryOf(heading.dataset.place, heading.dataset.dimension)?.expanded.includes(heading.dataset.path);
    let target = null;
    switch (event.key) {
      case 'ArrowLeft':
      case 'ArrowRight': {
        const step = event.key === 'ArrowRight' ? 1 : -1;
        const sibling = row.cells[heading.cellIndex + step];
        if (sibling?.tagName === 'TH' && !sibling.classList.contains('corner')) target = sibling;
        else if (!inHead && step > 0) target = this.selection.reveal(Number(row.dataset.r), 0);
        break;
      }
      case 'ArrowUp':
      case 'ArrowDown': {
        const step = event.key === 'ArrowDown' ? 1 : -1;
        if (inHead) {
          const c = Number(heading.dataset.c1);
          const level = row.sectionRowIndex + step;
          target = level < table.tHead.rows.length ? this.columnHeading(table, level, c) : this.selection.reveal(0, c);
        } else {
          target = this.revealRow(Number(row.dataset.r) + step)?.cells[heading.cellIndex];
        }
        break;
      }
      case 'Enter':
      case ' ':
        if (member) this.openHeading(heading);
        break;
      case '+':
        if (member && !opened()) this.openHeading(heading);
        break;
      case '-':
        if (member && opened()) this.openHeading(heading);
        break;
      case '*':
        if (member) this.openBeneath(heading, true);
        break;
      case 'ContextMenu':
      case 'F10':
        if (event.key === 'F10' && !event.shiftKey) return;
        if (member) this.headingMenu(heading, heading);
        break;
      default:
        return;
    }
    event.preventDefault();
    target?.focus();
  },

  /** Opens a heading's member where it stands, or closes it, keeping the keys on the heading. */
  openHeading(heading) {
    const { place, dimension, member, path } = heading.dataset;
    if (!path || !this.dims.get(dimension)?.byName.get(member)?.children.length || !this.entryOf(place, dimension)) return;
    this.keepHeading(heading);
    this.toggle(place, dimension, path);
  },

  /** Opens, or closes, a heading's member where it stands and everything beneath it there. */
  openBeneath(heading, open) {
    const { place, dimension, path } = heading.dataset;
    if (!path) return;
    const within = pathsBeneath(this.dims.get(dimension), [namesOf(path)]);
    this.keepHeading(heading);
    this.setOpen(place, dimension, (expanded) => (open ? [...new Set([...expanded, ...within])] : expanded.filter((each) => !within.includes(each))));
  },

  /** Keeps the keys on a heading through the grid being drawn again, if they were on it. */
  keepHeading(heading) {
    if (heading.contains(document.activeElement)) this.headingAfter = { ...heading.dataset };
  },

  refocusHeading(table) {
    const kept = this.headingAfter;
    this.headingAfter = null;
    if (!kept) return;
    // A row's heading may lie beyond the rows drawn: the first row it heads is brought into view.
    const at = (row) => row.some((place) => place?.path === kept.path);
    if (kept.place === 'rows') this.revealRow(kept.path ? this.grid.rowPlaces.findIndex(at) : this.grid.rowTuples.findIndex((tuple) => tuple.some((m) => m.name === kept.member && m.dimension === kept.dimension)));
    const same = [...table.querySelectorAll('th[data-member]')].find((th) => th.dataset.member === kept.member && th.dataset.dimension === kept.dimension && th.dataset.place === kept.place && th.dataset.path === kept.path);
    same?.focus();
  },

  /** The menu of a member's heading: opening and closing, keeping and removing, copying. */
  headingMenu(heading, at) {
    const { place, dimension, member: name } = heading.dataset;
    const dim = this.dims.get(dimension);
    const entry = this.entryOf(place, dimension);
    const element = dim?.byName.get(name);
    const kids = element && heading.dataset.path ? this.childrenOf(dim, name).length : 0;
    const opened = entry?.expanded.includes(heading.dataset.path);
    const chosen = entry?.members.includes(name);
    menu([
      { heading: name },
      entry && kids ? { label: opened ? 'Close it' : 'Open it', icon: opened ? 'minus' : 'plus', action: () => this.openHeading(heading) } : null,
      entry && kids ? { label: 'Open everything beneath it', icon: 'expand', action: () => this.openBeneath(heading, true) } : null,
      entry && opened ? { label: 'Close everything beneath it', icon: 'collapse', action: () => this.openBeneath(heading, false) } : null,
      entry && element ? '-' : null,
      entry && element ? {
        label: `Keep only ${name}`,
        icon: 'filter',
        action: () => this.change((layout) => {
          layout[place].find((candidate) => candidate.dimension === dimension).members = [name];
          layout.sort = null;
          layout.top = null;
        }),
      } : null,
      entry && element ? {
        label: `Remove ${name}`,
        icon: 'eyeOff',
        // A member shown because its parent is open goes with its parent; one chosen can go.
        disabled: !chosen || entry.members.length < 2,
        action: () => this.change((layout) => {
          const target = layout[place].find((candidate) => candidate.dimension === dimension);
          target.members = target.members.filter((other) => other !== name);
        }),
      } : null,
      entry && element ? { label: 'Add a calculation…', icon: 'sigma', action: () => this.addCalculation(place, dimension, name) } : null,
      this.calculated(dimension, name) ? {
        label: 'Remove the calculation',
        icon: 'trash',
        action: () => this.change((layout) => {
          layout.calculations = layout.calculations.filter((each) => !(each.dimension === dimension && each.name === name));
        }),
      } : null,
      '-',
      { label: 'Copy the name', icon: 'copy', action: () => copyText(name) },
      { label: 'Copy its MDX name', icon: 'code', action: () => copyText(member(dimension, name)) },
      entry ? { label: `Choose members of ${dimension}…`, icon: 'check', action: () => this.pick(dimension) } : null,
    ], at);
  },

  cellMenu(r, c, at) {
    const [columns, rows] = this.result.axes;
    const rowTuple = rows?.tuples[r] ?? [];
    const columnTuple = columns.tuples[c] ?? [];
    // The members of the cell's row that open, each where it stands there.
    const rowOpen = rowTuple.map((m, level) => ({ m, at: this.grid?.rowPlaces[r]?.[level] }))
      .filter(({ m, at: place }) => place && this.dims.get(m.dimension)?.byName.get(m.name)?.children.length);
    const chosen = this.selection?.cells().length ?? 0;
    menu([
      { heading: [...rowTuple, ...columnTuple].map((m) => m.name).join(' · ') },
      { label: 'Sort rows by this column, largest first', icon: 'down', disabled: !this.layout.rows.length, action: () => this.sortBy(columnTuple, true) },
      { label: 'Sort rows by this column, smallest first', icon: 'chevron', disabled: !this.layout.rows.length, action: () => this.sortBy(columnTuple, false) },
      { label: 'Keep only the largest or smallest rows by it…', icon: 'filter', disabled: !this.layout.rows.length, action: () => this.keepTop(columnTuple) },
      '-',
      ...rowOpen.map(({ m, at: place }) => ({ label: `${place.opened ? 'Close' : 'Open'} ${m.name}`, icon: place.opened ? 'minus' : 'plus', action: () => this.toggle('rows', m.dimension, place.path) })),
      this.writable.has(`${r},${c}`)
        ? { label: 'Change the value', icon: 'edit', action: () => this.edit(this.output.querySelector(`td[data-r="${r}"][data-c="${c}"]`)) }
        : null,
      this.mayWrite() && this.coordinate(r, c) && [...rowTuple, ...columnTuple].some((m) => m.type === 'consolidated')
        ? { label: 'Spread a value over its leaves…', icon: 'sigma', action: () => this.spread(r, c) }
        : null,
      this.mayWrite() && this.coordinate(r, c)
        ? (this.holds.has(this.key(r, c))
          ? { label: 'Release the hold', icon: 'lock', action: () => this.hold(r, c, false) }
          : { label: 'Hold it, as spreads go', icon: 'lock', action: () => this.hold(r, c, true) })
        : null,
      this.holds.size ? { label: `Release every hold (${this.holds.size.toLocaleString()})`, icon: 'lock', action: () => this.hold(null, null, false) } : null,
      { label: 'Explain this value', icon: 'info', action: () => this.explain(rowTuple, columnTuple) },
      this.coordinate(r, c) ? { label: this.commentsAt(r, c).length ? `Comments (${this.commentsAt(r, c).length})…` : 'Comment on it…', icon: 'edit', action: () => this.openComments(r, c) } : null,
      this.coordinate(r, c) ? { label: 'Who changed it, and when…', icon: 'clock', action: () => this.openHistory(r, c) } : null,
      this.coordinate(r, c) && typeof this.valueAt(r, c) === 'number' && ![...rowTuple, ...columnTuple].some((m) => m.type === 'consolidated')
        ? { label: this.detailed.has(`${r},${c}`) ? `Detail (${counted(this.detailed.get(`${r},${c}`), 'line')})…` : 'Break it down into lines…', icon: 'bars', action: () => this.openCell(r, c, 'detail') }
        : null,
      [...rowTuple, ...columnTuple].some((m) => m.type === 'consolidated')
        ? { label: 'Show the leaves beneath', icon: 'level', action: () => this.drillToLeaves(rowTuple, columnTuple) }
        : null,
      '-',
      { label: chosen > 1 ? `Copy the ${chosen.toLocaleString()} cells` : 'Copy the value', icon: 'copy', action: () => copyText(this.selection.text()) },
      { label: 'Copy with headings', icon: 'grid', action: () => copyText(this.textWithHeadings()) },
      { label: 'Copy as Excel formulas (DBRW)', icon: 'code', action: () => copyText(this.textWithHeadings(true)) },
      this.mayWrite() ? { label: 'Paste', icon: 'paste', action: () => this.pasteFromClipboard() } : null,
      this.selection.cells().some(([row, column]) => this.writable.has(`${row},${column}`))
        ? { label: chosen > 1 ? 'Clear the cells chosen' : 'Clear the value', icon: 'trash', action: () => this.clear(this.selection.cells()) }
        : null,
    ], at);
  },

  /**
   * Shows what lies behind a total: each consolidated member of its row and column replaced
   * on its axis by the leaves beneath it, and empty rows and columns left out. Undo goes back.
   */
  drillToLeaves(rowTuple, columnTuple) {
    this.change((layout) => {
      for (const [place, tuple] of [['rows', rowTuple], ['columns', columnTuple]]) {
        for (const m of tuple) {
          const dim = this.dims.get(m.dimension);
          const entry = layout[place].find((candidate) => candidate.dimension === m.dimension);
          if (m.type !== 'consolidated' || !entry || !dim) continue;
          const under = beneath([m.name], (name) => this.childrenOf(dim, name));
          entry.members = dim.elements.filter((element) => under.has(element.name) && !element.children.length).map((element) => element.name);
          entry.expanded = [];
        }
      }
      layout.nonEmpty = { rows: true, columns: true };
      layout.sort = null;
      layout.top = null;
    });
  },

  /**
   * The cells chosen, with the headings of their rows and columns, as tab-separated text;
   * with `formulas`, each value is a DBRW formula that the Excel add-in reads live, naming
   * the cube and one element for each dimension, so that it works wherever it is pasted.
   */
  textWithHeadings(formulas = false) {
    if (!this.selection.ranges.length) return '';
    const box = this.selection.ranges.reduce((a, b) => ({ r1: Math.min(a.r1, b.r1), r2: Math.max(a.r2, b.r2), c1: Math.min(a.c1, b.c1), c2: Math.max(a.c2, b.c2) }));
    const table = this.tableOf([box.r1, box.r2], [box.c1, box.c2]);
    if (formulas) {
      const heads = table.length - (box.r2 - box.r1 + 1);
      const quote = (text) => `"${String(text).replaceAll('"', '""')}"`;
      table.slice(heads).forEach((cells, down) => {
        const lead = cells.length - (box.c2 - box.c1 + 1);
        for (let across = 0; across <= box.c2 - box.c1; across += 1) {
          const at = this.coordinate(box.r1 + down, box.c1 + across);
          // A calculated member's cell has no address in the cube, and keeps its value.
          if (at) cells[lead + across] = { formula: `=DBRW(${[this.cube.name, ...at].map(quote).join(',')})` };
        }
      });
    }
    return table.map((cells) => cells.map((cell) => cell?.formula ?? cellText(cell)).join('\t')).join('\n');
  },

  /**
   * A box that finds members among the grid's headings: Enter goes to the next, Shift Enter
   * to the one before, and Escape clears it. A member not shown, beneath one closed, is
   * opened to by Enter.
   */
  buildFinder() {
    const input = h('input', { class: 'input', type: 'search', placeholder: 'Find in the grid', 'aria-label': 'Find a member in the grid', autocomplete: 'off', spellcheck: 'false' });
    const count = h('span', { class: 'search-count', 'aria-live': 'polite' });
    this.findWords = '';
    this.findAt = -1;
    input.addEventListener('input', () => {
      this.findWords = input.value.trim().toLowerCase();
      this.findAt = -1;
      this.markFound();
    });
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        this.findNext(event.shiftKey ? -1 : 1);
      } else if (event.key === 'Escape') {
        event.preventDefault();
        if (input.value) {
          input.value = '';
          this.findWords = '';
          this.markFound();
        } else {
          const { r, c } = this.selection?.current ?? { r: 0, c: 0 };
          this.selection?.activate(r, c);
        }
      }
    });
    return { box: h('div', { class: 'search finder' }, icon('search', 16), input, count), input, count };
  },

  /**
   * Finds the headings that hold the words, and says how many there are: the column headings
   * as drawn, and the row headings from the rows themselves, drawn or not, each where it
   * first heads a group of rows.
   */
  markFound() {
    const words = this.findWords;
    const grid = this.grid;
    this.found = [];
    if (words && grid) {
      const holds = (text) => String(text).toLowerCase().includes(words);
      for (const heading of grid.table.tHead.querySelectorAll('th[data-member]')) {
        if (holds(heading.textContent) || holds(heading.dataset.member)) this.found.push({ heading });
      }
      const labels = (grid.rowTuples[0] ?? []).map((first) => {
        const entry = this.entryOf('rows', first.dimension);
        const dim = this.dims.get(first.dimension);
        return (m) => (entry?.alias ? dim?.byName.get(m.name)?.attributes?.[entry.alias] : null) || m.name;
      });
      grid.rowTuples.forEach((tuple, r) => tuple.forEach((m, level) => {
        const above = grid.rowTuples[r - 1];
        if (above?.slice(0, level + 1).every((o, i) => o.unique_name === tuple[i].unique_name)) return;
        if (holds(labels[level](m)) || holds(m.name)) this.found.push({ r, level });
      }));
    }
    this.unseen = words && !this.found.length ? this.hiddenMatch(words) : null;
    this.paintFound();
  },

  /** A row's heading at a level, where the row is drawn and the heading is not blank. */
  rowHeading(r, level) {
    return this.grid?.rows.get(r)?.querySelector(`th.row-head[data-level="${level}"]`) ?? null;
  },

  /** Marks the headings found among those drawn, the current one apart, and says how many. */
  paintFound() {
    const grid = this.grid;
    if (!grid) return;
    for (const heading of grid.table.querySelectorAll('th.found')) heading.classList.remove('found', 'found-current');
    (this.found ?? []).forEach((hit, index) => {
      const heading = hit.heading ?? this.rowHeading(hit.r, hit.level);
      heading?.classList.add('found');
      if (index === this.findAt) heading?.classList.add('found-current');
    });
    const words = this.findWords;
    const count = this.found?.length ?? 0;
    this.finder.count.textContent = !words ? ''
      : count ? (this.findAt >= 0 && this.findAt < count ? `${this.findAt + 1} of ${count}` : `${count} found`)
        : this.unseen ? 'Enter shows it' : 'None found';
  },

  /** A member of a dimension on an axis, not shown, whose name holds the words. */
  hiddenMatch(words) {
    for (const entry of [...this.layout.rows, ...this.layout.columns]) {
      const element = this.dims.get(entry.dimension).elements.find((candidate) => candidate.name.toLowerCase().includes(words));
      if (element) return { dimension: entry.dimension, name: element.name };
    }
    return null;
  },

  /** Goes to the next heading found, or the one before; opens the grid to one not shown. */
  findNext(step) {
    if (!this.found?.length) {
      if (this.unseen) {
        this.findAt = 0;
        this.focusMember(this.unseen.dimension, this.unseen.name);
      }
      return;
    }
    this.findAt = (this.findAt + step + this.found.length) % this.found.length;
    const hit = this.found[this.findAt];
    if (hit.r !== undefined) this.revealRow(hit.r);
    this.paintFound();
    const heading = hit.heading ?? this.rowHeading(hit.r, hit.level);
    if (!heading) return;
    heading.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    // The row or column found is chosen, so that its cells stand out too.
    this.selection?.choose(this.selection.at(heading));
  },

  /** Says how a cell comes by its value, for a cell named by its row and column members. */
  /**
   * What a total adds up, along the first dimension in which its member is one: each child,
   * its weight, and its value as the grid reads it; null where they cannot be read.
   */
  async partsOf(cell) {
    const place = cell.at.findIndex((name, index) => this.dims.get(this.cube.dimensions[index])?.byName.get(name)?.children.length);
    if (place < 0) return null;
    const dimension = this.cube.dimensions[place];
    const children = this.dims.get(dimension).byName.get(cell.at[place]).children;
    const cells = children.map((child) => ({ cube: cell.cube, at: cell.at.map((name, index) => (index === place ? child.name : name)) }));
    try {
      const answer = await post('/cells/read', { cells, transaction: this.tx ?? undefined });
      return { dimension, lines: children.map((child, index) => ({ name: child.name, weight: child.weight ?? 1, value: answer.cells[index]?.value ?? null })) };
    } catch {
      return null;
    }
  },

  /** The newest change saved to a cell: null where none is on record, undefined where it cannot be read. */
  async lastChange(cell) {
    try {
      const answer = await post('/cells/history', { cube: cell.cube, at: cell.at, most: 1 }, { quiet: true });
      return answer.changes[0] ?? null;
    } catch {
      return undefined;
    }
  },

  async explain(rowTuple, columnTuple) {
    const at = new Map();
    for (const filter of this.layout.filters) at.set(filter.dimension, filter.member);
    for (const m of this.result.slicer ?? []) at.set(m.dimension, m.name);
    for (const m of [...rowTuple, ...columnTuple]) at.set(m.dimension, m.name);
    if ([...rowTuple, ...columnTuple].some((m) => m.type === 'calculated')) {
      notify('A calculated member is worked out by the query alone, so it has no explanation stored with the cube.');
      return;
    }
    const cell = { cube: this.cube.name, at: this.cube.dimensions.map((dim) => at.get(dim) ?? this.dims.get(dim).roots[0]) };
    let explanation;
    try {
      // As the grid shows it: with the changes not yet saved, or the sandbox worked in.
      const answer = await post('/cells/explain', { cells: [cell], transaction: this.tx ?? undefined });
      explanation = answer.cells[0];
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    if (explanation.error) {
      notify(explanation.error.message, 'bad');
      return;
    }
    const facts = explanation.cell;
    // A total no rule gives: what it adds up, and how much each part brings; a value stored,
    // who changed it last, and when.
    const parts = facts.kind === 'consolidated' && !facts.rule ? await this.partsOf(cell) : null;
    const last = facts.kind !== 'consolidated' && !facts.rule ? await this.lastChange(cell) : undefined;
    const part = ({ name, weight, value }) => h('li', {}, `${weight === 1 ? '' : `${formatNumber(weight)} × `}${name}: `,
      h('span', { class: 'num' }, typeof value === 'number' ? formatNumber(value, 6) : '(empty)'));
    const how = {
      leaf: facts.rule ? 'Worked out by a rule' : 'Stored in the cube',
      consolidated: facts.rule ? 'Worked out by a rule' : 'Added up from the leaves beneath it',
      text: 'Text stored in the cube',
    }[facts.kind];
    const value = typeof facts.value === 'number' ? formatNumber(facts.value, 6) : facts.value;
    const row = (label, content) => h('div', { class: 'fact' }, h('span', { class: 'muted' }, label), h('span', {}, content));
    await dialog({
      title: 'How this value comes about',
      confirm: 'Close',
      cancel: null,
      body: h('div', { class: 'facts' },
        row('Cell', cell.at.join(' · ')),
        row('Value', h('strong', { class: 'num' }, value === '' ? '(empty)' : value)),
        row('How', how),
        parts ? row(`Adds up, by ${parts.dimension}`, h('ul', { class: 'plain-list' }, parts.lines.map(part))) : null,
        last === undefined ? null : row('Last changed', last
          ? h('span', {}, last.author ?? 'Not known', ' · ', when(last.time))
          : h('span', { class: 'muted' }, 'Never, that is on record')),
        facts.rule ? row('Rule', h('code', { class: 'mono' }, facts.rule.text ?? JSON.stringify(facts.rule))) : null,
        facts.stored !== null && facts.stored !== undefined && facts.rule ? row('Stored beneath it', String(facts.stored)) : null,
        row('Can be written', facts.writable ? 'Yes' : !this.mayWrite() ? 'No: you may read it, not write it'
          : facts.rule ? 'No: a rule gives its value'
          : facts.kind === 'consolidated' ? 'No: it is a total, and a value typed into it is spread among the leaves beneath'
          : 'No'),
        facts.detail?.length ? row('Detail', h('ul', { class: 'plain-list' }, facts.detail.map((line) => h('li', {},
          `${line.label || '—'} `, h('span', { class: 'num muted' }, formatValue(line.value, this.layout.format)))))) : null,
        facts.invalid?.length ? row('Breaks', h('ul', { class: 'plain-list' }, facts.invalid.map((check) => h('li', {},
          check.message, check.refuses ? h('span', { class: 'muted' }, ' (written before the check refused such values)') : null)))) : null,
        facts.fed === null || facts.fed === undefined ? null : row('Fed', facts.fed ? 'Yes' : h('span', {}, 'No: consolidations leave it out. ',
          h('button', { class: 'button small', type: 'button', onclick: (event) => checkFeeders(this.cube.name, event.currentTarget) }, 'Check the feeders')))),
    });
  },

};
