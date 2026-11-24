// Dimensions as the pages use them, each fetched again only once it has changed, and the
// dialog that chooses one member of a dimension from its tree.

import { get, modelVersion, part } from './api.js';
import { model } from './app.js';
import { marked, openButtons, reveal, Rows, searchBox, treeKey, treeRow, treeRows, twisty } from './tree.js';
import { dialog, h, icon } from './ui.js';

/**
 * Each dimension fetched, by its name: the model version it last changed in, as the outline
 * says, and the dimension, or the fetching of it. A dimension is fetched again only once it
 * has changed, and the copy before let go: one of a million elements holds some hundreds of
 * megabytes of the page, and saving a dashboard changes the model but no dimension.
 */
const loaded = new Map();

/**
 * What an element has none of: one empty list and one empty map for all, never changed. Made
 * afresh for each of a million leaves, they held 84 MB more of the page's memory, a third.
 */
const NONE = Object.freeze([]);
const NOTHING = Object.freeze({});

/**
 * A dimension with its elements, each by name in `byName`, and the names of its top
 * elements in `roots`; fetched again only once it has changed.
 */
export async function dimension(name) {
  const changed = (await model()).dimensions.find((each) => each.name === name)?.changed_in ?? modelVersion;
  if (loaded.get(name)?.changed !== changed) {
    const fetching = fetched(name);
    loaded.set(name, { changed, dimension: fetching });
    // One that could not be fetched is not kept, to be asked for again.
    fetching.catch(() => loaded.get(name)?.dimension === fetching && loaded.delete(name));
  }
  return loaded.get(name).dimension;
}

/** A dimension as the server says it, made ready to be looked through. */
async function fetched(name) {
  const view = await get(`/dimensions/${part(name)}`);
  // In one pass, as a dimension may hold a million: what the server leaves out of an
  // element, it having none, filled in empty; each by name; and those at the top.
  const byName = new Map();
  const roots = [];
  for (const element of view.elements) {
    element.parents ??= NONE;
    element.children ??= NONE;
    element.attributes ??= NOTHING;
    byName.set(element.name, element);
    if (!element.parents.length) roots.push(element.name);
  }
  return { ...view, byName, roots };
}

/**
 * Asks for one member of a dimension, from its tree: searched in place, opened and closed,
 * moved about by the keys, with `current` chosen to begin with. Double-click or Enter takes
 * a member at once. Resolves with the member's name, or null if dismissed.
 */
export async function chooseMember(dim, { title, current, hint = 'Double-click a member, or press Enter on it, to choose it at once.' }) {
  const name = dim.name;
  let chosen = current ?? dim.roots[0];
  const children = (element) => (dim.byName.get(element)?.children ?? []).map((child) => child.name).filter((child) => dim.byName.has(child));
  const parents = (element) => dim.byName.get(element)?.parents ?? [];
  const roots = dim.roots;
  // Open at the top members' children, and down to the member chosen.
  let open = new Set([...roots, ...reveal({ roots, children, parents, matches: new Set([chosen]) })]);
  let found = null;
  let words = '';
  let rows = [];
  // The row the keys act on.
  let at = 0;
  const tree = h('div', { class: 'picker-tree', role: 'tree', 'aria-label': `Members of ${name}` });
  const none = h('p', { class: 'muted tree-none', hidden: true }, 'Nothing matches.');
  // Choosing a member and taking it at once: the dialog's form is the tree's.
  const apply = () => tree.closest('form')?.requestSubmit();
  const toggle = (names, opened) => {
    const into = found ?? open;
    for (const each of names) {
      if (opened) into.add(each);
      else into.delete(each);
    }
  };
  const choose = (element) => {
    chosen = element;
    draw(element);
  };
  const list = new Rows(tree, (index) => {
    const row = rows[index];
    return treeRow(row, row.name === chosen, {
      class: `picker-row${row.context ? ' context' : ''}`,
      tabindex: index === at ? '0' : '-1',
      dataset: { index },
      onclick: () => choose(row.name),
      ondblclick: () => {
        chosen = row.name;
        apply();
      },
    },
    twisty(row, (opened) => {
      toggle([row.name], opened);
      draw(row.name);
    }),
    h('span', { class: 'choice', 'aria-hidden': 'true' }),
    icon(dim.byName.get(row.name).type === 'consolidated' ? 'sigma' : 'member', 14),
    h('span', { class: 'grow' }, row.match ? marked(row.name, words) : row.name),
    row.kids ? h('span', { class: 'muted' }, String(row.kids)) : null);
  });
  tree.append(list.element, none);
  // One member is chosen, so the choice follows the keys.
  tree.addEventListener('keydown', (event) => {
    const row = event.target.closest('.picker-row');
    if (!row) return;
    const index = Number(row.dataset.index);
    if (event.key === ' ') {
      event.preventDefault();
      choose(rows[index].name);
      return;
    }
    treeKey(event, rows, index, {
      go: choose,
      open: (names, opened) => {
        toggle(names, opened);
        draw(rows[index].name);
      },
      act: (target) => {
        chosen = target;
        apply();
      },
      page: list.page,
    });
  });
  const draw = (focus) => {
    const matches = words ? new Set(dim.elements.filter((element) => element.name.toLowerCase().includes(words)).map((element) => element.name)) : null;
    ({ rows } = treeRows({ roots, children, parents, open: found ?? open, matches }));
    at = rows.findIndex((row) => row.name === (focus ?? chosen));
    if (at < 0) at = Math.max(0, rows.findIndex((row) => row.match));
    list.show(rows.length);
    none.hidden = rows.length > 0;
    search.found(matches?.size ?? 0, Boolean(matches));
    if (focus) list.reveal(at)?.focus();
  };
  const search = searchBox({
    label: `Find in ${name}`,
    onsearch: (next) => {
      words = next;
      found = words ? reveal({ roots, children, parents, matches: new Set(dim.elements.filter((element) => element.name.toLowerCase().includes(words)).map((element) => element.name)) }) : null;
      draw();
    },
    // Down goes to the first member found, or, with no search, to the one chosen.
    ondown: () => {
      const first = words ? rows.find((row) => !row.context) : null;
      if (first) chosen = first.name;
      draw(chosen);
    },
  });
  const levels = [...new Set(dim.elements.map((element) => element.level))].sort((a, b) => b - a);
  const reopen = (names) => {
    if (found) found = new Set(names);
    else open = new Set(names);
    draw();
  };
  const opening = openButtons({
    all: () => reopen(dim.elements.filter((element) => element.children.length).map((element) => element.name)),
    none: () => reopen([]),
    levels: levels.map((level) => [level, dim.levels?.[level] || (level === 0 ? 'Leaves' : `Level ${level}`)]),
    to: (level) => reopen(dim.elements.filter((element) => element.level > level).map((element) => element.name)),
  });
  draw();
  const asked = dialog({
    title,
    confirm: 'Apply',
    body: h('div', { class: 'picker' },
      h('div', { class: 'picker-tools' }, search.box, opening),
      tree,
      h('p', { class: 'hint' }, hint)),
    onsubmit: () => chosen,
  });
  // Open, the tree shows the member chosen.
  list.reveal(at);
  return asked;
}
