// The set editor: a dimension's members on one side, the set being built on the other.

import { dialog, h, icon, mount } from './ui.js';

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
    this.right = h('ol', { class: 'set-list', 'aria-label': 'Current set', tabindex: '-1' });
    this.count = h('span', { class: 'muted' });
    this.undoButton = h('button', { class: 'button small quiet icon', type: 'button', 'aria-label': 'Undo', title: 'Undo', onclick: () => this.undo() }, icon('undo', 16));
    this.redoButton = h('button', { class: 'button small quiet icon', type: 'button', 'aria-label': 'Redo', title: 'Redo', onclick: () => this.redo() }, icon('redo', 16));
    const search = h('input', { class: 'input', type: 'search', placeholder: `Find in ${this.dim.name}`, 'aria-label': `Find in ${this.dim.name}` });
    // Enter in the search adds what was picked rather than applying the set.
    search.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') event.preventDefault();
    });
    search.addEventListener('input', () => {
      this.words = search.value.trim().toLowerCase();
      this.drawLeft();
    });
    const toggle = (label, key, iconName) => {
      const button = h('button', {
        class: 'button small quiet',
        type: 'button',
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
    const expand = h('select', {
      class: 'select small-select',
      'aria-label': 'Open the tree down to a level',
      onchange: (event) => {
        const level = Number(event.target.value);
        if (event.target.value === '') return;
        this.open = new Set(this.dim.elements.filter((element) => element.level > level).map((element) => element.name));
        event.target.value = '';
        this.drawLeft();
      },
    }, h('option', { value: '' }, 'Open to a level…'), levels.filter((level) => level < levels[0]).map((level) => h('option', { value: level }, levelName(level))));
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
    const move = (label, iconName, action, title) => h('button', { class: 'button small', type: 'button', title, onclick: action }, icon(iconName, 14), h('span', {}, label));
    const body = h('div', { class: 'set-editor' },
      h('section', { class: 'set-pane', 'aria-label': 'Available members' },
        h('div', { class: 'set-pane-head' }, h('h3', {}, 'Available members'), h('span', { class: 'muted' }, `${this.dim.elements.length.toLocaleString()} in ${this.dim.name}`)),
        h('div', { class: 'search' }, icon('search', 16), search),
        h('div', { class: 'toolbar' },
          toggle('With descendants', 'withDescendants', 'level'),
          this.attributes.length ? toggle('Attributes', 'showAttributes', 'eye') : null,
          h('div', { class: 'grow' }),
          levels.length > 1 ? expand : null),
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
          h('button', { class: 'button small quiet', type: 'button', onclick: () => this.change(() => { this.set = []; }) }, 'Clear')),
        this.right,
        h('p', { class: 'hint' }, 'Drag to reorder, or Alt with the arrow keys. Delete removes the members marked.')));
    this.draw();
    return dialog({
      title: `Members of ${this.dim.name}`,
      confirm: 'Apply',
      wide: true,
      body,
      onsubmit: () => {
        if (!this.set.length) throw new Error('The set is empty. Add at least one member.');
        return this.set;
      },
    });
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

  drawLeft() {
    const rows = [];
    const inSet = new Set(this.set);
    const row = (name, depth, path) => {
      const element = this.dim.byName.get(name);
      const kids = this.children(name);
      const opened = this.open.has(name);
      const picked = this.picked.has(name);
      const element_row = h('div', {
        class: 'set-row',
        role: 'treeitem',
        'aria-selected': String(picked),
        'aria-expanded': kids.length && !this.words ? String(opened) : null,
        tabindex: '-1',
        dataset: { depth: Math.min(depth, 8), name },
        onclick: (event) => this.pick(name, event),
        ondblclick: () => this.add([name]),
        onkeydown: (event) => this.leftKey(event, name),
      },
      kids.length && !this.words
        ? h('button', {
          class: 'twisty',
          type: 'button',
          tabindex: '-1',
          'aria-label': opened ? `Close ${name}` : `Open ${name}`,
          onclick: (event) => {
            event.stopPropagation();
            if (opened) this.open.delete(name);
            else this.open.add(name);
            this.drawLeft();
          },
        }, icon(opened ? 'down' : 'chevron', 12))
        : h('span', { class: 'twisty' }),
      icon(element.type === 'consolidated' ? 'sigma' : 'member', 14),
      h('span', { class: 'set-name' }, name, path ? h('span', { class: 'set-path' }, path) : null),
      this.showAttributes ? this.attributes.map((attribute) => h('span', { class: 'set-attribute', title: attribute }, String(element.attributes?.[attribute] ?? ''))) : null,
      inSet.has(name) ? h('span', { class: 'badge accent', title: 'In the set' }, icon('check', 12)) : null);
      rows.push(element_row);
    };
    if (this.words) {
      // Matches by name, or by attribute where attributes are shown; with descendants,
      // everything beneath a match comes too.
      const matches = this.dim.elements.filter((element) => element.name.toLowerCase().includes(this.words)
        || (this.showAttributes && Object.values(element.attributes ?? {}).some((value) => String(value).toLowerCase().includes(this.words))));
      const shown = new Set();
      for (const element of matches.slice(0, 500)) {
        if (!shown.has(element.name)) {
          shown.add(element.name);
          row(element.name, 0, element.parents.length ? `in ${element.parents.join(', ')}` : null);
        }
        if (this.withDescendants) {
          for (const below of this.descendants(element.name)) {
            if (!shown.has(below)) {
              shown.add(below);
              row(below, 1, null);
            }
          }
        }
      }
      if (matches.length > 500) rows.push(h('p', { class: 'hint set-more' }, `Showing the first 500 of ${matches.length.toLocaleString()} matches. Narrow the search to see the rest.`));
    } else {
      const seen = new Set();
      const visit = (name, depth) => {
        if (seen.has(name) && depth > 0) return;
        seen.add(name);
        row(name, depth, null);
        if (this.open.has(name)) for (const child of this.children(name)) visit(child, depth + 1);
      };
      for (const root of this.dim.roots) visit(root, 0);
    }
    mount(this.left, rows.length ? rows : h('p', { class: 'muted set-none' }, 'Nothing matches.'));
    if (this.attributes.length && this.showAttributes) {
      this.left.prepend(h('div', { class: 'set-row set-head-row' }, h('span', { class: 'twisty' }), h('span', { class: 'set-name' }, 'Member'),
        this.attributes.map((attribute) => h('span', { class: 'set-attribute' }, attribute))));
    }
  }

  /** Picks a member on the left: alone, or with Ctrl added or taken away, or with Shift a run. */
  pick(name, event) {
    if (event?.shiftKey && this.anchor) {
      const shown = [...this.left.querySelectorAll('.set-row[data-name]')].map((row) => row.dataset.name);
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
    this.drawLeft();
    this.left.querySelector(`.set-row[data-name="${CSS.escape(name)}"]`)?.focus();
  }

  leftKey(event, name) {
    const shown = [...this.left.querySelectorAll('.set-row[data-name]')];
    const at = shown.findIndex((row) => row.dataset.name === name);
    const go = (index) => {
      const target = shown[Math.max(0, Math.min(index, shown.length - 1))];
      if (target) this.pick(target.dataset.name, event.shiftKey ? { shiftKey: true } : null);
    };
    if (event.key === 'ArrowDown') go(at + 1);
    else if (event.key === 'ArrowUp') go(at - 1);
    else if (event.key === 'ArrowRight' && this.children(name).length && !this.open.has(name)) {
      this.open.add(name);
      this.drawLeft();
      this.left.querySelector(`.set-row[data-name="${CSS.escape(name)}"]`)?.focus();
    } else if (event.key === 'ArrowLeft' && this.open.has(name)) {
      this.open.delete(name);
      this.drawLeft();
      this.left.querySelector(`.set-row[data-name="${CSS.escape(name)}"]`)?.focus();
    } else if (event.key === 'Enter') {
      this.add(this.pickedInOrder().length ? this.pickedInOrder() : [name]);
    } else {
      return;
    }
    event.preventDefault();
  }

  /* ---- The set ---- */

  drawRight() {
    this.count.textContent = `${this.set.length.toLocaleString()} member${this.set.length === 1 ? '' : 's'}`;
    if (!this.set.length) {
      mount(this.right, h('li', { class: 'set-empty' }, icon('plus', 18), 'Add members from the left: double-click one, or pick some and choose Add.'));
      return;
    }
    let dragged = null;
    mount(this.right, this.set.map((name, index) => {
      const element = this.dim.byName.get(name);
      const item = h('li', {
        class: 'set-item',
        draggable: 'true',
        tabindex: '-1',
        'aria-selected': String(this.marked.has(name)),
        dataset: { index },
        onclick: (event) => {
          if (event.ctrlKey || event.metaKey) {
            if (this.marked.has(name)) this.marked.delete(name);
            else this.marked.add(name);
          } else {
            this.marked = new Set([name]);
          }
          this.drawRight();
          this.right.querySelector(`[data-index="${index}"]`)?.focus();
        },
        onkeydown: (event) => {
          if (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
            event.preventDefault();
            const to = index + (event.key === 'ArrowUp' ? -1 : 1);
            if (to < 0 || to >= this.set.length) return;
            this.change(() => {
              [this.set[index], this.set[to]] = [this.set[to], this.set[index]];
            });
            this.right.querySelector(`[data-index="${to}"]`)?.focus();
          } else if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
            event.preventDefault();
            this.right.querySelector(`[data-index="${index + (event.key === 'ArrowUp' ? -1 : 1)}"]`)?.click();
          } else if (event.key === 'Delete' || event.key === 'Backspace') {
            event.preventDefault();
            if (!this.marked.size) this.marked.add(name);
            this.removeMarked();
          }
        },
        ondragstart: (event) => {
          dragged = index;
          event.dataTransfer.effectAllowed = 'move';
          event.dataTransfer.setData('text/plain', name);
        },
        ondragover: (event) => {
          if (dragged === null) return;
          event.preventDefault();
          item.classList.add('drop-before');
        },
        ondragleave: () => item.classList.remove('drop-before'),
        ondrop: (event) => {
          event.preventDefault();
          item.classList.remove('drop-before');
          if (dragged === null || dragged === index) return;
          const from = dragged;
          this.change(() => {
            const [moved] = this.set.splice(from, 1);
            this.set.splice(from < index ? index - 1 : index, 0, moved);
          });
          dragged = null;
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
    }));
  }
}
