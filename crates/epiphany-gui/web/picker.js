// Dimensions as the pages use them, fetched once for each model version, and the dialog
// that chooses one member of a dimension from its tree.

import { get, modelVersion, part } from './api.js';
import { marked, openButtons, reveal, Rows, searchBox, treeKey, treeRow, treeRows, twisty } from './tree.js';
import { dialog, h, icon } from './ui.js';

const loaded = new Map();

/**
 * A dimension with its elements, each by name in `byName`, and the names of its top
 * elements in `roots`; fetched once for each version of the model.
 */
export async function dimension(name) {
  const key = `${modelVersion}\u0000${name}`;
  if (!loaded.has(key)) {
    const view = await get(`/dimensions/${part(name)}`);
    // The server leaves out what an element has none of; the pages find it empty.
    for (const element of view.elements) {
      element.parents ??= [];
      element.children ??= [];
      element.attributes ??= {};
    }
    const byName = new Map(view.elements.map((element) => [element.name, element]));
    const roots = view.elements.filter((element) => element.parents.length === 0).map((element) => element.name);
    loaded.set(key, { ...view, byName, roots });
  }
  return loaded.get(key);
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
  const roots = dim.roots;
  // Open at the top members' children, and down to the member chosen.
  let open = new Set([...roots, ...reveal({ roots, children, matches: new Set([chosen]) })]);
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
    ({ rows } = treeRows({ roots, children, open: found ?? open, matches }));
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
      found = words ? reveal({ roots, children, matches: new Set(dim.elements.filter((element) => element.name.toLowerCase().includes(words)).map((element) => element.name)) }) : null;
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
