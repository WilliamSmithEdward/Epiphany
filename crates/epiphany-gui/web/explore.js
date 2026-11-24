// Exploring a cube: a field list, a layout of rows, columns and filters, and the pivot grid
// of the query that layout makes.

import { ApiError, del, get, modelVersion, part, post, put } from './api.js';
import { empty, failure, go, loading, model, readdress, state } from './app.js';
import { chart, seriesNames, shape } from './chart.js';
import { chooser, choseLately, lateFirst, lately, pick } from './finder.js';
import { CALCULATIONS, cubeOf, defaultLayout, emptiness, folded, handOver, linkedView, linkOf, mdx, namesOf, pathKey, pathsBeneath, quote, saved, shown, takeOver } from './layout.js';
import { chooseMember, dimension } from './picker.js';
import { csvOf } from './selection.js';
import { askDrawn, AXES, CHARTS } from './tiles.js';
import { editSet } from './sets.js';
import { choices, copyText, counted, cubeSaid, dialog, download, formatValue, h, icon, invalid, KINDS, menu, MOD, mount, notify, parseNumber, refocus, SCALES, splitter, textWidths, working } from './ui.js';
import { comments } from './comments.js';
import { grid, toneOf } from './grid.js';
import { sandboxes } from './sandboxes.js';
import { writeback } from './writeback.js';

/** What an entry of the history of changes changed, as the Undo and Redo buttons say it. */
const said = (entry) => (entry === undefined ? '' : typeof entry === 'string' ? 'the change to the layout' : entry.label);

/* ---- The page ---- */

/** Where the cube explored last is kept, in this browser. */


/** The explorer made last, whose transaction is let go when the page is closed. */
let onPage = null;

// A page closed with changes unsaved lets them go at once, rather than leaving them to hold
// their cells until the server gives up on them. A sandbox keeps its changes.
addEventListener('pagehide', () => {
  const explorer = onPage?.page?.isConnected ? onPage : null;
  if (explorer?.tx && !explorer.sandbox) {
    fetch(`/api/transactions/${explorer.tx}/rollback`, { method: 'POST', keepalive: true, headers: { 'x-requested-by': 'epiphany-gui' } });
  }
});

export async function render(target, { params, titled }) {
  const definition = await model();
  if (!definition.cubes.length) {
    mount(target, h('div', { class: 'page' }, empty('cube', 'No cubes yet', 'An administrator adds cubes to the model.')));
    return;
  }
  // Not told which cube, Explore opens the one explored last, while the model has it, and the
  // address names it; never explored, the first with views saved of it, as the one most worked
  // in, else the first.
  const [last] = lately('cubes');
  const first = (definition.cubes.find((each) => Object.keys(each.views ?? {}).length) ?? definition.cubes[0]).name;
  const name = params.get('cube') ?? (definition.cubes.some((each) => each.name === last) ? last : first);
  const cube = definition.cubes.find((candidate) => candidate.name === name);
  if (!cube) {
    mount(target, h('div', { class: 'page' }, empty('cube', `There is no cube called ${name}`, 'It may have been renamed or removed.',
      h('a', { class: 'button', href: '/explore', 'data-link': '' }, 'Explore another cube'))));
    return;
  }
  choseLately('cubes', cube.name);
  if (!params.has('cube')) readdress(`/explore?cube=${encodeURIComponent(cube.name)}`);
  // The user's sandboxes come with the dimensions, so that the first query runs in the one
  // last worked in.
  const [dims, kept] = await Promise.all([
    Promise.all(cube.dimensions.map(async (dim) => [dim, await dimension(dim)])).then((pairs) => new Map(pairs)),
    get('/sandboxes', { quiet: true }).catch(() => []),
  ]);
  // A query left for this cube, from another cube's page or another page, is run here; or the
  // view a link carries, or one saved with the cube.
  const handed = takeOver(cube.name);
  const shared = params.get('shared');
  const linked = shared ? linkedView(shared) : null;
  if (shared && !linked) notify('This link is damaged: it carries no query to show. The last layout of the cube is shown.', 'bad');
  // Its view, once shown, is the cube's last layout: the address is the cube's again.
  if (shared) readdress(`/explore?cube=${encodeURIComponent(cube.name)}`);
  const view = handed ? { mdx: handed } : linked ?? cube.views?.[params.get('view')];
  // Opened at a saved view, the page names it before the cube.
  const named = !handed && !linked && cube.views?.[params.get('view')] ? params.get('view') : null;
  return new Explorer(target, definition, cube, dims, view, kept, named ? (name, unsaved) => titled(`${named} · ${name}`, unsaved) : titled);
}

/** The most views the Views menu lists: past them, they are found by typing. */
const MENU_MOST = 12;

/** A saved view's query, and how it is shown, put into a layout: its chart where it has one. */
function opened(layout, view) {
  Object.assign(layout, { mdx: view.mdx, geometry: view.geometry ?? null, format: view.format ?? null, highlights: view.highlights ?? null });
  if (view.chart) {
    const { kind = 'bars', by_columns: byColumns = false, labels = false, lines = [], second_axis: second = [] } = view.chart;
    layout.chart = { kind, byColumns, labels, lines, second };
  }
}

/** A layout's chart as a view keeps it. */
function chartOf({ kind = 'bars', byColumns, labels, lines, second } = {}) {
  return {
    kind,
    ...(byColumns ? { by_columns: true } : {}),
    ...(labels ? { labels: true } : {}),
    ...(lines?.length ? { lines } : {}),
    ...(second?.length ? { second_axis: second } : {}),
  };
}

class Explorer {
  /**
   * `view`, where one is given, is a saved view to open in place of the last layout; `kept`
   * the user's sandboxes, one of which the values may be worked on in; `titled` names the
   * page in the browser's title.
   */
  constructor(target, definition, cube, dims, view, kept, titled) {
    this.titled = titled;
    this.definition = definition;
    this.cube = cube;
    this.dims = dims;
    this.layout = this.restore() ?? defaultLayout(cube, dims);
    if (view) opened(this.layout, view);
    this.past = [];
    this.future = [];
    // Saved from its chart, a view opens as that chart; anything else, as its grid.
    this.view = view?.chart ? 'chart' : 'grid';
    this.result = null;
    this.ticket = 0;
    // Writing back: the transaction the edits are held in, a sandbox's where one is worked
    // in, the edits by cell, and the cells of the result that may be written, as "row,column".
    this.tx = null;
    this.resume(kept);
    this.edits = new Map();
    this.forgetWritable();
    // Cells held, which spreads leave as they are: by their place in the cube, so that a hold
    // outlasts sorting, pivoting and saving, until it is released.
    // A hold that no longer fits the cube, after a change to the model, is let go.
    const fits = (key) => {
      const at = JSON.parse(key);
      return at.length === cube.dimensions.length && at.every((name, index) => dims.get(cube.dimensions[index])?.byName.has(name));
    };
    this.holds = new Set(JSON.parse(localStorage.getItem(`epiphany.holds.${cube.name}`) ?? '[]').filter(fits));
    onPage = this;
    this.build(target);
    this.run();
  }

  /** Whether changes to the stored values are held, not saved; a sandbox keeps its own. */
  unsaved() {
    return Boolean(this.tx && !this.sandbox && this.changes);
  }

  /** Saves the changes held, as the strip's Save button does. */
  saveChanges() {
    return this.save();
  }

  /** Asked before the page is left: unsaved changes are discarded only if the user agrees. */
  async leave() {
    if (!this.tx || this.sandbox) return true;
    // A transaction whose changes were all taken back is let go without asking.
    if (!this.changes) {
      post(`/transactions/${this.tx}/rollback`).catch(() => {});
      this.forget();
      return true;
    }
    const agreed = await dialog({
      title: 'Discard your changes?',
      body: h('p', {}, `You have ${counted(this.changes || 0, 'unsaved change')} to ${this.cube.name}. Leaving discards them.`),
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
      // A layout kept when rows and columns were not told apart left out empty ones of both, or neither.
      if (typeof layout.nonEmpty !== 'object' || layout.nonEmpty === null) {
        layout.nonEmpty = { rows: Boolean(layout.nonEmpty), columns: Boolean(layout.nonEmpty) };
      }
      // Members that no longer exist are let go.
      for (const entry of [...layout.rows, ...layout.columns]) {
        const dim = this.dims.get(entry.dimension);
        entry.members = entry.members.filter((name) => dim.byName.has(name));
        entry.expanded = entry.expanded.filter((path) => namesOf(path).every((name) => dim.byName.has(name)));
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
      // Read as an outline where they are one, as Descendants gives them, so that the grid
      // indents them and marks the members it opens.
      for (const entry of entries) {
        const dim = this.dims.get(entry.dimension);
        const outline = dim && folded(entry.members, dim);
        if (outline) Object.assign(entry, outline);
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
      nonEmpty: emptiness(text),
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
    // Escape, anywhere in the panel, closes it, and the keys go back to the grid's cell.
    this.commentPanel = h('aside', {
      class: 'comment-panel',
      'aria-label': 'About the cell',
      hidden: true,
      onkeydown: (event) => {
        if (event.key !== 'Escape' || event.defaultPrevented) return;
        event.preventDefault();
        this.closeComments();
      },
    });
    this.pendingBar = h('div', { class: 'pending', role: 'status', 'aria-live': 'polite', hidden: true });
    // The cube explored, and the way to another, found by typing among however many, those
    // explored lately first.
    const cubes = chooser({
      items: () => lateFirst(this.definition.cubes.map((cube) => ({ label: cube.name, icon: 'cube', detail: cubeSaid(cube) })), 'cubes', { rest: 'Every cube' }),
      now: this.cube.name,
      label: 'Cube',
      chosen: (name) => go(`/explore?cube=${encodeURIComponent(name)}`),
    });
    cubes.title = 'Explore another cube';
    const viewButton = h('button', {
      class: 'button',
      'aria-haspopup': 'menu',
      onclick: (event) => {
        const button = event.currentTarget;
        working(button, () => this.viewsMenu(button));
      },
    }, icon('grid', 16), 'Views', icon('down', 14));
    // Where the values come from: the stored ones, or a sandbox of the user's.
    this.sandboxButton = h('button', {
      class: 'button sandbox-button',
      'aria-haspopup': 'menu',
      onclick: (event) => {
        const button = event.currentTarget;
        working(button, () => this.sandboxMenu(button));
      },
    });
    this.tabs = h('div');
    this.finder = this.buildFinder();
    const outputRow = h('div', { class: 'output-row' }, this.output, this.commentPanel);
    this.page = h('div', { class: 'explorer' },
      this.fields,
      h('section', { class: 'work' },
        // The cube's choice heads the page to the eye; this, to a screen reader.
        h('h1', { class: 'visually-hidden' }, `Explore ${this.cube.name}`),
        h('div', { class: 'work-head' },
          h('div', { class: 'toolbar' }, icon('cube', 20), cubes, viewButton, this.sandboxButton, this.finder.box, h('div', { class: 'grow' }), this.tabs),
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

  /**
   * The strip that says what is not yet saved, with the ways to save or discard it: in a
   * sandbox, what it holds, to publish or discard.
   */
  drawPending() {
    const sandbox = this.sandbox;
    this.titled(this.cube.name, this.unsaved());
    this.pendingBar.classList.toggle('sandbox', Boolean(sandbox));
    this.pendingBar.hidden = sandbox ? !sandbox.pending : !this.tx || !this.changes;
    if (this.pendingBar.hidden) return;
    if (sandbox) {
      const cells = counted(sandbox.pending, 'cell');
      mount(this.pendingBar,
        icon('sandbox', 16),
        h('span', { class: 'grow' }, `${sandbox.name} changes ${cells}, not yet published. Totals here include them; nobody else sees them until you publish.`),
        h('button', { class: 'button small', onclick: (event) => working(event.currentTarget, () => this.discard()) }, 'Discard…'),
        h('button', { class: 'button small primary', onclick: (event) => working(event.currentTarget, () => this.save()) }, icon('publish', 14), 'Publish'));
      return;
    }
    const count = this.changes || 0;
    mount(this.pendingBar,
      icon('edit', 16),
      h('span', { class: 'grow' }, `${counted(count, 'unsaved change')}. Totals already include them; nothing is stored until you save.`),
      h('button', { class: 'button small', onclick: (event) => working(event.currentTarget, () => this.discard()) }, 'Discard'),
      h('button', { class: 'button small primary', onclick: (event) => working(event.currentTarget, () => this.save()) }, icon('save', 14), 'Save changes'));
  }

  drawTabs() {
    choices({
      into: this.tabs,
      label: 'Show as',
      options: [['grid', 'Grid'], ['chart', 'Chart'], ['mdx', 'MDX']],
      chosen: this.view,
      choose: (id) => {
        this.view = id;
        this.drawTabs();
        this.draw();
      },
    });
  }

  drawToolbar() {
    this.drawPending();
    this.drawSandboxButton();
    const layout = this.layout;
    const button = (label, iconName, action, props = {}) => h('button', { class: 'button small quiet', onclick: action, ...props }, icon(iconName, 16), h('span', {}, label));
    // Undo and Redo by their marks alone, as over a dashboard edited, their names said pointed at.
    const mark = (label, iconName, action, props) => h('button', { class: 'button small quiet icon', 'aria-label': label, onclick: action, ...props }, icon(iconName, 16));
    mount(this.toolbar,
      mark('Undo', 'undo', () => this.undo(), { disabled: !this.past.length, title: `Undo ${said(this.past.at(-1))} (${MOD} Z)` }),
      mark('Redo', 'redo', () => this.redo(), { disabled: !this.future.length, title: `Redo ${said(this.future.at(-1))} (${MOD} Y)` }),
      h('span', { class: 'divider' }),
      button('Swap axes', 'swap', () => this.change((l) => {
        [l.rows, l.columns] = [l.columns, l.rows];
        l.sort = null;
        l.top = null;
      })),
      // Rows with no values, and columns, are left out each on their own.
      h('div', { class: 'toggle-group', role: 'group', 'aria-label': 'Hide empty rows and columns' },
        h('span', { class: 'toggle-label', 'aria-hidden': 'true' }, 'Hide empty'),
        ['rows', 'columns'].map((axis) => button(axis === 'rows' ? 'Rows' : 'Columns', layout.nonEmpty?.[axis] ? 'eyeOff' : 'eye',
          () => this.change((l) => {
            l.nonEmpty = { ...l.nonEmpty, [axis]: !l.nonEmpty?.[axis] };
          }), {
            'aria-pressed': String(Boolean(layout.nonEmpty?.[axis])),
            'aria-label': `Hide empty ${axis}`,
            title: layout.nonEmpty?.[axis] ? `Show the ${axis} with no values again` : `Leave out the ${axis} with no values`,
          }))),
      layout.sort ? button('Clear sort', 'close', () => this.change((l) => {
        l.sort = null;
      })) : null,
      layout.top ? button('Show every row', 'close', () => this.change((l) => {
        l.top = null;
      }), { title: `Showing the ${counted(layout.top.count, `${layout.top.bottom ? 'smallest' : 'largest'} row`)} only` }) : null,
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
    const list = h('div', { class: 'field-list', role: 'list', 'aria-label': 'Dimensions' });
    const draw = () => {
      const words = search.value.trim().toLowerCase();
      mount(list, this.cube.dimensions.map((name) => {
        const dim = this.dims.get(name);
        // The first eight found: the rest of a large dimension is not looked through.
        const matches = [];
        for (const element of words ? dim.elements : []) {
          if (element.name.toLowerCase().includes(words) && matches.push(element) === 8) break;
        }
        if (words && !name.toLowerCase().includes(words) && !matches.length) return null;
        const place = this.placeOf(name);
        // Dragged by the mouse; its Place button is the way to the same places by the keys.
        return h('div', { class: 'field', role: 'listitem' },
          h('div', {
            class: 'field-row',
            draggable: 'true',
            ondragstart: (event) => {
              event.dataTransfer.setData('text/x-epiphany-dimension', name);
              event.dataTransfer.effectAllowed = 'move';
              this.page.classList.add('dragging');
            },
            ondragend: () => this.page.classList.remove('dragging'),
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
      h('div', { class: 'fields-head' }, h('h3', {}, 'Fields'), h('span', { class: 'muted' }, counted(this.cube.dimensions.length, 'dimension'))),
      h('div', { class: 'search' }, icon('search', 16), search),
      list,
      h('p', { class: 'hint fields-hint' }, 'Drag a dimension to Rows, Columns or Filters, or choose where it goes with the ⋯ button beside it. Drag one back here to leave it out of the query: it then stands at its first top element.'));
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
    // Each member is opened where it stands: one under two members it opens under both.
    const chosen = entry.members.map((name) => [name]);
    const within = pathsBeneath(dim, chosen);
    const closed = shown(entry, dim).filter((row) => !row.opened && this.childrenOf(dim, row.name).length).map((row) => row.path);
    const levels = [...new Set(within.map((path) => dim.byName.get(namesOf(path).at(-1)).level))].sort((a, b) => b - a);
    return [
      { label: 'Open one level more', icon: 'plus', disabled: !closed.length, action: () => this.setOpen(place, dimension, (expanded) => [...expanded, ...closed]) },
      { label: 'Open every level', icon: 'expand', disabled: within.every((path) => entry.expanded.includes(path)), action: () => this.setOpen(place, dimension, () => within) },
      ...levels.slice(1).map((level) => ({
        label: `Open down to ${dim.levels?.[level] || `level ${level}`}`,
        icon: 'level',
        action: () => this.setOpen(place, dimension, () => pathsBeneath(dim, chosen, (element) => element.level > level)),
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
      if (path) entry.expanded = [...new Set([...entry.expanded, ...path.map((_, index) => pathKey(path.slice(0, index + 1)))])];
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

  /**
   * The members a dimension's subset selects now: for one that reads cells, as this
   * exploration sees them, its transaction's or sandbox's values with the rest.
   */
  async subsetNow(dimension, subset) {
    const query = `SELECT TM1SubsetToSet(${quote(dimension)}, '${subset.replaceAll('\'', '\'\'')}') ON COLUMNS FROM ${quote(this.cube.name)}`;
    const answer = await post('/mdx', { mdx: query, transaction: this.tx ?? undefined }, { quiet: true });
    return answer.axes[0].tuples.map((tuple) => tuple[0].name);
  }

  async pick(name) {
    const dim = this.dims.get(name);
    const place = this.placeOf(name);
    if (place !== 'filters') {
      // A dimension on an axis gets the set editor, and keeps the order built there.
      const entry = this.layout[place].find((candidate) => candidate.dimension === name);
      const set = await editSet({ ...dim, name }, entry.members, (subset) => this.subsetNow(name, subset));
      if (!set) return;
      this.change((layout) => {
        const target = layout[place].find((candidate) => candidate.dimension === name);
        target.members = set;
        target.expanded = target.expanded.filter((path) => set.includes(namesOf(path)[0]));
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
    if (this.layout.mdx && this.elsewhere(text)) return;
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
    const asking = { quiet: true, signal: stopping.signal };
    try {
      // In a sandbox the stored answer comes beside its own, for the cells it changes to be
      // marked, and the sandbox's count of them with it.
      const [result, stored, kept] = await Promise.all([
        post('/mdx', { mdx: text, transaction: this.tx ?? undefined }, asking),
        this.sandbox ? post('/mdx', { mdx: text }, asking) : null,
        this.sandbox ? get('/sandboxes', asking) : null,
      ]);
      if (ticket !== this.ticket) return;
      this.result = result;
      this.error = null;
      this.took = performance.now() - started;
      this.forgetWritable();
      this.compareStored(result, stored);
      if (kept) this.takeSandboxes(kept);
      if (this.layout.mdx) this.adopt(result);
    } catch (problem) {
      if (ticket !== this.ticket) return;
      if (problem.code === 'stopped') {
        this.stopped();
        return;
      }
      if (problem.code === 'unknown_transaction' && this.sandbox) {
        this.lostSandbox();
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
    // The numbers show at once; the comments mark their cells as they come.
    if (this.result && (await this.loadComments()) && ticket === this.ticket) {
      this.markAllComments();
      if (this.commentCell) this.drawComments();
    }
  }

  /**
   * Opens the page of the cube a written query asks of, where that is another cube of the
   * model, and runs the query there; says whether it did. This page goes back to its own
   * layout's query, so that it keeps to its cube. A cube the model lacks is left to the
   * server to say so.
   */
  elsewhere(text) {
    const asked = cubeOf(text)?.toLowerCase();
    const cube = this.definition.cubes.find((each) => each.name.toLowerCase() === asked);
    if (!cube || cube.name === this.cube.name) return false;
    this.layout.mdx = null;
    localStorage.setItem(saved(this.cube.name), JSON.stringify(this.layout));
    handOver(cube.name, text);
    notify(`The query asks of ${cube.name}, so it runs there.`);
    go(`/explore?cube=${encodeURIComponent(cube.name)}`);
    return true;
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
      // A query the server could not run would fail the same run again, so it is not offered
      // again; one written by hand goes back to be mended, where it says the problem is if
      // it says. One laid out is changed in the layout.
      const refused = this.error instanceof ApiError && this.error.status === 422;
      const mendable = refused && Boolean(this.layout.mdx);
      const at = mendable ? this.error.position ?? null : null;
      const show = mendable ? h('button', { class: 'button primary', onclick: () => this.showInQuery(at) }, icon('code', 16), at == null ? 'Edit the query' : 'Show it in the query') : null;
      mount(this.output, failure(this.error, refused ? null : () => this.run(), show, refused ? 'The query could not be run' : null));
      mount(this.statusLine);
      return;
    }
    if (!this.result) {
      const missing = !this.layout.rows.length ? 'rows' : 'columns';
      mount(this.output, empty('grid', `Put a dimension on ${missing}`, 'Drag one from the field list, or choose where it goes with the ⋯ button beside it.'));
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

  /** Opens a member where it stands, by its path from the member chosen, or closes it there. */
  toggle(place, dimName, path) {
    this.change((layout) => {
      const entry = layout[place].find((candidate) => candidate.dimension === dimName);
      entry.expanded = entry.expanded.includes(path)
        ? entry.expanded.filter((candidate) => candidate !== path)
        : [...entry.expanded, path];
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
    const data = shape(this.result, settings.byColumns);
    const plotted = data.series.some((entry) => entry.values.some((value) => value !== null));
    // Any series may be drawn as a line over the rest, or against a scale at the right.
    const names = AXES.has(settings.kind) ? seriesNames(data) : [];
    const drawnAs = async () => {
      const drawn = await askDrawn(settings.kind, names, settings);
      if (drawn) set(drawn);
    };
    mount(this.output, h('div', { class: 'chart-pane' },
      h('div', { class: 'toolbar' },
        choices({
          kind: 'radiogroup',
          label: 'Kind of chart',
          options: [['bars', 'Bars'], ['stacked', 'Stacked'], ['shares', '100%'], ['lines', 'Lines'], ['area', 'Area'], ['ranked', 'Ranked'], ['donut', 'Donut']],
          chosen: settings.kind,
          choose: (kind) => set({ kind }),
        }),
        choices({
          kind: 'radiogroup',
          label: 'Series from',
          options: [[false, 'A series for each row'], [true, 'A series for each column']],
          chosen: Boolean(settings.byColumns),
          // Its series turned the other way are others: none of them is drawn as a line.
          choose: (byColumns) => set({ byColumns, lines: [], second: [] }),
        }),
        h('button', { class: 'button small quiet', 'aria-pressed': String(Boolean(settings.labels)), onclick: () => set({ labels: !settings.labels }) }, icon('hash', 14), 'Values'),
        names.length > 1 ? h('button', { class: 'button small quiet', title: 'How its series are drawn', onclick: drawnAs }, icon('explore', 14), 'Series…') : null),
      plotted
        ? chart(data, settings.kind, {
          labels: settings.labels,
          lines: settings.lines,
          second: settings.second,
          format: (value) => formatValue(value, this.layout.format),
          tone: this.layout.highlights?.rules?.length ? (value) => toneOf(this.layout.highlights, value) : null,
        })
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
        if (!(n >= 1)) throw invalid(count, 'Keep at least one row.');
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
        if (!label) throw invalid(named, 'Give the calculation a name.');
        if (dim.byName.has(label) || this.calculated(dimension, label)) throw invalid(named, `${dimension} has a member called ${label} already.`);
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
    const addRule = h('button', { class: 'button small', type: 'button', onclick: () => { rules.push({ test: 'below', value: 0, tone: 'bad' }); draw(); } }, icon('plus', 14), 'Add a rule');
    const TESTS = [['above', 'is above'], ['at_least', 'is at least'], ['below', 'is below'], ['at_most', 'is at most'], ['equals', 'equals']];
    const TONES = [['good', 'Good, in green'], ['bad', 'Bad, in red'], ['warn', 'Watch, in amber'], ['accent', 'Note, in indigo']];
    const draw = () => mount(list, rules.length ? rules.map((rule, index) => h('div', { class: 'rule' },
      h('span', { class: 'rule-join muted' }, index === 0 ? 'A number that' : 'or else'),
      h('select', { class: 'select', 'aria-label': 'Test', onchange: (event) => { rule.test = event.target.value; } },
        TESTS.map(([value, label]) => h('option', { value, selected: rule.test === value }, label))),
      h('input', { class: 'input', 'aria-label': 'Value', inputmode: 'decimal', value: String(rule.value), onchange: (event) => { rule.value = parseNumber(event.target.value); } }),
      h('select', { class: 'select', 'aria-label': 'Colour', onchange: (event) => { rule.tone = event.target.value; } },
        TONES.map(([value, label]) => h('option', { value, selected: rule.tone === value }, label))),
      h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Remove this rule', onclick: () => { rules.splice(index, 1); draw(); refocus(list, index, addRule); } }, icon('close', 14))))
      : h('p', { class: 'hint' }, 'No rules: add one to colour the numbers that meet it.'));
    draw();
    const chosen = await dialog({
      title: 'Highlights',
      confirm: 'Apply',
      wide: false,
      body: h('div', { class: 'facts' },
        h('label', { class: 'check' }, bars, 'Data bars: each number\'s share of the largest in its column'),
        h('label', { class: 'check' }, shades, 'Shades: from the lowest number in each column to the highest'),
        h('fieldset', { class: 'field' }, h('legend', {}, 'Colours by rule: the first rule a number meets decides'), list,
          h('div', { class: 'toolbar' }, addRule)),
        h('p', { class: 'hint' }, 'Totals take no bar or shade, being out of scale with what they add up. A view saved keeps its highlights.')),
      onsubmit: () => {
        const unnumbered = rules.findIndex((rule) => rule.value === null || !Number.isFinite(rule.value));
        if (unnumbered >= 0) throw invalid(list.children[unnumbered].querySelector('input'), 'Give every rule a number.');
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

  /* ---- Views: the user's own, and everyone's, kept with the cube ---- */

  /**
   * The views to open: the user's own, which only they see, then the cube's, which everyone
   * does; and saving the query shown as one. A server without users keeps no views of
   * anybody's own, and a view as another user changes nothing of theirs.
   */
  async viewsMenu(button) {
    const keepsOwn = !state.open;
    const viewing = Boolean(state.user?.viewer);
    let own = {};
    let unfetched = false;
    if (keepsOwn) {
      try {
        own = await get(`/cubes/${part(this.cube.name)}/private-views`, { quiet: true });
      } catch {
        unfetched = true;
      }
    }
    const open = (view) => () => {
      this.view = view.chart ? 'chart' : 'grid';
      this.drawTabs();
      this.change((layout) => opened(layout, view), true);
    };
    const mine = Object.entries(own);
    const everyones = Object.entries(this.cube.views ?? {});
    const saving = !viewing && (keepsOwn || state.user?.role === 'admin');
    const actions = [
      { label: 'Copy a link to this query', icon: 'link', disabled: !this.text, action: () => copyText(linkOf(this.cube.name, this.asView())) },
      saving ? { label: 'Save this query as a view…', icon: 'save', disabled: !this.text, action: () => this.saveView() } : null,
      !viewing && state.user?.role === 'admin' ? { label: 'Pin to a dashboard…', icon: 'dashboard', disabled: !this.text, action: () => this.pin() } : null,
      saving && mine.length ? { label: 'Take one of your views away…', icon: 'trash', action: () => this.removeOwnViews(mine) } : null,
    ];
    // Each view marked as it opens: as the chart it was saved from, or as its grid.
    const kind = (view) => KINDS[view.chart?.kind]?.[1] ?? 'grid';
    // More than a menu holds at a glance, they are found by typing, and offered by the dozen.
    if (mine.length + everyones.length > MENU_MOST) {
      const items = [
        ...mine.map(([name, view]) => ({ label: name, icon: kind(view), group: 'Your views', view })),
        ...everyones.map(([name, view]) => ({ label: name, icon: kind(view), group: 'Everyone\'s views', view })),
      ];
      const fetched = unfetched ? { label: 'Your views could not be fetched', icon: 'alert', disabled: true, action: () => {} } : null;
      // The button is at work until the list opens, not while it is open.
      pick({ items, at: button, label: `Find a view of ${this.cube.name}`, actions: [fetched, ...actions] }).then((chosen) => chosen && open(chosen.view)());
      return;
    }
    const item = ([name, view]) => ({ label: name, icon: kind(view), action: open(view) });
    menu([
      mine.length ? { heading: 'Your views' } : null,
      ...mine.map(item),
      unfetched ? { heading: 'Your views could not be fetched' } : null,
      { heading: everyones.length ? 'Everyone\'s views' : 'No views for everyone yet' },
      ...everyones.map(item),
      '-',
      ...actions,
    ], button);
  }


  /**
   * The query shown, with the sizes, number format and highlights it is shown with, and the
   * chart it is shown as, where it is.
   */
  asView() {
    const geometry = this.layout.geometry;
    const sized = geometry && (geometry.row_height || Object.keys(geometry.widths ?? {}).length)
      ? { geometry: { ...(geometry.row_height ? { row_height: geometry.row_height } : {}), widths: geometry.widths ?? {} } }
      : {};
    const format = this.layout.format ? { format: this.layout.format } : {};
    const highlights = this.layout.highlights ? { highlights: this.layout.highlights } : {};
    const chart = this.view === 'chart' ? { chart: chartOf(this.layout.chart) } : {};
    return { mdx: this.text, ...sized, ...format, ...highlights, ...chart };
  }

  /**
   * Saves the query shown as a view: the user's own, on a server with users, or with the
   * cube for everyone, which takes an administrator, who chooses which.
   */
  async saveView() {
    const keepsOwn = !state.open;
    const shares = state.user?.role === 'admin';
    const name = h('input', { class: 'input', id: 'view-name', required: true, maxlength: 120 });
    const own = h('input', { type: 'radio', name: 'view-whose', checked: true });
    const everyone = h('input', { type: 'radio', name: 'view-whose' });
    const whose = keepsOwn && shares
      ? h('fieldset', { class: 'field' }, h('legend', {}, 'Who sees it'),
        h('label', { class: 'check' }, own, 'Only you'),
        h('label', { class: 'check' }, everyone, `Everyone who may read ${this.cube.name}`))
      : null;
    const saved = await dialog({
      title: 'Save as a view',
      confirm: 'Save',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'view-name' }, 'Name'), name),
        whose,
        h('p', { class: 'hint' }, keepsOwn && !shares
          ? 'Kept for you alone: nobody else sees it. A view of yours of the same name is replaced.'
          : 'A view of the same name is replaced.')),
      onsubmit: async () => {
        const view = name.value.trim();
        if (!view) throw invalid(name, 'Give the view a name.');
        if (keepsOwn && (!shares || own.checked)) {
          await put(`/cubes/${part(this.cube.name)}/private-views/${part(view)}`, this.asView());
          return `Saved the view “${view}” for you alone.`;
        }
        // The cube as it is now, saved against the version it was read at, so that a change
        // someone else made meanwhile is not overwritten.
        const cube = await get(`/cubes/${part(this.cube.name)}`);
        const views = { ...(cube.views ?? {}), [view]: this.asView() };
        await put(`/cubes/${part(this.cube.name)}`, { based_on: Number(modelVersion), cube: { ...cube, views } });
        this.cube.views = views;
        return `Saved the view “${view}” for everyone.`;
      },
    });
    if (saved) notify(saved, 'good');
  }

  /**
   * Pins the query shown to a dashboard, as Power BI pins a visual: saved as a view for
   * everyone, under the name given, and a tile showing it added to the dashboard named, or to
   * a new one of that name. The tile shows it as the page does: as the chart shown, or a grid.
   */
  async pin() {
    const dashboards = (await model()).dashboards ?? [];
    const name = h('input', { class: 'input', id: 'pin-view', required: true, maxlength: 120 });
    const board = h('input', { class: 'input', id: 'pin-dashboard', required: true, maxlength: 120, list: 'pin-dashboards' });
    const shown = this.view === 'chart' ? this.layout.chart?.kind ?? 'bars' : 'grid';
    const show = h('select', { class: 'select', id: 'pin-show' },
      Object.entries(KINDS).filter(([kind]) => kind !== 'text').map(([kind, [label]]) => h('option', { value: kind, selected: kind === shown }, label)));
    board.value = dashboards[0]?.name ?? '';
    const pinned = await dialog({
      title: 'Pin to a dashboard',
      confirm: 'Pin',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'pin-view' }, 'The view it shows'), name,
          h('span', { class: 'hint' }, `Saved with ${this.cube.name} for everyone. A view of the same name is replaced.`)),
        h('div', { class: 'field' }, h('label', { for: 'pin-dashboard' }, 'The dashboard'), board,
          h('datalist', { id: 'pin-dashboards' }, dashboards.map((dashboard) => h('option', { value: dashboard.name }))),
          h('span', { class: 'hint' }, 'A name no dashboard has makes a new one.')),
        h('div', { class: 'field' }, h('label', { for: 'pin-show' }, 'Shown as'), show)),
      onsubmit: async () => {
        const view = name.value.trim();
        if (!view) throw invalid(name, 'Give the view a name.');
        const to = board.value.trim();
        if (!to) throw invalid(board, 'Name the dashboard.');
        // The view, saved with the cube as it is now, against the version it was read at.
        const cube = await get(`/cubes/${part(this.cube.name)}`);
        const views = { ...(cube.views ?? {}), [view]: this.asView() };
        await put(`/cubes/${part(this.cube.name)}`, { based_on: Number(modelVersion), cube: { ...cube, views } });
        this.cube.views = views;
        // The tile, added to the dashboard as it is now, where the board has room for it.
        const outline = await get('/model/outline');
        // Charted, it is drawn as here: its series the same way, its values on the marks if
        // they are here, and the series drawn as lines here drawn so.
        const settings = this.layout.chart ?? {};
        const charted = CHARTS.has(show.value);
        const axes = AXES.has(show.value);
        const tile = {
          cube: this.cube.name,
          view,
          show: show.value,
          ...(charted && settings.byColumns ? { by_columns: true } : {}),
          ...(charted && settings.labels ? { labels: true } : {}),
          ...(axes && settings.lines?.length ? { lines: settings.lines } : {}),
          ...(axes && settings.second?.length ? { second_axis: settings.second } : {}),
        };
        const had = outline.dashboards.find((dashboard) => dashboard.name.toLowerCase() === to.toLowerCase());
        const dashboard = had ? { ...had, tiles: [...had.tiles, tile] } : { name: to, tiles: [tile] };
        await put(`/dashboards/${part(dashboard.name)}`, { based_on: outline.version, dashboard });
        return dashboard.name;
      },
    });
    if (!pinned) return;
    notify(h('span', {}, `Pinned to ${pinned}. `, h('a', { href: `/dashboards?name=${encodeURIComponent(pinned)}`, 'data-link': '' }, 'Open it')), 'good');
  }

  /** Lists the user's own views of the cube, each to be taken away. */
  async removeOwnViews(mine) {
    const list = h('ul', { class: 'history-list' });
    const none = () => h('li', { class: 'muted' }, 'You keep no views of this cube now.');
    mount(list, mine.map(([name]) => {
      const item = h('li', {},
        h('span', { class: 'grow' }, name),
        h('button', {
          class: 'button small danger',
          type: 'button',
          'aria-label': `Take away the view ${name}`,
          onclick: (event) => working(event.currentTarget, async () => {
            try {
              await del(`/cubes/${part(this.cube.name)}/private-views/${part(name)}`);
            } catch (problem) {
              notify(problem.message, 'bad');
              return;
            }
            // The keys go on to the view after, or before, or to Done.
            const next = item.nextElementSibling ?? item.previousElementSibling;
            item.remove();
            if (!list.children.length) list.append(none());
            (next?.querySelector('button') ?? list.closest('dialog')?.querySelector('button[type="submit"]'))?.focus();
            notify(`Took away the view “${name}”.`, 'good');
          }),
        }, 'Take away'));
      return item;
    }));
    await dialog({ title: `Your views of ${this.cube.name}`, confirm: 'Done', cancel: null, body: list });
  }

  /* ---- MDX ---- */

  /** Opens the query where the server found a problem in it, at `position`, with what it said. */
  showInQuery(position) {
    this.mended = { position, message: this.error.message };
    this.view = 'mdx';
    this.drawTabs();
    this.draw();
  }

  drawMdx() {
    const editor = h('textarea', { class: 'textarea mdx-editor', spellcheck: 'false', 'aria-label': 'MDX query', rows: 14 }, this.text ?? '');
    editor.value = this.text ?? '';
    // Where the server found a problem: the character chosen, and what it said beneath, until
    // the query is changed. Counted in characters, which the text counts in halves of some.
    const problem = this.mended;
    this.mended = null;
    const said = problem ? h('p', { class: 'error-text', id: 'mdx-problem', role: 'alert' }, problem.message) : null;
    if (problem) {
      // Where the server said, or at the end where it said nothing of where.
      const at = problem.position == null ? editor.value.length : [...editor.value].slice(0, problem.position).join('').length;
      requestAnimationFrame(() => {
        editor.focus();
        editor.setSelectionRange(at, Math.min(at + 1, editor.value.length));
      });
      editor.setAttribute('aria-invalid', 'true');
      editor.setAttribute('aria-describedby', 'mdx-problem');
      editor.addEventListener('input', () => {
        said.remove();
        editor.removeAttribute('aria-invalid');
        editor.removeAttribute('aria-describedby');
      }, { once: true });
    }
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
      said,
      h('div', { class: 'toolbar' },
        h('button', { class: 'button primary', onclick: run }, icon('play', 16), 'Run'),
        h('span', { class: 'hint' }, `${MOD} Enter runs it`),
        h('div', { class: 'grow' }),
        h('button', { class: 'button', onclick: () => copyText(editor.value) }, icon('copy', 16), 'Copy'))));
    mount(this.statusLine);
  }

  /* ---- Export ---- */
  exportCsv() {
    download(`${this.cube.name}.csv`, csvOf(this.tableOf()), 'text/csv');
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
Object.assign(Explorer.prototype, grid, writeback, comments, sandboxes);
