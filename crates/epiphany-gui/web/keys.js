// API keys: what programs use to reach the server in a user's name, each given what it may do.

import { del, get, part, post } from './api.js';
import { empty, failure, loading, model, state } from './app.js';
import { choices, dialog, h, icon, invalid, mount, notify, when, working } from './ui.js';

/** How far a key may reach into a part, from nowhere on. */
const LEVELS = ['none', 'read', 'write'];

/** The parts of the server a key reaches into, and what each step into one is called. */
const PARTS = [
  {
    key: 'cells',
    title: 'Cells',
    steps: ['No access', 'Read', 'Read and write'],
    hint: 'Reading cells, queries, views and history; writing and spreading values, with transactions, locks, sandboxes and comments.',
  },
  { key: 'model', title: 'The model', steps: ['No access', 'Read', 'Read and change'], hint: 'Its dimensions, cubes and dashboards.' },
  {
    key: 'admin',
    title: 'Administration',
    steps: ['No access', 'Read', 'Read and manage'],
    hint: 'The server’s state, its users and their sessions, the audit and the changes to cells.',
  },
];

/** How long a new key lasts, in days; one given none lasts until it is revoked. */
const LASTING = [['30', 'For 30 days'], ['90', 'For 90 days'], ['365', 'For a year'], ['', 'Until revoked']];

/** What a key may do, said briefly: Cells: read and write · The model: read. */
function reach(key) {
  const reached = PARTS.filter((each) => key[each.key] !== 'none');
  const said = reached.map((each) => `${each.title}: ${each.steps[LEVELS.indexOf(key[each.key])].toLowerCase()}`);
  return said.join(' · ') || 'Nothing';
}

/** Until when a key works: until revoked, a time to come, or no longer. */
function lasts(key) {
  if (!key.expires) return h('span', { class: 'muted' }, 'Until revoked');
  if (key.expires * 1000 <= Date.now()) return h('span', { class: 'badge bad' }, 'Expired');
  return h('span', {}, 'Until ', when(key.expires));
}

/** Keys in a table, each with a button that revokes it where `revoke` is given. */
export function keyTable(keys, revoke = null) {
  return h('table', { class: 'table' },
    h('thead', {}, h('tr', {},
      h('th', {}, 'Name'), h('th', {}, 'May'), h('th', {}, 'Cubes'), h('th', {}, 'Made'), h('th', {}, 'Last used'), h('th', {}, 'Lasts'),
      revoke ? h('th', { class: 'actions' }, '') : null)),
    h('tbody', {}, keys.map((key) => h('tr', {},
      h('td', {}, h('strong', {}, key.name), h('div', { class: 'mono muted' }, `epk_${key.id}…`)),
      h('td', {}, reach(key)),
      h('td', { class: 'muted' }, key.cubes?.length ? key.cubes.join(', ') : 'Every cube'),
      h('td', { class: 'muted nowrap' }, when(key.created)),
      // A key unused for long is one to think of revoking.
      h('td', { class: 'nowrap' }, key.used ? when(key.used) : h('span', { class: 'muted' }, 'Never')),
      h('td', { class: 'nowrap' }, lasts(key)),
      // Not one that submits, for the table may stand in a dialog's form.
      revoke ? h('td', { class: 'actions' }, h('button', {
        class: 'button small quiet danger',
        type: 'button',
        'aria-label': `Revoke ${key.name}`,
        onclick: () => revoke(key),
      }, icon('trash', 14), 'Revoke')) : null))));
}

export async function render(target) {
  return new KeysPage(target);
}

class KeysPage {
  constructor(target) {
    this.card = h('section', { class: 'card fit', 'aria-label': 'Your API keys' });
    this.makeButton = h('button', { class: 'button primary', hidden: true, onclick: (event) => this.make(event.currentTarget) }, icon('plus', 16), 'New key');
    mount(target, h('div', { class: 'page' },
      h('div', { class: 'page-head' },
        h('div', { class: 'grow' }, h('h1', {}, 'API keys'),
          h('p', {}, 'Programs reach the server in your name with a key: a script, a loader, a report. Each does only what you give it and never more than you may, until you revoke it.')),
        this.makeButton),
      this.card));
    mount(this.card, h('div', { class: 'card-body' }, loading('lines')));
    this.load();
  }

  async load() {
    try {
      this.answer = await get('/keys');
    } catch (problem) {
      mount(this.card, failure(problem, () => this.load()));
      return;
    }
    this.draw();
  }

  draw() {
    const { allowed, keys } = this.answer;
    // A view as another user reads their keys, and makes and revokes none.
    const viewing = Boolean(state.user?.viewer);
    const makes = allowed && !viewing;
    this.makeButton.hidden = !makes;
    if (!keys.length) {
      mount(this.card, allowed
        ? empty('key', 'No keys yet', 'Make one for each program, so that each can be revoked alone.',
          ...(makes ? [h('button', { class: 'button primary', onclick: (event) => this.make(event.currentTarget) }, icon('plus', 16), 'New key')] : []))
        : empty('key', 'Keys are not open to you', 'An administrator lets a user make API keys. Ask one if a program of yours needs to reach the server.'));
      return;
    }
    mount(this.card,
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Your keys'),
        h('span', { class: 'muted' }, 'Each is held to what you may do at the time, whatever it was given.')),
      keyTable(keys, viewing ? null : (key) => this.revoke(key)));
  }

  /** Asks what a new key is for and may do, makes it, and shows it the one time it is shown. */
  async make(button) {
    // The cubes a key may be kept to.
    const definition = await working(button, () => model());
    if (!definition) return;
    const { most } = this.answer;
    const reachable = (each) => LEVELS.indexOf(most[each.key]);
    const at = (each, level) => LEVELS[Math.min(LEVELS.indexOf(level), reachable(each))];
    const name = h('input', { class: 'input', id: 'key-name', required: true, autocomplete: 'off', maxlength: '100', placeholder: 'What it is for, such as Nightly load' });
    const lasting = h('select', { class: 'select', id: 'key-lasting' }, LASTING.map(([value, label]) => h('option', { value, selected: value === '90' }, label)));
    // Reading, to begin with: everything the user may do is a click away.
    const chosen = Object.fromEntries(PARTS.map((each) => [each.key, at(each, each.key === 'admin' ? 'none' : 'read')]));
    const offered = PARTS.filter((each) => reachable(each) > 0);
    const rows = Object.fromEntries(offered.map((each) => [each.key, h('div')]));
    const every = h('input', { type: 'radio', name: 'key-cubes', id: 'key-every-cube', checked: true });
    const only = h('input', { type: 'radio', name: 'key-cubes', id: 'key-only-cubes' });
    const boxes = definition.cubes.map((cube) => h('label', { class: 'check' }, h('input', { type: 'checkbox', value: cube.name }), cube.name));
    const list = h('div', { class: 'checks key-cubes' }, boxes);
    const cubesHint = h('span', { class: 'hint' });
    const cubes = h('fieldset', { class: 'field' }, h('legend', {}, 'Cubes'),
      h('label', { class: 'check' }, every, 'Every cube'),
      h('label', { class: 'check' }, only, 'Only these'),
      list, cubesHint);
    const draw = () => {
      for (const each of offered) {
        choices({
          label: each.title,
          kind: 'radiogroup',
          options: LEVELS.slice(0, reachable(each) + 1).map((level, step) => [level, each.steps[step]]),
          chosen: chosen[each.key],
          choose: (level) => {
            chosen[each.key] = level;
            draw();
          },
          into: rows[each.key],
        });
      }
      // Changing the model or administering the server reaches every cube.
      const wide = chosen.model === 'write' || chosen.admin !== 'none';
      if (wide) every.checked = true;
      only.disabled = wide;
      list.hidden = !only.checked;
      cubes.hidden = chosen.cells === 'none';
      cubesHint.textContent = wide ? 'A key that changes the model or reaches administration reaches every cube.' : 'Kept to cubes, the key reads and writes their cells and no others.';
    };
    every.addEventListener('change', draw);
    only.addEventListener('change', draw);
    const choose = (levels) => {
      for (const each of PARTS) chosen[each.key] = levels[each.key];
      draw();
    };
    const presets = h('div', { class: 'toolbar' },
      h('button', { class: 'button small', type: 'button', onclick: () => choose({ cells: at(PARTS[0], 'read'), model: at(PARTS[1], 'read'), admin: 'none' }) }, icon('eye', 14), 'Reading only'),
      h('button', { class: 'button small', type: 'button', onclick: () => choose(most) }, icon('check', 14), 'Everything you may do'));
    draw();
    const made = await dialog({
      title: 'New API key',
      confirm: 'Make key',
      wide: true,
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'key-name' }, 'Name'), name),
        h('fieldset', { class: 'field' }, h('legend', {}, 'What it may do'), presets,
          h('div', { class: 'choice-rows' }, offered.map((each) => h('div', { class: 'choice-row' },
            h('div', {}, h('strong', {}, each.title), h('div', { class: 'hint' }, each.hint)),
            rows[each.key]))),
          most.admin === 'none' ? h('span', { class: 'hint' }, 'Changing the model and administration take an administrator.') : null),
        cubes,
        h('div', { class: 'field' }, h('label', { for: 'key-lasting' }, 'Lasts'), lasting)),
      onsubmit: async () => {
        const named = name.value.trim();
        if (!named) throw invalid(name, 'Give the key a name, to tell it from your others.');
        if (PARTS.every((each) => chosen[each.key] === 'none')) throw invalid(null, 'Give the key something it may do.');
        const kept = only.checked && !cubes.hidden
          ? boxes.map((box) => box.querySelector('input')).filter((box) => box.checked).map((box) => box.value)
          : [];
        if (only.checked && !cubes.hidden && !kept.length) throw invalid(boxes[0]?.querySelector('input') ?? only, 'Choose the cubes it reaches, or every cube.');
        const asked = { name: named, ...chosen, cubes: kept };
        const days = Number(lasting.value);
        if (days) asked.expires = Math.floor(Date.now() / 1000) + days * 86_400;
        return post('/keys', asked);
      },
    });
    if (!made) return;
    await this.shown(made);
    this.load();
  }

  /** Shows a key just made: the one time it is shown, for only a hash of it is kept. */
  shown(made) {
    const field = h('input', { class: 'input mono', readonly: true, value: made.key, 'aria-label': `The key ${made.name}` });
    field.addEventListener('focus', () => field.select());
    const copy = h('button', {
      class: 'button',
      type: 'button',
      onclick: async () => {
        try {
          await navigator.clipboard.writeText(made.key);
          notify('The key is copied.', 'good');
        } catch {
          // Where the page may not write to the clipboard, the key is chosen to be copied by hand.
          field.focus();
          notify('Copy it with the keyboard: it is chosen.');
        }
      },
    }, icon('copy', 16), 'Copy');
    // Wide enough for the whole key to be seen.
    return dialog({
      title: `Your key ${made.name}`,
      confirm: 'Done',
      cancel: null,
      wide: true,
      body: h('div', { class: 'facts' },
        h('p', {}, h('strong', {}, 'Copy it now. '), 'It is shown this once: only a hash of it is kept, and it cannot be shown again.'),
        h('div', { class: 'key-shown' }, field, copy),
        h('p', { class: 'hint' }, 'A program sends it with every request, as the header ', h('code', {}, 'Authorization: Bearer ', h('em', {}, 'key')), '.')),
    });
  }

  async revoke(key) {
    const revoked = await dialog({
      title: `Revoke ${key.name}?`,
      body: h('p', {}, 'A program using it is refused from its next request. It cannot be undone; a new key would be needed.'),
      confirm: 'Revoke key',
      danger: true,
      onsubmit: async () => {
        await del(`/keys/${part(key.id)}`);
        return true;
      },
    });
    if (!revoked) return;
    notify(`Revoked ${key.name}.`, 'good');
    this.load();
  }
}
