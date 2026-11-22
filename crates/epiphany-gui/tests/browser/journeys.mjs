// The GUI's main journeys, taken in a browser as a person takes them: signing in, and again
// when the session ends; exploring a cube, an MDX query and writing a value back; changing
// the model; administering users and viewing the server as one of them; and scrolling a grid
// of 100,000 cells. Each page is checked as it stands by audit.js, in both themes.
//
//   cargo build -p epiphany-server -p epiphany-gui
//   node crates/epiphany-gui/tests/browser/journeys.mjs [--headed] [--keep]
//
// Nothing is installed. Node drives Chrome, or Edge, over the DevTools protocol on a pipe;
// the browser is found where it is usually installed, or named by CHROME. The servers run
// the builds in target/debug, the GUI with the files built into it, on a copy of lab/model
// in a folder of their own under target, on ports of their own, with users made for the run
// whose passwords are new each time. A run that fails keeps its folder, with a picture of
// the page as it failed, failed.png; --keep keeps it anyway, and --headed shows the browser.

import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { delimiter, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('../../../../', import.meta.url));
const AUDIT = readFileSync(new URL('./audit.js', import.meta.url), 'utf8');
const HEADED = process.argv.includes('--headed');
const KEEP = process.argv.includes('--keep');
/** How long anything is waited for, in milliseconds. */
const PATIENCE = 30_000;

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
const secret = () => randomBytes(18).toString('base64url');
const quoted = JSON.stringify;

function expect(holds, message) {
  if (!holds) throw new Error(message);
}

/* ---- The servers ---- */

function program(name) {
  const path = join(ROOT, 'target', 'debug', `${name}${process.platform === 'win32' ? '.exe' : ''}`);
  if (!existsSync(path)) throw new Error(`${path} is not built: cargo build -p epiphany-server -p epiphany-gui`);
  return path;
}

const started = [];
/** What each server has said, to be shown when a journey fails. */
const said = new Map();

/** Starts a server, resolving with the address it says it listens on. */
function serve(path, args) {
  const child = spawn(path, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  started.push(child);
  said.set(path, '');
  return new Promise((resolve, reject) => {
    for (const stream of [child.stdout, child.stderr]) {
      stream.setEncoding('utf8').on('data', (text) => {
        said.set(path, said.get(path) + text);
        const address = /listening on (http:\/\/[^\s,]+)/.exec(said.get(path))?.[1];
        if (address) resolve(address);
      });
    }
    child.on('exit', (code) => reject(new Error(`${path} stopped (${code}): ${said.get(path)}`)));
    setTimeout(() => reject(new Error(`${path} did not start: ${said.get(path)}`)), PATIENCE);
  });
}

/** Adds a user to a server's folder, the password given as a person would type it. */
function addUser(folder, name, role, password) {
  return new Promise((resolve, reject) => {
    const child = spawn(program('epiphany-server'), ['user', folder, name, role], { stdio: ['pipe', 'ignore', 'pipe'] });
    let said = '';
    child.stderr.setEncoding('utf8').on('data', (text) => {
      said += text;
    });
    child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error(`Adding ${name} failed: ${said}`))));
    child.stdin.end(`${password}\n`);
  });
}

/* ---- The browser, over the DevTools protocol ---- */

function findBrowser() {
  if (process.env.CHROME) return process.env.CHROME;
  const { env } = process;
  const places = {
    win32: [
      [env.PROGRAMFILES, 'Google/Chrome/Application/chrome.exe'],
      [env['PROGRAMFILES(X86)'], 'Google/Chrome/Application/chrome.exe'],
      [env.LOCALAPPDATA, 'Google/Chrome/Application/chrome.exe'],
      [env['PROGRAMFILES(X86)'], 'Microsoft/Edge/Application/msedge.exe'],
      [env.PROGRAMFILES, 'Microsoft/Edge/Application/msedge.exe'],
    ].filter(([folder]) => folder).map(([folder, path]) => join(folder, path)),
    darwin: ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge'],
  }[process.platform] ?? (env.PATH ?? '').split(delimiter)
    .flatMap((folder) => ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'microsoft-edge'].map((name) => join(folder, name)));
  const found = places.find((path) => existsSync(path));
  if (!found) throw new Error('Neither Chrome nor Edge was found: name one in the CHROME environment variable.');
  return found;
}

/**
 * The browser, spoken to on a pipe: messages of JSON, each ended by a zero byte, written to
 * its fourth stream and read from its fifth.
 */
class Browser {
  constructor(profile) {
    const flags = [
      `--user-data-dir=${profile}`,
      '--remote-debugging-pipe',
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-extensions',
      '--disable-sync',
      '--disable-background-networking',
      '--disable-component-update',
      '--password-store=basic',
      '--lang=en-US',
      '--window-size=1400,900',
      ...(HEADED ? [] : ['--headless=new']),
      // A Linux machine for builds may not let the browser make its sandbox; it is shown only
      // the pages of the servers started here.
      ...(process.platform === 'linux' && process.env.CI ? ['--no-sandbox'] : []),
      'about:blank',
    ];
    this.child = spawn(findBrowser(), flags, { stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe'] });
    started.push(this.child);
    this.calls = new Map();
    this.listeners = new Set();
    this.count = 0;
    let unread = '';
    this.child.stdio[4].setEncoding('utf8').on('data', (text) => {
      unread += text;
      for (let end = unread.indexOf('\0'); end >= 0; end = unread.indexOf('\0')) {
        this.receive(JSON.parse(unread.slice(0, end)));
        unread = unread.slice(end + 1);
      }
    });
  }

  receive(message) {
    const call = this.calls.get(message.id);
    if (!call) {
      for (const listener of this.listeners) listener(message);
      return;
    }
    this.calls.delete(message.id);
    if (message.error) call.reject(new Error(`${call.method}: ${message.error.message}`));
    else call.resolve(message.result);
  }

  send(method, params = {}, sessionId = undefined) {
    const id = (this.count += 1);
    this.child.stdio[3].write(`${quoted({ id, method, params, sessionId })}\0`);
    return new Promise((resolve, reject) => this.calls.set(id, { method, resolve, reject }));
  }

  /** A new tab, at the GUI's address. */
  async page(base) {
    const { targetId } = await this.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await this.send('Target.attachToTarget', { targetId, flatten: true });
    const page = new Page(this, sessionId, base);
    await page.call('Page.enable');
    await page.call('Runtime.enable');
    // What moves stands still, so that what is checked is what stays on the page; and
    // numbers are written as in the United States, wherever the run is.
    await page.call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'reduce' }] });
    await page.call('Emulation.setLocaleOverride', { locale: 'en-US' });
    return page;
  }
}

const KEYS = { Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46, Home: 36, End: 35, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, F2: 113 };
const SHIFT = 8;

/** A tab, and what a person does in it: going to a page, clicking, typing, pressing keys. */
class Page {
  constructor(browser, sessionId, base) {
    this.browser = browser;
    this.sessionId = sessionId;
    this.base = base;
    this.waiters = new Set();
    /** Errors the page's scripts threw or reported, which no journey expects. */
    this.errors = [];
    browser.listeners.add(({ method, params, sessionId: from }) => {
      if (from !== sessionId) return;
      if (method === 'Runtime.exceptionThrown') this.errors.push(params.exceptionDetails.exception?.description ?? params.exceptionDetails.text);
      if (method === 'Runtime.consoleAPICalled' && params.type === 'error') this.errors.push(params.args.map((arg) => arg.value ?? arg.description).join(' '));
      for (const waiter of this.waiters) {
        if (waiter.method !== method) continue;
        this.waiters.delete(waiter);
        waiter.resolve(params);
      }
    });
  }

  call(method, params) {
    return this.browser.send(method, params, this.sessionId);
  }

  /** The next event of a kind, as a promise. */
  once(method) {
    return new Promise((resolve) => this.waiters.add({ method, resolve }));
  }

  async evaluate(expression) {
    const { result, exceptionDetails } = await this.call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (exceptionDetails) throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text);
    return result.value;
  }

  /** Waits for an expression to come true in the page, giving what it comes to. */
  async waitFor(expression, what) {
    const until = Date.now() + PATIENCE;
    for (;;) {
      const value = await this.evaluate(expression).catch(() => undefined);
      if (value) return value;
      if (Date.now() > until) throw new Error(`Waited in vain for ${what}.`);
      await sleep(100);
    }
  }

  /** Until nothing is loading or being waited for. */
  settled() {
    return this.waitFor(`document.readyState === 'complete' && !document.querySelector('.loading, .skeleton, [aria-busy="true"]') && !document.documentElement.classList.contains('waiting')`, 'the page to settle');
  }

  async goto(path) {
    const loaded = this.once('Page.loadEventFired');
    await this.call('Page.navigate', { url: new URL(path, this.base).href });
    await loaded;
    await this.settled();
  }

  /** The middle of the element `css` finds, of the name given if one is, brought into view. */
  point(css, name) {
    return this.waitFor(`(() => {
      const element = [...document.querySelectorAll(${quoted(css)})].find((each) => {
        const box = each.getBoundingClientRect();
        const named = ${quoted(name ?? null)};
        return box.width > 0 && box.height > 0 && (named === null || each.getAttribute('aria-label') === named || each.textContent.trim() === named);
      });
      if (!element) return null;
      element.scrollIntoView({ block: 'center', inline: 'center' });
      const box = element.getBoundingClientRect();
      return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    })()`, name ? `“${name}”` : css);
  }

  async click(css, name) {
    const { x, y } = await this.point(css, name);
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await this.call('Input.dispatchMouseEvent', { type, x, y, button: 'left', buttons: type === 'mousePressed' ? 1 : 0, clickCount: 1 });
    }
  }

  /** A key pressed and let go, as `Enter`, `ArrowRight` or a letter, with Shift or Ctrl held. */
  async press(key, modifiers = 0) {
    const named = KEYS[key];
    const down = { key, code: named ? key : `Key${key.toUpperCase()}`, windowsVirtualKeyCode: named ?? key.toUpperCase().charCodeAt(0), modifiers };
    // Enter types a line break, by which a form is sent.
    const typing = key === 'Enter' && !modifiers ? { type: 'keyDown', text: '\r' } : { type: 'rawKeyDown' };
    await this.call('Input.dispatchKeyEvent', { ...down, ...typing });
    await this.call('Input.dispatchKeyEvent', { ...down, type: 'keyUp' });
  }

  /** Text typed a key at a time, into whatever has the keys. */
  async type(text) {
    for (const key of text) {
      await this.call('Input.dispatchKeyEvent', { type: 'keyDown', key, text: key });
      await this.call('Input.dispatchKeyEvent', { type: 'keyUp', key });
    }
  }

  /** A field clicked into and its text replaced. */
  async fill(css, text) {
    await this.click(css);
    await this.evaluate('document.activeElement.select?.()');
    await this.call('Input.insertText', { text });
  }

  /** A call to the Epiphany server through the GUI's, as the signed-in user. */
  api(method, path, body) {
    return this.evaluate(`fetch(${quoted(`/api${path}`)}, {
      method: ${quoted(method)},
      headers: { 'x-requested-by': 'epiphany-gui', 'content-type': 'application/json' },
      body: ${body === undefined ? 'undefined' : quoted(quoted(body))},
    }).then(async (response) => ({ status: response.status, data: response.status === 204 ? null : await response.json() }))`);
  }

  /** The page as it stands, checked by audit.js in the light theme and the dark. */
  async audit(what) {
    const found = [];
    const was = await this.evaluate('document.documentElement.dataset.theme');
    for (const theme of ['light', 'dark']) {
      // With motion reduced, as it is here, the colours change at once.
      await this.evaluate(`document.documentElement.dataset.theme = ${quoted(theme)}`);
      for (const problem of await this.evaluate(AUDIT)) {
        found.push(`${theme}: ${problem.what}, at ${problem.where}${problem.count > 1 ? `, and ${problem.count - 1} more` : ''}`);
      }
    }
    await this.evaluate(`document.documentElement.dataset.theme = ${quoted(was)}`);
    expect(!found.length, `${what} has problems:\n${found.map((line) => `  ${line}`).join('\n')}`);
  }

  async screenshot(path) {
    const { data } = await this.call('Page.captureScreenshot', { format: 'png' });
    writeFileSync(path, Buffer.from(data, 'base64'));
  }
}

/* ---- The journeys ---- */

let page;
let folder;

async function step(name, run) {
  const began = Date.now();
  try {
    await run();
  } catch (problem) {
    const shot = join(folder, 'failed.png');
    await page.screenshot(shot).catch(() => {});
    console.log(`  FAILED ${name}\n${String(problem.message).replace(/^/gm, '    ')}\n    The page as it was: ${shot}`);
    throw problem;
  }
  console.log(`  ok ${name} (${Date.now() - began} ms)`);
}

/** Reads the grid as it is drawn: its row and column headings, and the text of each cell. */
const GRID = `(() => {
  const cells = {};
  for (const td of document.querySelectorAll('td[data-r][data-c]')) cells[td.dataset.r + ',' + td.dataset.c] = td.textContent.trim();
  const headings = (css, key) => [...document.querySelectorAll(css)].map((th) => [Number(th.dataset[key]), th.textContent.trim()]);
  return { cells, rows: headings('th.row-head[data-r1]', 'r1'), columns: headings('th.col-head[data-c1]', 'c1') };
})()`;

/** A number as the grid shows it, or null for an empty cell. */
const shownNumber = (text) => (text === '' ? null : Number(text.replace(/,/g, '').replace(/−/g, '-')));

async function signingIn({ admin }) {
  await step('The sign-in page asks for a name and a password', async () => {
    await page.goto('/');
    await page.waitFor(`document.querySelector('form #password')`, 'the sign-in form');
    await page.audit('The sign-in page');
  });
  await step('A wrong password is refused, and says so', async () => {
    await page.fill('#name', admin.name);
    await page.fill('#password', 'not the password');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.error-text:not([hidden])')?.textContent.includes('do not match')`, 'the refusal');
  });
  await step('Signing in opens the overview', async () => {
    await page.fill('#password', admin.password);
    await page.press('Enter');
    await page.waitFor(`document.querySelector('h1')?.textContent === ${quoted(`Welcome, ${admin.name}`)}`, 'the welcome');
    await page.settled();
    await page.audit('The overview');
  });
  await step('A session that ends is signed into again over the page, which stays', async () => {
    await page.click('.nav a', 'Administration');
    await page.waitFor(`document.querySelector('h1')?.textContent === 'Administration'`, 'Administration');
    await page.settled();
    expect((await page.api('DELETE', '/sessions')).status === 204, 'The session would not end.');
    await page.click('button', 'Refresh');
    await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Your session has ended'`, 'the sign-in dialog');
    await page.audit('The sign-in dialog');
    await page.fill('#again-password', admin.password);
    await page.press('Enter');
    await page.waitFor(`!document.querySelector('dialog[open]') && [...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Signed in again'))`, 'signing in again');
    await page.waitFor(`document.querySelector('h1')?.textContent === 'Administration'`, 'the page as it was');
  });
}

async function exploring() {
  const QUERY = 'SELECT {[Period].[Jan], [Period].[Feb]} ON COLUMNS, {[Account].[Salaries], [Account].[Rent]} ON ROWS FROM [General Ledger] WHERE ([Entity].[US], [Version].[Budget])';
  const at = (account, period) => [account, 'US', period, 'Budget'];
  await step('Explore shows a cube in the grid', async () => {
    const written = await page.api('POST', '/cells/write', {
      cells: [
        { cube: 'General Ledger', at: at('Salaries', 'Jan'), value: 1000 },
        { cube: 'General Ledger', at: at('Salaries', 'Feb'), value: 1100.5 },
        { cube: 'General Ledger', at: at('Rent', 'Jan'), value: -200 },
      ],
    });
    expect(written.status < 300, `The values to explore were not written: ${quoted(written.data)}`);
    await page.goto('/explore?cube=General%20Ledger');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
    await page.settled();
    await page.audit('Explore');
  });
  await step('An MDX query typed in shows what the API answers to it', async () => {
    await page.click('[role="tab"]', 'MDX');
    await page.fill('textarea.mdx-editor', QUERY);
    await page.click('button', 'Run');
    await page.waitFor(`[...document.querySelectorAll('th.row-head[data-r1]')].map((th) => th.textContent.trim()).join() === 'Salaries,Rent'`, 'the rows the query asks for');
    await page.settled();
    const shown = await page.evaluate(GRID);
    const { data } = await page.api('POST', '/mdx', { mdx: QUERY });
    const [columns, rows] = data.axes;
    const names = (axis) => axis.tuples.map((tuple) => tuple.map((member) => member.name).join(' · '));
    expect(quoted(shown.rows.map(([, name]) => name)) === quoted(names(rows)), `The grid's rows are ${quoted(shown.rows)}; the API's ${quoted(names(rows))}.`);
    expect(quoted(shown.columns.map(([, name]) => name)) === quoted(names(columns)), `The grid's columns are ${quoted(shown.columns)}; the API's ${quoted(names(columns))}.`);
    rows.tuples.forEach((_, r) => columns.tuples.forEach((__, c) => {
      const text = shown.cells[`${r},${c}`];
      const value = data.cells[r * columns.tuples.length + c] ?? null;
      expect(shownNumber(text) === value, `The grid shows ${quoted(text)} at row ${r}, column ${c}; the API gives ${quoted(value)}.`);
    }));
    await page.audit('A query typed in');
  });
  await step('A value typed into a cell is saved, and read back through the API', async () => {
    await page.click('td[data-r="1"][data-c="1"]');
    await page.waitFor(`document.querySelector('td[data-r="1"][data-c="1"]').classList.contains('editable')`, 'the cell to be writable');
    await page.type('2');
    await page.waitFor(`document.querySelector('.cell-editor')`, 'the cell\'s editor');
    await page.type('50');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.pending:not([hidden])')?.textContent.includes('1 unsaved change')`, 'the change, waiting');
    await page.audit('A change waiting to be saved');
    await page.click('.pending button', 'Save changes');
    await page.waitFor(`document.querySelector('.pending')?.hidden`, 'the change, saved');
    const { data } = await page.api('POST', '/cells/read', { cells: [{ cube: 'General Ledger', at: at('Rent', 'Feb') }] });
    expect(data.cells[0].value === 250, `The API reads ${quoted(data.cells[0])} where 250 was saved.`);
  });
  await step('The tabs over the grid go along by the arrow keys', async () => {
    await page.click('[role="tab"]', 'Grid');
    await page.press('ArrowRight');
    await page.waitFor(`document.activeElement?.textContent === 'Chart' && document.activeElement.getAttribute('aria-selected') === 'true' && document.querySelector('.chart svg')`, 'the chart, chosen by the keys');
    await page.audit('The chart');
    await page.press('ArrowRight');
    await page.waitFor(`document.activeElement?.textContent === 'MDX' && document.querySelector('textarea.mdx-editor')`, 'the MDX, chosen by the keys');
    await page.press('Home');
    await page.waitFor(`document.activeElement?.textContent === 'Grid' && document.querySelector('td[data-r]')`, 'the grid, chosen by the keys');
  });
  await step('A cell\'s comments, history and detail open beside the grid', async () => {
    await page.click('td[data-r="1"][data-c="1"]');
    await page.press('F2', SHIFT);
    await page.waitFor(`document.querySelector('.comment-panel:not([hidden]) textarea')`, 'the comments');
    await page.settled();
    await page.audit('A cell\'s comments');
    await page.click('[role="tab"]', 'History');
    await page.waitFor(`document.querySelector('.comment-panel')?.textContent.includes('250')`, 'the change in the history');
    await page.settled();
    await page.audit('A cell\'s history');
    await page.click('[role="tab"]', 'Detail');
    await page.waitFor(`document.querySelector('.comment-panel .detail-lines')`, 'the detail');
    await page.settled();
    await page.audit('A cell\'s detail');
    await page.click('button', 'Close the panel');
  });
}

async function changingTheModel() {
  await step('The model\'s editor shows a dimension', async () => {
    await page.goto('/model?dimension=Entity');
    await page.waitFor(`document.querySelector('[role="treeitem"][data-name="Total Entity"]')`, 'the tree of elements');
    await page.audit('The dimension editor');
  });
  await step('An element added in the editor is the one the API gives back', async () => {
    const before = (await page.api('GET', '/model')).data.version;
    await page.click('[role="treeitem"][data-name="Total Entity"]');
    await page.click('button', 'Add a child');
    await page.waitFor(`document.querySelector('dialog[open] #element-new')`, 'the dialog');
    await page.audit('Adding an element');
    await page.fill('#element-new', 'FR');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('[role="treeitem"][data-name="FR"]') && !document.querySelector('dialog[open]')`, 'FR in the tree');
    await page.click('.pending button', 'Save');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Saved'))`, 'the model, saved');
    const { data } = await page.api('GET', '/model');
    const entity = data.model.dimensions.find((dimension) => dimension.name === 'Entity');
    const total = entity.elements.find((element) => element.name === 'Total Entity');
    expect(data.version === before + 1, `The model is at version ${data.version}, not ${before + 1}.`);
    expect(total.children.some((child) => child.name === 'FR'), `Total Entity's children are ${quoted(total.children)}.`);
    expect(entity.elements.find((element) => element.name === 'FR')?.type === 'numeric', 'FR is not a numeric element.');
  });
}

async function administering({ reader }) {
  await step('Administration lists the users', async () => {
    await page.goto('/admin');
    await page.waitFor(`[...document.querySelectorAll('td strong')].some((name) => name.textContent === ${quoted(reader.name)})`, 'the users');
    await page.audit('Administration');
  });
  await step('An administrator views the server as a reader, who may only read', async () => {
    const loaded = page.once('Page.loadEventFired');
    await page.click('button', `View as ${reader.name}`);
    await loaded;
    await page.waitFor(`document.querySelector('.viewing')?.textContent.includes(${quoted(`Viewing as ${reader.name}`)})`, 'the bar saying whom');
    await page.settled();
    const pages = await page.evaluate(`[...document.querySelectorAll('.nav a')].map((link) => link.textContent.trim())`);
    expect(!pages.includes('Model') && !pages.includes('Administration'), `A reader is shown ${quoted(pages)}.`);
    await page.audit('Viewing as a reader');
    const refused = await page.api('POST', '/cells/write', { cells: [{ cube: 'General Ledger', at: ['Rent', 'US', 'Mar', 'Budget'], value: 1 }] });
    expect(refused.status === 403, `A write while viewing was answered ${refused.status}.`);
    await page.goto('/explore?cube=General%20Ledger');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
    await page.settled();
    await page.click('td[data-r="0"][data-c="0"]');
    await page.type('5');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('not write to it'))`, 'the refusal');
    await page.audit('A value refused');
  });
  await step('Stop goes back to the administrator, and the audit keeps the view', async () => {
    const loaded = page.once('Page.loadEventFired');
    await page.click('.viewing button', 'Stop');
    await loaded;
    await page.waitFor(`location.pathname === '/admin' && document.querySelector('h1')?.textContent === 'Administration'`, 'Administration again');
    await page.waitFor(`document.querySelector('main')?.textContent.includes(${quoted(`Viewed the server as ${reader.name}`)})`, 'the view in the audit');
    await page.settled();
  });
}

async function aLargeGrid() {
  await step('A grid of 100,000 cells draws only what is in view, and scrolls smoothly', async () => {
    const made = await page.evaluate(`(async () => {
      const call = async (method, path, body) => {
        const response = await fetch('/api' + path, { method, headers: { 'x-requested-by': 'epiphany-gui', 'content-type': 'application/json' }, body: body && JSON.stringify(body) });
        const data = response.status === 204 ? null : await response.json();
        if (!response.ok) throw new Error(data.message);
        return data;
      };
      const named = (prefix, count, width) => Array.from({ length: count }, (_, i) => prefix + String(i + 1).padStart(width, '0'));
      const rows = named('R', 2000, 4);
      const columns = named('C', 50, 2);
      let { version } = await call('GET', '/model');
      ({ version } = await call('PUT', '/dimensions/Row', { based_on: version, dimension: { name: 'Row', elements: rows.map((name) => ({ name, type: 'numeric' })) } }));
      ({ version } = await call('PUT', '/dimensions/Column', { based_on: version, dimension: { name: 'Column', elements: columns.map((name) => ({ name, type: 'numeric' })) } }));
      await call('PUT', '/cubes/Large', { based_on: version, cube: { name: 'Large', dimensions: ['Row', 'Column'] } });
      await call('POST', '/cells/write', { cells: rows.flatMap((row, r) => columns.map((column, c) => ({ cube: 'Large', at: [row, column], value: r * 100 + c }))) });
      return rows.length * columns.length;
    })()`);
    expect(made === 100_000, `${made} cells were made.`);
    await page.goto('/explore?cube=Large');
    await page.waitFor(`document.querySelector('.status-line')?.textContent.includes('2,000 rows × 50 columns')`, 'the grid of 100,000 cells');
    await page.settled();
    const drawn = await page.evaluate(`document.querySelectorAll('td[data-r]').length`);
    expect(drawn < 10_000, `${drawn} cells are drawn, where only those in view should be.`);
    await page.audit('A grid of 100,000 cells');
    // The time between frames, first at rest, then scrolling down to the end as a wheel
    // does, a little each frame. Smooth is keeping the pace the page keeps at rest, whatever
    // pace the machine paints at.
    const scrolled = await page.evaluate(`(async () => {
      const scroller = document.querySelector('.pivot-scroll');
      const frame = () => new Promise((done) => requestAnimationFrame(done));
      const timed = async (more, move) => {
        const gaps = [];
        let before = await frame();
        while (more(gaps.length)) {
          move();
          const now = await frame();
          gaps.push(now - before);
          before = now;
        }
        return gaps.sort((a, b) => a - b);
      };
      const rest = await timed((count) => count < 60, () => {});
      const moving = await timed(() => scroller.scrollTop + scroller.clientHeight < scroller.scrollHeight - 1, () => { scroller.scrollTop += 200; });
      scroller.scrollLeft = scroller.scrollWidth;
      await frame();
      await frame();
      const last = [...document.querySelectorAll('th.row-head[data-r1]')].pop();
      const cell = document.querySelector('td[data-r="' + last?.dataset.r1 + '"][data-c="49"]');
      const middle = (gaps) => gaps[Math.floor(gaps.length / 2)];
      return { rest: middle(rest), frames: moving.length, median: middle(moving), slowest: moving.at(-1), last: last?.textContent.trim(), value: cell?.textContent.trim() };
    })()`);
    console.log(`    ${scrolled.frames} frames scrolled, ${scrolled.median.toFixed(0)} ms apart as a rule and ${scrolled.slowest.toFixed(0)} ms at most, against ${scrolled.rest.toFixed(0)} ms at rest`);
    expect(scrolled.last === 'R2000' && scrolled.value === '199,949', `The last row reads ${quoted(scrolled.last)}, ${quoted(scrolled.value)}.`);
    expect(scrolled.median < scrolled.rest * 1.5, `Scrolling, frames come ${scrolled.median.toFixed(0)} ms apart as a rule, against ${scrolled.rest.toFixed(0)} ms at rest.`);
    expect(scrolled.slowest < 500, `Scrolling stood still for ${scrolled.slowest.toFixed(0)} ms.`);
  });
}

/* ---- The run ---- */

async function main() {
  mkdirSync(join(ROOT, 'target'), { recursive: true });
  folder = mkdtempSync(join(ROOT, 'target', 'browser-journeys-'));
  const data = join(folder, 'server');
  cpSync(join(ROOT, 'lab', 'model'), join(data, 'model'), { recursive: true });
  const users = { admin: { name: 'ada', password: secret() }, reader: { name: 'lin', password: secret() } };
  await addUser(data, users.admin.name, 'admin', users.admin.password);
  await addUser(data, users.reader.name, 'read', users.reader.password);
  const server = await serve(program('epiphany-server'), ['serve', data, '127.0.0.1:0']);
  const gui = await serve(program('epiphany-gui'), ['--listen', '127.0.0.1:0', '--server', server]);
  const browser = new Browser(join(folder, 'browser'));
  page = await browser.page(gui);
  const journeys = [
    ['Signing in', () => signingIn(users)],
    ['Exploring and writing back', exploring],
    ['Changing the model', changingTheModel],
    ['Administering, and viewing as another user', () => administering(users)],
    ['A large grid', aLargeGrid],
  ];
  for (const [name, run] of journeys) {
    console.log(name);
    await run();
  }
  expect(!page.errors.length, `The page's scripts failed:\n${page.errors.join('\n')}`);
}

let passed = false;
try {
  await main();
  passed = true;
  console.log('Every journey passed.');
} catch (problem) {
  console.log(`\n${problem.stack ?? problem}`);
  for (const [path, text] of said) console.log(`\n${path} said:\n${text.trim().split('\n').slice(-20).join('\n')}`);
  process.exitCode = 1;
} finally {
  for (const child of started) child.kill();
  if (folder && (passed && !KEEP)) {
    await sleep(500);
    rmSync(folder, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } else if (folder) {
    console.log(`The run's folder is kept: ${folder}`);
  }
}
