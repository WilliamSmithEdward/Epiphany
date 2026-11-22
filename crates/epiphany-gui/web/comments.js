// Comments: notes pinned to cells, as planners explain a number beside it; and the history
// of a cell's changes, who made each and from what to what. A cell with comments is marked
// in its corner; both open in a panel beside the grid, which keeps the numbers in view.
// Methods of the explorer, kept apart to keep each part readable.

import { del, get, part, post } from './api.js';
import { loading, state } from './app.js';
import { dialog, formatValue, h, icon, mount, notify, working } from './ui.js';

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

  /** Opens the panel beside the grid on a cell: on its comments, or on its history. */
  openCell(r, c, tab) {
    const at = this.coordinate(r, c);
    if (!at) {
      notify('A calculated member\'s cells are worked out by the query: they take no comments, and keep no history.');
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
   * Draws the panel for the cell it is open on: its comments, or its history, by the tab
   * chosen. With `focus`, as the panel opens or the tab changes, the keys go to it; drawn
   * again as the grid is, it leaves them where they are.
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
          h('div', { class: 'segmented', role: 'tablist', 'aria-label': 'About the cell' }, tab('comments', 'Comments'), tab('history', 'History')),
          h('p', { class: 'muted' }, cell.at.join(' · '))),
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Close the panel', onclick: () => this.closeComments() }, icon('close', 16))),
      cell.tab === 'history' ? this.drawHistory(cell) : this.drawThread(cell));
    if (focus) (this.commentPanel.querySelector('textarea:not([disabled])') ?? this.commentPanel.querySelector('[role="tab"][aria-selected="true"]'))?.focus();
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
