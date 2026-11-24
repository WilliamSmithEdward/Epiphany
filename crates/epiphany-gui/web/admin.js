// Administration: the server's state, its users and their roles, and open transactions.

import { del, get, part, post, put } from './api.js';
import { empty, failure, go, loading, model, state } from './app.js';
import { keyTable } from './keys.js';
import { cellQuery, handOver } from './layout.js';
import { chooseMember, dimension } from './picker.js';
import { dialog, h, icon, invalid, mount, notify, refocus, revealable, when, working } from './ui.js';

const ROLES = { admin: 'Administrator', write: 'Can write', read: 'Can read', none: 'No access' };
/** The roles an element takes: hidden, it is not there for the user at all. */
const ELEMENT_ROLES = { none: 'Hidden', read: 'Can read', write: 'Can write' };

/** A user's roles on elements, as the users list says them: Region · Europe: hidden. */
const elementRoles = (elements) => Object.entries(elements ?? {})
  .flatMap(([dimensionName, roles]) => Object.entries(roles).map(([element, role]) => `${dimensionName} · ${element}: ${ELEMENT_ROLES[role].toLowerCase()}`));

/** Whether a user may make API keys: an administrator always, anyone else once let. */
const makesKeys = (user) => user.role === 'admin' || user.makes_keys;

/** A user's API keys, as the users list says them. */
const keysSaid = (user) => {
  if (!makesKeys(user)) return h('span', { class: 'muted' }, 'Not allowed');
  const count = user.keys.length;
  return count ? `${count} key${count === 1 ? '' : 's'}` : h('span', { class: 'muted' }, 'None yet');
};

const bytes = (count) => {
  if (count === null || count === undefined) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = count;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toLocaleString(undefined, { maximumFractionDigits: unit ? 1 : 0 })} ${units[unit]}`;
};

const duration = (seconds) => {
  if (seconds < 60) return `${seconds} s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} h ${Math.floor((seconds % 3600) / 60)} min`;
  return `${Math.floor(seconds / 86400)} d ${Math.floor((seconds % 86400) / 3600)} h`;
};

export async function render(target) {
  return new AdminPage(target);
}

class AdminPage {
  constructor(target) {
    this.statusCard = h('section', { class: 'admin-section', 'aria-label': 'Server' });
    this.usersCard = h('section', { class: 'card fit', 'aria-label': 'Users' });
    this.auditCard = h('section', { class: 'card fit', 'aria-label': 'Audit' });
    this.transactionsCard = h('section', { class: 'card fit', 'aria-label': 'Open transactions' });
    this.changesCard = h('section', { class: 'card fit', 'aria-label': 'Changes to cells' });
    this.updated = h('span', { class: 'muted' });
    mount(target, h('div', { class: 'page' },
      h('div', { class: 'page-head' },
        h('div', { class: 'grow' }, h('h1', {}, 'Administration'), h('p', {}, 'The server, its users and the work in progress on it.')),
        this.updated,
        h('button', { class: 'button', onclick: (event) => working(event.currentTarget, () => this.load()) }, icon('refresh', 16), 'Refresh')),
      this.statusCard,
      this.usersCard,
      this.auditCard,
      this.transactionsCard,
      this.changesCard));
    // Until the first answer, each part holds its outline.
    mount(this.statusCard, loading('cards'));
    mount(this.usersCard, h('div', { class: 'card-body' }, loading('lines')));
    mount(this.auditCard, h('div', { class: 'card-body' }, loading('lines')));
    mount(this.transactionsCard, h('div', { class: 'card-body' }, loading('lines')));
    this.buildChanges();
    this.load();
    this.loadChanges();
    // Kept current while the page is open.
    this.timer = setInterval(() => {
      if (!target.isConnected) clearInterval(this.timer);
      else if (!document.hidden && !document.querySelector('dialog[open]')) this.load(true);
    }, 15000);
  }

  /** Fetches the page's parts; `quietly`, as the page keeps itself current, without a word. */
  async load(quietly = false) {
    try {
      const [status, users, transactions, audit] = await Promise.all([
        get('/admin/status', { quiet: quietly }),
        get('/admin/users', { quiet: quietly }),
        get('/admin/transactions', { quiet: quietly }),
        get('/admin/audit?most=50', { quiet: quietly }),
      ]);
      this.drawStatus(status);
      this.drawUsers(users);
      this.drawTransactions(transactions);
      this.drawAudit(audit);
      this.offer(this.userFilter, 'Anyone', (users.users ?? []).map((user) => user.name));
      this.offer(this.cubeFilter, 'Every cube', status.cubes.map((cube) => cube.name));
      this.usersCard.hidden = false;
      this.auditCard.hidden = !users.signing_in;
      this.transactionsCard.hidden = false;
      mount(this.updated, 'Updated ', when(Date.now() / 1000));
    } catch (problem) {
      if (quietly) return;
      // What could not be fetched is not shown as if it were coming.
      this.usersCard.hidden = true;
      this.auditCard.hidden = true;
      this.transactionsCard.hidden = true;
      mount(this.statusCard, h('div', { class: 'card' }, failure(problem, () => this.load())));
    }
  }

  /* ---- The server ---- */

  drawStatus(status) {
    const cells = status.cubes.reduce((sum, cube) => sum + cube.cells, 0);
    const tile = (label, value, iconName, detail) => h('div', { class: 'card tile' },
      h('div', { class: 'tile-icon' }, icon(iconName, 20)),
      h('div', { class: 'stat' }, value),
      h('div', { class: 'muted' }, label),
      detail ? h('div', { class: 'hint' }, detail) : null);
    mount(this.statusCard,
      h('div', { class: 'grid-cards' },
        tile('Running for', duration(status.uptime_seconds), 'server', `Epiphany ${status.server} · model version ${status.model_version}`),
        tile('Cells stored', cells.toLocaleString(), 'cube', `In ${status.cubes.length} cube${status.cubes.length === 1 ? '' : 's'}`),
        tile('Sessions open', status.sessions === null ? '—' : status.sessions.toLocaleString(), 'users', status.sessions === null ? 'No one needs to sign in' : null),
        tile('Transactions open', status.transactions.toLocaleString(), 'lock', null),
        // Every save of the model keeps the one it replaces: what they take is said apart.
        tile('On disk', bytes(status.log_bytes === null ? null : status.log_bytes + status.snapshot_bytes + status.model_bytes + status.earlier_models_bytes), 'save',
          `Log ${bytes(status.log_bytes)} · snapshot ${bytes(status.snapshot_bytes)} · model ${bytes(status.model_bytes)} · earlier models ${bytes(status.earlier_models_bytes)}`),
        status.memory_bytes !== null ? tile('Memory held', bytes(status.memory_bytes), 'server', null) : null),
      status.cubes.length ? h('div', { class: 'card fit' },
        h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Cells by cube')),
        h('table', { class: 'table' },
          h('thead', {}, h('tr', {}, h('th', {}, 'Cube'), h('th', { class: 'num-col' }, 'Cells stored'), h('th', { class: 'bar-col' }, ''))),
          h('tbody', {}, status.cubes.map((cube) => {
            const share = cells ? cube.cells / cells : 0;
            const bar = h('div', { class: 'share' }, h('span', { class: 'share-fill' }));
            bar.firstChild.style.width = `${Math.max(share * 100, cube.cells ? 1 : 0)}%`;
            return h('tr', {},
              h('td', {}, cube.name),
              h('td', { class: 'num-col num' }, cube.cells.toLocaleString()),
              h('td', { class: 'bar-col' }, bar));
          })))) : null);
  }

  /* ---- Users ---- */

  drawUsers(list) {
    const head = h('div', { class: 'card-head' },
      h('h3', { class: 'grow' }, 'Users'),
      list.signing_in ? h('button', { class: 'button small primary', onclick: (event) => this.editUser(null, event.currentTarget) }, icon('plus', 14), 'Add user') : null);
    if (!list.signing_in) {
      mount(this.usersCard, head, empty('users', 'No one needs to sign in', 'This server has no users, so it only listens on its own machine. Add the first with “epiphany-server user”, then restart the server; after that, users are managed here.'));
      return;
    }
    const me = state.user?.name?.toLowerCase();
    mount(this.usersCard, head,
      h('table', { class: 'table' },
        h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Role'), h('th', {}, 'On particular cubes'), h('th', {}, 'On particular elements'), h('th', {}, 'API keys'), h('th', { class: 'num-col' }, 'Sessions'), h('th', { class: 'actions' }, ''))),
        h('tbody', {}, list.users.map((user) => {
          const cubes = Object.entries(user.cubes);
          return h('tr', {},
            h('td', {}, h('strong', {}, user.name), user.name.toLowerCase() === me ? h('span', { class: 'badge accent you' }, 'You') : null),
            h('td', {}, h('span', { class: `badge ${user.role === 'admin' ? 'accent' : user.role === 'none' ? 'bad' : ''}` }, ROLES[user.role])),
            h('td', { class: 'muted' }, cubes.length ? cubes.map(([cube, role]) => `${cube}: ${ROLES[role].toLowerCase()}`).join(' · ') : '—'),
            h('td', { class: 'muted' }, elementRoles(user.elements).join(', ') || '—'),
            h('td', { class: 'nowrap' }, keysSaid(user)),
            h('td', { class: 'num-col num' }, user.sessions.toLocaleString()),
            // Each row's buttons name its user, to be told from the next row's.
            h('td', { class: 'actions' },
              h('button', { class: 'button small quiet', 'aria-label': `Edit ${user.name}`, onclick: (event) => this.editUser(user, event.currentTarget) }, icon('edit', 14), 'Edit'),
              user.keys.length ? h('button', { class: 'button small quiet', 'aria-label': `API keys of ${user.name}`, onclick: () => this.showKeys(user) }, icon('key', 14), 'Keys') : null,
              user.name.toLowerCase() === me ? null : h('button', {
                class: 'button small quiet',
                'aria-label': `View as ${user.name}`,
                title: `See the server as ${user.name} does, reading only. It is kept on record.`,
                onclick: (event) => working(event.currentTarget, () => this.viewAs(user)),
              }, icon('eye', 14), 'View as'),
              h('button', { class: 'button small quiet', 'aria-label': `Sign out ${user.name}`, disabled: !user.sessions, onclick: (event) => working(event.currentTarget, () => this.signOut(user)) }, icon('out', 14), 'Sign out'),
              h('button', { class: 'button small quiet danger', 'aria-label': `Remove ${user.name}`, onclick: () => this.removeUser(user) }, icon('trash', 14), 'Remove')));
        }))));
  }

  /**
   * Views the server as another user does, reading only, until the bar's Stop: the whole
   * application starts again as they have it. The server keeps it on record.
   */
  async viewAs(user) {
    try {
      await post('/gui/view-as', { user: user.name });
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    location.assign('/');
  }

  /** What the audit records, the newest first: each view as another user, and each API key made or revoked, by whom and when. */
  drawAudit(entries) {
    const says = {
      'view as': (subject) => `Viewed the server as ${subject}`,
      'make key': (subject) => `Made the API key ${subject}`,
      'revoke key': (subject) => `Revoked the API key ${subject}`,
    };
    const said = (entry) => says[entry.action]?.(entry.subject) ?? `${entry.action}: ${entry.subject}`;
    mount(this.auditCard,
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Audit'), h('span', { class: 'muted' }, 'Kept on record: each view of the server as another user, and each API key made or revoked')),
      entries.length
        ? h('table', { class: 'table' },
          h('thead', {}, h('tr', {}, h('th', {}, 'When'), h('th', {}, 'Who'), h('th', {}, 'What'))),
          h('tbody', {}, entries.map((entry) => h('tr', {},
            h('td', { class: 'muted' }, when(entry.time)),
            h('td', {}, h('strong', {}, entry.user)),
            h('td', {}, said(entry))))))
        : h('div', { class: 'card-body' }, h('p', { class: 'muted' }, 'Nothing yet.')));
  }

  /**
   * Adds a user, or edits one: their password, role, and roles on particular cubes and on
   * particular elements. The button that asked shows it at work while the model is fetched.
   */
  async editUser(user, button) {
    const adding = !user;
    const definition = await working(button, () => model());
    if (!definition) return;
    const cubes = definition.cubes.map((cube) => cube.name);
    const name = h('input', { class: 'input', id: 'user-name', required: adding, readonly: !adding, autocomplete: 'off' });
    name.value = user?.name ?? '';
    const password = h('input', { class: 'input', id: 'user-password', type: 'password', autocomplete: 'new-password', required: adding });
    const again = h('input', { class: 'input', id: 'user-again', type: 'password', autocomplete: 'new-password', required: adding });
    const role = h('select', { class: 'select', id: 'user-role' }, Object.entries(ROLES).map(([value, label]) => h('option', { value, selected: (user?.role ?? 'read') === value }, label)));
    // Whether they may make API keys: theirs to be let, an administrator's always.
    let allowed = Boolean(user?.makes_keys);
    const allowKeys = h('input', { type: 'checkbox', id: 'user-keys', onchange: () => {
      allowed = allowKeys.checked;
      drawKeys();
    } });
    const keysHint = h('span', { class: 'hint' });
    const held = user?.keys.length ?? 0;
    const drawKeys = () => {
      const admin = role.value === 'admin';
      allowKeys.checked = admin || allowed;
      allowKeys.disabled = admin;
      keysHint.textContent = admin
        ? 'Administrators always may.'
        : !allowed && held
          ? `Their ${held === 1 ? 'key stops' : `${held} keys stop`} working, and ${held === 1 ? 'is' : 'are'} taken away.`
          : 'Keys let their programs reach the server in their name, each doing no more than they may. Off until you let them.';
    };
    role.addEventListener('change', drawKeys);
    drawKeys();
    const rows = h('div', { class: 'cube-roles' });
    // A row taken away, the keys stay in the list.
    const takeAway = (list, row, otherwise) => {
      const index = [...list.children].indexOf(row);
      row.remove();
      refocus(list, index, otherwise);
    };
    const addRow = (cube = cubes[0], chosen = 'read') => {
      const cubeSelect = h('select', { class: 'select', 'aria-label': 'Cube' }, cubes.map((name) => h('option', { value: name, selected: name === cube }, name)));
      const roleSelect = h('select', { class: 'select', 'aria-label': 'Role on the cube' }, Object.entries(ROLES).map(([value, label]) => h('option', { value, selected: value === chosen }, label)));
      const row = h('div', { class: 'cube-role' }, cubeSelect, roleSelect,
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Remove this cube role', onclick: () => takeAway(rows, row, addCubeRole) }, icon('close', 14)));
      rows.append(row);
    };
    for (const [cube, chosen] of Object.entries(user?.cubes ?? {})) addRow(cube, chosen);
    // A role on an element: its dimension, the element chosen from the dimension's tree, and
    // the role, which holds beneath it too.
    const dimensions = definition.dimensions.map((each) => each.name);
    const elementRows = h('div', { class: 'cube-roles' });
    const addElementRow = (dimensionName = dimensions[0], element = null, chosen = 'none') => {
      const dimensionSelect = h('select', { class: 'select', 'aria-label': 'Dimension' }, dimensions.map((each) => h('option', { value: each, selected: each === dimensionName }, each)));
      const elementButton = h('button', { class: 'button', type: 'button' });
      const roleSelect = h('select', { class: 'select', 'aria-label': 'Role on the element' }, Object.entries(ELEMENT_ROLES).map(([value, label]) => h('option', { value, selected: value === chosen }, label)));
      const row = h('div', { class: 'cube-role element-role' }, dimensionSelect, elementButton, roleSelect,
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Remove this element role', onclick: () => takeAway(elementRows, row, addElementRole) }, icon('close', 14)));
      const pick = (picked) => {
        row.dataset.element = picked ?? '';
        elementButton.textContent = picked ?? 'Choose an element';
        elementButton.setAttribute('aria-label', picked ? `Element ${picked}, chosen from ${dimensionSelect.value}` : `Choose an element of ${dimensionSelect.value}`);
      };
      pick(element);
      elementButton.addEventListener('click', async () => {
        const dim = await working(elementButton, () => dimension(dimensionSelect.value));
        if (!dim) return;
        const member = await chooseMember(dim, { title: `An element of ${dim.name}`, current: row.dataset.element || undefined });
        if (member !== null) pick(member);
      });
      dimensionSelect.addEventListener('change', () => pick(null));
      elementRows.append(row);
    };
    for (const [dimensionName, roles] of Object.entries(user?.elements ?? {})) {
      for (const [element, chosen] of Object.entries(roles)) addElementRow(dimensionName, element, chosen);
    }
    const addCubeRole = h('button', { class: 'button small', type: 'button', onclick: () => addRow() }, icon('plus', 14), 'Add a cube role');
    const addElementRole = h('button', { class: 'button small', type: 'button', onclick: () => addElementRow() }, icon('plus', 14), 'Add an element role');
    const saved = await dialog({
      title: adding ? 'Add a user' : `Edit ${user.name}`,
      confirm: adding ? 'Add user' : 'Save',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'user-name' }, 'Name'), name),
        h('div', { class: 'field-pair' },
          h('div', { class: 'field' }, h('label', { for: 'user-password' }, adding ? 'Password' : 'New password'), revealable(password)),
          h('div', { class: 'field' }, h('label', { for: 'user-again' }, 'The same again'), revealable(again))),
        adding ? null : h('p', { class: 'hint' }, 'Leave the password empty to keep the one they have.'),
        h('div', { class: 'field' }, h('label', { for: 'user-role' }, 'Role'), role,
          h('span', { class: 'hint' }, 'What they may do on every cube, unless a cube below says otherwise. Only an administrator may change the model or manage users.')),
        h('div', { class: 'field' }, h('label', { class: 'check' }, allowKeys, 'May make API keys'), keysHint),
        h('div', { class: 'field' },
          h('label', {}, 'On particular cubes'),
          rows,
          cubes.length ? h('div', {}, addCubeRole) : null),
        h('div', { class: 'field' },
          h('label', {}, 'On particular elements'),
          elementRows,
          dimensions.length ? h('div', {}, addElementRole) : null,
          h('span', { class: 'hint' }, 'A role on an element holds for everything beneath it, unless an element nearer has its own. A hidden element is not there for the user at all, though totals still count it. Administrators are bound by none.'))),
      onsubmit: async () => {
        const value = name.value.trim();
        if (!value) throw invalid(name, 'Give the user a name.');
        if (password.value !== again.value) throw invalid(again, 'The two passwords differ.');
        if (adding && !password.value) throw invalid(password, 'A new user needs a password.');
        const cubeRoles = {};
        for (const row of rows.children) {
          const [cube, chosen] = row.querySelectorAll('select');
          cubeRoles[cube.value] = chosen.value;
        }
        const elements = {};
        for (const row of elementRows.children) {
          const [dimensionSelect, roleSelect] = row.querySelectorAll('select');
          if (!row.dataset.element) throw invalid(row.querySelector('button'), 'Choose an element for each element role, or take the role away.');
          (elements[dimensionSelect.value] ??= {})[row.dataset.element] = roleSelect.value;
        }
        const body = { role: role.value, cubes: cubeRoles, elements };
        if (password.value) body.password = password.value;
        // An administrator's own leave is kept as it is, for if they are one no longer.
        if (role.value !== 'admin') body.makes_keys = allowed;
        await put(`/admin/users/${part(value)}`, body);
        return value;
      },
    });
    if (!saved) return;
    notify(adding ? `Added ${saved}. They can sign in now.` : `Saved ${saved}. Their open sessions use the change at once.`, 'good');
    this.load();
  }

  /** A user's API keys, each to be revoked: it stops working at once. */
  async showKeys(user) {
    let keys = user.keys;
    const holder = h('div');
    const revoke = async (key) => {
      const revoked = await dialog({
        title: `Revoke ${key.name}?`,
        body: h('p', {}, `A program of ${user.name}’s using it is refused from its next request. It cannot be undone.`),
        confirm: 'Revoke key',
        danger: true,
        onsubmit: async () => {
          await del(`/admin/users/${part(user.name)}/keys/${part(key.id)}`);
          return true;
        },
      });
      if (!revoked) return;
      notify(`Revoked ${user.name}’s key ${key.name}.`, 'good');
      keys = keys.filter((each) => each.id !== key.id);
      draw();
    };
    const draw = () => mount(holder, keys.length ? keyTable(keys, revoke) : h('p', { class: 'muted' }, 'They hold none now.'));
    draw();
    await dialog({ title: `API keys of ${user.name}`, body: holder, confirm: 'Done', cancel: null, wide: true });
    this.load();
  }

  async signOut(user) {
    try {
      const answer = await del(`/admin/users/${part(user.name)}/sessions`);
      notify(`Ended ${answer.ended} session${answer.ended === 1 ? '' : 's'} of ${user.name}.`, 'good');
      this.load();
    } catch (problem) {
      notify(problem.message, 'bad');
    }
  }

  async removeUser(user) {
    const removed = await dialog({
      title: `Remove ${user.name}?`,
      body: h('p', {}, `${user.name} can no longer sign in, and their open sessions end now.`),
      confirm: 'Remove user',
      danger: true,
      onsubmit: async () => {
        await del(`/admin/users/${part(user.name)}`);
        return true;
      },
    });
    if (!removed) return;
    notify(`Removed ${user.name}.`, 'good');
    this.load();
  }

  /* ---- Changes to cells ---- */

  /**
   * The card of changes saved to cells: chosen by cube, by user and by how long ago, the
   * newest first, each to be opened in Explore. It is fetched as it opens and as the choices
   * change, not with the rest of the page every few seconds, the history being long to read.
   */
  buildChanges() {
    const choice = (label) => h('select', { class: 'select small-select', 'aria-label': label, onchange: () => this.loadChanges() }, h('option', { value: '' }, label));
    this.cubeFilter = choice('Every cube');
    this.userFilter = choice('Anyone');
    this.sinceFilter = h('select', { class: 'select small-select', 'aria-label': 'How long ago', onchange: () => this.loadChanges() },
      [['', 'At any time'], ['3600', 'In the last hour'], ['86400', 'In the last day'], ['604800', 'In the last week'], ['2592000', 'In the last 30 days']]
        .map(([value, label]) => h('option', { value }, label)));
    this.changesList = h('div', { class: 'changes' });
    mount(this.changesCard,
      h('div', { class: 'card-head' },
        h('h3', { class: 'grow' }, 'Changes to cells'),
        this.cubeFilter, this.userFilter, this.sinceFilter,
        h('button', { class: 'button small quiet icon', 'aria-label': 'Fetch the changes again', title: 'Fetch them again', onclick: (event) => working(event.currentTarget, () => this.loadChanges()) }, icon('refresh', 14))),
      this.changesList);
    mount(this.changesList, h('div', { class: 'card-body' }, loading('lines')));
  }

  /**
   * Puts the choices given in a filter, after the one that chooses everything, keeping what
   * was chosen; left alone while they are the same, so that the page keeping itself current
   * does not close it under the pointer.
   */
  offer(select, all, names) {
    const listed = names.join('\n');
    if (select.dataset.listed === listed) return;
    select.dataset.listed = listed;
    const chosen = select.value;
    mount(select, h('option', { value: '' }, all), names.map((name) => h('option', { value: name, selected: name === chosen }, name)));
  }

  /** Fetches the changes as the filters ask, the newest 200, and shows them; the last answer fades meanwhile. */
  async loadChanges() {
    const asked = new URLSearchParams({ most: '200' });
    if (this.cubeFilter.value) asked.set('cube', this.cubeFilter.value);
    if (this.userFilter.value) asked.set('user', this.userFilter.value);
    if (this.sinceFilter.value) asked.set('since', String(Math.floor(Date.now() / 1000) - Number(this.sinceFilter.value)));
    const ticket = (this.changesTicket = (this.changesTicket ?? 0) + 1);
    this.changesList.classList.add('busy');
    try {
      const answer = await get(`/admin/changes?${asked}`, { quiet: true });
      if (ticket === this.changesTicket) this.drawChanges(answer.changes);
    } catch (problem) {
      if (ticket === this.changesTicket) mount(this.changesList, h('div', { class: 'card-body' }, h('p', { class: 'error-text' }, problem.message)));
    } finally {
      if (ticket === this.changesTicket) this.changesList.classList.remove('busy');
    }
  }

  drawChanges(changes) {
    if (!changes.length) {
      mount(this.changesList, h('div', { class: 'card-body' }, h('p', { class: 'muted' }, 'No change to a cell has been saved as asked.')));
      return;
    }
    const shown = (value) => (value === 0 || value === '' || value === null || value === undefined
      ? h('span', { class: 'muted' }, 'empty')
      : typeof value === 'number' ? value.toLocaleString(undefined, { maximumFractionDigits: 6 }) : String(value));
    mount(this.changesList,
      h('table', { class: 'table' },
        h('thead', {}, h('tr', {}, h('th', {}, 'When'), h('th', {}, 'Who'), h('th', {}, 'Cube'), h('th', {}, 'Cell'), h('th', { class: 'num-col' }, 'From'), h('th', { class: 'num-col' }, 'To'), h('th', { class: 'actions' }, ''))),
        h('tbody', {}, changes.map((change) => h('tr', {},
          h('td', { class: 'muted nowrap' }, when(change.time)),
          h('td', {}, change.author ?? h('span', { class: 'muted' }, 'Not known')),
          h('td', {}, change.cube),
          h('td', {}, change.at.join(' · ')),
          h('td', { class: 'num-col num' }, shown(change.before)),
          h('td', { class: 'num-col num' }, h('strong', {}, shown(change.after))),
          h('td', { class: 'actions' }, h('button', {
            class: 'button small quiet',
            'aria-label': `Open ${change.at.join(' · ')} of ${change.cube} in Explore`,
            title: 'Open the cell in Explore',
            onclick: () => this.explore(change),
          }, icon('explore', 14), 'Open')))))),
      changes.length === 200 ? h('p', { class: 'hint card-body' }, 'The newest 200. Choose a cube, a user or a time to see others.') : null);
  }

  /** Opens a changed cell in Explore, alone in the grid, to look about from there. */
  async explore(change) {
    const cube = (await model()).cubes.find((each) => each.name === change.cube);
    if (!cube) {
      notify(`There is no cube called ${change.cube} now.`, 'bad');
      return;
    }
    handOver(cube.name, cellQuery(cube.name, cube.dimensions, change.at));
    go(`/explore?cube=${encodeURIComponent(cube.name)}`);
  }

  /* ---- Transactions ---- */

  drawTransactions(open) {
    mount(this.transactionsCard,
      h('div', { class: 'card-head' }, h('h3', { class: 'grow' }, 'Open transactions'),
        h('span', { class: 'muted' }, 'Writes held until they are committed. One left idle is rolled back by the server in time.')),
      open.length
        ? h('table', { class: 'table' },
          h('thead', {}, h('tr', {}, h('th', {}, 'Transaction'), h('th', {}, 'Idle for'), h('th', { class: 'num-col' }, 'Writes waiting'), h('th', { class: 'actions' }, ''))),
          h('tbody', {}, open.map((transaction) => h('tr', {},
            h('td', { class: 'mono' }, `…${String(transaction.transaction).slice(-6)}`),
            h('td', {}, duration(transaction.idle_seconds)),
            h('td', { class: 'num-col num' }, transaction.pending.toLocaleString()),
            h('td', { class: 'actions' }, h('button', {
              class: 'button small quiet danger',
              'aria-label': `Roll back the transaction ending ${String(transaction.transaction).slice(-6)}`,
              onclick: () => this.rollBack(transaction),
            }, icon('undo', 14), 'Roll back'))))))
        : h('div', { class: 'card-body' }, h('p', { class: 'muted' }, 'None. Every write has been committed or rolled back.')));
  }

  async rollBack(transaction) {
    const done = await dialog({
      title: 'Roll the transaction back?',
      body: h('p', {}, `Its ${transaction.pending.toLocaleString()} waiting write${transaction.pending === 1 ? ' is' : 's are'} discarded, and whoever was working in it will be told their changes are gone.`),
      confirm: 'Roll back',
      danger: true,
      onsubmit: async () => {
        await post(`/transactions/${transaction.transaction}/rollback`);
        return true;
      },
    });
    if (!done) return;
    notify('The transaction was rolled back.', 'good');
    this.load();
  }
}
