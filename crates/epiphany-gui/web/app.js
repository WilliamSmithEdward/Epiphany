// The application: signing in, the shell around every page, and moving between pages.

import { ApiError, api, events, get } from './api.js';
import { marked } from './tree.js';
import { counted, dialog, h, icon, invalid, KINDS, menu, MOD, mount, notify, revealable, when, working } from './ui.js';

/** Who is signed in, and the model as last fetched. */
export const state = { user: null, open: false, model: null, version: null };

/**
 * The pages, in the order the navigation lists them. `admin` pages need the admin role; a
 * user's `own` page is reached from their menu, not the navigation, where users sign in;
 * `shape` is the outline shown while a page loads.
 */
const PAGES = [
  { path: '/', title: 'Overview', icon: 'home', shape: 'cards', load: async () => ({ render: overview }) },
  { path: '/explore', title: 'Explore', icon: 'explore', shape: 'panes', load: () => import('./explore.js') },
  { path: '/dashboards', title: 'Dashboards', icon: 'dashboard', shape: 'cards', load: () => import('./dashboards.js') },
  { path: '/model', title: 'Model', icon: 'model', shape: 'panes', load: () => import('./model.js'), section: 'Manage', admin: true },
  { path: '/admin', title: 'Administration', icon: 'admin', shape: 'cards', load: () => import('./admin.js'), admin: true },
  { path: '/keys', title: 'API keys', icon: 'key', shape: 'cards', load: () => import('./keys.js'), own: true },
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
  return h('a', { class: 'brand', href: '/', 'data-link': '' }, h('img', { src: '/icon.svg', alt: '' }), h('span', {}, 'Epiphany'));
}

/** The browser's title for what is shown: its names, from the most particular, then the application's. */
function setTitle(...names) {
  document.title = [...names.filter(Boolean), 'Epiphany'].join(' · ');
}

function signIn(reason) {
  setTitle('Sign in');
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
          shell(true);
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
  h('div', { class: 'field' }, h('label', { for: 'password' }, 'Password'), revealable(password)),
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
      h('div', { class: 'field' }, h('label', { for: 'again-password' }, 'Password'), revealable(password))),
    onsubmit: async () => {
      try {
        return await api('POST', '/gui/session', { name: name.value.trim(), password: password.value });
      } catch (problem) {
        password.select();
        throw problem.status === 401 ? invalid(password, 'That name and password do not match.') : new Error(problem.message);
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
    shell(true);
    return;
  }
  notify('Signed in again. Carry on where you were.', 'good');
  // A page that could not be drawn for want of a session is drawn now, and what failed on the
  // page for want of one, and is shown still, is tried again.
  if (!active) move();
  for (const failed of [...unsigned]) {
    unsigned.delete(failed);
    if (failed.shown.isConnected) failed.retry();
  }
}

events.addEventListener('model-changed', () => {
  state.model = null;
});

/* ---- The shell ---- */

let page;
let shellElement;
let statusElement;
let announcer;

const isAdmin = () => state.user?.role === 'admin';
const visible = () => PAGES.filter((entry) => (!entry.admin || isAdmin()) && (!entry.own || !state.open));

let fetchedAhead = false;

/**
 * Once a page is shown, the other pages' modules are fetched while the browser has nothing
 * else to do, so that going to one does not wait for its files, fetched one import after
 * another.
 */
function fetchPagesAhead() {
  if (fetchedAhead) return;
  fetchedAhead = true;
  const idle = window.requestIdleCallback ?? ((then) => setTimeout(then, 1000));
  idle(() => {
    for (const entry of visible()) entry.load().catch(() => {});
  });
}

/** The application around the page; `signedIn` as the user has just signed in, to be told where they are. */
function shell(signedIn = false) {
  const narrow = localStorage.getItem('epiphany.nav') === 'narrow';
  announcer = h('p', { class: 'visually-hidden', 'aria-live': 'polite', 'aria-atomic': 'true' });
  statusElement = h('span', { class: 'status', role: 'status', title: 'The Epiphany server is answering' }, 'Connected');
  const nav = h('nav', { class: 'nav', 'aria-label': 'Pages' });
  let section;
  for (const entry of visible().filter((each) => !each.own)) {
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
      state.open ? null : { label: 'API keys', icon: 'key', action: () => go('/keys') },
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
    page,
    announcer);
  mount(root, shellElement);
  if (signedIn) move();
  else route();
  watch();
}

function roleName(role) {
  return { admin: 'Administrator', write: 'Can write', read: 'Can read', none: 'No access' }[role] ?? role;
}

/* ---- Moving between pages ---- */

let rendering = 0;
/**
 * The page shown, which may ask to stay when it is left, by its `leave`, say it holds changes
 * not saved, by its `unsaved`, and move to other parameters in place, by its `update`; and its
 * path, without them.
 */
let active = null;
let activePath = null;
let activeEntry = null;
let shownPath = location.pathname + location.search;

/** How many places visited lately the overview lists. */
const PLACES_KEPT = 6;

/** Where the places a user visited lately are kept in this browser, the latest first. */
const placesKey = () => `epiphany.places.${state.user?.name?.toLowerCase() ?? ''}`;

function placesVisited() {
  try {
    return JSON.parse(localStorage.getItem(placesKey())) ?? [];
  } catch {
    return [];
  }
}

/** Keeps the page shown, by its address and title, among the places visited lately; the overview is not one. */
function visited(entry) {
  if (!entry || entry.path === '/') return;
  const path = location.pathname + location.search;
  const title = document.title.replace(/^• /, '').split(' · ').slice(0, -1).join(' · ');
  const place = { path, title, icon: entry.icon, at: Math.round(Date.now() / 1000) };
  localStorage.setItem(placesKey(), JSON.stringify([place, ...placesVisited().filter((other) => other.path !== path && other.title !== title)].slice(0, PLACES_KEPT)));
}

// The browser asks before the page is closed or loaded afresh while it holds changes not saved.
addEventListener('beforeunload', (event) => {
  if (active?.unsaved?.()) event.preventDefault();
});

/** Shows the page the address names; whether it moved to it, rather than staying where it was. */
async function route() {
  if (!state.user) return false;
  if (active?.leave && !(await active.leave())) {
    history.pushState(null, '', shownPath);
    return false;
  }
  shownPath = location.pathname + location.search;
  const path = location.pathname.replace(/\/+$/, '') || '/';
  // The page shown, asked for again with other parameters, moves to them where it stands:
  // what stays the same is not drawn again, and nothing comes in between.
  if (active?.update && path === activePath) {
    active.update(new URLSearchParams(location.search));
    visited(activeEntry);
    return true;
  }
  active = null;
  activePath = path;
  const entry = visible().find((candidate) => candidate.path === path)
    ?? visible().find((candidate) => candidate.path !== '/' && path.startsWith(`${candidate.path}/`));
  activeEntry = entry;
  for (const link of shellElement.querySelectorAll('.nav a')) {
    link.toggleAttribute('aria-current', false);
    if (entry && link.getAttribute('href') === entry.path) link.setAttribute('aria-current', 'page');
  }
  const ticket = ++rendering;
  if (!entry) {
    setTitle('Not found');
    mount(page, h('div', { class: 'page' }, empty('search', 'There is no such page', 'Check the address, or choose a page from the navigation.')));
    return true;
  }
  // The page names what it shows before its own name, for tabs and history to tell apart,
  // marked while it holds changes not saved, for as long as it is the page shown.
  const titled = (name, unsaved = false) => ticket === rendering && setTitle(name && `${unsaved ? '• ' : ''}${name}`, entry.title);
  titled();
  awaitNext(entry.shape);
  try {
    const module = await entry.load();
    if (ticket !== rendering) return false;
    const shown = await module.render(page, { state, params: new URLSearchParams(location.search), path, title: entry.title, titled });
    // A page another move has passed by is not the one shown, to be asked before leaving.
    if (ticket === rendering) {
      active = shown;
      visited(entry);
      fetchPagesAhead();
    }
  } catch (problem) {
    if (ticket !== rendering) return false;
    mount(page, h('div', { class: 'page' }, failure(problem, route)));
  }
  return ticket === rendering;
}

/**
 * Moves to the page the address names and says so, to a screen reader, as the browser says the
 * title of a page it loads: the focus stays where it was, the link or the button followed.
 */
async function move() {
  if (!(await route())) return;
  announce(document.title.split(' · ').slice(0, -1).join(', '));
}

/** Says `words` to a screen reader, as it says a change it was not looking at. */
export function announce(words) {
  announcer.textContent = '';
  // Said again, the same words are said again.
  setTimeout(() => {
    announcer.textContent = words;
  }, 100);
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

/** Puts `path` in the address in place of the one there, as the page shown now has it, without moving. */
export function readdress(path) {
  history.replaceState(null, '', path);
  shownPath = path;
}

/** Goes to a page within the application. */
export function go(path) {
  if (path === location.pathname + location.search) return;
  history.pushState(null, '', path);
  move();
}

addEventListener('popstate', move);
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

/** What failed for want of a session, each with its way to be tried again. */
const unsigned = new Set();

/** What went wrong, with a button to try again where `retry` is given, and `also` beside it. */
export function failure(problem, retry, also = null) {
  const refused = problem instanceof ApiError && problem.status === 403;
  const shown = empty(refused ? 'lock' : 'alert',
    refused ? 'You may not see this' : 'Something went wrong',
    refused ? 'Your role does not allow it. Ask an administrator if you need it.' : problem.message,
    also,
    retry ? h('button', { class: 'button', onclick: retry }, icon('refresh', 16), 'Try again') : null);
  // Failed for want of a session, it is tried again by itself once the user signs in again.
  if (retry && problem instanceof ApiError && problem.status === 401) unsigned.add({ shown, retry });
  return shown;
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

/** Where the commands chosen lately are kept, the latest first. */
const RECENT = 'epiphany.palette.recent';
const RECENT_KEPT = 5;
const commandKey = (command) => `${command.kind}: ${command.label}`;

function chosenLately() {
  try {
    return JSON.parse(localStorage.getItem(RECENT)) ?? [];
  } catch {
    return [];
  }
}

/** Whether `word` begins one of the words of `text`. */
function beginsWord(text, word) {
  for (let at = text.indexOf(word); at >= 0; at = text.indexOf(word, at + 1)) {
    if (at === 0 || !/[\p{L}\p{N}]/u.test(text[at - 1])) return true;
  }
  return false;
}

/**
 * The commands with every word typed, in their label or else in what they belong to, the best
 * matches first: words that begin the label, then words that begin one of its words, then
 * any. With nothing typed, the commands chosen lately come first.
 */
function ranked(commands, words) {
  if (!words.length) {
    const lately = chosenLately();
    const place = (command) => {
      const at = lately.indexOf(commandKey(command));
      return at < 0 ? lately.length : at;
    };
    return [...commands].sort((a, b) => place(a) - place(b));
  }
  const scored = [];
  for (const command of commands) {
    const label = command.label.toLowerCase();
    const context = command.context?.toLowerCase() ?? '';
    let score = 0;
    for (const word of words) {
      const points = label.startsWith(word) ? 3 : beginsWord(label, word) ? 2 : label.includes(word) || context.includes(word) ? 1 : 0;
      if (!points) {
        score = 0;
        break;
      }
      score += points;
    }
    if (score) scored.push([score, command]);
  }
  return scored.sort((a, b) => b[0] - a[0]).map(([, command]) => command);
}

async function palette() {
  // One closed a moment ago may wait for its close event to be taken away.
  if (document.querySelector('dialog.palette[open]')) return;
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
    shown = ranked([...pages, ...found, ...others], words);
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
      }, icon(command.icon, 16), h('span', {}, marked(command.label, words)), h('span', { class: 'kind' }, command.kind)))
      : finding ? null : h('li', { class: 'none' }, 'Nothing matches'),
    finding ? h('li', { class: 'none finding' }, h('span', { class: 'spinner small', 'aria-hidden': 'true' }), 'Finding cubes and dashboards…') : null);
    input.setAttribute('aria-activedescendant', shown.length ? `palette-${selected}` : '');
    list.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  };
  const choose = (index) => {
    const command = shown[index];
    element.close();
    if (!command) return;
    const key = commandKey(command);
    localStorage.setItem(RECENT, JSON.stringify([key, ...chosenLately().filter((other) => other !== key)].slice(0, RECENT_KEPT)));
    command.run();
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
    const explore = (cube, view) => `/explore?cube=${encodeURIComponent(cube.name)}${view ? `&view=${encodeURIComponent(view)}` : ''}`;
    found = [
      ...definition.cubes.map((cube) => ({ label: `Explore ${cube.name}`, kind: 'Cube', icon: 'cube', run: () => go(explore(cube)) })),
      // Each view marked as it opens: its chart's icon, or the grid's.
      ...definition.cubes.flatMap((cube) => Object.entries(cube.views ?? {}).map(([view, { chart }]) => ({
        label: view, context: cube.name, kind: `View of ${cube.name}`, icon: KINDS[chart?.kind]?.[1] ?? 'grid', run: () => go(explore(cube, view)),
      }))),
      ...(definition.dashboards ?? []).map((dashboard) => ({ label: dashboard.name, kind: 'Dashboard', icon: 'dashboard', run: () => go(`/dashboards?name=${encodeURIComponent(dashboard.name)}`) })),
      ...(isAdmin() ? [
        ...definition.dimensions.map((dimension) => ({ label: `Edit ${dimension.name}`, kind: 'Dimension', icon: 'dimension', run: () => go(`/model?dimension=${encodeURIComponent(dimension.name)}`) })),
        ...definition.cubes.map((cube) => ({ label: `Edit ${cube.name}`, kind: 'Cube in the model', icon: 'model', run: () => go(`/model?cube=${encodeURIComponent(cube.name)}`) })),
      ] : []),
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
  } else if (event.key.toLowerCase() === 's' && (event.ctrlKey || event.metaKey) && !event.shiftKey && !event.altKey) {
    // The page's changes are saved, as its Save button saves them: saving the page as a file,
    // as the browser would, is of no use here. Pressed again while it saves, nothing more.
    event.preventDefault();
    if (saving || document.querySelector('dialog[open]')) return;
    if (!active?.unsaved?.()) {
      notify('Nothing to save.');
      return;
    }
    saving = Promise.resolve(active.saveChanges()).finally(() => {
      saving = null;
    });
  }
});

/** The changes being saved by the keys, while they are. */
let saving = null;

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
    cube.rules?.length ? h('span', { class: 'badge accent' }, counted(cube.rules.length, 'rule')) : null,
    views(cube) ? h('span', { class: 'badge' }, counted(views(cube), 'view')) : null),
  h('h3', {}, cube.name),
  h('p', { class: 'muted' }, cube.dimensions.join(' · '))));
  const dashboards = (definition.dashboards ?? []).map((dashboard) => h('a', {
    class: 'card tile',
    href: `/dashboards?name=${encodeURIComponent(dashboard.name)}`,
    'data-link': '',
  },
  h('div', { class: 'toolbar' }, h('div', { class: 'tile-icon' }, icon('dashboard', 20)), h('div', { class: 'grow' }),
    h('span', { class: 'badge' }, counted(dashboard.tiles.length, 'tile'))),
  h('h3', {}, dashboard.name),
  dashboard.description ? h('p', { class: 'muted' }, dashboard.description) : null));
  // The places visited lately, to go back to where one was.
  const places = placesVisited().map((place) => h('li', {},
    h('a', { href: place.path, 'data-link': '' }, icon(place.icon, 16), h('span', { class: 'grow' }, place.title), when(place.at))));
  mount(target, h('div', { class: 'page' },
    h('div', { class: 'page-head' },
      h('div', { class: 'grow' }, h('h1', {}, state.open ? 'Welcome' : `Welcome, ${state.user.name}`), h('p', {}, `Choose a cube to explore, or press ${MOD} K to find anything.`))),
    h('div', { class: 'grid-cards' },
      stat('Cubes', definition.cubes.length, 'cube'),
      stat('Dimensions', definition.dimensions.length, 'dimension'),
      stat('Elements', elements, 'member'),
      stat('Model version', Number(state.version ?? 0), 'model')),
    places.length ? h('h2', { class: 'section-title' }, 'Recent') : null,
    places.length ? h('ul', { class: 'recent', 'aria-label': 'Places visited lately' }, places) : null,
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
