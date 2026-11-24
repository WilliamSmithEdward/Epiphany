// Dashboards: pages of saved views, kept with the model. Each tile shows a view as a key
// figure, a chart, a table or a heatmap, or says a few lines of its own; a reader clicking a
// mark or a row of one tile sees every other at its members. Administrators arrange them: on
// a grid, where tiles keep clear of one another and rise to fill a gap, or freely, where each
// stands where it is put and lines up with the others. A change is saved against the model
// version it was made from, so that nobody's change is overwritten unseen.

import { del, modelVersion, part, post, put } from './api.js';
import { announce, empty, failure, go, model, readdress, state } from './app.js';
import { COLUMNS, firstFree, inReadingOrder, linesOf, moveOnGrid, nearest, onPage, ontoGrid, resizeOnGrid } from './arrange.js';
import { seriesNames, shape } from './chart.js';
import { chooseMember, dimension } from './picker.js';
import { csvOf, tsvOf } from './selection.js';
import { askDrawn, AXES, CHARTS, numbersOf, present, prose } from './tiles.js';
import { choices, copyText, dialog, download, h, icon, invalid, KINDS, menu, MOD, mount, notify, when, working } from './ui.js';

/** A row's height and the gap between tiles, in pixels: as the styles have them. */
const ROW = 40;
const GAP = 16;
/** A board narrower than this, as on a phone, stands its tiles one above the other, in reading order. */
const NARROW = 600;
/** How near, in pixels, an edge comes to a line before it is put on it. */
const NEAR = 8;

/** How large a tile of each kind is made, and the least it may be, in columns and rows. */
const SIZES = {
  figure: { w: 3, h: 5, least: [2, 3] },
  bars: { w: 6, h: 8, least: [3, 5] },
  stacked: { w: 6, h: 8, least: [3, 5] },
  shares: { w: 6, h: 8, least: [3, 5] },
  lines: { w: 6, h: 8, least: [3, 5] },
  area: { w: 6, h: 8, least: [3, 5] },
  ranked: { w: 4, h: 8, least: [3, 5] },
  donut: { w: 4, h: 8, least: [3, 5] },
  grid: { w: 6, h: 9, least: [3, 4] },
  heatmap: { w: 6, h: 8, least: [3, 4] },
  text: { w: 6, h: 3, least: [2, 2] },
};
const sizeOf = (tile) => SIZES[tile.show ?? 'bars'] ?? SIZES.bars;

/** The sizes a tile may be given at once, as a home screen's widgets are. */
const PRESETS = [['Small', 3, 4], ['Medium', 6, 8], ['Large', 8, 12], ['Full width', 12, 8]];

/** How often the tiles may run again by themselves, in minutes. */
const EVERY = [[0, 'By hand'], [1, 'Every minute'], [5, 'Every 5 minutes'], [15, 'Every 15 minutes'], [60, 'Every hour']];

/** Where the dashboard shown last, a dashboard's refreshing and freeform snapping are kept, in this browser. */
const LAST = 'epiphany.dashboards.last';
const SNAP = 'epiphany.dashboards.snap';
const refreshKey = (name) => `epiphany.dashboard.${name}.refresh`;

export async function render(target, { params, titled }) {
  const definition = await model();
  return new Dashboards(target, definition, params, titled);
}

/**
 * The place of every tile of a dashboard: its own, or for one without, the first free place
 * as wide as its `width` says; on a grid, settled onto whole columns and rows.
 */
function placesOf(dashboard) {
  const places = [];
  for (const tile of dashboard.tiles) {
    places.push(tile.place ? { ...tile.place } : firstFree(places, { w: tile.width === 2 ? COLUMNS : COLUMNS / 2, h: sizeOf(tile).h }));
  }
  return dashboard.layout === 'free' ? places : ontoGrid(places);
}

/** A place as it is kept: whole on a grid, to a hundredth laid out freely. */
const kept = (place, free) => Object.fromEntries(['x', 'y', 'w', 'h'].map((key) => [key, free ? Math.round(place[key] * 100) / 100 : Math.round(place[key])]));

/** Where a tile stands on the page, as the styles lay it out, and where it is read, for a narrow page. */
function stand(element, place, order) {
  for (const key of ['x', 'y', 'w', 'h']) element.style.setProperty(`--${key}`, String(place[key]));
  if (order !== undefined) element.style.setProperty('--order', String(order));
}

class Dashboards {
  /** `titled` names the page in the browser's title. */
  constructor(target, definition, params, titled) {
    this.titled = titled;
    this.target = target;
    this.definition = definition;
    this.admin = state.user?.role === 'admin';
    // The answers the tiles were given, by what each asked, until they are run again; and
    // the one each tile showed last, by its place in the dashboard.
    this.answers = new Map();
    this.lastShown = new Map();
    // The tiles shown as their numbers for now, in place of how they show them, by place.
    this.tabled = new Set();
    this.snapping = localStorage.getItem(SNAP) !== 'off';
    // A page too narrow for its columns stands the tiles one above the other.
    this.watcher = new ResizeObserver(() => {
      this.narrow = this.board.clientWidth > 0 && this.board.clientWidth < NARROW;
      this.board.classList.toggle('stacked', this.narrow);
    });
    this.update(params);
  }

  /** Shows the dashboard the address names, or else the first, moving to it in place. */
  update(params) {
    const name = params.get('name');
    // The dashboard being edited, a copy, and whether it is new; null when only shown.
    this.editing = null;
    this.fresh = false;
    this.past = [];
    this.future = [];
    this.adding = false;
    this.selection = null;
    this.focused = null;
    this.lastShown.clear();
    this.tabled.clear();
    // Not told which, or told of one there is not, the page shows the dashboard shown last,
    // while there is one of that name, or else the first; and the address names it.
    const named = (wanted) => this.list().find((dashboard) => dashboard.name === wanted)?.name;
    this.name = named(name) ?? named(localStorage.getItem(LAST)) ?? this.list()[0]?.name ?? null;
    if (this.name) {
      localStorage.setItem(LAST, this.name);
      if (name !== this.name) readdress(`/dashboards?name=${encodeURIComponent(this.name)}`);
    }
    this.draw();
  }

  list() {
    return this.definition.dashboards ?? [];
  }

  current() {
    return this.list().find((dashboard) => dashboard.name === this.name) ?? null;
  }

  shown() {
    return this.editing ?? this.current();
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

  /**
   * The members a tile stands at: those chosen for the page's filters and those picked in
   * another tile, of its cube's dimensions; what was picked over what was chosen.
   */
  standing(tile, index) {
    const chosen = this.chosen();
    const at = new Map();
    for (const dimensionName of this.shown()?.filters ?? []) {
      if (chosen[dimensionName] && this.cubeHas(tile.cube, dimensionName)) at.set(dimensionName, chosen[dimensionName]);
    }
    if (this.selection && this.selection.tile !== index) {
      for (const member of this.selection.members) {
        if (this.cubeHas(tile.cube, member.dimension)) at.set(member.dimension, member.name);
      }
    }
    return [...at].map(([dimensionName, member]) => ({ dimension: dimensionName, member }));
  }

  cubeHas(cubeName, dimensionName) {
    return Boolean(this.definition.cubes.find((cube) => cube.name.toLowerCase() === cubeName?.toLowerCase())?.dimensions.includes(dimensionName));
  }

  viewOf(tile) {
    return this.definition.cubes.find((cube) => cube.name === tile.cube)?.views?.[tile.view] ?? null;
  }

  /** The filters above the tiles: a member chosen for each, which every tile that can stands at; and what is picked. */
  drawFilters(shown) {
    const filters = shown.filters ?? [];
    const chosen = this.chosen();
    const picked = this.selection
      ? h('div', { class: 'chip dash-filter picked' },
        h('span', { class: 'chip-main' }, h('strong', {}, 'Picked'), h('span', { class: 'chip-summary' }, this.selection.members.map((m) => m.name).join(' · '))),
        h('button', { class: 'chip-more', 'aria-label': 'Show every tile as before the pick (Escape)', title: 'Show every tile as before (Escape)', onclick: () => this.select(null) }, icon('close', 14)))
      : null;
    if (!filters.length && !this.editing && !picked) return null;
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
          onclick: () => this.change(() => {
            this.editing.filters = filters.filter((other) => other !== dimensionName);
          }),
        }, icon('close', 14))
        : chosen[dimensionName] ? h('button', { class: 'chip-more', 'aria-label': `Show the tiles as their views have ${dimensionName}`, title: 'As each view has it', onclick: () => this.choose(dimensionName, null) }, icon('close', 14)) : null);
    const offered = [...new Set(shown.tiles.flatMap((tile) => this.definition.cubes.find((cube) => cube.name.toLowerCase() === tile.cube?.toLowerCase())?.dimensions ?? []))]
      .filter((dimensionName) => !filters.includes(dimensionName));
    return h('div', { class: 'dash-filters', role: 'group', 'aria-label': 'Filters for every tile' },
      icon('filter', 16),
      filters.map(chip),
      picked,
      this.editing ? h('button', {
        class: 'button small quiet',
        disabled: !offered.length,
        title: offered.length ? 'Offer a choice of member that every tile stands at' : 'Add tiles first: a filter is a dimension of their cubes',
        onclick: (event) => menu([
          { heading: 'Filter every tile by' },
          ...offered.map((dimensionName) => ({
            label: dimensionName,
            icon: 'dimension',
            action: () => this.change(() => {
              this.editing.filters = [...filters, dimensionName];
            }),
          })),
        ], event.currentTarget),
      }, icon('plus', 14), 'Add a filter') : null);
  }

  /** Asked before the page is left: unsaved edits are discarded only if the user agrees. */
  async leave() {
    if (!this.unsaved()) return true;
    return Boolean(await dialog({
      title: 'Discard your changes?',
      body: h('p', {}, `Your changes to ${this.editing.name} are not saved. Leaving discards them.`),
      confirm: 'Discard and leave',
      cancel: 'Stay',
      danger: true,
    }));
  }

  unsaved() {
    return Boolean(this.editing) && (this.fresh || JSON.stringify(this.editing) !== JSON.stringify(this.current()));
  }

  /** Saves the dashboard being edited, as its Save button does. */
  saveChanges() {
    return this.save();
  }

  /** Names the dashboard shown in the browser's title, marked while changes to it are not saved. */
  retitle() {
    this.titled(this.shown()?.name, this.unsaved());
  }

  draw() {
    const shown = this.shown();
    this.retitle();
    clearInterval(this.timer);
    // What held the keys in a tile, its chart, table or a button, holds them again once drawn.
    const holding = this.cards?.findIndex((card) => card.contains(document.activeElement)) ?? -1;
    const held = holding >= 0 ? ['.chart-plot', '.tile-table', '[aria-haspopup="menu"]', '.tile-head .button'].find((css) => document.activeElement.matches(css)) : null;
    if (!shown) {
      mount(this.target, h('div', { class: 'page' },
        h('div', { class: 'page-head' }, h('div', { class: 'grow' }, h('h1', {}, 'Dashboards'))),
        h('div', { class: 'card' }, empty('dashboard', 'No dashboards yet',
          this.admin ? 'A dashboard gathers saved views, each shown as a key figure, a chart or a grid. Save views in Explore, then make a dashboard of them.' : 'An administrator makes dashboards from views saved in Explore.',
          this.admin ? h('button', { class: 'button primary', onclick: () => this.create() }, icon('plus', 16), 'New dashboard') : null))));
      return;
    }
    const editing = Boolean(this.editing);
    const presenting = this.presenting && !editing;
    // Links from one dashboard to the next, the one shown marked as the page it is.
    const tabs = h('nav', { class: 'dash-tabs', 'aria-label': 'Dashboards' }, this.list().map((dashboard) => h('a', {
      href: `/dashboards?name=${encodeURIComponent(dashboard.name)}`,
      'data-link': '',
      'aria-current': dashboard.name === this.name ? 'page' : null,
    }, dashboard.name)));
    const head = editing
      ? h('div', { class: 'grow dash-names' },
        // The name is in a field while it is edited, so the page is headed for a screen reader.
        h('h1', { class: 'visually-hidden' }, 'Editing a dashboard'),
        h('input', { class: 'input dash-name', 'aria-label': 'Name of the dashboard', value: shown.name, oninput: (event) => { this.editing.name = event.target.value; this.retitle(); } }),
        h('input', { class: 'input', 'aria-label': 'What the dashboard is for', placeholder: 'What it is for, in a line', value: shown.description ?? '', oninput: (event) => { this.editing.description = event.target.value; this.retitle(); } }))
      : h('div', { class: 'grow' },
        h('div', { class: 'dash-title' }, h('h1', {}, shown.name), presenting ? null : this.updatedNote()),
        shown.description ? h('p', {}, shown.description) : null);
    this.places = placesOf(shown);
    this.board = h('div', { class: `board${shown.layout === 'free' ? ' free' : ''}${editing ? ' editing' : ''}${editing && (shown.layout !== 'free' || this.snapping) ? ' gridded' : ''}` });
    this.ghost = h('div', { class: 'board-ghost', hidden: true });
    this.guides = h('div', { class: 'board-guides' });
    // How to run each tile again, for Refresh.
    this.filling = [];
    const order = new Map(inReadingOrder(this.places).map((place, rank) => [place, rank]));
    this.cards = shown.tiles.map((tile, index) => this.tile(tile, index, this.places[index], order.get(this.places[index])));
    this.board.append(...this.cards, this.ghost, this.guides);
    // Edited, an empty board is where the first tile is brought.
    if (editing && !shown.tiles.length) {
      this.board.append(h('div', { class: 'board-empty' }, icon('dashboard', 32), h('p', {}, 'Drag a view here from the list, or choose one there.'),
        this.adding ? null : h('button', { class: 'button primary', onclick: () => this.toggleAdding() }, icon('plus', 16), 'Add tiles')));
    }
    this.fitHeight();
    if (editing) this.board.addEventListener('pointerdown', (event) => this.grab(event));
    const frame = h('div', { class: 'board-frame' },
      // Stood one above the other, the tiles cannot be arranged here, and the page says where.
      editing ? h('p', { class: 'hint stack-hint' }, 'The tiles stand one above the other at this width: arrange them in a wider window. Their options are here as well.') : null,
      shown.tiles.length || editing
        ? this.board
        : h('div', { class: 'card' }, empty('dashboard', 'No tiles yet', 'An administrator adds tiles that show saved views.')));
    mount(this.target, h('div', { class: `page dashboard${editing ? ' editing' : ''}${this.adding ? ' adding' : ''}${presenting ? ' presenting' : ''}`, onkeydown: (event) => this.key(event) },
      this.list().length > 1 && !editing && !presenting ? tabs : null,
      h('div', { class: 'page-head' }, head, h('div', { class: 'toolbar' }, editing ? this.editActions(shown) : presenting ? this.presentActions() : this.viewActions())),
      this.drawFilters(shown),
      h('div', { class: 'dash-work' }, frame, this.adding ? this.drawer() : null),
      h('p', { class: 'visually-hidden', id: 'tile-keys' }, 'The arrow keys move the tile, and with Shift make it larger or smaller; Enter opens its options, and Delete removes it.'),
      this.focused !== null ? h('div', { class: 'focus-backdrop', onclick: () => this.focus(null) }) : null));
    this.watcher.disconnect();
    this.watcher.observe(frame);
    this.everyMinutes();
    const card = this.cards[holding];
    if (card) (held && card.querySelector(held) || card.querySelector('.tile-head .button') || card).focus({ preventScroll: true });
    // Presented, the keys stay on the page, for the arrows to go on from one dashboard to the next.
    else if (presenting && !this.target.contains(document.activeElement)) this.target.querySelector('.page-head .toolbar .button')?.focus({ preventScroll: true });
  }

  /**
   * When the tiles were run, as long ago as it was, beside the dashboard's name rather than
   * among its buttons, which then fit on one line in a smaller laptop's window.
   */
  updatedNote() {
    let updated = when(Date.now() / 1000);
    this.refreshed = () => updated.replaceWith(updated = when(Date.now() / 1000));
    return h('span', { class: 'muted' }, 'Updated ', updated);
  }

  /** The buttons over a dashboard shown: Refresh and its pace, and for an administrator, Edit, New and Delete. */
  viewActions() {
    const pace = Number(localStorage.getItem(refreshKey(this.name)) ?? 0);
    return [
      h('button', {
        class: 'button quiet',
        title: 'Run every tile again',
        onclick: (event) => working(event.currentTarget, () => this.refresh()),
      }, icon('refresh', 16), 'Refresh'),
      h('select', {
        class: 'select small-select',
        'aria-label': 'Run the tiles again by themselves',
        onchange: (event) => {
          localStorage.setItem(refreshKey(this.name), event.target.value);
          this.everyMinutes();
        },
      }, EVERY.map(([minutes, text]) => h('option', { value: minutes, selected: minutes === pace }, text))),
      h('button', { class: 'button', title: 'Show the dashboard over the whole screen, as on a wall', onclick: () => this.present(true) }, icon('monitor', 16), 'Present'),
      this.admin ? h('button', { class: 'button', onclick: () => this.edit() }, icon('edit', 16), 'Edit') : null,
      this.admin ? h('button', { class: 'button', 'aria-haspopup': 'menu', title: 'A new dashboard, blank or a copy of this one', onclick: (event) => this.newMenu(event.currentTarget) }, icon('plus', 16), 'New', icon('down', 14)) : null,
      this.admin ? h('button', { class: 'button danger quiet', onclick: () => this.remove() }, icon('trash', 16), 'Delete') : null,
    ];
  }

  /**
   * The buttons over a dashboard presented: to the one before and the next, each by itself in
   * turn, and the end. Escape ends it too, and the arrow keys go along.
   */
  presentActions() {
    const list = this.list();
    const several = list.length > 1;
    const at = list.findIndex((dashboard) => dashboard.name === this.name);
    return [
      several ? h('button', { class: 'button quiet icon', 'aria-label': 'The dashboard before (Left arrow)', title: 'The dashboard before', onclick: () => this.step(-1) }, icon('back', 16)) : null,
      several ? h('span', { class: 'muted' }, `${at + 1} of ${list.length}`) : null,
      several ? h('button', { class: 'button quiet icon', 'aria-label': 'The next dashboard (Right arrow)', title: 'The next dashboard', onclick: () => this.step(1) }, icon('chevron', 16)) : null,
      several ? h('button', {
        class: 'button small quiet',
        'aria-pressed': String(Boolean(this.advancing)),
        title: 'Go on to the next dashboard every 30 seconds, as a screen on a wall does',
        onclick: () => {
          if (this.advancing) {
            clearInterval(this.advancing);
            this.advancing = null;
          } else {
            this.advancing = setInterval(() => (this.board?.isConnected && this.presenting ? this.step(1) : clearInterval(this.advancing)), 30_000);
          }
          this.draw();
        },
      }, icon('play', 14), 'Every 30 seconds') : null,
      h('button', { class: 'button', onclick: () => this.present(false) }, icon('unfocus', 16), 'End'),
    ];
  }

  /**
   * Presents the dashboard over the whole screen, or ends it: the screen's own, where the
   * browser allows, and the page's otherwise. Leaving the browser's whole screen ends it.
   */
  async present(on) {
    this.presenting = on;
    if (!on) {
      clearInterval(this.advancing);
      this.advancing = null;
    }
    this.draw();
    if (on) {
      this.target.querySelector('.page-head .toolbar .button:last-child')?.focus();
      if (!document.fullscreenElement) await this.target.requestFullscreen?.().catch(() => {});
      this.leaving ??= () => {
        if (!document.fullscreenElement && this.presenting && this.board?.isConnected) this.present(false);
      };
      document.addEventListener('fullscreenchange', this.leaving);
    } else {
      document.removeEventListener('fullscreenchange', this.leaving);
      if (document.fullscreenElement) await document.exitFullscreen().catch(() => {});
    }
    announce(on ? `Presenting ${this.name}. The arrow keys go from one dashboard to the next, and Escape ends it.` : 'No longer presenting');
  }

  /** Goes to the dashboard `by` places along, round from the last to the first. */
  step(by) {
    const list = this.list();
    const at = list.findIndex((dashboard) => dashboard.name === this.name);
    const next = list[(at + by + list.length) % list.length];
    if (next && next.name !== this.name) go(`/dashboards?name=${encodeURIComponent(next.name)}`);
  }

  /** The buttons over a dashboard being edited: back and again, how it is laid out, adding tiles, Cancel and Save. */
  editActions(shown) {
    const free = shown.layout === 'free';
    this.undoButton = h('button', { class: 'button quiet icon', 'aria-label': `Undo (${MOD} Z)`, title: `Undo (${MOD} Z)`, disabled: !this.past.length, onclick: () => this.undo() }, icon('undo', 16));
    this.redoButton = h('button', { class: 'button quiet icon', 'aria-label': `Redo (${MOD} Y)`, title: `Redo (${MOD} Y)`, disabled: !this.future.length, onclick: () => this.redo() }, icon('redo', 16));
    return [
      this.undoButton,
      this.redoButton,
      choices({
        kind: 'radiogroup',
        label: 'Lay the tiles out',
        options: [['grid', 'Grid'], ['free', 'Freeform']],
        chosen: free ? 'free' : 'grid',
        choose: (layout) => this.layOut(layout),
      }),
      free ? h('button', {
        class: 'button small quiet',
        'aria-pressed': String(this.snapping),
        title: 'Put tiles on whole columns and rows; without, they line up with one another as they come near',
        onclick: () => {
          this.snapping = !this.snapping;
          localStorage.setItem(SNAP, this.snapping ? 'on' : 'off');
          this.draw();
        },
      }, icon('snap', 14), 'Snap to grid') : null,
      h('button', { class: 'button', 'aria-pressed': String(this.adding), onclick: () => this.toggleAdding() }, icon('plus', 16), 'Add'),
      h('button', { class: 'button', onclick: () => this.stopEditing() }, 'Cancel'),
      h('button', { class: 'button primary', onclick: (event) => working(event.currentTarget, () => this.save()) }, icon('save', 16), 'Save'),
    ];
  }

  /** Runs every tile again, and the page's time with them, each by its own question. */
  async refresh() {
    this.answers.clear();
    await Promise.all(this.filling.map((fill) => fill()));
    this.refreshed?.();
  }

  /** Runs the tiles again by themselves at the pace chosen for the dashboard, while it is shown. */
  everyMinutes() {
    clearInterval(this.timer);
    const minutes = Number(localStorage.getItem(refreshKey(this.name)) ?? 0);
    if (!minutes || this.editing) return;
    this.timer = setInterval(() => {
      if (!this.board?.isConnected) clearInterval(this.timer);
      else if (!document.hidden) this.refresh();
    }, minutes * 60_000);
  }

  /** The board as high as its lowest tile, and while it is edited, room beneath to bring one to. */
  fitHeight(places = this.places) {
    const rows = Math.max(0, ...places.map((place) => place.y + place.h)) + (this.editing ? 4 : 0);
    this.board.style.setProperty('--rows', String(Math.max(rows, 1)));
  }

  /* ---- Tiles ---- */

  tile(tile, index, place, order) {
    const editing = Boolean(this.editing);
    const text = tile.show === 'text';
    const view = text ? null : this.viewOf(tile);
    const title = tile.title || (text ? '' : tile.view);
    const named = title || 'Text';
    // Beneath its name, where it comes from: its cube, and its view where the tile is named otherwise.
    const source = tile.title && tile.title !== tile.view ? `${tile.cube} · ${tile.view}` : tile.cube;
    const standing = text ? [] : this.standing(tile, index).map((at) => at.member);
    // Until its answer comes, a tile holds the outline of what it shows.
    const body = h('div', { class: 'tile-body' }, text ? prose(tile.text) : h('div', { class: `skeleton ${tile.show ?? 'bars'}` }));
    const open = `/explore?cube=${encodeURIComponent(tile.cube)}&view=${encodeURIComponent(tile.view)}`;
    const focused = this.focused === index;
    const actions = editing
      ? [
        h('button', { class: 'button small quiet icon', 'aria-label': `Options of ${named}`, 'aria-haspopup': 'menu', title: 'Options', onclick: (event) => this.tileMenu(index, event.currentTarget) }, icon('more', 16)),
        h('button', { class: 'button small quiet icon', 'aria-label': `Remove ${named}`, title: 'Remove', onclick: () => this.removeTile(index) }, icon('close', 16)),
      ]
      : [
        h('button', { class: 'button small quiet icon', 'aria-label': focused ? `Back to the dashboard from ${named}` : `Show ${named} on the whole page`, title: focused ? 'Back (Escape)' : 'Show on the whole page', onclick: () => this.focus(focused ? null : index) }, icon(focused ? 'unfocus' : 'focus', 16)),
        text ? null : h('button', { class: 'button small quiet icon', 'aria-label': `More of ${named}`, 'aria-haspopup': 'menu', title: 'Its numbers, copied or downloaded, and its view in Explore', onclick: (event) => this.viewMenu(index, event.currentTarget, open) }, icon('more', 16)),
      ];
    const head = title || editing || !text
      ? h('div', { class: 'tile-head' },
        // Edited, a tile is named, or its text changed, by a double-click on its name.
        h('div', { class: 'grow' }, h('h3', { ondblclick: editing ? () => this.retell(index) : null, title: editing ? (text ? 'Double-click to change the text' : 'Double-click to rename') : null }, title || h('span', { class: 'muted' }, 'Text')), text ? null : h('p', { class: 'muted' }, source,
          standing.length ? h('span', { class: 'tile-standing' }, ` · at ${standing.join(', ')}`) : null)),
        actions)
      : null;
    // Text alone has its buttons where they can be reached, over its corner.
    const corner = head ? null : h('div', { class: 'tile-corner' }, actions);
    const handles = editing ? (this.shown().layout === 'free' ? ['n', 'e', 's', 'w', 'ne', 'nw', 'se', 'sw'] : ['e', 's', 'se']).map((edge) => h('span', { class: `handle ${edge}`, dataset: { handle: edge }, 'aria-hidden': 'true' })) : null;
    const card = h('section', {
      class: `card dash-tile${text ? ' text-tile' : ''}${focused ? ' focused' : ''}`,
      'aria-label': named,
      dataset: { index },
      tabindex: editing ? '0' : null,
      'aria-describedby': editing ? 'tile-keys' : null,
      onkeydown: editing ? (event) => this.tileKey(event, index) : null,
    }, head, body, corner, handles);
    stand(card, place, order);
    if (this.shown().layout === 'free') card.style.zIndex = String(index + 1);
    if (!text) {
      const fill = () => this.fill(body, tile, view, index);
      this.filling.push(fill);
      fill();
    }
    return card;
  }

  /**
   * Runs a tile's view, or takes the answer it was given last to the same question, and shows
   * it as the tile asks. Run again, a tile keeps its last answer, fading after a moment, until
   * the new one comes.
   */
  async fill(body, tile, view, index) {
    if (!view) {
      mount(body, empty('alert', 'The view is gone', `${tile.cube} has no view called ${tile.view}. It may have been renamed or removed.`));
      return;
    }
    const context = this.standing(tile, index);
    const asked = JSON.stringify([tile.cube, tile.view, context]);
    // Stood at members none of its rows or columns are, a view has nothing to show, and says so.
    const shown = (result, fresh) => mount(body, context.length && result.axes.some((axis) => !axis.tuples.length)
      ? empty('filter', 'Nothing here at these members', `None of the view's rows or columns stand at ${context.map((at) => at.member).join(', ')}.`)
      : present(result, this.tabled.has(index) ? { ...tile, show: 'grid' } : tile, view, {
        pick: this.editing ? null : (members, key) => this.select({ tile: index, key, members }),
        picked: this.selection?.tile === index ? this.selection.key : null,
        fresh,
      }));
    // An answer given before is shown again at once, and still.
    const answer = this.answers.get(asked);
    if (answer) {
      this.lastShown.set(index, { tile, result: answer });
      shown(answer, false);
      return;
    }
    // Asked anew, as at other members, it shows what it showed last until the new answer comes.
    const last = this.lastShown.get(index);
    if (last && last.tile.cube === tile.cube && last.tile.view === tile.view) shown(last.result, false);
    // Only a tile's first answer comes in moving: run again, its values change in place.
    const first = Boolean(body.querySelector(':scope > .skeleton'));
    body.setAttribute('aria-busy', 'true');
    body.classList.add('busy');
    try {
      const result = await post('/mdx', { mdx: view.mdx, context }, { quiet: true });
      this.answers.set(asked, result);
      this.lastShown.set(index, { tile, result });
      if (body.isConnected) shown(result, first);
    } catch (problem) {
      mount(body, failure(problem, () => this.fill(body, tile, view, index)));
    }
    body.classList.remove('busy');
    body.removeAttribute('aria-busy');
  }

  /**
   * Picks the members of a mark or a row of one tile, the others to stand at them; the same
   * picked again, or none, shows every tile as before.
   */
  select(selection) {
    const same = selection && this.selection && selection.tile === this.selection.tile && selection.key === this.selection.key;
    // Only members a cube can stand at are picked: a calculated member is the query's own.
    const members = (selection?.members ?? []).filter((member) => member.type !== 'calculated');
    this.selection = same || !selection || !members.length ? null : { ...selection, members };
    this.draw();
    announce(this.selection ? `Every other tile at ${this.selection.members.map((m) => m.name).join(', ')}` : 'Every tile as before');
  }

  /**
   * A shown tile's menu: its numbers in place of its chart, or its chart again; its numbers
   * copied, to paste into a spreadsheet, or downloaded as CSV; and its view in Explore.
   */
  viewMenu(index, at, open) {
    const tile = this.shown().tiles[index];
    const result = this.lastShown.get(index)?.result;
    const name = tile.title || tile.view;
    const tabled = this.tabled.has(index);
    const [kind, kindIcon] = KINDS[tile.show ?? 'bars'];
    menu([
      { heading: name },
      tile.show === 'grid' ? null : {
        label: tabled ? `Show the ${kind.toLowerCase()} again` : 'Show its numbers',
        icon: tabled ? kindIcon : 'grid',
        action: () => {
          if (tabled) this.tabled.delete(index);
          else this.tabled.add(index);
          this.draw();
        },
      },
      { label: 'Copy its numbers', icon: 'copy', disabled: !result, action: () => copyText(tsvOf(numbersOf(result))) },
      { label: 'Download them as CSV', icon: 'download', disabled: !result, action: () => download(`${name}.csv`, csvOf(numbersOf(result)), 'text/csv') },
      '-',
      { label: 'Open in Explore', icon: 'explore', action: () => go(open) },
    ], at);
  }

  /** Shows a tile over the whole page, or the dashboard again: its button keeps the keys either way. */
  focus(index) {
    const was = this.focused;
    this.focused = index;
    this.draw();
    this.cards?.[index ?? was]?.querySelector('.tile-head .button, .tile-corner .button')?.focus({ preventScroll: true });
  }

  /**
   * The page's keys: Escape puts back a tile shown over the page, then what was picked, then
   * ends presenting; presented, the arrows go from one dashboard to the next, but in a chart or
   * a table, which they go along; edited, Ctrl Z and Ctrl Y take a change back and make it again.
   */
  key(event) {
    if (event.target.closest('input, textarea, select')) return;
    const presenting = this.presenting && !this.editing;
    if (event.key === 'Escape' && (this.focused !== null || this.selection || presenting) && !document.querySelector('dialog[open], .menu')) {
      event.preventDefault();
      if (this.focused !== null) this.focus(null);
      else if (this.selection) this.select(null);
      else this.present(false);
    } else if (presenting && (event.key === 'ArrowLeft' || event.key === 'ArrowRight') && !event.target.closest('.chart-plot, .tile-table')) {
      event.preventDefault();
      this.step(event.key === 'ArrowRight' ? 1 : -1);
    } else if (this.editing && (event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'z') {
      event.preventDefault();
      if (event.shiftKey) this.redo();
      else this.undo();
    } else if (this.editing && (event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'y') {
      event.preventDefault();
      this.redo();
    }
  }

  /* ---- Editing ---- */

  edit() {
    this.editing = structuredClone(this.current());
    this.fresh = false;
    this.past = [];
    this.future = [];
    this.selection = null;
    this.focused = null;
    this.draw();
  }

  stopEditing() {
    this.editing = null;
    this.fresh = false;
    this.adding = false;
    this.draw();
  }

  /** Makes a change to the dashboard being edited, which Undo takes back, and draws it again. */
  change(make, { draw = true } = {}) {
    this.past.push(JSON.stringify(this.editing));
    if (this.past.length > 100) this.past.shift();
    this.future = [];
    make();
    if (draw) this.draw();
    else this.retitle();
  }

  undo() {
    if (!this.past.length) return;
    this.future.push(JSON.stringify(this.editing));
    this.editing = JSON.parse(this.past.pop());
    this.draw();
    announce('Undone');
  }

  redo() {
    if (!this.future.length) return;
    this.past.push(JSON.stringify(this.editing));
    this.editing = JSON.parse(this.future.pop());
    this.draw();
    announce('Done again');
  }

  /** A new dashboard: a blank one, or a copy of the one shown, as Power BI duplicates a page. */
  newMenu(at) {
    const shown = this.current();
    menu([
      { label: 'A blank dashboard', icon: 'plus', action: () => this.create() },
      shown ? { label: `A copy of ${shown.name}`, icon: 'copy', action: () => this.create(shown) } : null,
    ], at);
  }

  /** Names a new dashboard, blank or a copy of `from`, and opens it to be arranged and saved. */
  async create(from = null) {
    const name = h('input', { class: 'input', id: 'dashboard-name', required: true, maxlength: 120 });
    const description = h('input', { class: 'input', id: 'dashboard-description', maxlength: 240 });
    name.value = from ? `Copy of ${from.name}` : '';
    description.value = from?.description ?? '';
    const made = await dialog({
      title: from ? `A copy of ${from.name}` : 'New dashboard',
      confirm: 'Make it',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'dashboard-name' }, 'Name'), name),
        h('div', { class: 'field' }, h('label', { for: 'dashboard-description' }, 'What it is for'), description)),
      onsubmit: () => {
        const value = name.value.trim();
        if (!value) throw invalid(name, 'Give the dashboard a name.');
        if (this.list().some((dashboard) => dashboard.name.toLowerCase() === value.toLowerCase())) throw invalid(name, `There is a dashboard called ${value} already.`);
        const named = { name: value, description: description.value.trim() };
        return from ? { ...structuredClone(from), ...named } : { ...named, tiles: [] };
      },
    });
    if (!made) return;
    this.editing = made;
    this.fresh = true;
    this.past = [];
    this.future = [];
    // A blank one opens with the views to add beside it; a copy has its tiles already.
    this.adding = !from;
    this.draw();
  }

  /** Lays the tiles out on the grid, or freely; brought onto the grid, each comes to whole columns and rows. */
  layOut(layout) {
    if ((this.editing.layout ?? 'grid') === layout) return;
    this.change(() => {
      const places = placesOf(this.editing);
      if (layout === 'grid') ontoGrid(places);
      this.editing.tiles.forEach((tile, index) => {
        tile.place = kept(places[index], layout === 'free');
      });
      if (layout === 'free') this.editing.layout = 'free';
      else delete this.editing.layout;
    });
    announce(layout === 'free' ? 'Laid out freely: tiles stay where they are put' : 'Laid out on the grid');
  }

  /** The places shown now, kept as the tiles', with Undo able to take them back. */
  settleOn(places) {
    const free = this.editing.layout === 'free';
    this.change(() => {
      this.editing.tiles.forEach((tile, index) => {
        tile.place = kept(places[index], free);
      });
    }, { draw: false });
    this.places = places.map((place) => ({ ...place }));
    this.place(this.places);
    this.ghost.hidden = true;
    this.guides.replaceChildren();
    this.fitHeight();
    // Back and again come to life with the first change.
    this.undoButton.disabled = !this.past.length;
    this.redoButton.disabled = !this.future.length;
  }

  /** Stands each tile at its place, the reading order kept for a narrow page. */
  place(places) {
    const order = new Map(inReadingOrder(places).map((place, rank) => [place, rank]));
    places.forEach((place, index) => stand(this.cards[index], place, order.get(place)));
  }

  /** A column's and a row's step on the board, in pixels: each with the gap after it. */
  units() {
    return { across: (this.board.clientWidth + GAP) / COLUMNS, down: ROW + GAP };
  }

  /**
   * Moves or resizes a tile from where the pointer went down on it until it comes up: on a
   * grid, the others make way and a ghost shows where it will settle; laid out freely, it goes
   * where it is taken, lining up with the others, or on whole columns and rows when snapping.
   */
  grab(event) {
    const card = event.target.closest('.dash-tile');
    if (!card || event.button !== 0 || this.narrow) return;
    const edge = event.target.closest('[data-handle]')?.dataset.handle ?? null;
    if (!edge && event.target.closest('button, a, input, select, textarea')) return;
    event.preventDefault();
    card.focus({ preventScroll: true });
    const index = Number(card.dataset.index);
    const free = this.editing.layout === 'free';
    const start = this.places.map((place) => ({ ...place }));
    const unit = this.units();
    const from = { x: event.clientX, y: event.clientY };
    let moved = false;
    let shown = start;
    card.setPointerCapture(event.pointerId);
    const follow = (moveEvent) => {
      const dx = moveEvent.clientX - from.x;
      const dy = moveEvent.clientY - from.y;
      if (!moved && Math.hypot(dx, dy) < 4) return;
      if (!moved) {
        moved = true;
        card.classList.add(edge ? 'sizing' : 'lifted');
        this.board.classList.add('arranging');
      }
      shown = edge
        ? this.resized(start, index, edge, dx / unit.across, dy / unit.down, free)
        : this.moved(start, index, { x: start[index].x + dx / unit.across, y: start[index].y + dy / unit.down }, free);
      if (!edge && !free) {
        // On a grid the tile follows the pointer, and a ghost shows where it will settle.
        card.style.transform = `translate(${dx}px, ${dy}px) scale(1.02)`;
        shown.forEach((place, other) => other !== index && stand(this.cards[other], place));
        stand(this.ghost, shown[index]);
        this.ghost.hidden = false;
      } else {
        shown.forEach((place, other) => stand(this.cards[other], place));
      }
      this.fitHeight(shown);
      if (edge) card.dataset.size = `${Math.round(shown[index].w * 10) / 10} × ${Math.round(shown[index].h * 10) / 10}`;
    };
    const end = (keep) => {
      card.removeEventListener('pointermove', follow);
      card.removeEventListener('pointerup', done);
      card.removeEventListener('pointercancel', cancel);
      this.board.classList.remove('arranging');
      delete card.dataset.size;
      if (!moved) return;
      glide(card, () => {
        card.classList.remove('lifted', 'sizing');
        if (keep) this.settleOn(shown);
        else {
          this.place(this.places);
          this.ghost.hidden = true;
          this.guides.replaceChildren();
          this.fitHeight();
        }
      });
      announce(`${card.getAttribute('aria-label')}: ${this.said(this.places[index])}`);
    };
    const done = () => end(true);
    const cancel = () => end(false);
    card.addEventListener('pointermove', follow);
    card.addEventListener('pointerup', done);
    card.addEventListener('pointercancel', cancel);
  }

  /** The places with tile `index` moved to `to`: on a grid, to whole columns and rows, the others making way. */
  moved(start, index, to, free) {
    const places = start.map((place) => ({ ...place }));
    const moving = places[index];
    if (!free) return moveOnGrid(places, moving, { x: Math.round(to.x), y: Math.max(0, Math.round(to.y)) });
    Object.assign(moving, onPage({ ...moving, ...to }));
    if (this.snapping) {
      moving.x = Math.round(moving.x);
      moving.y = Math.round(moving.y);
    } else {
      this.lineUp(places, moving, { x: ['start', 'middle', 'end'], y: ['start', 'middle', 'end'] });
    }
    Object.assign(moving, onPage(moving));
    return places;
  }

  /**
   * The places with tile `index` resized from `edge` by `across` columns and `down` rows: on a
   * grid to whole ones, pushing the others down; never smaller than its kind may be.
   */
  resized(start, index, edge, across, down, free) {
    const places = start.map((place) => ({ ...place }));
    const place = places[index];
    const [leastW, leastH] = sizeOf(this.editing.tiles[index]).least;
    const was = start[index];
    const right = was.x + was.w;
    const bottom = was.y + was.h;
    if (edge.includes('e')) place.w = was.w + across;
    if (edge.includes('s')) place.h = was.h + down;
    if (edge.includes('w')) place.x = was.x + across;
    if (edge.includes('n')) place.y = was.y + down;
    if (!free || this.snapping) {
      for (const key of ['x', 'y', 'w', 'h']) place[key] = Math.round(place[key]);
    } else {
      this.lineUp(places, place, {
        x: edge.includes('w') ? ['start'] : edge.includes('e') ? ['end'] : [],
        y: edge.includes('n') ? ['start'] : edge.includes('s') ? ['end'] : [],
      }, true);
    }
    // A left or top edge moved keeps the right or bottom where it was.
    if (edge.includes('w')) place.w = right - place.x;
    if (edge.includes('n')) place.h = bottom - place.y;
    if (place.w < leastW) {
      if (edge.includes('w')) place.x = right - leastW;
      place.w = leastW;
    }
    if (place.h < leastH) {
      if (edge.includes('n')) place.y = bottom - leastH;
      place.h = leastH;
    }
    if (place.x < 0) {
      place.w += place.x;
      place.x = 0;
    }
    place.w = Math.min(place.w, COLUMNS - place.x);
    place.y = Math.max(place.y, 0);
    return free ? places : resizeOnGrid(places, place, { w: place.w, h: place.h });
  }

  /**
   * Lines a place laid out freely up with the others: an edge, or its middle, that comes
   * within a few pixels of another's edge or middle, or the page's, goes onto it, and the line
   * is drawn. `sized`: the edges move alone, the size changing, rather than the place.
   */
  lineUp(places, place, edges, sized = false) {
    const unit = this.units();
    const drawn = [];
    for (const axis of ['x', 'y']) {
      const [start, size, step] = axis === 'x' ? ['x', 'w', unit.across] : ['y', 'h', unit.down];
      const lines = linesOf(places, place, axis);
      let best = null;
      for (const which of edges[axis]) {
        const at = place[start] + { start: 0, middle: place[size] / 2, end: place[size] }[which];
        const line = nearest(at, lines, NEAR / step);
        if (line !== null && (!best || Math.abs(line - at) < Math.abs(best.by))) best = { by: line - at, line, which };
      }
      if (!best) continue;
      if (!sized) place[start] += best.by;
      else if (best.which === 'end') place[size] += best.by;
      else place[start] += best.by;
      // Placed through its style's properties: a style attribute is refused by the page's
      // policy, and the line was drawn at the board's edge.
      const guide = h('div', { class: `board-guide ${axis}` });
      guide.style[axis === 'x' ? 'left' : 'top'] = `${best.line * step - GAP / 2}px`;
      drawn.push(guide);
    }
    this.guides.replaceChildren(...drawn);
  }

  /** Where a place is, as said to a screen reader: from one, as it is seen. */
  said(place) {
    const whole = (n) => Math.round(n * 10) / 10;
    return `column ${whole(place.x + 1)}, row ${whole(place.y + 1)}, ${whole(place.w)} wide and ${whole(place.h)} high`;
  }

  /** The arrow keys move a tile, with Shift resize it; Enter opens its options and Delete removes it. */
  tileKey(event, index) {
    if (event.target !== event.currentTarget) return;
    const step = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[event.key];
    if (event.key === 'Enter') {
      event.preventDefault();
      this.tileMenu(index, event.currentTarget.querySelector('[aria-haspopup="menu"]') ?? event.currentTarget);
    } else if (event.key === 'Delete' || event.key === 'Backspace') {
      event.preventDefault();
      this.removeTile(index);
    } else if (step && !this.narrow) {
      event.preventDefault();
      const free = this.editing.layout === 'free';
      const start = this.places.map((place) => ({ ...place }));
      const [across, down] = step;
      const shown = event.shiftKey
        ? this.resized(start, index, `${down ? 's' : ''}${across ? 'e' : ''}`, across, down, free)
        : this.moved(start, index, { x: start[index].x + across, y: start[index].y + down }, free);
      this.guides.replaceChildren();
      this.settleOn(shown);
      announce(`${event.currentTarget.getAttribute('aria-label')}: ${this.said(this.places[index])}`);
    }
  }

  /**
   * A tile's options: how it shows its view, its size, its series and values and how each
   * series is drawn, its name or text, its order, a copy, and removing it.
   */
  tileMenu(index, at) {
    const tile = this.editing.tiles[index];
    const free = this.editing.layout === 'free';
    const text = tile.show === 'text';
    const charted = CHARTS.has(tile.show ?? 'bars');
    const set = (change) => this.change(() => Object.assign(tile, change));
    // The series of the chart's last answer, by name, each to be drawn as a line if asked.
    const last = this.lastShown.get(index);
    const names = AXES.has(tile.show ?? 'bars') && last?.tile.cube === tile.cube && last.tile.view === tile.view
      ? seriesNames(shape(last.result, Boolean(tile.by_columns)))
      : [];
    const sized = (w, h) => this.change(() => {
      const places = placesOf(this.editing);
      const [leastW, leastH] = sizeOf(tile).least;
      const size = { w: Math.max(w, leastW), h: Math.max(h, leastH) };
      if (free) Object.assign(places[index], onPage({ ...places[index], ...size }));
      else resizeOnGrid(places, places[index], size);
      this.editing.tiles.forEach((each, other) => {
        each.place = kept(places[other], free);
      });
    });
    menu([
      text ? null : { heading: 'Show as' },
      ...(text ? [] : Object.entries(KINDS).filter(([kind]) => kind !== 'text').map(([kind, [label, iconName]]) => ({
        label: (tile.show ?? 'bars') === kind ? `${label} (shown)` : label,
        icon: (tile.show ?? 'bars') === kind ? 'check' : iconName,
        action: () => set({ show: kind }),
      }))),
      charted ? '-' : null,
      // Its series turned the other way are others: none of them is drawn as a line.
      charted ? { label: tile.by_columns ? 'A series for each row' : 'A series for each column', icon: 'swap', action: () => set({ by_columns: !tile.by_columns, lines: undefined, second_axis: undefined }) } : null,
      charted ? { label: tile.labels ? 'Values off the marks' : 'Values on the marks', icon: 'hash', action: () => set({ labels: !tile.labels }) } : null,
      names.length > 1 ? { label: 'How its series are drawn…', icon: 'explore', action: () => this.drawnAs(index, names) } : null,
      { heading: 'Size' },
      ...PRESETS.map(([label, w, h]) => ({ label: `${label}, ${w} × ${h}`, icon: 'columns', action: () => sized(w, h) })),
      '-',
      { label: text ? 'Change the text…' : 'Rename…', icon: 'edit', action: () => this.retell(index) },
      free ? { label: 'Bring to front', icon: 'front', disabled: index === this.editing.tiles.length - 1, action: () => this.restack(index, this.editing.tiles.length - 1) } : null,
      free ? { label: 'Send to back', icon: 'behind', disabled: index === 0, action: () => this.restack(index, 0) } : null,
      { label: 'Duplicate', icon: 'copy', action: () => this.addTile(structuredClone({ ...tile, place: undefined })) },
      { label: 'Remove', icon: 'trash', action: () => this.removeTile(index) },
    ], at);
  }

  /** Names a tile anew, or for a tile of text, says it anew. */
  async retell(index) {
    const tile = this.editing.tiles[index];
    const text = tile.show === 'text';
    const field = text
      ? h('textarea', { class: 'textarea words', id: 'tile-text', rows: 8 }, tile.text)
      : h('input', { class: 'input', id: 'tile-title', maxlength: 120, placeholder: tile.view, value: tile.title ?? '' });
    const told = await dialog({
      title: text ? 'The tile\'s text' : 'Name the tile',
      confirm: 'Done',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: field.id }, text ? 'Text' : 'Title'), field),
        h('p', { class: 'hint' }, text
          ? 'A line that begins with # and a space is a heading, one that begins with - and a space an item of a list, and words between ** are strong. An empty line ends a paragraph.'
          : 'Left empty, the tile is named for its view.')),
      onsubmit: () => {
        if (text && !field.value.trim()) throw invalid(field, 'Say something, or remove the tile.');
        return field.value;
      },
    });
    if (told === null) return;
    this.change(() => {
      if (text) tile.text = told.trim();
      else if (told.trim()) tile.title = told.trim();
      else delete tile.title;
    });
  }

  /** Asks how each of a chart's series, `names`, is drawn: as the rest, or as a line over them or at the right. */
  async drawnAs(index, names) {
    const tile = this.editing.tiles[index];
    const drawn = await askDrawn(tile.show ?? 'bars', names, { lines: tile.lines, second: tile.second_axis });
    if (drawn) this.change(() => Object.assign(tile, { lines: drawn.lines, second_axis: drawn.second }));
  }

  /** Puts a tile laid out freely over the others, or beneath them: later tiles are drawn over earlier ones. */
  restack(index, to) {
    this.change(() => {
      const [tile] = this.editing.tiles.splice(index, 1);
      this.editing.tiles.splice(to, 0, tile);
    });
  }

  removeTile(index) {
    const named = this.editing.tiles[index].title || this.editing.tiles[index].view || 'Text';
    this.change(() => {
      this.editing.tiles.splice(index, 1);
    });
    (this.cards?.[Math.min(index, this.cards.length - 1)] ?? this.target.querySelector('.page-head .toolbar .button.primary'))?.focus();
    announce(`Removed ${named}. ${MOD} Z brings it back.`);
  }

  /** Adds a tile, at `place` or the first free one, as large as its kind is made. */
  addTile(tile, place = null) {
    const size = sizeOf(tile);
    this.change(() => {
      const places = placesOf(this.editing);
      const free = this.editing.layout === 'free';
      const at = place ?? firstFree(places, size);
      places.push({ ...at });
      if (!free && place) moveOnGrid(places, places.at(-1), place);
      this.editing.tiles.push(tile);
      this.editing.tiles.forEach((each, index) => {
        each.place = kept(places[index], free);
      });
    });
    const card = this.cards.at(-1);
    card?.focus({ preventScroll: true });
    card?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    announce(`Added ${tile.title || tile.view || 'a tile of text'}`);
  }

  toggleAdding() {
    this.adding = !this.adding;
    this.draw();
    if (this.adding) this.target.querySelector('.dash-drawer input')?.focus();
  }

  /**
   * The views there are to add, and a tile of text: chosen, one takes the first free place;
   * dragged onto the board, it takes the place it is brought to, the others making way.
   */
  drawer() {
    const search = h('input', { class: 'input', type: 'search', placeholder: 'Find a view', 'aria-label': 'Find a view to add' });
    const list = h('div', { class: 'drawer-list' });
    const item = (tile, name, detail, iconName) => h('button', {
      class: 'drawer-item',
      type: 'button',
      title: 'Choose it to add it, or drag it onto the dashboard',
      onclick: (event) => {
        if (event.detail === 0 || !this.dragged) this.addTile(structuredClone(tile));
      },
      onpointerdown: (event) => this.bringIn(event, tile),
    }, h('span', { class: 'drawer-icon' }, icon(iconName, 18)), h('span', { class: 'grow' }, h('strong', {}, name), h('span', { class: 'muted' }, detail)));
    const draw = () => {
      const words = search.value.trim().toLowerCase();
      const cubes = this.definition.cubes
        .map((cube) => ({ cube, views: Object.keys(cube.views ?? {}).filter((view) => !words || `${cube.name} ${view}`.toLowerCase().includes(words)) }))
        .filter(({ views }) => views.length);
      mount(list,
        h('div', { class: 'drawer-section' }, 'Notes'),
        item({ show: 'text', text: '# A heading\nWhat the tiles beside it show, and why.' }, 'Text', 'A heading and a few lines', 'text'),
        cubes.length
          ? cubes.map(({ cube, views }) => [
            h('div', { class: 'drawer-section' }, cube.name),
            views.map((view) => {
              // A view saved from its chart is shown as that chart, drawn so; any other as bars.
              const { kind = 'bars', ...drawn } = cube.views[view].chart ?? {};
              return item({ cube: cube.name, view, show: kind, ...drawn }, view, 'Saved view', KINDS[kind]?.[1] ?? 'chart');
            }),
          ])
          : h('p', { class: 'hint' }, words ? 'No view has those words.' : 'No cube has a saved view yet. Save a view in Explore first.'));
    };
    search.addEventListener('input', draw);
    draw();
    return h('aside', { class: 'card dash-drawer', 'aria-label': 'Add to the dashboard' },
      h('div', { class: 'drawer-head' }, h('h2', {}, 'Add'), h('button', { class: 'button small quiet icon', 'aria-label': 'Close the list', onclick: () => this.toggleAdding() }, icon('close', 16))),
      h('div', { class: 'search' }, icon('search', 16), search),
      list);
  }

  /** Drags a tile from the list onto the board: where it is let go over the board, it is added. */
  bringIn(event, tile) {
    if (event.button !== 0 || this.narrow) return;
    const button = event.currentTarget;
    const from = { x: event.clientX, y: event.clientY };
    const size = sizeOf(tile);
    const free = this.editing.layout === 'free';
    let token = null;
    let place = null;
    this.dragged = false;
    button.setPointerCapture(event.pointerId);
    const follow = (moveEvent) => {
      if (!this.dragged && Math.hypot(moveEvent.clientX - from.x, moveEvent.clientY - from.y) < 4) return;
      if (!this.dragged) {
        this.dragged = true;
        token = document.body.appendChild(h('div', { class: 'drag-token' }, button.querySelector('.drawer-icon').cloneNode(true), button.querySelector('strong').textContent));
      }
      token.style.left = `${moveEvent.clientX}px`;
      token.style.top = `${moveEvent.clientY}px`;
      const board = this.board?.isConnected ? this.board.getBoundingClientRect() : null;
      const over = board && moveEvent.clientX >= board.left && moveEvent.clientX <= board.right && moveEvent.clientY >= board.top && moveEvent.clientY <= board.bottom + 200;
      token.classList.toggle('over', Boolean(over));
      if (!over) {
        place = null;
        this.place(this.places);
        this.ghost.hidden = true;
        return;
      }
      const unit = this.units();
      const at = onPage({ x: (moveEvent.clientX - board.left) / unit.across - size.w / 2, y: (moveEvent.clientY - board.top) / unit.down - 1, w: size.w, h: size.h });
      place = free && !this.snapping ? at : { ...at, x: Math.round(at.x), y: Math.round(at.y) };
      const places = [...this.places.map((each) => ({ ...each })), { ...place }];
      if (!free) moveOnGrid(places, places.at(-1), place);
      places.slice(0, -1).forEach((each, index) => stand(this.cards[index], each));
      stand(this.ghost, places.at(-1));
      this.ghost.hidden = false;
      this.fitHeight(places);
      place = places.at(-1);
    };
    const end = () => {
      button.removeEventListener('pointermove', follow);
      button.removeEventListener('pointerup', end);
      button.removeEventListener('pointercancel', end);
      token?.remove();
      if (!this.dragged) return;
      if (place) this.addTile(structuredClone(tile), place);
      else {
        this.place(this.places);
        this.ghost.hidden = true;
      }
      // The click that follows letting go adds nothing more.
      setTimeout(() => {
        this.dragged = false;
      });
    };
    button.addEventListener('pointermove', follow);
    button.addEventListener('pointerup', end);
    button.addEventListener('pointercancel', end);
  }

  /** Saves the dashboard being edited, against the model version it was made from. */
  async save() {
    const dashboard = this.editing;
    dashboard.name = dashboard.name.trim();
    if (!dashboard.name) {
      notify('Give the dashboard a name.', 'bad');
      return;
    }
    const free = dashboard.layout === 'free';
    const places = placesOf(dashboard);
    const clean = {
      name: dashboard.name,
      ...(dashboard.description?.trim() ? { description: dashboard.description.trim() } : {}),
      ...(dashboard.filters?.length ? { filters: dashboard.filters } : {}),
      ...(free ? { layout: 'free' } : {}),
      tiles: dashboard.tiles.map((tile, index) => ({
        ...(tile.title?.trim() ? { title: tile.title.trim() } : {}),
        ...(tile.show === 'text' ? { text: tile.text } : { cube: tile.cube, view: tile.view }),
        show: tile.show ?? 'bars',
        place: kept(places[index], free),
        ...(tile.by_columns ? { by_columns: true } : {}),
        ...(tile.labels ? { labels: true } : {}),
        ...(tile.lines?.length ? { lines: tile.lines } : {}),
        ...(tile.second_axis?.length ? { second_axis: tile.second_axis } : {}),
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
    this.answers.clear();
    this.lastShown.clear();
    this.definition = await model();
    this.editing = null;
    this.fresh = false;
    this.adding = false;
    this.name = this.list().find((dashboard) => dashboard.name === name)?.name ?? this.list()[0]?.name ?? null;
    const path = this.name ? `/dashboards?name=${encodeURIComponent(this.name)}` : '/dashboards';
    if (path === location.pathname + location.search) this.draw();
    else go(path);
  }
}

/**
 * Lets a tile let go come to its place from where it was let go, rather than jump there: it
 * is put in its place by `move` at once, drawn where it was, then slid the rest of the way.
 */
function glide(element, move) {
  const from = element.getBoundingClientRect();
  element.style.transition = 'none';
  element.style.transform = '';
  move();
  const to = element.getBoundingClientRect();
  element.style.transform = `translate(${from.left - to.left}px, ${from.top - to.top}px)`;
  element.getBoundingClientRect();
  element.style.transition = '';
  element.style.transform = '';
}
