// Dimensions as the pages use them, fetched once for each model version, and the dialog
// that chooses one member of a dimension from its tree.

import { get, modelVersion, part } from './api.js';
import { marked, openButtons, reveal, ROW_LIMIT, searchBox, treeKey, treeRows } from './tree.js';
import { dialog, h, icon, mount } from './ui.js';

const loaded = new Map();

/**
 * A dimension with its elements, each by name in `byName`, and the names of its top
 * elements in `roots`; fetched once for each version of the model.
 */
export async function dimension(name) {
  const key = `${modelVersion}\u0000${name}`;
  if (!loaded.has(key)) {
    const view = await get(`/dimensions/${part(name)}`);
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
  const tree = h('div', { class: 'picker-tree', role: 'tree', 'aria-label': `Members of ${name}` });
  // Choosing a member and taking it at once: the dialog's form is the tree's.
  const apply = () => tree.closest('form')?.requestSubmit();
  const toggle = (names, opened) => {
    const into = found ?? open;
    for (const each of names) {
      if (opened) into.add(each);
      else into.delete(each);
    }
  };
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
              toggle(names, opened);
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
            toggle([row.name], !row.open);
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
  return dialog({
    title,
    confirm: 'Apply',
    body: h('div', { class: 'picker' },
      h('div', { class: 'picker-tools' }, search.box, opening),
      tree,
      h('p', { class: 'hint' }, hint)),
    onsubmit: () => chosen,
  });
}
