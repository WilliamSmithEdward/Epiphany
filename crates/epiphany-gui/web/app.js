// The application: signing in, the shell around every page, and moving between pages.

import { ApiError, api, events, get } from './api.js';
import { dialog, h, icon, menu, MOD, mount, notify, working } from './ui.js';

/** Who is signed in, and the model as last fetched. */
export const state = { user: null, open: false, model: null, version: null };

/**
 * The pages, in the order the navigation lists them. `admin` pages need the admin role;
 * `shape` is the outline shown while a page loads.
 */
const PAGES = [
  { path: '/', title: 'Overview', icon: 'home', shape: 'cards', load: async () => ({ render: overview }) },
  { path: '/explore', title: 'Explore', icon: 'explore', shape: 'panes', load: () => import('./explore.js') },
  { path: '/dashboards', title: 'Dashboards', icon: 'dashboard', shape: 'cards', load: () => import('./dashboards.js') },
  { path: '/model', title: 'Model', icon: 'model', shape: 'panes', load: () => import('./model.js'), section: 'Manage', admin: true },
  { path: '/admin', title: 'Administration', icon: 'admin', shape: 'cards', load: () => import('./admin.js'), admin: true },
];

const root = document.getElementById('app');

/* ---- Theme ---- */

const THEME = 'epiphany.theme';
const dark = matchMedia('(prefers-color-scheme: dark)');

function applyTheme() {
  const chosen = localStorage.getItem(THEME) ?? 'system';
  const theme = chosen === 'system' ? (dark.matches ? 'dark' : 'light') : chosen;
  document.documentElement.dataset.theme = theme;
  document.getElementById('theme-button')?.replaceChildren(icon(theme === 'dark' ? 'moon' : 'sun', 18));
}
dark.addEventListener('change', applyTheme);
applyTheme();

function chooseTheme(theme) {
  localStorage.setItem(THEME, theme);
  applyTheme();
}

/* ---- Starting, signing in and out ---- */

async function start() {
  try {
    const session = await api('GET', '/gui/session');
    state.open = session.open === true;
    state.user = session.signed_in ? session.user : state.open ? { name: 'This machine', role: 'admin' } : null;
  } catch (problem) {
    return unreachable(problem);
  }
  if (state.user) shell();
  else signIn();
}

function unreachable(problem) {
  mount(root, h('div', { class: 'sign-in' },
    h('div', { class: 'card' },
      brand(),
      h('div', { class: 'banner bad' }, icon('alert'), problem.message),
      h('button', { class: 'button primary', onclick: (event) => working(event.currentTarget, start) }, icon('refresh', 16), 'Try again'))));
}

function brand() {
  return h('a', { class: 'brand', href: '/', 'data-link': '' }, h('img', { src: '/icon.svg', alt: '' }), 'Epiphany');
}

function signIn(reason) {
  document.title = 'Sign in · Epiphany';
  const name = h('input', { class: 'input', id: 'name', name: 'name', autocomplete: 'username', required: true, autofocus: true });
  const password = h('input', { class: 'input', id: 'password', name: 'password', type: 'password', autocomplete: 'current-password', required: true });
  const error = h('p', { class: 'error-text', role: 'alert', hidden: !reason }, reason ?? '');
  const submit = h('button', { class: 'button primary', type: 'submit' }, 'Sign in');
  const form = h('form', {
    onsubmit: (event) => {
      event.preventDefault();
      working(submit, async () => {
        error.hidden = true;
        try {
          const session = await api('POST', '/gui/session', { name: name.value.trim(), password: password.value });
          state.user = session.user;
          shell();
        } catch (problem) {
          error.textContent = problem.status === 401
            ? 'That name and password do not match.'
            : problem.message;
          error.hidden = false;
          password.select();
        }
      });
    },
  },
  h('div', { class: 'field' }, h('label', { for: 'name' }, 'Name'), name),
  h('div', { class: 'field' }, h('label', { for: 'password' }, 'Password'), password),
  error,
  submit);
  mount(root, h('div', { class: 'sign-in' },
    h('main', { class: 'card' },
      brand(),
      h('div', {}, h('h1', {}, 'Sign in'), h('p', { class: 'muted' }, 'Use the name and password your administrator gave you.')),
      form)));
  name.focus();
}

async function signOut() {
  // What is not saved goes only if the user agrees.
  if (active?.leave && !(await active.leave())) return;
  try {
    await api('DELETE', '/gui/session');
  } catch {
    // Signed out here whether or not the server heard.
  }
  leaveApp();
}

/** Ends a view as another user, and starts the application again as the administrator. */
async function stopViewing() {
  try {
    await api('DELETE', '/gui/view-as');
  } catch (problem) {
    notify(problem.message, 'bad');
    return;
  }
  location.assign('/admin');
}

/** Leaves the application for the sign-in page, letting go of the page shown. */
function leaveApp(reason) {
  active = null;
  state.user = null;
  history.pushState(null, '', '/');
  signIn(reason);
}

events.addEventListener('signed-out', () => {
  if (!state.user || state.open || reauthenticating) return;
  reauthenticate();
});

let reauthenticating = false;

/**
 * Asks for the password again when the session ends, over the page, so that nothing on it
 * is lost: the user signs in again and carries on. Signing in as someone else starts afresh,
 * and dismissing it signs out.
 */
async function reauthenticate() {
  reauthenticating = true;
  const before = state.user;
  const name = h('input', { class: 'input', id: 'again-name', autocomplete: 'username', required: true });
  // A view as another user that has ended is the administrator's to sign in again from.
  name.value = before.viewer ?? before.name;
  const password = h('input', { class: 'input', id: 'again-password', type: 'password', autocomplete: 'current-password', required: true });
  const asked = dialog({
    title: 'Your session has ended',
    confirm: 'Sign in',
    cancel: 'Sign out',
    body: h('div', { class: 'facts' },
      h('p', { class: 'muted' }, 'Sign in again to carry on. The page stays as it was, unsaved changes and all.'),
      h('div', { class: 'field' }, h('label', { for: 'again-name' }, 'Name'), name),
      h('div', { class: 'field' }, h('label', { for: 'again-password' }, 'Password'), password)),
    onsubmit: async () => {
      try {
        return await api('POST', '/gui/session', { name: name.value.trim(), password: password.value });
      } catch (problem) {
        password.select();
        throw new Error(problem.status === 401 ? 'That name and password do not match.' : problem.message);
      }
    },
  });
  password.focus();
  const session = await asked;
  reauthenticating = false;
  if (!session) return leaveApp();
  state.user = session.user;
  if (session.user.name.toLowerCase() !== before.name.toLowerCase() || session.user.role !== before.role) {
    // Another user, or another role: the page is drawn again for them.
    active = null;
    shell();
    return;
  }
  notify('Signed in again. Carry on where you were.', 'good');
  // A page that could not be drawn for want of a session is drawn now.
  if (!active) route();
}

events.addEventListener('model-changed', () => {
  state.model = null;
});

/* ---- The shell ---- */

let page;
let shellElement;
let statusElement;

const isAdmin = () => state.user?.role === 'admin';
const visible = () => PAGES.filter((entry) => !entry.admin || isAdmin());

function shell() {
  const narrow = localStorage.getItem('epiphany.nav') === 'narrow';
  statusElement = h('span', { class: 'status', role: 'status', title: 'The Epiphany server is answering' }, 'Connected');
  const nav = h('nav', { class: 'nav', 'aria-label': 'Pages' });
  let section;
  for (const entry of visible()) {
    if (entry.section && entry.section !== section) {
      section = entry.section;
      nav.append(h('div', { class: 'section' }, section));
    }
    nav.append(h('a', { href: entry.path, 'data-link': '', title: entry.title }, icon(entry.icon, 20), h('span', { class: 'label' }, entry.title)));
  }
  nav.append(h('div', { class: 'grow' }));
  const toggle = h('button', {
    class: 'button quiet icon',
    'aria-label': 'Collapse navigation',
    onclick: () => {
      const now = shellElement.dataset.nav === 'narrow' ? 'wide' : 'narrow';
      shellElement.dataset.nav = now;
      localStorage.setItem('epiphany.nav', now);
    },
  }, icon('menu', 20));
  page = h('main', { class: 'main', id: 'main', tabindex: '-1' });
  // The first stop of the keys goes straight to the page, past the bar and the list of pages.
  const skip = h('a', {
    class: 'skip',
    href: '#main',
    onclick: (event) => {
      event.preventDefault();
      page.focus();
    },
  }, 'Skip to the page');
  const user = state.user;
  const userButton = h('button', {
    class: 'button quiet',
    'aria-haspopup': 'menu',
    onclick: (event) => menu([
      { heading: `${user.name} · ${roleName(user.role)}` },
      user.viewer ? { heading: `Viewed by ${user.viewer}, reading only` } : null,
      user.viewer ? { label: 'Stop viewing', icon: 'eyeOff', action: stopViewing } : null,
      { label: 'Keyboard shortcuts', icon: 'keyboard', action: shortcuts },
      ...(state.open ? [] : [{ label: 'Sign out', icon: 'out', action: signOut }]),
    ], event.currentTarget),
  }, icon('user', 18), h('span', { class: 'label' }, user.name));
  // While an administrator views the server as another user, the bar says so on every page.
  const viewing = user.viewer
    ? h('div', { class: 'viewing', role: 'status' },
      icon('eye', 16),
      h('span', {}, 'Viewing as ', h('strong', {}, user.name), ', reading only'),
      h('button', { class: 'button small', onclick: (event) => working(event.currentTarget, stopViewing) }, 'Stop'))
    : null;
  const themeButton = h('button', {
    class: 'button quiet icon',
    id: 'theme-button',
    'aria-label': 'Theme',
    'aria-haspopup': 'menu',
    onclick: (event) => menu([
      { heading: 'Theme' },
      { label: 'Light', icon: 'sun', action: () => chooseTheme('light') },
      { label: 'Dark', icon: 'moon', action: () => chooseTheme('dark') },
      { label: 'As the system', icon: 'monitor', action: () => chooseTheme('system') },
    ], event.currentTarget),
  }, icon(document.documentElement.dataset.theme === 'dark' ? 'moon' : 'sun', 18));
  shellElement = h('div', { class: 'shell', dataset: { nav: narrow ? 'narrow' : 'wide' } },
    skip,
    h('header', { class: 'bar' },
      toggle,
      brand(),
      h('button', { class: 'search-button', onclick: palette, 'aria-label': 'Search and commands' },
        icon('search', 16), h('span', {}, 'Search cubes, pages and commands'), h('kbd', {}, `${MOD} K`)),
      h('div', { class: 'spacer' }),
      viewing,
      statusElement,
      themeButton,
      userButton),
    nav,
    page);
  mount(root, shellElement);
  route();
  watch();
}

function roleName(role) {
  return { admin: 'Administrator', write: 'Can write', read: 'Can read', none: 'No access' }[role] ?? role;
}

/* ---- Moving between pages ---- */

let rendering = 0;
/**
 * The page shown, which may ask to stay when it is left, and may move to other parameters
 * in place, by its `update`; and its path, without them.
 */
let active = null;
let activePath = null;
let shownPath = location.pathname + location.search;

async function route() {
  if (!state.user) return;
  if (active?.leave && !(await active.leave())) {
    history.pushState(null, '', shownPath);
    return;
  }
  shownPath = location.pathname + location.search;
  const path = location.pathname.replace(/\/+$/, '') || '/';
  // The page shown, asked for again with other parameters, moves to them where it stands:
  // what stays the same is not drawn again, and nothing comes in between.
  if (active?.update && path === activePath) {
    active.update(new URLSearchParams(location.search));
    return;
  }
  active = null;
  activePath = path;
  const entry = visible().find((candidate) => candidate.path === path)
    ?? visible().find((candidate) => candidate.path !== '/' && path.startsWith(`${candidate.path}/`));
  for (const link of shellElement.querySelectorAll('.nav a')) {
    link.toggleAttribute('aria-current', false);
    if (entry && link.getAttribute('href') === entry.path) link.setAttribute('aria-current', 'page');
  }
  const ticket = ++rendering;
  if (!entry) {
    document.title = 'Not found · Epiphany';
    mount(page, h('div', { class: 'page' }, empty('search', 'There is no such page', 'Check the address, or choose a page from the navigation.')));
    return;
  }
  document.title = `${entry.title} · Epiphany`;
  awaitNext(entry.shape);
  try {
    const module = await entry.load();
    if (ticket !== rendering) return;
    const shown = await module.render(page, { state, params: new URLSearchParams(location.search), path, title: entry.title });
    // A page another move has passed by is not the one shown, to be asked before leaving.
    if (ticket === rendering) active = shown;
  } catch (problem) {
    if (ticket !== rendering) return;
    mount(page, h('div', { class: 'page' }, failure(problem, route)));
  }
}

/** The wait for the next page to be drawn, which the next move between pages ends. */
let waiting = null;

/**
 * Keeps the page shown, still and out of reach, until the next one draws anything in its
 * place: a quick load then shows no gap at all, and a slow one, after a moment, the outline
 * of what is coming. The first page has nothing to keep, so its outline is drawn at once,
 * showing itself after the same moment.
 */
function awaitNext(shape) {
  waiting?.();
  const leaving = page.firstElementChild;
  const outline = () => h('div', { class: 'page' }, loading(shape));
  if (!leaving) {
    mount(page, outline());
    return;
  }
  page.inert = true;
  page.setAttribute('aria-busy', 'true');
  const timer = setTimeout(() => {
    if (page.firstElementChild !== leaving) return;
    const shown = outline();
    // The moment has passed already, waiting here.
    shown.firstElementChild.style.animationDelay = '0s';
    mount(page, shown);
  }, 150);
  const watcher = new MutationObserver(() => {
    if (page.firstElementChild !== leaving) waiting();
  });
  waiting = () => {
    clearTimeout(timer);
    watcher.disconnect();
    page.inert = false;
    page.removeAttribute('aria-busy');
    waiting = null;
  };
  watcher.observe(page, { childList: true });
}

/** Goes to a page within the application. */
export function go(path) {
  if (path === location.pathname + location.search) return;
  history.pushState(null, '', path);
  route();
}

addEventListener('popstate', route);
document.addEventListener('click', (event) => {
  const link = event.target.closest?.('a[data-link]');
  if (!link || event.ctrlKey || event.metaKey || event.shiftKey || event.button !== 0) return;
  event.preventDefault();
  go(link.getAttribute('href'));
});

/* ---- Shared states of a page ---- */

/**
 * The outline of what is loading, in grey: `cards` for a page of cards, `panes` for a list
 * beside a work area, and `lines` for rows of a table or grid. It shows after a moment, so
 * that a quick load shows nothing at all.
 */
export function loading(shape = 'lines') {
  const bones = (count, kind) => Array.from({ length: count }, () => h('div', { class: `skeleton ${kind}` }));
  const lines = (count) => h('div', { class: 'lines' }, bones(count, 'line'));
  const shapes = {
    cards: () => [bones(1, 'title'), h('div', { class: 'grid-cards' }, bones(4, 'card')), lines(5)],
    panes: () => h('div', { class: 'panes' }, lines(12), h('div', { class: 'lines' }, bones(1, 'title'), bones(11, 'line'))),
    lines: () => lines(12),
  };
  return h('div', { class: 'loading', role: 'status', 'aria-busy': 'true' }, h('span', { class: 'visually-hidden' }, 'Loading…'), shapes[shape]());
}

export function empty(iconName, title, text, ...actions) {
  return h('div', { class: 'empty' }, icon(iconName, 40), h('h3', {}, title), text && h('p', {}, text), actions.length ? h('div', { class: 'toolbar' }, actions) : null);
}

export function failure(problem, retry) {
  const refused = problem instanceof ApiError && problem.status === 403;
  return empty(refused ? 'lock' : 'alert',
    refused ? 'You may not see this' : 'Something went wrong',
    refused ? 'Your role does not allow it. Ask an administrator if you need it.' : problem.message,
    retry ? h('button', { class: 'button', onclick: retry }, icon('refresh', 16), 'Try again') : null);
}

/**
 * The model in outline, fetched once and again after it changes: its cubes and dashboards,
 * and its dimensions by name, type and `element_count`. A dimension's elements are fetched
 * apart, only where they are shown, however many there are.
 */
export async function model() {
  if (!state.model) {
    const outline = await get('/model/outline');
    state.model = { dimensions: outline.dimensions, cubes: outline.cubes, dashboards: outline.dashboards };
    state.version = outline.version;
  }
  return state.model;
}

/* ---- Server status ---- */

let watching;
let watched = false;
/** Whether the Epiphany server answered when last asked; null before it is first asked. */
let reachable = null;

/**
 * Keeps the connection's status in the bar. The server is asked every half minute while the
 * page is in view, every five seconds while it does not answer, and at once when the page
 * comes back into view or the network comes back; and every call the application makes says
 * as it goes whether it reached it.
 */
function watch() {
  if (!watched) {
    watched = true;
    document.addEventListener('visibilitychange', () => (document.hidden ? clearTimeout(watching) : check()));
    addEventListener('online', check);
    addEventListener('offline', () => setStatus(false));
    events.addEventListener('reached', (event) => setStatus(event.detail));
  }
  setStatus(reachable ?? true);
  schedule();
}

function schedule() {
  clearTimeout(watching);
  if (!document.hidden) watching = setTimeout(check, reachable === false ? 5000 : 30000);
}

async function check() {
  try {
    const session = await api('GET', '/gui/session', undefined, { quiet: true });
    if (!session.signed_in && !session.open && state.user && !state.open) {
      events.dispatchEvent(new Event('signed-out'));
    }
  } catch {
    // The call has said whether the server was reached.
  }
  schedule();
}

function setStatus(up) {
  const was = reachable;
  reachable = up;
  if (was === false && up) notify('The Epiphany server answers again.', 'good');
  if (was !== up) schedule();
  // Changed only when it changes, so that a reader is told once, not at every call.
  const now = up ? 'up' : 'down';
  if (!statusElement || statusElement.dataset.state === now) return;
  statusElement.dataset.state = now;
  statusElement.textContent = up ? 'Connected' : 'Unreachable';
  statusElement.title = up ? 'The Epiphany server is answering' : 'The Epiphany server is not answering';
}

/* ---- Command palette ---- */

async function palette() {
  if (document.querySelector('dialog.palette')) return;
  const pages = visible().map((entry) => ({ label: entry.title, kind: 'Page', icon: entry.icon, run: () => go(entry.path) }));
  const others = [
    { label: 'Use the light theme', kind: 'Theme', icon: 'sun', run: () => chooseTheme('light') },
    { label: 'Use the dark theme', kind: 'Theme', icon: 'moon', run: () => chooseTheme('dark') },
    { label: 'Follow the system theme', kind: 'Theme', icon: 'monitor', run: () => chooseTheme('system') },
    { label: 'Show the keyboard shortcuts', kind: 'Help', icon: 'keyboard', run: shortcuts },
    ...(state.open ? [] : [{ label: 'Sign out', kind: 'Session', icon: 'out', run: signOut }]),
  ];
  // The pages are there at once; the cubes and dashboards join them as the model comes.
  let found = [];
  let finding = !state.model;

  const input = h('input', { placeholder: 'Type a cube, page or command', 'aria-label': 'Search', role: 'combobox', 'aria-expanded': 'true', 'aria-controls': 'palette-list', autocomplete: 'off' });
  const list = h('ul', { id: 'palette-list', role: 'listbox' });
  const element = h('dialog', { class: 'palette', 'aria-label': 'Search and commands' }, input, list);
  let shown = [];
  let selected = 0;
  const draw = () => {
    const words = input.value.toLowerCase().split(/\s+/).filter(Boolean);
    shown = [...pages, ...found, ...others].filter((command) => words.every((word) => command.label.toLowerCase().includes(word)));
    selected = Math.min(selected, Math.max(0, shown.length - 1));
    mount(list, shown.length
      ? shown.map((command, index) => h('li', {
        role: 'option',
        id: `palette-${index}`,
        'aria-selected': String(index === selected),
        onpointermove: () => {
          if (selected !== index) {
            selected = index;
            draw();
          }
        },
        onclick: () => choose(index),
      }, icon(command.icon, 16), command.label, h('span', { class: 'kind' }, command.kind)))
      : finding ? null : h('li', { class: 'none' }, 'Nothing matches'),
    finding ? h('li', { class: 'none finding' }, h('span', { class: 'spinner small', 'aria-hidden': 'true' }), 'Finding cubes and dashboards…') : null);
    input.setAttribute('aria-activedescendant', shown.length ? `palette-${selected}` : '');
    list.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  };
  const choose = (index) => {
    const command = shown[index];
    element.close();
    command?.run();
  };
  input.addEventListener('input', () => {
    selected = 0;
    draw();
  });
  input.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      selected = (selected + (event.key === 'ArrowDown' ? 1 : -1) + shown.length) % Math.max(shown.length, 1);
      draw();
    } else if (event.key === 'Enter') {
      event.preventDefault();
      choose(selected);
    }
  });
  element.addEventListener('close', () => element.remove());
  element.addEventListener('click', (event) => {
    if (event.target === element) element.close();
  });
  document.body.append(element);
  draw();
  element.showModal();
  input.focus();
  try {
    const definition = await model();
    found = [
      ...definition.cubes.map((cube) => ({ label: `Explore ${cube.name}`, kind: 'Cube', icon: 'cube', run: () => go(`/explore?cube=${encodeURIComponent(cube.name)}`) })),
      ...(definition.dashboards ?? []).map((dashboard) => ({ label: dashboard.name, kind: 'Dashboard', icon: 'dashboard', run: () => go(`/dashboards?name=${encodeURIComponent(dashboard.name)}`) })),
    ];
  } catch {
    // The pages are still there to choose.
  }
  finding = false;
  if (!element.open) return;
  // The command chosen stays chosen as the others join the list.
  const was = shown[selected];
  draw();
  if (was && shown.includes(was) && shown.indexOf(was) !== selected) {
    selected = shown.indexOf(was);
    draw();
  }
}

addEventListener('keydown', (event) => {
  if (!state.user) return;
  const typing = event.target.closest?.('input, textarea, select, [contenteditable]');
  if ((event.key === 'k' && (event.ctrlKey || event.metaKey)) || (event.key === '/' && !typing)) {
    event.preventDefault();
    palette();
  } else if (event.key === '?' && !typing && !document.querySelector('dialog[open]')) {
    event.preventDefault();
    shortcuts();
  }
});

/** Lists the keys the application takes; the list is fetched the first time it is asked for. */
function shortcuts() {
  import('./shortcuts.js').then((module) => module.showShortcuts());
}

/* ---- Overview ---- */

async function overview(target) {
  const definition = await model();
  const elements = definition.dimensions.reduce((total, dimension) => total + dimension.element_count, 0);
  const stat = (label, value, iconName) => h('div', { class: 'card tile' },
    h('div', { class: 'tile-icon' }, icon(iconName, 20)),
    h('div', { class: 'stat' }, value.toLocaleString()),
    h('div', { class: 'muted' }, label));
  const cubes = definition.cubes.map((cube) => h('a', {
    class: 'card tile',
    href: `/explore?cube=${encodeURIComponent(cube.name)}`,
    'data-link': '',
  },
  h('div', { class: 'toolbar' }, h('div', { class: 'tile-icon' }, icon('cube', 20)), h('div', { class: 'grow' }),
    cube.rules?.length ? h('span', { class: 'badge accent' }, `${cube.rules.length} rule${cube.rules.length === 1 ? '' : 's'}`) : null,
    views(cube) ? h('span', { class: 'badge' }, `${views(cube)} view${views(cube) === 1 ? '' : 's'}`) : null),
  h('h3', {}, cube.name),
  h('p', { class: 'muted' }, cube.dimensions.join(' · '))));
  const dashboards = (definition.dashboards ?? []).map((dashboard) => h('a', {
    class: 'card tile',
    href: `/dashboards?name=${encodeURIComponent(dashboard.name)}`,
    'data-link': '',
  },
  h('div', { class: 'toolbar' }, h('div', { class: 'tile-icon' }, icon('dashboard', 20)), h('div', { class: 'grow' }),
    h('span', { class: 'badge' }, `${dashboard.tiles.length} tile${dashboard.tiles.length === 1 ? '' : 's'}`)),
  h('h3', {}, dashboard.name),
  dashboard.description ? h('p', { class: 'muted' }, dashboard.description) : null));
  mount(target, h('div', { class: 'page' },
    h('div', { class: 'page-head' },
      h('div', { class: 'grow' }, h('h1', {}, state.open ? 'Welcome' : `Welcome, ${state.user.name}`), h('p', {}, 'Choose a cube to explore, or press Ctrl K to find anything.'))),
    h('div', { class: 'grid-cards' },
      stat('Cubes', definition.cubes.length, 'cube'),
      stat('Dimensions', definition.dimensions.length, 'dimension'),
      stat('Elements', elements, 'member'),
      stat('Model version', Number(state.version ?? 0), 'model')),
    h('h2', { class: 'section-title' }, 'Cubes'),
    definition.cubes.length
      ? h('div', { class: 'grid-cards' }, cubes)
      : empty('cube', 'No cubes yet', 'An administrator adds cubes to the model.'),
    dashboards.length ? h('h2', { class: 'section-title' }, 'Dashboards') : null,
    dashboards.length ? h('div', { class: 'grid-cards' }, dashboards) : null));
}

/** How many views a cube has saved: they come keyed by name. */
const views = (cube) => Object.keys(cube.views ?? {}).length;

window.addEventListener('unhandledrejection', (event) => {
  if (event.reason instanceof ApiError) notify(event.reason.message, 'bad');
});

start();
