// Exploring a cube: a field list, a layout of rows, columns and filters, and the pivot grid
// of the query that layout makes.

import { ApiError, get, part, post } from './api.js';
import { empty, failure, go, model, state } from './app.js';
import { dialog, download, formatNumber, h, icon, menu, mount, notify } from './ui.js';

/* ---- Names in MDX ---- */

const quote = (name) => `[${String(name).replaceAll(']', ']]')}]`;
const member = (dimension, name) => `${quote(dimension)}.${quote(name)}`;

/* ---- Dimensions, fetched once for each model version ---- */

const dimensions = new Map();

async function dimension(name) {
  if (!dimensions.has(name)) {
    const view = await get(`/dimensions/${part(name)}`);
    const byName = new Map(view.elements.map((element) => [element.name, element]));
    const roots = view.elements.filter((element) => element.parents.length === 0).map((element) => element.name);
    dimensions.set(name, { ...view, byName, roots });
  }
  return dimensions.get(name);
}

/* ---- The layout ---- */

/**
 * A layout is `{ cube, rows, columns, filters, nonEmpty, sort, mdx }`. Each entry of rows
 * and columns is `{ dimension, members, expanded }`, the members chosen in order and those
 * opened to show their children; each filter is `{ dimension, member }`. `mdx` is set when
 * the query has been written by hand.
 */
function defaultLayout(cube, dims) {
  const time = cube.dimensions.find((name) => dims.get(name).type === 'time');
  const rows = cube.dimensions[0];
  const columns = time && time !== rows ? time : cube.dimensions[1];
  const axis = (name) => name ? [{ dimension: name, members: [...dims.get(name).roots], expanded: [] }] : [];
  return {
    cube: cube.name,
    rows: axis(rows),
    columns: axis(columns),
    filters: cube.dimensions.filter((name) => name !== rows && name !== columns)
      .map((name) => ({ dimension: name, member: dims.get(name).roots[0] })),
    nonEmpty: false,
    sort: null,
    mdx: null,
  };
}

/** The members an axis entry shows: each chosen member, then its children where it is open. */
function shown(entry, dim) {
  const open = new Set(entry.expanded);
  const out = [];
  const visit = (name, depth) => {
    out.push({ name, depth });
    if (open.has(name)) {
      for (const child of dim.byName.get(name)?.children ?? []) visit(child.name, depth + 1);
    }
  };
  for (const name of entry.members) visit(name, 0);
  return out;
}

/** The MDX a layout asks. */
function mdx(layout, dims) {
  if (layout.mdx) return layout.mdx;
  const set = (entries) => entries
    .map((entry) => `{${shown(entry, dims.get(entry.dimension)).map(({ name }) => member(entry.dimension, name)).join(', ')}}`)
    .join(' * ');
  const nonEmpty = layout.nonEmpty ? 'NON EMPTY ' : '';
  let rows = set(layout.rows);
  if (layout.sort && rows) {
    const column = layout.sort.tuple.map(([dim, name]) => member(dim, name)).join(', ');
    rows = `ORDER(${rows}, (${column}), ${layout.sort.descending ? 'BDESC' : 'BASC'})`;
  }
  const axes = [];
  if (layout.columns.length) axes.push(`${nonEmpty}${set(layout.columns)} ON COLUMNS`);
  if (layout.rows.length) axes.push(`${nonEmpty}${rows} ON ROWS`);
  const slicer = layout.filters.map((filter) => member(filter.dimension, filter.member));
  const where = slicer.length ? `\nWHERE (${slicer.join(', ')})` : '';
  return `SELECT\n  ${axes.join(',\n  ')}\nFROM ${quote(layout.cube)}${where}`;
}

const saved = (cube) => `epiphany.explore.${cube}`;

/* ---- The page ---- */

export async function render(target, { params }) {
  const definition = await model();
  if (!definition.cubes.length) {
    mount(target, h('div', { class: 'page' }, empty('cube', 'No cubes yet', 'An administrator adds cubes to the model.')));
    return;
  }
  const name = params.get('cube') ?? definition.cubes[0].name;
  const cube = definition.cubes.find((candidate) => candidate.name === name);
  if (!cube) {
    mount(target, h('div', { class: 'page' }, empty('cube', `There is no cube called ${name}`, 'It may have been renamed or removed.',
      h('a', { class: 'button', href: '/explore', 'data-link': '' }, 'Explore another cube'))));
    return;
  }
  dimensions.clear();
  const dims = new Map(await Promise.all(cube.dimensions.map(async (dim) => [dim, await dimension(dim)])));
  return new Explorer(target, definition, cube, dims);
}

class Explorer {
  constructor(target, definition, cube, dims) {
    this.definition = definition;
    this.cube = cube;
    this.dims = dims;
    this.layout = this.restore() ?? defaultLayout(cube, dims);
    this.past = [];
    this.future = [];
    this.view = 'grid';
    this.result = null;
    this.ticket = 0;
    // Writing back: the transaction the edits are held in, the edits by cell, and the cells
    // of the result that may be written, as "row,column".
    this.tx = null;
    this.edits = new Map();
    this.writable = new Set();
    addEventListener('beforeunload', (event) => {
      if (this.tx && this.page.isConnected) event.preventDefault();
    });
    // A page closed with changes unsaved lets them go at once, rather than leaving them to
    // hold their cells until the server gives up on them.
    addEventListener('pagehide', () => {
      if (this.tx) {
        fetch(`/api/transactions/${this.tx}/rollback`, { method: 'POST', keepalive: true, headers: { 'x-requested-by': 'epiphany-gui' } });
      }
    });
    this.build(target);
    this.run();
  }

  /** Asked before the page is left: unsaved changes are discarded only if the user agrees. */
  async leave() {
    if (!this.tx) return true;
    const agreed = await dialog({
      title: 'Discard your changes?',
      body: h('p', {}, `You have ${this.changes || 0} unsaved change${this.changes === 1 ? '' : 's'} to ${this.cube.name}. Leaving discards them.`),
      confirm: 'Discard and leave',
      cancel: 'Stay',
      danger: true,
    });
    if (!agreed) return false;
    await this.discard();
    return true;
  }

  /** Whether the user may write to this cube at all. */
  mayWrite() {
    const role = state.user?.cubes?.[this.cube.name] ?? state.user?.role;
    return role === 'write' || role === 'admin';
  }

  restore() {
    try {
      const layout = JSON.parse(localStorage.getItem(saved(this.cube.name)));
      const known = (name) => this.dims.has(name);
      const placed = [...layout.rows, ...layout.columns].map((entry) => entry.dimension).concat(layout.filters.map((filter) => filter.dimension));
      if (layout.cube !== this.cube.name || !placed.every(known) || new Set(placed).size !== placed.length) return null;
      // Members that no longer exist are let go.
      for (const entry of [...layout.rows, ...layout.columns]) {
        const dim = this.dims.get(entry.dimension);
        entry.members = entry.members.filter((name) => dim.byName.has(name));
        entry.expanded = entry.expanded.filter((name) => dim.byName.has(name));
        if (!entry.members.length) entry.members = [...dim.roots];
      }
      for (const filter of layout.filters) {
        if (!this.dims.get(filter.dimension).byName.has(filter.member)) filter.member = this.dims.get(filter.dimension).roots[0];
      }
      return layout;
    } catch {
      return null;
    }
  }

  /** Changes the layout, keeping the old one to go back to, and runs the query again. */
  change(edit) {
    this.past.push(JSON.stringify(this.layout));
    if (this.past.length > 100) this.past.shift();
    this.future = [];
    edit(this.layout);
    this.after();
  }

  after() {
    localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
    this.drawLayout();
    this.drawToolbar();
    this.run();
  }

  undo() {
    if (!this.past.length) return;
    this.future.push(JSON.stringify(this.layout));
    this.layout = JSON.parse(this.past.pop());
    this.after();
  }

  redo() {
    if (!this.future.length) return;
    this.past.push(JSON.stringify(this.layout));
    this.layout = JSON.parse(this.future.pop());
    this.after();
  }

  /* ---- Building the page ---- */

  build(target) {
    this.fields = h('aside', { class: 'fields', 'aria-label': 'Fields' });
    this.toolbar = h('div', { class: 'toolbar' });
    this.layoutBar = h('div', { class: 'layout-bar' });
    this.output = h('div', { class: 'output' });
    this.statusLine = h('div', { class: 'status-line', role: 'status', 'aria-live': 'polite' });
    this.pendingBar = h('div', { class: 'pending', role: 'status', 'aria-live': 'polite', hidden: true });
    const cubes = h('select', {
      class: 'select',
      'aria-label': 'Cube',
      onchange: (event) => go(`/explore?cube=${encodeURIComponent(event.target.value)}`),
    }, this.definition.cubes.map((cube) => h('option', { value: cube.name, selected: cube.name === this.cube.name }, cube.name)));
    const views = Object.keys(this.cube.views ?? {});
    const viewButton = views.length
      ? h('button', {
        class: 'button',
        'aria-haspopup': 'menu',
        onclick: (event) => menu([{ heading: 'Saved views' }, ...views.map((name) => ({
          label: name,
          icon: 'grid',
          action: () => this.change((layout) => {
            layout.mdx = this.cube.views[name].mdx;
          }),
        }))], event.currentTarget),
      }, icon('grid', 16), 'Views', icon('down', 14))
      : null;
    this.tabs = h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'Show as' });
    this.page = h('div', { class: 'explorer' },
      this.fields,
      h('section', { class: 'work' },
        h('div', { class: 'work-head' },
          h('div', { class: 'toolbar' }, icon('cube', 20), cubes, viewButton, h('div', { class: 'grow' }), this.tabs),
          this.toolbar,
          this.layoutBar),
        this.output,
        this.pendingBar,
        this.statusLine));
    mount(target, this.page);
    this.drawFields();
    this.drawTabs();
    this.drawToolbar();
    this.drawLayout();
    this.page.addEventListener('keydown', (event) => {
      if (event.target.closest('input, textarea, select')) return;
      if ((event.ctrlKey || event.metaKey) && event.key === 'z') {
        event.preventDefault();
        if (event.shiftKey) this.redo();
        else this.undo();
      } else if ((event.ctrlKey || event.metaKey) && event.key === 'y') {
        event.preventDefault();
        this.redo();
      }
    });
  }

  /** The strip that says what is not yet saved, with the ways to save or discard it. */
  drawPending() {
    this.pendingBar.hidden = !this.tx;
    if (!this.tx) return;
    const count = this.changes || 0;
    mount(this.pendingBar,
      icon('edit', 16),
      h('span', { class: 'grow' }, `${count} unsaved change${count === 1 ? '' : 's'}. Totals already include them; nothing is stored until you save.`),
      h('button', { class: 'button small', onclick: () => this.discard() }, 'Discard'),
      h('button', { class: 'button small primary', onclick: () => this.save() }, icon('save', 14), 'Save changes'));
  }

  drawTabs() {
    const tab = (id, label, iconName) => h('button', {
      role: 'tab',
      'aria-selected': String(this.view === id),
      onclick: () => {
        this.view = id;
        this.drawTabs();
        this.draw();
      },
    }, label);
    mount(this.tabs, tab('grid', 'Grid'), tab('mdx', 'MDX'));
  }

  drawToolbar() {
    this.drawPending();
    const layout = this.layout;
    const written = Boolean(layout.mdx);
    const button = (label, iconName, action, props = {}) => h('button', { class: 'button small quiet', onclick: action, ...props }, icon(iconName, 16), h('span', {}, label));
    mount(this.toolbar,
      button('Undo', 'undo', () => this.undo(), { disabled: !this.past.length, title: 'Undo (Ctrl Z)' }),
      button('Redo', 'redo', () => this.redo(), { disabled: !this.future.length, title: 'Redo (Ctrl Y)' }),
      h('span', { class: 'divider' }),
      button('Swap axes', 'swap', () => this.change((l) => {
        [l.rows, l.columns] = [l.columns, l.rows];
        l.sort = null;
      }), { disabled: written }),
      button(layout.nonEmpty ? 'Showing non-empty' : 'Showing empty rows', layout.nonEmpty ? 'eyeOff' : 'eye',
        () => this.change((l) => {
          l.nonEmpty = !l.nonEmpty;
        }), { disabled: written, 'aria-pressed': String(layout.nonEmpty), title: 'Leave out rows and columns with no values' }),
      layout.sort && !written ? button('Clear sort', 'close', () => this.change((l) => {
        l.sort = null;
      })) : null,
      h('div', { class: 'grow' }),
      button('Refresh', 'refresh', () => this.run()),
      button('Export CSV', 'download', () => this.exportCsv(), { disabled: !this.result }),
      button('Reset layout', 'trash', () => this.change((l) => Object.assign(l, defaultLayout(this.cube, this.dims)))));
  }

  /* ---- Field list ---- */

  drawFields() {
    const search = h('input', { class: 'input', type: 'search', placeholder: 'Find a dimension or element', 'aria-label': 'Find a dimension or element' });
    const list = h('div', { class: 'field-list', role: 'tree', 'aria-label': 'Dimensions' });
    const draw = () => {
      const words = search.value.trim().toLowerCase();
      mount(list, this.cube.dimensions.map((name) => {
        const dim = this.dims.get(name);
        const matches = words ? dim.elements.filter((element) => element.name.toLowerCase().includes(words)).slice(0, 8) : [];
        if (words && !name.toLowerCase().includes(words) && !matches.length) return null;
        const place = this.placeOf(name);
        return h('div', { class: 'field', role: 'treeitem', 'aria-expanded': 'false' },
          h('div', {
            class: 'field-row',
            draggable: 'true',
            tabindex: '0',
            ondragstart: (event) => {
              event.dataTransfer.setData('text/x-epiphany-dimension', name);
              event.dataTransfer.effectAllowed = 'move';
              this.page.classList.add('dragging');
            },
            ondragend: () => this.page.classList.remove('dragging'),
            onkeydown: (event) => {
              if (event.key === 'Enter' || event.key === ' ' || event.key === 'ContextMenu') {
                event.preventDefault();
                this.placeMenu(name, event.currentTarget);
              }
            },
            oncontextmenu: (event) => {
              event.preventDefault();
              this.placeMenu(name, { x: event.clientX, y: event.clientY });
            },
          },
          h('span', { class: 'drag' }, icon('drag', 14)),
          icon(dim.type === 'time' ? 'chart' : dim.type === 'measure' ? 'measure' : 'dimension', 16),
          h('span', { class: 'field-name' }, name),
          h('span', { class: `badge ${place === 'filters' ? '' : 'accent'}` }, { rows: 'Rows', columns: 'Columns', filters: 'Filter' }[place]),
          h('button', { class: 'button quiet small icon', 'aria-label': `Place ${name}`, onclick: (event) => this.placeMenu(name, event.currentTarget) }, icon('more', 16))),
          matches.length ? h('ul', { class: 'matches' }, matches.map((element) => h('li', {},
            h('button', {
              class: 'match',
              onclick: () => this.focusMember(name, element.name),
            }, icon(element.type === 'consolidated' ? 'sigma' : 'member', 14), element.name)))) : null);
      }));
    };
    search.addEventListener('input', draw);
    mount(this.fields,
      h('div', { class: 'fields-head' }, h('h3', {}, 'Fields'), h('span', { class: 'muted' }, `${this.cube.dimensions.length} dimensions`)),
      h('div', { class: 'search' }, icon('search', 16), search),
      list,
      h('p', { class: 'hint fields-hint' }, 'Drag a dimension to Rows, Columns or Filters, or press Enter on it to choose where it goes.'));
    this.drawFieldList = draw;
    draw();
  }

  placeOf(name) {
    if (this.layout.rows.some((entry) => entry.dimension === name)) return 'rows';
    if (this.layout.columns.some((entry) => entry.dimension === name)) return 'columns';
    return 'filters';
  }

  placeMenu(name, at) {
    const place = this.placeOf(name);
    menu([
      { heading: name },
      { label: 'Put on rows', icon: 'rows', disabled: place === 'rows' || Boolean(this.layout.mdx), action: () => this.move(name, 'rows') },
      { label: 'Put on columns', icon: 'columns', disabled: place === 'columns' || Boolean(this.layout.mdx), action: () => this.move(name, 'columns') },
      { label: 'Use as a filter', icon: 'filter', disabled: place === 'filters' || Boolean(this.layout.mdx), action: () => this.move(name, 'filters') },
      '-',
      { label: 'Choose members…', icon: 'check', disabled: Boolean(this.layout.mdx), action: () => this.pick(name) },
    ], at);
  }

  /** Puts a dimension on an axis or among the filters, at `index` there or at the end. */
  move(name, to, index) {
    this.change((layout) => {
      const dim = this.dims.get(name);
      let entry = [...layout.rows, ...layout.columns].find((candidate) => candidate.dimension === name);
      const filter = layout.filters.find((candidate) => candidate.dimension === name);
      layout.rows = layout.rows.filter((candidate) => candidate.dimension !== name);
      layout.columns = layout.columns.filter((candidate) => candidate.dimension !== name);
      layout.filters = layout.filters.filter((candidate) => candidate.dimension !== name);
      if (to === 'filters') {
        const chosen = filter?.member ?? entry?.members[0] ?? dim.roots[0];
        layout.filters.splice(index ?? layout.filters.length, 0, { dimension: name, member: chosen });
      } else {
        entry ??= { dimension: name, members: filter ? [filter.member] : [...dim.roots], expanded: [] };
        layout[to].splice(index ?? layout[to].length, 0, entry);
      }
      layout.sort = null;
    });
    this.drawFieldList();
  }

  /** Shows an element found in the field list: on its axis, opened down to it. */
  focusMember(dimName, name) {
    const dim = this.dims.get(dimName);
    const place = this.placeOf(dimName);
    this.change((layout) => {
      if (place === 'filters') {
        layout.filters.find((filter) => filter.dimension === dimName).member = name;
        return;
      }
      const entry = layout[place].find((candidate) => candidate.dimension === dimName);
      // Open every ancestor on the way from a chosen member down to it, or choose it.
      const ancestors = [];
      let parents = dim.byName.get(name).parents;
      while (parents.length) {
        ancestors.push(parents[0]);
        parents = dim.byName.get(parents[0]).parents;
      }
      if (ancestors.some((ancestor) => entry.members.includes(ancestor))) {
        for (const ancestor of ancestors) if (!entry.expanded.includes(ancestor)) entry.expanded.push(ancestor);
      } else if (!entry.members.includes(name)) {
        entry.members.push(name);
      }
    });
  }

  /* ---- Layout bar ---- */

  drawLayout() {
    const layout = this.layout;
    if (layout.mdx) {
      mount(this.layoutBar, h('div', { class: 'banner' }, icon('code', 18),
        h('span', { class: 'grow' }, 'This query was written in MDX, so the layout does not apply to it.'),
        h('button', { class: 'button small', onclick: () => this.change((l) => { l.mdx = null; }) }, 'Back to the layout')));
      return;
    }
    const zone = (place, label, iconName, entries) => {
      const chips = entries.map((entry, index) => this.chip(place, entry, index));
      const element = h('div', {
        class: 'zone',
        dataset: { place },
        ondragover: (event) => {
          if (!event.dataTransfer.types.includes('text/x-epiphany-dimension')) return;
          event.preventDefault();
          element.classList.add('over');
        },
        ondragleave: () => element.classList.remove('over'),
        ondrop: (event) => {
          event.preventDefault();
          element.classList.remove('over');
          const name = event.dataTransfer.getData('text/x-epiphany-dimension');
          const chip = event.target.closest('.chip');
          const index = chip ? Number(chip.dataset.index) : undefined;
          if (name) this.move(name, place, index);
        },
      },
      h('span', { class: 'zone-label' }, icon(iconName, 14), label),
      chips.length ? chips : h('span', { class: 'zone-empty' }, place === 'filters' ? 'Drop dimensions to filter by' : `Drop dimensions here`));
      return element;
    };
    mount(this.layoutBar,
      zone('filters', 'Filters', 'filter', layout.filters),
      zone('columns', 'Columns', 'columns', layout.columns),
      zone('rows', 'Rows', 'rows', layout.rows));
  }

  chip(place, entry, index) {
    const name = entry.dimension;
    const summary = place === 'filters'
      ? entry.member
      : entry.members.length === 1 ? entry.members[0] : `${entry.members.length} members`;
    return h('div', {
      class: 'chip',
      draggable: 'true',
      dataset: { index },
      ondragstart: (event) => {
        event.dataTransfer.setData('text/x-epiphany-dimension', name);
        this.page.classList.add('dragging');
      },
      ondragend: () => this.page.classList.remove('dragging'),
    },
    h('button', {
      class: 'chip-main',
      title: `Choose ${place === 'filters' ? 'the member' : 'members'} of ${name}`,
      onclick: () => this.pick(name),
      oncontextmenu: (event) => {
        event.preventDefault();
        this.placeMenu(name, { x: event.clientX, y: event.clientY });
      },
    }, h('strong', {}, name), h('span', { class: 'chip-summary' }, summary)),
    h('button', { class: 'chip-more', 'aria-label': `Place ${name}`, onclick: (event) => this.placeMenu(name, event.currentTarget) }, icon('down', 14)));
  }

  /* ---- Choosing members ---- */

  async pick(name) {
    const dim = this.dims.get(name);
    const place = this.placeOf(name);
    const single = place === 'filters';
    const entry = single ? null : this.layout[place].find((candidate) => candidate.dimension === name);
    const chosen = new Set(single ? [this.layout.filters.find((filter) => filter.dimension === name).member] : entry.members);
    const open = new Set(dim.roots);
    const search = h('input', { class: 'input', type: 'search', placeholder: `Find in ${name}`, 'aria-label': `Find in ${name}` });
    const tree = h('div', { class: 'picker-tree', role: 'tree', 'aria-multiselectable': String(!single) });
    const count = h('span', { class: 'muted' });
    const draw = () => {
      const words = search.value.trim().toLowerCase();
      const rows = [];
      const visit = (element, depth, seen) => {
        if (seen.has(element.name)) return;
        seen.add(element.name);
        const matches = !words || element.name.toLowerCase().includes(words);
        const children = element.children.map((child) => dim.byName.get(child.name)).filter(Boolean);
        if (matches) {
          const checked = chosen.has(element.name);
          rows.push(h('div', { class: 'picker-row', role: 'treeitem', 'aria-selected': String(checked), dataset: { depth } },
            children.length && !words
              ? h('button', {
                class: 'twisty',
                'aria-label': open.has(element.name) ? `Close ${element.name}` : `Open ${element.name}`,
                onclick: () => {
                  if (open.has(element.name)) open.delete(element.name);
                  else open.add(element.name);
                  draw();
                },
              }, icon(open.has(element.name) ? 'down' : 'chevron', 14))
              : h('span', { class: 'twisty' }),
            h('label', {},
              h('input', {
                type: single ? 'radio' : 'checkbox',
                name: 'member',
                checked,
                onchange: (event) => {
                  if (single) chosen.clear();
                  if (event.target.checked) chosen.add(element.name);
                  else chosen.delete(element.name);
                  if (single) draw();
                  count.textContent = `${chosen.size} chosen`;
                },
              }),
              icon(element.type === 'consolidated' ? 'sigma' : 'member', 14),
              element.name,
              element.type === 'consolidated' ? h('span', { class: 'muted' }, ` ${element.children.length}`) : null)));
        }
        if (words || open.has(element.name)) for (const child of children) visit(child, depth + 1, seen);
      };
      const seen = new Set();
      for (const root of dim.roots) visit(dim.byName.get(root), 0, seen);
      mount(tree, rows.length ? rows : h('p', { class: 'muted' }, 'Nothing matches.'));
      count.textContent = `${chosen.size} chosen`;
    };
    search.addEventListener('input', draw);
    const choose = (names) => {
      chosen.clear();
      for (const element of names) chosen.add(element);
      draw();
    };
    const byLevel = [...new Set(dim.elements.map((element) => element.level))].sort((a, b) => b - a);
    const quick = single ? null : h('div', { class: 'toolbar' },
      h('button', { class: 'button small', type: 'button', onclick: () => choose(dim.roots) }, 'Top'),
      h('button', { class: 'button small', type: 'button', onclick: () => choose(dim.elements.filter((element) => element.level === 0).map((element) => element.name)) }, 'All leaves'),
      byLevel.length > 2 ? h('select', {
        class: 'select',
        'aria-label': 'Choose a level',
        onchange: (event) => {
          if (event.target.value !== '') choose(dim.elements.filter((element) => element.level === Number(event.target.value)).map((element) => element.name));
          event.target.value = '';
        },
      }, h('option', { value: '' }, 'A level…'), byLevel.map((level) => h('option', { value: level }, dim.levels[level] ?? `Level ${level}`))) : null,
      Object.keys(dim.subsets ?? {}).length ? h('select', {
        class: 'select',
        'aria-label': 'Choose a subset',
        onchange: (event) => {
          if (event.target.value) choose(dim.subsets[event.target.value]);
          event.target.value = '';
        },
      }, h('option', { value: '' }, 'A subset…'), Object.keys(dim.subsets).map((subset) => h('option', { value: subset }, subset))) : null,
      h('button', { class: 'button small quiet', type: 'button', onclick: () => choose([]) }, 'Clear'),
      h('div', { class: 'grow' }),
      count);
    draw();
    await dialog({
      title: single ? `Filter by ${name}` : `Members of ${name}`,
      confirm: 'Apply',
      body: h('div', { class: 'picker' }, h('div', { class: 'search' }, icon('search', 16), search), quick, tree),
      onsubmit: () => {
        if (!chosen.size) throw new Error('Choose at least one member.');
        // In the dimension's own order, so that totals sit where they belong.
        const ordered = dim.elements.map((element) => element.name).filter((element) => chosen.has(element));
        this.change((layout) => {
          if (single) {
            layout.filters.find((filter) => filter.dimension === name).member = ordered[0];
          } else {
            const target = layout[place].find((candidate) => candidate.dimension === name);
            target.members = ordered;
            target.expanded = target.expanded.filter((element) => chosen.has(element));
          }
          layout.sort = null;
        });
        return true;
      },
    });
  }

  /* ---- Running the query ---- */

  async run() {
    const ticket = ++this.ticket;
    const text = mdx(this.layout, this.dims);
    this.text = text;
    if (!this.layout.mdx && (!this.layout.rows.length || !this.layout.columns.length)) {
      this.result = null;
      this.draw();
      return;
    }
    this.output.classList.add('busy');
    const started = performance.now();
    try {
      const result = await post('/mdx', { mdx: text, transaction: this.tx ?? undefined });
      if (ticket !== this.ticket) return;
      this.result = result;
      this.error = null;
      this.took = performance.now() - started;
      this.writable = await this.findWritable(result);
      if (ticket !== this.ticket) return;
    } catch (problem) {
      if (ticket !== this.ticket) return;
      this.result = null;
      this.error = problem;
    } finally {
      if (ticket === this.ticket) this.output.classList.remove('busy');
    }
    this.drawToolbar();
    this.draw();
  }

  draw() {
    if (this.view === 'mdx') return this.drawMdx();
    if (this.error) {
      mount(this.output, failure(this.error, () => this.run()));
      mount(this.statusLine);
      return;
    }
    if (!this.result) {
      const missing = !this.layout.rows.length ? 'rows' : 'columns';
      mount(this.output, empty('grid', `Put a dimension on ${missing}`, 'Drag one from the field list, or press Enter on it to choose where it goes.'));
      mount(this.statusLine);
      return;
    }
    this.drawGrid();
  }

  /* ---- The grid ---- */

  drawGrid() {
    const result = this.result;
    const [columns = { tuples: [[]] }, rows = { tuples: [[]] }] = result.axes;
    const columnTuples = columns.tuples.length ? columns.tuples : [];
    const rowTuples = rows.tuples.length ? rows.tuples : [];
    const columnDepth = columnTuples[0]?.length ?? 0;
    const rowDepth = rowTuples[0]?.length ?? 0;
    if (!columnTuples.length || (result.axes.length > 1 && !rowTuples.length)) {
      mount(this.output, empty('grid', 'Nothing to show', this.layout.nonEmpty ? 'Every cell is empty. Show empty rows to see them.' : 'The query chose no members.'));
      mount(this.statusLine);
      return;
    }
    const limit = 5000;
    const shownRows = rowTuples.slice(0, limit);
    const total = (tuple) => tuple.some((m) => m.type === 'consolidated');
    const table = h('table', { class: 'pivot', role: 'grid', 'aria-rowcount': rowTuples.length + columnDepth, 'aria-colcount': columnTuples.length + rowDepth });
    const head = h('thead');
    for (let level = 0; level < columnDepth; level += 1) {
      const tr = h('tr');
      if (level === 0) {
        tr.append(h('th', { class: 'corner', colspan: Math.max(rowDepth, 1), rowspan: columnDepth, scope: 'col' },
          rowTuples[0]?.map((m) => m.dimension).join(' · ') ?? ''));
      }
      // Neighbouring columns that share this member and those above it share one heading.
      for (let index = 0; index < columnTuples.length;) {
        const key = (i) => columnTuples[i].slice(0, level + 1).map((m) => m.unique_name).join('|');
        let span = 1;
        while (index + span < columnTuples.length && key(index + span) === key(index)) span += 1;
        const m = columnTuples[index][level];
        tr.append(h('th', { scope: 'col', colspan: span, class: `col-head ${m.type === 'consolidated' ? 'total' : ''}` },
          this.memberLabel('columns', m)));
        index += span;
      }
      head.append(tr);
    }
    const body = h('tbody');
    const columnCount = columnTuples.length;
    shownRows.forEach((tuple, r) => {
      const tr = h('tr', { class: total(tuple) ? 'total-row' : '' });
      tuple.forEach((m, level) => {
        // A member repeated from the row above is left blank, so groups read clearly.
        const same = r > 0 && shownRows[r - 1].slice(0, level + 1).every((other, i) => other.unique_name === tuple[i].unique_name);
        tr.append(h('th', { scope: 'row', class: `row-head ${m.type === 'consolidated' ? 'total' : ''}` }, same ? '' : this.memberLabel('rows', m)));
      });
      for (let c = 0; c < columnCount; c += 1) {
        const value = result.cells[r * columnCount + c];
        const totalCell = total(tuple) || total(columnTuples[c]);
        const number = typeof value === 'number';
        const editable = this.writable.has(`${r},${c}`);
        const edited = editable && this.edits.has(this.key(r, c));
        tr.append(h('td', {
          class: `${totalCell ? 'total' : ''} ${number ? 'n' : 's'} ${number && value < 0 ? 'negative' : ''} ${editable ? 'editable' : ''} ${edited ? 'edited' : ''}`,
          tabindex: r === 0 && c === 0 ? '0' : '-1',
          title: edited ? 'Changed, not yet saved' : undefined,
          dataset: { r, c },
        }, value === null || value === undefined ? '' : number ? formatNumber(value) : value));
      }
      body.append(tr);
    });
    table.append(head, body);
    table.addEventListener('contextmenu', (event) => {
      const cell = event.target.closest('td');
      if (!cell) return;
      event.preventDefault();
      this.cellMenu(Number(cell.dataset.r), Number(cell.dataset.c), { x: event.clientX, y: event.clientY });
    });
    table.addEventListener('keydown', (event) => this.moveFocus(event, table));
    table.addEventListener('dblclick', (event) => {
      const cell = event.target.closest('td.editable');
      if (cell) this.edit(cell);
    });
    const scroller = h('div', { class: 'pivot-scroll' }, table);
    mount(this.output, scroller);
    this.focusCell();
    const cells = rowTuples.length * columnCount;
    mount(this.statusLine,
      h('span', {}, `${rowTuples.length.toLocaleString()} rows × ${columnCount.toLocaleString()} columns`),
      h('span', { class: 'muted' }, `${cells.toLocaleString()} cells in ${Math.round(this.took)} ms`),
      rowTuples.length > limit ? h('span', { class: 'badge warn' }, `Showing the first ${limit.toLocaleString()} rows`) : null,
      this.layout.sort ? h('span', { class: 'badge accent' }, `Sorted ${this.layout.sort.descending ? 'largest' : 'smallest'} first`) : null);
  }

  /** A member's heading: with a button that opens or closes it where it has children. */
  memberLabel(place, m) {
    const entry = this.layout[place]?.find((candidate) => candidate.dimension === m.dimension);
    const dim = this.dims.get(m.dimension);
    const element = dim?.byName.get(m.name);
    const depth = entry ? this.depthOf(entry, dim, m.name) : 0;
    const label = h('span', { class: 'member-label', dataset: { depth } });
    if (entry && !this.layout.mdx && element?.children.length) {
      const opened = entry.expanded.includes(m.name);
      label.append(h('button', {
        class: 'twisty',
        'aria-expanded': String(opened),
        'aria-label': `${opened ? 'Close' : 'Open'} ${m.name}`,
        onclick: () => this.toggle(place, m.dimension, m.name),
      }, icon(opened ? 'minus' : 'plus', 12)));
    } else if (depth) {
      label.append(h('span', { class: 'twisty' }));
    }
    label.append(h('span', {}, m.name));
    return label;
  }

  depthOf(entry, dim, name) {
    let depth = 0;
    let current = dim.byName.get(name);
    while (current && !entry.members.includes(current.name) && current.parents.length) {
      const parent = current.parents.find((candidate) => entry.expanded.includes(candidate));
      if (!parent) break;
      depth += 1;
      current = dim.byName.get(parent);
    }
    return Math.min(depth, 8);
  }

  toggle(place, dimName, name) {
    this.change((layout) => {
      const entry = layout[place].find((candidate) => candidate.dimension === dimName);
      entry.expanded = entry.expanded.includes(name)
        ? entry.expanded.filter((candidate) => candidate !== name)
        : [...entry.expanded, name];
    });
  }

  moveFocus(event, table) {
    const cell = event.target.closest('td');
    if (!cell) return;
    const r = Number(cell.dataset.r);
    const c = Number(cell.dataset.c);
    const moves = { ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
    if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
      event.preventDefault();
      this.cellMenu(r, c, cell);
      return;
    }
    if (cell.classList.contains('editable') && !event.ctrlKey && !event.metaKey && !event.altKey) {
      if (event.key === 'Enter' || event.key === 'F2') {
        event.preventDefault();
        this.edit(cell);
        return;
      }
      if (event.key === 'Delete' || event.key === 'Backspace') {
        event.preventDefault();
        this.write(r, c, this.result.cells[r * this.result.axes[0].tuples.length + c], '');
        return;
      }
      if (event.key.length === 1) {
        event.preventDefault();
        this.edit(cell, event.key);
        return;
      }
    }
    const move = moves[event.key];
    if (!move) return;
    const next = table.querySelector(`td[data-r="${r + move[0]}"][data-c="${c + move[1]}"]`);
    if (!next) return;
    event.preventDefault();
    cell.tabIndex = -1;
    next.tabIndex = 0;
    next.focus();
  }

  cellMenu(r, c, at) {
    const [columns, rows] = this.result.axes;
    const rowTuple = rows?.tuples[r] ?? [];
    const columnTuple = columns.tuples[c] ?? [];
    const written = Boolean(this.layout.mdx);
    const openable = (place, tuple) => tuple.filter((m) => this.dims.get(m.dimension)?.byName.get(m.name)?.children.length);
    const rowOpen = written ? [] : openable('rows', rowTuple);
    menu([
      { heading: [...rowTuple, ...columnTuple].map((m) => m.name).join(' · ') },
      { label: 'Sort rows by this column, largest first', icon: 'down', disabled: written || !this.layout.rows.length, action: () => this.sortBy(columnTuple, true) },
      { label: 'Sort rows by this column, smallest first', icon: 'chevron', disabled: written || !this.layout.rows.length, action: () => this.sortBy(columnTuple, false) },
      '-',
      ...rowOpen.map((m) => {
        const entry = this.layout.rows.find((candidate) => candidate.dimension === m.dimension);
        const opened = entry?.expanded.includes(m.name);
        return { label: `${opened ? 'Close' : 'Open'} ${m.name}`, icon: opened ? 'minus' : 'plus', action: () => this.toggle('rows', m.dimension, m.name) };
      }),
      this.writable.has(`${r},${c}`)
        ? { label: 'Change the value', icon: 'edit', action: () => this.edit(this.output.querySelector(`td[data-r="${r}"][data-c="${c}"]`)) }
        : null,
      this.mayWrite() && this.coordinate(r, c) && [...rowTuple, ...columnTuple].some((m) => m.type === 'consolidated')
        ? { label: 'Spread a value over its leaves…', icon: 'sigma', action: () => this.spread(r, c) }
        : null,
      { label: 'Explain this value', icon: 'info', action: () => this.explain(rowTuple, columnTuple) },
      { label: 'Copy the value', icon: 'save', action: () => navigator.clipboard?.writeText(String(this.result.cells[r * columns.tuples.length + c] ?? '')) },
    ], at);
  }

  /* ---- Writing back ---- */

  /** A grid cell's place in the cube, by element name; none where a member is calculated. */
  coordinate(r, c) {
    const [columns, rows] = this.result.axes;
    const members = [...(rows?.tuples[r] ?? []), ...(columns?.tuples[c] ?? []), ...(this.result.slicer ?? [])];
    if (members.some((m) => m.type === 'calculated')) return null;
    const at = new Map();
    // A dimension neither on an axis nor in the slicer stands at its first top element.
    for (const m of members) if (!at.has(m.dimension)) at.set(m.dimension, m.name);
    return this.cube.dimensions.map((dim) => at.get(dim) ?? this.dims.get(dim).roots[0]);
  }

  key(r, c) {
    return JSON.stringify(this.coordinate(r, c));
  }

  /** The cells of a result that may be written, as the server says: "row,column". */
  async findWritable(result) {
    const found = new Set();
    if (!this.mayWrite()) return found;
    const [columns, rows] = result.axes;
    const columnCount = columns?.tuples.length ?? 0;
    const rowCount = rows ? Math.min(rows.tuples.length, 5000) : 1;
    const leaf = (m) => m.type === 'numeric' || m.type === 'string';
    if (!(result.slicer ?? []).every(leaf)) return found;
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
    if (!asked.length) return found;
    try {
      const answer = await post('/cells/explain', { cells: asked, transaction: this.tx ?? undefined });
      answer.cells.forEach((cell, index) => {
        if (cell.cell?.writable) found.add(places[index]);
      });
    } catch {
      // Without an answer nothing is offered for writing; the grid still shows.
    }
    return found;
  }

  /** Opens the editor in a cell, with what was typed into it or its current value. */
  edit(cell, typed) {
    if (!cell || cell.querySelector('input')) return;
    const r = Number(cell.dataset.r);
    const c = Number(cell.dataset.c);
    const before = this.result.cells[r * this.result.axes[0].tuples.length + c];
    const input = h('input', {
      class: 'cell-editor',
      'aria-label': 'New value',
      inputmode: typeof before === 'string' ? 'text' : 'decimal',
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
      if (save) this.write(r, c, before, input.value, move);
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
  }

  /** Writes a value typed into a cell, within the transaction the edits are held in. */
  async write(r, c, before, text, move) {
    const at = this.coordinate(r, c);
    let value;
    if (typeof before === 'string') {
      value = text;
    } else {
      const cleaned = String(text).trim().replace(/[\s,]/g, '');
      value = cleaned === '' ? 0 : Number(cleaned);
      if (!Number.isFinite(value)) {
        notify(`“${text}” is not a number.`, 'bad');
        return;
      }
    }
    this.focusAfter = [r + (move?.[0] ?? 0), c + (move?.[1] ?? 0)];
    if (value === before || ((before === null || before === undefined) && (value === 0 || value === ''))) {
      this.focusCell();
      return;
    }
    try {
      await this.ensureTransaction();
      await post('/cells/write', { transaction: this.tx, cells: [{ cube: this.cube.name, at, value }] });
    } catch (problem) {
      this.refused(problem);
      return;
    }
    this.edits.set(JSON.stringify(at), value);
    this.changes = (this.changes ?? 0) + 1;
    await this.run();
  }

  /** Shares a value out among the leaves beneath a total. */
  async spread(r, c) {
    const at = this.coordinate(r, c);
    const before = this.result.cells[r * this.result.axes[0].tuples.length + c];
    const amount = h('input', { class: 'input', id: 'spread-value', inputmode: 'decimal', required: true });
    amount.value = typeof before === 'number' ? String(before) : '';
    const method = h('select', { class: 'select', id: 'spread-method' },
      h('option', { value: 'proportional' }, 'In proportion to what each holds now'),
      h('option', { value: 'equal' }, 'Equally among them'),
      h('option', { value: 'repeat' }, 'The whole value in each'));
    const written = await dialog({
      title: 'Spread a value',
      confirm: 'Spread',
      body: h('div', { class: 'facts' },
        h('p', { class: 'muted' }, `Over the leaves of ${at.join(' · ')}.`),
        h('div', { class: 'field' }, h('label', { for: 'spread-value' }, 'Value'), amount),
        h('div', { class: 'field' }, h('label', { for: 'spread-method' }, 'How'), method),
        h('p', { class: 'hint' }, 'Nothing is saved until you save your changes.')),
      onsubmit: async () => {
        const value = Number(amount.value.trim().replace(/[\s,]/g, ''));
        if (!Number.isFinite(value)) throw new Error('Enter a number.');
        await this.ensureTransaction();
        const answer = await post('/cells/spread', {
          transaction: this.tx,
          spread: { cube: this.cube.name, at, value, method: method.value },
        });
        return answer.written;
      },
    });
    if (written === null) return;
    notify(`Spread over ${written.toLocaleString()} cell${written === 1 ? '' : 's'}.`, 'good');
    this.changes = (this.changes ?? 0) + 1;
    await this.run();
  }

  async ensureTransaction() {
    if (this.tx) return;
    const answer = await post('/transactions');
    this.tx = answer.transaction;
  }

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
  }

  async discard() {
    try {
      await post(`/transactions/${this.tx}/rollback`);
    } catch {
      // Gone already: discarded either way.
    }
    this.forget();
    await this.run();
  }

  forget() {
    this.tx = null;
    this.edits.clear();
    this.changes = 0;
  }

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
  }

  focusCell() {
    if (!this.focusAfter) return;
    const [r, c] = this.focusAfter;
    const cell = this.output.querySelector(`td[data-r="${r}"][data-c="${c}"]`);
    this.focusAfter = null;
    if (!cell) return;
    for (const other of this.output.querySelectorAll('td[tabindex="0"]')) other.tabIndex = -1;
    cell.tabIndex = 0;
    cell.focus();
  }

  sortBy(columnTuple, descending) {
    this.change((layout) => {
      layout.sort = { tuple: columnTuple.map((m) => [m.dimension, m.name]), descending };
    });
  }

  /** Says how a cell comes by its value, for a cell named by its row and column members. */
  async explain(rowTuple, columnTuple) {
    const at = new Map();
    for (const filter of this.layout.filters) at.set(filter.dimension, filter.member);
    for (const m of this.result.slicer ?? []) at.set(m.dimension, m.name);
    for (const m of [...rowTuple, ...columnTuple]) at.set(m.dimension, m.name);
    if ([...rowTuple, ...columnTuple].some((m) => m.type === 'calculated')) {
      notify('A calculated member is worked out by the query alone, so it has no explanation stored with the cube.');
      return;
    }
    const cell = { cube: this.cube.name, at: this.cube.dimensions.map((dim) => at.get(dim) ?? this.dims.get(dim).roots[0]) };
    let explanation;
    try {
      const answer = await post('/cells/explain', { cells: [cell] });
      explanation = answer.cells[0];
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    if (explanation.error) {
      notify(explanation.error.message, 'bad');
      return;
    }
    const facts = explanation.cell;
    const how = {
      leaf: facts.rule ? 'Worked out by a rule' : 'Stored in the cube',
      consolidated: facts.rule ? 'Worked out by a rule' : 'Added up from the leaves beneath it',
      text: 'Text stored in the cube',
    }[facts.kind];
    const value = typeof facts.value === 'number' ? formatNumber(facts.value, 6) : facts.value;
    const row = (label, content) => h('div', { class: 'fact' }, h('span', { class: 'muted' }, label), h('span', {}, content));
    await dialog({
      title: 'How this value comes about',
      confirm: 'Close',
      cancel: null,
      body: h('div', { class: 'facts' },
        row('Cell', cell.at.join(' · ')),
        row('Value', h('strong', { class: 'num' }, value === '' ? '(empty)' : value)),
        row('How', how),
        facts.rule ? row('Rule', h('code', { class: 'mono' }, facts.rule.text ?? JSON.stringify(facts.rule))) : null,
        facts.stored !== null && facts.stored !== undefined && facts.rule ? row('Stored beneath it', String(facts.stored)) : null,
        row('Can be written', facts.writable ? 'Yes' : 'No'),
        facts.fed === null || facts.fed === undefined ? null : row('Fed', facts.fed ? 'Yes' : 'No: consolidations leave it out')),
    });
  }
  /* ---- MDX ---- */

  drawMdx() {
    const editor = h('textarea', { class: 'textarea mdx-editor', spellcheck: 'false', 'aria-label': 'MDX query', rows: 14 }, this.text ?? '');
    editor.value = this.text ?? '';
    const run = () => {
      const text = editor.value.trim();
      if (!text) return;
      this.change((layout) => {
        layout.mdx = text;
      });
      this.view = 'grid';
      this.drawTabs();
    };
    editor.addEventListener('keydown', (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
        event.preventDefault();
        run();
      }
    });
    mount(this.output, h('div', { class: 'mdx-pane' },
      h('p', { class: 'muted' }, this.layout.mdx ? 'This query was written by hand.' : 'The query the layout makes. Edit it and run it to go further than the layout can.'),
      editor,
      h('div', { class: 'toolbar' },
        h('button', { class: 'button primary', onclick: run }, icon('play', 16), 'Run'),
        h('span', { class: 'hint' }, 'Ctrl Enter runs it'),
        h('div', { class: 'grow' }),
        h('button', {
          class: 'button',
          onclick: async () => {
            await navigator.clipboard?.writeText(editor.value);
            notify('The query is copied.', 'good');
          },
        }, 'Copy'))));
    mount(this.statusLine);
  }

  /* ---- Export ---- */

  exportCsv() {
    const result = this.result;
    const [columns, rows] = result.axes;
    const field = (value) => {
      const text = value === null || value === undefined ? '' : String(value);
      return /[",\n\r]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
    };
    const rowTuples = rows?.tuples ?? [[]];
    const lines = [];
    const depth = rowTuples[0]?.length ?? 0;
    for (let level = 0; level < (columns.tuples[0]?.length ?? 0); level += 1) {
      lines.push([...Array(depth).fill(''), ...columns.tuples.map((tuple) => tuple[level].name)].map(field).join(','));
    }
    rowTuples.forEach((tuple, r) => {
      const values = columns.tuples.map((_, c) => result.cells[r * columns.tuples.length + c]);
      lines.push([...tuple.map((m) => m.name), ...values].map(field).join(','));
    });
    download(`${this.cube.name}.csv`, `${lines.join('\r\n')}\r\n`, 'text/csv');
  }
}

/* An unhandled refusal is reported, not lost. */
export { ApiError };
