// Finding one among many: a search over a list drawn only where it is seen, however long it
// is, the best matches first, moved through by the keys. The palette, the cubes, views and
// dashboards offered to choose from, the model's list and the dimensions a cube is made of
// are each one, at five hundred cubes and ten thousand dimensions as at five.

import { marked, Rows, searchBox } from './tree.js';
import { h, icon } from './ui.js';

let finders = 0;
/** What marks a heading among the items shown. */
const HEADING = Symbol('heading');

/** Whether `word` begins one of the words of `text`. */
function beginsWord(text, word) {
  for (let at = text.indexOf(word); at >= 0; at = text.indexOf(word, at + 1)) {
    if (at === 0 || !/[\p{L}\p{N}]/u.test(text[at - 1])) return true;
  }
  return false;
}

/** Each item's label and what it belongs to, in lower case, worked out once. */
const lowers = new WeakMap();
function lowered(item) {
  let text = lowers.get(item);
  if (!text) lowers.set(item, (text = { label: item.label.toLowerCase(), context: item.context?.toLowerCase() ?? '' }));
  return text;
}

/**
 * The items with every word typed, in their label or else in what they belong to, the best
 * matches first: words that begin the label, then words that begin one of its words, then
 * any; matches alike stay in the order given. With nothing typed, every item, as given.
 */
export function ranked(items, words) {
  if (!words.length) return items;
  const scored = [];
  for (const item of items) {
    const { label, context } = lowered(item);
    let score = 0;
    for (const word of words) {
      const points = label.startsWith(word) ? 3 : beginsWord(label, word) ? 2 : label.includes(word) || context.includes(word) ? 1 : 0;
      if (!points) {
        score = 0;
        break;
      }
      score += points;
    }
    if (score) scored.push([score, item]);
  }
  return scored.sort((a, b) => b[0] - a[0]).map(([, item]) => item);
}

/**
 * A search box over `items`, each `{ label, icon, detail, context, group, href }`: typing
 * narrows them, the best first, and Up and Down, Page Up and Page Down move through them;
 * Enter or a click chooses one, as `choose(item)` takes it, and one with an `href` is gone
 * to. With nothing typed they stand as given, each `group` under its heading. With `chosen`,
 * a set of items, each row is ticked or not, and choosing one ticks or unticks it. `current`
 * is marked as the one there is now, and the keys begin on it. `row` is a row's height in
 * pixels; `draw(item, words)` makes a row's content where the label, icon and detail will
 * not do, and `props(item)` gives a row more of its own. Returns the `element` to place,
 * its search `input` and `refresh(items, current)` to show others.
 */
export function finder({ items, label, choose, chosen = null, current = null, row = 36, draw = null, props = null, nothing = 'Nothing matches.' }) {
  const id = `finder-${++finders}`;
  let all = items;
  let words = [];
  // The items shown, in order, with the headings of their groups among them.
  let shown = [];
  // Where the keys are, among those shown.
  let at = -1;
  const isItem = (index) => index >= 0 && index < shown.length && !shown[index][HEADING];
  const list = h('div', { class: 'finder-list', id, role: 'listbox', 'aria-label': label, 'aria-multiselectable': chosen ? 'true' : null });
  list.style.setProperty('--row', `${row}px`);
  const rows = new Rows(list, (index) => drawRow(index), { row });
  list.append(rows.element);
  const none = h('p', { class: 'finder-none', hidden: true }, nothing);
  const search = searchBox({ label, onsearch: () => show() });
  const { input } = search;
  for (const [name, value] of [['role', 'combobox'], ['aria-controls', id], ['aria-expanded', 'true'], ['aria-autocomplete', 'list']]) input.setAttribute(name, value);

  function drawRow(index) {
    const entry = shown[index];
    if (entry[HEADING]) return h('div', { class: 'finder-group', 'aria-hidden': 'true' }, entry[HEADING]);
    const { class: more, ...own } = props?.(entry) ?? {};
    return h(entry.href ? 'a' : 'div', {
      class: ['finder-item', index === at ? 'active' : '', entry === current ? 'current' : '', more ?? ''].filter(Boolean).join(' '),
      role: 'option',
      id: `${id}-${index}`,
      href: entry.href,
      'data-link': entry.href ? '' : null,
      tabindex: '-1',
      'aria-selected': String(chosen ? chosen.has(entry) : index === at),
      'aria-current': entry === current ? 'true' : null,
      onpointermove: () => activate(index),
      // The keys stay in the search as a row is pressed.
      onmousedown: (event) => event.preventDefault(),
      // A link is followed as any is; anything else is chosen here.
      onclick: entry.href ? null : () => take(index),
      ...own,
    },
    chosen ? h('span', { class: 'finder-tick' }, icon('check', 12)) : null,
    draw ? draw(entry, words) : [
      entry.icon ? icon(entry.icon, 16) : null,
      h('span', { class: 'grow' }, marked(entry.label, words)),
      entry.detail ? h('span', { class: 'finder-detail' }, entry.detail) : null,
    ]);
  }

  /** Puts the keys on a row; `reveal` scrolls it into view, as the keys do but the pointer does not. */
  function activate(index, reveal = false) {
    if (!isItem(index)) return;
    if (index !== at) {
      const was = rows.at(at);
      was?.classList.remove('active');
      if (!chosen) was?.setAttribute('aria-selected', 'false');
      at = index;
    }
    const element = reveal ? rows.reveal(at) : rows.at(at);
    if (!element) return;
    element.classList.add('active');
    if (!chosen) element.setAttribute('aria-selected', 'true');
    input.setAttribute('aria-activedescendant', element.id);
  }

  /**
   * Moves the keys to the row at `index`, held to the ends: past a heading the way `step`
   * goes, or back the other way where a heading is first.
   */
  function go(index, step) {
    const along = (from, direction) => {
      let to = from;
      while (to >= 0 && to < shown.length && !isItem(to)) to += direction;
      return isItem(to) ? to : -1;
    };
    const from = Math.max(0, Math.min(shown.length - 1, index));
    const direction = step < 0 ? -1 : 1;
    const to = along(from, direction);
    activate(to >= 0 ? to : along(from, -direction), true);
  }

  function take(index) {
    const item = shown[index];
    activate(index);
    if (item.href) {
      rows.at(index)?.click();
      return;
    }
    if (chosen) {
      if (chosen.has(item)) chosen.delete(item);
      else chosen.add(item);
      rows.at(index)?.setAttribute('aria-selected', String(chosen.has(item)));
    }
    choose?.(item);
  }

  /** Shows the items the words find, the keys staying on the one they were on, where it is still shown. */
  function show() {
    const kept = isItem(at) ? shown[at] : current;
    words = input.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
    const found = ranked(all, words);
    shown = [];
    let group = null;
    for (const item of found) {
      if (!words.length && item.group && item.group !== group) shown.push({ [HEADING]: (group = item.group) });
      shown.push(item);
    }
    search.found(found.length, words.length > 0);
    none.hidden = found.length > 0;
    list.hidden = !found.length;
    at = -1;
    input.removeAttribute('aria-activedescendant');
    rows.show(shown.length);
    const again = shown.indexOf(kept);
    if (again < 0) list.scrollTop = 0;
    go(again >= 0 ? again : 0, 1);
  }

  input.addEventListener('keydown', (event) => {
    const step = { ArrowDown: 1, ArrowUp: -1, PageDown: rows.page, PageUp: -rows.page }[event.key];
    if (step) {
      event.preventDefault();
      go(at + step, step);
    } else if (event.key === 'Enter' && isItem(at)) {
      event.preventDefault();
      take(at);
    }
  });
  // Drawn before it was in the page, the list shows the row the keys are on once it is.
  const placed = new ResizeObserver(() => {
    if (!list.clientHeight) return;
    placed.disconnect();
    activate(at, true);
  });
  placed.observe(list);
  show();
  return {
    element: h('div', { class: 'finder' }, search.box, list, none),
    input,
    /** Shows `others` in place of the items, and `now`, where given, as the current one, the keys on it. */
    refresh(others, now = undefined) {
      all = others;
      if (now !== undefined) {
        current = now;
        at = -1;
      }
      show();
    },
  };
}

/**
 * A button that stands for a select: it shows the item chosen, `now`, by its label, and
 * offers any other of `items()` in a panel, found by typing among however many. `label`
 * names what it chooses, as Cube; `chosen(label)` is told of another chosen. Its `value` is
 * the label chosen. `find` labels the panel's search.
 */
export function chooser({ items, now, label, chosen = null, find = `Find a ${label.toLowerCase()}` }) {
  const button = h('button', { class: 'button chooser', type: 'button', 'aria-haspopup': 'dialog' });
  const show = (name) => {
    button.value = name;
    button.setAttribute('aria-label', `${label}: ${name}. Choose another`);
    button.replaceChildren(h('span', { class: 'grow' }, name), icon('down', 14));
  };
  button.addEventListener('click', async () => {
    const offered = items();
    const picked = await pick({ items: offered, at: button, label: find, current: offered.find((item) => item.label === button.value) });
    if (!picked || picked.label === button.value) return;
    show(picked.label);
    chosen?.(picked.label);
  });
  show(now);
  return button;
}

/** How many of a kind are kept as chosen lately. */
const LATELY = 8;

/** The names of a `kind` chosen lately in this browser, such as cubes, the latest first. */
export function lately(kind) {
  try {
    return JSON.parse(localStorage.getItem(`epiphany.lately.${kind}`)) ?? [];
  } catch {
    return [];
  }
}

/** Keeps `name` as the one of its `kind` chosen latest. */
export function choseLately(kind, name) {
  localStorage.setItem(`epiphany.lately.${kind}`, JSON.stringify([name, ...lately(kind).filter((other) => other !== name)].slice(0, LATELY)));
}

/**
 * The items, those of a `kind` chosen lately first, latest first, each known by `name`. With
 * `rest`, the heading of those not chosen lately, the others are headed Lately; without it,
 * none is headed.
 */
export function lateFirst(items, kind, { name = (item) => item.label, rest = null } = {}) {
  const place = new Map(lately(kind).map((each, index) => [each, index]));
  const early = items.filter((item) => place.has(name(item))).sort((a, b) => place.get(name(a)) - place.get(name(b)));
  if (!early.length) return items;
  const others = items.filter((item) => !place.has(name(item)));
  if (rest) {
    for (const item of early) item.group = 'Lately';
    for (const item of others) item.group = rest;
  }
  return [...early, ...others];
}

/** The panel open now, to be closed as another opens. */
let opened = null;

/**
 * Asks for one of `items` in a panel beside the element `at`, as a menu opens beside its
 * button: found by typing, `current` marked as the one there is now. `actions`, as a menu's
 * items, stand beneath the list. Resolves with the item chosen, or null if none is.
 */
export function pick({ items, at, label, current = null, actions = [], row, draw, nothing }) {
  opened?.(null);
  return new Promise((resolve) => {
    const previous = document.activeElement;
    const close = (item, refocus = true) => {
      panel.remove();
      document.removeEventListener('pointerdown', outside, true);
      opened = null;
      if (refocus) previous?.focus?.();
      resolve(item);
    };
    const found = finder({ items, label, current, row, draw, nothing, choose: (item) => close(item) });
    const offered = actions.filter(Boolean);
    const panel = h('div', { class: 'finder-panel', role: 'dialog', 'aria-label': label },
      found.element,
      offered.length ? h('div', { class: 'finder-actions' }, offered.map((action) => h('button', {
        type: 'button',
        disabled: action.disabled,
        onclick: () => {
          close(null);
          action.action();
        },
      }, icon(action.icon, 16), action.label))) : null);
    const outside = (event) => {
      if (!panel.contains(event.target)) close(null, false);
    };
    panel.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      // Only the panel closes, not a dialog it is open over.
      event.preventDefault();
      event.stopPropagation();
      close(null);
    });
    // The keys gone elsewhere, the panel goes, and leaves them there.
    panel.addEventListener('focusout', (event) => {
      if (event.relatedTarget && !panel.contains(event.relatedTarget)) close(null, false);
    });
    // Over a dialog, the panel goes in it: the rest of the page lies beneath, out of reach.
    ([...document.querySelectorAll('dialog[open]')].pop() ?? document.body).append(panel);
    // Beneath what opened it, or above where there is no room, held to that edge as it narrows.
    const box = at.getBoundingClientRect();
    panel.style.left = `${Math.max(8, Math.min(box.left, innerWidth - panel.offsetWidth - 8))}px`;
    if (box.bottom + 4 + panel.offsetHeight <= innerHeight - 8 || box.top < innerHeight / 2) panel.style.top = `${box.bottom + 4}px`;
    else {
      panel.style.bottom = `${innerHeight - box.top + 4}px`;
      panel.classList.add('above');
    }
    document.addEventListener('pointerdown', outside, true);
    opened = close;
    found.input.focus();
  });
}
