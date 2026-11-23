// The set editor: a dimension's members on one side, the set being built on the other.

import { del, get, part, put } from './api.js';
import { state } from './app.js';
import { beneath, marked, openButtons, reveal, Rows, searchBox, treeKey, treeRow, treeRows, twisty } from './tree.js';
import { choices, copyText, dialog, h, icon, invalid, menu, MOD, mount, notify } from './ui.js';

/**
 * Edits the set of members a dimension shows on an axis. `dim` is the dimension as the
 * explorer holds it (elements, byName, roots, levels, subsets, attributes); `chosen` the
 * members in their order; `now(subset)`, where given, the members a subset that reads cells
 * selects as they stand. Resolves with the new set, or null if it is dismissed.
 */
export function editSet(dim, chosen, now) {
  return new SetEditor(dim, chosen, now).show();
}

class SetEditor {
  constructor(dim, chosen, now) {
    this.dim = dim;
    this.now = now;
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
    // The user's own subsets of the dimension, by name: none until they are fetched.
    this.own = {};
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
        this.rules.length > 1 ? choices({
          kind: 'radiogroup',
          label: 'Combine the rules',
          options: [[true, 'Match all'], [false, 'Match any']],
          chosen: this.matchAll,
          choose: (all) => {
            this.matchAll = all;
            this.drawFilter();
            this.drawLeft();
          },
        }) : null,
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

  /** Puts members into the set at a place, at its end unless told, those in it already left where they are. */
  add(names, place = this.set.length) {
    const present = new Set(this.set);
    const fresh = [...new Set(names)].filter((name) => !present.has(name));
    if (fresh.length) this.change(() => this.set.splice(place, 0, ...fresh));
  }

  /** Moves members of the set to a place in it, keeping their order among themselves. */
  moveTo(names, place) {
    const moving = new Set(names);
    const stay = (name) => !moving.has(name);
    this.change(() => {
      this.set = [...this.set.slice(0, place).filter(stay), ...this.set.filter((name) => moving.has(name)), ...this.set.slice(place).filter(stay)];
    });
  }

  /** Takes members out of the set. */
  remove(names) {
    const going = new Set(names);
    for (const name of going) this.marked.delete(name);
    this.change(() => {
      this.set = this.set.filter((name) => !going.has(name));
    });
  }

  /** The members marked in the set, in its order. */
  markedInOrder() {
    return this.set.filter((name) => this.marked.has(name));
  }

  /** Shows which members are picked, or marked, on the rows drawn, without drawing them again. */
  paintChoices() {
    for (const row of this.left.querySelectorAll('.set-row')) row.setAttribute('aria-selected', String(this.picked.has(row.dataset.name)));
    for (const item of this.right.querySelectorAll('.set-item')) item.setAttribute('aria-selected', String(this.marked.has(this.set[Number(item.dataset.index)])));
  }

  /* ---- Dragging ---- */

  /**
   * Starts dragging members, from the tree or from the set, their names going along as text
   * for dropping elsewhere. Several say how many, rather than showing the one row.
   */
  dragStart(event, from, names) {
    this.dragging = { from, names };
    event.dataTransfer.effectAllowed = from === 'tree' ? 'copy' : 'copyMove';
    event.dataTransfer.setData('text/plain', names.join('\n'));
    if (names.length > 1) {
      const label = h('div', { class: 'drag-count' }, `${names.length.toLocaleString()} members`);
      this.dialogBody.append(label);
      event.dataTransfer.setDragImage(label, 14, 14);
      setTimeout(() => label.remove());
    }
  }

  /** Ends a drag, dropped or not. */
  dragged() {
    this.dragging = null;
    this.clearDrop();
  }

  /** Where in the set a drop at the pointer puts what is dropped: before the member under it, or after it, in its lower half. */
  dropPlace(event) {
    const item = event.target.closest('.set-item');
    if (!item) return this.set.length;
    const index = Number(item.dataset.index);
    const box = item.getBoundingClientRect();
    return event.clientY > box.top + box.height / 2 ? index + 1 : index;
  }

  /** Shows where a drop would go: a line before a member, or after the last, or the whole list when it is empty. */
  markDrop(place) {
    this.clearDrop();
    const next = this.rightRows.at(place);
    const last = this.rightRows.at(place - 1);
    if (next) next.classList.add('drop-before');
    else if (last) last.classList.add('drop-after');
    else this.right.classList.add('drop-target');
  }

  clearDrop() {
    for (const element of this.dialogBody.querySelectorAll('.drop-before, .drop-after, .drop-target')) element.classList.remove('drop-before', 'drop-after', 'drop-target');
  }

  /** Lets members dragged from the tree, or from the set, be dropped on an element, which acts on them. */
  dropTarget(element, from, act) {
    element.addEventListener('dragover', (event) => {
      if (this.dragging?.from !== from) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = from === 'tree' ? 'copy' : 'move';
      element.classList.add('drop-target');
    });
    element.addEventListener('dragleave', (event) => {
      if (!element.contains(event.relatedTarget)) element.classList.remove('drop-target');
    });
    element.addEventListener('drop', (event) => {
      if (this.dragging?.from !== from) return;
      event.preventDefault();
      const { names } = this.dragging;
      this.dragged();
      act(names);
    });
    return element;
  }

  /* ---- Menus ---- */

  /**
   * The menu of a member of the tree, and of those picked with it: ways to add them to the
   * set, to make the set of them, to open it, and to copy their names. A member not among
   * those picked is picked alone first, as a click would.
   */
  leftMenu(name, at) {
    if (!this.picked.has(name)) {
      this.picked = new Set([name]);
      this.anchor = name;
      this.paintChoices();
    }
    const names = this.pickedInOrder();
    const they = (one, more) => (names.length > 1 ? more : one);
    const row = this.shown.find((each) => each.name === name);
    const beneath = this.descendants(name);
    menu([
      { heading: they(name, `${names.length.toLocaleString()} members`) },
      { label: 'Add to the set', icon: 'chevron', action: () => this.add(names) },
      { label: they('Add its children', 'Add their children'), icon: 'plus', disabled: !names.some((each) => this.children(each).length), action: () => this.add(names.flatMap((each) => this.children(each))) },
      { label: they('Add it and all beneath it', 'Add them and all beneath them'), icon: 'level', action: () => this.add(names.flatMap((each) => [each, ...this.descendants(each)])) },
      { label: they('Add its leaves', 'Add their leaves'), icon: 'member', action: () => this.add(names.flatMap((each) => this.leaves(each))) },
      { label: they('Make the set of it alone', 'Make the set of these'), icon: 'swap', action: () => this.change(() => { this.set = [...names]; }) },
      row?.kids ? '-' : null,
      row?.kids ? { label: row.open ? 'Close it' : 'Open it', icon: row.open ? 'collapse' : 'expand', action: () => this.openRows([name], !row.open) } : null,
      row?.kids ? { label: 'Open all beneath it', icon: 'expand', action: () => this.openRows([name, ...beneath].filter((each) => this.children(each).length), true) } : null,
      row?.kids ? {
        label: 'Pick all beneath it',
        icon: 'check',
        action: () => {
          for (const each of [name, ...beneath]) if (this.children(each).length) (this.found ?? this.open).add(each);
          this.picked = new Set([...this.picked, ...beneath]);
          this.drawLeft(name);
        },
      } : null,
      '-',
      { label: they('Copy its name', 'Copy their names'), icon: 'copy', action: () => copyText(names.join('\n')) },
    ], at);
  }

  /**
   * The menu of a member of the set, and of those marked with it: moving them, opening one
   * into its children or leaves, keeping or removing them, finding one among the available
   * members, and copying their names. A member not marked is marked alone first.
   */
  rightMenu(index, at) {
    const name = this.set[index];
    if (!this.marked.has(name)) {
      this.marked = new Set([name]);
      this.paintChoices();
    }
    const names = this.markedInOrder();
    const one = names.length === 1;
    const they = (single, more) => (one ? single : more);
    const kids = this.children(name);
    // Members put after the member, or in its place; those in the set already stay where they are.
    const into = (members, replace) => this.change(() => {
      const present = new Set(this.set);
      const fresh = [...new Set(members)].filter((each) => !present.has(each));
      const place = this.set.indexOf(name);
      this.set.splice(replace ? place : place + 1, replace ? 1 : 0, ...fresh);
    });
    menu([
      { heading: they(name, `${names.length.toLocaleString()} members`) },
      { label: they('Move it to the top', 'Move them to the top'), icon: 'up', action: () => this.moveTo(names, 0) },
      { label: they('Move it to the bottom', 'Move them to the bottom'), icon: 'down', action: () => this.moveTo(names, this.set.length) },
      one && kids.length ? '-' : null,
      one && kids.length ? { label: 'Put its children after it', icon: 'plus', action: () => into(kids, false) } : null,
      one && kids.length ? { label: 'Put its children in its place', icon: 'swap', action: () => into(kids, true) } : null,
      one && kids.length ? { label: 'Put its leaves in its place', icon: 'member', action: () => into(this.leaves(name), true) } : null,
      '-',
      { label: they('Keep only it', 'Keep only these'), icon: 'filter', disabled: names.length === this.set.length, action: () => this.change(() => { this.set = names; }) },
      { label: they('Remove it', 'Remove them'), icon: 'close', action: () => this.removeMarked() },
      '-',
      one ? { label: 'Find it among the available members', icon: 'search', action: () => this.showInTree(name) } : null,
      { label: they('Copy its name', 'Copy their names'), icon: 'copy', action: () => copyText(names.join('\n')) },
    ], at);
  }

  /** The ways to sort the set: each a step of its own, which Undo takes back. */
  sortMenu(at) {
    const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
    const element = (name) => this.dim.byName.get(name);
    const level = (name) => element(name)?.level ?? 0;
    const sort = (label, iconName, compare) => ({ label, icon: iconName, action: () => this.change(() => this.set.sort(compare)) });
    // An attribute's values in order, numbers as numbers and text as the reader sorts it;
    // members without one go last either way.
    const byAttribute = (attribute, descending) => {
      const numeric = this.dim.attributes?.[attribute] === 'numeric';
      const missing = (value) => value === undefined || value === null || value === '';
      return (a, b) => {
        const [x, y] = [element(a)?.attributes?.[attribute], element(b)?.attributes?.[attribute]];
        if (missing(x) || missing(y)) return missing(x) - missing(y);
        const order = numeric ? Number(x) - Number(y) : collator.compare(String(x), String(y));
        return descending ? -order : order;
      };
    };
    menu([
      { heading: 'Sort the set' },
      { label: 'As the dimension has them', icon: 'rows', action: () => this.change(() => this.sortHierarchy(false)) },
      { label: 'As the dimension has them, totals last', icon: 'rows', action: () => this.change(() => this.sortHierarchy(true)) },
      sort('By name, A to Z', 'down', (a, b) => collator.compare(a, b)),
      sort('By name, Z to A', 'up', (a, b) => collator.compare(b, a)),
      sort('By level, the top first', 'level', (a, b) => level(b) - level(a)),
      sort('By level, leaves first', 'level', (a, b) => level(a) - level(b)),
      ...this.attributes.flatMap((attribute) => {
        const numeric = this.dim.attributes?.[attribute] === 'numeric';
        return [
          sort(`By ${attribute}, ${numeric ? 'smallest first' : 'A to Z'}`, 'down', byAttribute(attribute, false)),
          sort(`By ${attribute}, ${numeric ? 'largest first' : 'Z to A'}`, 'up', byAttribute(attribute, true)),
        ];
      }),
      '-',
      { label: 'Reverse the order', icon: 'swap', action: () => this.change(() => this.set.reverse()) },
    ], at);
  }

  /** Shows a member among the available ones: found, the tree opened down to it, and picked. */
  showInTree(name) {
    if (this.words || this.filtering) {
      this.words = '';
      this.search.input.value = '';
      this.filtering = false;
      this.drawFilter();
    }
    for (const each of reveal({ roots: this.dim.roots, children: (member) => this.children(member), matches: new Set([name]) })) this.open.add(each);
    this.picked = new Set([name]);
    this.anchor = name;
    this.drawLeft(name);
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
    this.rightEmpty = h('p', { class: 'set-empty', hidden: true }, icon('plus', 18), 'Drag members here from the left, double-click one, or pick some and choose Add.');
    this.right.append(this.rightRows.element, this.rightEmpty);
    this.right.addEventListener('keydown', (event) => this.rightKey(event));
    // Members dragged from the tree, or within the set, or names dropped from elsewhere, go
    // into the set where they are dropped; members of the set dragged back to the tree leave it.
    this.right.addEventListener('dragover', (event) => {
      const from = this.dragging?.from;
      if (!from && !event.dataTransfer.types.includes('text/plain')) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = from === 'set' ? 'move' : 'copy';
      this.markDrop(this.dropPlace(event));
    });
    this.right.addEventListener('dragleave', (event) => {
      if (!this.right.contains(event.relatedTarget)) this.clearDrop();
    });
    this.right.addEventListener('drop', (event) => {
      event.preventDefault();
      const place = this.dropPlace(event);
      const dragging = this.dragging;
      this.dragged();
      if (dragging?.from === 'tree') this.add(dragging.names, place);
      else if (dragging?.from === 'set') this.moveTo(dragging.names, place);
      else this.addNamed(event.dataTransfer.getData('text/plain'), place);
    });
    this.dropTarget(this.left, 'set', (names) => this.remove(names));
    // A member's menu, at the pointer or, by the menu key or Shift F10, at its row.
    this.left.addEventListener('contextmenu', (event) => {
      const row = event.target.closest('.set-row');
      if (!row) return;
      event.preventDefault();
      this.leftMenu(row.dataset.name, { x: event.clientX, y: event.clientY });
    });
    this.right.addEventListener('contextmenu', (event) => {
      const item = event.target.closest('.set-item');
      if (!item) return;
      event.preventDefault();
      this.rightMenu(Number(item.dataset.index), { x: event.clientX, y: event.clientY });
    });
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
    // The set replaced whole: by the top members, a level, one of the dimension's subsets, or
    // one of the user's own, which come once they are fetched.
    const quick = h('select', {
      class: 'select small-select',
      'aria-label': 'Replace the set',
      onchange: async (event) => {
        const chosen = event.target.value;
        event.target.value = '';
        const [kind, ...rest] = chosen.split(':');
        const value = rest.join(':');
        if (!kind) return;
        let names = kind === 'top' ? this.dim.roots
          : kind === 'level' ? this.dim.elements.filter((element) => element.level === Number(value)).map((element) => element.name)
            : kind === 'own' ? this.own[value] ?? []
              : this.dim.subsets[value] ?? [];
        // A subset that reads cells is asked for as they stand, not as they stood when the
        // dimension was fetched.
        if (kind === 'subset' && this.now && this.dim.subsets_reading_cells?.includes(value)) {
          try {
            names = await this.now(value);
          } catch (problem) {
            notify(problem.message, 'bad');
            return;
          }
        }
        this.change(() => {
          this.set = names.filter((name) => this.dim.byName.has(name));
        });
      },
    });
    this.drawQuick = () => mount(quick,
      h('option', { value: '' }, 'Replace with…'),
      h('option', { value: 'top:' }, 'The top members'),
      levels.map((level) => h('option', { value: `level:${level}` }, `Every member of ${levelName(level)}`)),
      Object.keys(this.dim.subsets ?? {}).map((subset) => h('option', { value: `subset:${subset}` }, `The subset ${subset}`)),
      Object.keys(this.own).map((subset) => h('option', { value: `own:${subset}` }, `Your subset ${subset}`)));
    this.drawQuick();
    this.fetchOwn();
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
    // Each way of adding takes the members picked when pressed, or those dropped on it.
    const way = (label, iconName, reach, title) => this.dropTarget(
      h('button', { class: 'button small', type: 'button', title, onclick: () => this.add(reach(this.pickedInOrder())) }, icon(iconName, 14), h('span', {}, label)),
      'tree',
      (names) => this.add(reach(names)),
    );
    const body = h('div', { class: 'set-editor' },
      h('section', { class: 'set-pane', 'aria-label': 'Available members' },
        h('div', { class: 'set-pane-head' }, h('h3', {}, 'Available members'), h('span', { class: 'muted' }, `${this.dim.elements.length.toLocaleString()} in ${this.dim.name}`)),
        h('div', { class: 'tree-tools' }, this.search.box, opening),
        h('div', { class: 'toolbar' },
          this.filterButton,
          toggle('With descendants', 'withDescendants', 'level', 'Open each member found to show everything beneath it'),
          this.attributes.length ? toggle('Attributes', 'showAttributes', 'eye', 'Show attributes beside the members, and search them too') : null),
        this.filterPanel,
        this.left),
      h('div', { class: 'set-moves', role: 'group', 'aria-label': 'Add to the set' },
        way('Add', 'chevron', (names) => names, 'Add the members picked, or dropped here'),
        way('Children', 'plus', (names) => names.flatMap((name) => this.children(name)), 'Add the children of the members picked, or dropped here'),
        way('Descendants', 'level', (names) => names.flatMap((name) => [name, ...this.descendants(name)]), 'Add the members picked, or dropped here, and everything beneath them'),
        way('Leaves', 'member', (names) => names.flatMap((name) => this.leaves(name)), 'Add the leaves beneath the members picked, or dropped here'),
        h('hr'),
        this.dropTarget(
          h('button', { class: 'button small', type: 'button', title: 'Remove the members marked in the set, or dropped here', onclick: () => this.removeMarked() }, icon('close', 14), h('span', {}, 'Remove')),
          'set',
          (names) => this.remove(names),
        )),
      h('section', { class: 'set-pane', 'aria-label': 'Current set' },
        h('div', { class: 'set-pane-head' }, h('h3', {}, 'Current set'), this.count, h('div', { class: 'grow' }), this.undoButton, this.redoButton),
        h('div', { class: 'toolbar' },
          quick,
          h('div', { class: 'grow' }),
          this.keeps() ? h('button', { class: 'button small quiet', type: 'button', 'aria-haspopup': 'menu', title: 'Keep the set as one of your subsets, or take one away', onclick: (event) => this.keepMenu(event.currentTarget) }, icon('save', 14), 'Keep', icon('down', 12)) : null,
          h('button', { class: 'button small quiet', type: 'button', 'aria-haspopup': 'menu', title: 'Sort the set', onclick: (event) => this.sortMenu(event.currentTarget) }, icon('bars', 14), 'Sort', icon('down', 12)),
          h('button', { class: 'button small quiet', type: 'button', title: 'Add members by name or alias, from a list', onclick: () => this.pasteList() }, icon('paste', 14), 'Paste'),
          h('button', { class: 'button small quiet', type: 'button', onclick: () => this.change(() => { this.set = []; }) }, 'Clear')),
        this.right,
        h('p', { class: 'hint' }, `Drag members in from the left, or within the set to reorder; drag them back to take them out. Right-click a member for more. Alt with the arrow keys moves those marked, Delete removes them, and ${MOD} C and ${MOD} V copy and paste names.`)));
    this.dialogBody = body;
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

  /* ---- The user's own subsets ---- */

  /**
   * Whether the user keeps subsets of their own: on a server with users, and not in a view
   * as another user, which changes nothing of theirs.
   */
  keeps() {
    return !state.open && !state.user?.viewer;
  }

  /** The user's own subsets of the dimension, offered beside its own once they come. */
  async fetchOwn() {
    if (state.open) return;
    try {
      this.own = await get(`/dimensions/${part(this.dim.name)}/private-subsets`, { quiet: true });
    } catch {
      // The dimension's own subsets are still there to choose.
      return;
    }
    this.drawQuick();
  }

  keepMenu(at) {
    const mine = Object.keys(this.own);
    menu([
      { label: 'Keep this set as one of your subsets…', icon: 'save', disabled: !this.set.length, action: () => this.keepOwn() },
      mine.length ? { label: 'Take one of your subsets away…', icon: 'trash', action: () => this.removeOwn() } : null,
    ], at);
  }

  /** Keeps the set as one of the user's own subsets, under a name they give. */
  async keepOwn() {
    const name = h('input', { class: 'input', id: 'subset-name', required: true, maxlength: 120 });
    const kept = await dialog({
      title: 'Keep as your subset',
      confirm: 'Keep',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'subset-name' }, 'Name'), name),
        h('p', { class: 'hint' }, `The ${this.set.length.toLocaleString()} member${this.set.length === 1 ? '' : 's'} of the set, in order, kept for you alone: nobody else sees it. One of yours of the same name is replaced.`)),
      onsubmit: async () => {
        const subset = name.value.trim();
        if (!subset) throw invalid(name, 'Give the subset a name.');
        await put(`/dimensions/${part(this.dim.name)}/private-subsets/${part(subset)}`, { elements: this.set });
        return subset;
      },
    });
    if (!kept) return;
    notify(`Kept the subset “${kept}” for you alone.`, 'good');
    await this.fetchOwn();
  }

  /** Lists the user's own subsets of the dimension, each to be taken away. */
  async removeOwn() {
    const list = h('ul', { class: 'history-list' });
    mount(list, Object.keys(this.own).map((name) => {
      const item = h('li', {},
        h('span', { class: 'grow' }, name),
        h('span', { class: 'muted' }, `${this.own[name].length.toLocaleString()} member${this.own[name].length === 1 ? '' : 's'}`),
        h('button', {
          class: 'button small danger',
          type: 'button',
          'aria-label': `Take away the subset ${name}`,
          onclick: async () => {
            try {
              await del(`/dimensions/${part(this.dim.name)}/private-subsets/${part(name)}`);
            } catch (problem) {
              notify(problem.message, 'bad');
              return;
            }
            // The keys go on to the subset after, or before, or to Done.
            const next = item.nextElementSibling ?? item.previousElementSibling;
            item.remove();
            if (!list.children.length) list.append(h('li', { class: 'muted' }, `You keep no subsets of ${this.dim.name} now.`));
            (next?.querySelector('button') ?? list.closest('dialog')?.querySelector('button[type="submit"]'))?.focus();
            notify(`Took away the subset “${name}”.`, 'good');
          },
        }, 'Take away'));
      return item;
    }));
    await dialog({ title: `Your subsets of ${this.dim.name}`, confirm: 'Done', cancel: null, body: list });
    await this.fetchOwn();
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
   * Adds the members a list names, one to a line or a cell, by name or by alias, at `place`
   * where one is given, as where it was dropped, or else after the last member marked or at
   * the end; and says what it could not find.
   */
  addNamed(text, place) {
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
      this.change(() => this.set.splice(place ?? (after >= 0 ? after + 1 : this.set.length), 0, ...fresh));
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

  /** Puts the set in the dimension's order, each total before its members, or after them with `totalsLast`. */
  sortHierarchy(totalsLast = false) {
    const order = new Map(this.hierarchyOrder(totalsLast).map((name, index) => [name, index]));
    this.set.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0));
  }

  /** Every member as the tree lists them: each parent before its children, or after them with `totalsLast`. */
  hierarchyOrder(totalsLast = false) {
    const out = [];
    const seen = new Set();
    const visit = (name) => {
      if (seen.has(name)) return;
      seen.add(name);
      if (!totalsLast) out.push(name);
      for (const child of this.children(name)) visit(child);
      if (totalsLast) out.push(name);
    };
    for (const root of this.dim.roots) visit(root);
    return out;
  }

  removeMarked() {
    if (this.marked.size) this.remove([...this.marked]);
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
      draggable: 'true',
      onclick: (event) => this.pick(name, event),
      ondblclick: () => this.add([name]),
      // Dragged, a member goes with those picked, or alone if it was not among them.
      ondragstart: (event) => {
        if (!this.picked.has(name)) {
          this.picked = new Set([name]);
          this.anchor = name;
          this.paintChoices();
        }
        this.dragStart(event, 'tree', this.pickedInOrder());
      },
      ondragend: () => this.dragged(),
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
    if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
      event.preventDefault();
      this.leftMenu(row.dataset.name, row);
      return;
    }
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
      // Dragged, a member goes with those marked, or alone if it was not among them.
      ondragstart: (event) => {
        if (!this.marked.has(name)) {
          this.marked = new Set([name]);
          this.paintChoices();
        }
        this.dragStart(event, 'set', this.markedInOrder());
      },
      ondragend: () => this.dragged(),
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
    if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
      event.preventDefault();
      this.rightMenu(index, item);
    } else if (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
      event.preventDefault();
      // The members marked move a step together, or the one with the keys if it is not marked.
      const step = event.key === 'ArrowUp' ? -1 : 1;
      const names = this.marked.has(this.set[index]) ? this.markedInOrder() : [this.set[index]];
      const places = names.map((name) => this.set.indexOf(name)).sort((a, b) => (a - b) * step).reverse();
      if (places.some((place) => place + step < 0 || place + step >= this.set.length)) return;
      this.change(() => {
        // The nearest the edge moves first, so that those beside it follow into its place.
        for (const place of places) [this.set[place], this.set[place + step]] = [this.set[place + step], this.set[place]];
      });
      this.drawRight(index + step);
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
