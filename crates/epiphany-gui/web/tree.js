// Trees of members, as the set editor, the member picker and the dimension editor show them:
// the rows to draw, given what is open and what is searched for; the members to open to
// show what was found; long lists drawn only where they can be seen; the keys that move
// about a tree; the buttons that open and close it; and the words found, marked.

import { h, icon, printsWhole } from './ui.js';

/** How tall a row of a list or tree is, in pixels: every row the same, to draw only those in view. */
export const ROW = 28;

/** How far a row of a tree is indented for each level beneath the top, in pixels, as the styles have it. */
export const INDENT = 18;

/**
 * The rows a tree shows, in order, each `{ name, parent, depth, kids, open, match, context,
 * position, siblings }`. Browsing, they are the roots and the children of the members open.
 * Searching, with `matches` the names found, they are the matches and the members above
 * them, which are `context` unless they match too; a match that is opened shows every
 * child. `position` and `siblings` place a row among the rows shown beside it.
 */
export function treeRows({ roots, children, open, matches = null, parents = null }) {
  const leads = parents && matches ? climbedTo(parents, matches) : leadsTo(children, matches);
  const rows = [];
  const visit = (name, parent, depth, trail, whole) => {
    if (trail.has(name)) return;
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
  // Each row's place among the rows shown beside it, for those who hear the tree read.
  const groups = [];
  const levels = [];
  for (const row of rows) {
    levels.length = row.depth + 1;
    if (!levels[row.depth]) groups.push((levels[row.depth] = []));
    row.position = levels[row.depth].push(row);
  }
  for (const group of groups) for (const row of group) row.siblings = group.length;
  return { rows };
}

/**
 * The members to open so that every match shows: each with a match beneath it. Where each
 * member's `parents` are known, those above the matches, climbed to from them alone: a search
 * of a million elements need not look beneath every one. Where members are beneath themselves,
 * as no saved hierarchy is, the climb opens a member of the loop the walk down leaves shut.
 */
export function reveal({ roots, children, matches, parents = null }) {
  if (parents) {
    const open = new Set();
    const climb = (name) => {
      for (const parent of parents(name)) {
        if (open.has(parent)) continue;
        open.add(parent);
        climb(parent);
      }
    };
    for (const name of matches) climb(name);
    return open;
  }
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

/** Whether a member is a match or above one: the members above them climbed to from the matches. */
function climbedTo(parents, matches) {
  const above = reveal({ matches, parents });
  return (name) => matches.has(name) || above.has(name);
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

/** Text with each place the words occur marked: `words`, in lower case, a string or several. */
export function marked(text, words) {
  const value = String(text);
  const lower = value.toLowerCase();
  const sought = [words].flat().filter(Boolean);
  // Where lower case changes the length, the places found would not line up.
  if (!sought.length || lower.length !== value.length) return value;
  const found = new Array(value.length).fill(false);
  for (const word of sought) {
    for (let at = lower.indexOf(word); at >= 0; at = lower.indexOf(word, at + word.length)) found.fill(true, at, at + word.length);
  }
  const parts = [];
  for (let from = 0, to = 0; from < value.length; from = to) {
    while (to < value.length && found[to] === found[from]) to += 1;
    parts.push(found[from] ? h('mark', {}, value.slice(from, to)) : value.slice(from, to));
  }
  return parts;
}

/** The most rows a list draws whole for printing. */
const PRINTED = 10_000;

/**
 * The rows of a long list, drawn only where they can be seen in `scroller`, so that a list of
 * any length shows at once and scrolls smoothly. Every row is `row` pixels tall, `ROW` unless said; `draw(index)`
 * makes a row's element. The row holding the focus is kept, wherever it is, so that the keys
 * stay on it as the list scrolls. `covered()` is how much of the scroller's top a heading
 * pinned there hides.
 */
export class Rows {
  constructor(scroller, draw, { covered = () => 0, row = ROW } = {}) {
    this.scroller = scroller;
    this.row = row;
    this.draw = draw;
    this.covered = covered;
    this.count = 0;
    this.drawn = new Map();
    this.element = h('div', { class: 'rows' });
    printsWhole(this.element, () => this.drawWhole(), () => this.update());
    let queued = false;
    scroller.addEventListener('scroll', () => {
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        this.update();
      });
    });
    new ResizeObserver(() => this.update()).observe(scroller);
  }

  /** Shows `count` rows, drawn afresh. */
  show(count) {
    for (const element of this.drawn.values()) element.remove();
    this.drawn.clear();
    this.count = count;
    this.element.style.height = `${count * this.row}px`;
    this.update();
  }

  /** How many rows the scroller shows at once. */
  get page() {
    return Math.max(1, Math.floor(this.scroller.clientHeight / this.row) - 1);
  }

  /**
   * Where the rows begin, down what the scroller scrolls: as laid out, whatever transforms
   * it, such as a dialog's opening. The styles make the scroller the rows' offset parent.
   */
  offset() {
    return this.element.offsetTop;
  }

  /** Draws the rows in view, a few beyond, and lets go of the rest but the one with the focus. */
  update() {
    if (!this.element.isConnected) return;
    const top = this.scroller.scrollTop - this.offset();
    const first = Math.max(0, Math.floor(top / this.row) - 8);
    const last = Math.min(this.count, Math.ceil((top + this.scroller.clientHeight) / this.row) + 8);
    for (const [index, element] of this.drawn) {
      if ((index < first || index >= last) && !element.contains(document.activeElement)) {
        element.remove();
        this.drawn.delete(index);
      }
    }
    for (let index = first; index < last; index += 1) if (!this.drawn.has(index)) this.place(index);
  }

  /**
   * Every row, for paper, which has no scrolling: up to PRINTED of them, past which drawing
   * them would hold the page up, and the printout says that only the rows in view are on it.
   * Those drawn stay where they are, the focus with them.
   */
  drawWhole() {
    const whole = this.count <= PRINTED;
    this.element.toggleAttribute('data-part', !whole);
    this.element.dataset.rows = this.count.toLocaleString();
    if (!whole) return;
    let next = this.element.firstElementChild;
    for (let index = 0; index < this.count; index += 1) {
      const drawn = this.drawn.get(index);
      if (drawn) {
        next = drawn.nextElementSibling;
        continue;
      }
      const element = this.draw(index);
      element.style.top = `${index * this.row}px`;
      this.element.insertBefore(element, next);
      this.drawn.set(index, element);
    }
  }

  /** Draws a row in its place, among the others in order, so that the page reads in order. */
  place(index) {
    const element = this.draw(index);
    element.style.top = `${index * this.row}px`;
    let next = null;
    for (const [other, drawn] of this.drawn) if (other > index && (!next || other < next.index)) next = { index: other, drawn };
    this.element.insertBefore(element, next?.drawn ?? null);
    this.drawn.set(index, element);
    return element;
  }

  /** A row's element, scrolled into view and drawn if it was not; null past the ends. */
  reveal(index) {
    if (index < 0 || index >= this.count) return null;
    const top = this.offset() + index * this.row;
    const covered = this.covered();
    if (top < this.scroller.scrollTop + covered) this.scroller.scrollTop = top - covered;
    else if (top + this.row > this.scroller.scrollTop + this.scroller.clientHeight) this.scroller.scrollTop = top + this.row - this.scroller.clientHeight;
    this.update();
    return this.drawn.get(index) ?? this.place(index);
  }

  /** A row's element, where it is drawn. */
  at(index) {
    return this.drawn.get(index) ?? null;
  }
}

let typed = '';
let typedAt = 0;

/**
 * Moves about a tree, as the WAI-ARIA tree pattern has it, from the row `at` of `rows` (as
 * `treeRows` gives them). Up and Down, Page Up and Page Down, Home and End move; Right opens
 * a row, or moves to its first child; Left closes it, or moves to its parent; `*` opens it
 * and its siblings; Enter acts on it; and typing moves to the next row whose name begins
 * with what was typed. `go(name, event)` moves to a row, `open(names, opened)` opens or
 * closes rows, `act(name, event)` acts on one, and `page` is how many rows a page moves.
 * Returns whether the key was taken.
 */
export function treeKey(event, rows, at, { go, open, act, page = 10 }) {
  const row = rows[at];
  if (!row || event.altKey || event.ctrlKey || event.metaKey) return false;
  const parent = () => {
    for (let index = at - 1; index >= 0; index -= 1) if (rows[index].depth < row.depth) return index;
    return -1;
  };
  let target = -1;
  switch (event.key) {
    case 'ArrowDown':
      target = Math.min(at + 1, rows.length - 1);
      break;
    case 'ArrowUp':
      target = Math.max(at - 1, 0);
      break;
    case 'PageDown':
      target = Math.min(at + page, rows.length - 1);
      break;
    case 'PageUp':
      target = Math.max(at - page, 0);
      break;
    case 'Home':
      target = 0;
      break;
    case 'End':
      target = rows.length - 1;
      break;
    case 'ArrowRight':
      if (row.kids && !row.open) open([row.name], true);
      else if (row.kids) target = at + 1;
      else return false;
      break;
    case 'ArrowLeft':
      if (row.kids && row.open) open([row.name], false);
      else if ((target = parent()) < 0) return false;
      break;
    case '*': {
      const siblings = [];
      for (let index = parent() + 1; index < rows.length; index += 1) {
        if (rows[index].depth < row.depth) break;
        if (rows[index].depth === row.depth && rows[index].kids && !rows[index].open) siblings.push(rows[index].name);
      }
      if (siblings.length) open(siblings, true);
      break;
    }
    case 'Enter':
      if (!act) return false;
      act(row.name, event);
      break;
    default: {
      if (event.key.length !== 1 || event.key === ' ') return false;
      // Letters typed in quick succession are one word; the same letter again moves on.
      const now = Date.now();
      typed = now - typedAt > 700 ? event.key.toLowerCase() : typed + event.key.toLowerCase();
      typedAt = now;
      const again = typed.length === 1 || [...typed].every((letter) => letter === typed[0]);
      const word = again ? typed[0] : typed;
      for (let step = again ? 1 : 0; step <= rows.length; step += 1) {
        const index = (at + step) % rows.length;
        if (rows[index].name.toLowerCase().startsWith(word)) {
          target = index;
          break;
        }
      }
    }
  }
  event.preventDefault();
  if (target >= 0 && target !== at) go(rows[target].name, event);
  return true;
}

/**
 * A row of a tree: saying what it is to those who hear it read, its level and its place
 * among the rows beside it, and indented to its depth. `props` and `children` are its own.
 */
export function treeRow(row, selected, props, ...children) {
  const element = h('div', {
    role: 'treeitem',
    'aria-level': row.depth + 1,
    'aria-setsize': row.siblings,
    'aria-posinset': row.position,
    'aria-selected': String(selected),
    'aria-expanded': row.kids ? String(row.open) : null,
    ...props,
    class: `tree-row ${props.class ?? ''}`,
    dataset: { name: row.name, ...props.dataset },
  }, ...children);
  element.style.setProperty('--depth', row.depth);
  return element;
}

/** The button that opens or closes a row of a tree, or the space for one; `toggle(opened)` does it. */
export function twisty(row, toggle) {
  if (!row.kids) return h('span', { class: 'twisty' });
  return h('button', {
    class: 'twisty',
    type: 'button',
    tabindex: '-1',
    'aria-label': row.open ? `Close ${row.name}` : `Open ${row.name}`,
    onclick: (event) => {
      event.stopPropagation();
      toggle(!row.open);
    },
  }, icon(row.open ? 'down' : 'chevron', 12));
}

/**
 * Buttons that open every member of a tree and close them all and, where `levels` names
 * more than one, a choice of the level to open it down to: kept together, so that they wrap
 * as one. `levels` are `[level, name]`, highest first.
 */
export function openButtons({ all, none, levels = [], to }) {
  return h('div', { class: 'open-group', role: 'group', 'aria-label': 'Open and close' },
    h('button', { class: 'button small quiet icon', type: 'button', title: 'Open every member', 'aria-label': 'Open every member', onclick: all }, icon('expand', 16)),
    h('button', { class: 'button small quiet icon', type: 'button', title: 'Close every member', 'aria-label': 'Close every member', onclick: none }, icon('collapse', 16)),
    levels.length > 2 ? h('select', {
      class: 'select small-select',
      'aria-label': 'Open down to a level',
      onchange: (event) => {
        if (event.target.value !== '') to(Number(event.target.value));
        event.target.value = '';
      },
    }, h('option', { value: '' }, 'Open to…'), levels.slice(1).map(([level, name]) => h('option', { value: level }, name))) : null);
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
