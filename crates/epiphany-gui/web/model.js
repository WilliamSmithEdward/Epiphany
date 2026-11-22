// Editing the model: dimensions, their elements, attributes and levels, and cubes with
// their rules, feeders and views. Every change is saved against the model version it was
// made from, so that nobody's change is overwritten unseen.

import { del, get, part, put } from './api.js';
import { empty, failure, go } from './app.js';
import { dialog, h, icon, menu, mount, notify } from './ui.js';

const TYPES = { numeric: 'Number', string: 'Text', consolidated: 'Consolidation' };

export async function render(target, { params }) {
  const view = await get('/model');
  const page = new ModelPage(target, view, params);
  return page;
}

class ModelPage {
  constructor(target, view, params) {
    this.target = target;
    this.version = view.version;
    this.definition = view.model;
    this.editor = null;
    const dimension = params.get('dimension');
    const cube = params.get('cube');
    this.list = h('nav', { class: 'model-list', 'aria-label': 'Dimensions and cubes' });
    this.pane = h('section', { class: 'model-pane' });
    mount(target, h('div', { class: 'model-page' }, this.list, this.pane));
    this.drawList(dimension, cube);
    if (dimension) this.open('dimension', dimension);
    else if (cube) this.open('cube', cube);
    else mount(this.pane, empty('model', 'Choose a dimension or a cube', 'Pick one from the list to edit it, or add a new one.'));
  }

  /** Asked before the page is left: unsaved edits are discarded only if the user agrees. */
  async leave() {
    return this.editor ? this.editor.leave() : true;
  }

  drawList(dimension, cube) {
    const search = h('input', { class: 'input', type: 'search', placeholder: 'Find', 'aria-label': 'Find a dimension or cube' });
    const items = h('div', { class: 'model-items' });
    const draw = () => {
      const words = search.value.trim().toLowerCase();
      const entry = (kind, name, iconName, detail) => {
        if (words && !name.toLowerCase().includes(words)) return null;
        const current = (kind === 'dimension' ? dimension : cube) === name;
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
        this.definition.dimensions.map((d) => entry('dimension', d.name, d.type === 'time' ? 'chart' : 'dimension', d.elements.length.toLocaleString())),
        h('div', { class: 'model-section' }, h('span', {}, 'Cubes'),
          h('button', { class: 'button quiet small icon', 'aria-label': 'Add a cube', onclick: () => this.addCube() }, icon('plus', 16))),
        this.definition.cubes.map((c) => entry('cube', c.name, 'cube', `${c.dimensions.length}`)));
    };
    search.addEventListener('input', draw);
    mount(this.list,
      h('div', { class: 'model-head' }, h('h2', {}, 'Model'), h('span', { class: 'badge' }, `Version ${this.version}`)),
      h('div', { class: 'search' }, icon('search', 16), search),
      items);
    draw();
  }

  open(kind, name) {
    const found = kind === 'dimension'
      ? this.definition.dimensions.find((d) => d.name === name)
      : this.definition.cubes.find((c) => c.name === name);
    if (!found) {
      mount(this.pane, empty('search', `There is no ${kind} called ${name}`, 'It may have been renamed or removed.'));
      return;
    }
    this.editor = kind === 'dimension' ? new DimensionEditor(this, found) : new CubeEditor(this, found);
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

  /** Keeps the page's copy of the model as saved, and its list current. */
  replace(kind, item) {
    const list = kind === 'cube' ? this.definition.cubes : this.definition.dimensions;
    const at = list.findIndex((entry) => entry.name === item.name);
    if (at >= 0) list[at] = structuredClone(item);
    else list.push(structuredClone(item));
    this.drawList(kind === 'dimension' ? item.name : null, kind === 'cube' ? item.name : null);
  }

  async remove(kind, name) {
    const agreed = await dialog({
      title: `Remove ${name}?`,
      body: h('p', {}, kind === 'cube'
        ? `The cube ${name} and every cell stored in it are removed. This cannot be undone.`
        : `The dimension ${name} is removed. A dimension a cube still uses cannot be removed.`),
      confirm: `Remove ${kind}`,
      danger: true,
    });
    if (!agreed) return;
    try {
      const update = await del(`/${kind === 'cube' ? 'cubes' : 'dimensions'}/${part(name)}?based_on=${this.version}`);
      this.version = update.version;
      notify(`Removed ${name}.`, 'good');
      this.editor = null;
      go('/model');
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
        if (!value) throw new Error('Give the dimension a name.');
        if (this.definition.dimensions.some((d) => d.name.toLowerCase() === value.toLowerCase())) throw new Error('There is already a dimension of that name.');
        const dimension = { name: value, type: type.value, elements: [{ name: 'Total', type: 'consolidated', children: [] }] };
        if (!(await this.save(`/dimensions/${part(value)}`, { dimension }))) throw new Error('It was not added.');
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
        if (!value) throw new Error('Give the cube a name.');
        if (chosen.size < 1) throw new Error('Choose at least one dimension.');
        const dimensions = this.definition.dimensions.map((d) => d.name).filter((d) => chosen.has(d));
        if (!(await this.save(`/cubes/${part(value)}`, { cube: { name: value, dimensions } }))) throw new Error('It was not added.');
        return value;
      },
    });
    if (added) go(`/model?cube=${encodeURIComponent(added)}`);
  }
}

/* ---- Shared by both editors ---- */

class Editor {
  constructor(page, original, kind) {
    this.page = page;
    this.kind = kind;
    this.original = JSON.stringify(original);
    this.working = structuredClone(original);
    this.tab = 'main';
    this.strip = h('div', { class: 'pending', role: 'status', hidden: true });
    this.body = h('div', { class: 'editor-body' });
    this.tabs = h('div', { class: 'segmented', role: 'tablist' });
    mount(page.pane,
      h('div', { class: 'editor-head' },
        h('div', { class: 'grow' }, h('p', { class: 'muted' }, kind === 'dimension' ? 'Dimension' : 'Cube'), h('h1', {}, original.name)),
        this.tabs,
        h('button', { class: 'button danger', onclick: () => page.remove(kind, original.name) }, icon('trash', 16), 'Remove')),
      this.body,
      this.strip);
  }

  get dirty() {
    return JSON.stringify(this.working) !== this.original;
  }

  /** Marks the working copy changed and draws it again. */
  changed() {
    this.draw();
  }

  drawStrip() {
    this.strip.hidden = !this.dirty;
    if (!this.dirty) return;
    mount(this.strip,
      icon('edit', 16),
      h('span', { class: 'grow' }, `Unsaved changes to ${this.working.name}.`),
      h('button', { class: 'button small', onclick: () => this.discard() }, 'Discard'),
      h('button', { class: 'button small primary', onclick: () => this.save() }, icon('save', 14), 'Save'));
  }

  drawTabs(tabs) {
    mount(this.tabs, tabs.map(([id, label]) => h('button', {
      role: 'tab',
      'aria-selected': String(this.tab === id),
      onclick: () => {
        this.tab = id;
        this.draw();
      },
    }, label)));
  }

  discard() {
    this.working = JSON.parse(this.original);
    this.draw();
  }

  async save() {
    const path = `/${this.kind === 'cube' ? 'cubes' : 'dimensions'}/${part(this.working.name)}`;
    if (await this.page.save(path, { [this.kind]: this.working })) {
      this.original = JSON.stringify(this.working);
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
    mount(this.body, h('div', { class: 'card card-body facts' },
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
    this.draw();
  }

  get byName() {
    return new Map(this.working.elements.map((element) => [element.name, element]));
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
    const byName = this.byName;
    const children = new Set(this.working.elements.flatMap((element) => (element.children ?? []).map((child) => child.name)));
    const roots = this.working.elements.filter((element) => !children.has(element.name));
    const tree = h('div', { class: 'element-tree', role: 'tree', 'aria-label': 'Elements' });
    const visit = (element, depth, weight, trail) => {
      const kids = (element.children ?? []).filter((child) => byName.has(child.name) && !trail.has(child.name));
      const opened = this.open.has(element.name);
      tree.append(h('div', {
        class: 'element-row',
        role: 'treeitem',
        'aria-selected': String(this.selected === element.name),
        'aria-expanded': kids.length ? String(opened) : null,
        dataset: { depth: Math.min(depth, 8) },
        tabindex: '0',
        onclick: () => {
          this.selected = element.name;
          this.draw();
        },
        onkeydown: (event) => {
          if (event.key === 'Enter') {
            this.selected = element.name;
            this.draw();
          }
        },
      },
      kids.length
        ? h('button', {
          class: 'twisty',
          'aria-label': opened ? `Close ${element.name}` : `Open ${element.name}`,
          onclick: (event) => {
            event.stopPropagation();
            if (opened) this.open.delete(element.name);
            else this.open.add(element.name);
            this.draw();
          },
        }, icon(opened ? 'down' : 'chevron', 12))
        : h('span', { class: 'twisty' }),
      icon(element.type === 'consolidated' ? 'sigma' : 'member', 14),
      h('span', { class: 'grow' }, element.name),
      weight !== undefined && weight !== 1 ? h('span', { class: 'badge' }, `× ${weight}`) : null,
      h('span', { class: 'muted' }, TYPES[element.type])));
      if (opened) {
        const next = new Set(trail).add(element.name);
        for (const child of kids) visit(byName.get(child.name), depth + 1, child.weight ?? 1, next);
      }
    };
    for (const root of roots) visit(root, 0, undefined, new Set());
    const selected = byName.get(this.selected);
    mount(this.body, h('div', { class: 'element-layout' },
      h('div', { class: 'card element-list' },
        h('div', { class: 'card-head' },
          h('h3', { class: 'grow' }, `${this.working.elements.length.toLocaleString()} elements`),
          h('button', { class: 'button small', onclick: () => this.addElement() }, icon('plus', 14), 'Add element')),
        tree),
      h('div', { class: 'card element-detail' }, selected ? this.detail(selected) : empty('member', 'Choose an element', 'Its name, type, children and attributes are edited here.'))));
  }

  detail(element) {
    const byName = this.byName;
    const name = h('input', { class: 'input', id: 'element-name', value: element.name });
    name.value = element.name;
    name.addEventListener('change', () => this.rename(element.name, name.value.trim()));
    const type = h('select', { class: 'select', id: 'element-type', onchange: (event) => this.retype(element, event.target.value) },
      Object.entries(TYPES).map(([value, label]) => h('option', { value, selected: element.type === value }, label)));
    const parents = this.working.elements.filter((other) => other.children?.some((child) => child.name === element.name));
    const attributes = Object.entries(this.working.attributes ?? {});
    return h('div', { class: 'facts' },
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, element.name),
        h('button', { class: 'button small danger', onclick: () => this.deleteElement(element.name) }, icon('trash', 14), 'Delete')),
      h('div', { class: 'card-body facts' },
        h('div', { class: 'field' }, h('label', { for: 'element-name' }, 'Name'), name),
        h('div', { class: 'field' }, h('label', { for: 'element-type' }, 'Type'), type),
        parents.length ? h('p', { class: 'muted' }, `Part of ${parents.map((parent) => parent.name).join(', ')}.`) : null,
        element.type === 'consolidated' ? h('div', { class: 'field' },
          h('label', {}, 'Children and their weights'),
          (element.children ?? []).length
            ? h('div', { class: 'child-list' }, element.children.map((child, index) => h('div', { class: 'child-row' },
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
                },
              }, icon('close', 14)))))
            : h('p', { class: 'muted' }, 'No children yet.'),
          h('div', { class: 'toolbar' }, h('button', { class: 'button small', onclick: () => this.addChild(element) }, icon('plus', 14), 'Add a child'))) : null,
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
    this.changed();
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
        if (!value) throw new Error('Give the element a name.');
        const w = Number(weight.value);
        if (withWeight && !Number.isFinite(w)) throw new Error('The weight must be a number.');
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
    const height = Math.max(...this.levelsOf(), 0) + 1;
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
    const byName = this.byName;
    const memo = new Map();
    const level = (name, trail = new Set()) => {
      if (memo.has(name)) return memo.get(name);
      const element = byName.get(name);
      if (!element?.children?.length || trail.has(name)) return 0;
      trail.add(name);
      const value = 1 + Math.max(...element.children.map((child) => level(child.name, trail)));
      trail.delete(name);
      memo.set(name, value);
      return value;
    };
    return this.working.elements.map((element) => level(element.name));
  }
}

/* ---- Cubes ---- */

class CubeEditor extends Editor {
  constructor(page, original) {
    super(page, original, 'cube');
    this.draw();
  }

  draw() {
    this.drawTabs([['main', 'Dimensions'], ['rules', 'Rules and feeders'], ['views', 'Views'], ['json', 'JSON']]);
    this.drawStrip();
    if (this.tab === 'rules') return this.drawRules();
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
    const add = h('select', { class: 'select', 'aria-label': 'Add a dimension' }, h('option', { value: '' }, 'Add a dimension…'), others.map((name) => h('option', { value: name }, name)));
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
    const check = h('button', { class: 'button', onclick: () => this.checkFeeders() }, icon('check', 16), 'Check the feeders');
    mount(this.body, h('div', { class: 'facts' },
      area('rules', 'Rules', 'One a line: [N: | C: | S:] area = expression. The first that covers a cell decides it.'),
      area('feeders', 'Feeders', 'One a line: source => target, … They say which calculated cells may hold a value.'),
      h('div', { class: 'toolbar' }, check, h('span', { class: 'hint' }, 'Looks for calculated cells that hold a value but are not fed. It checks what is saved.'))));
  }

  async checkFeeders() {
    try {
      const answer = await get(`/cubes/${part(this.working.name)}/feeders/check`);
      if (!answer.unfed.length) {
        notify('Every calculated cell that holds a value is fed.', 'good');
        return;
      }
      await dialog({
        title: `${answer.unfed.length} cell${answer.unfed.length === 1 ? ' is' : 's are'} not fed`,
        confirm: 'Close',
        cancel: null,
        body: h('div', { class: 'facts' },
          h('p', { class: 'muted' }, 'A rule gives these cells a value, but no feeder feeds them, so totals leave them out.'),
          h('ul', { class: 'plain-list' }, answer.unfed.map((cell) => h('li', { class: 'mono' }, cell.at.join(' · '))))),
      });
    } catch (problem) {
      notify(problem.message, 'bad');
    }
  }

  drawViews() {
    const views = Object.entries(this.working.views ?? {});
    mount(this.body, views.length
      ? h('div', { class: 'facts' }, views.map(([name, view]) => {
        const text = h('textarea', { class: 'textarea', spellcheck: 'false', rows: 4, 'aria-label': `Query of ${name}` });
        text.value = view.mdx;
        text.addEventListener('change', () => {
          this.working.views[name] = { mdx: text.value.trim() };
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
