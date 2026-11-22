// Working in a sandbox: changes kept apart under a name, on the server, until they are
// published to the stored values, and the cells they change marked against the stored ones.
// Methods of the explorer, kept apart to keep each part readable.

import { del, get, part, post } from './api.js';
import { state } from './app.js';
import { dialog, formatValue, h, icon, menu, mount, notify } from './ui.js';

/** Where the sandbox someone last worked in is remembered, by their name. */
const remembered = () => `epiphany.sandbox.${state.user?.name ?? ''}`;
const COMPARING = 'epiphany.sandbox.compare';

/** "Its 3 changed cells go", or "Its changed cell goes", for what happens to a sandbox's cells. */
const cellsGo = (count) => (count === 1 ? 'Its changed cell goes' : `Its ${count.toLocaleString()} changed cells go`);

/** Whether two values of a cell differ. Empty is as zero, and numbers a rounding apart are one. */
function differs(a, b) {
  const plain = (value) => (value === null || value === undefined || value === '' ? 0 : value);
  const [x, y] = [plain(a), plain(b)];
  if (typeof x === 'number' && typeof y === 'number') return Math.abs(x - y) > 1e-9 * Math.max(1, Math.abs(x), Math.abs(y));
  return x !== y;
}

/**
 * The stored values of a sandbox's answer, cell for cell: the stored answer to the same
 * query, matched by members, for the empty rows and columns left out may differ between them.
 */
function alongside(result, stored) {
  const key = (tuple) => tuple.map((m) => m.unique_name).join('\u0001');
  const places = (axis) => new Map((axis?.tuples.length ? axis.tuples : [[]]).map((tuple, index) => [key(tuple), index]));
  const [storedColumns, storedRows] = [places(stored.axes[0]), places(stored.axes[1])];
  const width = stored.axes[0]?.tuples.length || 1;
  const [columns, rows] = result.axes;
  const across = (columns?.tuples.length ? columns.tuples : [[]]).map((tuple) => storedColumns.get(key(tuple)));
  const down = (rows?.tuples.length ? rows.tuples : [[]]).map((tuple) => storedRows.get(key(tuple)));
  return down.flatMap((r) => across.map((c) => (r === undefined || c === undefined ? null : stored.cells[r * width + c])));
}

export const sandboxes = {
  /** Takes up the sandbox last worked in, where it is still there, from the user's list. */
  resume(list) {
    this.sandboxes = list;
    this.sandbox = list.find((sandbox) => sandbox.name === localStorage.getItem(remembered())) ?? null;
    this.tx = this.sandbox?.transaction ?? null;
    this.compare = localStorage.getItem(COMPARING) === 'true';
  },

  /** Whether the user may write to some cube, and so keep sandboxes to write in. */
  mayKeep() {
    const roles = [state.user?.role, ...Object.values(state.user?.cubes ?? {})];
    return roles.some((role) => role === 'write' || role === 'admin');
  },

  /** Says where the grid's values come from, on the button that opens the sandbox menu. */
  drawSandboxButton() {
    const button = this.sandboxButton;
    button.hidden = !this.mayKeep();
    button.classList.toggle('in-sandbox', Boolean(this.sandbox));
    button.title = this.sandbox
      ? `Working in the sandbox ${this.sandbox.name}: changes wait there, yours alone, until you publish them`
      : 'Working on the stored values. A sandbox keeps changes apart until you publish them';
    mount(button, icon('sandbox', 16), h('span', { class: 'clip' }, this.sandbox?.name ?? 'Stored values'), icon('down', 14));
  },

  /** The sandbox menu: where to work, and what to do with the sandbox worked in. */
  async sandboxMenu(button) {
    // The list as it is now: another window may have changed it.
    try {
      this.takeSandboxes(await get('/sandboxes', { quiet: true }));
    } catch {
      // The list as last known.
    }
    const current = this.sandbox;
    const pending = current?.pending ?? 0;
    const changed = (count) => (count ? ` · ${count.toLocaleString()} changed` : '');
    menu([
      { heading: 'Work in' },
      { label: 'Stored values', icon: current ? 'cube' : 'check', action: () => this.useSandbox(null) },
      ...this.sandboxes.map((sandbox) => ({
        label: `${sandbox.name}${changed(sandbox.pending)}`,
        icon: sandbox.transaction === current?.transaction ? 'check' : 'sandbox',
        action: () => this.useSandbox(sandbox),
      })),
      '-',
      { label: 'New sandbox…', icon: 'plus', action: () => this.newSandbox() },
      current ? '-' : null,
      current ? { label: this.compare ? 'Stop showing stored values' : 'Show stored values beside', icon: 'eye', action: () => this.showStored(!this.compare) } : null,
      current ? { label: 'Publish to stored values', icon: 'publish', disabled: !pending, action: () => this.save() } : null,
      current ? { label: 'Discard its changes…', icon: 'undo', disabled: !pending, action: () => this.discard() } : null,
      current ? { label: `Delete ${current.name}…`, icon: 'trash', action: () => this.deleteSandbox() } : null,
    ], button);
  },

  /** The user's sandboxes as the server lists them, the one worked in among them brought up to date. */
  takeSandboxes(list) {
    this.sandboxes = list;
    const current = list.find((sandbox) => sandbox.transaction === this.sandbox?.transaction);
    if (current) this.sandbox = current;
  },

  /**
   * Works in a sandbox, or on the stored values for none. Unsaved changes to the stored
   * values are settled first, as leaving the page would settle them.
   */
  async useSandbox(sandbox) {
    if ((sandbox?.transaction ?? null) === (this.sandbox?.transaction ?? null)) return;
    if (!(await this.leave())) return;
    this.sandbox = sandbox;
    if (sandbox) localStorage.setItem(remembered(), sandbox.name);
    else localStorage.removeItem(remembered());
    this.stored = null;
    this.forget();
    notify(sandbox ? `Working in ${sandbox.name}. Changes wait there until you publish them.` : 'Working on the stored values.');
    await this.run();
  },

  async newSandbox() {
    const name = h('input', { class: 'input', id: 'sandbox-name', required: true, maxlength: 100, autocomplete: 'off' });
    const made = await dialog({
      title: 'New sandbox',
      confirm: 'Make it',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'sandbox-name' }, 'Name'), name),
        h('p', { class: 'hint' }, 'Changes made in a sandbox are yours alone, kept on the server, across every cube, until you publish them to the stored values or discard them. Totals in it include them.')),
      onsubmit: async () => {
        const wanted = name.value.trim();
        if (!wanted) throw new Error('Give the sandbox a name.');
        return post('/sandboxes', { name: wanted });
      },
    });
    if (!made) return;
    this.sandboxes = [...this.sandboxes, made];
    await this.useSandbox(made);
  },

  async deleteSandbox() {
    const sandbox = this.sandbox;
    const count = sandbox.pending;
    const deleted = await dialog({
      title: `Delete ${sandbox.name}?`,
      body: h('p', {}, count
        ? `${cellsGo(count)} with it, never published. This cannot be undone.`
        : 'It holds no changes.'),
      confirm: 'Delete',
      danger: true,
      onsubmit: async () => {
        await del(`/sandboxes/${part(sandbox.name)}`);
        return true;
      },
    });
    if (!deleted) return;
    notify(`Deleted ${sandbox.name}.`);
    this.leaveSandbox();
  },

  /** Asks before every change a sandbox holds is let go; says whether the user agreed. */
  async discarding() {
    const count = this.sandbox.pending;
    return dialog({
      title: `Discard the changes in ${this.sandbox.name}?`,
      body: h('p', {}, `${cellsGo(count)} back to the stored values. The sandbox stays, empty. This cannot be undone.`),
      confirm: 'Discard',
      danger: true,
    });
  },

  /** Works on the stored values again, the sandbox worked in being gone. */
  leaveSandbox() {
    this.sandboxes = this.sandboxes.filter((each) => each.transaction !== this.sandbox?.transaction);
    this.sandbox = null;
    localStorage.removeItem(remembered());
    this.stored = null;
    this.forget();
    this.run();
  },

  /** The sandbox worked in has gone, deleted from another window. */
  lostSandbox() {
    notify(`The sandbox ${this.sandbox.name} is gone: it was deleted elsewhere. Showing the stored values.`, 'bad');
    this.leaveSandbox();
  },

  /** Shows the stored value beside each value the sandbox changes, or stops. */
  showStored(compare) {
    this.compare = compare;
    localStorage.setItem(COMPARING, String(compare));
    this.draw();
  },

  /**
   * Lines the stored answer up beside the sandbox's, once both have come; counts the cells
   * whose values differ.
   */
  compareStored(result, stored) {
    this.stored = stored ? alongside(result, stored) : null;
    this.differing = this.stored ? this.stored.reduce((count, value, index) => count + (differs(value, result.cells[index]) ? 1 : 0), 0) : 0;
  },

  /**
   * The stored value of a cell of a sandbox's grid, as the view formats it, where it differs
   * from the sandbox's; null where it does not, and outside a sandbox.
   */
  storedText(r, c) {
    if (!this.stored) return null;
    const index = r * this.result.axes[0].tuples.length + c;
    const [stored, value] = [this.stored[index], this.result.cells[index]];
    if (!differs(stored, value)) return null;
    return String(formatValue(stored ?? (typeof value === 'number' ? 0 : ''), this.layout.format));
  },
};
