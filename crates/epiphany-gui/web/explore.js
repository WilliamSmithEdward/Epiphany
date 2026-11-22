// Exploring a cube: a field list, a layout of rows, columns and filters, and the pivot grid
// of the query that layout makes.

import { ApiError, get, modelVersion, part, post, put } from './api.js';
import { chart, shape } from './chart.js';
import { cellText, Selection } from './selection.js';
import { beneath, marked, openButtons, reveal, ROW_LIMIT, searchBox, treeKey, treeRows } from './tree.js';
import { editSet } from './sets.js';
import { empty, failure, go, model, state } from './app.js';
import { dialog, download, formatNumber, h, icon, menu, mount, notify, parseNumber } from './ui.js';

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

/** Puts text on the clipboard, and says so. */
function copied(text) {
  navigator.clipboard?.writeText(text).then(
    () => notify('Copied.', 'good'),
    () => notify('The browser kept the page from the clipboard. Press Ctrl C instead.', 'bad'),
  );
}

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
  return new Explorer(target, definition, cube, dims, cube.views?.[params.get('view')]);
}

class Explorer {
  /** `view`, where one is given, is a saved view to open in place of the last layout. */
  constructor(target, definition, cube, dims, view) {
    this.definition = definition;
    this.cube = cube;
    this.dims = dims;
    this.layout = this.restore() ?? defaultLayout(cube, dims);
    if (view) Object.assign(this.layout, { mdx: view.mdx, geometry: view.geometry ?? null });
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

  /**
   * Changes the layout, keeping the old one to go back to, and runs the query again. A
   * change to the layout replaces a query written in MDX with the layout's own; `written`
   * says the change is to the written query itself.
   */
  change(edit, written = false) {
    this.past.push(JSON.stringify(this.layout));
    if (this.past.length > 100) this.past.shift();
    this.future = [];
    edit(this.layout);
    if (!written) this.layout.mdx = null;
    this.after();
  }

  /**
   * Reads a result back into the layout: the dimensions on each axis with the members it
   * shows, and the slicer's members as filters. Where the layout makes the very same
   * query, it takes the written one's place; where it cannot, a calculated member or a
   * filtered or reordered set, the written query stays, and the layout shows its parts.
   */
  adopt(result) {
    const [columns, rows] = result.axes;
    const previous = new Map([...this.layout.rows, ...this.layout.columns].map((entry) => [entry.dimension, entry]));
    let exact = true;
    const axis = (tuples) => {
      if (!tuples?.length) return [];
      const width = tuples[0].length;
      const entries = Array.from({ length: width }, (_, position) => {
        const dimension = tuples[0][position].dimension;
        const members = [];
        for (const tuple of tuples) {
          const m = tuple[position];
          if (m.type === 'calculated' || !this.dims.get(dimension)?.byName.has(m.name)) exact = false;
          else if (!members.includes(m.name)) members.push(m.name);
        }
        const before = previous.get(dimension);
        return { dimension, members, expanded: [], attributes: before?.attributes ?? [], alias: before?.alias ?? null };
      });
      // The layout crosses its members in full, the first dimension turning slowest.
      const product = entries.reduce((count, entry) => count * entry.members.length, 1);
      if (product !== tuples.length) exact = false;
      else {
        tuples.forEach((tuple, index) => {
          let rest = index;
          for (let position = width - 1; position >= 0; position -= 1) {
            const members = entries[position].members;
            if (tuple[position].name !== members[rest % members.length]) exact = false;
            rest = Math.floor(rest / members.length);
          }
        });
      }
      return entries.filter((entry) => entry.members.length);
    };
    const placed = {
      columns: axis(columns?.tuples),
      rows: axis(rows?.tuples),
    };
    if (!placed.columns.length || result.axes.length < 2) exact = false;
    const filters = (result.slicer ?? [])
      .filter((m) => m.type !== 'calculated' && this.dims.get(m.dimension)?.byName.has(m.name))
      .map((m) => ({ dimension: m.dimension, member: m.name }));
    if ((result.slicer ?? []).some((m) => m.type === 'calculated')) exact = false;
    const text = this.layout.mdx;
    Object.assign(this.layout, {
      columns: placed.columns,
      rows: placed.rows,
      filters,
      sort: null,
      nonEmpty: /\bNON\s+EMPTY\b/i.test(text),
      mdx: exact ? null : text,
    });
    localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
    this.drawLayout();
    this.drawFieldList?.();
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
    const viewButton = h('button', {
      class: 'button',
      'aria-haspopup': 'menu',
      onclick: (event) => {
        const views = Object.keys(this.cube.views ?? {});
        const admin = state.user?.role === 'admin';
        menu([
          { heading: views.length ? 'Saved views' : 'No saved views yet' },
          ...views.map((name) => ({
            label: name,
            icon: 'grid',
            action: () => this.change((layout) => {
              layout.mdx = this.cube.views[name].mdx;
              layout.geometry = this.cube.views[name].geometry ?? null;
            }, true),
          })),
          admin ? '-' : null,
          admin ? { label: 'Save this query as a view…', icon: 'save', disabled: !this.text, action: () => this.saveView() } : null,
        ], event.currentTarget);
      },
    }, icon('grid', 16), 'Views', icon('down', 14));
    this.tabs = h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'Show as' });
    this.finder = this.buildFinder();
    this.page = h('div', { class: 'explorer' },
      this.fields,
      h('section', { class: 'work' },
        h('div', { class: 'work-head' },
          h('div', { class: 'toolbar' }, icon('cube', 20), cubes, viewButton, this.finder.box, h('div', { class: 'grow' }), this.tabs),
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
      if ((event.ctrlKey || event.metaKey) && event.key === 'f' && this.output.contains(event.target)) {
        // Within the grid, Ctrl F finds among its headings.
        event.preventDefault();
        this.finder.input.focus();
        this.finder.input.select();
      } else if ((event.ctrlKey || event.metaKey) && event.key === 'z') {
        event.preventDefault();
        if (event.shiftKey) this.redo();
        else this.undo();
      } else if ((event.ctrlKey || event.metaKey) && event.key === 'y') {
        event.preventDefault();
        this.redo();
      }
    });
  }

  /**
   * A box that finds members among the grid's headings: Enter goes to the next, Shift Enter
   * to the one before, and Escape clears it. A member not shown, beneath one closed, is
   * opened to by Enter.
   */
  buildFinder() {
    const input = h('input', { class: 'input', type: 'search', placeholder: 'Find in the grid', 'aria-label': 'Find a member in the grid', autocomplete: 'off', spellcheck: 'false' });
    const count = h('span', { class: 'search-count', 'aria-live': 'polite' });
    this.findWords = '';
    this.findAt = -1;
    input.addEventListener('input', () => {
      this.findWords = input.value.trim().toLowerCase();
      this.findAt = -1;
      this.markFound();
    });
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        this.findNext(event.shiftKey ? -1 : 1);
      } else if (event.key === 'Escape') {
        event.preventDefault();
        if (input.value) {
          input.value = '';
          this.findWords = '';
          this.markFound();
        } else {
          this.output.querySelector('td[tabindex="0"]')?.focus();
        }
      }
    });
    return { box: h('div', { class: 'search finder' }, icon('search', 16), input, count), input, count };
  }

  /** Marks the headings that hold the words found, and says how many there are. */
  markFound() {
    const words = this.findWords;
    const headings = [...this.output.querySelectorAll('.pivot th[data-member]')];
    this.found = [];
    for (const heading of headings) {
      const hit = Boolean(words) && (heading.textContent.toLowerCase().includes(words) || heading.dataset.member.toLowerCase().includes(words));
      heading.classList.toggle('found', hit);
      heading.classList.remove('found-current');
      if (hit) this.found.push(heading);
    }
    this.unseen = words && !this.found.length ? this.hiddenMatch(words) : null;
    const at = this.found[this.findAt];
    at?.classList.add('found-current');
    this.finder.count.textContent = !words ? ''
      : this.found.length ? (at ? `${this.findAt + 1} of ${this.found.length}` : `${this.found.length} found`)
        : this.unseen ? 'Enter shows it' : 'None found';
  }

  /** A member of a dimension on an axis, not shown, whose name holds the words. */
  hiddenMatch(words) {
    for (const entry of [...this.layout.rows, ...this.layout.columns]) {
      const element = this.dims.get(entry.dimension).elements.find((candidate) => candidate.name.toLowerCase().includes(words));
      if (element) return { dimension: entry.dimension, name: element.name };
    }
    return null;
  }

  /** Goes to the next heading found, or the one before; opens the grid to one not shown. */
  findNext(step) {
    if (!this.found?.length) {
      if (this.unseen) {
        this.findAt = 0;
        this.focusMember(this.unseen.dimension, this.unseen.name);
      }
      return;
    }
    this.findAt = (this.findAt + step + this.found.length) % this.found.length;
    this.markFound();
    const heading = this.found[this.findAt];
    heading.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    // The row or column found is chosen, so that its cells stand out too.
    this.selection?.choose(this.selection.at(heading));
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
    mount(this.tabs, tab('grid', 'Grid'), tab('chart', 'Chart'), tab('mdx', 'MDX'));
  }

  drawToolbar() {
    this.drawPending();
    const layout = this.layout;
    const button = (label, iconName, action, props = {}) => h('button', { class: 'button small quiet', onclick: action, ...props }, icon(iconName, 16), h('span', {}, label));
    mount(this.toolbar,
      button('Undo', 'undo', () => this.undo(), { disabled: !this.past.length, title: 'Undo (Ctrl Z)' }),
      button('Redo', 'redo', () => this.redo(), { disabled: !this.future.length, title: 'Redo (Ctrl Y)' }),
      h('span', { class: 'divider' }),
      button('Swap axes', 'swap', () => this.change((l) => {
        [l.rows, l.columns] = [l.columns, l.rows];
        l.sort = null;
      })),
      button(layout.nonEmpty ? 'Showing non-empty' : 'Showing empty rows', layout.nonEmpty ? 'eyeOff' : 'eye',
        () => this.change((l) => {
          l.nonEmpty = !l.nonEmpty;
        }), { 'aria-pressed': String(layout.nonEmpty), title: 'Leave out rows and columns with no values' }),
      layout.sort ? button('Clear sort', 'close', () => this.change((l) => {
        l.sort = null;
      })) : null,
      h('div', { class: 'grow' }),
      layout.mdx ? h('span', {
        class: 'badge accent',
        title: 'The query is as written in the MDX tab. The layout shows its parts; changing the layout replaces it with the layout\'s own query.',
      }, icon('code', 12), 'Written in MDX') : null,
      button('Refresh', 'refresh', () => this.run()),
      button('Export CSV', 'download', () => this.exportCsv(), { disabled: !this.result }),
      button('Reset', 'down', (event) => menu([
        {
          label: 'Reset sizes',
          icon: 'columns',
          disabled: !layout.geometry,
          action: () => {
            this.layout.geometry = null;
            localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
            this.drawToolbar();
            this.draw();
          },
        },
        { label: 'Reset layout', icon: 'trash', action: () => this.change((l) => Object.assign(l, defaultLayout(this.cube, this.dims))) },
      ], event.currentTarget), { 'aria-haspopup': 'menu', title: 'Fit the sizes to the content again, or start the layout over' }));
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
          h('span', { class: `badge ${place === 'rows' || place === 'columns' ? 'accent' : place === 'unused' ? 'unused' : ''}` }, { rows: 'Rows', columns: 'Columns', filters: 'Filter', unused: 'Not used' }[place]),
          h('button', { class: 'button quiet small icon', 'aria-label': `Place ${name}`, onclick: (event) => this.placeMenu(name, event.currentTarget) }, icon('more', 16))),
          matches.length ? h('ul', { class: 'matches' }, matches.map((element) => h('li', {},
            h('button', {
              class: 'match',
              onclick: () => this.focusMember(name, element.name),
            }, icon(element.type === 'consolidated' ? 'sigma' : 'member', 14), element.name)))) : null);
      }));
    };
    search.addEventListener('input', draw);
    // A chip dropped back on the field list leaves the query.
    this.fields.ondragover = (event) => {
      if (!event.dataTransfer.types.includes('text/x-epiphany-dimension')) return;
      event.preventDefault();
      this.fields.classList.add('over');
    };
    this.fields.ondragleave = (event) => {
      if (!this.fields.contains(event.relatedTarget)) this.fields.classList.remove('over');
    };
    this.fields.ondrop = (event) => {
      event.preventDefault();
      this.fields.classList.remove('over');
      const name = event.dataTransfer.getData('text/x-epiphany-dimension');
      if (name && this.placeOf(name) !== 'unused') this.move(name, 'unused');
    };
    mount(this.fields,
      h('div', { class: 'fields-head' }, h('h3', {}, 'Fields'), h('span', { class: 'muted' }, `${this.cube.dimensions.length} dimensions`)),
      h('div', { class: 'search' }, icon('search', 16), search),
      list,
      h('p', { class: 'hint fields-hint' }, 'Drag a dimension to Rows, Columns or Filters, or press Enter on it to choose where it goes. Drag one back here to leave it out of the query: it then stands at its first top element.'));
    this.drawFieldList = draw;
    draw();
  }

  placeOf(name) {
    if (this.layout.rows.some((entry) => entry.dimension === name)) return 'rows';
    if (this.layout.columns.some((entry) => entry.dimension === name)) return 'columns';
    if (this.layout.filters.some((filter) => filter.dimension === name)) return 'filters';
    return 'unused';
  }

  placeMenu(name, at) {
    const place = this.placeOf(name);
    const onAxis = place === 'rows' || place === 'columns';
    menu([
      { heading: name },
      ...(onAxis ? [...this.openItems(place, name), '-'] : []),
      { label: 'Put on rows', icon: 'rows', disabled: place === 'rows', action: () => this.move(name, 'rows') },
      { label: 'Put on columns', icon: 'columns', disabled: place === 'columns', action: () => this.move(name, 'columns') },
      { label: 'Use as a filter', icon: 'filter', disabled: place === 'filters', action: () => this.move(name, 'filters') },
      { label: 'Leave out of the query', icon: 'eyeOff', disabled: place === 'unused', action: () => this.move(name, 'unused') },
      {
        label: 'Attributes and aliases…',
        icon: 'eye',
        disabled: (place !== 'rows' && place !== 'columns') || !Object.keys(this.dims.get(name).attributes ?? {}).length,
        action: () => this.chooseAttributes(name),
      },
      '-',
      { label: 'Choose members…', icon: 'check', disabled: place === 'unused', action: () => this.pick(name) },
    ], at);
  }

  /**
   * The ways to open and close a dimension's members on an axis: a level more than shows
   * now, every level, none, or down to a named level.
   */
  openItems(place, dimension) {
    const dim = this.dims.get(dimension);
    const entry = this.entryOf(place, dimension);
    const children = (name) => this.childrenOf(dim, name);
    const within = [...new Set([...entry.members, ...beneath(entry.members, children)])].filter((name) => children(name).length);
    const closed = shown(entry, dim).map(({ name }) => name).filter((name) => children(name).length && !entry.expanded.includes(name));
    const levels = [...new Set(within.map((name) => dim.byName.get(name).level))].sort((a, b) => b - a);
    return [
      { label: 'Open one level more', icon: 'plus', disabled: !closed.length, action: () => this.setOpen(place, dimension, (expanded) => [...expanded, ...closed]) },
      { label: 'Open every level', icon: 'expand', disabled: within.every((name) => entry.expanded.includes(name)), action: () => this.setOpen(place, dimension, () => within) },
      ...levels.slice(1).map((level) => ({
        label: `Open down to ${dim.levels?.[level] || `level ${level}`}`,
        icon: 'level',
        action: () => this.setOpen(place, dimension, () => within.filter((name) => dim.byName.get(name).level > level)),
      })),
      { label: 'Close every level', icon: 'collapse', disabled: !entry.expanded.length, action: () => this.setOpen(place, dimension, () => []) },
    ];
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
      if (to === 'unused') {
        // Out of the query: the server takes the dimension at its first top element.
      } else if (to === 'filters') {
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
      if (place === 'unused') {
        layout.filters.push({ dimension: dimName, member: name });
        return;
      }
      const entry = layout[place].find((candidate) => candidate.dimension === dimName);
      if (entry.members.includes(name)) return;
      // Open the members on the way down to it from the nearest one chosen, or choose it.
      const path = this.pathFrom(dim, entry.members, name);
      if (path) entry.expanded = [...new Set([...entry.expanded, ...path])];
      else entry.members.push(name);
    });
  }

  /**
   * The members between the nearest of `chosen` above an element and the element, that one
   * first: those to open to show it. Null when none of `chosen` is above it.
   */
  pathFrom(dim, chosen, name) {
    const from = new Map([[name, null]]);
    const queue = [name];
    while (queue.length) {
      const current = queue.shift();
      for (const parent of dim.byName.get(current)?.parents ?? []) {
        if (from.has(parent)) continue;
        from.set(parent, current);
        if (chosen.includes(parent)) {
          const path = [];
          for (let step = parent; step !== name; step = from.get(step)) path.push(step);
          return path;
        }
        queue.push(parent);
      }
    }
    return null;
  }

  /* ---- Layout bar ---- */

  drawLayout() {
    const layout = this.layout;

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
    if (place !== 'filters') {
      // A dimension on an axis gets the set editor, and keeps the order built there.
      const entry = this.layout[place].find((candidate) => candidate.dimension === name);
      const set = await editSet({ ...dim, name }, entry.members);
      if (!set) return;
      this.change((layout) => {
        const target = layout[place].find((candidate) => candidate.dimension === name);
        target.members = set;
        target.expanded = target.expanded.filter((element) => set.includes(element));
        layout.sort = null;
      });
      return;
    }
    // A filter stands at one member, chosen from the dimension's tree.
    let chosen = this.layout.filters.find((filter) => filter.dimension === name).member;
    const children = (element) => (dim.byName.get(element)?.children ?? []).map((child) => child.name).filter((child) => dim.byName.has(child));
    const roots = dim.roots;
    let open = reveal({ roots, children, matches: new Set([chosen]) });
    let found = null;
    let words = '';
    const tree = h('div', { class: 'picker-tree', role: 'tree', 'aria-label': `Members of ${name}` });
    // Choosing a member and applying it at once: the dialog's form is the tree's.
    const apply = () => tree.closest('form')?.requestSubmit();
    const draw = (focus) => {
      const matches = words ? new Set(dim.elements.filter((element) => element.name.toLowerCase().includes(words)).map((element) => element.name)) : null;
      const { rows, cut } = treeRows({ roots, children, open: found ?? open, matches });
      const current = rows.find((row) => row.name === (focus ?? chosen)) ?? rows.find((row) => row.match) ?? rows[0];
      const choose = (element) => {
        chosen = element;
        draw(element);
      };
      const drawn = rows.map((row) => {
        const element = dim.byName.get(row.name);
        return h('div', {
          class: `picker-row${row.context ? ' context' : ''}`,
          role: 'treeitem',
          'aria-level': row.depth + 1,
          'aria-selected': String(row.name === chosen),
          'aria-expanded': row.kids ? String(row.open) : null,
          tabindex: row === current ? '0' : '-1',
          dataset: { depth: Math.min(row.depth, 8), name: row.name },
          onclick: () => choose(row.name),
          ondblclick: () => {
            chosen = row.name;
            apply();
          },
          onkeydown: (event) => {
            if (event.key === ' ') {
              event.preventDefault();
              choose(row.name);
              return;
            }
            // One member is chosen, so the choice follows the keys.
            treeKey(event, {
              go: (target) => choose(target),
              open: (names, opened) => {
                const into = found ?? open;
                for (const each of names) {
                  if (opened) into.add(each);
                  else into.delete(each);
                }
                draw(row.name);
              },
              act: (target) => {
                chosen = target;
                apply();
              },
            });
          },
        },
        row.kids
          ? h('button', {
            class: 'twisty',
            type: 'button',
            tabindex: '-1',
            'aria-label': row.open ? `Close ${row.name}` : `Open ${row.name}`,
            onclick: (event) => {
              event.stopPropagation();
              const into = found ?? open;
              if (row.open) into.delete(row.name);
              else into.add(row.name);
              draw(row.name);
            },
          }, icon(row.open ? 'down' : 'chevron', 14))
          : h('span', { class: 'twisty' }),
        h('span', { class: 'choice', 'aria-hidden': 'true' }),
        icon(element.type === 'consolidated' ? 'sigma' : 'member', 14),
        h('span', { class: 'grow' }, row.match ? marked(row.name, words) : row.name),
        row.kids ? h('span', { class: 'muted' }, String(row.kids)) : null);
      });
      if (cut) drawn.push(h('p', { class: 'hint' }, `Showing the first ${ROW_LIMIT.toLocaleString()} rows. Search to find the rest.`));
      mount(tree, drawn.length ? drawn : h('p', { class: 'muted' }, 'Nothing matches.'));
      search.found(matches?.size ?? 0, Boolean(matches));
      if (focus) tree.querySelector('[tabindex="0"]')?.focus();
    };
    const search = searchBox({
      label: `Find in ${name}`,
      onsearch: (next) => {
        words = next;
        found = words ? reveal({ roots, children, matches: new Set(dim.elements.filter((element) => element.name.toLowerCase().includes(words)).map((element) => element.name)) }) : null;
        draw();
      },
      // Down goes to the first member found, or, with no search, to the one chosen.
      ondown: () => {
        const first = words ? tree.querySelector('.picker-row:not(.context)') : null;
        if (first) chosen = first.dataset.name;
        draw(chosen);
      },
    });
    const levels = [...new Set(dim.elements.map((element) => element.level))].sort((a, b) => b - a);
    const opening = openButtons({
      all: () => {
        const every = new Set(dim.elements.filter((element) => element.children.length).map((element) => element.name));
        if (found) found = every;
        else open = every;
        draw();
      },
      none: () => {
        if (found) found = new Set();
        else open = new Set();
        draw();
      },
      levels: levels.map((level) => [level, dim.levels?.[level] || (level === 0 ? 'Leaves' : `Level ${level}`)]),
      to: (level) => {
        const down = new Set(dim.elements.filter((element) => element.level > level).map((element) => element.name));
        if (found) found = down;
        else open = down;
        draw();
      },
    });
    draw();
    await dialog({
      title: `Filter by ${name}`,
      confirm: 'Apply',
      body: h('div', { class: 'picker' },
        h('div', { class: 'picker-tools' }, search.box, opening),
        tree,
        h('p', { class: 'hint' }, 'Double-click a member, or press Enter on it, to filter by it at once.')),
      onsubmit: () => {
        this.change((layout) => {
          layout.filters.find((filter) => filter.dimension === name).member = chosen;
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
      if (this.layout.mdx) this.adopt(result);
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
    if (this.view === 'chart' && this.result && !this.error) return this.drawChart();
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
    // The attributes shown beside each dimension's members, where the layout asks for them.
    const shownAttributes = (place, tuple) => (tuple ?? []).map((m) => this.entryOf(place, m.dimension)?.attributes ?? []);
    const rowAttributes = shownAttributes('rows', rowTuples[0]);
    const columnAttributes = shownAttributes('columns', columnTuples[0]);
    const rowWidth = rowAttributes.reduce((sum, attributes) => sum + 1 + attributes.length, 0);
    const headRows = columnAttributes.reduce((sum, attributes) => sum + 1 + attributes.length, 0);
    const table = h('table', { class: 'pivot', role: 'grid', 'aria-rowcount': rowTuples.length + headRows, 'aria-colcount': columnTuples.length + rowWidth });
    const head = h('thead');
    for (let level = 0; level < columnDepth; level += 1) {
      const groups = [];
      // Neighbouring columns that share this member and those above it share one heading.
      for (let index = 0; index < columnTuples.length;) {
        const key = (i) => columnTuples[i].slice(0, level + 1).map((m) => m.unique_name).join('|');
        let span = 1;
        while (index + span < columnTuples.length && key(index + span) === key(index)) span += 1;
        groups.push([columnTuples[index][level], span, index]);
        index += span;
      }
      const tr = h('tr');
      if (level === 0) {
        // A heading over each column of row headings: its dimension, or an attribute shown.
        const labels = rowTuples[0]?.flatMap((m, index) => [m.dimension, ...rowAttributes[index]]) ?? [];
        tr.append(...(labels.length ? labels : ['']).map((label) => h('th', { class: 'corner', rowspan: headRows, scope: 'col' }, label)));
      }
      // Each heading knows the columns beneath it, to choose them when it is clicked.
      const across = (span, index) => ({ c1: index, c2: index + span - 1 });
      for (const [m, span, index] of groups) {
        tr.append(h('th', {
          scope: 'col',
          colspan: span,
          tabindex: '-1',
          class: `col-head ${m.type === 'consolidated' ? 'total' : ''}`,
          dataset: { ...across(span, index), ...this.headingOf('columns', m) },
        }, this.memberLabel('columns', m)));
      }
      head.append(tr);
      for (const attribute of columnAttributes[level]) {
        head.append(h('tr', { class: 'attribute-row-head' }, groups.map(([m, span, index]) => h('th', {
          scope: 'col',
          colspan: span,
          class: 'col-attribute',
          title: attribute,
          dataset: across(span, index),
        }, this.attributeValue(m, attribute)))));
      }
    }
    const body = h('tbody');
    const columnCount = columnTuples.length;
    shownRows.forEach((tuple, r) => {
      const tr = h('tr', { class: total(tuple) ? 'total-row' : '' });
      tuple.forEach((m, level) => {
        // A member repeated from the row above is left blank, so groups read clearly.
        const shares = (other) => other && other.slice(0, level + 1).every((o, i) => o.unique_name === tuple[i].unique_name);
        const same = shares(shownRows[r - 1]);
        // A heading stands for its row and the rows beneath that repeat it.
        let last = r;
        while (!same && shares(shownRows[last + 1])) last += 1;
        const down = { r1: r, r2: last };
        tr.append(h('th', {
          scope: 'row',
          tabindex: '-1',
          class: `row-head ${m.type === 'consolidated' ? 'total' : ''}`,
          dataset: same ? down : { ...down, ...this.headingOf('rows', m) },
        }, same ? '' : this.memberLabel('rows', m)));
        for (const attribute of rowAttributes[level] ?? []) {
          tr.append(h('th', { scope: 'row', tabindex: '-1', class: 'row-attribute', title: attribute, dataset: down }, same ? '' : this.attributeValue(m, attribute)));
        }
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
    const keys = [
      ...(rowTuples[0] ?? []).flatMap((m, index) => [`row:${m.dimension}`, ...rowAttributes[index].map((attribute) => `row:${m.dimension}:${attribute}`)]),
      ...columnTuples.map((tuple) => `col:${tuple.map((m) => m.unique_name).join('|')}`),
    ];
    table.append(h('colgroup', {}, keys.map((key) => h('col', { dataset: { key } }))), head, body);
    const summary = h('span', { class: 'selection-summary', 'aria-live': 'polite' });
    this.selection = new Selection(table, {
      rows: shownRows.length,
      columns: columnCount,
      value: (r, c) => result.cells[r * columnCount + c],
      changed: (chosen) => mount(summary, ...this.summarize(chosen)),
    });
    table.addEventListener('contextmenu', (event) => {
      const heading = event.target.closest('th[data-member]');
      if (heading) {
        event.preventDefault();
        this.headingMenu(heading, { x: event.clientX, y: event.clientY });
        return;
      }
      const cell = event.target.closest('td');
      if (!cell) return;
      event.preventDefault();
      const [r, c] = [Number(cell.dataset.r), Number(cell.dataset.c)];
      if (!this.selection.has(r, c)) this.selection.choose({ r1: r, r2: r, c1: c, c2: c });
      this.cellMenu(r, c, { x: event.clientX, y: event.clientY });
    });
    table.addEventListener('keydown', (event) => {
      if (event.target.closest('th')) this.headingKey(event, table);
      else this.moveFocus(event, table);
    });
    table.addEventListener('paste', (event) => {
      const text = event.clipboardData?.getData('text/plain');
      if (!text || event.target.closest('input, textarea')) return;
      event.preventDefault();
      this.paste(text);
    });
    table.addEventListener('dblclick', (event) => {
      // A heading opens or closes; a cell that may be written is edited.
      const heading = event.target.closest('th[data-member]');
      if (heading && !event.target.closest('button')) {
        this.openHeading(heading);
        return;
      }
      const cell = event.target.closest('td.editable');
      if (cell) this.edit(cell);
    });
    const scroller = h('div', { class: 'pivot-scroll' }, table);
    mount(this.output, scroller);
    this.size(table);
    this.pinHeads(table);
    this.focusCell();
    this.refocusHeading(table);
    this.markFound();
    const cells = rowTuples.length * columnCount;
    mount(this.statusLine,
      h('span', {}, `${rowTuples.length.toLocaleString()} rows × ${columnCount.toLocaleString()} columns`),
      h('span', { class: 'muted' }, `${cells.toLocaleString()} cells in ${Math.round(this.took)} ms`),
      rowTuples.length > limit ? h('span', { class: 'badge warn' }, `Showing the first ${limit.toLocaleString()} rows`) : null,
      this.layout.sort ? h('span', { class: 'badge accent' }, `Sorted ${this.layout.sort.descending ? 'largest' : 'smallest'} first`) : null,
      summary);
  }

  /** Says, for more than one cell chosen, how many there are and what their numbers add to. */
  summarize(chosen) {
    if (chosen.count < 2) return [];
    const figure = (label, value) => h('span', {}, h('span', { class: 'muted' }, `${label} `), formatNumber(value));
    return [
      h('span', {}, h('span', { class: 'muted' }, 'Count '), chosen.count.toLocaleString()),
      ...(chosen.numbers ? [
        figure('Sum', chosen.sum),
        figure('Average', chosen.average),
        figure('Min', chosen.min),
        figure('Max', chosen.max),
      ] : []),
    ];
  }

  /** A member's heading: with a button that opens or closes it where it has children. */
  memberLabel(place, m) {
    const entry = this.layout[place]?.find((candidate) => candidate.dimension === m.dimension);
    const dim = this.dims.get(m.dimension);
    const element = dim?.byName.get(m.name);
    const depth = entry ? this.depthOf(entry, dim, m.name) : 0;
    const label = h('span', { class: 'member-label', dataset: { depth } });
    if (entry && element?.children.length) {
      const opened = entry.expanded.includes(m.name);
      // Not a stop for Tab: the heading itself takes the keys that open and close it.
      label.append(h('button', {
        class: 'twisty',
        tabindex: '-1',
        'aria-expanded': String(opened),
        'aria-label': `${opened ? 'Close' : 'Open'} ${m.name}`,
        onclick: () => this.toggle(place, m.dimension, m.name),
      }, icon(opened ? 'minus' : 'plus', 12)));
    } else if (depth) {
      label.append(h('span', { class: 'twisty' }));
    }
    const alias = entry?.alias ? element?.attributes?.[entry.alias] : null;
    label.append(h('span', { title: alias ? m.name : null }, alias || m.name));
    return label;
  }

  entryOf(place, dimension) {
    return this.layout[place]?.find((candidate) => candidate.dimension === dimension);
  }

  /** An attribute of a member, as text. */
  attributeValue(m, attribute) {
    const value = this.dims.get(m.dimension)?.byName.get(m.name)?.attributes?.[attribute];
    if (value === undefined || value === null) return '';
    return typeof value === 'number' ? formatNumber(value) : String(value);
  }

  /**
   * Sizes the grid: every column to its content and headings, unless given a width by hand,
   * and every row to the height given by hand, if one is. Then puts the handles that change
   * them on the headings.
   */
  size(table) {
    const geometry = this.layout.geometry ?? {};
    const cols = [...table.querySelectorAll('col')];
    if (geometry.row_height) table.style.setProperty('--row-height', `${geometry.row_height}px`);
    // Measured as laid out by their content, then held, so that changing one moves no other.
    const fitted = cols.map((col) => Math.ceil(col.getBoundingClientRect().width));
    const widths = cols.map((col, index) => geometry.widths?.[col.dataset.key] ?? fitted[index]);
    const apply = () => {
      cols.forEach((col, index) => {
        col.style.width = `${widths[index]}px`;
      });
      table.style.width = `${widths.reduce((sum, width) => sum + width, 0)}px`;
    };
    apply();
    table.classList.add('sized');
    const keep = (change) => {
      this.layout.geometry = { ...(this.layout.geometry ?? {}), ...change(this.layout.geometry ?? {}) };
      localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
      this.drawToolbar();
    };
    // Follows the pointer wherever it goes until it is let go.
    const drag = (handle, start, move, end) => handle.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      event.stopPropagation();
      const origin = { x: event.clientX, y: event.clientY, value: start() };
      document.body.classList.add('resizing');
      const moved = (next) => move(origin, next);
      const done = () => {
        window.removeEventListener('pointermove', moved);
        window.removeEventListener('pointerup', done);
        window.removeEventListener('pointercancel', done);
        document.body.classList.remove('resizing');
        end();
      };
      window.addEventListener('pointermove', moved);
      window.addEventListener('pointerup', done);
      window.addEventListener('pointercancel', done);
    });
    // A handle on the right edge of the heading of each column.
    const headRows = [...table.tHead.rows];
    const dataHeads = [...headRows[headRows.length - 1].cells].filter((cell) => !cell.classList.contains('corner'));
    const corners = [...table.tHead.querySelectorAll('th.corner')];
    const rowHeads = [...(table.tBodies[0]?.rows[0]?.cells ?? [])].filter((cell) => cell.tagName === 'TH');
    const columnHeads = [...(rowHeads.length ? corners : []), ...dataHeads];
    if (columnHeads.length !== cols.length) return;
    columnHeads.forEach((cell, index) => {
      const handle = h('span', { class: 'col-resize', role: 'separator', 'aria-orientation': 'vertical', 'aria-label': 'Drag to change the width of the column', title: 'Drag to change the width; double-click to fit it' });
      cell.append(handle);
      drag(handle, () => widths[index], (origin, event) => {
        widths[index] = Math.max(40, Math.round(origin.value + event.clientX - origin.x));
        apply();
      }, () => keep((geometry) => ({ widths: { ...(geometry.widths ?? {}), [cols[index].dataset.key]: widths[index] } })));
      handle.addEventListener('dblclick', (event) => {
        event.stopPropagation();
        widths[index] = fitted[index];
        apply();
        keep((geometry) => {
          const kept = { ...(geometry.widths ?? {}) };
          delete kept[cols[index].dataset.key];
          return { widths: kept };
        });
      });
    });
    // One handle beneath the first row's heading sets the height of every row.
    const first = rowHeads[0] ?? table.tBodies[0]?.rows[0]?.cells[0];
    if (!first) return;
    const handle = h('span', { class: 'row-resize', role: 'separator', 'aria-orientation': 'horizontal', 'aria-label': 'Drag to change the height of every row', title: 'Drag to change the height of every row' });
    first.append(handle);
    let height = geometry.row_height ?? first.getBoundingClientRect().height;
    drag(handle, () => height, (origin, event) => {
      height = Math.max(22, Math.min(120, Math.round(origin.value + event.clientY - origin.y)));
      table.style.setProperty('--row-height', `${height}px`);
    }, () => {
      keep(() => ({ row_height: height }));
      this.pinHeads(table);
    });
  }

  /**
   * Keeps the heading rows and the row headings in view as the grid scrolls: each heading
   * row below the one above it, and each row heading column beside those before it.
   */
  pinHeads(table) {
    let top = 0;
    for (const row of table.tHead.rows) {
      for (const cell of row.cells) {
        if (!cell.classList.contains('corner')) cell.style.top = `${top}px`;
      }
      top += row.offsetHeight;
    }
    const first = table.tBodies[0]?.rows[0];
    if (!first) return;
    const lefts = [];
    let left = 0;
    for (const cell of first.cells) {
      if (cell.tagName !== 'TH') break;
      lefts.push(left);
      left += cell.offsetWidth;
    }
    table.tHead.querySelectorAll('th.corner').forEach((cell, index) => {
      cell.style.left = `${lefts[index] ?? 0}px`;
    });
    for (const row of table.tBodies[0].rows) {
      [...row.cells].slice(0, lefts.length).forEach((cell, index) => {
        cell.style.left = `${lefts[index]}px`;
        cell.classList.add('pinned');
      });
    }
  }

  /** Chooses the attributes shown beside a dimension's members, and an alias for its names. */
  async chooseAttributes(name) {
    const place = this.placeOf(name);
    const dim = this.dims.get(name);
    const entry = this.entryOf(place, name);
    const declared = Object.entries(dim.attributes ?? {});
    const shown = new Set(entry.attributes ?? []);
    const boxes = declared.map(([attribute, kind]) => h('label', { class: 'check' },
      h('input', { type: 'checkbox', value: attribute, checked: shown.has(attribute) }),
      attribute, kind === 'alias' ? h('span', { class: 'muted' }, ' alias') : null));
    const aliases = declared.filter(([, kind]) => kind === 'alias');
    const alias = h('select', { class: 'select', id: 'alias' },
      h('option', { value: '' }, 'Their names'),
      aliases.map(([attribute]) => h('option', { value: attribute, selected: entry.alias === attribute }, attribute)));
    const chosen = await dialog({
      title: `Attributes of ${name}`,
      confirm: 'Show',
      body: h('div', { class: 'facts' },
        h('fieldset', { class: 'field' }, h('legend', {}, place === 'rows' ? 'Shown in columns beside the members' : 'Shown in rows beneath the members'), h('div', { class: 'checks' }, boxes)),
        aliases.length ? h('div', { class: 'field' }, h('label', { for: 'alias' }, 'Show the members by'), alias) : null),
      onsubmit: (form) => ({
        attributes: [...form.querySelectorAll('input[type=checkbox]:checked')].map((box) => box.value),
        alias: alias.value || null,
      }),
    });
    if (!chosen) return;
    this.change((layout) => {
      const target = layout[place].find((candidate) => candidate.dimension === name);
      target.attributes = chosen.attributes;
      target.alias = chosen.alias;
    });
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
    if (!cell || event.defaultPrevented) return;
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
        const chosen = this.selection.cells();
        this.clear(chosen.length ? chosen : [[r, c]]);
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
    const next = this.selection.cell(r + move[0], c + move[1]);
    // Past the first column or row, the keys go on into the headings.
    const heading = next ? null
      : event.key === 'ArrowLeft' ? [...cell.parentElement.cells].filter((other) => other.tagName === 'TH').pop()
        : event.key === 'ArrowUp' ? this.columnHeading(table, table.tHead.rows.length - 1, c) : null;
    const target = next ?? heading;
    if (!target) return;
    event.preventDefault();
    if (next) {
      cell.tabIndex = -1;
      next.tabIndex = 0;
    }
    target.focus();
  }

  /** The heading, in one of the heading rows, over a column of cells. */
  columnHeading(table, row, c) {
    return [...(table.tHead.rows[row]?.cells ?? [])].find((cell) => cell.dataset.c1 !== undefined && Number(cell.dataset.c1) <= c && Number(cell.dataset.c2) >= c);
  }

  /** What a heading stands for, kept on it to act on: its member, dimension and axis. */
  headingOf(place, m) {
    return { member: m.name, dimension: m.dimension, place };
  }

  /**
   * The keys of a heading. The arrows move among the headings and back into the cells;
   * Enter, Space, + and - open and close its member, * opens everything beneath it, and the
   * menu key or Shift F10 opens its menu.
   */
  headingKey(event, table) {
    const heading = event.target.closest('th');
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const member = heading.dataset.member;
    const row = heading.parentElement;
    const inHead = row.parentElement === table.tHead;
    const opened = () => this.entryOf(heading.dataset.place, heading.dataset.dimension)?.expanded.includes(member);
    let target = null;
    switch (event.key) {
      case 'ArrowLeft':
      case 'ArrowRight': {
        const step = event.key === 'ArrowRight' ? 1 : -1;
        const sibling = row.cells[heading.cellIndex + step];
        if (sibling?.tagName === 'TH' && !sibling.classList.contains('corner')) target = sibling;
        else if (!inHead && step > 0) target = this.selection.cell(Number(row.querySelector('td')?.dataset.r), 0);
        break;
      }
      case 'ArrowUp':
      case 'ArrowDown': {
        const step = event.key === 'ArrowDown' ? 1 : -1;
        if (inHead) {
          const c = Number(heading.dataset.c1);
          const level = row.sectionRowIndex + step;
          target = level < table.tHead.rows.length ? this.columnHeading(table, level, c) : this.selection.cell(0, c);
        } else {
          const other = table.tBodies[0].rows[row.sectionRowIndex + step];
          target = other?.cells[heading.cellIndex];
        }
        break;
      }
      case 'Enter':
      case ' ':
        if (member) this.openHeading(heading);
        break;
      case '+':
        if (member && !opened()) this.openHeading(heading);
        break;
      case '-':
        if (member && opened()) this.openHeading(heading);
        break;
      case '*':
        if (member) this.openBeneath(heading, true);
        break;
      case 'ContextMenu':
      case 'F10':
        if (event.key === 'F10' && !event.shiftKey) return;
        if (member) this.headingMenu(heading, heading);
        break;
      default:
        return;
    }
    event.preventDefault();
    target?.focus();
  }

  /** Opens a heading's member, or closes it, keeping the keys on the heading. */
  openHeading(heading) {
    const { place, dimension, member } = heading.dataset;
    if (!this.dims.get(dimension)?.byName.get(member)?.children.length || !this.entryOf(place, dimension)) return;
    this.keepHeading(heading);
    this.toggle(place, dimension, member);
  }

  /** Opens, or closes, a heading's member and everything beneath it. */
  openBeneath(heading, open) {
    const { place, dimension, member } = heading.dataset;
    const dim = this.dims.get(dimension);
    const within = [member, ...beneath([member], (name) => this.childrenOf(dim, name))].filter((name) => this.childrenOf(dim, name).length);
    this.keepHeading(heading);
    this.setOpen(place, dimension, (expanded) => (open ? [...new Set([...expanded, ...within])] : expanded.filter((name) => !within.includes(name))));
  }

  childrenOf(dim, name) {
    return (dim.byName.get(name)?.children ?? []).map((child) => child.name).filter((child) => dim.byName.has(child));
  }

  /** Changes the members open of a dimension on an axis. */
  setOpen(place, dimension, open) {
    this.change((layout) => {
      const entry = layout[place].find((candidate) => candidate.dimension === dimension);
      entry.expanded = open(entry.expanded, entry);
    });
  }

  /** Keeps the keys on a heading through the grid being drawn again, if they were on it. */
  keepHeading(heading) {
    if (heading.contains(document.activeElement)) this.headingAfter = { ...heading.dataset };
  }

  refocusHeading(table) {
    const kept = this.headingAfter;
    this.headingAfter = null;
    if (!kept) return;
    const same = [...table.querySelectorAll('th[data-member]')].find((th) => th.dataset.member === kept.member && th.dataset.dimension === kept.dimension && th.dataset.place === kept.place);
    same?.focus();
  }

  /** The menu of a member's heading: opening and closing, keeping and removing, copying. */
  headingMenu(heading, at) {
    const { place, dimension, member: name } = heading.dataset;
    const dim = this.dims.get(dimension);
    const entry = this.entryOf(place, dimension);
    const element = dim?.byName.get(name);
    const kids = element ? this.childrenOf(dim, name).length : 0;
    const opened = entry?.expanded.includes(name);
    const chosen = entry?.members.includes(name);
    menu([
      { heading: name },
      entry && kids ? { label: opened ? 'Close it' : 'Open it', icon: opened ? 'minus' : 'plus', action: () => this.openHeading(heading) } : null,
      entry && kids ? { label: 'Open everything beneath it', icon: 'expand', action: () => this.openBeneath(heading, true) } : null,
      entry && opened ? { label: 'Close everything beneath it', icon: 'collapse', action: () => this.openBeneath(heading, false) } : null,
      entry && element ? '-' : null,
      entry && element ? {
        label: `Keep only ${name}`,
        icon: 'filter',
        action: () => this.change((layout) => {
          layout[place].find((candidate) => candidate.dimension === dimension).members = [name];
          layout.sort = null;
        }),
      } : null,
      entry && element ? {
        label: `Remove ${name}`,
        icon: 'eyeOff',
        // A member shown because its parent is open goes with its parent; one chosen can go.
        disabled: !chosen || entry.members.length < 2,
        action: () => this.change((layout) => {
          const target = layout[place].find((candidate) => candidate.dimension === dimension);
          target.members = target.members.filter((other) => other !== name);
        }),
      } : null,
      '-',
      { label: 'Copy the name', icon: 'copy', action: () => copied(name) },
      { label: 'Copy its MDX name', icon: 'code', action: () => copied(member(dimension, name)) },
      entry ? { label: `Choose members of ${dimension}…`, icon: 'check', action: () => this.pick(dimension) } : null,
    ], at);
  }

  cellMenu(r, c, at) {
    const [columns, rows] = this.result.axes;
    const rowTuple = rows?.tuples[r] ?? [];
    const columnTuple = columns.tuples[c] ?? [];
    const openable = (place, tuple) => tuple.filter((m) => this.dims.get(m.dimension)?.byName.get(m.name)?.children.length);
    const rowOpen = openable('rows', rowTuple);
    const chosen = this.selection?.cells().length ?? 0;
    menu([
      { heading: [...rowTuple, ...columnTuple].map((m) => m.name).join(' · ') },
      { label: 'Sort rows by this column, largest first', icon: 'down', disabled: !this.layout.rows.length, action: () => this.sortBy(columnTuple, true) },
      { label: 'Sort rows by this column, smallest first', icon: 'chevron', disabled: !this.layout.rows.length, action: () => this.sortBy(columnTuple, false) },
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
      '-',
      { label: chosen > 1 ? `Copy the ${chosen.toLocaleString()} cells` : 'Copy the value', icon: 'copy', action: () => copied(this.selection.text()) },
      { label: 'Copy with headings', icon: 'grid', action: () => copied(this.textWithHeadings()) },
      this.mayWrite() ? { label: 'Paste', icon: 'paste', action: () => this.pasteFromClipboard() } : null,
      this.selection.cells().some(([row, column]) => this.writable.has(`${row},${column}`))
        ? { label: chosen > 1 ? 'Clear the cells chosen' : 'Clear the value', icon: 'trash', action: () => this.clear(this.selection.cells()) }
        : null,
    ], at);
  }

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
  }

  /**
   * Writes values into cells of the grid, `{ r, c, value }` each, in one write within the
   * transaction the edits are held in, then runs the query again. Says `done` once written.
   */
  async writeCells(cells, done) {
    const written = cells.map(({ r, c, value }) => ({ at: this.coordinate(r, c), value }));
    this.focusAfter ??= [cells[0].r, cells[0].c];
    try {
      await this.ensureTransaction();
      await post('/cells/write', { transaction: this.tx, cells: written.map(({ at, value }) => ({ cube: this.cube.name, at, value })) });
    } catch (problem) {
      this.refused(problem);
      return;
    }
    for (const { at, value } of written) this.edits.set(JSON.stringify(at), value);
    this.changes = (this.changes ?? 0) + written.length;
    if (done) notify(done, 'good');
    await this.run();
  }

  /** The value a cell of the result holds. */
  valueAt(r, c) {
    return this.result.cells[r * this.result.axes[0].tuples.length + c];
  }

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
  }

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
  }

  /** The cells chosen, with the headings of their rows and columns, as tab-separated text. */
  textWithHeadings() {
    if (!this.selection.ranges.length) return '';
    const box = this.selection.ranges.reduce((a, b) => ({ r1: Math.min(a.r1, b.r1), r2: Math.max(a.r2, b.r2), c1: Math.min(a.c1, b.c1), c2: Math.max(a.c2, b.c2) }));
    return this.tableOf([box.r1, box.r2], [box.c1, box.c2]).map((cells) => cells.map(cellText).join('\t')).join('\n');
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
  /* ---- Chart ---- */

  drawChart() {
    const settings = this.layout.chart ?? { kind: 'bars', byColumns: false };
    const set = (change) => {
      this.layout.chart = { ...settings, ...change };
      localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
      this.drawChart();
    };
    const choice = (value, label, current, pick) => h('button', {
      role: 'tab',
      'aria-selected': String(current === value),
      onclick: () => pick(value),
    }, label);
    const data = shape(this.result, settings.byColumns);
    const plotted = data.series.some((entry) => entry.values.some((value) => value !== null));
    mount(this.output, h('div', { class: 'chart-pane' },
      h('div', { class: 'toolbar' },
        h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'Kind of chart' },
          choice('bars', 'Bars', settings.kind, (kind) => set({ kind })),
          choice('stacked', 'Stacked', settings.kind, (kind) => set({ kind })),
          choice('lines', 'Lines', settings.kind, (kind) => set({ kind }))),
        h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'Series from' },
          choice(false, 'A series for each row', settings.byColumns, (byColumns) => set({ byColumns })),
          choice(true, 'A series for each column', settings.byColumns, (byColumns) => set({ byColumns })))),
      plotted
        ? chart(data, settings.kind)
        : empty('chart', 'Nothing to chart', 'The result has no numbers. Choose other members, or show empty rows.')));
  }

  /* ---- Saving a view ---- */

  async saveView() {
    const name = h('input', { class: 'input', id: 'view-name', required: true, maxlength: 120 });
    const saved = await dialog({
      title: 'Save as a view',
      confirm: 'Save',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'view-name' }, 'Name'), name),
        h('p', { class: 'hint' }, `Saved with ${this.cube.name}, where everyone who may read the cube can open it. A view of the same name is replaced.`)),
      onsubmit: async () => {
        const view = name.value.trim();
        if (!view) throw new Error('Give the view a name.');
        // The cube as it is now, saved against the version it was read at, so that a change
        // someone else made meanwhile is not overwritten.
        const cube = await get(`/cubes/${part(this.cube.name)}`);
        const geometry = this.layout.geometry;
        const kept = geometry && (geometry.row_height || Object.keys(geometry.widths ?? {}).length)
          ? { geometry: { ...(geometry.row_height ? { row_height: geometry.row_height } : {}), widths: geometry.widths ?? {} } }
          : {};
        const views = { ...(cube.views ?? {}), [view]: { mdx: this.text, ...kept } };
        await put(`/cubes/${part(this.cube.name)}`, { based_on: Number(modelVersion), cube: { ...cube, views } });
        this.cube.views = views;
        return view;
      },
    });
    if (saved) notify(`Saved the view “${saved}”.`, 'good');
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
      }, true);
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
    const field = (value) => {
      const text = value === null || value === undefined ? '' : String(value);
      return /[",\n\r]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
    };
    const lines = this.tableOf().map((cells) => cells.map(field).join(','));
    download(`${this.cube.name}.csv`, `${lines.join('\r\n')}\r\n`, 'text/csv');
  }

  /**
   * The grid as rows of cells, as it shows them: headings first, names by alias where one
   * is chosen, with the attributes shown. `rows` and `columns`, each `[first, last]`, take
   * part of it; all of it otherwise.
   */
  tableOf(rows = null, columns = null) {
    const result = this.result;
    const [columnAxis, rowAxis] = result.axes;
    const columnCount = columnAxis.tuples.length;
    const rowTuples = rowAxis?.tuples ?? [[]];
    const [r1, r2] = rows ?? [0, rowTuples.length - 1];
    const [c1, c2] = columns ?? [0, columnCount - 1];
    const columnTuples = columnAxis.tuples.slice(c1, c2 + 1);
    const label = (place, m) => {
      const entry = this.entryOf(place, m.dimension);
      const alias = entry?.alias ? this.dims.get(m.dimension)?.byName.get(m.name)?.attributes?.[entry.alias] : null;
      return alias || m.name;
    };
    const attributesOf = (place, m) => this.entryOf(place, m.dimension)?.attributes ?? [];
    const rowHeading = rowTuples[0] ?? [];
    const rowCells = (tuple) => tuple.flatMap((m) => [label('rows', m), ...attributesOf('rows', m).map((attribute) => this.attributeValue(m, attribute))]);
    const width = rowCells(rowHeading).length;
    const heading = rowHeading.flatMap((m) => [m.dimension, ...attributesOf('rows', m)]);
    const depth = columnAxis.tuples[0]?.length ?? 0;
    const table = [];
    for (let level = 0; level < depth; level += 1) {
      const lead = level === depth - 1 ? heading : Array(width).fill('');
      table.push([...lead, ...columnTuples.map((tuple) => label('columns', tuple[level]))]);
      for (const attribute of attributesOf('columns', columnAxis.tuples[0][level])) {
        const named = width ? [...Array(width - 1).fill(''), attribute] : [];
        table.push([...named, ...columnTuples.map((tuple) => this.attributeValue(tuple[level], attribute))]);
      }
    }
    for (let r = r1; r <= r2; r += 1) {
      table.push([...rowCells(rowTuples[r]), ...columnTuples.map((_, index) => result.cells[r * columnCount + c1 + index])]);
    }
    return table;
  }
}
/* An unhandled refusal is reported, not lost. */
export { ApiError };
