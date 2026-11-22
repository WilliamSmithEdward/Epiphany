// Writing back: typed values, pastes, spreads and holds, within an edit transaction that is
// saved, discarded, or taken back step by step. Methods of the explorer, kept apart to keep
// each part readable.

import { get, post } from './api.js';
import { dialog, h, MOD, notify, parseNumber } from './ui.js';

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

/**
 * What a spreading code typed into a total asks for: `{ method, value }`, the value being
 * the total's new one; null when the text is not a code. A number alone is spread in
 * proportion; P, S or LS, and R or LR before it say in proportion, equally, or the whole
 * into each; + or ~ after P or S adds to the total `before` or takes from it; P% changes it
 * by a percentage, in proportion; C clears.
 */
function spreadCode(text, before) {
  const [, letters = 'P', action, rest] = /^\s*(?:(LS|LR|P%|P|S|R|C)\s*([+~])?)?\s*(.*?)\s*$/i.exec(String(text));
  const kind = letters.toUpperCase();
  if (kind === 'C') return rest === '' && !action ? { method: 'repeat', value: 0 } : null;
  const amount = parseNumber(rest);
  const method = { P: 'proportional', 'P%': 'proportional', S: 'equal', LS: 'equal', R: 'repeat', LR: 'repeat' }[kind];
  if (amount === null || (action && (method === 'repeat' || kind === 'P%'))) return null;
  if (kind === 'P%') return { method, value: before * (1 + amount / 100) };
  return { method, value: action === '+' ? before + amount : action === '~' ? before - amount : amount };
}

/** The way a code that runs along the grid goes from its cell, a row and a column at a step. */
const ALONG = { '>': [0, 1], '<': [0, -1], '^': [-1, 0], '|': [1, 0], _: [1, 0] };

/**
 * What a spreading code that runs along the grid asks for, `R>100` say, or null when the
 * text is not one: the step it goes by, and `values(befores)`, which makes the values of the
 * cells it reaches from what they hold. R repeats a value into each, or adds it with + or
 * takes it away with ~; S shares a value out equally and P in proportion to what they hold;
 * GR grows from a first value by a percentage a cell; SL runs in a straight line from a first
 * value to a last; C clears them. Two values stand apart by a colon, a semicolon or a space.
 * It runs with > to the end of the row, < to its start, ^ to the top of the column, and
 * | or _ to its foot.
 */
function alongCode(text) {
  const typed = /^\s*(GR|SL|R|S|P|C)\s*([<>^|_])\s*([+~])?\s*(.*?)\s*$/i.exec(String(text));
  if (!typed) return null;
  const [, letters, direction, action, rest] = typed;
  const kind = letters.toUpperCase();
  const numbers = rest === '' ? [] : rest.split(/\s*[:;]\s*|\s+/).map(parseNumber);
  const wanted = { R: 1, S: 1, P: 1, GR: 2, SL: 2, C: 0 }[kind];
  if (numbers.length !== wanted || numbers.includes(null) || (action && kind !== 'R')) return null;
  const [first, second] = numbers;
  const values = (befores) => {
    const count = befores.length;
    const sum = befores.reduce((all, before) => all + before, 0);
    return befores.map((before, index) => {
      if (kind === 'R') return action === '+' ? before + first : action === '~' ? before - first : first;
      if (kind === 'S') return first / count;
      if (kind === 'P') return sum ? (first * before) / sum : first / count;
      if (kind === 'GR') return first * (1 + second / 100) ** index;
      if (kind === 'SL') return count === 1 ? first : first + ((second - first) * index) / (count - 1);
      return 0;
    });
  };
  return { step: ALONG[direction], values };
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

  /** Forgets which cells may be written, for a new answer to the query. */
  forgetWritable() {
    // The cells that may be written, as "row,column"; why others may not; the checks of the
    // cube each cell's value breaks; and, by row, the asking about them, and the rows answered.
    this.writable = new Set();
    this.why = new Map();
    this.invalid = new Map();
    // The cells whose values are the sum of supporting detail, and how many lines each has.
    this.detailed = new Map();
    this.explained = new Map();
    this.known = new Set();
  },

  /**
   * Asks the server which cells of the rows given may be written, for those rows not asked
   * about yet, at most 20,000 cells a call, and marks them where they are drawn when it
   * answers. Resolves once every row given is answered. Without an answer, nothing in them
   * is offered for writing; the grid still shows. A cube with checks has every cell asked
   * about, totals too and whoever reads them, for the values that break a check to be marked.
   */
  explainRows(rows) {
    const result = this.result;
    const writing = this.mayWrite();
    const checking = Boolean(this.cube.validations?.length);
    if (!result || (!writing && !checking)) return Promise.resolve();
    const [columns, rowAxis] = result.axes;
    const leaf = (m) => m.type === 'numeric' || m.type === 'string';
    const writable = writing && (result.slicer ?? []).every(leaf);
    if (!writable && !checking) return Promise.resolve();
    const asked = (tuple) => checking || tuple.every(leaf);
    const wanted = columns.tuples.flatMap((tuple, c) => (asked(tuple) ? [c] : []));
    const batches = [];
    let batch = null;
    for (const r of new Set(rows)) {
      if (this.explained.has(r)) continue;
      if (!batch || batch.cells.length + wanted.length > 20000) batches.push((batch = { rows: [], cells: [], places: [] }));
      batch.rows.push(r);
      if (rowAxis && !asked(rowAxis.tuples[r])) continue;
      for (const c of wanted) {
        // A calculated member's cell is the query's, and nobody's to check.
        const at = this.coordinate(r, c);
        if (!at) continue;
        batch.cells.push({ cube: this.cube.name, at });
        batch.places.push(`${r},${c}`);
      }
    }
    for (const each of batches) {
      const asked = each.cells.length
        ? post('/cells/explain', { cells: each.cells, transaction: this.tx ?? undefined }, { quiet: true }).catch(() => null)
        : Promise.resolve(null);
      const answered = asked.then((answer) => {
        if (this.result !== result) return;
        answer?.cells.forEach((cell, index) => {
          const place = each.places[index];
          if (cell.cell?.writable && writable) this.writable.add(place);
          else if (cell.cell?.rule) this.why.set(place, `A rule works this cell out: ${cell.cell.rule.text ?? ''}`.trim());
          if (cell.cell?.invalid?.length) this.invalid.set(place, cell.cell.invalid);
          if (cell.cell?.detail?.length) this.detailed.set(place, cell.cell.detail.length);
        });
        for (const r of each.rows) this.known.add(r);
        this.markWritable(each.rows);
      });
      for (const r of each.rows) this.explained.set(r, answered);
    }
    return Promise.all([...new Set(rows)].map((r) => this.explained.get(r)));
  },

  /** Asks about the rows drawn once scrolling pauses, rather than at every step of it. */
  explainSoon() {
    clearTimeout(this.explaining);
    this.explaining = setTimeout(() => {
      if (this.grid?.scroller.isConnected) this.explainRows(this.drawnRows());
    }, 150);
  },

  /** Marks the cells of rows just answered, where they are drawn, as they may be written. */
  markWritable(rows) {
    const grid = this.grid;
    if (!grid?.scroller.isConnected) return;
    for (const r of rows) {
      const tr = grid.rows.get(r);
      if (!tr) continue;
      for (let c = 0; c < grid.columnCount; c += 1) this.markWriting(tr.children[grid.rowWidth + c], r, c);
    }
    if (grid.legend) grid.legend.hidden = !this.writable.size;
    if (grid.invalidLegend) grid.invalidLegend.hidden = !this.invalid.size;
  },

  /** The values a cell's picklist offers, where its element in the cube's last dimension has one. */
  picklistAt(r, c) {
    const at = this.coordinate(r, c);
    return at ? this.dims.get(this.cube.dimensions.at(-1))?.byName.get(at.at(-1))?.picklist ?? null : null;
  },

  /**
   * Marks a cell as it may be written: taking a value, from a picklist or not, changed and
   * not yet saved, changed in the sandbox worked in, taking a value to spread over the
   * leaves beneath it, or held as spreads go.
   */
  markWriting(td, r, c) {
    const editable = this.writable.has(`${r},${c}`);
    const edited = editable && this.edits.size > 0 && this.edits.has(this.key(r, c));
    const spreadable = this.spreadable(r, c);
    const held = this.holds.size > 0 && this.holds.has(this.key(r, c));
    const stored = this.storedText(r, c);
    const invalid = this.invalid.get(`${r},${c}`);
    const detail = this.detailed.get(`${r},${c}`);
    td.classList.toggle('editable', editable);
    td.classList.toggle('picklist', editable && typeof this.valueAt(r, c) !== 'number' && Boolean(this.picklistAt(r, c)));
    td.classList.toggle('edited', edited);
    td.classList.toggle('sandboxed', stored !== null);
    td.classList.toggle('spreadable', spreadable);
    td.classList.toggle('held', held);
    td.classList.toggle('invalid', Boolean(invalid));
    td.classList.toggle('detailed', Boolean(detail));
    // Everything there is to say of the cell, the checks it breaks first.
    const title = [
      ...(invalid ?? []).map((check) => check.message),
      detail ? `The total of its ${detail} line${detail === 1 ? '' : 's'} of detail` : null,
      held ? 'Held: spreads leave it as it is' : null,
      edited ? 'Changed, not yet saved' : null,
      stored !== null ? `Changed in ${this.sandbox.name}. Stored: ${stored || 'nothing'}` : null,
      spreadable && !held ? 'Type a value to spread it over the leaves beneath' : null,
    ].filter(Boolean).join('\n');
    if (title) td.title = title;
    else td.removeAttribute('title');
  },

  /** Whether a cell is a total that takes a value to spread over the leaves beneath it. */
  spreadable(r, c) {
    const { total, rowTuples, columnTuples, spreads } = this.grid;
    const [tuple, column] = [rowTuples[r], columnTuples[c]];
    return spreads && (total(tuple) || total(column)) && !this.writable.has(`${r},${c}`) && ![...tuple, ...column].some((m) => m.type === 'calculated');
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
    if (!cell || cell.querySelector('.cell-editor')) return;
    const r = Number(cell.dataset.r);
    const c = Number(cell.dataset.c);
    const before = this.result.cells[r * this.result.axes[0].tuples.length + c];
    // A total takes a value, or a spreading code, to spread over the leaves beneath it.
    const spreading = cell.classList.contains('spreadable');
    // A text cell with a picklist is given one of its values, chosen from the list; one typed
    // chooses the first value that begins with it.
    const picklist = spreading ? null : this.picklistAt(r, c);
    const input = picklist
      ? h('select', { class: 'cell-editor', 'aria-label': 'New value' },
        h('option', { value: '' }, '—'),
        before && !picklist.includes(before) ? h('option', { value: before }, `${before} (not offered)`) : null,
        picklist.map((value) => h('option', { value }, value)))
      : h('input', {
        class: 'cell-editor',
        'aria-label': spreading ? 'A value to spread over the leaves beneath' : 'New value',
        title: spreading ? 'A number spreads in proportion. Or P, S or R and a number: in proportion, equally, or the whole into each; + or ~ after the letter adds or takes away; P% and a number changes it by that percentage; C clears. With > < ^ or | after the letter, R, S, P, GR and SL run along the row or the column.' : null,
        inputmode: typeof before === 'string' ? 'text' : spreading ? 'text' : 'decimal',
      });
    input.value = picklist && typed !== undefined
      ? picklist.find((value) => value.toLowerCase().startsWith(typed.toLowerCase())) ?? (before ?? '')
      : typed ?? (before ?? '');
    const shown = [...cell.childNodes];
    cell.replaceChildren(input);
    input.focus();
    if (!picklist && typed === undefined) input.select();
    if (picklist && typed === undefined) {
      try {
        // The list opens at once where the browser lets it, within a click or a key. A value
        // typed is chosen already, for Enter to take.
        input.showPicker?.();
      } catch {
        // Opened after waiting on the server, it opens at a click or a key instead.
      }
    }
    let done = false;
    const finish = (save, move) => {
      if (done) return;
      done = true;
      cell.replaceChildren(...shown);
      const along = save && !picklist ? alongCode(input.value) : null;
      if (along) this.spreadAlong(r, c, along, move);
      else if (save && spreading) this.spreadTyped(r, c, before, input.value, move);
      else if (save) this.write(r, c, before, input.value, move);
      else cell.focus();
    };
    // A value chosen from the open list is taken at once; one stepped to with the keys, the
    // list closed, waits for Enter, Tab or leaving the cell.
    let keyed = false;
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
      } else {
        keyed = true;
        setTimeout(() => { keyed = false; });
      }
    });
    if (picklist) input.addEventListener('change', () => { if (!keyed) finish(true); });
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
    if (!(await this.replacingDetail([[r, c]]))) {
      this.focusAfter = [r, c];
      this.focusCell();
      return;
    }
    await this.writeCells([{ r, c, value }]);
  },

  /**
   * Changes values within the edit transaction through `send`, which posts the change and
   * resolves with what to say of it. The change goes on the history of changes: the point in
   * the transaction it was made from, to go back to, and how to make it again. A change
   * refused leaves the keys on `from`, the cell it was made in, to be put right there.
   */
  async changeValues(label, send, redoing = false, from = null) {
    const before = { edits: new Map(this.edits), changes: this.changes ?? 0 };
    let said;
    let point;
    try {
      await this.ensureTransaction();
      ({ point } = await get(`/transactions/${this.tx}/point`));
      said = await send();
    } catch (problem) {
      if (from) this.focusAfter = from;
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
      // In a sandbox, the cells it changes are marked against the stored values instead.
      if (!this.sandbox) for (const { at, value } of written) this.edits.set(JSON.stringify(at), value);
      this.changes = (this.changes ?? 0) + written.length;
      await this.warnOfChecks(written.map(({ at }) => at));
      return done;
    }, false, [cells[0].r, cells[0].c]);
  },

  /**
   * Says which of the cube's checks the values just written break, where it has checks that
   * only warn: the values are kept, and marked in the grid. A check that refuses them has
   * refused the write already.
   */
  async warnOfChecks(written) {
    if (!this.cube.validations?.length) return;
    const cells = written.slice(0, 1000).map((at) => ({ cube: this.cube.name, at }));
    const answer = await post('/cells/explain', { cells, transaction: this.tx ?? undefined }, { quiet: true }).catch(() => null);
    const broken = new Map();
    for (const check of (answer?.cells ?? []).flatMap((cell) => cell.cell?.invalid ?? [])) {
      broken.set(check.message, (broken.get(check.message) ?? 0) + 1);
    }
    if (!broken.size) return;
    const said = [...broken].map(([message, count]) => (count > 1 ? `${message} (${count.toLocaleString()} cells)` : message));
    notify(`Kept, but ${said.join(' ')}`, 'bad');
  },

  /** The value a cell of the result holds. */
  valueAt(r, c) {
    return this.result.cells[r * this.result.axes[0].tuples.length + c];
  },

  /** Empties the cells given that may be written and hold something: numbers to zero, text to nothing. */
  async clear(cells) {
    await this.explainRows(cells.map(([r]) => r));
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
   * and down. Cells that may not be written, empty values, text where a number belongs and
   * text a cell's picklist does not offer are left out, and the notice says how many.
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
    // Rows not yet drawn are asked about first: which of their cells may be written.
    const rowCount = this.result.axes[1]?.tuples.length ?? 1;
    await this.explainRows(targets.map(([r]) => r).filter((r) => r < rowCount));
    const cells = [];
    let leftOut = 0;
    for (const [r, c, raw] of targets) {
      if (raw.trim() === '') continue;
      if (!this.writable.has(`${r},${c}`)) {
        leftOut += 1;
        continue;
      }
      const value = typeof this.valueAt(r, c) === 'string' ? this.offeredAs(r, c, raw) : parseNumber(raw);
      if (value === null) leftOut += 1;
      else cells.push({ r, c, value });
    }
    if (!cells.length) {
      notify(leftOut ? 'None of the cells it would go into take it.' : 'There is nothing to paste.', 'bad');
      return;
    }
    if (!(await this.replacingDetail(cells.map(({ r, c }) => [r, c])))) return;
    this.focusAfter = [top, left];
    const pasted = `Pasted ${cells.length.toLocaleString()} value${cells.length === 1 ? '' : 's'}.`;
    await this.writeCells(cells, leftOut ? `${pasted} ${leftOut.toLocaleString()} left out: cells that cannot be written, or values they do not take.` : pasted);
  },

  /**
   * Whether values may go into cells given as [row, column], once the user agrees, where
   * some have supporting detail that a value written plainly takes away.
   */
  async replacingDetail(places) {
    const detailed = places.filter(([r, c]) => this.detailed.has(`${r},${c}`));
    if (!detailed.length) return true;
    const one = detailed.length === 1;
    return dialog({
      title: one ? 'Replace the cell\'s detail?' : `Replace the detail of ${detailed.length.toLocaleString()} cells?`,
      body: h('p', {}, `${one ? 'Its value is' : 'Their values are'} the total of supporting detail. A value written in takes the detail away; change the lines in the cell's Detail instead to keep them.`),
      confirm: 'Replace it',
      cancel: 'Keep the detail',
    });
  },

  /** Text as a cell takes it: as given, or as its picklist spells it; null where its picklist does not offer it. */
  offeredAs(r, c, text) {
    const picklist = this.picklistAt(r, c);
    if (!picklist) return text;
    const wanted = text.trim().toLowerCase();
    return picklist.find((value) => value.toLowerCase() === wanted) ?? null;
  },

  /** Pastes what the clipboard holds, where the browser lets the page read it. */
  async pasteFromClipboard() {
    let text;
    try {
      text = await navigator.clipboard.readText();
    } catch {
      notify(`The browser keeps the clipboard from the page. Press ${MOD} V to paste instead.`);
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
   * number to the total or takes it away; P% before a number changes every leaf by that
   * percentage; and C clears every leaf.
   */
  async spreadTyped(r, c, before, text, move) {
    this.focusAfter = [r + (move?.[0] ?? 0), c + (move?.[1] ?? 0)];
    const asked = spreadCode(text, typeof before === 'number' ? before : 0);
    if (!asked) {
      if (String(text).trim() !== '' && String(text) !== String(before ?? '')) notify(`“${text}” is neither a number nor a spreading code such as P100, S100, R100 or P%10.`, 'bad');
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
  /**
   * Gives the values a code that runs along the grid makes to its cell and every cell beyond
   * it to the end of its row or column that takes one, as TM1 does: a leaf is written, and a
   * total is spread over the leaves beneath it in proportion, before the leaves are written,
   * so that leaves it reaches as well keep the values given them. Held cells, text and cells
   * that take no value are passed over, and the notice says how many.
   */
  async spreadAlong(r, c, code, move) {
    this.focusAfter = [r + (move?.[0] ?? 0), c + (move?.[1] ?? 0)];
    if (!this.mayWrite()) {
      notify(this.whyNot(r, c), 'bad');
      this.focusCell();
      return;
    }
    const rows = this.result.axes[1]?.tuples.length ?? 1;
    const columns = this.result.axes[0].tuples.length;
    const [down, across] = code.step;
    const reached = [];
    for (let [row, column] = [r, c]; row >= 0 && row < rows && column >= 0 && column < columns; row += down, column += across) {
      reached.push([row, column]);
    }
    await this.explainRows(reached.map(([row]) => row));
    // A cell is text where one of its members is a string element, empty or not.
    const [columnAxis, rowAxis] = this.result.axes;
    const text = (row, column) => [...(rowAxis?.tuples[row] ?? []), ...columnAxis.tuples[column], ...(this.result.slicer ?? [])]
      .some((m) => m.type === 'string');
    const takes = ([row, column]) => !text(row, column)
      && !(this.holds.size && this.holds.has(this.key(row, column)))
      && (this.writable.has(`${row},${column}`) || this.spreadable(row, column));
    const targets = reached.filter(takes);
    const passed = reached.length - targets.length;
    if (!targets.length) {
      notify(passed === 1 ? this.whyNot(r, c) : 'None of the cells it reaches takes a value.', 'bad');
      this.focusCell();
      return;
    }
    const values = code.values(targets.map(([row, column]) => {
      const before = this.valueAt(row, column);
      return typeof before === 'number' ? before : 0;
    }));
    const given = targets.map(([row, column], index) => ({ r: row, c: column, value: values[index] }));
    const totals = given.filter(({ r: row, c: column }) => this.spreadable(row, column));
    const leaves = given.filter(({ r: row, c: column }) => !this.spreadable(row, column));
    const count = leaves.length + totals.reduce((all, { r: row, c: column }) => all + this.leavesBeneath(row, column), 0);
    const emptying = given.every(({ value }) => value === 0);
    if (count > SPREAD_ASKS || (emptying && count > 1)) {
      const agreed = await dialog({
        title: emptying ? `Empty ${count.toLocaleString()} cells?` : `Change ${count.toLocaleString()} cells?`,
        body: h('p', {}, `From ${this.coordinate(r, c).join(' · ')} on, ${given.length.toLocaleString()} cells of the grid ${emptying ? 'become empty' : 'take new values'}. Nothing is stored until you save your changes, and Discard takes it back.`),
        confirm: emptying ? 'Empty them' : 'Change them',
        danger: emptying,
      });
      if (!agreed) {
        this.focusCell();
        return;
      }
    }
    if (!(await this.replacingDetail(leaves.map(({ r: row, c: column }) => [row, column])))) {
      this.focusCell();
      return;
    }
    const holds = [...this.holds].map((key) => JSON.parse(key));
    await this.changeValues(`the values from ${this.coordinate(r, c).join(' · ')} on`, async () => {
      for (const { r: row, c: column, value } of totals) {
        await post('/cells/spread', { transaction: this.tx, spread: { cube: this.cube.name, at: this.coordinate(row, column), value, method: 'proportional', holds } });
      }
      const written = leaves.map(({ r: row, c: column, value }) => ({ at: this.coordinate(row, column), value }));
      if (written.length) {
        await post('/cells/write', { transaction: this.tx, cells: written.map(({ at, value }) => ({ cube: this.cube.name, at, value })) });
        if (!this.sandbox) for (const { at, value } of written) this.edits.set(JSON.stringify(at), value);
        await this.warnOfChecks(written.map(({ at }) => at));
      }
      this.changes = (this.changes ?? 0) + given.length;
      const left = passed ? ` ${passed.toLocaleString()} cell${passed === 1 ? '' : 's'} took no value.` : '';
      return `Gave ${given.length.toLocaleString()} cell${given.length === 1 ? '' : 's'} new values.${left}`;
    }, false, [r, c]);
  },

  async spreadValue(r, c, value, method) {
    const at = this.coordinate(r, c);
    const holds = [...this.holds].map((key) => JSON.parse(key));
    await this.changeValues(`the spread over ${at.join(' · ')}`, async () => {
      const answer = await post('/cells/spread', { transaction: this.tx, spread: { cube: this.cube.name, at, value, method, holds } });
      this.changes = (this.changes ?? 0) + 1;
      return `Spread over ${answer.written.toLocaleString()} cell${answer.written === 1 ? '' : 's'}.`;
    }, false, [r, c]);
  },

  async ensureTransaction() {
    if (this.tx) return;
    const answer = await post('/transactions');
    this.tx = answer.transaction;
  },

  /**
   * Stores every change made since the last save, in one step. A sandbox's are published,
   * every cell it changes, and it stays, empty.
   */
  async save() {
    const published = this.sandbox?.pending;
    try {
      await post(`/transactions/${this.tx}/commit`);
    } catch (problem) {
      this.refused(problem);
      return;
    }
    notify(this.sandbox
      ? `Published ${published.toLocaleString()} changed cell${published === 1 ? '' : 's'} from ${this.sandbox.name} to the stored values.`
      : `Saved ${this.changes} change${this.changes === 1 ? '' : 's'}.`, 'good');
    this.forget();
    await this.run();
  },

  /** Lets every change made since the last save go: a sandbox's, all it holds, once the user agrees. */
  async discard() {
    if (this.sandbox && !(await this.discarding())) return;
    try {
      await post(`/transactions/${this.tx}/rollback`);
    } catch {
      // Gone already: discarded either way.
    }
    this.forget();
    await this.run();
  },

  /** Forgets the changes of values made so far: the edit transaction goes, a sandbox stays. */
  forget() {
    this.tx = this.sandbox?.transaction ?? null;
    this.edits.clear();
    this.changes = 0;
    // Changes of values went with the transaction; the layout's history stays.
    this.past = this.past.filter((entry) => typeof entry === 'string');
    this.future = this.future.filter((entry) => typeof entry === 'string');
    this.drawToolbar();
  },

  /** Reports a write the server refused; a transaction it no longer has is let go. */
  refused(problem) {
    const gone = problem.code === 'unknown_transaction';
    if (gone && this.sandbox) {
      this.lostSandbox();
      return;
    }
    if (gone && this.tx) {
      notify('Your unsaved changes were dropped after a long pause. Make them again to carry on.', 'bad');
      this.forget();
      this.run();
      return;
    }
    notify(problem.message, 'bad');
    this.focusCell();
  },

  /** Puts the keys back on the cell they are to go to after a change, brought into view. */
  focusCell() {
    if (!this.focusAfter) return;
    const [r, c] = this.focusAfter;
    this.focusAfter = null;
    this.selection?.activate(r, c);
  },
};
