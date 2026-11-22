// The cells chosen in a grid, as in a spreadsheet: one or more rectangles of cells. A click
// chooses a cell, a click on a heading its row or column, and on the corner every cell;
// Shift stretches the last rectangle to the cell or heading, Ctrl or Cmd adds one, and a
// drag chooses everything it passes over. Shift and the arrow keys stretch it from the
// keyboard, Shift Space and Ctrl Space choose the row and the column, Ctrl A every cell,
// Escape just the active one, and copying gives the chosen values as tab-separated text.

import { numberText } from './ui.js';

const ignored = 'button, a, input, select, textarea, .col-resize, .row-resize';

export class Selection {
  /**
   * @param table the grid, whose body cells carry `data-r` and `data-c`, whose row and
   *   column headings carry `data-r1`/`data-r2` and `data-c1`/`data-c2`, and whose corner
   *   headings choose every cell
   * @param rows, columns how many rows and columns of cells the grid has
   * @param value gives a cell's value by its row and column
   * @param changed is told each time the cells chosen change
   * @param cell gives a cell's element by its row and column, where its row is drawn
   * @param reveal scrolls a cell into view, drawing its row, and gives its element
   * @param drawn gives the rows drawn, by their place
   */
  constructor(table, { rows, columns, value, changed, cell, reveal, drawn }) {
    Object.assign(this, { table, rows, columns, value, changed, cellAt: cell, revealAt: reveal, drawn });
    this.ranges = [];
    this.painted = [];
    this.origin = null;
    this.end = null;
    // The cell the keys act on, by its row and column.
    this.current = null;
    table.addEventListener('pointerdown', (event) => this.pointed(event));
    table.addEventListener('keydown', (event) => this.key(event));
    table.addEventListener('copy', (event) => {
      if (!this.ranges.length || event.target.closest('input, textarea')) return;
      event.preventDefault();
      event.clipboardData.setData('text/plain', this.text());
    });
    // Moving to another cell, by the arrow keys or Tab, chooses it on its own; focus coming
    // back to the active cell, from a menu say, leaves the choice as it is.
    table.addEventListener('focusin', (event) => {
      const element = event.target.closest('td[data-r]');
      if (!element || this.pointing) return;
      const [r, c] = [Number(element.dataset.r), Number(element.dataset.c)];
      if (this.current?.r === r && this.current?.c === c) return;
      this.current = { r, c };
      this.choose(this.at(element));
    });
  }

  /** Takes on what another selection of a grid of the same size had chosen, drawn again. */
  restore(other) {
    Object.assign(this, { ranges: other.ranges, origin: other.origin, end: other.end, current: other.current });
    this.paint();
    this.changed?.(this.summary());
  }

  /** The rectangle a cell or heading stands for. */
  at(element) {
    const { r, c, r1, r2, c1, c2 } = element.dataset;
    if (r !== undefined) return { r1: Number(r), r2: Number(r), c1: Number(c), c2: Number(c) };
    if (r1 !== undefined) return { r1: Number(r1), r2: Number(r2), c1: 0, c2: this.columns - 1 };
    if (c1 !== undefined) return { r1: 0, r2: this.rows - 1, c1: Number(c1), c2: Number(c2) };
    return this.all();
  }

  all() {
    return { r1: 0, r2: this.rows - 1, c1: 0, c2: this.columns - 1 };
  }

  /** Chooses a rectangle: in place of the others, beside them, or stretching the last. */
  choose(range, how = 'only') {
    if (how === 'stretch' && this.origin) {
      this.ranges[this.ranges.length - 1] = union(this.origin, range);
      this.end = { r: range.r2, c: range.c2 };
    } else {
      if (how === 'only') this.ranges = [];
      this.ranges.push(range);
      this.origin = range;
      this.end = { r: range.r2, c: range.c2 };
    }
    this.paint();
    this.changed?.(this.summary());
  }

  has(r, c) {
    return this.ranges.some((range) => r >= range.r1 && r <= range.r2 && c >= range.c1 && c <= range.c2);
  }

  /** Every chosen cell, each once, row by row. */
  cells() {
    if (this.ranges.length === 1) {
      const [{ r1, r2, c1, c2 }] = this.ranges;
      const cells = [];
      for (let r = r1; r <= r2; r += 1) for (let c = c1; c <= c2; c += 1) cells.push([r, c]);
      return cells;
    }
    const seen = new Set();
    const cells = [];
    for (const range of this.ranges) {
      for (let r = range.r1; r <= range.r2; r += 1) {
        for (let c = range.c1; c <= range.c2; c += 1) {
          const key = r * this.columns + c;
          if (!seen.has(key)) {
            seen.add(key);
            cells.push([r, c]);
          }
        }
      }
    }
    return cells.sort(([r1, c1], [r2, c2]) => r1 - r2 || c1 - c2);
  }

  pointed(event) {
    if (event.button !== 0 || event.target.closest(ignored)) return;
    const target = event.target.closest('td[data-r], th[data-r1], th[data-c1], th.corner');
    if (!target) return;
    const kind = target.tagName === 'TD' ? 'td[data-r]' : target.dataset.r1 !== undefined ? 'th[data-r1]' : target.dataset.c1 !== undefined ? 'th[data-c1]' : null;
    const how = event.shiftKey ? 'stretch' : event.ctrlKey || event.metaKey ? 'add' : 'only';
    // Focus is given here, not by the browser, and no text is chosen as the pointer drags.
    event.preventDefault();
    this.pointing = true;
    this.choose(this.at(target), how);
    // The first cell of what was clicked is the active one, unless the click stretched.
    const range = this.ranges[this.ranges.length - 1];
    if (how !== 'stretch') this.activate(range.r1, range.c1);
    // Dragging stretches the choice over the cells, or headings, of the kind it began on.
    const over = (next) => {
      const element = kind && next.target.closest?.(kind);
      if (element && this.table.contains(element)) this.choose(this.at(element), 'stretch');
    };
    const done = () => {
      this.pointing = false;
      this.table.removeEventListener('pointerover', over);
      window.removeEventListener('pointerup', done);
      window.removeEventListener('pointercancel', done);
    };
    this.table.addEventListener('pointerover', over);
    window.addEventListener('pointerup', done);
    window.addEventListener('pointercancel', done);
  }

  /**
   * Makes a cell the one keys act on, and gives it the focus: brought into view, unless it
   * was pointed at, when it is where the pointer is, or nowhere drawn.
   */
  activate(r, c) {
    const cell = this.pointing ? this.cell(r, c) : this.reveal(r, c);
    if (!cell) return;
    this.current = { r, c };
    for (const other of this.table.querySelectorAll('td[tabindex="0"]')) other.tabIndex = -1;
    cell.tabIndex = 0;
    cell.focus({ preventScroll: this.pointing });
  }

  /** A cell's element, where its row is drawn. */
  cell(r, c) {
    return this.within(r, c) ? this.cellAt(r, c) : undefined;
  }

  /** A cell's element, scrolled into view and drawn if it was not; none past the edges. */
  reveal(r, c) {
    return this.within(r, c) ? this.revealAt(r, c) : undefined;
  }

  within(r, c) {
    return r >= 0 && r < this.rows && c >= 0 && c < this.columns;
  }

  key(event) {
    const cell = event.target.closest('td[data-r]');
    if (!cell || event.altKey) return;
    const r = Number(cell.dataset.r);
    const c = Number(cell.dataset.c);
    const command = event.ctrlKey || event.metaKey;
    const moves = { ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
    let range = null;
    let how = 'only';
    if (event.shiftKey && moves[event.key]) {
      const [down, across] = moves[event.key];
      const end = this.end ?? { r, c };
      const next = { r: clamp(end.r + down, this.rows), c: clamp(end.c + across, this.columns) };
      range = { r1: next.r, r2: next.r, c1: next.c, c2: next.c };
      how = 'stretch';
    } else if (event.key === ' ' && event.shiftKey && !command) {
      range = { r1: r, r2: r, c1: 0, c2: this.columns - 1 };
    } else if (event.key === ' ' && command) {
      range = { r1: 0, r2: this.rows - 1, c1: c, c2: c };
    } else if (command && event.key.toLowerCase() === 'a') {
      range = this.all();
    } else if (event.key === 'Escape' && this.cells().length > 1) {
      range = { r1: r, r2: r, c1: c, c2: c };
    }
    if (!range) return;
    event.preventDefault();
    if (how === 'stretch' && !this.origin) this.origin = { r1: r, r2: r, c1: c, c2: c };
    this.choose(range, how);
    if (how === 'stretch') this.reveal(this.end.r, this.end.c);
  }

  /** The chosen values as tab-separated text, the cells between rectangles left blank. */
  text() {
    if (!this.ranges.length) return '';
    const { r1: top, r2: bottom, c1: left, c2: right } = this.ranges.reduce(union);
    const lines = [];
    for (let r = top; r <= bottom; r += 1) {
      const line = [];
      for (let c = left; c <= right; c += 1) line.push(this.has(r, c) ? cellText(this.value(r, c)) : '');
      lines.push(line.join('\t'));
    }
    return lines.join('\n');
  }

  /** Marks the chosen cells of the rows drawn, and the headings of their rows and columns. */
  paint() {
    for (const element of this.painted) element.classList.remove('selected', 'in-selection');
    this.painted = [];
    const mark = (element, name) => {
      element.classList.add(name);
      this.painted.push(element);
    };
    const [first] = this.ranges;
    const many = this.ranges.length > 1 || (first && (first.r1 !== first.r2 || first.c1 !== first.c2));
    if (many) {
      for (const r of this.drawn().keys()) {
        for (const range of this.ranges) {
          if (r < range.r1 || r > range.r2) continue;
          for (let c = range.c1; c <= range.c2; c += 1) {
            const cell = this.cellAt(r, c);
            if (cell) mark(cell, 'selected');
          }
        }
      }
    }
    const meets = (from, to, low, high) => from <= high && to >= low;
    for (const head of this.table.querySelectorAll('th[data-r1]')) {
      if (this.ranges.some((range) => meets(Number(head.dataset.r1), Number(head.dataset.r2), range.r1, range.r2))) mark(head, 'in-selection');
    }
    for (const head of this.table.querySelectorAll('th[data-c1]')) {
      if (this.ranges.some((range) => meets(Number(head.dataset.c1), Number(head.dataset.c2), range.c1, range.c2))) mark(head, 'in-selection');
    }
  }

  /** How many cells are chosen and, of those holding numbers, their sum, mean, least and most. */
  summary() {
    const cells = this.cells();
    const numbers = cells.map(([r, c]) => this.value(r, c)).filter((value) => typeof value === 'number');
    const sum = numbers.reduce((total, value) => total + value, 0);
    return {
      count: cells.length,
      numbers: numbers.length,
      sum,
      average: numbers.length ? sum / numbers.length : null,
      min: numbers.length ? numbers.reduce((least, value) => Math.min(least, value)) : null,
      max: numbers.length ? numbers.reduce((most, value) => Math.max(most, value)) : null,
    };
  }
}

/** A value as one cell of tab-separated text: numbers as the reader writes them. */
export function cellText(value) {
  if (typeof value === 'number') return numberText(value);
  return String(value ?? '').replace(/[\t\r\n]+/g, ' ');
}

function union(a, b) {
  return { r1: Math.min(a.r1, b.r1), r2: Math.max(a.r2, b.r2), c1: Math.min(a.c1, b.c1), c2: Math.max(a.c2, b.c2) };
}

function clamp(value, count) {
  return Math.max(0, Math.min(count - 1, value));
}
