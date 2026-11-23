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
    return this.waitFor(`(async () => {
      const element = [...document.querySelectorAll(${quoted(css)})].find((each) => {
        const box = each.getBoundingClientRect();
        const named = ${quoted(name ?? null)};
        return box.width > 0 && box.height > 0 && (named === null || each.getAttribute('aria-label') === named || each.textContent.trim() === named);
      });
      if (!element) return null;
      // Brought into view only if out of it, as a person scrolls to what they cannot see.
      const seen = element.getBoundingClientRect();
      if (seen.top < 0 || seen.left < 0 || seen.bottom > innerHeight || seen.right > innerWidth) {
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
  });
  await step('At a phone\'s width no page is wider than the screen, the grid shows on the first, and a name cut short shows whole', async () => {
    // A phone widens the page to what it holds and draws it smaller: the screen is the measure.
    const phone = { width: 390, height: 844 };
    await page.call('Emulation.setDeviceMetricsOverride', { ...phone, deviceScaleFactor: 1, mobile: true });
    try {
      for (const [path, ready] of [['/', '.tile'], ['/explore?cube=General%20Ledger', 'td[data-r]'], ['/admin', 'table'], ['/model?dimension=Account', '.tree-row']]) {
        await page.goto(path);
        await page.waitFor(`document.querySelector(${quoted(ready)})`, path);
        await page.settled();
        const wider = await page.evaluate('document.documentElement.scrollWidth') - phone.width;
        expect(wider <= 0, `${path} is ${wider} pixels wider than a phone's screen.`);
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
        }
      }
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
    const printed = await page.evaluate(`(() => {
      const rows = [...document.querySelectorAll('.element-tree .rows > *')];
      return { count: rows.length, ordered: rows.every((row, index) => !index || parseFloat(row.style.top) > parseFloat(rows[index - 1].style.top)) };
    })()`);
    expect(printed.count === 2000 && printed.ordered, `${printed.count} elements are drawn for paper, of 2,000${printed.ordered ? '' : ', out of order'}.`);
    await page.evaluate(`dispatchEvent(new Event('afterprint'))`);
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
