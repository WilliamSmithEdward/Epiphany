// Exploring a cube: a field list, a layout of rows, columns and filters, and the pivot grid
// of the query that layout makes.

import { ApiError, get, modelVersion, part, post, put } from './api.js';
import { empty, failure, go, loading, model, state } from './app.js';
import { chart, shape } from './chart.js';
import { CALCULATIONS, defaultLayout, mdx, saved, shown } from './layout.js';
import { chooseMember, dimension } from './picker.js';
import { editSet } from './sets.js';
import { beneath } from './tree.js';
import { dialog, download, formatValue, h, icon, menu, mount, notify, parseNumber, SCALES, splitter, textWidths, working } from './ui.js';
import { comments } from './comments.js';
import { grid } from './grid.js';
import { writeback } from './writeback.js';

/** What an entry of the history of changes changed, as the Undo and Redo buttons say it. */
const said = (entry) => (entry === undefined ? '' : typeof entry === 'string' ? 'the change to the layout' : entry.label);

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
    if (view) Object.assign(this.layout, { mdx: view.mdx, geometry: view.geometry ?? null, format: view.format ?? null, highlights: view.highlights ?? null });
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
    // Cells held, which spreads leave as they are: by their place in the cube, so that a hold
    // outlasts sorting, pivoting and saving, until it is released.
    // A hold that no longer fits the cube, after a change to the model, is let go.
    const fits = (key) => {
      const at = JSON.parse(key);
      return at.length === cube.dimensions.length && at.every((name, index) => dims.get(cube.dimensions[index])?.byName.has(name));
    };
    this.holds = new Set(JSON.parse(localStorage.getItem(`epiphany.holds.${cube.name}`) ?? '[]').filter(fits));
    addEventListener('beforeunload', (event) => {
      if (this.tx && this.changes && this.page.isConnected) event.preventDefault();
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
    // A transaction whose changes were all taken back is let go without asking.
    if (!this.changes) {
      post(`/transactions/${this.tx}/rollback`).catch(() => {});
      this.forget();
      return true;
    }
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
      // A calculation over members that are gone goes with them.
      const fits = (each) => CALCULATIONS[each.kind] && [each.a, each.b].every((name) => name === undefined || this.dims.get(each.dimension)?.byName.has(name));
      layout.calculations = (layout.calculations ?? []).filter(fits);
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
      top: null,
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
    // While paused, the layout is built step by step and the query runs once, when asked.
    if (this.paused) {
      this.stale = true;
      this.output.classList.add('stale');
      this.drawToolbar();
      return;
    }
    this.drawToolbar();
    this.run();
  }

  /** Pauses running the query as the layout changes, or resumes, running it if it changed. */
  pause(paused) {
    this.paused = paused;
    if (!paused && this.stale) this.run();
    else this.drawToolbar();
  }

  /**
   * Takes back the last change, of the layout or of values. A change of values goes back
   * within the edit transaction, to the point it was made from.
   */
  async undo() {
    const last = this.past.pop();
    if (last === undefined) return;
    if (typeof last === 'string') {
      this.future.push(JSON.stringify(this.layout));
      this.layout = JSON.parse(last);
      this.after();
      return;
    }
    try {
      await post(`/transactions/${this.tx}/undo`, { to: last.point });
    } catch (problem) {
      this.refused(problem);
      return;
    }
    this.future.push(last);
    this.edits = last.edits;
    this.changes = last.changes;
    notify(`Undid ${last.label}.`);
    await this.run();
  }

  /** Makes the last change taken back again. */
  async redo() {
    const next = this.future.pop();
    if (next === undefined) return;
    if (typeof next === 'string') {
      this.past.push(JSON.stringify(this.layout));
      this.layout = JSON.parse(next);
      this.after();
      return;
    }
    await next.again();
  }

  /* ---- Building the page ---- */
  build(target) {
    this.fields = h('aside', { class: 'fields', 'aria-label': 'Fields' });
    this.toolbar = h('div', { class: 'toolbar' });
    this.layoutBar = h('div', { class: 'layout-bar' });
    this.output = h('div', { class: 'output' });
    this.statusLine = h('div', { class: 'status-line', role: 'status', 'aria-live': 'polite' });
    this.commentPanel = h('aside', { class: 'comment-panel', 'aria-label': 'Comments', hidden: true });
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
              layout.format = this.cube.views[name].format ?? null;
              layout.highlights = this.cube.views[name].highlights ?? null;
            }, true),
          })),
          admin ? '-' : null,
          admin ? { label: 'Save this query as a view…', icon: 'save', disabled: !this.text, action: () => this.saveView() } : null,
        ], event.currentTarget);
      },
    }, icon('grid', 16), 'Views', icon('down', 14));
    this.tabs = h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'Show as' });
    this.finder = this.buildFinder();
    const outputRow = h('div', { class: 'output-row' }, this.output, this.commentPanel);
    this.page = h('div', { class: 'explorer' },
      this.fields,
      h('section', { class: 'work' },
        h('div', { class: 'work-head' },
          h('div', { class: 'toolbar' }, icon('cube', 20), cubes, viewButton, this.finder.box, h('div', { class: 'grow' }), this.tabs),
          this.toolbar,
          this.layoutBar),
        outputRow,
        this.pendingBar,
        this.statusLine));
    mount(target, this.page);
    // The field list as wide as its dimensions' names need, and the comments as wide as
    // they were dragged to be.
    const width = textWidths();
    const names = this.cube.dimensions.map(width);
    splitter(this.page, { name: 'fields', key: 'fields', min: 220, max: 560, fit: () => Math.ceil(Math.max(130, ...names) + 150) }).fit();
    splitter(outputRow, { name: 'comments', key: 'comments', min: 260, max: 720, edge: 'start', fit: () => 340 }).fit();
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

  /** The strip that says what is not yet saved, with the ways to save or discard it. */
  drawPending() {
    this.pendingBar.hidden = !this.tx || !this.changes;
    if (this.pendingBar.hidden) return;
    const count = this.changes || 0;
    mount(this.pendingBar,
      icon('edit', 16),
      h('span', { class: 'grow' }, `${count} unsaved change${count === 1 ? '' : 's'}. Totals already include them; nothing is stored until you save.`),
      h('button', { class: 'button small', onclick: (event) => working(event.currentTarget, () => this.discard()) }, 'Discard'),
      h('button', { class: 'button small primary', onclick: (event) => working(event.currentTarget, () => this.save()) }, icon('save', 14), 'Save changes'));
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
      button('Undo', 'undo', () => this.undo(), { disabled: !this.past.length, title: `Undo ${said(this.past.at(-1))} (Ctrl Z)` }),
      button('Redo', 'redo', () => this.redo(), { disabled: !this.future.length, title: `Redo ${said(this.future.at(-1))} (Ctrl Y)` }),
      h('span', { class: 'divider' }),
      button('Swap axes', 'swap', () => this.change((l) => {
        [l.rows, l.columns] = [l.columns, l.rows];
        l.sort = null;
        l.top = null;
      })),
      button(layout.nonEmpty ? 'Showing non-empty' : 'Showing empty rows', layout.nonEmpty ? 'eyeOff' : 'eye',
        () => this.change((l) => {
          l.nonEmpty = !l.nonEmpty;
        }), { 'aria-pressed': String(layout.nonEmpty), title: 'Leave out rows and columns with no values' }),
      layout.sort ? button('Clear sort', 'close', () => this.change((l) => {
        l.sort = null;
      })) : null,
      layout.top ? button('Show every row', 'close', () => this.change((l) => {
        l.top = null;
      }), { title: `Showing the ${layout.top.count.toLocaleString()} ${layout.top.bottom ? 'smallest' : 'largest'} rows only` }) : null,
      h('div', { class: 'grow' }),
      layout.mdx ? h('span', {
        class: 'badge accent',
        title: 'The query is as written in the MDX tab. The layout shows its parts; changing the layout replaces it with the layout\'s own query.',
      }, icon('code', 12), 'Written in MDX') : null,
      button(this.paused ? 'Resume' : 'Pause', this.paused ? 'play' : 'pause', () => this.pause(!this.paused), {
        'aria-pressed': String(Boolean(this.paused)),
        title: this.paused ? 'Run the query as the layout changes again' : 'Build the layout without running the query at each step',
      }),
      button(this.stale ? 'Run' : 'Refresh', this.stale ? 'play' : 'refresh', () => this.run(), this.stale ? { class: 'button small primary' } : {}),
      button('Format', 'hash', () => this.chooseFormat(), { 'aria-pressed': String(Boolean(layout.format)), title: 'Decimals, units, negatives and zeros' }),
      button('Highlight', 'bars', () => this.chooseHighlights(), { 'aria-pressed': String(Boolean(layout.highlights)), title: 'Data bars, shades and coloured rules' }),
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
      {
        label: entry.totalsLast ? 'Totals before their members' : 'Totals after their members',
        icon: 'sigma',
        action: () => this.change((layout) => {
          const target = layout[place].find((candidate) => candidate.dimension === dimension);
          target.totalsLast = !target.totalsLast;
        }),
      },
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
      layout.top = null;
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
        layout.top = null;
      });
      return;
    }
    // A filter stands at one member, chosen from the dimension's tree.
    const filter = this.layout.filters.find((candidate) => candidate.dimension === name);
    const member = await chooseMember({ ...dim, name }, {
      title: `Filter by ${name}`,
      current: filter.member,
      hint: 'Double-click a member, or press Enter on it, to filter by it at once.',
    });
    if (member === null) return;
    this.change((layout) => {
      layout.filters.find((candidate) => candidate.dimension === name).member = member;
      layout.sort = null;
      layout.top = null;
    });
  }

  /* ---- Running the query ---- */
  async run() {
    const ticket = ++this.ticket;
    // A query still running for an earlier layout is let go.
    this.stopping?.abort();
    const stopping = new AbortController();
    this.stopping = stopping;
    this.stale = false;
    this.output.classList.remove('stale');
    const text = mdx(this.layout, this.dims);
    this.text = text;
    if (!this.layout.mdx && (!this.layout.rows.length || !this.layout.columns.length)) {
      this.result = null;
      this.draw();
      return;
    }
    // The first answer is awaited in the grid's outline; after that, the last answer stays,
    // fading if the next is slow to come.
    if (!this.result && !this.error && this.view !== 'mdx') mount(this.output, loading('lines'));
    this.output.classList.add('busy');
    const started = performance.now();
    const telling = setTimeout(() => this.drawRunning(started, stopping), 700);
    try {
      const result = await post('/mdx', { mdx: text, transaction: this.tx ?? undefined }, { quiet: true, signal: stopping.signal });
      if (ticket !== this.ticket) return;
      this.result = result;
      this.error = null;
      this.took = performance.now() - started;
      if (this.layout.mdx) this.adopt(result);
      const [found] = await Promise.all([this.findWritable(result), this.loadComments()]);
      ({ writable: this.writable, why: this.why } = found);
      if (ticket !== this.ticket) return;
    } catch (problem) {
      if (ticket !== this.ticket) return;
      if (problem.code === 'stopped') {
        this.stopped();
        return;
      }
      this.result = null;
      this.error = problem;
    } finally {
      clearTimeout(telling);
      if (ticket === this.ticket) this.output.classList.remove('busy');
    }
    this.drawToolbar();
    this.draw();
    if (this.commentCell) this.drawComments();
  }

  /** Says the query is still running, and for how long, with a button to stop waiting. */
  drawRunning(started, stopping) {
    const time = h('span', { class: 'muted', 'aria-hidden': 'true' });
    const tick = () => {
      if (!time.isConnected) return;
      time.textContent = `${((performance.now() - started) / 1000).toFixed(1)} s`;
      setTimeout(tick, 100);
    };
    mount(this.statusLine,
      h('span', { class: 'running' }, h('span', { class: 'spinner small', 'aria-hidden': 'true' }), 'Running the query…', time),
      h('button', { class: 'button small quiet', onclick: () => stopping.abort() }, icon('close', 14), 'Stop'));
    tick();
  }

  /** A query stopped before it answered: what was shown stays, marked out of date. */
  stopped() {
    this.stale = true;
    this.output.classList.add('stale');
    this.drawToolbar();
    if (!this.result && !this.error && this.view !== 'mdx') {
      mount(this.output, empty('pause', 'Stopped', 'The query was stopped before it answered. Run it when you are ready.'));
    }
    mount(this.statusLine, h('span', { class: 'muted' }, this.result ? 'Stopped. The grid shows the last answer; Run asks again.' : 'Stopped.'));
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

  entryOf(place, dimension) {
    return this.layout[place]?.find((candidate) => candidate.dimension === dimension);
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

  toggle(place, dimName, name) {
    this.change((layout) => {
      const entry = layout[place].find((candidate) => candidate.dimension === dimName);
      entry.expanded = entry.expanded.includes(name)
        ? entry.expanded.filter((candidate) => candidate !== name)
        : [...entry.expanded, name];
    });
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

  sortBy(columnTuple, descending) {
    this.change((layout) => {
      layout.sort = { tuple: columnTuple.map((m) => [m.dimension, m.name]), descending };
    });
  }

  /* ---- Chart ---- */
  drawChart() {
    mount(this.statusLine);
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

  /** Asks how many of the largest or smallest rows by a column to keep, and keeps only them. */
  async keepTop(columnTuple) {
    const count = h('input', { class: 'input', id: 'top-count', type: 'number', min: 1, step: 1, value: String(this.layout.top?.count ?? 10), required: true });
    const which = h('select', { class: 'select', id: 'top-which' },
      h('option', { value: 'largest', selected: !this.layout.top?.bottom }, 'The largest'),
      h('option', { value: 'smallest', selected: Boolean(this.layout.top?.bottom) }, 'The smallest'));
    const asked = await dialog({
      title: 'Keep only some rows',
      confirm: 'Keep them',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field-pair' },
          h('div', { class: 'field' }, h('label', { for: 'top-which' }, 'Which'), which),
          h('div', { class: 'field' }, h('label', { for: 'top-count' }, 'How many'), count)),
        h('p', { class: 'hint' }, `By ${columnTuple.map((m) => m.name).join(' · ')}. Show every row again from the toolbar.`)),
      onsubmit: () => {
        const n = Math.floor(Number(count.value));
        if (!(n >= 1)) throw new Error('Keep at least one row.');
        return { count: n, bottom: which.value === 'smallest' };
      },
    });
    if (!asked) return;
    this.change((layout) => {
      layout.top = { ...asked, tuple: columnTuple.map((m) => [m.dimension, m.name]) };
    });
  }

  /** Whether a member of a dimension is one of the layout's calculations. */
  calculated(dimension, name) {
    return (this.layout.calculations ?? []).some((each) => each.dimension === dimension && each.name === name);
  }

  /**
   * Asks which calculation to add beside a member of a dimension on an axis: its
   * difference from another member, its ratio or change against one, or its share of its
   * parent. It stands after the dimension's members, worked out by the query.
   */
  async addCalculation(place, dimension, name) {
    const dim = this.dims.get(dimension);
    const others = shown(this.entryOf(place, dimension), dim).map((each) => each.name).filter((other) => other !== name);
    const parented = dim.byName.get(name)?.parents.length > 0;
    const kinds = Object.entries(CALCULATIONS).filter(([kind]) => (kind === 'share' ? parented : others.length > 0));
    if (!kinds.length) {
      notify(`Show another member of ${dimension} beside ${name} to calculate with it.`);
      return;
    }
    const kind = h('select', { class: 'select', id: 'calc-kind' }, kinds.map(([value, [label]]) => h('option', { value }, label)));
    const other = h('select', { class: 'select', id: 'calc-other' }, others.map((each) => h('option', { value: each }, each)));
    const otherField = h('div', { class: 'field' }, h('label', { for: 'calc-other' }, 'The other member'), other);
    const named = h('input', { class: 'input', id: 'calc-name', required: true, maxlength: 120 });
    let typed = false;
    named.addEventListener('input', () => { typed = true; });
    const suggest = () => {
      otherField.hidden = kind.value === 'share';
      const b = other.value;
      if (!typed) named.value = { difference: `${name} - ${b}`, ratio: `${name} / ${b}`, change: `${name} vs ${b} %`, share: `${name} % of parent` }[kind.value];
    };
    kind.addEventListener('change', suggest);
    other.addEventListener('change', suggest);
    suggest();
    const added = await dialog({
      title: `A calculation beside ${name}`,
      confirm: 'Add it',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'calc-kind' }, 'Show'), kind),
        otherField,
        h('div', { class: 'field' }, h('label', { for: 'calc-name' }, 'Called'), named),
        h('p', { class: 'hint' }, `It stands after the members of ${dimension}, worked out by the query for every cell; it is kept with the layout and any view saved from it.`)),
      onsubmit: () => {
        const label = named.value.trim();
        if (!label) throw new Error('Give the calculation a name.');
        if (dim.byName.has(label) || this.calculated(dimension, label)) throw new Error(`${dimension} has a member called ${label} already.`);
        return { dimension, name: label, kind: kind.value, a: name, ...(kind.value === 'share' ? {} : { b: other.value }) };
      },
    });
    if (!added) return;
    this.change((layout) => {
      layout.calculations = [...(layout.calculations ?? []), added];
    });
  }

  /** Asks how the grid shows its numbers: decimals, the unit, negatives and zeros. */
  async chooseFormat() {
    const format = this.layout.format ?? {};
    const select = (id, options, current) => h('select', { class: 'select', id },
      options.map(([value, label]) => h('option', { value, selected: String(current ?? '') === value }, label)));
    const decimals = select('format-decimals', [['', 'As needed'], ...['0', '1', '2', '3', '4'].map((digits) => [digits, digits])], format.decimals);
    const scale = select('format-scale', [['', 'As stored'], ...Object.entries(SCALES).map(([value, [, said]]) => [value, said.replace('In ', '').replace(/^./, (c) => c.toUpperCase())])], format.scale);
    const parentheses = h('input', { type: 'checkbox', id: 'format-parentheses', checked: Boolean(format.parentheses) });
    const dash = h('input', { type: 'checkbox', id: 'format-dash', checked: Boolean(format.dash_zeros) });
    const read = () => {
      const next = {
        ...(decimals.value !== '' ? { decimals: Number(decimals.value) } : {}),
        ...(scale.value ? { scale: scale.value } : {}),
        ...(parentheses.checked ? { parentheses: true } : {}),
        ...(dash.checked ? { dash_zeros: true } : {}),
      };
      return Object.keys(next).length ? next : null;
    };
    const sample = h('div', { class: 'format-sample' });
    const preview = () => mount(sample, [1234567.891, -98765.4, 0].map((value) => h('span', {}, formatValue(value, read()))));
    for (const input of [decimals, scale, parentheses, dash]) input.addEventListener('change', preview);
    preview();
    const chosen = await dialog({
      title: 'Number format',
      confirm: 'Apply',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field-pair' },
          h('div', { class: 'field' }, h('label', { for: 'format-decimals' }, 'Decimals'), decimals),
          h('div', { class: 'field' }, h('label', { for: 'format-scale' }, 'Show in'), scale)),
        h('label', { class: 'check' }, parentheses, 'Negatives in parentheses'),
        h('label', { class: 'check' }, dash, 'Zeros as a dash'),
        h('div', { class: 'field' }, h('span', { class: 'muted' }, 'Looks like'), sample),
        h('p', { class: 'hint' }, 'Only the showing changes: values are typed, copied and exported in full. A view saved keeps its format.')),
      onsubmit: () => ({ format: read() }),
    });
    if (!chosen) return;
    this.layout.format = chosen.format;
    localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
    this.drawToolbar();
    this.draw();
  }

  /** Asks how the grid draws the eye: bars, shades, and colours by rule. */
  async chooseHighlights() {
    const now = this.layout.highlights ?? {};
    const bars = h('input', { type: 'checkbox', checked: Boolean(now.bars) });
    const shades = h('input', { type: 'checkbox', checked: Boolean(now.shades) });
    const rules = (now.rules ?? []).map((rule) => ({ ...rule }));
    const list = h('div', { class: 'rules' });
    const TESTS = [['above', 'is above'], ['at_least', 'is at least'], ['below', 'is below'], ['at_most', 'is at most'], ['equals', 'equals']];
    const TONES = [['good', 'Good, in green'], ['bad', 'Bad, in red'], ['warn', 'Watch, in amber'], ['accent', 'Note, in the accent colour']];
    const draw = () => mount(list, rules.length ? rules.map((rule, index) => h('div', { class: 'rule' },
      h('span', { class: 'rule-join muted' }, index === 0 ? 'A number that' : 'or else'),
      h('select', { class: 'select', 'aria-label': 'Test', onchange: (event) => { rule.test = event.target.value; } },
        TESTS.map(([value, label]) => h('option', { value, selected: rule.test === value }, label))),
      h('input', { class: 'input', 'aria-label': 'Value', inputmode: 'decimal', value: String(rule.value), onchange: (event) => { rule.value = parseNumber(event.target.value); } }),
      h('select', { class: 'select', 'aria-label': 'Colour', onchange: (event) => { rule.tone = event.target.value; } },
        TONES.map(([value, label]) => h('option', { value, selected: rule.tone === value }, label))),
      h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Remove this rule', onclick: () => { rules.splice(index, 1); draw(); } }, icon('close', 14))))
      : h('p', { class: 'hint' }, 'No rules: add one to colour the numbers that meet it.'));
    draw();
    const chosen = await dialog({
      title: 'Highlights',
      confirm: 'Apply',
      wide: false,
      body: h('div', { class: 'facts' },
        h('label', { class: 'check' }, bars, 'Data bars: each number\'s share of the largest in its column'),
        h('label', { class: 'check' }, shades, 'Shades: from the lowest number in each column to the highest'),
        h('fieldset', { class: 'field' }, h('legend', {}, 'Colours by rule, the first a number meets deciding'), list,
          h('div', { class: 'toolbar' }, h('button', { class: 'button small', type: 'button', onclick: () => { rules.push({ test: 'below', value: 0, tone: 'bad' }); draw(); } }, icon('plus', 14), 'Add a rule'))),
        h('p', { class: 'hint' }, 'Totals take no bar or shade, being out of scale with what they add up. A view saved keeps its highlights.')),
      onsubmit: () => {
        if (rules.some((rule) => rule.value === null || !Number.isFinite(rule.value))) throw new Error('Give every rule a number.');
        const next = { ...(bars.checked ? { bars: true } : {}), ...(shades.checked ? { shades: true } : {}), ...(rules.length ? { rules } : {}) };
        return { highlights: Object.keys(next).length ? next : null };
      },
    });
    if (!chosen) return;
    this.layout.highlights = chosen.highlights;
    localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
    this.drawToolbar();
    this.draw();
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
        const format = this.layout.format ? { format: this.layout.format } : {};
        const highlights = this.layout.highlights ? { highlights: this.layout.highlights } : {};
        const views = { ...(cube.views ?? {}), [view]: { mdx: this.text, ...kept, ...format, ...highlights } };
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

// The grid, writing back and comments are the explorer's too.
Object.assign(Explorer.prototype, grid, writeback, comments);
