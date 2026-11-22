// Comments: notes pinned to cells, as planners explain a number beside it; the history of a
// cell's changes, who made each and from what to what; and its supporting detail, the lines
// its value is the sum of. A cell with comments is marked in its corner; all three open in a
// panel beside the grid, which keeps the numbers in view. Methods of the explorer, kept
// apart to keep each part readable.

import { del, get, part, post } from './api.js';
import { loading, state } from './app.js';
import { dialog, formatValue, h, icon, mount, notify, parseNumber, working } from './ui.js';

/** Comments: methods of the explorer. */
export const comments = {
  /**
   * Fetches the cube's comments, each cell's kept by its place; resolves with whether they
   * came. Until they do, the comments fetched before stay.
   */
  async loadComments() {
    const commented = new Map();
    try {
      const found = await get(`/cubes/${part(this.cube.name)}/comments`, { quiet: true });
      for (const comment of found.comments) {
        const key = JSON.stringify(comment.at);
        commented.set(key, [...(commented.get(key) ?? []), comment]);
      }
    } catch {
      // Without them the grid still shows; no cell is marked.
      return false;
    }
    this.commented = commented;
    return true;
  },

  /** Marks the comments on the cells drawn, again, as they now are. */
  markAllComments() {
    for (const tr of this.grid?.rows.values() ?? []) {
      for (const td of tr.querySelectorAll('td[data-r]')) {
        td.classList.remove('commented');
        td.querySelector('.comment-mark')?.remove();
        this.markComments(td, Number(td.dataset.r), Number(td.dataset.c));
      }
    }
  },

  /** The comments on a cell of the grid, the oldest first. */
  commentsAt(r, c) {
    return this.coordinate(r, c) ? this.commented?.get(this.key(r, c)) ?? [] : [];
  },

  /** Marks a cell of the grid that has comments, with a corner to open them by. */
  markComments(td, r, c) {
    const thread = this.commentsAt(r, c);
    if (!thread.length) return;
    td.classList.add('commented');
    td.append(h('button', {
      class: 'comment-mark',
      tabindex: '-1',
      'aria-label': `${thread.length} comment${thread.length === 1 ? '' : 's'}`,
      title: `${thread.at(-1).author}: ${thread.at(-1).text}`,
      onclick: (event) => {
        event.stopPropagation();
        this.openComments(r, c);
      },
    }));
  },

  /** Opens the panel beside the grid on a cell's comments. */
  openComments(r, c) {
    this.openCell(r, c, 'comments');
  },

  /** Opens the panel beside the grid on the history of a cell's changes. */
  openHistory(r, c) {
    this.openCell(r, c, 'history');
  },

  /** Opens the panel beside the grid on a cell: on its comments, its history or its detail. */
  openCell(r, c, tab) {
    const at = this.coordinate(r, c);
    if (!at) {
      notify('A calculated member\'s cells are worked out by the query: they take no comments, keep no history and have no detail.');
      return;
    }
    this.commentCell = { r, c, at, tab };
    this.drawComments(true);
  },

  closeComments() {
    this.commentCell = null;
    this.commentPanel.hidden = true;
    const { r, c } = this.selection?.current ?? { r: 0, c: 0 };
    this.selection?.activate(r, c);
  },

  /**
   * Draws the panel for the cell it is open on: its comments, its history or its detail, by
   * the tab chosen. With `focus`, as the panel opens or the tab changes, the keys go to it;
   * drawn again as the grid is, it leaves them where they are.
   */
  drawComments(focus = false) {
    const cell = this.commentCell;
    if (!cell) {
      this.commentPanel.hidden = true;
      return;
    }
    const tab = (id, label) => h('button', {
      type: 'button',
      role: 'tab',
      'aria-selected': String(cell.tab === id),
      onclick: () => {
        cell.tab = id;
        this.drawComments(true);
      },
    }, label);
    this.commentPanel.hidden = false;
    mount(this.commentPanel,
      h('div', { class: 'comment-head' },
        h('div', { class: 'grow' },
          h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'About the cell' }, tab('comments', 'Comments'), tab('history', 'History'), tab('detail', 'Detail')),
          h('p', { class: 'muted' }, cell.at.join(' · '))),
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Close the panel', onclick: () => this.closeComments() }, icon('close', 16))),
      cell.tab === 'history' ? this.drawHistory(cell) : cell.tab === 'detail' ? this.drawDetail(cell, focus) : this.drawThread(cell));
    if (focus && cell.tab !== 'detail') (this.commentPanel.querySelector('textarea:not([disabled])') ?? this.commentPanel.querySelector('[role="tab"][aria-selected="true"]'))?.focus();
  },

  /**
   * A cell's supporting detail, the lines its value is the sum of, each a label and an
   * amount, to change and save with the value, or to take away. Asked for each time it is
   * drawn, as the edit transaction or the sandbox worked in sees it.
   */
  drawDetail(cell, focus) {
    const body = h('div', { class: 'detail', 'aria-busy': 'true' }, loading('lines'));
    const asked = { cells: [{ cube: this.cube.name, at: cell.at }], transaction: this.tx ?? undefined };
    post('/cells/explain', asked, { quiet: true }).then((answer) => {
      const explained = answer.cells[0]?.cell;
      if (!explained) throw new Error(answer.cells[0]?.error?.message ?? 'The cell cannot be told about.');
      mount(body, this.detailEditor(cell, explained));
      if (focus) (body.querySelector('input:not([disabled])') ?? this.commentPanel.querySelector('[role="tab"][aria-selected="true"]'))?.focus();
    }).catch((problem) => {
      mount(body, h('p', { class: 'error-text comment-none' }, problem.message));
    }).finally(() => body.removeAttribute('aria-busy'));
    return body;
  },

  /** The lines of a cell's detail as fields, with their total and the ways to save or take them away. */
  detailEditor(cell, explained) {
    const kept = explained.detail ?? [];
    const writable = this.mayWrite() && explained.kind === 'leaf' && explained.writable;
    const format = (value) => formatValue(value, this.layout.format);
    if (!writable) {
      return [
        kept.length
          ? h('ol', { class: 'detail-read' }, kept.map((line) => h('li', {}, h('span', { class: 'grow' }, line.label || '—'), h('span', { class: 'num' }, format(line.value)))))
          : h('p', { class: 'muted comment-none' }, 'No detail.'),
        h('p', { class: 'hint' }, this.mayWrite()
          ? 'Only a number that can be written has detail: this cell is a total, text, or worked out by a rule.'
          : 'Your role reads this cube; it takes a writer to give a cell detail.'),
      ];
    }
    const list = h('ol', { class: 'detail-lines' });
    const total = h('strong', { class: 'num' });
    const row = (line = { label: '', value: '' }) => {
      const label = h('input', { class: 'input', value: line.label, placeholder: 'What it is', 'aria-label': 'What the line is', maxlength: 200 });
      const amount = h('input', { class: 'input num', value: line.value === '' ? '' : String(line.value), inputmode: 'decimal', placeholder: '0', 'aria-label': 'Its amount' });
      amount.addEventListener('input', sum);
      const item = h('li', {}, label, amount,
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Take the line away', onclick: () => { item.remove(); sum(); } }, icon('close', 14)));
      return item;
    };
    // The lines as typed: each a label and a number, blank ones left out; null where an
    // amount is not a number.
    const read = () => {
      const lines = [];
      for (const item of list.children) {
        const [label, amount] = item.querySelectorAll('input');
        if (!label.value.trim() && !amount.value.trim()) continue;
        const value = amount.value.trim() === '' ? 0 : parseNumber(amount.value);
        if (value === null) return null;
        lines.push({ label: label.value.trim(), value });
      }
      return lines;
    };
    function sum() {
      const lines = read();
      total.textContent = lines ? format(lines.reduce((all, line) => all + line.value, 0)) : '—';
    }
    list.append(...(kept.length ? kept : [undefined, undefined]).map(row));
    sum();
    const save = h('button', {
      class: 'button small primary',
      type: 'button',
      onclick: () => {
        const lines = read();
        if (lines?.length === 0) {
          notify(kept.length ? 'To take the detail away, choose Take it away.' : 'Give the detail a line or more.', 'bad');
          return;
        }
        working(save, () => this.saveDetail(cell, lines));
      },
    }, 'Save the detail');
    const add = () => {
      const added = row();
      list.append(added);
      added.querySelector('input').focus();
    };
    return [
      list,
      h('div', { class: 'toolbar' },
        h('button', { class: 'button small', type: 'button', onclick: add }, icon('plus', 14), 'Add a line'),
        h('span', { class: 'grow' }),
        h('span', { class: 'muted' }, 'Total'), total),
      h('div', { class: 'toolbar' },
        kept.length ? h('button', { class: 'button small', type: 'button', onclick: (event) => working(event.currentTarget, () => this.saveDetail(cell, [])) }, 'Take it away') : null,
        h('span', { class: 'grow' }),
        save),
      h('p', { class: 'hint' }, 'The cell takes the lines\' total. A value typed into the cell instead takes the detail away.'),
    ];
  },

  /**
   * Writes the lines given as the cell's detail, and their total to it, as a change of
   * values; no lines take the detail away and leave the value.
   */
  async saveDetail(cell, lines) {
    if (lines === null) {
      notify('Give each line an amount that is a number.', 'bad');
      return;
    }
    const label = lines.length ? `the detail of ${cell.at.join(' · ')}` : `taking away the detail of ${cell.at.join(' · ')}`;
    await this.changeValues(label, async () => {
      await post('/cells/detail', { transaction: this.tx, cube: this.cube.name, at: cell.at, lines });
      this.changes = (this.changes ?? 0) + 1;
      if (!this.sandbox && lines.length) this.edits.set(JSON.stringify(cell.at), lines.reduce((all, line) => all + line.value, 0));
      return lines.length ? `The cell is now the total of its ${lines.length} line${lines.length === 1 ? '' : 's'}.` : 'Took the detail away; the value stays.';
    }, false, [cell.r, cell.c]);
  },

  /** A cell's comments, the oldest first, and the box to add one. */
  drawThread(cell) {
    const thread = this.commented?.get(JSON.stringify(cell.at)) ?? [];
    const me = state.user?.name ?? '';
    const admin = state.user?.role === 'admin';
    const mayWrite = this.mayWrite();
    const text = h('textarea', { class: 'textarea', rows: 3, maxlength: 10000, placeholder: 'Say what explains the number', 'aria-label': 'A new comment', disabled: !mayWrite });
    const add = h('button', { class: 'button small primary', type: 'button', disabled: !mayWrite, onclick: () => working(add, () => this.addComment(text.value)) }, 'Comment');
    text.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        working(add, () => this.addComment(text.value));
      } else if (event.key === 'Escape') {
        event.preventDefault();
        this.closeComments();
      }
    });
    return [
      h('ol', { class: 'comment-thread' }, thread.length
        ? thread.map((comment) => h('li', { class: 'comment' },
          h('div', { class: 'comment-by' },
            h('strong', {}, comment.author),
            h('span', { class: 'muted' }, new Date(comment.written * 1000).toLocaleString()),
            comment.author.toLowerCase() === me.toLowerCase() || admin
              ? h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Take the comment away', onclick: () => this.removeComment(comment) }, icon('trash', 14))
              : null),
          h('p', {}, comment.text)))
        : h('li', { class: 'muted comment-none' }, 'No comments yet.')),
      h('div', { class: 'comment-new' }, text,
        h('div', { class: 'toolbar' }, h('span', { class: 'hint grow' }, mayWrite ? 'Ctrl Enter adds it.' : 'Your role reads this cube; it takes a writer to comment.'), add)),
    ];
  },

  /**
   * The changes saved to a cell, the newest first: who made each, when, and from what to
   * what; and first, a change made here and not yet saved. Fetched each time it is drawn,
   * its outline standing in meanwhile.
   */
  drawHistory(cell) {
    const list = h('ol', { class: 'comment-thread', 'aria-busy': 'true' }, h('li', {}, loading('lines')));
    const shown = (value) => (value === null || value === undefined || value === 0 || value === ''
      ? h('span', { class: 'muted' }, 'empty')
      : formatValue(value, this.layout.format));
    post('/cells/history', { cube: this.cube.name, at: cell.at }, { quiet: true }).then((answer) => {
      mount(list, answer.changes.length
        ? answer.changes.map((change) => h('li', { class: 'comment' },
          h('div', { class: 'comment-by' },
            h('strong', {}, change.author ?? 'Not known'),
            h('span', { class: 'muted' }, new Date(change.time * 1000).toLocaleString())),
          h('p', { class: 'history-change' }, shown(change.before), ' → ', h('strong', {}, shown(change.after)))))
        : h('li', { class: 'muted comment-none' }, 'No change has been saved to this cell.'));
    }, (problem) => {
      mount(list, h('li', { class: 'error-text comment-none' }, problem.message));
    }).finally(() => list.removeAttribute('aria-busy'));
    const pending = this.edits.get(JSON.stringify(cell.at));
    return [
      pending === undefined ? null : h('p', { class: 'history-pending' }, icon('edit', 14), 'Changed here to ', h('strong', {}, shown(pending)), ', not yet saved.'),
      list,
    ];
  },

  /** Pins a comment to the cell the panel is open on. */
  async addComment(text) {
    const cell = this.commentCell;
    if (!cell || !text.trim()) return;
    try {
      await post(`/cubes/${part(this.cube.name)}/comments`, { at: cell.at, text });
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    await this.loadComments();
    this.drawComments(true);
    this.markAllComments();
  },

  /** Takes a comment away, once asked. */
  async removeComment(comment) {
    const agreed = await dialog({
      title: 'Take the comment away?',
      body: h('p', {}, `“${comment.text.length > 120 ? `${comment.text.slice(0, 119)}…` : comment.text}”, by ${comment.author}. It goes for everyone.`),
      confirm: 'Take it away',
      danger: true,
    });
    if (!agreed) return;
    try {
      await del(`/cubes/${part(this.cube.name)}/comments/${comment.id}`);
    } catch (problem) {
      notify(problem.message, 'bad');
      return;
    }
    await this.loadComments();
    this.drawComments(true);
    this.markAllComments();
  },
};
