// The GUI's main journeys, taken in a browser as a person takes them: signing in, and again
// when the session ends; exploring a cube, an MDX query and writing a value back; changing
// the model; administering users and viewing the server as one of them; and scrolling and
// printing a grid of 100,000 cells. Each page is checked as it stands by audit.js, in both
// themes.
//
//   cargo build -p epiphany-server -p epiphany-gui
//   node crates/epiphany-gui/tests/browser/journeys.mjs [--headed] [--keep]
//
// Nothing is installed. Node drives Chrome, or Edge, over the DevTools protocol on a pipe;
// the browser is found where it is usually installed, or named by CHROME. The servers run
// the builds in target/debug, the GUI with the files built into it, on a copy of sample/model
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
    // Chrome on Windows learns whether the user's password is blank by signing in to Windows
    // with an empty one, and keeps the answer in the profile until the password next changes.
    // Each refused sign-in counts toward the account's lockout, and a fresh profile asked at
    // every run: ten runs within minutes locked the account, and Remote Desktop with it
    // (2026-11-22). The profile is given an answer that stands until the end of time, blank,
    // so that Chrome never signs in to Windows, nor asks the user for their password.
    if (process.platform === 'win32') {
      mkdirSync(profile, { recursive: true });
      writeFileSync(join(profile, 'Local State'), JSON.stringify({
        password_manager: { os_password_blank: true, os_password_last_changed: String(2n ** 63n - 1n) },
      }));
    }
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
    await page.call('Log.enable');
    // What moves stands still, so that what is checked is what stays on the page; and
    // numbers are written as in the United States, wherever the run is.
    await page.call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'reduce' }] });
    await page.call('Emulation.setLocaleOverride', { locale: 'en-US' });
    return page;
  }
}

const KEYS = { Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46, Home: 36, End: 35, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, F2: 113 };
const CTRL = 2;
const SHIFT = 8;

/** A tab, and what a person does in it: going to a page, clicking, typing, pressing keys. */
class Page {
  constructor(browser, sessionId, base) {
    this.browser = browser;
    this.sessionId = sessionId;
    this.base = base;
    this.waiters = new Set();
    /**
     * Errors the page's scripts threw or reported, which no journey expects; and what the
     * page's own policy refused it, such as a style attribute, which fails without a word.
     */
    this.errors = [];
    browser.listeners.add(({ method, params, sessionId: from }) => {
      if (from !== sessionId) return;
      if (method === 'Runtime.exceptionThrown') this.errors.push(params.exceptionDetails.exception?.description ?? params.exceptionDetails.text);
      if (method === 'Log.entryAdded' && params.entry.source === 'security' && params.entry.level === 'error') this.errors.push(params.entry.text);
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
    return this.waitFor(`(async () => {
      const element = [...document.querySelectorAll(${quoted(css)})].find((each) => {
        const box = each.getBoundingClientRect();
        const named = ${quoted(name ?? null)};
        return box.width > 0 && box.height > 0 && (named === null || each.getAttribute('aria-label') === named || each.textContent.trim() === named);
      });
      if (!element) return null;
      // Brought into view only if out of it, as a person scrolls to what they cannot see:
      // outside the window, or cut off by what it scrolls in, such as a long dialog's body,
      // where its middle is something else's.
      const seen = element.getBoundingClientRect();
      const there = document.elementFromPoint(seen.left + seen.width / 2, seen.top + seen.height / 2);
      const hidden = !there || !(there === element || element.contains(there));
      if (seen.top < 0 || seen.left < 0 || seen.bottom > innerHeight || seen.right > innerWidth || hidden) {
        element.scrollIntoView({ block: 'center', inline: 'center' });
      }
      // The pointer goes where a frame has drawn it: what appeared since is drawn first.
      await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
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

  /** The pointer pressed at `from`, taken to `to` a little at a time, as a hand moves it, and let go there. */
  async drag(from, to, steps = 8) {
    const mouse = (type, { x, y }, buttons) => this.call('Input.dispatchMouseEvent', { type, x, y, button: 'left', buttons, clickCount: 1 });
    await mouse('mouseMoved', from, 0);
    await mouse('mousePressed', from, 1);
    for (let step = 1; step <= steps; step += 1) {
      await mouse('mouseMoved', { x: from.x + ((to.x - from.x) * step) / steps, y: from.y + ((to.y - from.y) * step) / steps }, 1);
      await sleep(16);
    }
    await mouse('mouseReleased', to, 0);
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

  /** A field clicked into and emptied: its text chosen, and Backspace pressed. */
  async clear(css) {
    await this.click(css);
    await this.evaluate('document.activeElement.select?.()');
    await this.press('Backspace');
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
    // What the next frames draw is on the page first, such as the rows a table has in sight:
    // a reader sees them, and the check is to see what a reader does.
    await this.evaluate('new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))');
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
    const errors = page.errors.length ? `\n    The page's scripts failed:\n${page.errors.join('\n').replace(/^/gm, '      ')}` : '';
    console.log(`  FAILED ${name}\n${String(problem.message).replace(/^/gm, '    ')}${errors}\n    The page as it was: ${shot}`);
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

/** Whether the page, about to be closed, would have the browser ask first. */
const ASKS_TO_CLOSE = `(() => {
  const closing = new Event('beforeunload', { cancelable: true });
  dispatchEvent(closing);
  return closing.defaultPrevented;
})()`;

/**
 * How far the open dialog reaches sideways beyond what it shows, its body scrolled or a rule's
 * field past the rules' box; and how far a part of a set editor in it is drawn over the next.
 */
const OVERDRAWN = `(() => {
  const body = [...document.querySelectorAll('dialog[open] .dialog-body')].pop();
  const fields = [...body.querySelectorAll('.rule > *')].map((field) => field.getBoundingClientRect().right - field.closest('.rules').getBoundingClientRect().right);
  const parts = [...body.querySelectorAll('.set-editor > *')].map((part) => Math.max(...[...part.children].map((each) => each.getBoundingClientRect().bottom)) - part.getBoundingClientRect().bottom);
  return { sideways: Math.round(Math.max(body.scrollWidth - body.clientWidth, ...fields)), over: Math.round(Math.max(0, ...parts)) };
})()`;

/** Two highlight rules, then two of the set editor's filter rules, drawn within their dialogs. */
async function rulesFit(where) {
  await page.click('button', 'Highlight');
  await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Highlights'`, 'the highlights');
  await page.click('dialog[open] button', 'Add a rule');
  await page.click('dialog[open] button', 'Add a rule');
  await page.waitFor(`document.querySelectorAll('dialog[open] .rule').length === 2`, 'two highlight rules');
  const highlights = await page.evaluate(OVERDRAWN);
  expect(highlights.sideways <= 0, `The highlight rules reach ${highlights.sideways} pixels beyond their dialog at ${where}.`);
  await page.click('dialog[open] button', 'Cancel');
  await page.waitFor(`!document.querySelector('dialog[open]')`, 'the highlights, closed');
  await page.click('.zone[data-place="rows"] .chip-main');
  await page.waitFor(`document.querySelector('dialog[open] .set-editor')`, 'the set editor');
  await page.click('dialog[open] button', 'Filter');
  await page.click('dialog[open] button', 'Add a rule');
  await page.waitFor(`document.querySelectorAll('dialog[open] .rule').length === 2`, 'two filter rules');
  const filter = await page.evaluate(OVERDRAWN);
  expect(filter.sideways <= 0, `The set editor's filter rules reach ${filter.sideways} pixels beyond their box at ${where}.`);
  expect(filter.over <= 0, `A part of the set editor is drawn ${filter.over} pixels over the next at ${where}.`);
  await page.click('dialog[open] button', 'Cancel');
  await page.waitFor(`!document.querySelector('dialog[open]')`, 'the set editor, closed');
}

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
    // The password typed can be shown, and hidden again, the keys back in its field.
    const shown = `[document.querySelector('#password').type, document.querySelector('.reveal').getAttribute('aria-pressed'), document.activeElement?.id].join()`;
    await page.click('button', 'Show password');
    expect(await page.evaluate(shown) === 'text,true,password', `Shown, the password is ${await page.evaluate(shown)}.`);
    await page.click('button', 'Show password');
    expect(await page.evaluate(shown) === 'password,false,password', `Hidden again, the password is ${await page.evaluate(shown)}.`);
    // Typed in with Caps Lock on, the field says so beneath it, and no more once it is off.
    const capsLock = (on) => page.evaluate(`document.querySelector('#password').dispatchEvent(new KeyboardEvent('keyup', { key: 'a', modifierCapsLock: ${on} }))`);
    await capsLock(true);
    await page.waitFor(`document.querySelector('.caps')?.textContent === 'Caps Lock is on'`, 'Caps Lock, said');
    await page.audit('Caps Lock, said');
    await capsLock(false);
    await page.waitFor(`document.querySelector('.caps')?.textContent === ''`, 'Caps Lock, said no more');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.error-text:not([hidden])')?.textContent.includes('do not match')`, 'the refusal');
  });
  await step('Signing in opens the overview', async () => {
    await page.fill('#password', admin.password);
    await page.press('Enter');
    await page.waitFor(`document.querySelector('h1')?.textContent === ${quoted(`Welcome, ${admin.name}`)}`, 'the welcome');
    await page.settled();
    // Counted from the model's outline, the elements are as many as the whole model has.
    const { data } = await page.api('GET', '/model');
    const elements = data.model.dimensions.reduce((total, dimension) => total + dimension.elements.length, 0);
    const shown = await page.evaluate(`[...document.querySelectorAll('.tile')].find((tile) => tile.textContent.includes('Elements'))?.querySelector('.stat')?.textContent`);
    expect(shown === elements.toLocaleString('en-US'), `The overview counts ${shown} elements; the model has ${elements}.`);
    await page.audit('The overview');
    // What app.js imports came beside it, by the page's hints; the other pages' files, while
    // the overview stands, before any is asked for.
    const fetched = await page.evaluate(`Object.fromEntries(performance.getEntriesByType('resource')
      .filter((entry) => /\\/(app|api|ui|tree)\\.js$/.test(entry.name))
      .map((entry) => [entry.name.split('/').pop(), [entry.startTime, entry.responseEnd]]))`);
    const beside = ['api.js', 'ui.js', 'tree.js'].every((name) => fetched[name]?.[0] < fetched['app.js']?.[1]);
    expect(beside, `What app.js imports was asked for only once it had come: ${quoted(fetched)}.`);
    // Sent gzipped: the application's own script, and the API's answers, through the GUI.
    const sizes = await page.evaluate(`performance.getEntriesByType('resource')
      .filter((entry) => /\\/(app\\.js|api\\/model\\/outline)$/.test(entry.name))
      .map((entry) => [entry.name.split('/').pop(), entry.encodedBodySize, entry.decodedBodySize])`);
    expect(sizes.length === 2 && sizes.every(([, sent, whole]) => sent > 0 && sent < whole / 2), `Sent, of the whole: ${quoted(sizes)}.`);
    await page.waitFor(`performance.getEntriesByType('resource').some((entry) => entry.name.endsWith('/explore.js'))`, 'the other pages\' files, fetched ahead');
  });
  await step('The first key offers a way past the bar to the page', async () => {
    await page.evaluate('document.activeElement?.blur()');
    await page.press('Tab');
    await page.waitFor(`document.activeElement?.classList.contains('skip') && document.activeElement.getBoundingClientRect().top >= 0`, 'the skip link, shown');
    await page.audit('The skip link');
    await page.press('Enter');
    await page.waitFor(`document.activeElement?.id === 'main' && location.hash === ''`, 'the page, with the keys');
  });
  await step('? lists the keys the application takes', async () => {
    await page.type('?');
    await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Keyboard shortcuts'`, 'the keys');
    await page.audit('The keyboard shortcuts');
    const listed = await page.evaluate(`[...document.querySelectorAll('dialog[open] dd')].map((what) => what.textContent)`);
    expect(listed.includes('Search cubes, pages and commands') && listed.includes('Run the query'), `The keys listed are ${quoted(listed)}.`);
    await page.press('Escape');
    await page.waitFor(`!document.querySelector('dialog[open]')`, 'the keys, closed');
  });
  await step('A dark theme chosen is the page\'s from the first, before the application runs', async () => {
    // The theme as the page's body begins: the head's script has run, the application's not.
    const { identifier } = await page.call('Page.addScriptToEvaluateOnNewDocument', { source: `new MutationObserver((changes, watching) => {
      if (!document.body) return;
      window.themeAtFirst = document.documentElement.dataset.theme ?? null;
      watching.disconnect();
    }).observe(document, { childList: true, subtree: true });` });
    await page.evaluate(`localStorage.setItem('epiphany.theme', 'dark')`);
    await page.goto('/');
    await page.waitFor(`document.querySelector('h1')?.textContent.startsWith('Welcome')`, 'the overview');
    const first = await page.evaluate('window.themeAtFirst');
    await page.call('Page.removeScriptToEvaluateOnNewDocument', { identifier });
    await page.evaluate(`localStorage.removeItem('epiphany.theme'); document.documentElement.dataset.theme = 'light'`);
    expect(first === 'dark', `The page began ${quoted(first)}, not dark.`);
  });
  await step('The bar says at once when the connection goes, and when it comes back', async () => {
    const network = (offline) => page.call('Network.emulateNetworkConditions', { offline, latency: 0, downloadThroughput: -1, uploadThroughput: -1 });
    await page.call('Network.enable');
    await network(true);
    await page.waitFor(`document.querySelector('.status')?.textContent === 'Unreachable'`, 'the status, unreachable');
    await page.audit('No connection');
    await network(false);
    await page.waitFor(`document.querySelector('.status')?.textContent === 'Connected' && [...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('answers again'))`, 'the status, connected again');
    await page.call('Network.disable');
  });
  await step('A session that ends is signed into again over the page, which stays', async () => {
    await page.click('.nav a', 'Administration');
    await page.waitFor(`document.querySelector('h1')?.textContent === 'Administration'`, 'Administration');
    // The move is said, to a screen reader, as loading the page would say its title.
    await page.waitFor(`document.querySelector('p.visually-hidden[aria-live]')?.textContent === 'Administration'`, 'the move, said');
    await page.settled();
    expect((await page.api('DELETE', '/sessions')).status === 204, 'The session would not end.');
    await page.click('button', 'Refresh');
    await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Your session has ended'`, 'the sign-in dialog');
    await page.audit('The sign-in dialog');
    await page.fill('#again-password', admin.password);
    await page.press('Enter');
    await page.waitFor(`!document.querySelector('dialog[open]') && [...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Signed in again'))`, 'signing in again');
    await page.waitFor(`document.querySelector('h1')?.textContent === 'Administration'`, 'the page as it was');
    // What failed while the session was ended is fetched again by itself: the server's state.
    await page.waitFor(`!document.body.textContent.includes('Your session has ended') && document.querySelector('.stat')`, 'the status, fetched again');
  });
  await step('The palette finds a saved view by a word of its name, the best first, and offers again what was chosen', async () => {
    const options = `[...document.querySelectorAll('dialog.palette [role="option"]')].map((option) => ({
      label: option.firstElementChild.nextElementSibling.textContent,
      marked: [...option.querySelectorAll('mark')].map((mark) => mark.textContent).join('|'),
      kind: option.querySelector('.kind').textContent,
    }))`;
    const open = async () => {
      await page.press('k', CTRL);
      await page.waitFor(`document.querySelector('dialog.palette[open]') && !document.querySelector('dialog.palette .finding')`, 'the palette, with the cubes');
    };
    await open();
    await page.type('quart');
    await page.waitFor(`document.querySelector('dialog.palette [role="option"]')?.textContent.startsWith('Quarterly')`, 'the view, first');
    const [first] = await page.evaluate(options);
    expect(first.label === 'Quarterly income by entity' && first.marked === 'Quart' && first.kind === 'View of General Ledger', `The first command offered is ${quoted(first)}.`);
    await page.audit('The palette');
    // A word that begins a word of the name ranks above one found within what it belongs to.
    await page.fill('dialog.palette input', 'ledg');
    await page.waitFor(`document.querySelector('dialog.palette [role="option"]')?.textContent.startsWith('Explore General Ledger')`, 'the cube, first');
    await page.fill('dialog.palette input', 'overview');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('h1')?.textContent.startsWith('Welcome')`, 'the overview');
    // The overview lists the places visited lately, to go back to.
    const recent = await page.evaluate(`[...document.querySelectorAll('.recent a')].map((place) => place.querySelector('.grow').textContent)`);
    expect(recent[0] === 'Administration', `The places visited lately are ${quoted(recent)}.`);
    await open();
    const [lately] = await page.evaluate(options);
    expect(lately.label === 'Overview', `With nothing typed, the first command offered is ${quoted(lately)}, not the one chosen last.`);
    // A view chosen opens in Explore, which names it before the cube.
    await page.type('quart');
    await page.waitFor(`document.querySelector('dialog.palette [role="option"]')?.textContent.startsWith('Quarterly')`, 'the view, first');
    await page.press('Enter');
    await page.waitFor(`document.title === 'Quarterly income by entity · General Ledger · Explore · Epiphany'`, 'the view, named in the title');
  });
  await step('A notice stands while it is pointed at, the same one shows once, and four at most', async () => {
    const notices = `[...document.querySelectorAll('.notice p')].map((p) => p.textContent)`;
    await page.evaluate(`import('/ui.js').then(({ notify }) => {
      for (const n of [1, 2, 3, 4, 5]) notify('Notice ' + n);
      notify('Notice 5');
    })`);
    const shown = await page.evaluate(notices);
    expect(quoted(shown) === quoted(['Notice 2', 'Notice 3', 'Notice 4', 'Notice 5']), `The notices are ${quoted(shown)}.`);
    // Pointed at, the last stands past its time, while the others go; let go, it goes too.
    const middle = await page.evaluate(`(() => {
      const box = [...document.querySelectorAll('.notice')].at(-1).getBoundingClientRect();
      return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    })()`);
    await page.call('Input.dispatchMouseEvent', { type: 'mouseMoved', ...middle });
    // Had it not stood, it would have gone with them, at the same moment.
    await page.waitFor(`${notices}.join() === 'Notice 5'`, 'the others, gone');
    await page.call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 1, y: 1 });
    await page.waitFor(`!document.querySelector('.notice')`, 'the notice, gone once let go');
  });
  await step('A button shown as an icon alone says its name, pointed at', async () => {
    const middle = await page.evaluate(`(() => {
      const box = document.querySelector('#theme-button').getBoundingClientRect();
      return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    })()`);
    await page.call('Input.dispatchMouseEvent', { type: 'mouseMoved', ...middle });
    await page.waitFor(`document.querySelector('#theme-button').title === 'Theme'`, 'the theme button, named');
    await page.call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 1, y: 1 });
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
    const title = await page.evaluate('document.title');
    expect(title === 'General Ledger · Explore · Epiphany', `The browser's title is ${quoted(title)}.`);
    // Explore in the list of pages opens the cube explored last, not the first, and names it.
    await page.click('.nav a', 'Explore');
    await page.waitFor(`location.search === '?cube=General%20Ledger' && document.title.startsWith('General Ledger') && document.querySelector('td[data-r]')`, 'the cube explored last');
    // The fields found by a word of an element's name, with the elements that have it.
    await page.fill('input[placeholder="Find a dimension or element"]', 'rent');
    await page.waitFor(`[...document.querySelectorAll('.field-list .field')].map((field) => field.textContent).join('|').includes('Rent') && document.querySelectorAll('.field-list .field').length === 1`, 'Account, with Rent');
    await page.clear('input[placeholder="Find a dimension or element"]');
    await page.waitFor(`document.querySelectorAll('.field-list .field').length === 4`, 'every field again');
  });
  await step('A query typed in that cannot be read is shown where, to be mended there', async () => {
    const broken = 'SELECT {[Period].[Jan] ON COLUMNS FROM [General Ledger]';
    const { data } = await page.api('POST', '/mdx', { mdx: broken });
    expect(Number.isInteger(data.position), `The API says no place for the problem: ${quoted(data)}.`);
    await page.click('[role="tab"]', 'MDX');
    await page.fill('textarea.mdx-editor', broken);
    await page.click('button', 'Run');
    await page.click('button', 'Show it in the query');
    await page.waitFor(`document.activeElement?.matches('textarea.mdx-editor') && document.activeElement.selectionStart === ${data.position}`, 'the problem, chosen in the query');
    const said = await page.evaluate(`document.getElementById(document.querySelector('textarea.mdx-editor').getAttribute('aria-describedby'))?.textContent`);
    // The GUI says what the API says as a sentence.
    expect(said?.toLowerCase().replace(/\.$/, '') === data.message.toLowerCase(), `Beneath the query it says ${quoted(said)}, not ${quoted(data.message)}.`);
    await page.audit('A query mended where it went wrong');
    // A page given no clipboard, as one not served securely is, still copies, the older way.
    await page.evaluate(`Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true })`);
    await page.click('button', 'Copy');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent === 'Copied.')`, 'the query, copied the older way');
    await page.evaluate(`delete navigator.clipboard`);
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
    expect(await page.evaluate(ASKS_TO_CLOSE), 'The browser would close the page, its change unsaved, without asking.');
    expect(await page.evaluate('document.title') === '• General Ledger · Explore · Epiphany', 'The browser\'s title does not mark the change unsaved.');
    await page.click('.pending button', 'Save changes');
    await page.waitFor(`document.querySelector('.pending')?.hidden && document.title === 'General Ledger · Explore · Epiphany'`, 'the change, saved');
    expect(!(await page.evaluate(ASKS_TO_CLOSE)), 'The browser would ask before closing the page, with nothing unsaved.');
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
  await step('A view kept for oneself is listed apart, and taken away', async () => {
    const views = '/cubes/General%20Ledger/private-views';
    await page.click('button', 'Views');
    await page.click('.menu [role="menuitem"]', 'Save this query as a view…');
    await page.waitFor(`document.querySelector('dialog[open] #view-name')`, 'the dialog that saves a view');
    await page.audit('Saving a view');
    // A name of spaces is refused at its field: marked, described by the refusal beneath it, and
    // given the keys; changed, it is marked no more.
    await page.fill('#view-name', '   ');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('#view-name')?.getAttribute('aria-invalid') === 'true'`, 'the name, refused');
    const refusal = await page.evaluate(`(() => {
      const field = document.querySelector('#view-name');
      const said = document.getElementById(field.getAttribute('aria-describedby'));
      return { said: said?.textContent, beneath: Boolean(said && field.closest('.field').contains(said)), keys: document.activeElement === field };
    })()`);
    expect(refusal.said === 'Give the view a name.' && refusal.beneath && refusal.keys, `The refusal is ${quoted(refusal)}.`);
    await page.audit('A view\'s name, refused');
    await page.fill('#view-name', 'Budget detail');
    expect(await page.evaluate(`!document.querySelector('#view-name').hasAttribute('aria-invalid')`), 'The name stays marked once changed.');
    await page.press('Enter');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('for you alone'))`, 'the view, saved');
    const kept = (await page.api('GET', views)).data;
    // The query as typed, laid out over lines as Explore lays it out.
    const asked = (mdx) => mdx?.replace(/\s+/g, ' ').trim();
    expect(asked(kept['Budget detail']?.mdx) === asked(QUERY), `The API keeps ${quoted(kept)}.`);
    await page.click('button', 'Views');
    await page.waitFor(`[...document.querySelectorAll('.menu .menu-label')].some((label) => label.textContent === 'Your views')`, 'the user\'s own views, apart');
    await page.audit('The views, the user\'s own apart');
    await page.click('.menu [role="menuitem"]', 'Take one of your views away…');
    await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Your views of General Ledger'`, 'the user\'s views, to take away');
    await page.audit('Taking a view away');
    await page.click('dialog[open] button', 'Take away the view Budget detail');
    await page.waitFor(`document.querySelector('dialog[open]')?.textContent.includes('You keep no views of this cube now.')`, 'the view, taken away');
    await page.click('dialog[open] button', 'Done');
    expect(quoted((await page.api('GET', views)).data) === '{}', 'The view is still kept.');
  });
  await step('A set kept as one\'s own subset is offered again, and taken away', async () => {
    const subsets = '/dimensions/Account/private-subsets';
    await page.click('.chip-main', 'Account2 members');
    await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Members of Account'`, 'the set editor');
    await page.click('dialog[open] button', 'Keep');
    await page.click('.menu [role="menuitem"]', 'Keep this set as one of your subsets…');
    await page.waitFor(`document.querySelector('dialog[open] #subset-name')`, 'the dialog that keeps a subset');
    await page.audit('Keeping a subset');
    await page.fill('#subset-name', 'Costs');
    await page.press('Enter');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Kept the subset'))`, 'the subset, kept');
    const kept = (await page.api('GET', subsets)).data;
    expect(quoted(kept) === quoted({ Costs: ['Salaries', 'Rent'] }), `The API keeps ${quoted(kept)}.`);
    await page.waitFor(`[...document.querySelectorAll('dialog[open] option')].some((option) => option.textContent === 'Your subset Costs')`, 'the subset, offered to replace the set');
    await page.audit('The set editor, with a subset of one\'s own');
    await page.click('dialog[open] button', 'Keep');
    await page.click('.menu [role="menuitem"]', 'Take one of your subsets away…');
    await page.waitFor(`[...document.querySelectorAll('dialog[open]')].pop()?.getAttribute('aria-label') === 'Your subsets of Account'`, 'the user\'s subsets, to take away');
    await page.audit('Taking a subset away');
    await page.click('dialog[open] button', 'Take away the subset Costs');
    await page.waitFor(`document.body.textContent.includes('You keep no subsets of Account now.')`, 'the subset, taken away');
    await page.click('dialog[open] button', 'Done');
    expect(quoted((await page.api('GET', subsets)).data) === '{}', 'The subset is still kept.');
    await page.click('dialog[open] button', 'Cancel');
    await page.waitFor(`!document.querySelector('dialog[open]')`, 'the set editor, closed');
  });
  await step('A subset that reads cells gives the members they say now', async () => {
    // The cost largest in the US's budget for the year, worked out from the cube.
    const { data } = await page.api('GET', '/model');
    const account = data.model.dimensions.find((dimension) => dimension.name === 'Account');
    account.subsets = { 'Biggest cost': { mdx: 'TopCount([Account].[Operating Expenses].Children, 1, LookupCube(\'General Ledger\', [Entity].[US], [Version].[Budget]))' } };
    const put = await page.api('PUT', '/dimensions/Account', { based_on: data.version, dimension: account });
    expect(put.status === 200, `The subset was not added: ${quoted(put.data)}`);
    await page.goto('/explore?cube=General%20Ledger');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
    const choose = async () => {
      await page.click('.chip-main', 'Account2 members');
      await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === 'Members of Account'`, 'the set editor');
      await page.evaluate(`(() => {
        const replace = document.querySelector('dialog[open] select[aria-label="Replace the set"]');
        replace.value = 'subset:Biggest cost';
        replace.dispatchEvent(new Event('change'));
      })()`);
    };
    const set = `[...document.querySelectorAll('dialog[open] .set-item .set-name')].map((name) => name.textContent).join()`;
    await choose();
    await page.waitFor(`${set} === 'Salaries'`, 'the largest cost, Salaries');
    await page.click('dialog[open] button', 'Cancel');
    // Rent made the larger, the subset says so as it is chosen again.
    const written = await page.api('POST', '/cells/write', { cells: [{ cube: 'General Ledger', at: at('Rent', 'Mar'), value: 5000 }] });
    expect(written.status < 300, 'Rent was not written.');
    await choose();
    await page.waitFor(`${set} === 'Rent'`, 'the largest cost, now Rent');
    await page.click('dialog[open] button', 'Cancel');
    await page.waitFor(`!document.querySelector('dialog[open]')`, 'the set editor, closed');
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
    // The change, saved moments ago, is said to be from now, the moment itself kept beside.
    const moment = await page.evaluate(`(() => {
      const time = document.querySelector('.comment-panel time');
      return { said: time?.querySelector('.ago')?.textContent, since: (Date.now() - new Date(time?.dateTime)) / 1000 };
    })()`);
    expect(/^(now|\d+ minutes? ago)$/.test(moment.said) && moment.since >= 0 && moment.since < 600, `The change is said to be from ${quoted(moment)}.`);
    // The note of a change not yet saved, hidden with nothing to say, is not seen standing empty.
    const note = await page.evaluate(`(() => { const note = document.querySelector('.comment-panel .history-pending'); return { hidden: note.hidden, shown: getComputedStyle(note).display !== 'none' }; })()`);
    expect(!(note.hidden && note.shown), 'The history\'s note of what is not yet saved stands empty over it.');
    await page.audit('A cell\'s history');
    await page.click('[role="tab"]', 'Detail');
    await page.waitFor(`document.querySelector('.comment-panel .detail-lines')`, 'the detail');
    await page.settled();
    await page.audit('A cell\'s detail');
    // Escape, from the panel's tabs, closes it, and the keys go back to the cell.
    await page.click('[role="tab"]', 'History');
    await page.waitFor(`document.activeElement?.getAttribute('role') === 'tab'`, 'the keys on the tabs');
    await page.press('Escape');
    await page.waitFor(`document.querySelector('.comment-panel').hidden && document.activeElement?.matches('td[data-r="1"][data-c="1"]')`, 'the panel, closed, and the keys on the cell');
  });
  await step('A calculated member\'s format is what its cells show', async () => {
    // Rent as a share of salaries in the budget: -200 of 1,000 in January, 250 of 1,100.5 in February.
    const mdx = "WITH MEMBER [Account].[Share] AS [Account].[Rent] / [Account].[Salaries], FORMAT_STRING = '0.0%' SELECT {[Period].[Jan], [Period].[Feb]} ON COLUMNS, {[Account].[Salaries], [Account].[Share]} ON ROWS FROM [General Ledger] WHERE ([Entity].[US], [Version].[Budget])";
    await page.click('[role="tab"]', 'MDX');
    await page.fill('textarea.mdx-editor', mdx);
    await page.click('button', 'Run');
    await page.waitFor(`[...document.querySelectorAll('th.row-head[data-r1]')].map((th) => th.textContent.trim()).join() === 'Salaries,Share'`, 'the rows the query asks for');
    await page.settled();
    const shown = await page.evaluate(GRID);
    const { data } = await page.api('POST', '/mdx', { mdx });
    expect(quoted(data.formatted) === quoted([null, null, '-20.0%', '22.7%']), `The API writes the cells out as ${quoted(data.formatted)}.`);
    const shares = [shown.cells['1,0'], shown.cells['1,1']];
    expect(quoted(shares) === quoted(['-20.0%', '22.7%']), `The grid shows the shares as ${quoted(shares)}.`);
    expect(shownNumber(shown.cells['0,1']) === 1100.5, `The grid shows February's salaries as ${quoted(shown.cells['0,1'])}.`);
    await page.audit('Cells a calculated member formats');
  });
  await step('A percentage typed into a total changes every leaf beneath it', async () => {
    // January's budgeted operating expenses: salaries of 1,000 and rent of -200, 800 in all.
    const mdx = 'SELECT {[Period].[Jan], [Period].[Feb]} ON COLUMNS, {[Account].[Operating Expenses]} ON ROWS FROM [General Ledger] WHERE ([Entity].[US], [Version].[Budget])';
    await page.click('[role="tab"]', 'MDX');
    await page.fill('textarea.mdx-editor', mdx);
    await page.click('button', 'Run');
    await page.waitFor(`[...document.querySelectorAll('th.row-head[data-r1]')].map((th) => th.textContent.trim()).join() === 'Operating Expenses'`, 'the total the query asks for');
    await page.click('td[data-r="0"][data-c="0"]');
    await page.waitFor(`document.querySelector('td[data-r="0"][data-c="0"]').classList.contains('spreadable')`, 'the total to take a spread');
    await page.type('P');
    await page.waitFor(`document.querySelector('.cell-editor')`, 'the cell\'s editor');
    await page.type('%10');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.pending:not([hidden])')?.textContent.includes('unsaved change')`, 'the spread, waiting');
    await page.click('.pending button', 'Save changes');
    await page.waitFor(`document.querySelector('.pending')?.hidden`, 'the spread, saved');
    const cells = ['Salaries', 'Rent'].map((account) => ({ cube: 'General Ledger', at: at(account, 'Jan') }));
    const [salaries, rent] = (await page.api('POST', '/cells/read', { cells })).data.cells.map((cell) => cell.value);
    expect(Math.abs(salaries - 1100) < 1e-6 && Math.abs(rent + 220) < 1e-6, `The API reads salaries of ${quoted(salaries)} and rent of ${quoted(rent)} where a tenth more, 1,100 and -220, was spread.`);
  });
  await step('A growth typed into a cell runs along its row', async () => {
    const mdx = 'SELECT {[Period].[Jan], [Period].[Feb], [Period].[Mar]} ON COLUMNS, {[Account].[Salaries]} ON ROWS FROM [General Ledger] WHERE ([Entity].[US], [Version].[Budget])';
    await page.click('[role="tab"]', 'MDX');
    await page.fill('textarea.mdx-editor', mdx);
    await page.click('button', 'Run');
    await page.waitFor(`document.querySelectorAll('th.col-head[data-c1]').length === 3 && [...document.querySelectorAll('th.row-head[data-r1]')].map((th) => th.textContent.trim()).join() === 'Salaries'`, 'the months the query asks for');
    await page.click('td[data-r="0"][data-c="0"]');
    await page.waitFor(`document.querySelector('td[data-r="0"][data-c="0"]').classList.contains('editable')`, 'the cell to be writable');
    await page.type('G');
    await page.waitFor(`document.querySelector('.cell-editor')`, 'the cell\'s editor');
    await page.type('R>100:10');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.pending:not([hidden])')?.textContent.includes('unsaved change')`, 'the growth, waiting');
    await page.click('.pending button', 'Save changes');
    await page.waitFor(`document.querySelector('.pending')?.hidden`, 'the growth, saved');
    const cells = ['Jan', 'Feb', 'Mar'].map((period) => ({ cube: 'General Ledger', at: at('Salaries', period) }));
    const months = (await page.api('POST', '/cells/read', { cells })).data.cells.map((cell) => cell.value);
    const grown = [100, 110, 121].every((expected, index) => Math.abs(months[index] - expected) < 1e-6);
    expect(grown, `The API reads ${quoted(months)} where 100 grown by a tenth a month, 100, 110 and 121, was typed.`);
  });
  await step('A member shown twice opens where it is opened alone, each row standing in as far as it stands beneath', async () => {
    // Revenue chosen, and Net Income open, beneath which Revenue stands again; the layout as
    // one kept before kept it, its open member by name.
    const key = quoted('epiphany.explore.General Ledger');
    const kept = await page.evaluate(`localStorage.getItem(${key})`);
    const layout = { cube: 'General Ledger', rows: [{ dimension: 'Account', members: ['Revenue', 'Operating Expenses', 'Net Income'], expanded: ['Net Income'] }], columns: [{ dimension: 'Period', members: ['Q1'], expanded: [] }], filters: [{ dimension: 'Entity', member: 'Total Entity' }, { dimension: 'Version', member: 'Budget' }], nonEmpty: { rows: false, columns: false }, sort: null, mdx: null };
    await page.evaluate(`localStorage.setItem(${key}, ${quoted(JSON.stringify(layout))})`);
    await page.goto('/explore?cube=General%20Ledger');
    // Each row's member, how many steps in it stands, and whether it is open.
    const rows = `[...document.querySelectorAll('th.row-head[data-member]')].map((th) => [th.dataset.member, th.style.getPropertyValue('--depth') || 0, th.querySelector('button.twisty')?.getAttribute('aria-expanded') ?? ''].join(':')).join('|')`;
    await page.waitFor(`document.querySelectorAll('th.row-head[data-member]').length === 5`, 'the rows, Net Income open');
    const before = await page.evaluate(rows);
    expect(before === 'Revenue:0:false|Operating Expenses:0:false|Net Income:0:true|Revenue:1:false|Operating Expenses:1:false', `The rows stand as ${before}.`);
    await page.click('th.row-head[data-path*="Net Income"][data-path$="Revenue"] button.twisty');
    await page.waitFor(`document.querySelectorAll('th.row-head[data-member]').length === 7`, 'the Revenue beneath Net Income, open');
    const after = await page.evaluate(rows);
    expect(after === 'Revenue:0:false|Operating Expenses:0:false|Net Income:0:true|Revenue:1:true|Product Revenue:2:|Service Revenue:2:|Operating Expenses:1:false', `Opened beneath Net Income, the rows stand as ${after}.`);
    // A line runs down from each member opened through the rows beneath it.
    const guided = await page.evaluate(`getComputedStyle(document.querySelector('th.row-head[data-member="Product Revenue"]'), '::before').backgroundImage.includes('repeating-linear-gradient')`);
    expect(guided, 'No line runs down beside the rows beneath an opened member.');
    // The layout the journeys had, back for the steps after.
    await page.evaluate(kept === null ? `localStorage.removeItem(${key})` : `localStorage.setItem(${key}, ${quoted(kept)})`);
    await page.goto('/explore?cube=General%20Ledger');
  });
  await step('A link to the query shown opens it as it is shown, and a damaged one says so', async () => {
    await page.click('button', 'Views');
    await page.waitFor(`[...document.querySelectorAll('.menu [role="menuitem"]')].some((item) => item.textContent === 'Copy a link to this query')`, 'the link, offered');
    await page.press('Escape');
    // The link as the menu makes it, for the query in thousands to one decimal place.
    const link = new URL(await page.evaluate(`import('/layout.js').then(({ linkOf }) => linkOf('General Ledger', { mdx: ${quoted(QUERY)}, format: { decimals: 1, scale: 'thousands' } }))`));
    await page.goto(link.pathname + link.search);
    await page.waitFor(`[...document.querySelectorAll('th.row-head[data-r1]')].map((th) => th.textContent.trim()).join() === 'Salaries,Rent'`, 'the rows the link asks for');
    // Shown, the view is the cube's last layout, and the address the cube's again.
    const address = await page.evaluate('location.search');
    expect(address === '?cube=General%20Ledger', `The address is ${quoted(address)} once the link is shown.`);
    const { data } = await page.api('POST', '/cells/read', { cells: [{ cube: 'General Ledger', at: at('Salaries', 'Jan') }] });
    const expected = ((data.cells[0].value ?? 0) / 1000).toLocaleString('en-US', { minimumFractionDigits: 1, maximumFractionDigits: 1 });
    const shown = (await page.evaluate(GRID)).cells['0,0'];
    expect(shown === expected, `Salaries in January shows ${quoted(shown)}, not ${quoted(expected)}, in thousands to one place.`);
    await page.goto('/explore?cube=General%20Ledger&shared=not-a-link');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('This link is damaged'))`, 'the damaged link, said');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid, as it was last');
  });
  await step('A dashboard shows its tiles, says when they were run, and runs them again', async () => {
    const view = 'Quarterly income by entity';
    const tiles = [
      { cube: 'General Ledger', view, show: 'figure' },
      { cube: 'General Ledger', view, show: 'bars' },
      { cube: 'General Ledger', view, show: 'grid', width: 2 },
    ];
    const based = (await page.api('GET', '/model')).data.version;
    const put = await page.api('PUT', '/dashboards/Quarter', { based_on: based, dashboard: { name: 'Quarter', description: 'Income by entity, by quarter', tiles } });
    expect(put.status === 200, `The dashboard was not kept: ${quoted(put.data)}`);
    await page.goto('/dashboards?name=Quarter');
    await page.waitFor(`document.querySelectorAll('.dash-tile').length === 3 && !document.querySelector('.dash-tile .skeleton')`, 'the tiles, run');
    await page.settled();
    await page.audit('A dashboard');
    const said = `document.querySelector('.page-head time .ago')?.textContent`;
    expect(await page.evaluate(said) === 'now', `The dashboard says its tiles were run ${quoted(await page.evaluate(said))}.`);
    await page.click('button', 'Refresh');
    await page.waitFor(`!document.querySelector('.page-head [aria-busy="true"]') && ${said} === 'now'`, 'the tiles, run again');
    // The bars' legend, kept to one line, is gone along by its arrows to its last series.
    const bars = '.dash-tile[data-index="1"]';
    const arrow = (name) => `document.querySelector('${bars} .legend-along[aria-label="${name}"]')`;
    expect(await page.evaluate(`${arrow('Earlier series')}.hidden && !${arrow('Later series')}.hidden`), 'The bars\' legend offers no way along it.');
    for (let along = 0; await page.evaluate(`!${arrow('Later series')}.hidden`); along += 1) {
      expect(along < 20, 'The bars\' legend never comes to its end.');
      await page.click(`${bars} .legend-along`, 'Later series');
      await page.evaluate('new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))');
    }
    const last = `(() => {
      const legend = document.querySelector('${bars} .chart-legend').getBoundingClientRect();
      const item = document.querySelector('${bars} .chart-legend').lastElementChild.getBoundingClientRect();
      return item.left >= legend.left - 1 && item.right <= legend.right + 1;
    })()`;
    expect(await page.evaluate(last), 'The bars\' last series is not in sight at the end of the legend.');
    expect(await page.evaluate(`!${arrow('Earlier series')}.hidden`), 'The bars\' legend offers no way back along it.');
  });
  await step('On a dashboard\'s grid tiles change places as one passes them, rise into what is free, and make room for one made larger', async () => {
    const engine = await page.evaluate(`(async () => {
      const { firstFree, moveOnGrid, ontoGrid, overlaps, resizeOnGrid } = await import('/arrange.js');
      const make = (...rows) => rows.map(([x, y, w, h]) => ({ x, y, w, h }));
      const clear = (places) => places.every((a, i) => places.every((b, j) => i === j || !overlaps(a, b)));
      const moved = (places, index, to) => moveOnGrid(places, places[index], to);
      const placed = [];
      for (const [w, h] of [[6, 4], [6, 8], [12, 9], [6, 4]]) placed.push(firstFree(placed, { w, h }));
      const grown = make([0, 0, 6, 4], [6, 0, 6, 4], [0, 4, 12, 4]);
      resizeOnGrid(grown, grown[0], { w: 8, h: 6 });
      const loose = ontoGrid(make([0.4, 0.2, 5.6, 7.7], [5.1, 3.4, 6.9, 8.2], [2.2, 1, 4, 4]));
      return {
        partway: moved(make([0, 0, 6, 8], [6, 0, 6, 8]), 0, { x: 3, y: 0 }).map((p) => p.x).join(),
        passed: moved(make([0, 0, 6, 8], [6, 0, 6, 8]), 0, { x: 6, y: 0 }).map((p) => p.x).join(),
        along: moved(make([0, 0, 4, 8], [4, 0, 4, 8], [8, 0, 4, 8]), 0, { x: 8, y: 0 }).map((p) => p.x).join(),
        taller: moved(make([0, 0, 12, 4], [0, 4, 12, 8]), 1, { x: 0, y: 0 }).map((p) => p.y).join(),
        placed: placed.map((p) => p.x + ':' + p.y).join(),
        grown: clear(grown) && grown[1].y === 6,
        loose: clear(loose) && loose.every((p) => [p.x, p.y, p.w, p.h].every(Number.isInteger)),
      };
    })()`);
    expect(engine.partway === '0,6', `Dragged part of the way to its neighbour, a tile leaves the row at ${engine.partway}.`);
    expect(engine.passed === '6,0', `Dragged past its neighbour's middle, a tile leaves the row at ${engine.passed}.`);
    expect(engine.along === '8,0,4', `Dragged to the end of its row, a tile leaves the row at ${engine.along}.`);
    expect(engine.taller === '8,0', `A tall tile dragged above a short one leaves them at rows ${engine.taller}.`);
    expect(engine.placed === '0:0,6:0,0:8,0:4', `Tiles given the first free place in turn stand at ${engine.placed}.`);
    expect(engine.grown, 'A tile made larger came onto another.');
    expect(engine.loose, 'Tiles brought onto the grid are not on whole columns and rows, clear of one another.');
  });
  await step('A dashboard is arranged by dragging: on its grid a tile takes another\'s place or is made larger by its corner; freely it stands where it is let go, lined up with an edge it came near', async () => {
    const places = `[...document.querySelectorAll('.dash-tile')].map((tile) => ['x', 'y', 'w', 'h'].map((key) => Number(tile.style.getPropertyValue('--' + key))))`;
    await page.click('button', 'Edit');
    await page.waitFor(`document.querySelector('.board.editing .dash-tile')`, 'the dashboard, being edited');
    await page.settled();
    await page.audit('A dashboard, being edited');
    const unit = await page.evaluate(`({ across: (document.querySelector('.board').clientWidth + 16) / 12, down: 56 })`);
    const before = await page.evaluate(places);
    // The key figure, dragged across to the bars, takes their place, and they take its.
    const figure = await page.point('.dash-tile[data-index="0"] h3');
    await page.drag(figure, { x: figure.x + 6 * unit.across, y: figure.y });
    const moved = await page.evaluate(places);
    expect(moved[0][0] === before[1][0] && moved[1][0] === before[0][0], `Dragged across, the figure and the bars stand at ${quoted(moved)}, from ${quoted(before)}.`);
    expect(await page.evaluate(`document.querySelector('.board-ghost').hidden`), 'Where the tile would go is still shown after it was let go.');
    // Two rows taller by its corner, the bars push the grid beneath them down.
    const corner = await page.point('.dash-tile[data-index="1"] .handle.se');
    await page.drag(corner, { x: corner.x, y: corner.y + 2 * unit.down });
    const resized = await page.evaluate(places);
    expect(resized[1][3] === moved[1][3] + 2, `Its corner dragged down two rows, the bars are ${resized[1][3]} rows high, from ${moved[1][3]}.`);
    expect(resized[2][1] >= resized[1][1] + resized[1][3], `The grid stands at ${quoted(resized[2])}, on the bars at ${quoted(resized[1])}.`);
    // Shift and the right arrow make the bars a column wider, and Ctrl Z takes it back.
    await page.evaluate(`document.querySelector('.dash-tile[data-index="1"]').focus()`);
    await page.press('ArrowRight', SHIFT);
    expect((await page.evaluate(places))[1][2] === resized[1][2] + 1, 'Shift and the right arrow did not make the bars a column wider.');
    await page.press('z', CTRL);
    expect((await page.evaluate(places))[1][2] === resized[1][2], 'Ctrl Z did not take the widening back.');
    // Laid out freely, snapping off, the bars let go half a column across and a little down
    // stand there across, and on the line they came near down.
    await page.click('[role="radio"]', 'Freeform');
    await page.waitFor(`document.querySelector('.board.free')`, 'the tiles, laid out freely');
    if (await page.evaluate(`document.querySelector('.page-head .toolbar button[aria-pressed="true"]')?.textContent.trim() === 'Snap to grid'`)) {
      await page.click('button', 'Snap to grid');
      await page.waitFor(`document.querySelector('.board.free:not(.gridded)')`, 'snapping, off');
    }
    const loose = (await page.evaluate(places))[1];
    const bars = await page.point('.dash-tile[data-index="1"] h3');
    await page.drag(bars, { x: bars.x + unit.across / 2, y: bars.y + 5 });
    const freed = (await page.evaluate(places))[1];
    expect(Math.abs(freed[0] - loose[0] - 0.5) < 0.08 && Math.abs(freed[1] - loose[1]) < 1e-6, `Let go half a column across and five pixels down, the bars stand at ${quoted(freed)}, from ${quoted(loose)}.`);
    // A tile of text, dragged from the list onto the board, is added where it is let go.
    await page.click('button', 'Add');
    await page.waitFor(`document.querySelector('.dash-drawer .drawer-item')`, 'what may be added');
    await page.audit('Adding to a dashboard');
    const text = await page.point('.drawer-item');
    const board = await page.evaluate(`(() => { const box = document.querySelector('.board').getBoundingClientRect(); return { x: box.left + box.width / 4, y: box.top + 60 }; })()`);
    await page.drag(text, board, 12);
    await page.waitFor(`document.querySelectorAll('.dash-tile').length === 4`, 'the tile of text, added');
    await page.click('button', 'Save');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Saved Quarter'))`, 'the dashboard, saved');
    const kept = (await page.api('GET', '/model')).data.model.dashboards.find((each) => each.name === 'Quarter');
    expect(kept.layout === 'free' && kept.tiles.length === 4 && kept.tiles.every((tile) => tile.place) && kept.tiles[3].show === 'text', `The dashboard was kept as ${quoted(kept)}.`);
  });
  await step('Clicking a bar of one tile shows every other at its members, until Escape', async () => {
    // The view is of actuals, which nothing has written: revenue booked in February gives bars.
    const written = await page.api('POST', '/cells/write', { cells: [{ cube: 'General Ledger', at: ['Product Revenue', 'UK', 'Feb', 'Actual'], value: 4200 }] });
    expect(written.status < 300, 'The actual revenue was not written.');
    await page.goto('/dashboards?name=Quarter');
    await page.waitFor(`document.querySelector('.dash-tile .mark[data-s]') && !document.querySelector('.dash-tile .skeleton')`, 'the tiles, run');
    await page.click('.dash-tile .mark[data-s]');
    await page.waitFor(`document.querySelector('.dash-filter.picked')`, 'what was picked, said above the tiles');
    await page.waitFor(`document.querySelectorAll('.tile-standing').length >= 2 && !document.querySelector('.dash-tile [aria-busy="true"]')`, 'the other tiles, at the members picked');
    await page.audit('A dashboard, a bar picked');
    await page.press('Escape');
    await page.waitFor(`!document.querySelector('.dash-filter.picked') && !document.querySelector('.tile-standing')`, 'every tile as before');
  });
  await step('A tile is shown over the whole page, and Escape brings the dashboard back', async () => {
    await page.click('.dash-tile[data-index="1"] .tile-head .button');
    await page.waitFor(`document.querySelector('.dash-tile.focused') && document.querySelector('.focus-backdrop')`, 'the tile, over the whole page');
    const covers = await page.evaluate(`(() => { const box = document.querySelector('.dash-tile.focused').getBoundingClientRect(); return box.width > innerWidth * 0.8 && box.height > innerHeight * 0.7; })()`);
    expect(covers, 'The tile shown over the whole page is not nearly as large as it.');
    await page.press('Escape');
    await page.waitFor(`!document.querySelector('.dash-tile.focused')`, 'the dashboard, back');
  });
  await step('A tile\'s menu shows its numbers in place of its chart, copies them, and shows the chart again', async () => {
    const more = '.dash-tile[data-index="1"] .tile-head [aria-haspopup="menu"]';
    await page.click(more);
    await page.click('.menu [role="menuitem"]', 'Show its numbers');
    await page.waitFor(`document.querySelector('.dash-tile[data-index="1"] .tile-table [role="cell"]')`, 'the bars, as their numbers');
    await page.click(more);
    await page.click('.menu [role="menuitem"]', 'Copy its numbers');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent === 'Copied.')`, 'the numbers, copied');
    await page.click(more);
    await page.click('.menu [role="menuitem"]', 'Show the bars again');
    await page.waitFor(`document.querySelector('.dash-tile[data-index="1"] .chart') && !document.querySelector('.dash-tile[data-index="1"] .tile-table')`, 'the bars again');
  });
  await step('A series of a chart is drawn as a line against a scale of its own at its right, and kept so with the dashboard', async () => {
    const bars = '.dash-tile[data-index="1"]';
    const apart = `document.querySelector('${bars} text.tick.right') && document.querySelector('${bars} .legend-item.second')?.textContent === 'UK · Revenue'`;
    await page.click('button', 'Edit');
    await page.waitFor(`document.querySelector('.board.editing ${bars} .chart')`, 'the dashboard, being edited');
    // Laid out freely, the tile of text stands over the bars' head: their options are opened
    // from the keys, Enter on the tile.
    await page.evaluate(`document.querySelector('${bars}').focus()`);
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.menu [role="menuitemcheckbox"]')`, 'the bars\' options');
    const offered = await page.evaluate(`[...document.querySelectorAll('.menu [role="menuitemcheckbox"]')].map((item) => item.textContent.trim())`);
    expect(offered.includes('UK · Revenue'), `The bars offer ${quoted(offered)} to draw against a scale at the right.`);
    await page.click('.menu [role="menuitemcheckbox"]', 'UK · Revenue');
    await page.waitFor(apart, 'UK revenue, against a scale at the right');
    await page.audit('A chart with a scale at its right');
    await page.click('button', 'Save');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Saved Quarter'))`, 'the dashboard, saved');
    const kept = (await page.api('GET', '/model')).data.model.dashboards.find((each) => each.name === 'Quarter');
    expect(quoted(kept.tiles[1].second_axis) === quoted(['UK · Revenue']), `The bars were kept as ${quoted(kept.tiles[1])}.`);
    await page.goto('/dashboards?name=Quarter');
    await page.waitFor(apart, 'UK revenue, at the right still');
  });
  await step('A key figure takes the colour of the first of its view\'s rules its value meets', async () => {
    // The view coloured green at nought or more, as a target met.
    const { data } = await page.api('GET', '/model');
    const cube = data.model.cubes.find((each) => each.name === 'General Ledger');
    const view = cube.views['Quarterly income by entity'];
    cube.views['Quarterly income by entity'] = { ...view, highlights: { rules: [{ test: 'at_least', value: 0, tone: 'good' }] } };
    const put = await page.api('PUT', '/cubes/General%20Ledger', { based_on: data.version, cube });
    expect(put.status === 200, `The view's rule was not kept: ${quoted(put.data)}`);
    await page.goto('/dashboards?name=Quarter');
    await page.waitFor(`document.querySelector('.figure-value.tone-good')`, 'the key figure, in the rule\'s colour');
  });
  await step('A dashboard is presented over the whole screen, the arrows going on to the next, and Escape ends it', async () => {
    const based = (await page.api('GET', '/model')).data.version;
    const notes = { name: 'Notes', tiles: [{ show: 'text', text: '# Notes\nWhat the quarter showed.' }, { cube: 'General Ledger', view: 'Quarterly income by entity', show: 'heatmap' }] };
    const put = await page.api('PUT', '/dashboards/Notes', { based_on: based, dashboard: notes });
    expect(put.status === 200, `A second dashboard was not kept: ${quoted(put.data)}`);
    await page.goto('/dashboards?name=Quarter');
    await page.click('button', 'Present');
    await page.waitFor(`document.querySelector('.dashboard.presenting') && getComputedStyle(document.querySelector('.nav')).display === 'none'`, 'the dashboard, presented alone');
    // Its grid's cells are judged in the rule's colour, so their rows are waited for.
    await page.waitFor(`document.querySelector('.dashboard.presenting .tile-table .tile-row:not(.head) .tone-good')`, 'the grid, in the rule\'s colour');
    await page.audit('A dashboard, presented');
    await page.press('ArrowRight');
    await page.waitFor(`location.search === '?name=Notes' && document.querySelector('.dashboard.presenting .text-tile') && document.querySelector('.tile-table.heat .shade')`, 'the next dashboard, presented, its heatmap shaded');
    await page.audit('A heatmap, presented');
    await page.press('Escape');
    await page.waitFor(`!document.querySelector('.dashboard.presenting') && getComputedStyle(document.querySelector('.nav')).display !== 'none'`, 'the dashboard, no longer presented');
  });
  await step('A dashboard is copied from New, opened to be arranged, saved, and taken away', async () => {
    const tiles = (await page.api('GET', '/model')).data.model.dashboards.find((each) => each.name === 'Quarter').tiles;
    await page.goto('/dashboards?name=Quarter');
    await page.click('button', 'New');
    await page.click('.menu [role="menuitem"]', 'A copy of Quarter');
    await page.waitFor(`document.querySelector('dialog[open] #dashboard-name')?.value === 'Copy of Quarter'`, 'the copy, named');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('.board.editing') && document.querySelectorAll('.dash-tile').length === ${tiles.length}`, 'the copy, to be arranged');
    await page.click('button', 'Save');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent === 'Saved Copy of Quarter.')`, 'the copy, saved');
    const copy = (await page.api('GET', '/model')).data.model.dashboards.find((each) => each.name === 'Copy of Quarter');
    expect(quoted(copy?.tiles) === quoted(tiles), `The copy's tiles are ${quoted(copy?.tiles)}, not Quarter's.`);
    await page.click('button', 'Delete');
    await page.click('dialog[open] button', 'Delete');
    await page.waitFor(`!document.querySelector('dialog[open]') && ![...document.querySelectorAll('.dash-tabs a')].some((tab) => tab.textContent === 'Copy of Quarter')`, 'the copy, taken away');
  });
  await step('A query is pinned from Explore to a new dashboard, a tile showing it as Explore does', async () => {
    await page.goto('/explore?cube=General%20Ledger');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
    await page.click('button', 'Views');
    await page.click('.menu [role="menuitem"]', 'Pin to a dashboard…');
    await page.fill('#pin-view', 'Pinned');
    await page.fill('#pin-dashboard', 'Pins');
    await page.press('Enter');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.startsWith('Pinned to Pins.'))`, 'the query, pinned');
    const { data } = await page.api('GET', '/model');
    const [tile, ...more] = data.model.dashboards.find((each) => each.name === 'Pins')?.tiles ?? [];
    expect(!more.length && tile?.cube === 'General Ledger' && tile.view === 'Pinned' && tile.show === 'grid', `Pins has the tiles ${quoted([tile, ...more])}.`);
    await page.click('.notice a', 'Open it');
    await page.waitFor(`location.search === '?name=Pins' && document.querySelector('.dash-tile .tile-table [role="cell"]')`, 'the pinned tile');
    await page.click('button', 'Delete');
    await page.click('dialog[open] button', 'Delete');
    await page.waitFor(`!document.querySelector('dialog[open]') && ![...document.querySelectorAll('.dash-tabs a')].some((tab) => tab.textContent === 'Pins')`, 'Pins, taken away');
    // The view it showed, taken away too.
    const cube = (await page.api('GET', `/cubes/General%20Ledger`)).data;
    delete cube.views.Pinned;
    const based = (await page.api('GET', '/model')).data.version;
    expect((await page.api('PUT', `/cubes/General%20Ledger`, { based_on: based, cube })).status === 200, 'The pinned view was not taken away.');
  });
  await step('A change to the model that leaves the dimensions as they were fetches none of them again', async () => {
    await page.goto('/explore?cube=General%20Ledger');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
    const fetched = `performance.getEntriesByType('resource').filter((entry) => entry.name.includes('/api/dimensions/')).length`;
    const before = await page.evaluate(fetched);
    expect(before === 4, `${before} dimensions were fetched for the cube's four.`);
    // A dashboard saved moves the model on, its dimensions as they were.
    const based = (await page.api('GET', '/model')).data.version;
    const dashboard = { name: 'Unseen', tiles: [{ show: 'text', text: 'Hello' }] };
    expect((await page.api('PUT', '/dashboards/Unseen', { based_on: based, dashboard })).status === 200, 'The dashboard was not saved.');
    await page.click('.nav a', 'Dashboards');
    await page.waitFor(`location.pathname === '/dashboards' && document.querySelector('.dash-tile')`, 'the dashboards');
    await page.click('.nav a', 'Explore');
    await page.waitFor(`location.pathname === '/explore' && document.querySelector('td[data-r]')`, 'Explore again');
    const after = await page.evaluate(fetched);
    expect(after === before, `Explore fetched ${after - before} dimensions again, though none had changed.`);
    const version = (await page.api('GET', '/model')).data.version;
    expect((await page.api('DELETE', `/dashboards/Unseen?based_on=${version}`)).status === 200, 'The dashboard was not taken away.');
  });
  await step('At a phone\'s width no page is wider than the screen, the grid shows on the first, and a name cut short shows whole', async () => {
    // A phone widens the page to what it holds and draws it smaller: the screen is the measure.
    const phone = { width: 390, height: 844 };
    await page.call('Emulation.setDeviceMetricsOverride', { ...phone, deviceScaleFactor: 1, mobile: true });
    try {
      for (const [path, ready] of [['/', '.tile'], ['/explore?cube=General%20Ledger', 'td[data-r]'], ['/admin', 'table'], ['/model?dimension=Account', '.tree-row'], ['/dashboards?name=Quarter', '.dash-tile'], ['/keys', 'main .card']]) {
        await page.goto(path);
        await page.waitFor(`document.querySelector(${quoted(ready)})`, path);
        await page.settled();
        const wider = await page.evaluate('document.documentElement.scrollWidth') - phone.width;
        expect(wider <= 0, `${path} is ${wider} pixels wider than a phone's screen.`);
        if (path.startsWith('/dashboards')) {
          // The tiles, laid out freely across, stand one above the other as they are read.
          const stacked = await page.evaluate(`(() => {
            const tiles = [...document.querySelectorAll('.dash-tile')].map((tile) => tile.getBoundingClientRect()).sort((a, b) => a.top - b.top);
            return document.querySelector('.board.stacked') && tiles.every((box, index) => index === 0 || box.top >= tiles[index - 1].bottom - 1);
          })()`);
          expect(stacked, 'A dashboard\'s tiles do not stand one above the other on a phone.');
        }
        if (path === '/') {
          // A place's name cut short shows whole, pointed at.
          const cut = await page.evaluate(`(() => {
            const name = [...document.querySelectorAll('.recent .grow')].find((each) => each.scrollWidth > each.clientWidth);
            name?.scrollIntoView({ block: 'center' });
            const box = name?.getBoundingClientRect();
            return name ? { x: box.x + box.width / 2, y: box.y + box.height / 2, text: name.textContent } : null;
          })()`);
          expect(cut, 'No place\'s name is cut short at a phone\'s width.');
          await page.call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: cut.x, y: cut.y });
          await page.waitFor(`document.querySelector('.recent .grow[title]')?.title === ${quoted(cut.text)}`, 'the name, whole on hover');
        }
        if (path.startsWith('/explore')) {
          const below = await page.evaluate(`document.querySelector('td[data-r]').getBoundingClientRect().bottom`) - phone.height;
          expect(below < 0, `The grid's first cell is ${below} pixels below a phone's screen.`);
          // The layout's zones a row each, to leave the grid the height.
          const zones = await page.evaluate(`[...document.querySelectorAll('.zone')].map((zone) => Math.round(zone.getBoundingClientRect().height))`);
          expect(zones.every((height) => height <= 56), `The layout's zones are ${quoted(zones)} pixels high on a phone, rather than a row each.`);
          await rulesFit('a phone\'s width');
          // A cell's comments over the whole screen, the grid having no room beside them.
          await page.click('td[data-r][data-c]');
          await page.press('F2', SHIFT);
          await page.waitFor(`document.querySelector('.comment-panel:not([hidden]) textarea')`, 'the comments');
          const sheet = await page.evaluate(`(() => {
            const panel = document.querySelector('.comment-panel');
            const box = panel.getBoundingClientRect();
            const bottom = Math.max(...[...panel.querySelectorAll('*')].map((each) => each.getBoundingClientRect().bottom));
            return { left: Math.round(box.left), width: Math.round(box.width), below: Math.round(bottom - box.bottom) };
          })()`);
          expect(sheet.left === 0 && sheet.width === phone.width, `The comments are ${sheet.width} pixels wide, ${sheet.left} from the left, on a phone.`);
          expect(sheet.below <= 0, `The comments reach ${sheet.below} pixels below their panel on a phone.`);
          await page.click('.comment-panel button', 'Close the panel');
          await page.waitFor(`document.querySelector('.comment-panel').hidden`, 'the comments, closed');
        }
        if (path === '/admin') {
          // The user's editor, its widest rows in it, no wider than the dialog.
          await page.click('button', 'Add user');
          await page.waitFor(`document.querySelector('dialog[open] #user-again')`, 'the user\'s editor');
          await page.click('dialog[open] button', 'Add a cube role');
          await page.click('dialog[open] button', 'Add an element role');
          const sideways = await page.evaluate(`(() => {
            const body = document.querySelector('dialog[open] .dialog-body');
            return body.scrollWidth - body.clientWidth;
          })()`);
          expect(sideways <= 0, `The user's editor is ${sideways} pixels wider than its dialog on a phone.`);
          await page.press('Escape');
          await page.waitFor(`!document.querySelector('dialog[open]')`, 'the editor, closed');
        }
        if (path.startsWith('/model')) {
          // One at a time: the dimension open leads back to the list, which opens another.
          await page.click('.editor-back');
          await page.waitFor(`location.search === '' && [...document.querySelectorAll('.model-item')].some((item) => item.getBoundingClientRect().width > 0)`, 'the list of dimensions and cubes');
          const beside = await page.evaluate(`document.querySelector('.model-pane').getBoundingClientRect().width`);
          expect(beside === 0, `The pane beside the list is drawn ${beside} pixels wide on a phone.`);
          await page.click('.model-item[href="/model?dimension=Entity"]');
          await page.waitFor(`document.querySelector('.editor-head h2')?.textContent === 'Entity' && !document.querySelector('.model-list').getBoundingClientRect().width`, 'Entity, open in place of the list');
        }
      }
    } finally {
      await page.call('Emulation.clearDeviceMetricsOverride');
    }
  });
  await step('In a laptop\'s window the set editor\'s filter rules fit its narrower pane, and push nothing over the tree', async () => {
    await page.call('Emulation.setDeviceMetricsOverride', { width: 1100, height: 800, deviceScaleFactor: 1, mobile: false });
    try {
      await page.goto('/explore?cube=General%20Ledger');
      await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
      await rulesFit('a laptop\'s width');
    } finally {
      await page.call('Emulation.clearDeviceMetricsOverride');
    }
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
    expect(await page.evaluate('document.title') === '• Entity · Model · Epiphany', 'The browser\'s title does not mark the dimension unsaved.');
    expect(await page.evaluate(ASKS_TO_CLOSE), 'The browser would close the page, the dimension unsaved, without asking.');
    // Saved by the keys, as by the strip's button; pressed again, with nothing to save, it says so.
    await page.press('s', CTRL);
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('Saved'))`, 'the model, saved');
    await page.press('s', CTRL);
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent === 'Nothing to save.')`, 'nothing left to save');
    const { data } = await page.api('GET', '/model');
    const entity = data.model.dimensions.find((dimension) => dimension.name === 'Entity');
    const total = entity.elements.find((element) => element.name === 'Total Entity');
    expect(data.version === before + 1, `The model is at version ${data.version}, not ${before + 1}.`);
    expect(total.children.some((child) => child.name === 'FR'), `Total Entity's children are ${quoted(total.children)}.`);
    expect(entity.elements.find((element) => element.name === 'FR')?.type === 'numeric', 'FR is not a numeric element.');
  });
  await step('An element\'s children are listed to weigh or take out, the keys staying on the list, and the change discarded', async () => {
    const children = `[...document.querySelectorAll('.child-list .child-row .grow')].map((name) => name.textContent).join()`;
    await page.waitFor(`${children} === 'US,UK,FR'`, 'Total Entity\'s children');
    await page.click('.child-list button', 'Take FR out of Total Entity');
    await page.waitFor(`${children} === 'US,UK' && document.activeElement?.getAttribute('aria-label') === 'Take UK out of Total Entity'`, 'FR taken out, the keys on the child before it');
    await page.click('button', 'Discard');
    await page.waitFor(`${children} === 'US,UK,FR' && !document.querySelector('.pending:not([hidden])')`, 'the change, discarded');
    // Found by a word of its name, beneath the member above it.
    await page.fill('input[placeholder="Find an element"]', 'fr');
    await page.waitFor(`[...document.querySelectorAll('.element-row.match')].map((row) => row.dataset.name).join() === 'FR'`, 'FR, found');
    await page.clear('input[placeholder="Find an element"]');
  });
  await step('An earlier version of the model is compared with the model as it is', async () => {
    await page.click('button', 'Earlier versions of the model');
    await page.click('dialog[open] .history-list button');
    // The newest earlier version is the model before FR was added: Entity alone differs.
    await page.waitFor(`document.querySelector('dialog[open] .plain-list')?.textContent === 'Dimensions put back as they were: Entity'`, 'the comparison');
    await page.press('Escape');
    await page.waitFor(`!document.querySelector('dialog[open]')`, 'the comparison, closed');
  });
  await step('A text element\'s picklist offers another dimension\'s elements, or one of its subsets', async () => {
    const choose = (css, value) => page.evaluate(`(() => {
      const select = document.querySelector(${quoted(css)});
      select.value = ${quoted(value)};
      select.dispatchEvent(new Event('change'));
    })()`);
    await page.click('[role="treeitem"][data-name="FR"]');
    await page.waitFor(`document.querySelector('#element-name')?.value === 'FR'`, 'FR, chosen');
    await choose('#element-type', 'string');
    await page.waitFor(`document.querySelector('#picklist-kind')`, 'the picklist');
    await choose('#picklist-kind', 'members');
    await page.waitFor(`document.querySelector('.field-pair select')`, 'the dimension to offer');
    await choose('.field-pair select', 'Period');
    const offered = `[...document.querySelectorAll('.field-pair select:last-of-type option')].map((option) => option.textContent).join()`;
    await page.waitFor(`${offered} === 'Every element,The subset First half,The subset Months,The subset Quarters'`, 'Period\'s subsets, offered');
    await page.click('button', 'Discard');
    await page.waitFor(`!document.querySelector('.pending:not([hidden])') && !document.querySelector('#picklist-kind')`, 'the change, discarded');
  });
}

async function administering({ reader }) {
  await step('Administration lists the users', async () => {
    await page.goto('/admin');
    await page.waitFor(`[...document.querySelectorAll('td strong')].some((name) => name.textContent === ${quoted(reader.name)})`, 'the users');
    await page.audit('Administration');
    // When its figures were fetched, as long ago as it was.
    const fetched = await page.evaluate(`document.querySelector('.page-head time .ago')?.textContent`);
    expect(fetched === 'now', `Administration says its figures are from ${quoted(fetched)}.`);
  });
  await step('An administrator hides an element from a user, chosen from its tree', async () => {
    await page.click('button', `Edit ${reader.name}`);
    await page.waitFor(`document.querySelector('dialog[open]')?.getAttribute('aria-label') === ${quoted(`Edit ${reader.name}`)}`, 'the user\'s editor');
    await page.click('dialog[open] button', 'Add an element role');
    // A row added and taken away again, the keys stay in the list, on the row left.
    await page.click('dialog[open] button', 'Add an element role');
    await page.evaluate(`[...document.querySelectorAll('dialog[open] .element-role')].at(-1).querySelector('button[aria-label="Remove this element role"]').click()`);
    await page.waitFor(`document.querySelectorAll('dialog[open] .element-role').length === 1
      && document.activeElement === document.querySelector('dialog[open] .element-role button[aria-label="Remove this element role"]')`, 'the keys, on the row left');
    await page.evaluate(`(() => {
      const select = document.querySelector('dialog[open] .element-role select[aria-label="Dimension"]');
      select.value = 'Entity';
      select.dispatchEvent(new Event('change'));
    })()`);
    await page.click('dialog[open] button', 'Choose an element of Entity');
    await page.waitFor(`[...document.querySelectorAll('dialog[open]')].some((open) => open.getAttribute('aria-label') === 'An element of Entity')`, 'the tree of Entity');
    await page.click('dialog[open] .picker-row', 'UK');
    await page.click('dialog[open] button', 'Apply');
    await page.waitFor(`document.querySelector('dialog[open] .element-role button')?.getAttribute('aria-label') === 'Element UK, chosen from Entity'`, 'UK, chosen');
    await page.audit('Hiding an element from a user');
    await page.click('dialog[open] button', 'Save');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes(${quoted(`Saved ${reader.name}`)}))`, 'the user, saved');
    const users = (await page.api('GET', '/admin/users')).data.users;
    const elements = users.find((user) => user.name === reader.name)?.elements;
    expect(quoted(elements) === quoted({ Entity: { UK: 'none' } }), `The API keeps ${quoted(elements)} for ${reader.name}.`);
    await page.waitFor(`[...document.querySelectorAll('td')].some((cell) => cell.textContent === 'Entity · UK: hidden')`, 'the role in the list of users');
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
    // What is hidden from the reader is not there for the view as them either.
    const entity = (await page.api('GET', '/dimensions/Entity')).data.elements.map((element) => element.name);
    expect(!entity.includes('UK') && entity.includes('US'), `A view as the reader shows Entity as ${quoted(entity)}.`);
    const refused = await page.api('POST', '/cells/write', { cells: [{ cube: 'General Ledger', at: ['Rent', 'US', 'Mar', 'Budget'], value: 1 }] });
    expect(refused.status === 403, `A write while viewing was answered ${refused.status}.`);
    await page.goto('/explore?cube=General%20Ledger');
    await page.waitFor(`document.querySelector('td[data-r]')`, 'the grid');
    await page.settled();
    await page.click('td[data-r="0"][data-c="0"]');
    await page.type('5');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes('not write to it'))`, 'the refusal');
    await page.audit('A value refused');
    // Nor is a view saved in the reader's name.
    await page.click('button', 'Views');
    await page.waitFor(`document.querySelector('.menu')`, 'the views');
    const offered = await page.evaluate(`[...document.querySelectorAll('.menu [role="menuitem"]')].map((item) => item.textContent.trim())`);
    expect(!offered.some((label) => label.startsWith('Save')), `A view as a reader offers ${quoted(offered)}.`);
    await page.press('Escape');
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

async function usingKeys({ admin, reader }, server) {
  /** The server asked as a program asks it: directly, with a key or a session's token. */
  const direct = async (method, path, token, body) => {
    const headers = { authorization: `Bearer ${token}`, ...(body ? { 'content-type': 'application/json' } : {}) };
    const answer = await fetch(`${server}${path}`, { method, headers, body: body ? JSON.stringify(body) : undefined });
    return { status: answer.status, data: answer.status === 204 ? null : await answer.json() };
  };
  const dialogNamed = (name) => `document.querySelector('dialog[open]')?.getAttribute('aria-label') === ${quoted(name)}`;
  let made;
  await step('A key is made from the user menu, given all its user may do with one click, and shown once', async () => {
    await page.click('button[aria-haspopup="menu"]', admin.name);
    await page.click('.menu [role="menuitem"]', 'API keys');
    await page.waitFor(`document.querySelector('h1')?.textContent === 'API keys' && document.querySelector('main .empty')`, 'the keys page, with none');
    await page.audit('API keys, none yet');
    await page.click('.page-head button', 'New key');
    await page.waitFor(dialogNamed('New API key'), 'the new key');
    await page.fill('#key-name', 'Nightly load');
    await page.click('dialog[open] button', 'Everything you may do');
    const everything = 'Read and write,Read and change,Read and manage';
    await page.waitFor(`[...document.querySelectorAll('dialog[open] [role="radio"][aria-checked="true"]')].map((each) => each.textContent).join() === ${quoted(everything)}`, 'everything chosen');
    await page.audit('A new API key');
    await page.click('dialog[open] button', 'Make key');
    await page.waitFor(dialogNamed('Your key Nightly load'), 'the key, shown once');
    made = await page.evaluate(`document.querySelector('dialog[open] .key-shown input').value`);
    expect(/^epk_[0-9a-f]{64}$/.test(made), `The key shown is ${quoted(made)}.`);
    await page.audit('A key shown once');
    await page.click('dialog[open] button', 'Done');
    await page.waitFor(`[...document.querySelectorAll('td strong')].some((name) => name.textContent === 'Nightly load')`, 'the key, listed');
    // Never shown again; and a program uses it as its user may.
    const listed = await page.evaluate(`document.querySelector('main').textContent`);
    expect(!listed.includes(made.slice(16)), 'The list of keys shows a key itself.');
    const state = await direct('GET', '/admin/status', made);
    expect(state.status === 200, `The key asking for the server's state was answered ${state.status}.`);
    // Its list says it was used just now; before, never.
    await page.goto('/keys');
    await page.waitFor(`document.querySelector('main tbody tr td:nth-child(5) .ago')?.textContent === 'now'`, 'the key, used just now');
  });
  await step('An administrator lets a reader make keys, no more than the reader may, and revokes one', async () => {
    await page.goto('/admin');
    await page.click('button', `Edit ${reader.name}`);
    await page.waitFor(dialogNamed(`Edit ${reader.name}`), 'the user\'s editor');
    await page.click('dialog[open] label', 'May make API keys');
    await page.click('dialog[open] button', 'Save');
    await page.waitFor(`[...document.querySelectorAll('.notice')].some((notice) => notice.textContent.includes(${quoted(`Saved ${reader.name}`)}))`, 'the user, saved');
    const signedIn = await fetch(`${server}/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: reader.name, password: reader.password }),
    }).then((answer) => answer.json());
    const theirs = await direct('POST', '/keys', signedIn.token, { name: 'Report', cells: 'read', model: 'read' });
    expect(theirs.status === 201, `The reader's key was answered ${theirs.status}.`);
    const beyond = await direct('POST', '/keys', signedIn.token, { name: 'Loader', cells: 'write' });
    expect(beyond.status === 422, `A reader's key that writes was answered ${beyond.status}.`);
    await page.goto('/admin');
    await page.click('button', `API keys of ${reader.name}`);
    await page.waitFor(dialogNamed(`API keys of ${reader.name}`), 'the reader\'s keys');
    await page.audit('A user\'s API keys');
    await page.click('dialog[open] button', 'Revoke Report');
    await page.waitFor(`document.querySelectorAll('dialog[open]').length === 2`, 'the revoking, asked');
    await page.click('dialog[open] button', 'Revoke key');
    await page.waitFor(`document.querySelector('dialog[open]')?.textContent.includes('They hold none now.')`, 'the key, gone from the list');
    const after = await direct('GET', '/model', theirs.data.key);
    expect(after.status === 401, `The revoked key was answered ${after.status}.`);
    await page.click('dialog[open] button', 'Done');
    await page.settled();
    // The audit keeps who made it and who revoked it.
    const revoked = `Revoked the API key Report (epk_${theirs.data.id}) of ${reader.name}`;
    await page.waitFor(`document.querySelector('main')?.textContent.includes(${quoted(revoked)})`, 'the revoking, on record');
  });
  await step('A key its user revokes stops at once', async () => {
    await page.goto('/keys');
    await page.click('button', 'Revoke Nightly load');
    await page.waitFor(dialogNamed('Revoke Nightly load?'), 'the revoking, asked');
    await page.click('dialog[open] button', 'Revoke key');
    await page.waitFor(`document.querySelector('main .empty')`, 'no keys left');
    const after = await direct('GET', '/model', made);
    expect(after.status === 401, `The revoked key was answered ${after.status}.`);
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
  await step('Printing a grid draws all of it, and afterwards only what is in view again', async () => {
    const drawn = `document.querySelectorAll('td[data-r]').length`;
    const printed = await page.evaluate(`(() => {
      const began = performance.now();
      dispatchEvent(new Event('beforeprint'));
      return { cells: ${drawn}, took: performance.now() - began };
    })()`);
    console.log(`    ${printed.cells.toLocaleString('en-US')} cells drawn for paper in ${printed.took.toFixed(0)} ms`);
    expect(printed.cells === 100_000, `${printed.cells} cells are drawn for paper, of 100,000.`);
    await page.evaluate(`dispatchEvent(new Event('afterprint'))`);
    const after = await page.evaluate(drawn);
    expect(after < 10_000, `${after} cells are still drawn after printing.`);
  });
  await step('Printing a long list of elements draws all of it, in order, and afterwards only what is in view', async () => {
    await page.goto('/model?dimension=Row');
    await page.waitFor(`document.querySelector('.element-tree .tree-row')`, 'the elements');
    const drawn = `document.querySelectorAll('.element-tree .rows > *').length`;
    await page.evaluate(`dispatchEvent(new Event('beforeprint'))`);
    // Laid out for paper, the list changes size, as a page printing does: every row stays.
    await page.evaluate(`document.querySelector('.element-tree').style.height = '300px'`);
    await page.evaluate('new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))');
    const printed = await page.evaluate(`(() => {
      const rows = [...document.querySelectorAll('.element-tree .rows > *')];
      return { count: rows.length, ordered: rows.every((row, index) => !index || parseFloat(row.style.top) > parseFloat(rows[index - 1].style.top)) };
    })()`);
    expect(printed.count === 2000 && printed.ordered, `${printed.count} elements are drawn for paper, of 2,000${printed.ordered ? '' : ', out of order'}.`);
    await page.evaluate(`dispatchEvent(new Event('afterprint'))`);
    await page.evaluate(`document.querySelector('.element-tree').style.height = ''`);
    const after = await page.evaluate(drawn);
    expect(after < 200, `${after} elements are still drawn after printing.`);
  });
  await step('Rows sorted by a column say so on its heading, to the eye and to a screen reader', async () => {
    await page.goto('/explore?cube=Large');
    await page.waitFor(`document.querySelector('td[data-r="0"][data-c="3"]')`, 'the grid');
    await page.settled();
    await page.evaluate(`(() => {
      const cell = document.querySelector('td[data-r="0"][data-c="3"]');
      const box = cell.getBoundingClientRect();
      cell.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: box.x + 5, clientY: box.y + 5 }));
    })()`);
    await page.click('.menu [role="menuitem"]', 'Sort rows by this column, largest first');
    await page.waitFor(`document.querySelector('th[aria-sort="descending"] .sort-mark')`, 'the column, marked sorted');
    const marked = await page.evaluate(`[...document.querySelectorAll('th[aria-sort]')].map((th) => th.dataset.c1 + ' ' + th.getAttribute('aria-sort'))`);
    expect(quoted(marked) === quoted(['3 descending']), `The headings marked sorted are ${quoted(marked)}.`);
    await page.audit('Rows sorted by a column');
    await page.click('button', 'Clear sort');
    await page.waitFor(`!document.querySelector('th[aria-sort]')`, 'the sort, cleared');
  });
}

/* ---- The run ---- */

async function main() {
  mkdirSync(join(ROOT, 'target'), { recursive: true });
  folder = mkdtempSync(join(ROOT, 'target', 'browser-journeys-'));
  const data = join(folder, 'server');
  cpSync(join(ROOT, 'sample', 'model'), join(data, 'model'), { recursive: true });
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
    ['API keys', () => usingKeys(users, server)],
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
