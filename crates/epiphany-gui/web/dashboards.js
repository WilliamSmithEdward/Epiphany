// Dashboards: pages of saved views, kept with the model. Each tile runs a view and shows it
// as a key figure, a chart or a small grid. Everyone who may read sees them; administrators
// make them and change them, and a change is saved against the model version it was made
// from, so that nobody's change is overwritten unseen.

import { del, modelVersion, part, post, put } from './api.js';
import { chart, shape, sparkline } from './chart.js';
import { empty, failure, go, model, state } from './app.js';
import { chooseMember, dimension } from './picker.js';
import { dialog, formatValue, h, icon, menu, mount, notify, working } from './ui.js';

/** The ways a tile shows its view. */
const SHOWS = { figure: 'Key figure', bars: 'Bars', stacked: 'Stacked bars', lines: 'Lines', grid: 'Grid' };

/** How many rows a grid tile shows before it says there are more. */
const GRID_ROWS = 60;

export async function render(target, { params }) {
  const definition = await model();
  return new Dashboards(target, definition, params.get('name'));
}

class Dashboards {
  constructor(target, definition, name) {
    this.target = target;
    this.definition = definition;
    this.admin = state.user?.role === 'admin';
    // The dashboard being edited, a copy, and whether it is new; null when only shown.
    this.editing = null;
    this.fresh = false;
    this.name = this.list().find((dashboard) => dashboard.name === name)?.name ?? this.list()[0]?.name ?? null;
    this.draw();
  }

  list() {
    return this.definition.dashboards ?? [];
  }

  current() {
    return this.list().find((dashboard) => dashboard.name === this.name) ?? null;
  }

  /** The members chosen for the page's filters, by dimension, kept in this browser. */
  chosen() {
    try {
      return JSON.parse(localStorage.getItem(`epiphany.dashboard.${this.name}`)) ?? {};
    } catch {
      return {};
    }
  }

  choose(dimensionName, member) {
    const chosen = { ...this.chosen(), [dimensionName]: member };
    if (member === null) delete chosen[dimensionName];
    localStorage.setItem(`epiphany.dashboard.${this.name}`, JSON.stringify(chosen));
    this.draw();
  }

  /** The members the page's filters have a tile stand at: those chosen, of its cube's dimensions. */
  standing(tile) {
    const chosen = this.chosen();
    return ((this.editing ?? this.current())?.filters ?? [])
      .filter((dimensionName) => chosen[dimensionName] && this.cubeHas(tile.cube, dimensionName))
      .map((dimensionName) => ({ dimension: dimensionName, member: chosen[dimensionName] }));
  }

  cubeHas(cubeName, dimensionName) {
    return Boolean(this.definition.cubes.find((cube) => cube.name.toLowerCase() === cubeName.toLowerCase())?.dimensions.includes(dimensionName));
  }

  /** The filters above the tiles: a member chosen for each, which every tile that can stands at. */
  drawFilters(shown) {
    const filters = shown.filters ?? [];
    const chosen = this.chosen();
    if (!filters.length && !this.editing) return null;
    const chip = (dimensionName) => h('div', { class: 'chip dash-filter' },
      h('button', {
        class: 'chip-main',
        title: `Show every tile at a member of ${dimensionName}`,
        onclick: async (event) => {
          const dim = await working(event.currentTarget, () => dimension(dimensionName));
          if (!dim) return;
          const member = await chooseMember(dim, { title: `Show every tile at a member of ${dimensionName}`, current: chosen[dimensionName] });
          if (member !== null) this.choose(dimensionName, member);
        },
      }, h('strong', {}, dimensionName), h('span', { class: 'chip-summary' }, chosen[dimensionName] ?? 'As each view has it')),
      this.editing
        ? h('button', {
          class: 'chip-more',
          'aria-label': `Take the filter by ${dimensionName} off the dashboard`,
          onclick: () => {
            this.editing.filters = filters.filter((other) => other !== dimensionName);
            this.draw();
          },
        }, icon('close', 14))
        : chosen[dimensionName] ? h('button', { class: 'chip-more', 'aria-label': `Show the tiles as their views have ${dimensionName}`, title: 'As each view has it', onclick: () => this.choose(dimensionName, null) }, icon('close', 14)) : null);
    const offered = [...new Set(shown.tiles.flatMap((tile) => this.definition.cubes.find((cube) => cube.name.toLowerCase() === tile.cube.toLowerCase())?.dimensions ?? []))]
      .filter((dimensionName) => !filters.includes(dimensionName));
    return h('div', { class: 'dash-filters', role: 'group', 'aria-label': 'Filters for every tile' },
      icon('filter', 16),
      filters.map(chip),
      this.editing ? h('button', {
        class: 'button small quiet',
        disabled: !offered.length,
        title: offered.length ? 'Offer a choice of member that every tile stands at' : 'Add tiles first: a filter is a dimension of their cubes',
        onclick: (event) => menu([
          { heading: 'Filter every tile by' },
          ...offered.map((dimensionName) => ({
            label: dimensionName,
            icon: 'dimension',
            action: () => {
              this.editing.filters = [...filters, dimensionName];
              this.draw();
            },
          })),
        ], event.currentTarget),
      }, icon('plus', 14), 'Add a filter') : null);
  }

  /** Asked before the page is left: unsaved edits are discarded only if the user agrees. */
  async leave() {
    if (!this.changed()) return true;
    return Boolean(await dialog({
      title: 'Discard your changes?',
      body: h('p', {}, `Your changes to ${this.editing.name} are not saved. Leaving discards them.`),
      confirm: 'Discard and leave',
      cancel: 'Stay',
      danger: true,
    }));
  }

  changed() {
    return Boolean(this.editing) && (this.fresh || JSON.stringify(this.editing) !== JSON.stringify(this.current()));
  }

  draw() {
    const shown = this.editing ?? this.current();
    if (!shown) {
      mount(this.target, h('div', { class: 'page' },
        h('div', { class: 'page-head' }, h('div', { class: 'grow' }, h('h1', {}, 'Dashboards'))),
        h('div', { class: 'card' }, empty('dashboard', 'No dashboards yet',
          this.admin ? 'A dashboard gathers saved views, each shown as a key figure, a chart or a grid. Save views in Explore, then make a dashboard of them.' : 'An administrator makes dashboards from views saved in Explore.',
          this.admin ? h('button', { class: 'button primary', onclick: () => this.create() }, icon('plus', 16), 'New dashboard') : null))));
      return;
    }
    const editing = Boolean(this.editing);
    const tabs = h('div', { class: 'dash-tabs', role: 'tablist', 'aria-label': 'Dashboards' }, this.list().map((dashboard) => h('a', {
      role: 'tab',
      href: `/dashboards?name=${encodeURIComponent(dashboard.name)}`,
      'data-link': '',
      'aria-selected': String(dashboard.name === this.name),
    }, dashboard.name)));
    const actions = editing
      ? [
        h('button', { class: 'button', onclick: () => this.addTile() }, icon('plus', 16), 'Add a tile'),
        h('button', { class: 'button', onclick: () => this.stopEditing() }, 'Cancel'),
        h('button', { class: 'button primary', onclick: (event) => working(event.currentTarget, () => this.save()) }, icon('save', 16), 'Save'),
      ]
      : [
        h('button', {
          class: 'button quiet',
          title: 'Run every tile again',
          onclick: (event) => working(event.currentTarget, () => Promise.all(this.filling.map((fill) => fill()))),
        }, icon('refresh', 16), 'Refresh'),
        this.admin ? h('button', { class: 'button', onclick: () => this.edit() }, icon('edit', 16), 'Edit') : null,
        this.admin ? h('button', { class: 'button', onclick: () => this.create() }, icon('plus', 16), 'New') : null,
        this.admin ? h('button', { class: 'button danger quiet', onclick: () => this.remove() }, icon('trash', 16), 'Delete') : null,
      ];
    const head = editing
      ? h('div', { class: 'grow dash-names' },
        h('input', { class: 'input dash-name', 'aria-label': 'Name of the dashboard', value: shown.name, oninput: (event) => { this.editing.name = event.target.value; } }),
        h('input', { class: 'input', 'aria-label': 'What the dashboard is for', placeholder: 'What it is for, in a line', value: shown.description ?? '', oninput: (event) => { this.editing.description = event.target.value; } }))
      : h('div', { class: 'grow' }, h('h1', {}, shown.name), shown.description ? h('p', {}, shown.description) : null);
    // How to run each tile again, for Refresh.
    this.filling = [];
    const tiles = shown.tiles.map((tile, index) => this.tile(tile, index));
    mount(this.target, h('div', { class: `page dashboard${editing ? ' editing' : ''}` },
      this.list().length > 1 && !editing ? tabs : null,
      h('div', { class: 'page-head' }, head, h('div', { class: 'toolbar' }, actions)),
      this.drawFilters(shown),
      tiles.length
        ? h('div', { class: 'dash-grid' }, tiles)
        : h('div', { class: 'card' }, empty('dashboard', 'No tiles yet', editing ? 'Add a tile to show a saved view.' : 'An administrator adds tiles that show saved views.',
          editing ? h('button', { class: 'button primary', onclick: () => this.addTile() }, icon('plus', 16), 'Add a tile') : null))));
  }

  /* ---- Tiles ---- */

  tile(tile, index) {
    const view = this.definition.cubes.find((cube) => cube.name === tile.cube)?.views?.[tile.view];
    const title = tile.title || tile.view;
    const standing = this.standing(tile).map((at) => at.member);
    // Until its answer comes, a tile holds the outline of what it shows.
    const body = h('div', { class: 'tile-body' }, h('div', { class: `skeleton ${tile.show ?? 'bars'}` }));
    const open = `/explore?cube=${encodeURIComponent(tile.cube)}&view=${encodeURIComponent(tile.view)}`;
    const card = h('section', {
      class: `card dash-tile${tile.width === 2 ? ' wide' : ''}`,
      'aria-label': title,
      dataset: { index },
    },
    h('div', { class: 'tile-head' },
      h('div', { class: 'grow' }, h('h3', {}, title), h('p', { class: 'muted' }, `${tile.cube} · ${tile.view}`,
        standing.length ? h('span', { class: 'tile-standing' }, ` · at ${standing.join(', ')}`) : null)),
      this.editing ? this.tileControls(tile, index) : h('a', { class: 'button small quiet icon', href: open, 'data-link': '', title: 'Open in Explore', 'aria-label': `Open ${title} in Explore` }, icon('explore', 16))),
    body);
    if (this.editing) this.draggable(card, index);
    const fill = () => this.fill(body, tile, view);
    this.filling.push(fill);
    fill();
    return card;
  }

  /**
   * Runs a tile's view and shows the answer as the tile asks. Run again, a tile keeps its
   * last answer, fading after a moment, until the new one comes.
   */
  async fill(body, tile, view) {
    if (!view) {
      mount(body, empty('alert', 'The view is gone', `${tile.cube} has no view called ${tile.view}. It may have been renamed or removed.`));
      return;
    }
    body.setAttribute('aria-busy', 'true');
    body.classList.add('busy');
    try {
      const result = await post('/mdx', { mdx: view.mdx, context: this.standing(tile) }, { quiet: true });
      mount(body, this.show(result, tile.show ?? 'bars', view.format));
    } catch (problem) {
      mount(body, failure(problem, () => this.fill(body, tile, view)));
    }
    body.classList.remove('busy');
    body.removeAttribute('aria-busy');
  }

  /** A result as a tile shows it, its numbers as the view's format has them. */
  show(result, how, format) {
    if (how === 'figure') return figure(result, format);
    if (how === 'grid') return grid(result, format);
    const data = shape(result, false);
    if (!data.series.some((entry) => entry.values.some((value) => value !== null))) {
      return empty('chart', 'Nothing to chart', 'The view has no numbers.');
    }
    return chart(data, how, { height: 260 });
  }

  /* ---- Editing ---- */

  edit() {
    this.editing = structuredClone(this.current());
    this.fresh = false;
    this.draw();
  }

  stopEditing() {
    this.editing = null;
    this.fresh = false;
    this.draw();
  }

  async create() {
    const name = h('input', { class: 'input', id: 'dashboard-name', required: true, maxlength: 120 });
    const description = h('input', { class: 'input', id: 'dashboard-description', maxlength: 240 });
    const made = await dialog({
      title: 'New dashboard',
      confirm: 'Make it',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'dashboard-name' }, 'Name'), name),
        h('div', { class: 'field' }, h('label', { for: 'dashboard-description' }, 'What it is for'), description)),
      onsubmit: () => {
        const value = name.value.trim();
        if (!value) throw new Error('Give the dashboard a name.');
        if (this.list().some((dashboard) => dashboard.name.toLowerCase() === value.toLowerCase())) throw new Error(`There is a dashboard called ${value} already.`);
        return { name: value, description: description.value.trim(), tiles: [] };
      },
    });
    if (!made) return;
    this.editing = made;
    this.fresh = true;
    this.draw();
  }

  /** The controls a tile has while editing: its title, how it shows, its width, its place. */
  tileControls(tile, index) {
    const tiles = this.editing.tiles;
    const move = (to) => {
      if (to < 0 || to >= tiles.length) return;
      const [moved] = tiles.splice(index, 1);
      tiles.splice(to, 0, moved);
      this.draw();
      this.target.querySelector(`.dash-tile[data-index="${to}"] .tile-move`)?.focus();
    };
    return h('div', { class: 'tile-controls' },
      h('input', { class: 'input', 'aria-label': 'Title of the tile', placeholder: tile.view, value: tile.title ?? '', oninput: (event) => { tile.title = event.target.value; } }),
      h('select', {
        class: 'select',
        'aria-label': 'Show as',
        onchange: (event) => {
          tile.show = event.target.value;
          this.draw();
        },
      }, Object.entries(SHOWS).map(([value, label]) => h('option', { value, selected: (tile.show ?? 'bars') === value }, label))),
      h('button', {
        class: 'button small quiet',
        title: tile.width === 2 ? 'Make it one column wide' : 'Make it two columns wide',
        'aria-pressed': String(tile.width === 2),
        onclick: () => {
          tile.width = tile.width === 2 ? 1 : 2;
          this.draw();
        },
      }, icon('columns', 14), tile.width === 2 ? 'Wide' : 'Narrow'),
      h('button', {
        class: 'button small quiet icon tile-move',
        'aria-label': 'Move the tile: Left and Right arrows move it',
        title: 'Drag to move, or use the arrow keys',
        onkeydown: (event) => {
          const step = { ArrowLeft: -1, ArrowUp: -1, ArrowRight: 1, ArrowDown: 1 }[event.key];
          if (!step) return;
          event.preventDefault();
          move(index + step);
        },
      }, icon('drag', 16)),
      h('button', {
        class: 'button small quiet icon',
        'aria-label': `Remove ${tile.title || tile.view}`,
        onclick: () => {
          tiles.splice(index, 1);
          this.draw();
        },
      }, icon('close', 16)));
  }

  /** Lets a tile be dragged to another place while editing. */
  draggable(card, index) {
    card.draggable = true;
    card.addEventListener('dragstart', (event) => {
      if (event.target.closest('input, select')) {
        event.preventDefault();
        return;
      }
      event.dataTransfer.setData('text/x-epiphany-tile', String(index));
      event.dataTransfer.effectAllowed = 'move';
    });
    card.addEventListener('dragover', (event) => {
      if (!event.dataTransfer.types.includes('text/x-epiphany-tile')) return;
      event.preventDefault();
      card.classList.add('drop-target');
    });
    card.addEventListener('dragleave', () => card.classList.remove('drop-target'));
    card.addEventListener('drop', (event) => {
      event.preventDefault();
      card.classList.remove('drop-target');
      const from = Number(event.dataTransfer.getData('text/x-epiphany-tile'));
      if (Number.isNaN(from) || from === index) return;
      const [moved] = this.editing.tiles.splice(from, 1);
      this.editing.tiles.splice(index, 0, moved);
      this.draw();
    });
  }

  /** Asks which saved view a new tile shows, and how. */
  async addTile() {
    const cubes = this.definition.cubes.filter((cube) => Object.keys(cube.views ?? {}).length);
    if (!cubes.length) {
      notify('No cube has a saved view yet. Save a view in Explore first.', 'bad');
      return;
    }
    const cube = h('select', { class: 'select', id: 'tile-cube' }, cubes.map((each) => h('option', { value: each.name }, each.name)));
    const view = h('select', { class: 'select', id: 'tile-view' });
    const views = () => mount(view, Object.keys(cubes.find((each) => each.name === cube.value).views).map((name) => h('option', { value: name }, name)));
    cube.addEventListener('change', views);
    views();
    const how = h('select', { class: 'select', id: 'tile-show' }, Object.entries(SHOWS).map(([value, label]) => h('option', { value, selected: value === 'bars' }, label)));
    const wide = h('input', { type: 'checkbox', id: 'tile-wide' });
    const title = h('input', { class: 'input', id: 'tile-title', maxlength: 120, placeholder: 'The view\'s name' });
    const tile = await dialog({
      title: 'Add a tile',
      confirm: 'Add',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field-pair' },
          h('div', { class: 'field' }, h('label', { for: 'tile-cube' }, 'Cube'), cube),
          h('div', { class: 'field' }, h('label', { for: 'tile-view' }, 'Saved view'), view)),
        h('div', { class: 'field-pair' },
          h('div', { class: 'field' }, h('label', { for: 'tile-show' }, 'Show as'), how),
          h('div', { class: 'field' }, h('label', { for: 'tile-title' }, 'Title'), title)),
        h('label', { class: 'check' }, wide, 'Two columns wide')),
      onsubmit: () => ({
        cube: cube.value,
        view: view.value,
        show: how.value,
        ...(title.value.trim() ? { title: title.value.trim() } : {}),
        ...(wide.checked ? { width: 2 } : {}),
      }),
    });
    if (!tile) return;
    this.editing.tiles.push(tile);
    this.draw();
  }

  /** Saves the dashboard being edited, against the model version it was made from. */
  async save() {
    const dashboard = this.editing;
    dashboard.name = dashboard.name.trim();
    if (!dashboard.name) {
      notify('Give the dashboard a name.', 'bad');
      return;
    }
    const clean = {
      name: dashboard.name,
      ...(dashboard.description?.trim() ? { description: dashboard.description.trim() } : {}),
      ...(dashboard.filters?.length ? { filters: dashboard.filters } : {}),
      tiles: dashboard.tiles.map((tile) => ({
        ...(tile.title?.trim() ? { title: tile.title.trim() } : {}),
        cube: tile.cube,
        view: tile.view,
        show: tile.show ?? 'bars',
        ...(tile.width === 2 ? { width: 2 } : {}),
      })),
    };
    // A dashboard renamed is saved in its old one's place.
    const at = this.fresh ? clean.name : this.name;
    try {
      await put(`/dashboards/${part(at)}`, { based_on: Number(modelVersion ?? state.version), dashboard: clean });
    } catch (problem) {
      notify(problem.status === 409 ? 'Someone changed the model since this page was opened. Reload it, then make your change again.' : problem.message, 'bad');
      return;
    }
    notify(`Saved ${clean.name}.`, 'good');
    await this.reload(clean.name);
  }

  async remove() {
    const name = this.name;
    const deleted = await dialog({
      title: `Delete ${name}?`,
      body: h('p', {}, 'The dashboard goes for everyone. The views its tiles show stay with their cubes.'),
      confirm: 'Delete',
      danger: true,
      onsubmit: async () => {
        await del(`/dashboards/${part(name)}?based_on=${Number(modelVersion ?? state.version)}`);
        return true;
      },
    });
    if (!deleted) return;
    notify(`Deleted ${name}.`, 'good');
    await this.reload(null);
  }

  /** Fetches the model again after a change, and shows the dashboard named. */
  async reload(name) {
    state.model = null;
    this.definition = await model();
    this.editing = null;
    this.fresh = false;
    this.name = this.list().find((dashboard) => dashboard.name === name)?.name ?? this.list()[0]?.name ?? null;
    const path = this.name ? `/dashboards?name=${encodeURIComponent(this.name)}` : '/dashboards';
    if (path === location.pathname + location.search) this.draw();
    else go(path);
  }
}

/* ---- How a tile shows a result ---- */

/** The first value, large, named by its row and column, with the trend of its row beneath. */
function figure(result, format) {
  const [columns, rows] = result.axes;
  const value = result.cells[0];
  const name = [...(rows?.tuples[0] ?? []), ...(columns?.tuples[0] ?? [])].map((m) => m.name).join(' · ');
  const row = (columns?.tuples ?? []).map((_, c) => (typeof result.cells[c] === 'number' ? result.cells[c] : null));
  const trend = row.filter((each) => each !== null).length > 1;
  const last = columns?.tuples[columns.tuples.length - 1]?.map((m) => m.name).join(' · ');
  return h('div', { class: 'figure' },
    h('div', { class: `figure-value${typeof value === 'number' && value < 0 ? ' negative' : ''}` }, value === null || value === undefined ? '–' : formatValue(value, format)),
    h('div', { class: 'muted' }, name),
    trend ? h('div', { class: 'figure-trend' }, sparkline(row), h('span', { class: 'muted' }, `${columns.tuples[0].map((m) => m.name).join(' · ')} to ${last}`)) : null);
}

/** The result as a small table, its first rows only. */
function grid(result, format) {
  const [columns, rows] = result.axes;
  const label = (tuple) => tuple.map((m) => m.name).join(' · ');
  const rowTuples = rows?.tuples ?? [[]];
  const count = columns?.tuples.length ?? 0;
  const cell = (value) => h('td', { class: typeof value === 'number' ? `n${value < 0 ? ' negative' : ''}` : '' }, formatValue(value, format));
  return h('div', { class: 'tile-grid' },
    h('table', { class: 'table compact' },
      h('thead', {}, h('tr', {}, h('th', {}), (columns?.tuples ?? []).map((tuple) => h('th', { class: 'n' }, label(tuple))))),
      h('tbody', {}, rowTuples.slice(0, GRID_ROWS).map((tuple, r) => h('tr', {},
        h('th', { scope: 'row' }, label(tuple)),
        Array.from({ length: count }, (_, c) => cell(result.cells[r * count + c])))))),
    rowTuples.length > GRID_ROWS ? h('p', { class: 'hint' }, `The first ${GRID_ROWS} of ${rowTuples.length.toLocaleString()} rows. Open it in Explore to see them all.`) : null);
}
