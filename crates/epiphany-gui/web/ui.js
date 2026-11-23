// Building elements, icons, notices, dialogs and menus.

/**
 * Makes an element: `h('button', { class: 'button', onclick }, 'Save')`. Properties
 * beginning with `on` are listeners; `null`, `undefined` and `false` children are skipped.
 */
export function h(tag, props, ...children) {
  const element = tag === 'svg' || SVG_TAGS.has(tag)
    ? document.createElementNS('http://www.w3.org/2000/svg', tag)
    : document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith('on') && typeof value === 'function') {
      element.addEventListener(key.slice(2), value);
    } else if (key === 'class') {
      element.setAttribute('class', value);
    } else if (key === 'dataset') {
      Object.assign(element.dataset, value);
    } else if (key in element && !(element instanceof SVGElement) && typeof value !== 'string') {
      element[key] = value;
    } else {
      element.setAttribute(key, value === true ? '' : value);
    }
  }
  append(element, children);
  return element;
}

const SVG_TAGS = new Set(['path', 'circle', 'rect', 'line', 'polyline', 'polygon', 'g', 'text', 'title']);

function append(element, children) {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) append(element, child);
    else element.append(child instanceof Node ? child : String(child));
  }
}

/** Replaces an element's content. */
export function mount(element, ...children) {
  element.replaceChildren();
  append(element, children);
  return element;
}

/* Icons: 24-unit strokes, drawn at the size asked for. */
const ICONS = {
  explore: 'M3 3v18h18 M7 15l4-4 3 3 5-6',
  grid: 'M3 3h18v18H3z M3 9h18 M3 15h18 M9 3v18',
  cube: 'M12 2l9 5v10l-9 5-9-5V7z M12 22V12 M21 7l-9 5-9-5',
  model: 'M4 6h16 M4 12h10 M4 18h6 M18 14l3 3-3 3',
  admin: 'M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z M9 12l2 2 4-4',
  users: 'M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1 M9 10a3 3 0 1 0 0-6 3 3 0 0 0 0 6 M22 19v-1a4 4 0 0 0-3-3.9 M16 4.1a3 3 0 0 1 0 5.8',
  home: 'M3 11l9-8 9 8 M5 9.5V21h14V9.5',
  search: 'M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14z M21 21l-4.3-4.3',
  sun: 'M12 17a5 5 0 1 0 0-10 5 5 0 0 0 0 10z M12 1v2 M12 21v2 M4.2 4.2l1.4 1.4 M18.4 18.4l1.4 1.4 M1 12h2 M21 12h2 M4.2 19.8l1.4-1.4 M18.4 5.6l1.4-1.4',
  moon: 'M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z',
  monitor: 'M2 4h20v13H2z M8 21h8 M12 17v4',
  menu: 'M3 6h18 M3 12h18 M3 18h18',
  user: 'M20 21v-1a5 5 0 0 0-5-5H9a5 5 0 0 0-5 5v1 M12 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8',
  out: 'M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4 M16 17l5-5-5-5 M21 12H9',
  close: 'M18 6L6 18 M6 6l12 12',
  plus: 'M12 5v14 M5 12h14',
  minus: 'M5 12h14',
  chevron: 'M9 6l6 6-6 6',
  down: 'M6 9l6 6 6-6',
  up: 'M6 15l6-6 6 6',
  play: 'M6 4l14 8-14 8z',
  swap: 'M7 4v16 M3 8l4-4 4 4 M17 20V4 M13 16l4 4 4-4',
  filter: 'M3 4h18l-7 8v6l-4 2v-8z',
  rows: 'M3 5h18 M3 12h18 M3 19h18',
  columns: 'M5 3v18 M12 3v18 M19 3v18',
  download: 'M12 3v12 M7 10l5 5 5-5 M4 21h16',
  undo: 'M9 14L4 9l5-5 M4 9h11a5 5 0 0 1 0 10h-3',
  redo: 'M15 14l5-5-5-5 M20 9H9a5 5 0 0 0 0 10h3',
  save: 'M5 3h11l3 3v15H5z M8 3v6h8 M8 21v-7h8v7',
  code: 'M16 18l6-6-6-6 M8 6l-6 6 6 6',
  chart: 'M4 20V10 M10 20V4 M16 20v-7 M22 20H2',
  eye: 'M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6',
  eyeOff: 'M17.9 17.9A10 10 0 0 1 12 20c-7 0-11-8-11-8a18 18 0 0 1 5.1-5.9 M9.9 4.2A9 9 0 0 1 12 4c7 0 11 8 11 8a18 18 0 0 1-2.2 3.2 M1 1l22 22',
  dimension: 'M4 4h6v6H4z M14 4h6v6h-6z M4 14h6v6H4z M17 14v6 M14 17h6',
  measure: 'M4 20h16 M7 16V10 M12 16V6 M17 16v-4',
  level: 'M4 6h16 M8 12h12 M12 18h8',
  member: 'M12 12m-3 0a3 3 0 1 0 6 0 3 3 0 1 0-6 0',
  folder: 'M3 6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z',
  drag: 'M9 5h.01 M9 12h.01 M9 19h.01 M15 5h.01 M15 12h.01 M15 19h.01',
  alert: 'M12 9v4 M12 17h.01 M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z',
  check: 'M20 6L9 17l-5-5',
  info: 'M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20z M12 16v-4 M12 8h.01',
  edit: 'M12 20h9 M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z',
  trash: 'M3 6h18 M8 6V4h8v2 M19 6l-1 14H6L5 6',
  refresh: 'M21 12a9 9 0 1 1-2.6-6.4 M21 3v6h-6',
  lock: 'M5 11h14v10H5z M8 11V7a4 4 0 0 1 8 0v4',
  server: 'M3 4h18v6H3z M3 14h18v6H3z M7 7h.01 M7 17h.01',
  sigma: 'M18 7V4H6l6 8-6 8h12v-3',
  more: 'M12 12h.01 M19 12h.01 M5 12h.01',
  dashboard: 'M3 3h8v10H3z M13 3h8v6h-8z M13 11h8v10h-8z M3 15h8v6H3z',
  hash: 'M4 9h16 M4 15h16 M10 3L8 21 M16 3l-2 18',
  pause: 'M8 5v14 M16 5v14',
  bars: 'M4 6h10 M4 12h16 M4 18h7',
  copy: 'M9 9h11v11H9z M5 15H4V4h11v1',
  link: 'M9 15l6-6 M11 6l1-1a4.2 4.2 0 0 1 6 6l-1 1 M13 18l-1 1a4.2 4.2 0 0 1-6-6l1-1',
  paste: 'M9 3h6v4H9z M8 5H5v16h14V5h-3',
  expand: 'M7 15l5 5 5-5 M7 9l5-5 5 5',
  collapse: 'M7 20l5-5 5 5 M7 4l5 5 5-5',
  clock: 'M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20z M12 6v6l4 2',
  keyboard: 'M2 5h20v14H2z M6 9h.01 M10 9h.01 M14 9h.01 M18 9h.01 M8 13h.01 M12 13h.01 M16 13h.01 M7 16h10',
  sandbox: 'M12 3l9 4.5-9 4.5-9-4.5z M3 12l9 4.5 9-4.5 M3 16.5l9 4.5 9-4.5',
  publish: 'M12 20V8 M7 13l5-5 5 5 M4 4h16',
};

/** An icon by name, `size` pixels square. */
/** The key held with another for a command: Cmd on a Mac, Ctrl elsewhere. */
export const MOD = /mac|iphone|ipad/i.test(navigator.userAgentData?.platform ?? navigator.platform ?? '') ? '⌘' : 'Ctrl';

export function icon(name, size = 18) {
  return h('svg', {
    width: size,
    height: size,
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': 1.8,
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round',
    'aria-hidden': 'true',
  }, h('path', { d: ICONS[name] ?? ICONS.info }));
}

/* ---- Notices ---- */

let notices;

/** The most notices that stand at once: a newer takes the place of the oldest. */
const NOTICES_SHOWN = 4;

/** Each notice standing, and for how many milliseconds more. */
const standing = new Map();
let ticking = null;

/** Counts down the notices' time, except while the pointer or the keys are on one, or the page is out of sight. */
function tick() {
  for (const [notice, left] of standing) {
    if (!notice.isConnected) standing.delete(notice);
    else if (document.hidden || notice.matches(':hover, :focus-within')) continue;
    else if (left > 500) standing.set(notice, left - 500);
    else {
      notice.remove();
      standing.delete(notice);
    }
  }
  if (!standing.size) {
    clearInterval(ticking);
    ticking = null;
  }
}

/**
 * A short notice in the corner, standing long enough to read, an error twice as long, and
 * longer while it is pointed at, has the keys or the page is out of sight; `kind` is good, bad
 * or plain. The same notice again takes the place of the first.
 */
export function notify(message, kind = 'plain') {
  notices ??= document.body.appendChild(h('div', { class: 'notices', role: 'status', 'aria-live': 'polite' }));
  const said = `${kind}: ${message}`;
  [...notices.children].find((other) => other.dataset.said === said)?.remove();
  const notice = h('div', { class: `notice ${kind}`, dataset: { said } },
    icon(kind === 'bad' ? 'alert' : kind === 'good' ? 'check' : 'info'),
    h('p', {}, message),
    h('button', { class: 'button quiet small icon', 'aria-label': 'Dismiss', onclick: () => notice.remove() }, icon('close', 14)));
  notices.append(notice);
  while (notices.children.length > NOTICES_SHOWN) notices.firstElementChild.remove();
  // A fifteenth of a second a letter, at least four and a half seconds.
  standing.set(notice, Math.max(4500, String(message).length * 70) * (kind === 'bad' ? 2 : 1));
  ticking ??= setInterval(tick, 500);
}

/* ---- Dialogs ---- */

/**
 * Shows a modal dialog and resolves with what `onsubmit` returns, or null if it is
 * dismissed. `body` is the content; `actions` the label of the confirming button.
 */
let dialogs = 0;

/**
 * A refusal of what `field` holds, to throw from a dialog's `onsubmit`: the dialog says it
 * beneath the field, marks the field, and gives it the keys.
 */
export function invalid(field, message) {
  return Object.assign(new Error(message), { field });
}

export function dialog({ title, body, confirm = 'OK', cancel = 'Cancel', danger = false, wide = false, onsubmit = () => true }) {
  return new Promise((resolve) => {
    const error = h('p', { class: 'error-text', role: 'alert', id: `dialog-error-${++dialogs}`, hidden: true });
    const submit = h('button', { class: `button ${danger ? 'danger' : 'primary'}`, type: 'submit' }, confirm);
    const content = h('div', { class: 'dialog-body' }, body, error);
    // The field refused last: marked, and described by the refusal, until it is changed.
    let refused = null;
    const unmark = () => {
      if (!refused) return;
      const { field, listening } = refused;
      listening.abort();
      field.removeAttribute('aria-invalid');
      delete field.dataset.refused;
      const others = (field.getAttribute('aria-describedby') ?? '').split(' ').filter((id) => id && id !== error.id);
      if (others.length) field.setAttribute('aria-describedby', others.join(' '));
      else field.removeAttribute('aria-describedby');
      refused = null;
    };
    const refuse = (problem) => {
      const { field } = problem;
      error.textContent = problem.message;
      error.hidden = false;
      (field?.closest('.field') ?? content).append(error);
      if (!field) return;
      refused = { field, listening: new AbortController() };
      // A button, as one that chooses, is not a field that can be invalid, but is marked to the eye.
      const control = field.matches('input, select, textarea');
      if (control) field.setAttribute('aria-invalid', 'true');
      field.dataset.refused = '';
      field.setAttribute('aria-describedby', [field.getAttribute('aria-describedby'), error.id].filter(Boolean).join(' '));
      for (const kind of control ? ['input', 'change'] : ['click']) field.addEventListener(kind, unmark, { signal: refused.listening.signal });
      field.focus();
    };
    const form = h('form', { method: 'dialog' },
      h('div', { class: 'dialog-head' }, h('h2', {}, title),
        h('button', { class: 'button quiet icon small', type: 'button', 'aria-label': 'Close', onclick: () => element.close() }, icon('close', 16))),
      content,
      h('div', { class: 'dialog-foot' },
        cancel ? h('button', { class: 'button', type: 'button', onclick: () => element.close() }, cancel) : null,
        submit));
    const element = h('dialog', { 'aria-label': title, class: wide ? 'wide' : null }, form);
    let result = null;
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      working(submit, async () => {
        error.hidden = true;
        unmark();
        try {
          result = await onsubmit(form);
          element.close();
        } catch (problem) {
          refuse(problem);
        }
      });
    });
    element.addEventListener('close', () => {
      element.remove();
      resolve(result);
    });
    document.body.append(element);
    element.showModal();
    element.querySelector('input, select, textarea')?.focus();
  });
}

/**
 * Shows a button at work while the task it started runs: after a moment a spinner takes the
 * place of its label, and it takes no more presses until the task is done. Resolves with
 * what the task gives; pressed while it is at work, it does nothing.
 */
export async function working(button, task) {
  if (button.getAttribute('aria-busy') === 'true') return undefined;
  button.setAttribute('aria-busy', 'true');
  button.setAttribute('aria-disabled', 'true');
  try {
    return await task();
  } finally {
    button.removeAttribute('aria-busy');
    button.removeAttribute('aria-disabled');
  }
}

/* ---- Sizes ---- */

let canvas;

/** Measures text as `element`'s font draws it: returns how wide, in pixels, a text is. */
export function textWidths(element = document.body) {
  const style = getComputedStyle(element);
  const context = (canvas ??= document.createElement('canvas')).getContext('2d');
  context.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
  return (text) => context.measureText(String(text)).width;
}

/**
 * A handle on the edge of a pane that sets its width, as `--name` on `container`, which lays
 * the pane out by it. Dragging it or the arrow keys change the width, kept in this browser
 * by `key`; a double-click lets go of it, and the pane fits its content again, `fit()` being
 * that width. `edge` is the side of the pane the handle is on: its end, or its start for a
 * pane on the right. The container places the handle, by the same `--name`.
 */
export function splitter(container, { name, key, fit, min = 200, max = 900, edge = 'end' }) {
  const stored = `epiphany.width.${key}`;
  const handle = h('div', {
    class: `splitter ${edge}`,
    role: 'separator',
    tabindex: '0',
    'aria-orientation': 'vertical',
    'aria-valuemin': min,
    'aria-valuemax': max,
    'aria-label': 'Width of the pane',
    title: 'Drag to change the width; double-click to fit it to its content',
  });
  let width = 0;
  const set = (next, keep) => {
    width = Math.round(Math.max(min, Math.min(max, next)));
    container.style.setProperty(`--${name}`, `${width}px`);
    handle.setAttribute('aria-valuenow', width);
    if (keep) localStorage.setItem(stored, String(width));
  };
  const direction = edge === 'end' ? 1 : -1;
  handle.addEventListener('pointerdown', (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const origin = { x: event.clientX, width };
    document.body.classList.add('resizing');
    const moved = (next) => set(origin.width + (next.clientX - origin.x) * direction);
    const done = () => {
      window.removeEventListener('pointermove', moved);
      window.removeEventListener('pointerup', done);
      window.removeEventListener('pointercancel', done);
      document.body.classList.remove('resizing');
      set(width, true);
    };
    window.addEventListener('pointermove', moved);
    window.addEventListener('pointerup', done);
    window.addEventListener('pointercancel', done);
  });
  handle.addEventListener('keydown', (event) => {
    const step = { ArrowLeft: -16, ArrowRight: 16 }[event.key];
    if (!step) return;
    event.preventDefault();
    set(width + step * direction, true);
  });
  const refit = () => {
    localStorage.removeItem(stored);
    set(fit());
  };
  handle.addEventListener('dblclick', refit);
  container.append(handle);
  return {
    /** Sets the width as kept, or as the content needs; again when the content changes. */
    fit() {
      const kept = Number(localStorage.getItem(stored));
      set(kept || fit());
    },
    refit,
  };
}

/* ---- Menus ---- */

let openMenu;

/**
 * Opens a menu at a point or beside an element. `items` are `{ label, icon, action,
 * disabled }`, `'-'` for a divider, or `{ heading }`.
 */
export function menu(items, at) {
  openMenu?.close();
  const buttons = [];
  const element = h('div', { class: 'menu', role: 'menu' }, items.filter(Boolean).map((item) => {
    if (item === '-') return h('hr');
    if (item.heading) return h('div', { class: 'menu-label' }, item.heading);
    const button = h('button', {
      role: 'menuitem',
      disabled: item.disabled,
      onclick: () => {
        close();
        item.action();
      },
    }, item.icon ? icon(item.icon, 16) : h('span', { class: 'menu-gap' }), item.label);
    buttons.push(button);
    return button;
  }));
  const previous = document.activeElement;
  const close = () => {
    element.remove();
    document.removeEventListener('pointerdown', outside, true);
    openMenu = undefined;
    previous?.focus?.();
  };
  const outside = (event) => {
    if (!element.contains(event.target)) close();
  };
  // Over a dialog, the menu goes in it: the rest of the page lies beneath, out of reach.
  const host = [...document.querySelectorAll('dialog[open]')].pop() ?? document.body;
  element.addEventListener('keydown', (event) => {
    const index = buttons.indexOf(document.activeElement);
    const enabled = buttons.filter((button) => !button.disabled);
    if (event.key === 'Escape') {
      // Only the menu closes, not a dialog it is open over.
      event.preventDefault();
      event.stopPropagation();
      close();
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const step = event.key === 'ArrowDown' ? 1 : -1;
      const position = enabled.indexOf(buttons[index]);
      enabled[(position + step + enabled.length) % enabled.length]?.focus();
    }
  });
  host.append(element);
  const box = at instanceof Element ? at.getBoundingClientRect() : { left: at.x, bottom: at.y, top: at.y };
  const width = element.offsetWidth;
  const height = element.offsetHeight;
  element.style.left = `${Math.max(8, Math.min(box.left, innerWidth - width - 8))}px`;
  const below = box.bottom + 4;
  element.style.top = `${below + height > innerHeight - 8 ? Math.max(8, box.top - height - 4) : below}px`;
  document.addEventListener('pointerdown', outside, true);
  buttons.find((button) => !button.disabled)?.focus();
  openMenu = { close };
}

/* ---- Choices side by side ---- */

/**
 * A row of choices, one of them chosen: the tabs over a pane (`tablist`), or the values of a
 * setting (`radiogroup`). `options` are `[value, label]`; `choose(value, byKeys)` takes the
 * one chosen, and draws the row again. The arrow keys, Home and End go along the row,
 * choosing as they go, and Tab passes over all but the one chosen, as WAI-ARIA has it. The
 * row is drawn into `into` when it is given, which keeps its place on the page.
 */
export function choices({ label, kind = 'tablist', options, chosen, choose, into = h('div') }) {
  const role = kind === 'tablist' ? 'tab' : 'radio';
  const state = kind === 'tablist' ? 'aria-selected' : 'aria-checked';
  // With none chosen, Tab still comes to the row, at its first.
  const reached = options.some(([value]) => value === chosen) ? chosen : options[0]?.[0];
  into.classList.add('segmented');
  into.setAttribute('role', kind);
  into.setAttribute('aria-label', label);
  // Drawn again by a choice, perhaps afresh, the row keeps the keys on what is chosen, unless
  // the choice gave them somewhere else to go.
  const keep = (always) => {
    if (!always && document.activeElement && document.activeElement !== document.body) return;
    const row = into.isConnected ? into : document.querySelector(`[role="${kind}"][aria-label="${CSS.escape(label)}"]`);
    row?.querySelector('[tabindex="0"]')?.focus();
  };
  const buttons = options.map(([value, text]) => h('button', {
    type: 'button',
    role,
    [state]: String(value === chosen),
    tabindex: value === reached ? '0' : '-1',
    onclick: () => {
      choose(value, false);
      keep(false);
    },
  }, text));
  into.onkeydown = (event) => {
    const at = buttons.indexOf(event.target);
    const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
    const to = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : step && (at + step + buttons.length) % buttons.length;
    if (at < 0 || to === undefined) return;
    event.preventDefault();
    choose(options[to][0], true);
    keep(true);
  };
  return mount(into, buttons);
}

/** A number as the reader's locale writes it. */
export function formatNumber(value, digits = 2) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return value ?? '';
  return value.toLocaleString(undefined, { minimumFractionDigits: 0, maximumFractionDigits: digits });
}

/** What each unit a view may show numbers in divides them by, and how it is said. */
export const SCALES = { thousands: [1e3, 'In thousands'], millions: [1e6, 'In millions'], billions: [1e9, 'In billions'] };

/**
 * A value as a view's number format shows it: in its unit, to its decimals, a negative in
 * parentheses and a zero as a dash where it asks. Text is shown as it is.
 */
export function formatValue(value, format) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return value ?? '';
  if (!format) return formatNumber(value);
  if (format.dash_zeros && value === 0) return '–';
  const shown = value / (SCALES[format.scale]?.[0] ?? 1);
  const digits = format.decimals ?? (format.scale ? 1 : 2);
  const text = Math.abs(shown).toLocaleString(undefined, { minimumFractionDigits: format.decimals ?? 0, maximumFractionDigits: digits });
  // A value that rounds to nothing has no sign.
  if (shown >= 0 || /^[0.,\s]*$/.test(text)) return text;
  return format.parentheses ? `(${text})` : `-${text}`;
}

/**
 * What a cell of a query's answer shows: as the calculated member that decides it writes it
 * out, where that member has a FORMAT_STRING; otherwise its value as the view's format shows it.
 */
export function shownText(result, index, format) {
  return result.formatted?.[index] ?? formatValue(result.cells[index], format);
}

/** The reader's decimal sign, a point or a comma. */
const DECIMAL = (1.5).toLocaleString().replace(/\d/g, '') || '.';

/** A number as text to copy: in full, without grouping, with the reader's decimal sign. */
export function numberText(value) {
  return DECIMAL === '.' ? String(value) : String(value).replace('.', DECIMAL);
}

/**
 * A number as typed or pasted, or null if the text is not one. Spaces and currency signs
 * are let go, and parentheses make it negative. Of a point and a comma both present, the
 * later is the decimal sign. One alone, and only once, is the decimal sign if it is the
 * reader's, or if other than three digits follow it, as in 1.5 or 2,25; otherwise, and
 * whenever it comes more than once, it groups digits.
 */
export function parseNumber(text) {
  let cleaned = String(text).trim().replace(/[\s  $€£¥]/g, '');
  const negative = /^\(.*\)$/.test(cleaned);
  if (negative) cleaned = cleaned.slice(1, -1);
  const point = cleaned.lastIndexOf('.');
  const comma = cleaned.lastIndexOf(',');
  let decimal = null;
  if (point >= 0 && comma >= 0) decimal = point > comma ? '.' : ',';
  else if (point >= 0 || comma >= 0) {
    const sign = point >= 0 ? '.' : ',';
    const once = cleaned.indexOf(sign) === cleaned.lastIndexOf(sign);
    const after = cleaned.slice(cleaned.lastIndexOf(sign) + 1);
    decimal = once && (sign === DECIMAL || !/^\d{3}$/.test(after)) ? sign : null;
  }
  // Digits are grouped in threes, or the text is not a number.
  const whole = decimal ? cleaned.slice(0, cleaned.lastIndexOf(decimal)) : cleaned;
  if (/[.,]/.test(whole) && !/^[-+]?\d{1,3}([.,]\d{3})+$/.test(whole)) return null;
  const grouping = decimal === '.' ? ',' : decimal === ',' ? '.' : /[.,]/g;
  cleaned = cleaned.replaceAll(grouping, '');
  if (decimal === ',') cleaned = cleaned.replace(',', '.');
  if (!/^[-+]?(\d+\.?\d*|\.\d+)(e[-+]?\d+)?$/i.test(cleaned)) return null;
  const value = Number(cleaned);
  return Number.isFinite(value) ? (negative ? -value : value) : null;
}

const RELATIVE = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

/** The spans a moment within the week is said in, each in seconds, the largest first. */
const SPANS = [['day', 86_400], ['hour', 3_600], ['minute', 60]];

/** How long ago `date` was, as "5 minutes ago" or "yesterday", within the week; past it, its date and time. */
function ago(date) {
  const seconds = (Date.now() - date) / 1000;
  if (Math.abs(seconds) < 60) return RELATIVE.format(0, 'second');
  const span = seconds > 0 && seconds < 7 * 86_400 && SPANS.find(([, size]) => seconds >= size);
  return span ? RELATIVE.format(-Math.floor(seconds / span[1]), span[0]) : date.toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

/**
 * A moment, from seconds since 1970, said as long ago as it was, and again as time passes; the
 * moment itself on hover, on paper and to a machine.
 */
export function when(seconds) {
  const date = new Date(seconds * 1000);
  const exact = date.toLocaleString();
  return h('time', { datetime: date.toISOString(), title: exact }, h('span', { class: 'ago' }, ago(date)), h('span', { class: 'exact' }, exact));
}

// What was said as now grows older while the page is seen.
setInterval(() => {
  if (document.hidden) return;
  for (const said of document.querySelectorAll('time > .ago')) said.textContent = ago(new Date(said.parentElement.dateTime));
}, 30_000);

/** Text as a file the browser saves. */
export function download(name, text, type = 'text/plain') {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const link = h('a', { href: url, download: name });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/**
 * A password field with a button inside its end that shows what was typed, and hides it
 * again: the field keeps its label and has the keys back; the button is named for what it
 * does, pressed while the password shows.
 */
export function revealable(input) {
  const toggle = h('button', {
    class: 'reveal',
    type: 'button',
    'aria-label': 'Show password',
    'aria-pressed': 'false',
    onclick: () => {
      const showing = input.type === 'password';
      input.type = showing ? 'text' : 'password';
      toggle.setAttribute('aria-pressed', String(showing));
      toggle.replaceChildren(icon(showing ? 'eyeOff' : 'eye', 16));
      input.focus();
    },
  }, icon('eye', 16));
  return h('div', { class: 'revealable' }, input, toggle);
}

/** Puts text on the clipboard, and says so. */
export function copyText(text) {
  navigator.clipboard?.writeText(text).then(
    () => notify('Copied.', 'good'),
    () => notify(`The browser kept the page from the clipboard. Press ${MOD} C instead.`, 'bad'),
  );
}

/** What draws only the part of itself in view, by its element: how to draw it for paper, and back. */
const printed = new WeakMap();

/**
 * Paper has no scrolling: while the page is printed, `element`, drawn only in part where it
 * can be seen, is drawn whole by `whole()`, and afterwards as it was by `after()`.
 */
export function printsWhole(element, whole, after) {
  printed.set(element, { whole, after });
  element.dataset.printsWhole = '';
}

for (const [event, then] of [['beforeprint', 'whole'], ['afterprint', 'after']]) {
  addEventListener(event, () => {
    for (const element of document.querySelectorAll('[data-prints-whole]')) printed.get(element)?.[then]();
  });
}

/**
 * What an ellipsis cuts short, pointed at, is shown whole as its title: the text, or the
 * nearest of the two around it that cuts it, given a title only while it is cut, and never
 * in place of one of its own.
 */
document.addEventListener('pointerover', ({ target }) => {
  let element = target instanceof HTMLElement ? target : null;
  for (let up = 0; element && up < 2 && getComputedStyle(element).textOverflow !== 'ellipsis'; up += 1) element = element.parentElement;
  if (!element || element.matches('input, select, textarea') || (element.title && !('cut' in element.dataset))) return;
  if (element.scrollWidth > element.clientWidth && getComputedStyle(element).textOverflow === 'ellipsis') {
    element.title = element.textContent.trim();
    element.dataset.cut = '';
  } else if ('cut' in element.dataset) {
    element.removeAttribute('title');
    delete element.dataset.cut;
  }
});
