// Dimensions as the pages use them, each fetched again only once it has changed, whole or,
// where large, in parts as they are shown; and the dialog that chooses one member of a
// dimension from its tree.

import { get, modelVersion, part, post } from './api.js';
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

/** The most members a search of a dimension held in parts finds, for the picker to show. */
const FOUND_MOST = 200;

/**
 * What an element has none of: one empty list and one empty map for all, never changed. Made
 * afresh for each of a million leaves, they held 84 MB more of the page's memory, a third.
 */
const NONE = Object.freeze([]);
const NOTHING = Object.freeze({});

/**
 * The most elements a dimension has for Explore to hold it whole. A larger one is held in
 * parts, each fetched as it is shown: whole, a dimension of a million elements was 109 MB to
 * fetch and 300 MB of the page before Explore showed anything.
 */
const WHOLE_MOST = 50_000;

/** Where this browser is told to hold every dimension in parts, however small: the journeys' way to go through them so. */
const IN_PARTS = 'epiphany.dimensions-in-parts';

/** Each dimension held in parts, by its name, as `loaded` keeps those held whole. */
const parted = new Map();

/** An element as the pages read it: what the server leaves out of it, it having none, filled in empty. */
function prepared(element) {
  element.parents ??= NONE;
  element.children ??= NONE;
  element.attributes ??= NOTHING;
  return element;
}

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
    byName.set(element.name, prepared(element));
    if (!element.parents.length) roots.push(element.name);
  }
  return { ...view, byName, roots };
}

/**
 * A dimension as Explore holds it: whole where it is small, as `dimension` gives it; in parts
 * where it is large, its top elements to begin with and the rest fetched by `fill` as they are
 * shown. Either way it has `byName`, `roots` and `elements`; held in parts, `part` is true and
 * they hold what has been fetched, and its subsets are left to the set editor, which takes it
 * whole.
 */
export async function heldDimension(name) {
  const outline = (await model()).dimensions.find((each) => each.name === name);
  if ((outline?.element_count ?? 0) <= WHOLE_MOST && localStorage.getItem(IN_PARTS) !== 'on') return dimension(name);
  const changed = outline?.changed_in ?? modelVersion;
  if (parted.get(name)?.changed !== changed) {
    const fetching = post(`/dimensions/${part(name)}/elements`, { roots: true }).then((top) => {
      const dim = { name: top.name, type: top.type, levels: top.levels, attributes: top.attributes, size: top.size, subsets: {}, part: true, byName: new Map(), elements: [], roots: top.roots };
      take(dim, top.elements);
      return dim;
    });
    parted.set(name, { changed, dimension: fetching });
    // One that could not be fetched is not kept, to be asked for again.
    fetching.catch(() => parted.get(name)?.dimension === fetching && parted.delete(name));
  }
  return parted.get(name).dimension;
}

/**
 * Holds elements fetched of a dimension held in parts, each once: one whose children were
 * counted takes their list when it comes, as it is opened.
 */
function take(dim, elements) {
  for (const element of elements) {
    const held = dim.byName.get(element.name);
    if (!held) {
      dim.byName.set(element.name, prepared(element));
      dim.elements.push(element);
    } else if (element.children && held.children === NONE) {
      held.children = element.children;
    }
  }
}

/**
 * How many children an element has: as listed, or, in a dimension held in parts, as counted
 * where they are not listed yet, the element not opened.
 */
export const childCount = (element) => element?.child_count ?? element?.children.length ?? 0;

/**
 * Fetches what a page is to show of a dimension held in parts, leaving out what it holds
 * already: the elements `names` names, with every element above them where `ancestors`; the
 * children of those `childrenOf` names; everything beneath those `beneath` names; and the
 * first `most` whose names hold `find`, with the elements above them. Of one held whole
 * nothing is fetched, and nothing given. Gives the names found, where asked to find.
 */
export async function fill(dim, { names = [], ancestors = false, childrenOf = [], beneath = [], find = '', most = 50, quiet = false } = {}) {
  if (!dim?.part) return null;
  const above = (name) => {
    const element = dim.byName.get(name);
    return Boolean(element) && element.parents.every(above);
  };
  const unheld = (name) => {
    const element = dim.byName.get(name);
    if (!element) return true;
    const counted = element.children === NONE && childCount(element) > 0;
    return counted || element.children.some((child) => !dim.byName.has(child.name));
  };
  const asked = {
    names: [...new Set(names)].filter((name) => (ancestors ? !above(name) : !dim.byName.has(name))),
    ancestors: ancestors || undefined,
    children_of: [...new Set(childrenOf)].filter(unheld),
    descendants_of: [...new Set(beneath)],
    find: find || undefined,
    most: find ? most : undefined,
  };
  if (!asked.names.length && !asked.children_of.length && !asked.descendants_of.length && !find) return [];
  const answer = await post(`/dimensions/${part(dim.name)}/elements`, asked, { quiet });
  take(dim, answer.elements);
  return answer.found ?? [];
}

/**
 * Asks for one member of a dimension, from its tree: searched in place, opened and closed,
 * moved about by the keys, with `current` chosen to begin with. Double-click or Enter takes
 * a member at once. Resolves with the member's name, or null if dismissed.
 */
export async function chooseMember(dim, { title, current, hint = 'Double-click a member, or press Enter on it, to choose it at once.' }) {
  const name = dim.name;
  let chosen = current ?? dim.roots[0];
  // Held in parts, a dimension's members are as the server gives them, fetched as they are
  // opened; held whole, as it holds them.
  const children = (element) => {
    const names = (dim.byName.get(element)?.children ?? []).map((child) => child.name);
    return dim.part ? names : names.filter((child) => dim.byName.has(child));
  };
  const parents = (element) => dim.byName.get(element)?.parents ?? [];
  const roots = dim.roots;
  await fill(dim, { names: [chosen], ancestors: true });
  // Open at the top members' children, and down to the member chosen.
  let open = new Set([...roots, ...reveal({ roots, children, parents, matches: new Set([chosen]) })]);
  await fill(dim, { childrenOf: [...open] });
  // Members found by the server, in a dimension held in parts.
  let named = null;
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
    twisty(row, async (opened) => {
      if (opened) await fill(dim, { childrenOf: [row.name] });
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
      open: async (names, opened) => {
        if (opened) await fill(dim, { childrenOf: names });
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
  // Those whose names hold the words: of a dimension held whole, all of them; of one held in
  // parts, the first the server finds.
  const matching = () => (dim.part ? named : new Set(dim.elements.filter((element) => element.name.toLowerCase().includes(words)).map((element) => element.name)));
  const draw = (focus) => {
    const matches = words ? matching() ?? new Set() : null;
    const count = (element) => childCount(dim.byName.get(element));
    ({ rows } = treeRows({ roots, children, parents, open: found ?? open, matches, count }));
    at = rows.findIndex((row) => row.name === (focus ?? chosen));
    if (at < 0) at = Math.max(0, rows.findIndex((row) => row.match));
    list.show(rows.length);
    none.hidden = rows.length > 0;
    search.found(matches?.size ?? 0, Boolean(matches));
    if (focus) list.reveal(at)?.focus();
  };
  const search = searchBox({
    label: `Find in ${name}`,
    onsearch: async (next) => {
      words = next;
      named = dim.part && words ? new Set(await fill(dim, { find: words, most: FOUND_MOST, ancestors: true })) : null;
      // Typed on meanwhile, the newer words have the tree.
      if (words !== next) return;
      found = words ? reveal({ roots, children, parents, matches: matching() }) : null;
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
  // Every member opened, or down to a level, needs the whole of a dimension: one held in parts
  // is opened a member at a time, and searched.
  const opening = dim.part ? null : openButtons({
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
