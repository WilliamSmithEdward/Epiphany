// Editing the model: dimensions, their elements, attributes and levels, and cubes with
// their rules, feeders and views. Every change is saved against the model version it was
// made from, so that nobody's change is overwritten unseen.

import { del, get, part, put } from './api.js';
import { empty, failure, go, loading } from './app.js';
import { checkFeeders } from './feeders.js';
import { beneath, INDENT, marked, openButtons, reveal, Rows, searchBox, treeKey, treeRow, treeRows, twisty } from './tree.js';
import { choices, dialog, h, icon, invalid, menu, mount, notify, parseNumber, refocus, splitter, textWidths, when, widest, working } from './ui.js';

const TYPES = { numeric: 'Number', string: 'Text', consolidated: 'Consolidation' };
/** What an element without children has as its list of them: one list, never changed. */
const NO_CHILDREN = Object.freeze([]);
/** How tall a child's row is in an element's detail: its controls and the gap below them. */
const CHILD_ROW = 36;

export async function render(target, { params, titled }) {
  // The model in outline: a dimension's elements are fetched as it is opened, however many.
  const view = await get('/model/outline');
  return new ModelPage(target, view, params, titled);
}

class ModelPage {
  /** `titled` names the page in the browser's title. */
  constructor(target, view, params, titled) {
    this.titled = titled;
    this.target = target;
    this.version = view.version;
    // Each dimension in outline, by its name, type, size and subsets' names; each cube whole.
    this.definition = { dimensions: view.dimensions, cubes: view.cubes };
    this.editor = null;
    this.list = h('nav', { class: 'model-list', 'aria-label': 'Dimensions and cubes' });
    this.pane = h('section', { class: 'model-pane' });
    this.element = h('div', { class: 'model-page' }, this.list, this.pane);
    mount(target, this.element);
    // As wide as the names it lists need: icon, name, count and padding.
    const width = textWidths();
    const names = [...this.definition.dimensions, ...this.definition.cubes].map((item) => width(item.name));
    splitter(this.element, { name: 'model-list', key: 'model', min: 200, max: 520, fit: () => Math.ceil(Math.max(120, ...names) + 130) }).fit();
    this.drawList();
    this.update(params);
  }

  /**
   * Opens the dimension or cube the address names beside the list, which stays as it is,
   * scrolled and searched: choosing from it changes only what is open.
   */
  update(params) {
    const dimension = params.get('dimension');
    const cube = params.get('cube');
    this.chosen = dimension ? ['dimension', dimension] : cube ? ['cube', cube] : null;
    this.titled(this.chosen?.[1]);
    this.drawItems();
    this.editor = null;
    this.opening = null;
    this.element.classList.remove('editing');
    if (this.chosen) this.open(...this.chosen);
    else mount(this.pane, empty('model', 'Choose a dimension or a cube', 'Pick one from the list to edit it, or add a new one.'));
  }

  /** Asked before the page is left: unsaved edits are discarded only if the user agrees. */
  async leave() {
    return this.editor ? this.editor.leave() : true;
  }

  unsaved() {
    return Boolean(this.editor?.dirty);
  }

  /** Saves the dimension or cube being edited, as its strip's Save button does. */
  saveChanges() {
    return this.editor?.save();
  }

  drawList() {
    const search = h('input', { class: 'input', type: 'search', placeholder: 'Find', 'aria-label': 'Find a dimension or cube' });
    const items = h('div', { class: 'model-items' });
    const draw = () => {
      const words = search.value.trim().toLowerCase();
      const entry = (kind, name, iconName, detail) => {
        if (words && !name.toLowerCase().includes(words)) return null;
        const current = this.chosen?.[0] === kind && this.chosen[1] === name;
        return h('a', {
          class: 'model-item',
          href: `/model?${kind}=${encodeURIComponent(name)}`,
          'data-link': '',
          'aria-current': current ? 'page' : null,
        }, icon(iconName, 16), h('span', { class: 'grow' }, name), h('span', { class: 'muted' }, detail));
      };
      mount(items,
        h('div', { class: 'model-section' }, h('span', {}, 'Dimensions'),
          h('button', { class: 'button quiet small icon', 'aria-label': 'Add a dimension', onclick: () => this.addDimension() }, icon('plus', 16))),
        this.definition.dimensions.map((d) => entry('dimension', d.name, d.type === 'time' ? 'chart' : 'dimension', d.element_count.toLocaleString())),
        h('div', { class: 'model-section' }, h('span', {}, 'Cubes'),
          h('button', { class: 'button quiet small icon', 'aria-label': 'Add a cube', onclick: () => this.addCube() }, icon('plus', 16))),
        this.definition.cubes.map((c) => entry('cube', c.name, 'cube', `${c.dimensions.length}`)));
    };
    search.addEventListener('input', draw);
    this.versionBadge = h('span', { class: 'badge' });
    mount(this.list,
      h('div', { class: 'model-head' }, h('h1', {}, 'Model'), this.versionBadge,
        h('button', { class: 'button quiet small icon', 'aria-label': 'Earlier versions of the model', title: 'Earlier versions, to compare and restore', onclick: (event) => this.showHistory(event.currentTarget) }, icon('undo', 16))),
      h('div', { class: 'search' }, icon('search', 16), search),
      items);
    // The items, again, as the model or the one chosen changes; the search stays as typed.
    this.drawItems = () => {
      this.versionBadge.textContent = `Version ${this.version}`;
      draw();
    };
  }

  open(kind, name) {
    const found = kind === 'dimension'
      ? this.definition.dimensions.find((d) => d.name === name)
      : this.definition.cubes.find((c) => c.name === name);
    if (!found) {
      mount(this.pane, empty('search', `There is no ${kind} called ${name}`, 'It may have been renamed or removed.'));
      return;
    }
    // Too narrow for both, the page shows the list or the one open, which leads back to it.
    this.element.classList.add('editing');
    if (kind === 'cube') {
      this.editor = new CubeEditor(this, JSON.stringify(found));
      return;
    }
    // A dimension's definition is fetched as it is opened, the pane saying so meanwhile; one
    // chosen after it is opened in its place. Its text is the editor's original, read once.
    const opening = (this.opening = {});
    mount(this.pane, loading('panes'));
    get(`/dimensions/${part(name)}/definition`, { raw: true }).then((dimension) => {
      if (this.opening === opening) this.editor = new DimensionEditor(this, dimension);
    }, (problem) => {
      if (this.opening === opening) mount(this.pane, failure(problem, () => this.open(kind, name)));
    });
  }

  /** Saves a change to the model; says what it did, and when the model had moved on. */
  async save(path, body) {
    try {
      const update = await put(path, { based_on: this.version, ...body });
      this.version = update.version;
      const dropped = update.dropped_cells
        ? ` ${update.dropped_cells.toLocaleString()} stored cell${update.dropped_cells === 1 ? '' : 's'} no longer fitted and ${update.dropped_cells === 1 ? 'was' : 'were'} removed.`
        : '';
      notify(`Saved. The model is now at version ${update.version}.${dropped}`, 'good');
      return true;
    } catch (problem) {
      this.refused(problem);
      return false;
    }
  }

  /**
   * Keeps the page's outline of the model as saved, and its list current: a cube whole, and a
   * dimension by its name, type, size and subsets' names, its elements being the editor's.
   */
  replace(kind, item) {
    const list = kind === 'cube' ? this.definition.cubes : this.definition.dimensions;
    const kept = kind === 'cube'
      ? structuredClone(item)
      : { name: item.name, type: item.type ?? 'standard', element_count: item.elements.length, subsets: Object.keys(item.subsets ?? {}).sort() };
    const at = list.findIndex((entry) => entry.name === item.name);
    if (at >= 0) list[at] = kept;
    else list.push(kept);
    this.drawItems();
  }

  async remove(kind, name) {
    const update = await dialog({
      title: `Remove ${name}?`,
      body: h('p', {}, kind === 'cube'
        ? `The cube ${name} and every cell stored in it are removed. This cannot be undone.`
        : `The dimension ${name} is removed. A dimension a cube still uses cannot be removed.`),
      confirm: `Remove ${kind}`,
      danger: true,
      onsubmit: async () => {
        try {
          return await del(`/${kind === 'cube' ? 'cubes' : 'dimensions'}/${part(name)}?based_on=${this.version}`);
        } catch (problem) {
          throw new Error(problem.status === 409 ? 'Someone changed the model since you opened it. Reload the page, then try again.' : problem.message);
        }
      },
    });
    if (!update) return;
    this.version = update.version;
    const key = kind === 'cube' ? 'cubes' : 'dimensions';
    this.definition[key] = this.definition[key].filter((entry) => entry.name !== name);
    notify(`Removed ${name}.`, 'good');
    this.editor = null;
    go('/model');
  }

  /**
   * The earlier versions of the model the server keeps: each can be compared with the
   * current one and restored, which makes it the newest version. `button` shows the
   * fetching at work.
   */
  async showHistory(button) {
    let earlier;
    try {
      earlier = await working(button, () => get('/admin/models'));
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    if (!earlier) return;
    const list = h('ul', { class: 'history-list' }, earlier.length
      ? earlier.map((kept) => h('li', {},
        h('span', { class: 'grow' }, h('strong', {}, `Version ${kept.version}`), kept.name !== String(kept.version) ? h('span', { class: 'muted' }, ` (${kept.name})`) : null),
        h('span', { class: 'muted' }, kept.saved ? when(kept.saved) : null),
        h('button', {
          class: 'button small',
          type: 'button',
          // Each row's button says which version it is for, heard apart from the others, and
          // begins with the words shown, which speech input says to press it.
          'aria-label': `Compare and restore version ${kept.version}…`,
          onclick: (event) => this.compare(kept, event.currentTarget),
        }, 'Compare and restore…')))
      : h('li', { class: 'muted' }, 'No earlier versions are kept yet: each change to the model keeps the one it replaces.'));
    await dialog({ title: 'Earlier versions of the model', confirm: 'Close', cancel: null, body: list });
  }

  /** Shows what restoring an earlier model would change, and restores it if asked to. */
  async compare(kept, button) {
    // The model now whole, to compare with the earlier one, which only this asks for.
    let earlier;
    let current;
    try {
      const both = await working(button, () => Promise.all([get(`/admin/models/${part(kept.name)}`), get('/model')]));
      [earlier, current] = [both?.[0]?.model, both?.[1]?.model];
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    if (!earlier || !current) return;
    document.querySelector('dialog[open]')?.close();
    const said = [['dimensions', 'Dimensions'], ['cubes', 'Cubes'], ['dashboards', 'Dashboards']].flatMap(([key, label]) => {
      const byName = (list) => new Map((list ?? []).map((item) => [item.name, JSON.stringify(item)]));
      const [now, then] = [byName(current[key]), byName(earlier[key])];
      const added = [...then.keys()].filter((name) => !now.has(name));
      const removed = [...now.keys()].filter((name) => !then.has(name));
      const changed = [...then.keys()].filter((name) => now.has(name) && now.get(name) !== then.get(name));
      const line = (verb, names) => (names.length ? h('li', {}, h('strong', {}, `${label} ${verb}: `), names.join(', ')) : null);
      return [line('brought back', added), line('taken away', removed), line('put back as they were', changed)].filter(Boolean);
    });
    const agreed = await dialog({
      title: `Restore version ${kept.version}?`,
      confirm: 'Restore it',
      danger: true,
      body: h('div', { class: 'facts' },
        said.length
          ? h('ul', { class: 'plain-list' }, said)
          : h('p', {}, 'It is the same as the current model.'),
        h('p', { class: 'hint' }, `It becomes version ${Number(this.version) + 1}, and the current model is kept among the earlier ones, so this too can be undone. Cells stored where the restored model has no place for them are removed.`)),
    });
    // Changes being edited would go with the page; they are given up only if the user agrees.
    if (!agreed || !(await this.leave())) return;
    try {
      const update = await put('/model', { based_on: this.version, model: earlier });
      notify(`Restored version ${kept.version} as version ${update.version}.${update.dropped_cells ? ` ${update.dropped_cells.toLocaleString()} stored cells no longer fitted and were removed.` : ''}`, 'good');
      // The page is drawn again from the model as it now is.
      location.assign('/model');
    } catch (problem) {
      this.refused(problem);
    }
  }

  refused(problem) {
    if (problem.status === 409) {
      notify('Someone changed the model since you opened it. Reload the page to see their change, then make yours again.', 'bad');
    } else {
      notify(problem.message, 'bad');
    }
  }

  async addDimension() {
    const name = h('input', { class: 'input', id: 'new-name', required: true });
    const type = h('select', { class: 'select', id: 'new-type' }, h('option', { value: 'standard' }, 'Standard'), h('option', { value: 'time' }, 'Time'));
    const added = await dialog({
      title: 'Add a dimension',
      confirm: 'Add dimension',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'new-name' }, 'Name'), name),
        h('div', { class: 'field' }, h('label', { for: 'new-type' }, 'Type'), type),
        h('p', { class: 'hint' }, 'It starts with one consolidation, Total, to build from.')),
      onsubmit: async () => {
        const value = name.value.trim();
        if (!value) throw invalid(name, 'Give the dimension a name.');
        if (this.definition.dimensions.some((d) => d.name.toLowerCase() === value.toLowerCase())) throw invalid(name, 'There is already a dimension of that name.');
        const dimension = { name: value, type: type.value, elements: [{ name: 'Total', type: 'consolidated', children: [] }] };
        if (!(await this.save(`/dimensions/${part(value)}`, { dimension }))) throw new Error('It was not added.');
        this.replace('dimension', dimension);
        return value;
      },
    });
    if (added) go(`/model?dimension=${encodeURIComponent(added)}`);
  }

  async addCube() {
    const name = h('input', { class: 'input', id: 'new-name', required: true });
    const chosen = new Set();
    const boxes = this.definition.dimensions.map((d) => h('label', { class: 'check' },
      h('input', { type: 'checkbox', onchange: (event) => (event.target.checked ? chosen.add(d.name) : chosen.delete(d.name)) }), d.name));
    const added = await dialog({
      title: 'Add a cube',
      confirm: 'Add cube',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'new-name' }, 'Name'), name),
        h('fieldset', { class: 'field' }, h('legend', {}, 'Dimensions, in order'), h('div', { class: 'checks' }, boxes))),
      onsubmit: async () => {
        const value = name.value.trim();
        if (!value) throw invalid(name, 'Give the cube a name.');
        if (chosen.size < 1) throw invalid(boxes[0]?.querySelector('input'), 'Choose at least one dimension.');
        const dimensions = this.definition.dimensions.map((d) => d.name).filter((d) => chosen.has(d));
        const cube = { name: value, dimensions };
        if (!(await this.save(`/cubes/${part(value)}`, { cube }))) throw new Error('It was not added.');
        this.replace('cube', cube);
        return value;
      },
    });
    if (added) go(`/model?cube=${encodeURIComponent(added)}`);
  }
}

/* ---- Shared by both editors ---- */

class Editor {
  /**
   * `original` is the dimension or cube as JSON text, what Discard goes back to. The working
   * copy is read from it once: a dimension of a million elements was read as it came, written
   * out again and read a second time, a third of a second more.
   */
  constructor(page, original, kind) {
    this.page = page;
    this.kind = kind;
    this.original = original;
    this.working = JSON.parse(original);
    // The original as the working copy would be written out, to tell whether it changed:
    // worked out once it is first asked, as the server writes some things otherwise.
    this.written = null;
    // Its name as it was opened, which a rename not yet saved leaves as it is.
    const opened = this.working.name;
    // Whether the working copy has been changed since it was the original, and what is
    // worked out from it, kept until it changes.
    this.edited = false;
    this.worked = new Map();
    this.tab = 'main';
    this.strip = h('div', { class: 'pending', role: 'status', hidden: true });
    this.body = h('div', { class: 'editor-body' });
    this.tabs = h('div');
    mount(page.pane,
      h('div', { class: 'editor-head' },
        h('div', { class: 'grow' },
          h('a', { class: 'editor-back', href: '/model', 'data-link': '' }, icon('back', 14), 'Dimensions and cubes'),
          h('p', { class: 'muted' }, kind === 'dimension' ? 'Dimension' : 'Cube'),
          h('h2', {}, opened)),
        this.tabs,
        h('button', { class: 'button danger', onclick: () => page.remove(kind, opened) }, icon('trash', 16), 'Remove')),
      this.body,
      this.strip);
  }

  /** What `work()` gives for the working copy as it is: worked out again once it changes. */
  kept(name, work) {
    if (!this.worked.has(name)) this.worked.set(name, work());
    return this.worked.get(name);
  }

  /** The working copy as the original again, as it is after it is saved or discarded. */
  unchanged() {
    this.edited = false;
    this.worked.clear();
  }

  get dirty() {
    // Unchanged, it is the original: a million elements need not be written out to say so.
    return this.edited && this.kept('dirty', () => {
      this.written ??= JSON.stringify(JSON.parse(this.original));
      return JSON.stringify(this.working) !== this.written;
    });
  }

  /** Marks the working copy changed and draws it again. */
  changed() {
    this.edited = true;
    this.worked.clear();
    this.draw();
  }

  drawStrip() {
    this.page.titled(this.working.name, this.dirty);
    this.strip.hidden = !this.dirty;
    if (!this.dirty) return;
    mount(this.strip,
      icon('edit', 16),
      h('span', { class: 'grow' }, `Unsaved changes to ${this.working.name}.`),
      h('button', { class: 'button small', onclick: () => this.discard() }, 'Discard'),
      h('button', { class: 'button small primary', onclick: (event) => working(event.currentTarget, () => this.save()) }, icon('save', 14), 'Save'));
  }

  drawTabs(tabs) {
    choices({
      into: this.tabs,
      label: `Parts of the ${this.kind}`,
      options: tabs,
      chosen: this.tab,
      choose: (id) => {
        this.tab = id;
        this.draw();
      },
    });
  }

  discard() {
    this.working = JSON.parse(this.original);
    this.unchanged();
    this.draw();
  }

  async save() {
    const path = `/${this.kind === 'cube' ? 'cubes' : 'dimensions'}/${part(this.working.name)}`;
    if (await this.page.save(path, { [this.kind]: this.working })) {
      this.original = JSON.stringify(this.working);
      this.written = this.original;
      this.unchanged();
      this.page.replace(this.kind, this.working);
      this.draw();
    }
  }

  async leave() {
    if (!this.dirty) return true;
    const agreed = await dialog({
      title: 'Discard your changes?',
      body: h('p', {}, `Your changes to ${this.working.name} are not saved. Leaving discards them.`),
      confirm: 'Discard and leave',
      cancel: 'Stay',
      danger: true,
    });
    return Boolean(agreed);
  }

  /** The working copy as JSON, to read and edit whole. */
  drawJson(note) {
    const text = h('textarea', { class: 'textarea json-editor', spellcheck: 'false', 'aria-label': 'Definition as JSON', rows: 24 });
    text.value = JSON.stringify(this.working, null, 2);
    const error = h('p', { class: 'error-text', role: 'alert', hidden: true });
    mount(this.body, h('div', { class: 'card card-body facts json-card' },
      h('p', { class: 'muted' }, note),
      text,
      error,
      h('div', { class: 'toolbar' }, h('button', {
        class: 'button primary',
        onclick: () => {
          try {
            const parsed = JSON.parse(text.value);
            if (parsed.name !== this.working.name) throw new Error(`Keep the name ${this.working.name}; add a new ${this.kind} to make another.`);
            this.working = parsed;
            error.hidden = true;
            this.tab = 'main';
            this.changed();
          } catch (problem) {
            error.textContent = problem.message;
            error.hidden = false;
          }
        },
      }, 'Use this definition'))));
  }
}

/* ---- Dimensions ---- */

class DimensionEditor extends Editor {
  constructor(page, original) {
    super(page, original, 'dimension');
    this.selected = null;
    this.open = new Set();
    // What the tree was searched for, and the members open while it shows what was found.
    this.words = '';
    this.found = null;
    this.search = searchBox({
      label: 'Find an element',
      onsearch: (words) => {
        this.words = words;
        const { roots, children, parents } = this.hierarchy();
        if (words) {
          this.found = reveal({ roots, children, parents, matches: this.matching() });
        } else {
          // Back to browsing, opened to the element chosen while searching.
          if (this.selected) for (const name of reveal({ roots, children, parents, matches: new Set([this.selected]) })) this.open.add(name);
          this.found = null;
        }
        this.drawTree();
      },
      ondown: () => {
        const first = this.rows?.find((row) => row.match) ?? this.rows?.[0];
        if (first) this.select(first.name);
      },
    });
    this.draw();
  }

  /** The tree's roots, and each element's children, as the working copy has them. */
  hierarchy() {
    return this.kept('hierarchy', () => {
      const byName = this.byName;
      // Each element's parents, in the order they are declared: one by its name, as most
      // have, and several in a list.
      const above = new Map();
      for (const element of this.working.elements) {
        for (const child of element.children ?? []) {
          const had = above.get(child.name);
          if (had === undefined) above.set(child.name, element.name);
          else if (typeof had === 'string') {
            if (had !== element.name) above.set(child.name, [had, element.name]);
          } else if (had.at(-1) !== element.name) had.push(element.name);
        }
      }
      return {
        byName,
        roots: this.working.elements.filter((element) => !above.has(element.name)).map((element) => element.name),
        // A leaf, as most are, has no lists made for it: one empty list serves them all.
        children: (name) => {
          const element = byName.get(name);
          if (!element?.children?.length) return NO_CHILDREN;
          return element.children.map((child) => child.name).filter((child) => byName.has(child));
        },
        parents: (name) => [above.get(name) ?? []].flat(),
      };
    });
  }

  /** The elements whose names have the words searched for. */
  matching() {
    const lowered = this.kept('lowered', () => this.working.elements.map((element) => element.name.toLowerCase()));
    const found = new Set();
    lowered.forEach((name, index) => {
      if (name.includes(this.words)) found.add(this.working.elements[index].name);
    });
    return found;
  }

  /** Chooses an element to edit, keeping the keys on it in the tree. */
  select(name) {
    this.selected = name;
    this.draw();
    this.focusRow(name);
  }

  /** Puts the keys on an element's row of the tree, scrolled into view. */
  focusRow(name) {
    this.list?.reveal(this.rows.findIndex((row) => row.name === name))?.focus();
  }

  /** Opens or closes elements of the tree as shown, the keys staying where they were. */
  toggle(names, opened) {
    const into = this.found ?? this.open;
    for (const name of names) {
      if (opened) into.add(name);
      else into.delete(name);
    }
    const focused = document.activeElement?.closest?.('.element-row')?.dataset.name;
    this.drawTree();
    if (focused) this.focusRow(focused);
  }

  /**
   * How wide the list of elements must be to show each whole, indented to its depth, with
   * its type beside it.
   */
  listWidth() {
    return this.kept('width', () => {
      const { byName, roots } = this.hierarchy();
      // Each element's depth, the first it is reached at, top down: each queued once, as its
      // element, so that a leaf, as most are, is passed at a glance.
      const depths = new Map(roots.map((name) => [name, 0]));
      const queue = [...depths.keys()].map((name) => byName.get(name));
      for (let at = 0; at < queue.length; at += 1) {
        const element = queue[at];
        if (!element.children?.length) continue;
        const depth = depths.get(element.name) + 1;
        for (const child of element.children) {
          const below = byName.get(child.name);
          if (!below || depths.has(child.name)) continue;
          depths.set(child.name, depth);
          queue.push(below);
        }
      }
      const names = widest(this.working.elements, (element) => element.name, (element) => (depths.get(element.name) ?? 0) * INDENT);
      const type = Math.max(...Object.values(TYPES).map(textWidths()));
      const weighted = this.working.elements.some((element) => element.children?.some((child) => (child.weight ?? 1) !== 1));
      // The row's indent, button, icon, gaps and type, the tree's padding and its scroll bar.
      return Math.ceil(names + type + (weighted ? 56 : 0) + 120);
    });
  }

  get byName() {
    return this.kept('byName', () => {
      // Set one by one, rather than made from a pair for each of a million.
      const byName = new Map();
      for (const element of this.working.elements) byName.set(element.name, element);
      return byName;
    });
  }

  draw() {
    this.drawTabs([['main', 'Elements'], ['attributes', 'Attributes'], ['levels', 'Levels'], ['json', 'JSON']]);
    this.drawStrip();
    if (this.tab === 'attributes') return this.drawAttributes();
    if (this.tab === 'levels') return this.drawLevels();
    if (this.tab === 'json') return this.drawJson('Everything the dimension holds, subsets included. Edits apply when you choose to use them, and are saved with Save.');
    this.drawElements();
  }

  drawElements() {
    const levels = this.levelsOf();
    const height = levels.reduce((most, level) => Math.max(most, level), 0);
    const names = this.working.levels ?? [];
    const open = (names) => {
      if (this.found) this.found = new Set(names);
      else this.open = new Set(names);
      this.drawTree();
    };
    const consolidated = () => this.working.elements.filter((element) => element.children?.length).map((element) => element.name);
    // Drawn again, the tree stays scrolled where it was.
    const scrolled = this.tree?.isConnected ? this.tree.scrollTop : 0;
    this.tree = h('div', { class: 'element-tree', role: 'tree', 'aria-label': 'Elements' });
    this.none = h('p', { class: 'muted tree-none', hidden: true });
    this.list = new Rows(this.tree, (index) => this.drawRow(index));
    this.tree.append(this.list.element, this.none);
    this.tree.addEventListener('keydown', (event) => {
      const row = event.target.closest('.element-row');
      if (!row) return;
      treeKey(event, this.rows, Number(row.dataset.index), {
        go: (name) => this.select(name),
        open: (names, opened) => this.toggle(names, opened),
        // Enter goes to the element's name, to edit it.
        act: () => this.body.querySelector('#element-name')?.focus(),
        page: this.list.page,
      });
    });
    // Copying gives the element chosen and all beneath it as an outline; pasting adds one.
    this.tree.addEventListener('copy', (event) => {
      const { roots } = this.hierarchy();
      event.preventDefault();
      event.clipboardData.setData('text/plain', this.outline(this.selected ? [this.selected] : roots));
    });
    this.tree.addEventListener('paste', (event) => {
      const text = event.clipboardData?.getData('text/plain');
      if (!text) return;
      event.preventDefault();
      this.addOutline(text);
    });
    const selected = this.byName.get(this.selected);
    const layout = h('div', { class: 'element-layout' },
      h('div', { class: 'card element-list' },
        h('div', { class: 'card-head' },
          h('h3', { class: 'grow' }, `${this.working.elements.length.toLocaleString()} elements`),
          h('button', { class: 'button small quiet', title: 'Add elements from an outline, as pasted from elsewhere', onclick: () => this.pasteElements() }, icon('paste', 14), 'Paste'),
          h('button', { class: 'button small', onclick: () => this.addElement() }, icon('plus', 14), 'Add element')),
        h('div', { class: 'tree-tools' },
          this.search.box,
          openButtons({
            all: () => open(consolidated()),
            none: () => open([]),
            levels: Array.from({ length: height + 1 }, (_, level) => height - level).map((level) => [level, names[level] || (level === 0 ? 'Leaves' : `Level ${level}`)]),
            to: (level) => open(this.working.elements.filter((_, index) => levels[index] > level).map((element) => element.name)),
          })),
        this.tree),
      h('div', { class: 'card element-detail' }, selected ? this.detail(selected) : empty('member', 'Choose an element', 'Its name, type, children and attributes are edited here.')));
    mount(this.body, layout);
    // The list is as wide as its elements need, or as wide as it was dragged to be.
    splitter(layout, { name: 'element-list', key: 'elements', fit: () => this.listWidth(), min: 400, max: 960 }).fit();
    this.drawTree();
    this.tree.scrollTop = scrolled;
    this.list.update();
  }

  /** The tree of elements: as browsed, or showing what the search found in its place. */
  drawTree() {
    const { byName, roots, children, parents } = this.hierarchy();
    const matches = this.words ? this.matching() : null;
    this.shown = byName;
    ({ rows: this.rows } = treeRows({ roots, children, parents, open: this.found ?? this.open, matches }));
    // The row the keys act on: the element chosen, or else the first found or shown.
    this.at = this.rows.findIndex((row) => row.name === this.selected);
    if (this.at < 0) this.at = Math.max(0, this.rows.findIndex((row) => row.match));
    this.list.show(this.rows.length);
    this.none.hidden = this.rows.length > 0;
    this.none.textContent = this.words ? 'Nothing matches.' : 'No elements yet.';
    this.search.found(matches?.size ?? 0, Boolean(matches));
  }

  /** A row of the tree of elements. */
  drawRow(index) {
    const row = this.rows[index];
    const element = this.shown.get(row.name);
    const weight = row.parent ? this.shown.get(row.parent)?.children?.find((child) => child.name === row.name)?.weight ?? 1 : 1;
    return treeRow(row, this.selected === row.name, {
      class: `element-row${row.match ? ' match' : ''}${row.context ? ' context' : ''}`,
      tabindex: index === this.at ? '0' : '-1',
      dataset: { index },
      onclick: () => this.select(row.name),
    },
    twisty(row, (opened) => this.toggle([row.name], opened)),
    icon(element.type === 'consolidated' ? 'sigma' : 'member', 14),
    h('span', { class: 'grow' }, row.match ? marked(row.name, this.words) : row.name),
    weight !== 1 ? h('span', { class: 'badge' }, `× ${weight}`) : null,
    h('span', { class: 'muted element-type' }, TYPES[element.type]));
  }
  detail(element) {
    const byName = this.byName;
    const name = h('input', { class: 'input', id: 'element-name', value: element.name });
    name.value = element.name;
    name.addEventListener('change', () => this.rename(element.name, name.value.trim()));
    const type = h('select', { class: 'select', id: 'element-type', onchange: (event) => this.retype(element, event.target.value) },
      Object.entries(TYPES).map(([value, label]) => h('option', { value, selected: element.type === value }, label)));
    const parents = this.hierarchy().parents(element.name);
    const attributes = Object.entries(this.working.attributes ?? {});
    return h('div', { class: 'facts' },
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, element.name),
        h('button', { class: 'button small danger', onclick: () => this.deleteElement(element.name) }, icon('trash', 14), 'Delete')),
      h('div', { class: 'card-body facts' },
        h('div', { class: 'field' }, h('label', { for: 'element-name' }, 'Name'), name),
        h('div', { class: 'field' }, h('label', { for: 'element-type' }, 'Type'), type),
        element.type === 'string' ? this.picklistField(element) : null,
        parents.length ? h('p', { class: 'muted' }, `Part of ${parents.join(', ')}.`) : null,
        element.type === 'consolidated' ? h('div', { class: 'field' },
          h('label', {}, 'Children and their weights'),
          (element.children ?? []).length ? this.childList(element, byName) : h('p', { class: 'muted' }, 'No children yet.'),
          h('div', { class: 'toolbar' }, h('button', { class: 'button small child-adding', onclick: () => this.addChild(element) }, icon('plus', 14), 'Add a child'))) : null,
        attributes.length ? h('div', { class: 'field' },
          h('label', {}, 'Attributes'),
          attributes.map(([attribute, kind]) => {
            const input = h('input', {
              class: 'input',
              type: kind === 'numeric' ? 'number' : 'text',
              step: 'any',
              'aria-label': attribute,
              placeholder: attribute,
              onchange: (event) => {
                const raw = event.target.value;
                element.attributes ??= {};
                if (raw === '') delete element.attributes[attribute];
                else element.attributes[attribute] = kind === 'numeric' ? Number(raw) : raw;
                if (!Object.keys(element.attributes).length) delete element.attributes;
                this.changed();
              },
            });
            input.value = element.attributes?.[attribute] ?? '';
            return h('div', { class: 'attribute-row' }, h('span', { class: 'muted' }, `${attribute}${kind === 'alias' ? ' (alias)' : ''}`), input);
          })) : null));
  }

  /**
   * A consolidation's children, each with its weight to change and a button to take it out:
   * drawn only where they can be seen in the list, as a total may have a million.
   */
  childList(element, byName) {
    const list = h('div', { class: 'child-list' });
    const rows = new Rows(list, (index) => {
      const child = element.children[index];
      return h('div', { class: 'child-row' },
        icon(byName.get(child.name)?.type === 'consolidated' ? 'sigma' : 'member', 14),
        h('span', { class: 'grow' }, child.name),
        h('input', {
          class: 'input weight',
          type: 'number',
          step: 'any',
          value: child.weight ?? 1,
          'aria-label': `Weight of ${child.name}`,
          onchange: (event) => {
            const weight = Number(event.target.value);
            if (Number.isFinite(weight)) element.children[index].weight = weight;
            this.changed();
          },
        }),
        h('button', {
          class: 'button quiet small icon',
          'aria-label': `Take ${child.name} out of ${element.name}`,
          onclick: () => {
            element.children.splice(index, 1);
            this.changed();
            // The keys go to the child that took its place, or the one before, or to Add.
            const next = this.childRows.reveal(index) ?? this.childRows.reveal(index - 1);
            (next?.querySelector('button:last-of-type') ?? this.body.querySelector('.child-adding'))?.focus();
          },
        }, icon('close', 14)));
    }, { row: CHILD_ROW });
    list.append(rows.element);
    rows.show(element.children.length);
    this.childRows = rows;
    return list;
  }

  rename(from, to) {
    if (!to || to === from) return this.draw();
    if (this.byName.has(to)) {
      notify(`There is already an element called ${to}.`, 'bad');
      return this.draw();
    }
    for (const element of this.working.elements) {
      if (element.name === from) element.name = to;
      for (const child of element.children ?? []) if (child.name === from) child.name = to;
    }
    if (this.open.delete(from)) this.open.add(to);
    this.selected = to;
    this.changed();
  }

  retype(element, type) {
    if (type !== 'consolidated' && element.children?.length) {
      notify(`${element.name} no longer adds up its ${element.children.length} children.`);
    }
    element.type = type;
    if (type === 'consolidated') element.children ??= [];
    else delete element.children;
    // Only a string element offers a picklist.
    if (type !== 'string') delete element.picklist;
    this.changed();
  }

  /**
   * A string element's picklist, the values its text cells may be given where it is the
   * element of a cube's last dimension: any text, values listed one to a line, or the
   * elements of a dimension, or of one of its subsets.
   */
  picklistField(element) {
    const picklist = element.picklist;
    const kind = !picklist ? '' : Array.isArray(picklist) ? 'values' : 'members';
    // Each dimension in outline, with its subsets' names: this one's as it is being edited.
    const dimensions = this.page.definition.dimensions.map((dimension) => (dimension.name === this.working.name
      ? { name: dimension.name, subsets: Object.keys(this.working.subsets ?? {}) }
      : dimension));
    const how = h('select', {
      class: 'select',
      id: 'picklist-kind',
      onchange: (event) => {
        const chosen = event.target.value;
        if (!chosen) delete element.picklist;
        else element.picklist = chosen === 'values' ? [] : { dimension: dimensions[0].name };
        this.changed();
      },
    },
    h('option', { value: '', selected: kind === '' }, 'Any text'),
    h('option', { value: 'values', selected: kind === 'values' }, 'Values listed here'),
    h('option', { value: 'members', selected: kind === 'members' }, 'The elements of a dimension'));
    let chooser = null;
    if (kind === 'values') {
      chooser = h('textarea', { class: 'textarea', rows: 4, spellcheck: 'false', 'aria-label': 'The values offered, one to a line', placeholder: 'One value to a line' });
      chooser.value = picklist.join('\n');
      chooser.addEventListener('change', () => {
        element.picklist = chooser.value.split('\n').map((line) => line.trim()).filter(Boolean);
        this.changed();
      });
    } else if (kind === 'members') {
      // Named as the server names them, ignoring case.
      const same = (name, named) => name.toLowerCase() === named?.toLowerCase();
      const from = dimensions.find((dimension) => same(dimension.name, picklist.dimension));
      chooser = h('div', { class: 'field-pair' },
        h('select', {
          class: 'select',
          'aria-label': 'The dimension whose elements are offered',
          onchange: (event) => {
            element.picklist = { dimension: event.target.value };
            this.changed();
          },
        }, dimensions.map((dimension) => h('option', { value: dimension.name, selected: dimension === from }, dimension.name))),
        h('select', {
          class: 'select',
          'aria-label': 'Its subset, if only those',
          onchange: (event) => {
            element.picklist = { dimension: picklist.dimension, ...(event.target.value ? { subset: event.target.value } : {}) };
            this.changed();
          },
        },
        h('option', { value: '' }, 'Every element'),
        (from?.subsets ?? []).map((name) => h('option', { value: name, selected: same(name, picklist.subset) }, `The subset ${name}`))));
    }
    return h('div', { class: 'field' },
      h('label', { for: 'picklist-kind' }, 'Picklist'),
      how,
      chooser,
      h('span', { class: 'hint' }, 'Where this is a cube\'s last dimension, its text cells take only the values offered, chosen from a list. Text stored before stays.'));
  }

  deleteElement(name) {
    this.working.elements = this.working.elements.filter((element) => element.name !== name);
    for (const element of this.working.elements) {
      if (element.children) element.children = element.children.filter((child) => child.name !== name);
    }
    this.selected = null;
    this.changed();
  }

  /** Asks for a new element's name and type. */
  async ask(title, withWeight) {
    const name = h('input', { class: 'input', id: 'element-new', required: true, list: 'element-names' });
    const type = h('select', { class: 'select', id: 'element-new-type' }, Object.entries(TYPES).map(([value, label]) => h('option', { value }, label)));
    const weight = h('input', { class: 'input', id: 'element-new-weight', type: 'number', step: 'any', value: '1' });
    return dialog({
      title,
      confirm: 'Add',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'element-new' }, 'Name'), name,
          withWeight ? h('span', { class: 'hint' }, 'An element that already exists is added as a child as it is.') : null),
        h('datalist', { id: 'element-names' }, this.working.elements.map((element) => h('option', { value: element.name }))),
        h('div', { class: 'field' }, h('label', { for: 'element-new-type' }, 'Type, for a new element'), type),
        withWeight ? h('div', { class: 'field' }, h('label', { for: 'element-new-weight' }, 'Weight'), weight) : null),
      onsubmit: () => {
        const value = name.value.trim();
        if (!value) throw invalid(name, 'Give the element a name.');
        const w = Number(weight.value);
        if (withWeight && !Number.isFinite(w)) throw invalid(weight, 'The weight must be a number.');
        return { name: value, type: type.value, weight: w };
      },
    });
  }

  async addElement() {
    const answer = await this.ask('Add an element', false);
    if (!answer) return;
    if (this.byName.has(answer.name)) {
      notify(`There is already an element called ${answer.name}.`, 'bad');
      return;
    }
    this.working.elements.push(answer.type === 'consolidated'
      ? { name: answer.name, type: answer.type, children: [] }
      : { name: answer.name, type: answer.type });
    this.selected = answer.name;
    this.changed();
  }

  async addChild(parent) {
    const answer = await this.ask(`Add a child to ${parent.name}`, true);
    if (!answer) return;
    if (answer.name === parent.name) {
      notify('An element cannot be its own child.', 'bad');
      return;
    }
    if (parent.children.some((child) => child.name === answer.name)) {
      notify(`${answer.name} is already a child of ${parent.name}.`, 'bad');
      return;
    }
    if (!this.byName.has(answer.name)) {
      this.working.elements.push(answer.type === 'consolidated'
        ? { name: answer.name, type: answer.type, children: [] }
        : { name: answer.name, type: answer.type });
    }
    parent.children.push(answer.weight === 1 ? { name: answer.name } : { name: answer.name, weight: answer.weight });
    this.open.add(parent.name);
    this.changed();
  }

  /* ---- Elements as outlines, to copy and paste ---- */

  /**
   * Elements and everything beneath them as an outline: one to a line, a tab deeper for
   * each level, and a weight other than one after a tab.
   */
  outline(names) {
    const { byName } = this.hierarchy();
    const lines = [];
    const visit = (name, depth, weight, trail) => {
      if (trail.has(name)) return;
      lines.push(`${'\t'.repeat(depth)}${name}${weight === 1 ? '' : `\t${weight}`}`);
      const next = new Set(trail).add(name);
      for (const child of byName.get(name)?.children ?? []) if (byName.has(child.name)) visit(child.name, depth + 1, child.weight ?? 1, next);
    };
    for (const name of names) visit(name, 0, 1, new Set());
    return lines.join('\n');
  }

  /** Asks for an outline of elements, as typed or pasted, and adds them. */
  async pasteElements() {
    const text = h('textarea', { class: 'textarea', id: 'outline', rows: 12, spellcheck: 'false', placeholder: 'Revenue\n\tProduct Revenue\n\tService Revenue\nCosts\n\tRent\t-1' });
    const target = this.pasteTarget();
    const answer = await dialog({
      title: target ? `Add elements beneath ${target.name}` : 'Add elements',
      confirm: 'Add',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'outline' }, 'Elements, one to a line'), text),
        h('p', { class: 'hint' }, 'Indent a line with a tab, or spaces, to put it beneath the line above. A tab and a number after a name give its weight. Names the dimension has already are placed again where the outline puts them.')),
      onsubmit: () => text.value,
    });
    if (answer) this.addOutline(answer);
  }

  /** Where pasted elements go: beneath the element chosen, or beside it if it is a leaf. */
  pasteTarget() {
    const byName = this.byName;
    const chosen = byName.get(this.selected);
    if (!chosen) return null;
    if (chosen.type === 'consolidated') return chosen;
    return byName.get(this.hierarchy().parents(chosen.name)[0]) ?? null;
  }

  /**
   * Adds the elements an outline names: those new to the dimension as consolidations where
   * lines beneath them go deeper and as numbers otherwise, and each beneath the line above
   * it that is less indented, or beneath where pasted elements go. Says what it did, and
   * what it left out: a child of a leaf, or one that would contain itself.
   */
  addOutline(text) {
    const lines = text.replace(/\r\n?/g, '\n').split('\n').filter((line) => line.trim());
    if (!lines.length) return;
    // Spaces indent too: the narrowest indent of spaces is one level.
    const spaces = lines.map((line) => line.match(/^ */)[0].length).filter(Boolean);
    const unit = spaces.length ? spaces.reduce((least, count) => Math.min(least, count)) : 1;
    const parsed = lines.map((line) => {
      const indent = line.match(/^[\t ]*/)[0];
      const [name, weight] = line.slice(indent.length).split('\t');
      const depth = [...indent].filter((c) => c === '\t').length + Math.floor(indent.replaceAll('\t', '').length / unit);
      return { name: name.trim(), depth, weight: weight === undefined || !weight.trim() ? 1 : parseNumber(weight) ?? 1 };
    });
    const least = parsed.reduce((low, line) => Math.min(low, line.depth), Infinity);
    const target = this.pasteTarget();
    // The elements by name, and their children, kept as the outline adds to them.
    const byName = new Map(this.byName);
    const children = (name) => (byName.get(name)?.children ?? []).map((child) => child.name).filter((child) => byName.has(child));
    const parents = [];
    const added = new Set();
    const placed = [];
    const left = [];
    let previous = -1;
    parsed.forEach((line, index) => {
      // A line indented past the one above goes just one deeper.
      const depth = Math.min(line.depth - least, previous + 1);
      previous = depth;
      parents[depth] = line.name;
      const deeper = parsed[index + 1] && parsed[index + 1].depth - least > depth;
      let element = byName.get(line.name);
      if (!element) {
        element = deeper ? { name: line.name, type: 'consolidated', children: [] } : { name: line.name, type: 'numeric' };
        this.working.elements.push(element);
        byName.set(line.name, element);
        added.add(line.name);
      }
      const parent = depth === 0 ? target : byName.get(parents[depth - 1]);
      if (!parent) return;
      if (parent.type !== 'consolidated') {
        left.push(`${line.name} (${parent.name} is not a consolidation)`);
        return;
      }
      if (parent.children?.some((child) => child.name === line.name)) return;
      if (line.name === parent.name || beneath([line.name], children).has(parent.name)) {
        left.push(`${line.name} (it would contain itself)`);
        return;
      }
      parent.children ??= [];
      parent.children.push(line.weight === 1 ? { name: line.name } : { name: line.name, weight: line.weight });
      if (!added.has(line.name)) placed.push(line.name);
      this.open.add(parent.name);
    });
    const said = [
      added.size ? `Added ${added.size.toLocaleString()} element${added.size === 1 ? '' : 's'}.` : null,
      placed.length ? `Placed ${placed.length.toLocaleString()} the dimension had already.` : null,
      left.length ? `Left out ${left.slice(0, 3).join(', ')}${left.length > 3 ? ` and ${left.length - 3} more` : ''}.` : null,
    ].filter(Boolean).join(' ');
    notify(said || 'Nothing new to add.', left.length ? 'bad' : 'good');
    this.changed();
  }

  drawAttributes() {
    const attributes = Object.entries(this.working.attributes ?? {});
    const name = h('input', { class: 'input', placeholder: 'New attribute', 'aria-label': 'New attribute name' });
    const kind = h('select', { class: 'select', 'aria-label': 'New attribute type' },
      h('option', { value: 'string' }, 'Text'), h('option', { value: 'numeric' }, 'Number'), h('option', { value: 'alias' }, 'Alias'));
    mount(this.body, h('div', { class: 'card' },
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Attributes'), h('span', { class: 'muted' }, 'Facts each element may carry; an alias is another name it can be found by.')),
      attributes.length
        ? h('table', { class: 'table' },
          h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Type'), h('th', { class: 'actions' }, ''))),
          h('tbody', {}, attributes.map(([attribute, type]) => h('tr', {},
            h('td', {}, attribute),
            h('td', {}, { string: 'Text', numeric: 'Number', alias: 'Alias' }[type]),
            h('td', { class: 'actions' }, h('button', {
              class: 'button quiet small',
              onclick: () => {
                delete this.working.attributes[attribute];
                for (const element of this.working.elements) {
                  if (element.attributes) {
                    delete element.attributes[attribute];
                    if (!Object.keys(element.attributes).length) delete element.attributes;
                  }
                }
                if (!Object.keys(this.working.attributes).length) delete this.working.attributes;
                this.changed();
              },
            }, 'Remove'))))))
        : h('div', { class: 'card-body' }, h('p', { class: 'muted' }, 'No attributes yet.')),
      h('div', { class: 'card-body toolbar' }, name, kind, h('button', {
        class: 'button',
        onclick: () => {
          const value = name.value.trim();
          if (!value) return;
          this.working.attributes = { ...(this.working.attributes ?? {}), [value]: kind.value };
          this.changed();
        },
      }, icon('plus', 14), 'Add attribute'))));
  }

  drawLevels() {
    const height = this.levelsOf().reduce((most, level) => Math.max(most, level), 0) + 1;
    const levels = this.working.levels ?? [];
    mount(this.body, h('div', { class: 'card' },
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Levels'), h('span', { class: 'muted' }, 'Names for each level, from the leaves up. A time dimension\'s Year, Quarter, Month and Week let periods-to-date work.')),
      h('div', { class: 'card-body facts' }, Array.from({ length: height }, (_, level) => {
        const input = h('input', {
          class: 'input',
          'aria-label': `Name of level ${level}`,
          placeholder: level === 0 ? 'Leaves' : `Level ${level}`,
          onchange: (event) => {
            const next = [...(this.working.levels ?? [])];
            while (next.length <= level) next.push('');
            next[level] = event.target.value.trim();
            while (next.length && !next[next.length - 1]) next.pop();
            if (next.length) this.working.levels = next;
            else delete this.working.levels;
            this.changed();
          },
        });
        input.value = levels[level] ?? '';
        return h('div', { class: 'attribute-row' }, h('span', { class: 'muted' }, level === 0 ? 'Level 0, the leaves' : `Level ${level}`), input);
      }))));
  }

  /** The level of each element: zero for a leaf, one above its highest child otherwise. */
  levelsOf() {
    return this.kept('levels', () => {
      const byName = this.byName;
      const memo = new Map();
      // The elements being worked out, one within another, by which a loop is found; one set
      // for all, where one made for each element was a million.
      const trail = new Set();
      const level = (name) => {
        // A leaf, as most are, is known as one by its element alone.
        const element = byName.get(name);
        if (!element?.children?.length || trail.has(name)) return 0;
        const known = memo.get(name);
        if (known !== undefined) return known;
        trail.add(name);
        // A loop, not a spread: a total over a million children is too many arguments.
        let highest = 0;
        for (const child of element.children) highest = Math.max(highest, level(child.name));
        trail.delete(name);
        memo.set(name, highest + 1);
        return highest + 1;
      };
      return this.working.elements.map((element) => level(element.name));
    });
  }
}

/* ---- Cubes ---- */

class CubeEditor extends Editor {
  constructor(page, original) {
    super(page, original, 'cube');
    this.draw();
  }

  draw() {
    this.drawTabs([['main', 'Dimensions'], ['rules', 'Rules and feeders'], ['checks', 'Checks'], ['views', 'Views'], ['json', 'JSON']]);
    this.drawStrip();
    if (this.tab === 'rules') return this.drawRules();
    if (this.tab === 'checks') return this.drawChecks();
    if (this.tab === 'views') return this.drawViews();
    if (this.tab === 'json') return this.drawJson('Everything the cube holds. Edits apply when you choose to use them, and are saved with Save.');
    this.drawDimensions();
  }

  drawDimensions() {
    const dims = this.working.dimensions;
    const others = this.page.definition.dimensions.map((d) => d.name).filter((name) => !dims.includes(name));
    const move = (index, step) => {
      const [moved] = dims.splice(index, 1);
      dims.splice(index + step, 0, moved);
      this.changed();
    };
    const add = h('select', { class: 'select', 'aria-label': 'Add a dimension to the cube' }, h('option', { value: '' }, 'Add a dimension…'), others.map((name) => h('option', { value: name }, name)));
    add.addEventListener('change', () => {
      if (!add.value) return;
      dims.push(add.value);
      this.changed();
    });
    mount(this.body, h('div', { class: 'card' },
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Dimensions'), h('span', { class: 'muted' }, 'Changing them removes the cells stored in the cube.')),
      h('ol', { class: 'dimension-list' }, dims.map((name, index) => h('li', {},
        icon('dimension', 16),
        h('span', { class: 'grow' }, name),
        h('button', { class: 'button quiet small icon', 'aria-label': `Move ${name} up`, disabled: index === 0, onclick: () => move(index, -1) }, icon('up', 14)),
        h('button', { class: 'button quiet small icon', 'aria-label': `Move ${name} down`, disabled: index === dims.length - 1, onclick: () => move(index, 1) }, icon('down', 14)),
        h('button', {
          class: 'button quiet small icon',
          'aria-label': `Remove ${name}`,
          disabled: dims.length === 1,
          onclick: () => {
            dims.splice(index, 1);
            this.changed();
            refocus(this.body.querySelector('.dimension-list'), index, this.body.querySelector('.dimension-list ~ .card-body .select'));
          },
        }, icon('close', 14))))),
      others.length ? h('div', { class: 'card-body toolbar' }, add) : null));
  }

  drawRules() {
    const area = (key, label, help) => {
      const text = h('textarea', { class: 'textarea', spellcheck: 'false', rows: 8, 'aria-label': label });
      text.value = (this.working[key] ?? []).join('\n');
      text.addEventListener('change', () => {
        const lines = text.value.split('\n').map((line) => line.trim()).filter(Boolean);
        if (lines.length) this.working[key] = lines;
        else delete this.working[key];
        this.changed();
      });
      return h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, label), h('span', { class: 'muted' }, help)),
        h('div', { class: 'card-body' }, text));
    };
    const check = h('button', { class: 'button', onclick: (event) => checkFeeders(this.working.name, event.currentTarget) }, icon('check', 16), 'Check the feeders');
    mount(this.body, h('div', { class: 'facts' },
      area('rules', 'Rules', 'One a line: [N: | C: | S:] area = expression. The first that covers a cell decides it.'),
      area('feeders', 'Feeders', 'One a line: source => target, … They say which calculated cells may hold a value.'),
      h('div', { class: 'toolbar' }, check, h('span', { class: 'hint' }, 'Looks for calculated cells that hold a value but are not fed. It checks what is saved.'))));
  }

  /**
   * The cube's checks of its values: for each, the cells it checks, what holds where a value
   * is acceptable, what is said where not, and whether such a value is refused as it is
   * written or kept and marked.
   */
  drawChecks() {
    const checks = this.working.validations ?? [];
    const keep = (list) => {
      if (list.length) this.working.validations = list;
      else delete this.working.validations;
      this.changed();
    };
    // A field typed into changes its check where it is, and only the strip is drawn again,
    // so that Tab goes on to the next field.
    const field = (check, index, key, label, example) => {
      const id = `check-${index}-${key}`;
      const input = h('input', { class: 'input mono', id, value: check[key] ?? '', placeholder: example, spellcheck: 'false', autocomplete: 'off' });
      input.addEventListener('change', () => {
        check[key] = input.value.trim();
        this.drawStrip();
      });
      return h('div', { class: 'field' }, h('label', { for: id }, label), input);
    };
    const card = (check, index) => {
      const refuse = h('input', { type: 'checkbox', checked: Boolean(check.refuse) });
      refuse.addEventListener('change', () => {
        if (refuse.checked) check.refuse = true;
        else delete check.refuse;
        this.drawStrip();
      });
      return h('div', { class: 'card' },
        h('div', { class: 'card-head' }, icon('check', 16), h('h3', { class: 'grow' }, `Check ${index + 1}`),
          h('button', { class: 'button small danger', onclick: () => keep(checks.filter((each) => each !== check)) }, 'Remove')),
        h('div', { class: 'card-body facts' },
          field(check, index, 'cells', 'Cells', 'N: [Account].[Salaries]'),
          field(check, index, 'valid', 'Valid where', '[Account].[Salaries] >= 0'),
          field(check, index, 'message', 'Message', 'Salaries are never negative.'),
          h('label', { class: 'check' }, refuse, 'Refuse such a value as it is written, rather than keep it and mark it')));
    };
    mount(this.body, h('div', { class: 'facts' },
      checks.length ? checks.map(card) : h('div', { class: 'card' }, empty('check', 'No checks yet', 'A check marks the values that break it, or refuses them as they are written.')),
      h('div', { class: 'toolbar' },
        h('button', { class: 'button', onclick: () => keep([...checks, { cells: '', valid: '', message: '' }]) }, icon('plus', 16), 'Add a check'),
        h('span', { class: 'hint' }, 'Cells name an area as a rule does; N:, C: or S: before it keeps to leaf, consolidated or text cells. Valid where is written as a rule\'s expression is: true, or a number but zero, where a value is acceptable.'))));
  }

  drawViews() {
    const views = Object.entries(this.working.views ?? {});
    mount(this.body, views.length
      ? h('div', { class: 'facts' }, views.map(([name, view]) => {
        const text = h('textarea', { class: 'textarea', spellcheck: 'false', rows: 4, 'aria-label': `Query of ${name}` });
        text.value = view.mdx;
        text.addEventListener('change', () => {
          // The query changes; the view's sizes, format and highlights stay with it.
          this.working.views[name] = { ...view, mdx: text.value.trim() };
          this.changed();
        });
        return h('div', { class: 'card' },
          h('div', { class: 'card-head' }, icon('grid', 16), h('h3', { class: 'grow' }, name),
            h('a', { class: 'button small', href: `/explore?cube=${encodeURIComponent(this.working.name)}`, 'data-link': '' }, 'Explore'),
            h('button', {
              class: 'button small danger',
              onclick: () => {
                delete this.working.views[name];
                if (!Object.keys(this.working.views).length) delete this.working.views;
                this.changed();
              },
            }, 'Remove')),
          h('div', { class: 'card-body' }, text));
      }))
      : h('div', { class: 'card' }, empty('grid', 'No views yet', 'Build a query in the explorer and save it as a view from its Views menu.')));
  }
}

export { failure };
