// Administration: the server's state, its users and their roles, and open transactions.

import { del, get, part, post, put } from './api.js';
import { empty, failure, loading, model, state } from './app.js';
import { dialog, h, icon, mount, notify, working } from './ui.js';

const ROLES = { admin: 'Administrator', write: 'Can write', read: 'Can read', none: 'No access' };

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
    this.transactionsCard = h('section', { class: 'card fit', 'aria-label': 'Open transactions' });
    this.updated = h('span', { class: 'muted' });
    mount(target, h('div', { class: 'page' },
      h('div', { class: 'page-head' },
        h('div', { class: 'grow' }, h('h1', {}, 'Administration'), h('p', {}, 'The server, its users and the work in progress on it.')),
        this.updated,
        h('button', { class: 'button', onclick: (event) => working(event.currentTarget, () => this.load()) }, icon('refresh', 16), 'Refresh')),
      this.statusCard,
      this.usersCard,
      this.transactionsCard));
    // Until the first answer, each part holds its outline.
    mount(this.statusCard, loading('cards'));
    mount(this.usersCard, h('div', { class: 'card-body' }, loading('lines')));
    mount(this.transactionsCard, h('div', { class: 'card-body' }, loading('lines')));
    this.load();
    // Kept current while the page is open.
    this.timer = setInterval(() => {
      if (!target.isConnected) clearInterval(this.timer);
      else if (!document.hidden && !document.querySelector('dialog[open]')) this.load(true);
    }, 15000);
  }

  /** Fetches the page's parts; `quietly`, as the page keeps itself current, without a word. */
  async load(quietly = false) {
    try {
      const [status, users, transactions] = await Promise.all([
        get('/admin/status', { quiet: quietly }),
        get('/admin/users', { quiet: quietly }),
        get('/admin/transactions', { quiet: quietly }),
      ]);
      this.drawStatus(status);
      this.drawUsers(users);
      this.drawTransactions(transactions);
      this.usersCard.hidden = false;
      this.transactionsCard.hidden = false;
      this.updated.textContent = `Updated ${new Date().toLocaleTimeString()}`;
    } catch (problem) {
      if (quietly) return;
      // What could not be fetched is not shown as if it were coming.
      this.usersCard.hidden = true;
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
        tile('On disk', bytes((status.log_bytes ?? 0) + (status.snapshot_bytes ?? 0)), 'save', `Log ${bytes(status.log_bytes)} · snapshot ${bytes(status.snapshot_bytes)}`),
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
        h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Role'), h('th', {}, 'On particular cubes'), h('th', { class: 'num-col' }, 'Sessions'), h('th', { class: 'actions' }, ''))),
        h('tbody', {}, list.users.map((user) => {
          const cubes = Object.entries(user.cubes);
          return h('tr', {},
            h('td', {}, h('strong', {}, user.name), user.name.toLowerCase() === me ? h('span', { class: 'badge accent you' }, 'You') : null),
            h('td', {}, h('span', { class: `badge ${user.role === 'admin' ? 'accent' : user.role === 'none' ? 'bad' : ''}` }, ROLES[user.role])),
            h('td', { class: 'muted' }, cubes.length ? cubes.map(([cube, role]) => `${cube}: ${ROLES[role].toLowerCase()}`).join(' · ') : '—'),
            h('td', { class: 'num-col num' }, user.sessions.toLocaleString()),
            h('td', { class: 'actions' },
              h('button', { class: 'button small quiet', onclick: (event) => this.editUser(user, event.currentTarget) }, icon('edit', 14), 'Edit'),
              h('button', { class: 'button small quiet', disabled: !user.sessions, onclick: (event) => working(event.currentTarget, () => this.signOut(user)) }, icon('out', 14), 'Sign out'),
              h('button', { class: 'button small quiet danger', onclick: () => this.removeUser(user) }, icon('trash', 14), 'Remove')));
        }))));
  }

  /**
   * Adds a user, or edits one: their password, role and roles on particular cubes. The
   * button that asked shows it at work while the cubes are fetched.
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
    const rows = h('div', { class: 'cube-roles' });
    const addRow = (cube = cubes[0], chosen = 'read') => {
      const cubeSelect = h('select', { class: 'select', 'aria-label': 'Cube' }, cubes.map((name) => h('option', { value: name, selected: name === cube }, name)));
      const roleSelect = h('select', { class: 'select', 'aria-label': 'Role on the cube' }, Object.entries(ROLES).map(([value, label]) => h('option', { value, selected: value === chosen }, label)));
      const row = h('div', { class: 'cube-role' }, cubeSelect, roleSelect,
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Remove this cube role', onclick: () => row.remove() }, icon('close', 14)));
      rows.append(row);
    };
    for (const [cube, chosen] of Object.entries(user?.cubes ?? {})) addRow(cube, chosen);
    const saved = await dialog({
      title: adding ? 'Add a user' : `Edit ${user.name}`,
      confirm: adding ? 'Add user' : 'Save',
      body: h('div', { class: 'facts' },
        h('div', { class: 'field' }, h('label', { for: 'user-name' }, 'Name'), name),
        h('div', { class: 'field-pair' },
          h('div', { class: 'field' }, h('label', { for: 'user-password' }, adding ? 'Password' : 'New password'), password),
          h('div', { class: 'field' }, h('label', { for: 'user-again' }, 'The same again'), again)),
        adding ? null : h('p', { class: 'hint' }, 'Leave the password empty to keep the one they have.'),
        h('div', { class: 'field' }, h('label', { for: 'user-role' }, 'Role'), role,
          h('span', { class: 'hint' }, 'What they may do on every cube, unless a cube below says otherwise. Only an administrator may change the model or manage users.')),
        h('div', { class: 'field' },
          h('label', {}, 'On particular cubes'),
          rows,
          cubes.length ? h('div', {}, h('button', { class: 'button small', type: 'button', onclick: () => addRow() }, icon('plus', 14), 'Add a cube role')) : null)),
      onsubmit: async () => {
        const value = name.value.trim();
        if (!value) throw new Error('Give the user a name.');
        if (password.value !== again.value) throw new Error('The two passwords differ.');
        if (adding && !password.value) throw new Error('A new user needs a password.');
        const cubeRoles = {};
        for (const row of rows.children) {
          const [cube, chosen] = row.querySelectorAll('select');
          cubeRoles[cube.value] = chosen.value;
        }
        const body = { role: role.value, cubes: cubeRoles };
        if (password.value) body.password = password.value;
        await put(`/admin/users/${part(value)}`, body);
        return value;
      },
    });
    if (!saved) return;
    notify(adding ? `Added ${saved}. They can sign in now.` : `Saved ${saved}. Their open sessions use the change at once.`, 'good');
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
            h('td', { class: 'actions' }, h('button', { class: 'button small quiet danger', onclick: () => this.rollBack(transaction) }, icon('undo', 14), 'Roll back'))))))
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
