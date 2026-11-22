// The pivot grid: drawing and sizing it, moving about its headings, finding in it,
// highlighting it, and its menus. Methods of the explorer, kept apart to keep each part readable.

import { get, part, post } from './api.js';
import { empty } from './app.js';
import { member, saved } from './layout.js';
import { cellText, Selection } from './selection.js';
import { beneath } from './tree.js';
import { copyText, dialog, formatNumber, formatValue, h, icon, menu, mount, notify, SCALES } from './ui.js';

/**
 * What a spreading code typed into a total asks for: `{ method, value }`, the value being
 * the total's new one; null when the text is not a code. A number alone is spread in
 * proportion; P, S or LS, and R or LR before it say in proportion, equally, or the whole
 * into each; + or ~ after P or S adds to the total `before` or takes from it; C clears.
 */
/**
 * What a view's highlights do to a cell of its grid, or null when it has none: a bar as
 * long as the number's share of the largest in its column, a shade from the column's lowest
 * to its highest, and the colour of the first rule the number meets. Totals take no bar or
 * shade, being out of scale with what they add up.
 */
function highlighter(highlights, rows, columns, value, total) {
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
  const tests = { above: (a, b) => a > b, at_least: (a, b) => a >= b, below: (a, b) => a < b, at_most: (a, b) => a <= b, equals: (a, b) => a === b };
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
    const rule = highlights.rules?.find((each) => tests[each.test]?.(number, each.value));
    if (rule) td.classList.add(`tone-${rule.tone}`);
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
      mount(this.output, empty('grid', 'Nothing to show', this.layout.nonEmpty ? 'Every cell is empty. Show empty rows to see them.' : 'The query chose no members.'));
      mount(this.statusLine);
      return;
    }
    const limit = 5000;
    const shownRows = rowTuples.slice(0, limit);
    const total = (tuple) => tuple.some((m) => m.type === 'consolidated');
    // The attributes shown beside each dimension's members, where the layout asks for them.
    const shownAttributes = (place, tuple) => (tuple ?? []).map((m) => this.entryOf(place, m.dimension)?.attributes ?? []);
    const rowAttributes = shownAttributes('rows', rowTuples[0]);
    const columnAttributes = shownAttributes('columns', columnTuples[0]);
    const rowWidth = rowAttributes.reduce((sum, attributes) => sum + 1 + attributes.length, 0);
    const headRows = columnAttributes.reduce((sum, attributes) => sum + 1 + attributes.length, 0);
    const table = h('table', { class: 'pivot', role: 'grid', 'aria-rowcount': rowTuples.length + headRows, 'aria-colcount': columnTuples.length + rowWidth });
    const head = h('thead');
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
        tr.append(h('th', {
          scope: 'col',
          colspan: span,
          tabindex: '-1',
          class: `col-head ${m.type === 'consolidated' ? 'total' : ''}`,
          dataset: { ...across(span, index), ...this.headingOf('columns', m) },
        }, this.memberLabel('columns', m)));
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
    // Totals take a value to spread where the user may write and nothing in the slicer is calculated.
    const spreads = this.mayWrite() && !(result.slicer ?? []).some((m) => m.type === 'calculated');
    const highlight = highlighter(this.layout.highlights, shownRows, columnTuples, (r, c) => result.cells[r * columnCount + c], total);
    shownRows.forEach((tuple, r) => {
      const tr = h('tr', { class: total(tuple) ? 'total-row' : '' });
      tuple.forEach((m, level) => {
        // A member repeated from the row above is left blank, so groups read clearly.
        const shares = (other) => other && other.slice(0, level + 1).every((o, i) => o.unique_name === tuple[i].unique_name);
        const same = shares(shownRows[r - 1]);
        // A heading stands for its row and the rows beneath that repeat it.
        let last = r;
        while (!same && shares(shownRows[last + 1])) last += 1;
        const down = { r1: r, r2: last };
        tr.append(h('th', {
          scope: 'row',
          tabindex: '-1',
          class: `row-head ${m.type === 'consolidated' ? 'total' : ''}`,
          dataset: same ? down : { ...down, ...this.headingOf('rows', m) },
        }, same ? '' : this.memberLabel('rows', m)));
        for (const attribute of rowAttributes[level] ?? []) {
          tr.append(h('th', { scope: 'row', tabindex: '-1', class: 'row-attribute', title: attribute, dataset: down }, same ? '' : this.attributeValue(m, attribute)));
        }
      });
      for (let c = 0; c < columnCount; c += 1) {
        const value = result.cells[r * columnCount + c];
        const totalCell = total(tuple) || total(columnTuples[c]);
        const number = typeof value === 'number';
        const editable = this.writable.has(`${r},${c}`);
        const edited = editable && this.edits.has(this.key(r, c));
        const spreadable = spreads && totalCell && !editable && ![...tuple, ...columnTuples[c]].some((m) => m.type === 'calculated');
        const held = this.holds.size > 0 && this.holds.has(this.key(r, c));
        const td = h('td', {
          class: `${totalCell ? 'total' : ''} ${number ? 'n' : 's'} ${number && value < 0 ? 'negative' : ''} ${editable ? 'editable' : ''} ${edited ? 'edited' : ''} ${spreadable ? 'spreadable' : ''} ${held ? 'held' : ''}`,
          tabindex: r === 0 && c === 0 ? '0' : '-1',
          title: held ? 'Held: spreads leave it as it is' : edited ? 'Changed, not yet saved' : spreadable ? 'Type a value to spread it over the leaves beneath' : undefined,
          dataset: { r, c },
        }, formatValue(value, this.layout.format));
        highlight?.(td, r, c, value);
        if (this.commented?.size) this.markComments(td, r, c);
        tr.append(td);
      }
      body.append(tr);
    });
    const keys = [
      ...(rowTuples[0] ?? []).flatMap((m, index) => [`row:${m.dimension}`, ...rowAttributes[index].map((attribute) => `row:${m.dimension}:${attribute}`)]),
      ...columnTuples.map((tuple) => `col:${tuple.map((m) => m.unique_name).join('|')}`),
    ];
    table.append(h('colgroup', {}, keys.map((key) => h('col', { dataset: { key } }))), head, body);
    const summary = h('span', { class: 'selection-summary', 'aria-live': 'polite' });
    this.selection = new Selection(table, {
      rows: shownRows.length,
      columns: columnCount,
      value: (r, c) => result.cells[r * columnCount + c],
      changed: (chosen) => mount(summary, ...this.summarize(chosen)),
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
      if (!text || event.target.closest('input, textarea')) return;
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
    const scroller = h('div', { class: 'pivot-scroll' }, table);
    mount(this.output, scroller);
    this.size(table);
    this.pinHeads(table);
    this.focusCell();
    this.refocusHeading(table);
    this.markFound();
    const cells = rowTuples.length * columnCount;
    mount(this.statusLine,
      h('span', {}, `${rowTuples.length.toLocaleString()} rows × ${columnCount.toLocaleString()} columns`),
      h('span', { class: 'muted' }, `${cells.toLocaleString()} cells in ${Math.round(this.took)} ms`),
      rowTuples.length > limit ? h('span', { class: 'badge warn' }, `Showing the first ${limit.toLocaleString()} rows`) : null,
      this.writable.size ? h('span', { class: 'legend-input muted', title: 'Tinted cells take a value: type into one, or paste' }, 'Can be written') : null,
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

  /** A member's heading: with a button that opens or closes it where it has children. */
  memberLabel(place, m) {
    const entry = this.layout[place]?.find((candidate) => candidate.dimension === m.dimension);
    const dim = this.dims.get(m.dimension);
    const element = dim?.byName.get(m.name);
    const depth = entry ? this.depthOf(entry, dim, m.name) : 0;
    const label = h('span', { class: 'member-label', dataset: { depth } });
    if (entry && element?.children.length) {
      const opened = entry.expanded.includes(m.name);
      // Not a stop for Tab: the heading itself takes the keys that open and close it.
      label.append(h('button', {
        class: 'twisty',
        tabindex: '-1',
        'aria-expanded': String(opened),
        'aria-label': `${opened ? 'Close' : 'Open'} ${m.name}`,
        onclick: () => this.toggle(place, m.dimension, m.name),
      }, icon(opened ? 'minus' : 'plus', 12)));
    } else if (depth) {
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
    const drag = (handle, start, move, end) => handle.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      event.stopPropagation();
      const origin = { x: event.clientX, y: event.clientY, value: start() };
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
    });
    // A handle on the right edge of the heading of each column.
    const headRows = [...table.tHead.rows];
    const dataHeads = [...headRows[headRows.length - 1].cells].filter((cell) => !cell.classList.contains('corner'));
    const corners = [...table.tHead.querySelectorAll('th.corner')];
    const rowHeads = [...(table.tBodies[0]?.rows[0]?.cells ?? [])].filter((cell) => cell.tagName === 'TH');
    const columnHeads = [...(rowHeads.length ? corners : []), ...dataHeads];
    if (columnHeads.length !== cols.length) return;
    columnHeads.forEach((cell, index) => {
      const handle = h('span', { class: 'col-resize', role: 'separator', 'aria-orientation': 'vertical', 'aria-label': 'Drag to change the width of the column', title: 'Drag to change the width; double-click to fit it' });
      cell.append(handle);
      drag(handle, () => widths[index], (origin, event) => {
        widths[index] = Math.max(40, Math.round(origin.value + event.clientX - origin.x));
        apply();
      }, () => keep((geometry) => ({ widths: { ...(geometry.widths ?? {}), [cols[index].dataset.key]: widths[index] } })));
      handle.addEventListener('dblclick', (event) => {
        event.stopPropagation();
        widths[index] = fitted[index];
        apply();
        keep((geometry) => {
          const kept = { ...(geometry.widths ?? {}) };
          delete kept[cols[index].dataset.key];
          return { widths: kept };
        });
      });
    });
    // One handle beneath the first row's heading sets the height of every row.
    const first = rowHeads[0] ?? table.tBodies[0]?.rows[0]?.cells[0];
    if (!first) return;
    const handle = h('span', { class: 'row-resize', role: 'separator', 'aria-orientation': 'horizontal', 'aria-label': 'Drag to change the height of every row', title: 'Drag to change the height of every row' });
    first.append(handle);
    let height = geometry.row_height ?? first.getBoundingClientRect().height;
    drag(handle, () => height, (origin, event) => {
      height = Math.max(22, Math.min(120, Math.round(origin.value + event.clientY - origin.y)));
      table.style.setProperty('--row-height', `${height}px`);
    }, () => {
      keep(() => ({ row_height: height }));
      this.pinHeads(table);
    });
  },

  /**
   * Keeps the heading rows and the row headings in view as the grid scrolls: each heading
   * row below the one above it, and each row heading column beside those before it.
   */
  pinHeads(table) {
    let top = 0;
    for (const row of table.tHead.rows) {
      for (const cell of row.cells) {
        if (!cell.classList.contains('corner')) cell.style.top = `${top}px`;
      }
      top += row.offsetHeight;
    }
    const first = table.tBodies[0]?.rows[0];
    if (!first) return;
    const lefts = [];
    let left = 0;
    for (const cell of first.cells) {
      if (cell.tagName !== 'TH') break;
      lefts.push(left);
      left += cell.offsetWidth;
    }
    table.tHead.querySelectorAll('th.corner').forEach((cell, index) => {
      cell.style.left = `${lefts[index] ?? 0}px`;
    });
    for (const row of table.tBodies[0].rows) {
      [...row.cells].slice(0, lefts.length).forEach((cell, index) => {
        cell.style.left = `${lefts[index]}px`;
        cell.classList.add('pinned');
      });
    }
  },

  depthOf(entry, dim, name) {
    let depth = 0;
    let current = dim.byName.get(name);
    while (current && !entry.members.includes(current.name) && current.parents.length) {
      const parent = current.parents.find((candidate) => entry.expanded.includes(candidate));
      if (!parent) break;
      depth += 1;
      current = dim.byName.get(parent);
    }
    return Math.min(depth, 8);
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
      // Once for each cell, not at every key.
      if (this.toldWhy !== cell) notify(this.whyNot(r, c));
      this.toldWhy = cell;
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
    const move = moves[event.key];
    if (!move) return;
    const next = this.selection.cell(r + move[0], c + move[1]);
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

  /** What a heading stands for, kept on it to act on: its member, dimension and axis. */
  headingOf(place, m) {
    return { member: m.name, dimension: m.dimension, place };
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
    const opened = () => this.entryOf(heading.dataset.place, heading.dataset.dimension)?.expanded.includes(member);
    let target = null;
    switch (event.key) {
      case 'ArrowLeft':
      case 'ArrowRight': {
        const step = event.key === 'ArrowRight' ? 1 : -1;
        const sibling = row.cells[heading.cellIndex + step];
        if (sibling?.tagName === 'TH' && !sibling.classList.contains('corner')) target = sibling;
        else if (!inHead && step > 0) target = this.selection.cell(Number(row.querySelector('td')?.dataset.r), 0);
        break;
      }
      case 'ArrowUp':
      case 'ArrowDown': {
        const step = event.key === 'ArrowDown' ? 1 : -1;
        if (inHead) {
          const c = Number(heading.dataset.c1);
          const level = row.sectionRowIndex + step;
          target = level < table.tHead.rows.length ? this.columnHeading(table, level, c) : this.selection.cell(0, c);
        } else {
          const other = table.tBodies[0].rows[row.sectionRowIndex + step];
          target = other?.cells[heading.cellIndex];
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

  /** Opens a heading's member, or closes it, keeping the keys on the heading. */
  openHeading(heading) {
    const { place, dimension, member } = heading.dataset;
    if (!this.dims.get(dimension)?.byName.get(member)?.children.length || !this.entryOf(place, dimension)) return;
    this.keepHeading(heading);
    this.toggle(place, dimension, member);
  },

  /** Opens, or closes, a heading's member and everything beneath it. */
  openBeneath(heading, open) {
    const { place, dimension, member } = heading.dataset;
    const dim = this.dims.get(dimension);
    const within = [member, ...beneath([member], (name) => this.childrenOf(dim, name))].filter((name) => this.childrenOf(dim, name).length);
    this.keepHeading(heading);
    this.setOpen(place, dimension, (expanded) => (open ? [...new Set([...expanded, ...within])] : expanded.filter((name) => !within.includes(name))));
  },

  /** Keeps the keys on a heading through the grid being drawn again, if they were on it. */
  keepHeading(heading) {
    if (heading.contains(document.activeElement)) this.headingAfter = { ...heading.dataset };
  },

  refocusHeading(table) {
    const kept = this.headingAfter;
    this.headingAfter = null;
    if (!kept) return;
    const same = [...table.querySelectorAll('th[data-member]')].find((th) => th.dataset.member === kept.member && th.dataset.dimension === kept.dimension && th.dataset.place === kept.place);
    same?.focus();
  },

  /** The menu of a member's heading: opening and closing, keeping and removing, copying. */
  headingMenu(heading, at) {
    const { place, dimension, member: name } = heading.dataset;
    const dim = this.dims.get(dimension);
    const entry = this.entryOf(place, dimension);
    const element = dim?.byName.get(name);
    const kids = element ? this.childrenOf(dim, name).length : 0;
    const opened = entry?.expanded.includes(name);
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
    const openable = (place, tuple) => tuple.filter((m) => this.dims.get(m.dimension)?.byName.get(m.name)?.children.length);
    const rowOpen = openable('rows', rowTuple);
    const chosen = this.selection?.cells().length ?? 0;
    menu([
      { heading: [...rowTuple, ...columnTuple].map((m) => m.name).join(' · ') },
      { label: 'Sort rows by this column, largest first', icon: 'down', disabled: !this.layout.rows.length, action: () => this.sortBy(columnTuple, true) },
      { label: 'Sort rows by this column, smallest first', icon: 'chevron', disabled: !this.layout.rows.length, action: () => this.sortBy(columnTuple, false) },
      { label: 'Keep only the largest or smallest rows by it…', icon: 'filter', disabled: !this.layout.rows.length, action: () => this.keepTop(columnTuple) },
      '-',
      ...rowOpen.map((m) => {
        const entry = this.layout.rows.find((candidate) => candidate.dimension === m.dimension);
        const opened = entry?.expanded.includes(m.name);
        return { label: `${opened ? 'Close' : 'Open'} ${m.name}`, icon: opened ? 'minus' : 'plus', action: () => this.toggle('rows', m.dimension, m.name) };
      }),
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
      layout.nonEmpty = true;
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
          this.output.querySelector('td[tabindex="0"]')?.focus();
        }
      }
    });
    return { box: h('div', { class: 'search finder' }, icon('search', 16), input, count), input, count };
  },

  /** Marks the headings that hold the words found, and says how many there are. */
  markFound() {
    const words = this.findWords;
    const headings = [...this.output.querySelectorAll('.pivot th[data-member]')];
    this.found = [];
    for (const heading of headings) {
      const hit = Boolean(words) && (heading.textContent.toLowerCase().includes(words) || heading.dataset.member.toLowerCase().includes(words));
      heading.classList.toggle('found', hit);
      heading.classList.remove('found-current');
      if (hit) this.found.push(heading);
    }
    this.unseen = words && !this.found.length ? this.hiddenMatch(words) : null;
    const at = this.found[this.findAt];
    at?.classList.add('found-current');
    this.finder.count.textContent = !words ? ''
      : this.found.length ? (at ? `${this.findAt + 1} of ${this.found.length}` : `${this.found.length} found`)
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
    this.markFound();
    const heading = this.found[this.findAt];
    heading.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    // The row or column found is chosen, so that its cells stand out too.
    this.selection?.choose(this.selection.at(heading));
  },

  /** Says how a cell comes by its value, for a cell named by its row and column members. */
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
      const answer = await post('/cells/explain', { cells: [cell] });
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
        facts.rule ? row('Rule', h('code', { class: 'mono' }, facts.rule.text ?? JSON.stringify(facts.rule))) : null,
        facts.stored !== null && facts.stored !== undefined && facts.rule ? row('Stored beneath it', String(facts.stored)) : null,
        row('Can be written', facts.writable ? 'Yes' : 'No'),
        facts.fed === null || facts.fed === undefined ? null : row('Fed', facts.fed ? 'Yes' : h('span', {}, 'No: consolidations leave it out. ',
          h('button', { class: 'button small', type: 'button', onclick: () => this.checkFeeders() }, 'Check the feeders')))),
    });
  },

  /** Looks for cells of the cube that rules calculate and hold a value, but that no feeder feeds. */
  async checkFeeders() {
    let found;
    try {
      found = await get(`/cubes/${part(this.cube.name)}/feeders/check`);
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    const unfed = found.unfed ?? [];
    await dialog({
      title: unfed.length ? `${unfed.length.toLocaleString()}${unfed.length >= 100 ? ' or more' : ''} cells not fed` : 'Every calculated cell is fed',
      confirm: 'Close',
      cancel: null,
      body: unfed.length
        ? h('div', { class: 'facts' },
          h('p', { class: 'muted' }, 'These cells hold a value a rule works out, but no feeder feeds them, so the totals above them leave them out. A feeder that names them, or something beneath them, brings them in.'),
          h('ul', { class: 'plain-list mono' }, unfed.slice(0, 50).map((cell) => h('li', {}, cell.at.join(' · ')))),
          unfed.length > 50 ? h('p', { class: 'hint' }, `And ${(unfed.length - 50).toLocaleString()} more.`) : null)
        : h('p', {}, `No cell of ${this.cube.name} that a rule calculates holds a value its totals leave out.`),
    });
  },
};
