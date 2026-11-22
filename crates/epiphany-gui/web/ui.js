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
};

/** An icon by name, `size` pixels square. */
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

/** A short notice in the corner, gone after a while; `kind` is good, bad or plain. */
export function notify(message, kind = 'plain') {
  notices ??= document.body.appendChild(h('div', { class: 'notices', role: 'status', 'aria-live': 'polite' }));
  const close = () => notice.remove();
  const notice = h('div', { class: `notice ${kind}` },
    icon(kind === 'bad' ? 'alert' : kind === 'good' ? 'check' : 'info'),
    h('p', {}, message),
    h('button', { class: 'button quiet small icon', 'aria-label': 'Dismiss', onclick: close }, icon('close', 14)));
  notices.append(notice);
  setTimeout(close, kind === 'bad' ? 9000 : 4500);
}

/* ---- Dialogs ---- */

/**
 * Shows a modal dialog and resolves with what `onsubmit` returns, or null if it is
 * dismissed. `body` is the content; `actions` the label of the confirming button.
 */
export function dialog({ title, body, confirm = 'OK', cancel = 'Cancel', danger = false, onsubmit = () => true }) {
  return new Promise((resolve) => {
    const error = h('p', { class: 'error-text', role: 'alert', hidden: true });
    const submit = h('button', { class: `button ${danger ? 'danger' : 'primary'}`, type: 'submit' }, confirm);
    const form = h('form', { method: 'dialog' },
      h('div', { class: 'dialog-head' }, h('h2', {}, title),
        h('button', { class: 'button quiet icon small', type: 'button', 'aria-label': 'Close', onclick: () => element.close() }, icon('close', 16))),
      h('div', { class: 'dialog-body' }, body, error),
      h('div', { class: 'dialog-foot' },
        cancel ? h('button', { class: 'button', type: 'button', onclick: () => element.close() }, cancel) : null,
        submit));
    const element = h('dialog', { 'aria-label': title }, form);
    let result = null;
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      submit.disabled = true;
      error.hidden = true;
      try {
        result = await onsubmit(form);
        element.close();
      } catch (problem) {
        error.textContent = problem.message;
        error.hidden = false;
      } finally {
        submit.disabled = false;
      }
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

/* ---- Menus ---- */

let openMenu;

/**
 * Opens a menu at a point or beside an element. `items` are `{ label, icon, action,
 * disabled }`, `'-'` for a divider, or `{ heading }`.
 */
export function menu(items, at) {
  openMenu?.close();
  const buttons = [];
  const element = h('div', { class: 'menu', role: 'menu' }, items.map((item) => {
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
  element.addEventListener('keydown', (event) => {
    const index = buttons.indexOf(document.activeElement);
    const enabled = buttons.filter((button) => !button.disabled);
    if (event.key === 'Escape') close();
    else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const step = event.key === 'ArrowDown' ? 1 : -1;
      const position = enabled.indexOf(buttons[index]);
      enabled[(position + step + enabled.length) % enabled.length]?.focus();
    }
  });
  document.body.append(element);
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

/** A number as the reader's locale writes it. */
export function formatNumber(value, digits = 2) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return value ?? '';
  return value.toLocaleString(undefined, { minimumFractionDigits: 0, maximumFractionDigits: digits });
}

/** Text as a file the browser saves. */
export function download(name, text, type = 'text/plain') {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const link = h('a', { href: url, download: name });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
