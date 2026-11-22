// Trees of members, as the set editor, the member picker and the dimension editor show them:
// the rows to draw, given what is open and what is searched for; the members to open to
// show what was found; the keys that move about a tree; the buttons that open and close it;
// and the words found, marked where they occur.

import { h, icon } from './ui.js';

/** At most this many rows are drawn; past it, the reader is asked to search. */
export const ROW_LIMIT = 2000;

/**
 * The rows a tree shows, in order, each `{ name, parent, depth, kids, open, match, context }`.
 * Browsing, they are the roots and the children of the members open. Searching, with
 * `matches` the names found, they are the matches and the members above them, which are
 * `context` unless they match too; a match that is opened shows every child. `cut` says
 * the rows stopped at `limit`.
 */
export function treeRows({ roots, children, open, matches = null, limit = ROW_LIMIT }) {
  const leads = leadsTo(children, matches);
  const rows = [];
  let cut = false;
  const visit = (name, parent, depth, trail, whole) => {
    if (trail.has(name)) return;
    if (rows.length >= limit) {
      cut = true;
      return;
    }
    const kids = children(name);
    const match = Boolean(matches?.has(name));
    const opened = kids.length > 0 && open.has(name);
    rows.push({ name, parent, depth, kids: kids.length, open: opened, match, context: Boolean(matches) && !match && !whole });
    if (!opened) return;
    const next = new Set(trail).add(name);
    const beneath = whole || match;
    for (const kid of kids) if (!matches || beneath || leads(kid)) visit(kid, name, depth + 1, next, beneath);
  };
  for (const root of roots) if (leads(root)) visit(root, null, 0, new Set(), false);
  return { rows, cut };
}

/** The members to open so that every match shows: each with a match beneath it. */
export function reveal({ roots, children, matches }) {
  const leads = leadsTo(children, matches);
  const open = new Set();
  const visit = (name) => {
    if (open.has(name)) return;
    const kids = children(name).filter((kid) => leads(kid));
    if (!kids.length) return;
    open.add(name);
    for (const kid of kids) visit(kid);
  };
  for (const root of roots) if (leads(root)) visit(root);
  return open;
}

/** Whether a member, or any beneath it, is a match: worked out once for each. */
function leadsTo(children, matches) {
  if (!matches) return () => true;
  const known = new Map();
  const onPath = new Set();
  const leads = (name) => {
    if (known.has(name)) return known.get(name);
    // A member met again beneath itself leads nowhere new.
    if (onPath.has(name)) return false;
    onPath.add(name);
    const found = matches.has(name) || children(name).some(leads);
    onPath.delete(name);
    known.set(name, found);
    return found;
  };
  return leads;
}

/** Every member beneath those given, each once. */
export function beneath(names, children) {
  const out = new Set();
  const visit = (name) => {
    for (const kid of children(name)) {
      if (!out.has(kid)) {
        out.add(kid);
        visit(kid);
      }
    }
  };
  for (const name of names) visit(name);
  return out;
}

/** Text with each place the words occur marked. */
export function marked(text, words) {
  const value = String(text);
  const lower = value.toLowerCase();
  // Where lower case changes the length, the places found would not line up.
  if (!words || lower.length !== value.length) return value;
  const parts = [];
  let from = 0;
  for (let at = lower.indexOf(words); at >= 0; at = lower.indexOf(words, from)) {
    if (at > from) parts.push(value.slice(from, at));
    parts.push(h('mark', {}, value.slice(at, at + words.length)));
    from = at + words.length;
  }
  if (from < value.length) parts.push(value.slice(from));
  return parts;
}

let typed = '';
let typedAt = 0;

/**
 * Moves about a tree whose rows carry `data-name`, `aria-level` and, where they have
 * children, `aria-expanded`, as the WAI-ARIA tree pattern has it. Up and Down, Home and
 * End move; Right opens a row, or moves to its first child; Left closes it, or moves to its
 * parent; `*` opens it and its siblings; Enter acts on it; and typing moves to the next row
 * whose name begins with what was typed. `go(name, event)` moves to a row, `open(names,
 * opened)` opens or closes rows, and `act(name, event)` acts on one. Returns whether the
 * key was taken.
 */
export function treeKey(event, { go, open, act }) {
  const row = event.target.closest('[role=treeitem][data-name]');
  const tree = row?.closest('[role=tree]');
  if (!tree || event.altKey || event.ctrlKey || event.metaKey) return false;
  const rows = [...tree.querySelectorAll('[role=treeitem][data-name]')];
  const at = rows.indexOf(row);
  const level = (element) => Number(element.getAttribute('aria-level'));
  const expanded = row.getAttribute('aria-expanded');
  const name = row.dataset.name;
  const parent = () => rows.slice(0, at).reverse().find((other) => level(other) < level(row));
  let target = null;
  switch (event.key) {
    case 'ArrowDown':
      target = rows[Math.min(at + 1, rows.length - 1)];
      break;
    case 'ArrowUp':
      target = rows[Math.max(at - 1, 0)];
      break;
    case 'Home':
      target = rows[0];
      break;
    case 'End':
      target = rows[rows.length - 1];
      break;
    case 'ArrowRight':
      if (expanded === 'false') open([name], true);
      else if (expanded === 'true') target = rows[at + 1];
      else return false;
      break;
    case 'ArrowLeft':
      if (expanded === 'true') open([name], false);
      else if (!(target = parent())) return false;
      break;
    case '*': {
      const above = parent();
      const siblings = [];
      for (const other of rows.slice(above ? rows.indexOf(above) + 1 : 0)) {
        if (level(other) < level(row)) break;
        if (level(other) === level(row) && other.getAttribute('aria-expanded') === 'false') siblings.push(other.dataset.name);
      }
      if (siblings.length) open(siblings, true);
      break;
    }
    case 'Enter':
      if (!act) return false;
      act(name, event);
      break;
    default: {
      if (event.key.length !== 1 || event.key === ' ') return false;
      // Letters typed in quick succession are one word; the same letter again moves on.
      const now = Date.now();
      typed = now - typedAt > 700 ? event.key.toLowerCase() : typed + event.key.toLowerCase();
      typedAt = now;
      const again = typed.length === 1 || [...typed].every((letter) => letter === typed[0]);
      const start = again ? at + 1 : at;
      const order = [...rows.slice(start), ...rows.slice(0, start)];
      const word = again ? typed[0] : typed;
      target = order.find((other) => other.dataset.name.toLowerCase().startsWith(word));
    }
  }
  event.preventDefault();
  if (target && target !== row) go(target.dataset.name, event);
  return true;
}

/**
 * Buttons that open every member of a tree and close them all and, where `levels` names
 * more than one, a choice of the level to open it down to. `levels` are `[level, name]`,
 * highest first.
 */
export function openButtons({ all, none, levels = [], to }) {
  return [
    h('button', { class: 'button small quiet icon', type: 'button', title: 'Open every member', 'aria-label': 'Open every member', onclick: all }, icon('expand', 16)),
    h('button', { class: 'button small quiet icon', type: 'button', title: 'Close every member', 'aria-label': 'Close every member', onclick: none }, icon('collapse', 16)),
    levels.length > 2 ? h('select', {
      class: 'select small-select',
      'aria-label': 'Open down to a level',
      onchange: (event) => {
        if (event.target.value !== '') to(Number(event.target.value));
        event.target.value = '';
      },
    }, h('option', { value: '' }, 'Open to…'), levels.slice(1).map(([level, name]) => h('option', { value: level }, name))) : null,
  ];
}

/**
 * A search box that clears itself with Escape, rather than closing what it sits in, and
 * moves into the tree with Down; `count` shows how many were found.
 */
export function searchBox({ label, onsearch, ondown }) {
  const input = h('input', { class: 'input', type: 'search', placeholder: label, 'aria-label': label, autocomplete: 'off', spellcheck: 'false' });
  const count = h('span', { class: 'search-count', 'aria-live': 'polite' });
  input.addEventListener('input', () => onsearch(input.value.trim().toLowerCase()));
  input.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && input.value) {
      event.preventDefault();
      event.stopPropagation();
      input.value = '';
      onsearch('');
    } else if (event.key === 'ArrowDown' || event.key === 'Enter') {
      event.preventDefault();
      ondown?.();
    }
  });
  const box = h('div', { class: 'search' }, icon('search', 16), input, count);
  return {
    box,
    input,
    /** Says how many were found, or nothing when there is no search. */
    found(number, words) {
      count.textContent = words ? (number ? `${number.toLocaleString()} found` : 'None found') : '';
    },
  };
}
