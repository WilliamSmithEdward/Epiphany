// The set editor: a dimension's members on one side, the set being built on the other.

import { beneath, marked, openButtons, reveal, Rows, searchBox, treeKey, treeRow, treeRows, twisty } from './tree.js';
import { dialog, h, icon, mount, notify } from './ui.js';

/**
 * Edits the set of members a dimension shows on an axis. `dim` is the dimension as the
 * explorer holds it (elements, byName, roots, levels, subsets, attributes); `chosen` the
 * members in their order. Resolves with the new set, or null if it is dismissed.
 */
export function editSet(dim, chosen) {
  return new SetEditor(dim, chosen).show();
}

class SetEditor {
  constructor(dim, chosen) {
    this.dim = dim;
    this.set = [...chosen];
    this.past = [];
    this.future = [];
    this.open = new Set(dim.roots);
    this.picked = new Set();
    this.marked = new Set();
    this.words = '';
    this.withDescendants = false;
    this.showAttributes = false;
    this.attributes = Object.keys(dim.attributes ?? {});
    // Rules the available members must meet: each a field, a test and a value.
    this.rules = [];
    this.matchAll = true;
    this.filtering = false;
  }

  /* ---- Filtering by rules ---- */

  /** The fields a rule can test: name, level, type and each attribute. */
  fields() {
    return [
      { id: 'name', label: 'Name', kind: 'text' },
      { id: 'level', label: 'Level', kind: 'level' },
      { id: 'type', label: 'Type', kind: 'type' },
      ...this.attributes.map((attribute) => ({
        id: `attribute:${attribute}`,
        label: attribute,
        kind: this.dim.attributes[attribute] === 'numeric' ? 'number' : 'text',
      })),
    ];
  }

  field(id) {
    return this.fields().find((field) => field.id === id) ?? this.fields()[0];
  }

  /** The tests a field's kind allows. */
  tests(kind) {
    const empty = [['empty', 'is empty'], ['present', 'is not empty']];
    if (kind === 'text') {
      return [['contains', 'contains'], ['equals', 'is'], ['differs', 'is not'], ['starts', 'starts with'], ['ends', 'ends with'], ['lacks', 'does not contain'], ...empty];
    }
    if (kind === 'type') return [['equals', 'is'], ['differs', 'is not']];
    const order = [['equals', '='], ['differs', '≠'], ['above', '>'], ['from', '≥'], ['below', '<'], ['to', '≤']];
    return kind === 'level' ? order : [...order, ...empty];
  }

  value(element, field) {
    if (field.id === 'name') return element.name;
    if (field.id === 'level') return element.level;
    if (field.id === 'type') return element.type;
    return element.attributes?.[field.id.slice('attribute:'.length)];
  }

  /** The rules that say something: those with a value, or a test that needs none. */
  activeRules() {
    return this.rules.filter((rule) => rule.test === 'empty' || rule.test === 'present' || String(rule.value ?? '').trim() !== '');
  }

  meets(element, rule) {
    const field = this.field(rule.field);
    const actual = this.value(element, field);
    const missing = actual === undefined || actual === null || actual === '';
    if (rule.test === 'empty') return missing;
    if (rule.test === 'present') return !missing;
    if (field.kind === 'number' || field.kind === 'level') {
      if (missing) return rule.test === 'differs';
      const [a, b] = [Number(actual), Number(rule.value)];
      if (!Number.isFinite(b)) return false;
      return { equals: a === b, differs: a !== b, above: a > b, from: a >= b, below: a < b, to: a <= b }[rule.test] ?? false;
    }
    const a = String(actual ?? '').toLowerCase();
    const b = String(rule.value ?? '').trim().toLowerCase();
    return {
      contains: a.includes(b),
      lacks: !a.includes(b),
      equals: a === b,
      differs: a !== b,
      starts: a.startsWith(b),
      ends: a.endsWith(b),
    }[rule.test] ?? false;
  }

  /** Whether a member passes the rules: all of them, or any, as chosen. */
  passes(element) {
    const rules = this.activeRules();
    if (!rules.length) return true;
    return this.matchAll ? rules.every((rule) => this.meets(element, rule)) : rules.some((rule) => this.meets(element, rule));
  }

  /** Every member the search, and the rules while filtering, let through, in the dimension's order. */
  matches() {
    return this.dim.elements.filter((element) => this.named(element) && (!this.filtering || this.passes(element)));
  }

  /** Whether a member's name has the words searched for, or one of its attributes shown does. */
  named(element) {
    return !this.words || element.name.toLowerCase().includes(this.words)
      || (this.showAttributes && Object.values(element.attributes ?? {}).some((value) => String(value).toLowerCase().includes(this.words)));
  }

  /** Whether the tree shows what was searched or filtered for, rather than all of it. */
  searching() {
    return Boolean(this.words) || (this.filtering && this.activeRules().length > 0);
  }

  drawFilter() {
    this.filterPanel.hidden = !this.filtering;
    this.filterButton.setAttribute('aria-pressed', String(this.filtering));
    const active = this.activeRules().length;
    this.filterButton.querySelector('.filter-count').textContent = active ? String(active) : '';
    if (!this.filtering) return;
    const levelName = (level) => this.dim.levels?.[level] || (level === 0 ? 'Leaves' : `Level ${level}`);
    const levels = [...new Set(this.dim.elements.map((element) => element.level))].sort((a, b) => a - b);
    const rows = this.rules.map((rule, index) => {
      const field = this.field(rule.field);
      const tests = this.tests(field.kind);
      if (!tests.some(([id]) => id === rule.test)) rule.test = tests[0][0];
      const redraw = () => {
        this.drawFilter();
        this.drawLeft();
      };
      const fieldSelect = h('select', {
        class: 'select',
        'aria-label': 'Field',
        onchange: (event) => {
          rule.field = event.target.value;
          rule.value = '';
          redraw();
        },
      }, this.fields().map((option) => h('option', { value: option.id, selected: option.id === field.id }, option.label)));
      const testSelect = h('select', {
        class: 'select',
        'aria-label': 'Test',
        onchange: (event) => {
          rule.test = event.target.value;
          redraw();
        },
      }, tests.map(([id, label]) => h('option', { value: id, selected: id === rule.test }, label)));
      const needsValue = rule.test !== 'empty' && rule.test !== 'present';
      let value = null;
      if (needsValue && field.kind === 'type') {
        if (!rule.value) rule.value = 'consolidated';
        value = h('select', { class: 'select', 'aria-label': 'Value', onchange: (event) => { rule.value = event.target.value; this.drawLeft(); } },
          [['numeric', 'Number'], ['string', 'Text'], ['consolidated', 'Consolidation']].map(([id, label]) => h('option', { value: id, selected: id === rule.value }, label)));
      } else if (needsValue && field.kind === 'level') {
        if (rule.value === '') rule.value = String(levels[0] ?? 0);
        value = h('select', { class: 'select', 'aria-label': 'Value', onchange: (event) => { rule.value = event.target.value; this.drawLeft(); } },
          levels.map((level) => h('option', { value: level, selected: String(level) === String(rule.value) }, levelName(level))));
      } else if (needsValue) {
        value = h('input', {
          class: 'input',
          'aria-label': 'Value',
          type: field.kind === 'number' ? 'number' : 'text',
          step: 'any',
          placeholder: field.kind === 'number' ? 'A number' : 'Text',
          oninput: (event) => {
            rule.value = event.target.value;
            this.drawLeft();
            this.drawFilterCount();
          },
          onkeydown: (event) => {
            if (event.key === 'Enter') event.preventDefault();
          },
        });
        value.value = rule.value ?? '';
      }
      return h('div', { class: 'rule' },
        h('span', { class: 'rule-join muted' }, index === 0 ? 'Where' : this.matchAll ? 'and' : 'or'),
        fieldSelect, testSelect, value ?? h('span'),
        h('button', {
          class: 'button quiet small icon',
          type: 'button',
          'aria-label': 'Remove this rule',
          onclick: () => {
            this.rules.splice(index, 1);
            redraw();
          },
        }, icon('close', 14)));
    });
    this.filterCount = h('span', { class: 'muted' });
    mount(this.filterPanel,
      rows.length ? h('div', { class: 'rules' }, rows) : h('p', { class: 'hint' }, 'Add a rule to narrow the members: by name, level, type or any attribute.'),
      h('div', { class: 'toolbar' },
        h('button', {
          class: 'button small',
          type: 'button',
          onclick: () => {
            this.rules.push({ field: this.attributes.length ? `attribute:${this.attributes[0]}` : 'name', test: 'contains', value: '' });
            this.drawFilter();
            this.filterPanel.querySelector('.rule:last-child input, .rule:last-child select')?.focus();
          },
        }, icon('plus', 14), 'Add a rule'),
        this.rules.length > 1 ? h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'Combine the rules' },
          h('button', { type: 'button', role: 'tab', 'aria-selected': String(this.matchAll), onclick: () => { this.matchAll = true; this.drawFilter(); this.drawLeft(); } }, 'Match all'),
          h('button', { type: 'button', role: 'tab', 'aria-selected': String(!this.matchAll), onclick: () => { this.matchAll = false; this.drawFilter(); this.drawLeft(); } }, 'Match any')) : null,
        h('div', { class: 'grow' }),
        this.filterCount),
      h('div', { class: 'toolbar' },
        h('button', { class: 'button small', type: 'button', onclick: () => this.add(this.matches().map((element) => element.name)) }, icon('chevron', 14), 'Add the matches'),
        h('button', {
          class: 'button small',
          type: 'button',
          onclick: () => this.change(() => {
            this.set = this.matches().map((element) => element.name);
          }),
        }, icon('swap', 14), 'Replace the set with them'),
        h('div', { class: 'grow' }),
        this.rules.length ? h('button', { class: 'button small quiet', type: 'button', onclick: () => { this.rules = []; this.drawFilter(); this.drawLeft(); } }, 'Clear the rules') : null));
    this.drawFilterCount();
  }

  drawFilterCount() {
    if (!this.filterCount) return;
    const count = this.matches().length;
    this.filterCount.textContent = `${count.toLocaleString()} member${count === 1 ? '' : 's'} match`;
    const active = this.activeRules().length;
    this.filterButton.querySelector('.filter-count').textContent = active ? String(active) : '';
  }

  /* ---- Changing the set, with undo ---- */

  change(edit) {
    this.past.push([...this.set]);
    this.future = [];
    edit();
    this.set = [...new Set(this.set)];
    this.draw();
  }

  undo() {
    if (!this.past.length) return;
    this.future.push([...this.set]);
    this.set = this.past.pop();
    this.draw();
  }

  redo() {
    if (!this.future.length) return;
    this.past.push([...this.set]);
    this.set = this.future.pop();
    this.draw();
  }

  add(names) {
    const fresh = names.filter((name) => !this.set.includes(name));
    if (fresh.length) this.change(() => this.set.push(...fresh));
  }

  /* ---- What a member reaches ---- */

  children(name) {
    return (this.dim.byName.get(name)?.children ?? []).map((child) => child.name).filter((child) => this.dim.byName.has(child));
  }

  descendants(name, out = [], seen = new Set()) {
    for (const child of this.children(name)) {
      if (seen.has(child)) continue;
      seen.add(child);
      out.push(child);
      this.descendants(child, out, seen);
    }
    return out;
  }

  leaves(name) {
    const below = this.descendants(name);
    return (below.length ? below : [name]).filter((member) => this.dim.byName.get(member).level === 0);
  }

  /** The members picked on the left, in the dimension's order. */
  pickedInOrder() {
    return this.dim.elements.map((element) => element.name).filter((name) => this.picked.has(name));
  }

  /* ---- The dialog ---- */

  show() {
    this.left = h('div', { class: 'set-tree', role: 'tree', 'aria-label': 'Available members', 'aria-multiselectable': 'true', tabindex: '-1' });
    // Pinned above the members, the names of the attributes shown beside them.
    this.leftHead = h('div', { class: 'set-head-row', 'aria-hidden': 'true', hidden: true });
    this.leftRows = new Rows(this.left, (index) => this.drawLeftRow(index), { covered: () => (this.leftHead.hidden ? 0 : this.leftHead.offsetHeight) });
    this.leftNone = h('p', { class: 'muted set-none', hidden: true }, 'Nothing matches.');
    this.left.append(this.leftHead, this.leftRows.element, this.leftNone);
    this.left.addEventListener('keydown', (event) => this.leftKey(event));
    this.right = h('div', { class: 'set-list', role: 'listbox', 'aria-label': 'Current set', 'aria-multiselectable': 'true', tabindex: '-1' });
    this.rightRows = new Rows(this.right, (index) => this.drawItem(index));
    this.rightEmpty = h('p', { class: 'set-empty', hidden: true }, icon('plus', 18), 'Add members from the left: double-click one, or pick some and choose Add.');
    this.right.append(this.rightRows.element, this.rightEmpty);
    this.right.addEventListener('keydown', (event) => this.rightKey(event));
    this.count = h('span', { class: 'muted' });
    this.undoButton = h('button', { class: 'button small quiet icon', type: 'button', 'aria-label': 'Undo', title: 'Undo', onclick: () => this.undo() }, icon('undo', 16));
    this.redoButton = h('button', { class: 'button small quiet icon', type: 'button', 'aria-label': 'Redo', title: 'Redo', onclick: () => this.redo() }, icon('redo', 16));
    // Down or Enter in the search moves to the first member found.
    this.search = searchBox({
      label: `Find in ${this.dim.name}`,
      onsearch: (words) => {
        this.words = words;
        this.drawLeft();
      },
      ondown: () => {
        const first = this.shown.find((row) => row.match) ?? this.shown[0];
        if (first) this.pick(first.name);
      },
    });
    const toggle = (label, key, iconName, title) => {
      const button = h('button', {
        class: 'button small quiet',
        type: 'button',
        title,
        'aria-pressed': String(this[key]),
        onclick: () => {
          this[key] = !this[key];
          button.setAttribute('aria-pressed', String(this[key]));
          this.drawLeft();
        },
      }, icon(iconName, 14), label);
      return button;
    };
    const levels = [...new Set(this.dim.elements.map((element) => element.level))].sort((a, b) => b - a);
    const levelName = (level) => this.dim.levels?.[level] || (level === 0 ? 'Leaves' : `Level ${level}`);
    // Opening and closing act on the tree as shown: what was found, while searching.
    const openWhere = (names) => {
      if (this.found) this.found = new Set(names);
      else this.open = new Set(names);
      this.drawLeft();
    };
    const opening = openButtons({
      all: () => openWhere(this.dim.elements.filter((element) => this.children(element.name).length).map((element) => element.name)),
      none: () => openWhere([]),
      levels: levels.map((level) => [level, levelName(level)]),
      to: (level) => openWhere(this.dim.elements.filter((element) => element.level > level).map((element) => element.name)),
    });
    const quick = h('select', {
      class: 'select small-select',
      'aria-label': 'Replace the set',
      onchange: (event) => {
        const [kind, value] = event.target.value.split(':');
        event.target.value = '';
        if (!kind) return;
        const names = kind === 'top' ? this.dim.roots
          : kind === 'level' ? this.dim.elements.filter((element) => element.level === Number(value)).map((element) => element.name)
            : this.dim.subsets[value] ?? [];
        this.change(() => {
          this.set = names.filter((name) => this.dim.byName.has(name));
        });
      },
    },
    h('option', { value: '' }, 'Replace with…'),
    h('option', { value: 'top:' }, 'The top members'),
    levels.map((level) => h('option', { value: `level:${level}` }, `Every member of ${levelName(level)}`)),
    Object.keys(this.dim.subsets ?? {}).map((subset) => h('option', { value: `subset:${subset}` }, `The subset ${subset}`)));
    // Copying gives the names, one to a line; pasting adds the members a list names.
    this.left.addEventListener('copy', (event) => this.copyNames(event, this.pickedInOrder()));
    this.right.addEventListener('copy', (event) => this.copyNames(event, this.marked.size ? this.set.filter((name) => this.marked.has(name)) : this.set));
    for (const pane of [this.left, this.right]) {
      pane.addEventListener('paste', (event) => {
        const text = event.clipboardData?.getData('text/plain');
        if (!text) return;
        event.preventDefault();
        this.addNamed(text);
      });
    }
    this.filterPanel = h('div', { class: 'filter-panel', role: 'group', 'aria-label': 'Filter rules', hidden: true });
    this.filterButton = h('button', {
      class: 'button small quiet',
      type: 'button',
      'aria-pressed': 'false',
      onclick: () => {
        this.filtering = !this.filtering;
        if (this.filtering && !this.rules.length) {
          this.rules.push({ field: this.attributes.length ? `attribute:${this.attributes[0]}` : 'name', test: 'contains', value: '' });
        }
        this.drawFilter();
        this.drawLeft();
      },
    }, icon('filter', 14), 'Filter', h('span', { class: 'badge accent filter-count' }));
    const move = (label, iconName, action, title) => h('button', { class: 'button small', type: 'button', title, onclick: action }, icon(iconName, 14), h('span', {}, label));
    const body = h('div', { class: 'set-editor' },
      h('section', { class: 'set-pane', 'aria-label': 'Available members' },
        h('div', { class: 'set-pane-head' }, h('h3', {}, 'Available members'), h('span', { class: 'muted' }, `${this.dim.elements.length.toLocaleString()} in ${this.dim.name}`)),
        this.search.box,
        h('div', { class: 'toolbar' },
          this.filterButton,
          toggle('With descendants', 'withDescendants', 'level', 'Open each member found to show everything beneath it'),
          this.attributes.length ? toggle('Attributes', 'showAttributes', 'eye', 'Show attributes beside the members, and search them too') : null,
          h('div', { class: 'grow' }),
          opening),
        this.filterPanel,
        this.left),
      h('div', { class: 'set-moves', role: 'group', 'aria-label': 'Add to the set' },
        move('Add', 'chevron', () => this.add(this.pickedInOrder()), 'Add the members picked'),
        move('Children', 'plus', () => this.add(this.pickedInOrder().flatMap((name) => this.children(name))), 'Add the children of the members picked'),
        move('Descendants', 'level', () => this.add(this.pickedInOrder().flatMap((name) => [name, ...this.descendants(name)])), 'Add the members picked and everything beneath them'),
        move('Leaves', 'member', () => this.add(this.pickedInOrder().flatMap((name) => this.leaves(name))), 'Add the leaves beneath the members picked'),
        h('hr'),
        move('Remove', 'close', () => this.removeMarked(), 'Remove the members marked in the set')),
      h('section', { class: 'set-pane', 'aria-label': 'Current set' },
        h('div', { class: 'set-pane-head' }, h('h3', {}, 'Current set'), this.count, h('div', { class: 'grow' }), this.undoButton, this.redoButton),
        h('div', { class: 'toolbar' },
          quick,
          h('div', { class: 'grow' }),
          h('button', { class: 'button small quiet', type: 'button', title: 'Put the set in the dimension\'s order', onclick: () => this.change(() => this.sortHierarchy()) }, icon('rows', 14), 'Hierarchy'),
          h('button', { class: 'button small quiet', type: 'button', title: 'Sort the set by name', onclick: () => this.change(() => this.set.sort((a, b) => a.localeCompare(b))) }, icon('swap', 14), 'A–Z'),
          h('button', { class: 'button small quiet', type: 'button', title: 'Add members by name or alias, from a list', onclick: () => this.pasteList() }, icon('paste', 14), 'Paste'),
          h('button', { class: 'button small quiet', type: 'button', onclick: () => this.change(() => { this.set = []; }) }, 'Clear')),
        this.right,
        h('p', { class: 'hint' }, 'Drag to reorder, or Alt with the arrow keys. Delete removes the members marked. Ctrl C copies their names; Ctrl V adds the members a list names.')));
    this.draw();
    const asked = dialog({
      title: `Members of ${this.dim.name}`,
      confirm: 'Apply',
      wide: true,
      body,
      onsubmit: () => {
        if (!this.set.length) throw new Error('The set is empty. Add at least one member.');
        return this.set;
      },
    });
    // Open, the lists draw the rows they show.
    this.leftRows.update();
    this.rightRows.update();
    return asked;
  }

  /** Puts names on the clipboard, one to a line. */
  copyNames(event, names) {
    if (!names.length) return;
    event.preventDefault();
    event.clipboardData.setData('text/plain', names.join('\n'));
  }

  /** Asks for a list of names, as pasted from elsewhere, and adds the members it names. */
  async pasteList() {
    const list = h('textarea', { class: 'textarea', id: 'names', rows: 10, spellcheck: 'false', placeholder: 'One name to a line, or a column copied from a spreadsheet' });
    const text = await dialog({
      title: `Add members of ${this.dim.name} by name`,
      confirm: 'Add',
      body: h('div', { class: 'field' }, h('label', { for: 'names' }, 'Names or aliases'), list),
      onsubmit: () => list.value,
    });
    if (text) this.addNamed(text);
  }

  /**
   * Adds the members a list names, one to a line or a cell, by name or by alias, after the
   * last member marked or at the end, and says what it could not find.
   */
  addNamed(text) {
    const words = text.split(/[\r\n\t]+/).map((word) => word.trim()).filter(Boolean);
    const known = this.lookup();
    const found = [];
    const missing = [];
    for (const word of words) {
      const name = this.dim.byName.has(word) ? word : known.get(word.toLowerCase());
      if (name) found.push(name);
      else missing.push(word);
    }
    const fresh = [...new Set(found)].filter((name) => !this.set.includes(name));
    if (fresh.length) {
      const after = this.set.reduce((last, name, index) => (this.marked.has(name) ? index : last), -1);
      this.change(() => this.set.splice(after >= 0 ? after + 1 : this.set.length, 0, ...fresh));
    }
    const added = fresh.length ? `Added ${fresh.length.toLocaleString()} member${fresh.length === 1 ? '' : 's'}.` : 'Nothing new to add.';
    const shown = missing.slice(0, 5).join(', ') + (missing.length > 5 ? ` and ${(missing.length - 5).toLocaleString()} more` : '');
    notify(missing.length ? `${added} Not in ${this.dim.name}: ${shown}.` : added, missing.length ? 'bad' : 'good');
  }

  /** Each member by its name or alias in lower case; names before aliases. */
  lookup() {
    const known = new Map();
    const aliases = Object.entries(this.dim.attributes ?? {}).filter(([, kind]) => kind === 'alias').map(([attribute]) => attribute);
    for (const element of this.dim.elements) {
      for (const attribute of aliases) {
        const alias = element.attributes?.[attribute];
        if (alias !== undefined && alias !== null && alias !== '') known.set(String(alias).toLowerCase(), element.name);
      }
    }
    for (const element of this.dim.elements) known.set(element.name.toLowerCase(), element.name);
    return known;
  }

  sortHierarchy() {
    const order = new Map(this.hierarchyOrder().map((name, index) => [name, index]));
    this.set.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0));
  }

  /** Every member, each parent before its children, as the tree lists them. */
  hierarchyOrder() {
    const out = [];
    const seen = new Set();
    const visit = (name) => {
      if (seen.has(name)) return;
      seen.add(name);
      out.push(name);
      for (const child of this.children(name)) visit(child);
    };
    for (const root of this.dim.roots) visit(root);
    return out;
  }

  removeMarked() {
    if (!this.marked.size) return;
    this.change(() => {
      this.set = this.set.filter((name) => !this.marked.has(name));
    });
    this.marked.clear();
    this.draw();
  }

  draw() {
    this.drawLeft();
    this.drawRight();
    this.undoButton.disabled = !this.past.length;
    this.redoButton.disabled = !this.future.length;
  }

  /* ---- Available members ---- */

  /** Draws the available members, keeping focus on the member that had it, or on `focus`. */
  drawLeft(focus) {
    const keep = focus ?? (this.left.contains(document.activeElement) ? document.activeElement.closest?.('.set-row')?.dataset.name : null);
    const children = (name) => this.children(name);
    const roots = this.dim.roots;
    const matches = this.searching() ? new Set(this.matches().map((element) => element.name)) : null;
    // A new search opens the tree to what it finds, and from there the reader opens and
    // closes it; when the search is cleared, the tree opens to the members picked.
    const asked = matches ? JSON.stringify([this.words, this.filtering && this.activeRules(), this.matchAll, this.showAttributes, this.withDescendants]) : null;
    if (asked !== this.asked) {
      if (!asked) for (const name of reveal({ roots, children, matches: this.picked })) this.open.add(name);
      this.found = matches ? reveal({ roots, children, matches }) : null;
      if (matches && this.withDescendants) for (const name of [...matches, ...beneath(matches, children)]) this.found.add(name);
      this.asked = asked;
    }
    ({ rows: this.shown } = treeRows({ roots, children, open: this.found ?? this.open, matches }));
    this.inSet = new Set(this.set);
    // The member keys act on is the one last picked, or else the first found or shown.
    this.at = this.shown.findIndex((row) => row.name === this.anchor);
    if (this.at < 0) this.at = Math.max(0, this.shown.findIndex((row) => row.match));
    this.leftRows.show(this.shown.length);
    this.leftNone.hidden = this.shown.length > 0;
    this.search.found(matches?.size ?? 0, Boolean(matches));
    this.drawFilterCount();
    this.leftHead.hidden = !(this.attributes.length && this.showAttributes);
    if (!this.leftHead.hidden) {
      mount(this.leftHead, h('span', { class: 'set-name' }, 'Member'),
        this.attributes.map((attribute) => h('span', { class: 'set-attribute' }, attribute)), h('span', { class: 'set-in' }));
    }
    if (keep) this.leftRows.reveal(this.shown.findIndex((row) => row.name === keep))?.focus();
  }

  /** A row of the available members. */
  drawLeftRow(index) {
    const row = this.shown[index];
    const { name } = row;
    const element = this.dim.byName.get(name);
    return treeRow(row, this.picked.has(name), {
      class: `set-row${row.match ? ' match' : ''}${row.context ? ' context' : ''}`,
      tabindex: index === this.at ? '0' : '-1',
      dataset: { index },
      onclick: (event) => this.pick(name, event),
      ondblclick: () => this.add([name]),
    },
    twisty(row, (opened) => this.openRows([name], opened)),
    icon(element.type === 'consolidated' ? 'sigma' : 'member', 14),
    h('span', { class: 'set-name' }, row.match ? marked(name, this.words) : name),
    this.showAttributes ? this.attributes.map((attribute) => h('span', { class: 'set-attribute', title: attribute },
      row.match ? marked(element.attributes?.[attribute] ?? '', this.words) : String(element.attributes?.[attribute] ?? ''))) : null,
    // A place for the mark, kept whether or not the member is in the set, so that the attributes line up.
    h('span', { class: 'set-in' }, this.inSet.has(name) ? h('span', { class: 'badge accent', title: 'In the set' }, icon('check', 12)) : null));
  }

  /** Opens or closes members of the tree as shown. */
  openRows(names, opened) {
    const open = this.found ?? this.open;
    for (const name of names) {
      if (opened) open.add(name);
      else open.delete(name);
    }
    this.drawLeft();
  }
  /** Picks a member on the left: alone, or with Ctrl added or taken away, or with Shift a run. */
  pick(name, event) {
    if (event?.shiftKey && this.anchor) {
      const shown = this.shown.map((row) => row.name);
      const [from, to] = [shown.indexOf(this.anchor), shown.indexOf(name)].sort((a, b) => a - b);
      if (from >= 0) for (const member of shown.slice(from, to + 1)) this.picked.add(member);
    } else if (event?.ctrlKey || event?.metaKey) {
      if (this.picked.has(name)) this.picked.delete(name);
      else this.picked.add(name);
      this.anchor = name;
    } else {
      this.picked = new Set([name]);
      this.anchor = name;
    }
    this.drawLeft(name);
  }

  /** The tree's keys: moving picks, with Shift a run; Enter adds what is picked. */
  leftKey(event) {
    const row = event.target.closest('.set-row');
    if (!row) return;
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'a') {
      event.preventDefault();
      this.picked = new Set(this.shown.map((each) => each.name));
      this.drawLeft();
      return;
    }
    treeKey(event, this.shown, Number(row.dataset.index), {
      go: (name, key) => this.pick(name, key.shiftKey ? { shiftKey: true } : null),
      open: (names, opened) => this.openRows(names, opened),
      act: (name) => this.add(this.pickedInOrder().length ? this.pickedInOrder() : [name]),
      page: this.leftRows.page,
    });
  }

  /* ---- The set ---- */

  /** Draws the set, with the keys on the member at `focus`, where one is given. */
  drawRight(focus) {
    this.count.textContent = `${this.set.length.toLocaleString()} member${this.set.length === 1 ? '' : 's'}`;
    this.rightEmpty.hidden = this.set.length > 0;
    // The member the keys act on: the one given, or else the first marked.
    this.rightAt = focus ?? Math.max(0, this.set.findIndex((name) => this.marked.has(name)));
    this.rightRows.show(this.set.length);
    if (focus !== undefined) this.rightRows.reveal(focus)?.focus();
  }

  /** A member of the set, in its place: dragged, or moved by the keys, to another. */
  drawItem(index) {
    const name = this.set[index];
    const element = this.dim.byName.get(name);
    const item = h('div', {
      class: 'set-item',
      role: 'option',
      draggable: 'true',
      tabindex: index === this.rightAt ? '0' : '-1',
      'aria-selected': String(this.marked.has(name)),
      'aria-setsize': this.set.length,
      'aria-posinset': index + 1,
      dataset: { index },
      onclick: (event) => {
        if (event.ctrlKey || event.metaKey) {
          if (this.marked.has(name)) this.marked.delete(name);
          else this.marked.add(name);
        } else {
          this.marked = new Set([name]);
        }
        this.drawRight(index);
      },
      ondragstart: (event) => {
        this.dragged = index;
        event.dataTransfer.effectAllowed = 'move';
        event.dataTransfer.setData('text/plain', name);
      },
      ondragover: (event) => {
        if (this.dragged === null || this.dragged === undefined) return;
        event.preventDefault();
        item.classList.add('drop-before');
      },
      ondragleave: () => item.classList.remove('drop-before'),
      ondragend: () => {
        this.dragged = null;
      },
      ondrop: (event) => {
        event.preventDefault();
        item.classList.remove('drop-before');
        const from = this.dragged;
        this.dragged = null;
        if (from === null || from === undefined || from === index) return;
        this.change(() => {
          const [moved] = this.set.splice(from, 1);
          this.set.splice(from < index ? index - 1 : index, 0, moved);
        });
      },
    },
    h('span', { class: 'drag' }, icon('drag', 14)),
    h('span', { class: 'set-index muted' }, String(index + 1)),
    icon(element?.type === 'consolidated' ? 'sigma' : 'member', 14),
    h('span', { class: 'set-name' }, name),
    h('span', { class: 'muted set-level' }, this.dim.levels?.[element?.level] ?? (element?.level ? `Level ${element.level}` : 'Leaf')),
    h('button', {
      class: 'button quiet small icon',
      type: 'button',
      tabindex: '-1',
      'aria-label': `Remove ${name}`,
      onclick: (event) => {
        event.stopPropagation();
        this.change(() => {
          this.set.splice(index, 1);
        });
      },
    }, icon('close', 14)));
    return item;
  }

  /**
   * The set's keys: the arrows, Page Up and Down, Home and End mark a member; Alt with Up or
   * Down moves it; Delete removes those marked; Ctrl A marks them all.
   */
  rightKey(event) {
    const item = event.target.closest('.set-item');
    if (!item) return;
    const index = Number(item.dataset.index);
    const page = this.rightRows.page;
    const moves = { ArrowUp: index - 1, ArrowDown: index + 1, PageUp: index - page, PageDown: index + page, Home: 0, End: this.set.length - 1 };
    if (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
      event.preventDefault();
      const to = moves[event.key];
      if (to < 0 || to >= this.set.length) return;
      this.change(() => {
        [this.set[index], this.set[to]] = [this.set[to], this.set[index]];
      });
      this.drawRight(to);
    } else if (event.key in moves && !event.ctrlKey && !event.metaKey) {
      event.preventDefault();
      const to = Math.max(0, Math.min(this.set.length - 1, moves[event.key]));
      this.marked = new Set([this.set[to]]);
      this.drawRight(to);
    } else if (event.key === 'Delete' || event.key === 'Backspace') {
      event.preventDefault();
      if (!this.marked.size) this.marked.add(this.set[index]);
      this.removeMarked();
    } else if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'a') {
      event.preventDefault();
      this.marked = new Set(this.set);
      this.drawRight(index);
    }
  }
}
