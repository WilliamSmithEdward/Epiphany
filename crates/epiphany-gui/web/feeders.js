// The feeder check, asked for from Explore and from the model's editor alike.

import { get, part } from './api.js';
import { counted, dialog, h, notify, working } from './ui.js';

/**
 * Looks for cells of a cube that rules calculate and hold a value, but that no feeder feeds,
 * and shows every one the check gives; `button` shows it at work. The server's check stops at
 * each hundred it finds: Find more goes on from there, until every cell has been looked at.
 */
export async function checkFeeders(cube, button) {
  const check = (after) => get(`/cubes/${part(cube)}/feeders/check${after ? `?after=${encodeURIComponent(after)}` : ''}`);
  let found;
  try {
    found = await working(button, () => check(null));
  } catch (problem) {
    notify(problem.message, 'bad');
    return;
  }
  if (!found) return;
  if (!found.unfed.length && !found.next) {
    notify(`Every cell of ${cube} that a rule calculates and holds a value is fed.`, 'good');
    return;
  }
  // Every cell the check gives, the dialog scrolling where they are many.
  const list = h('ul', { class: 'plain-list mono' });
  const said = h('p', { class: 'hint', role: 'status' });
  const more = h('button', { class: 'button small', type: 'button' }, 'Find more');
  let count = 0;
  let next = null;
  const shown = (part) => {
    list.append(...part.unfed.map((cell) => h('li', {}, cell.at.join(' · '))));
    count += part.unfed.length;
    next = part.next ?? null;
    more.hidden = !next;
    said.textContent = next
      ? `${count.toLocaleString()} found so far: the check stops at each hundred it finds.`
      : `${counted(count, 'cell')} in all.`;
  };
  more.addEventListener('click', async () => {
    try {
      const part = await working(more, () => check(next));
      if (part) shown(part);
    } catch (problem) {
      notify(problem.message, 'bad');
    }
  });
  shown(found);
  await dialog({
    title: 'Cells not fed',
    confirm: 'Close',
    cancel: null,
    body: h('div', { class: 'facts' },
      h('p', { class: 'muted' }, 'These cells hold a value a rule works out, but no feeder feeds them, so the totals above them leave them out. A feeder that names them, or something beneath them, brings them in.'),
      list,
      h('div', { class: 'toolbar' }, said, more)),
  });
}
