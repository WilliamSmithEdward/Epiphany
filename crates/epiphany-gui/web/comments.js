// Comments: notes pinned to cells, as planners explain a number beside it. A cell with any
// is marked in its corner; its thread opens in a panel beside the grid, which keeps the
// numbers in view. Methods of the explorer, kept apart to keep each part readable.

import { del, get, part, post } from './api.js';
import { state } from './app.js';
import { dialog, h, icon, mount, notify } from './ui.js';

/** Comments: methods of the explorer. */
export const comments = {
  /** Fetches the cube's comments, each cell's kept by its place. */
  async loadComments() {
    this.commented = new Map();
    try {
      const found = await get(`/cubes/${part(this.cube.name)}/comments`);
      for (const comment of found.comments) {
        const key = JSON.stringify(comment.at);
        this.commented.set(key, [...(this.commented.get(key) ?? []), comment]);
      }
    } catch {
      // Without them the grid still shows; no cell is marked.
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

  /** Opens the panel of a cell's comments beside the grid. */
  openComments(r, c) {
    const at = this.coordinate(r, c);
    if (!at) {
      notify('A calculated member\'s cells take no comments.');
      return;
    }
    this.commentCell = { r, c, at };
    this.drawComments();
  },

  closeComments() {
    this.commentCell = null;
    this.commentPanel.hidden = true;
    this.output.querySelector('td[tabindex="0"]')?.focus();
  },

  /** Draws the panel of comments for the cell it is open on. */
  drawComments() {
    const cell = this.commentCell;
    if (!cell) {
      this.commentPanel.hidden = true;
      return;
    }
    const thread = this.commented?.get(JSON.stringify(cell.at)) ?? [];
    const me = state.user?.name ?? '';
    const admin = state.user?.role === 'admin';
    const mayWrite = this.mayWrite();
    const text = h('textarea', { class: 'textarea', rows: 3, maxlength: 10000, placeholder: 'Say what explains the number', 'aria-label': 'A new comment', disabled: !mayWrite });
    const add = h('button', { class: 'button small primary', type: 'button', disabled: !mayWrite, onclick: () => this.addComment(text.value) }, 'Comment');
    text.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        this.addComment(text.value);
      } else if (event.key === 'Escape') {
        event.preventDefault();
        this.closeComments();
      }
    });
    this.commentPanel.hidden = false;
    mount(this.commentPanel,
      h('div', { class: 'comment-head' },
        h('div', { class: 'grow' }, h('h3', {}, 'Comments'), h('p', { class: 'muted' }, cell.at.join(' · '))),
        h('button', { class: 'button quiet small icon', type: 'button', 'aria-label': 'Close the comments', onclick: () => this.closeComments() }, icon('close', 16))),
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
        h('div', { class: 'toolbar' }, h('span', { class: 'hint grow' }, mayWrite ? 'Ctrl Enter adds it.' : 'Your role reads this cube; it takes a writer to comment.'), add)));
    if (mayWrite) text.focus();
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
    this.drawComments();
    this.draw();
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
    this.drawComments();
    this.draw();
  },
};
