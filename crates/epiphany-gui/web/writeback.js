// Writing back: typed values, pastes, spreads and holds, within an edit transaction that is
// saved, discarded, or taken back step by step. Methods of the explorer, kept apart to keep
// each part readable.

import { get, post } from './api.js';
import { dialog, h, notify, parseNumber } from './ui.js';

/** A spread typed into a total asks first when it reaches more leaf cells than this. */
const SPREAD_ASKS = 1000;

/** How many leaves lie beneath an element of a dimension; one for a leaf. Kept with the dimension. */
function leafCount(dim, name) {
  dim.leafCounts ??= new Map();
  if (!dim.leafCounts.has(name)) {
    const leaves = new Set();
    const seen = new Set();
    const visit = (each) => {
      if (seen.has(each)) return;
      seen.add(each);
      const kids = dim.byName.get(each)?.children ?? [];
      if (!kids.length) leaves.add(each);
      for (const kid of kids) visit(kid.name);
    };
    visit(name);
    dim.leafCounts.set(name, leaves.size);
  }
  return dim.leafCounts.get(name);
}

function spreadCode(text, before) {
  const [, letters = 'P', action, rest] = /^\s*(?:(LS|LR|P|S|R|C)\s*([+~])?)?\s*(.*?)\s*$/i.exec(String(text));
  const kind = letters.toUpperCase();
  if (kind === 'C') return rest === '' && !action ? { method: 'repeat', value: 0 } : null;
  const amount = parseNumber(rest);
  const method = { P: 'proportional', S: 'equal', LS: 'equal', R: 'repeat', LR: 'repeat' }[kind];
  if (amount === null || (action && method === 'repeat')) return null;
  return { method, value: action === '+' ? before + amount : action === '~' ? before - amount : amount };
}

/** Writing back: typed values, pastes, spreads and holds, within an edit transaction that is saved, discarded, or taken back step by step. */
export const writeback = {
  /** A grid cell's place in the cube, by element name; none where a member is calculated. */
  coordinate(r, c) {
    const [columns, rows] = this.result.axes;
    const members = [...(rows?.tuples[r] ?? []), ...(columns?.tuples[c] ?? []), ...(this.result.slicer ?? [])];
    if (members.some((m) => m.type === 'calculated')) return null;
    const at = new Map();
    // A dimension neither on an axis nor in the slicer stands at its first top element.
    for (const m of members) if (!at.has(m.dimension)) at.set(m.dimension, m.name);
    return this.cube.dimensions.map((dim) => at.get(dim) ?? this.dims.get(dim).roots[0]);
  },

  key(r, c) {
    return JSON.stringify(this.coordinate(r, c));
  },

  /** The cells of a result that may be written, as the server says: "row,column". */
  async findWritable(result) {
    const found = new Set();
    const why = new Map();
    if (!this.mayWrite()) return { writable: found, why };
    const [columns, rows] = result.axes;
    const columnCount = columns?.tuples.length ?? 0;
    const rowCount = rows ? Math.min(rows.tuples.length, 5000) : 1;
    const leaf = (m) => m.type === 'numeric' || m.type === 'string';
    if (!(result.slicer ?? []).every(leaf)) return { writable: found, why };
    const asked = [];
    const places = [];
    for (let r = 0; r < rowCount && asked.length < 20000; r += 1) {
      if (rows && !rows.tuples[r].every(leaf)) continue;
      for (let c = 0; c < columnCount; c += 1) {
        if (!columns.tuples[c].every(leaf)) continue;
        asked.push({ cube: this.cube.name, at: this.coordinate(r, c) });
        places.push(`${r},${c}`);
      }
    }
    if (!asked.length) return { writable: found, why };
    try {
      const answer = await post('/cells/explain', { cells: asked, transaction: this.tx ?? undefined });
      answer.cells.forEach((cell, index) => {
        if (cell.cell?.writable) found.add(places[index]);
        else if (cell.cell?.rule) why.set(places[index], `A rule works this cell out: ${cell.cell.rule.text ?? ''}`.trim());
      });
    } catch {
      // Without an answer nothing is offered for writing; the grid still shows.
    }
    return { writable: found, why };
  },

  /** Why a cell takes no value, said when someone types into it rather than nothing at all. */
  whyNot(r, c) {
    if (!this.mayWrite()) return `You may read ${this.cube.name}, not write to it.`;
    if (!this.coordinate(r, c)) return 'A calculated member\'s cells are worked out by the query, not stored.';
    return this.why?.get(`${r},${c}`) ?? 'This cell cannot be written.';
  },

  /** How many leaf cells lie beneath a cell of the grid: the leaves beneath each of its members, multiplied. */
  leavesBeneath(r, c) {
    const at = this.coordinate(r, c);
    return this.cube.dimensions.reduce((count, name, index) => count * leafCount(this.dims.get(name), at[index]), 1);
  },

  /** Opens the editor in a cell, with what was typed into it or its current value. */
  edit(cell, typed) {
    if (!cell || cell.querySelector('input')) return;
    const r = Number(cell.dataset.r);
    const c = Number(cell.dataset.c);
    const before = this.result.cells[r * this.result.axes[0].tuples.length + c];
    // A total takes a value, or a spreading code, to spread over the leaves beneath it.
    const spreading = cell.classList.contains('spreadable');
    const input = h('input', {
      class: 'cell-editor',
      'aria-label': spreading ? 'A value to spread over the leaves beneath' : 'New value',
      title: spreading ? 'A number spreads in proportion. Or P, S or R and a number: in proportion, equally, or the whole into each; + or ~ after the letter adds or takes away; C clears.' : null,
      inputmode: typeof before === 'string' ? 'text' : spreading ? 'text' : 'decimal',
    });
    input.value = typed ?? (before ?? '');
    const shown = [...cell.childNodes];
    cell.replaceChildren(input);
    input.focus();
    if (typed === undefined) input.select();
    let done = false;
    const finish = (save, move) => {
      if (done) return;
      done = true;
      cell.replaceChildren(...shown);
      if (save && spreading) this.spreadTyped(r, c, before, input.value, move);
      else if (save) this.write(r, c, before, input.value, move);
      else cell.focus();
    };
    input.addEventListener('keydown', (event) => {
      event.stopPropagation();
      if (event.key === 'Enter') {
        event.preventDefault();
        finish(true, [event.shiftKey ? -1 : 1, 0]);
      } else if (event.key === 'Tab') {
        event.preventDefault();
        finish(true, [0, event.shiftKey ? -1 : 1]);
      } else if (event.key === 'Escape') {
        event.preventDefault();
        finish(false);
      }
    });
    input.addEventListener('blur', () => finish(true));
  },

  /** Writes a value typed into a cell, within the transaction the edits are held in. */
  async write(r, c, before, text, move) {
    let value = text;
    if (typeof before !== 'string') {
      value = String(text).trim() === '' ? 0 : parseNumber(text);
      if (value === null) {
        notify(`“${text}” is not a number.`, 'bad');
        return;
      }
    }
    this.focusAfter = [r + (move?.[0] ?? 0), c + (move?.[1] ?? 0)];
    if (value === before || ((before === null || before === undefined) && (value === 0 || value === ''))) {
      this.focusCell();
      return;
    }
    await this.writeCells([{ r, c, value }]);
  },

  /**
   * Changes values within the edit transaction through `send`, which posts the change and
   * resolves with what to say of it. The change goes on the history of changes: the point in
   * the transaction it was made from, to go back to, and how to make it again.
   */
  async changeValues(label, send, redoing = false) {
    const before = { edits: new Map(this.edits), changes: this.changes ?? 0 };
    let said;
    let point;
    try {
      await this.ensureTransaction();
      ({ point } = await get(`/transactions/${this.tx}/point`));
      said = await send();
    } catch (problem) {
      this.refused(problem);
      return;
    }
    this.past.push({ label, point, ...before, again: () => this.changeValues(label, send, true) });
    if (this.past.length > 100) this.past.shift();
    if (!redoing) this.future = [];
    if (said) notify(said, 'good');
    await this.run();
  },

  /**
   * Writes values into cells of the grid, `{ r, c, value }` each, in one write within the
   * transaction the edits are held in, then runs the query again. Says `done` once written.
   */
  async writeCells(cells, done) {
    const written = cells.map(({ r, c, value }) => ({ at: this.coordinate(r, c), value }));
    this.focusAfter ??= [cells[0].r, cells[0].c];
    const label = written.length === 1 ? `the change to ${written[0].at.join(' · ')}` : `${written.length.toLocaleString()} changes`;
    await this.changeValues(label, async () => {
      await post('/cells/write', { transaction: this.tx, cells: written.map(({ at, value }) => ({ cube: this.cube.name, at, value })) });
      for (const { at, value } of written) this.edits.set(JSON.stringify(at), value);
      this.changes = (this.changes ?? 0) + written.length;
      return done;
    });
  },

  /** The value a cell of the result holds. */
  valueAt(r, c) {
    return this.result.cells[r * this.result.axes[0].tuples.length + c];
  },

  /** Empties the cells given that may be written and hold something: numbers to zero, text to nothing. */
  async clear(cells) {
    const cleared = cells
      .filter(([r, c]) => this.writable.has(`${r},${c}`))
      .map(([r, c]) => ({ r, c, before: this.valueAt(r, c) }))
      .filter(({ before }) => before !== null && before !== undefined && before !== 0 && before !== '')
      .map(({ r, c, before }) => ({ r, c, value: typeof before === 'string' ? '' : 0 }));
    if (!cleared.length) return;
    this.focusAfter = cells[0];
    await this.writeCells(cleared, cleared.length > 1 ? `Cleared ${cleared.length.toLocaleString()} cells.` : null);
  },

  /**
   * Writes pasted text into the grid, as a spreadsheet does: one value into every cell
   * chosen, or a block of tab-separated values from the top left of what is chosen, across
   * and down. Cells that may not be written, empty values, and text where a number belongs
   * are left out, and the notice says how many.
   */
  async paste(text) {
    if (!this.mayWrite()) {
      notify(`You may read ${this.cube.name}, not write to it.`, 'bad');
      return;
    }
    const lines = text.replace(/\r\n?/g, '\n').replace(/\n$/, '').split('\n').map((line) => line.split('\t'));
    const chosen = this.selection.cells();
    const last = this.selection.ranges[this.selection.ranges.length - 1];
    const [top, left] = last ? [last.r1, last.c1] : chosen[0] ?? [0, 0];
    const one = lines.length === 1 && lines[0].length === 1;
    const targets = one && chosen.length > 1
      ? chosen.map(([r, c]) => [r, c, lines[0][0]])
      : lines.flatMap((line, down) => line.map((raw, across) => [top + down, left + across, raw]));
    const cells = [];
    let leftOut = 0;
    for (const [r, c, raw] of targets) {
      if (raw.trim() === '') continue;
      if (!this.writable.has(`${r},${c}`)) {
        leftOut += 1;
        continue;
      }
      const value = typeof this.valueAt(r, c) === 'string' ? raw : parseNumber(raw);
      if (value === null) leftOut += 1;
      else cells.push({ r, c, value });
    }
    if (!cells.length) {
      notify(leftOut ? 'None of the cells it would go into can be written.' : 'There is nothing to paste.', 'bad');
      return;
    }
    this.focusAfter = [top, left];
    const pasted = `Pasted ${cells.length.toLocaleString()} value${cells.length === 1 ? '' : 's'}.`;
    await this.writeCells(cells, leftOut ? `${pasted} ${leftOut.toLocaleString()} left out: cells that cannot be written, or text where a number belongs.` : pasted);
  },

  /** Pastes what the clipboard holds, where the browser lets the page read it. */
  async pasteFromClipboard() {
    let text;
    try {
      text = await navigator.clipboard.readText();
    } catch {
      notify('The browser keeps the clipboard from the page. Press Ctrl V to paste instead.');
      return;
    }
    await this.paste(text);
  },

  /** Asks for a value to share out among the leaves beneath a total, and how. */
  async spread(r, c) {
    const at = this.coordinate(r, c);
    const before = this.valueAt(r, c);
    const amount = h('input', { class: 'input', id: 'spread-value', inputmode: 'decimal', required: true });
    amount.value = typeof before === 'number' ? String(before) : '';
    const method = h('select', { class: 'select', id: 'spread-method' },
      h('option', { value: 'proportional' }, 'In proportion to what each holds now'),
      h('option', { value: 'equal' }, 'Equally among them'),
      h('option', { value: 'repeat' }, 'The whole value in each'));
    const asked = await dialog({
      title: 'Spread a value',
      confirm: 'Spread',
      body: h('div', { class: 'facts' },
        h('p', { class: 'muted' }, `Over the leaves of ${at.join(' · ')}.`),
        h('div', { class: 'field' }, h('label', { for: 'spread-value' }, 'Value'), amount),
        h('div', { class: 'field' }, h('label', { for: 'spread-method' }, 'How'), method),
        h('p', { class: 'hint' }, 'You can also type into a total: a number spreads in proportion, and P, S or R before it spreads in proportion, equally, or the whole into each. Nothing is saved until you save your changes.')),
      onsubmit: () => {
        const value = parseNumber(amount.value);
        if (value === null) throw new Error('Enter a number.');
        return { value, method: method.value };
      },
    });
    if (asked) await this.spreadValue(r, c, asked.value, asked.method);
  },

  /**
   * Spreads what was typed into a total over the leaves beneath it, as TM1 does: a number
   * in proportion to what they hold; P, S (or LS) and R (or LR) before a number spread it in
   * proportion, equally, or the whole of it into each leaf; + or ~ after P or S adds the
   * number to the total or takes it away; and C clears every leaf.
   */
  async spreadTyped(r, c, before, text, move) {
    this.focusAfter = [r + (move?.[0] ?? 0), c + (move?.[1] ?? 0)];
    const asked = spreadCode(text, typeof before === 'number' ? before : 0);
    if (!asked) {
      if (String(text).trim() !== '' && String(text) !== String(before ?? '')) notify(`“${text}” is neither a number nor a spreading code such as P100, S100 or R100.`, 'bad');
      this.focusCell();
      return;
    }
    if (asked.method === 'proportional' && asked.value === before) {
      this.focusCell();
      return;
    }
    const count = this.leavesBeneath(r, c);
    if (count > SPREAD_ASKS || (asked.value === 0 && count > 1)) {
      const agreed = await dialog({
        title: asked.value === 0 ? `Empty ${count.toLocaleString()} cells?` : `Spread over ${count.toLocaleString()} cells?`,
        body: h('p', {}, `${asked.value === 0 ? 'Every leaf beneath' : 'The leaves beneath'} ${this.coordinate(r, c).join(' · ')} ${asked.value === 0 ? 'becomes empty' : 'take a share of it'}. Nothing is stored until you save your changes, and Discard takes it back.`),
        confirm: asked.value === 0 ? 'Empty them' : 'Spread',
        danger: asked.value === 0,
      });
      if (!agreed) {
        this.focusCell();
        return;
      }
    }
    await this.spreadValue(r, c, asked.value, asked.method);
  },

  /**
   * Holds a cell, so that spreads leave it as it is, or releases it; with no cell given,
   * releases every hold. Holds are kept in this browser until released.
   */
  hold(r, c, held) {
    if (r === null) this.holds.clear();
    else if (held) this.holds.add(this.key(r, c));
    else this.holds.delete(this.key(r, c));
    localStorage.setItem(`epiphany.holds.${this.cube.name}`, JSON.stringify([...this.holds]));
    this.draw();
  },

  /** Shares a value out among the leaves beneath a total, within the edit transaction. */
  async spreadValue(r, c, value, method) {
    const at = this.coordinate(r, c);
    const holds = [...this.holds].map((key) => JSON.parse(key));
    await this.changeValues(`the spread over ${at.join(' · ')}`, async () => {
      const answer = await post('/cells/spread', { transaction: this.tx, spread: { cube: this.cube.name, at, value, method, holds } });
      this.changes = (this.changes ?? 0) + 1;
      return `Spread over ${answer.written.toLocaleString()} cell${answer.written === 1 ? '' : 's'}.`;
    });
  },

  async ensureTransaction() {
    if (this.tx) return;
    const answer = await post('/transactions');
    this.tx = answer.transaction;
  },

  /** Stores every change made since the last save, in one step. */
  async save() {
    try {
      await post(`/transactions/${this.tx}/commit`);
    } catch (problem) {
      this.refused(problem);
      return;
    }
    notify(`Saved ${this.changes} change${this.changes === 1 ? '' : 's'}.`, 'good');
    this.forget();
    await this.run();
  },

  async discard() {
    try {
      await post(`/transactions/${this.tx}/rollback`);
    } catch {
      // Gone already: discarded either way.
    }
    this.forget();
    await this.run();
  },

  forget() {
    this.tx = null;
    this.edits.clear();
    this.changes = 0;
    // Changes of values went with the transaction; the layout's history stays.
    this.past = this.past.filter((entry) => typeof entry === 'string');
    this.future = this.future.filter((entry) => typeof entry === 'string');
    this.drawToolbar();
  },

  /** Reports a write the server refused; a transaction it no longer has is let go. */
  refused(problem) {
    if (problem.status === 404 && this.tx) {
      notify('Your unsaved changes were dropped after a long pause. Make them again to carry on.', 'bad');
      this.forget();
      this.run();
      return;
    }
    notify(problem.message, 'bad');
    this.focusCell();
  },

  focusCell() {
    if (!this.focusAfter) return;
    const [r, c] = this.focusAfter;
    const cell = this.output.querySelector(`td[data-r="${r}"][data-c="${c}"]`);
    this.focusAfter = null;
    if (!cell) return;
    for (const other of this.output.querySelectorAll('td[tabindex="0"]')) other.tabIndex = -1;
    cell.tabIndex = 0;
    cell.focus();
  },
};
