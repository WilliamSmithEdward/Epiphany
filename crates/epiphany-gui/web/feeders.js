// The feeder check, asked for from Explore and from the model's editor alike.

import { get, part } from './api.js';
import { dialog, h, notify, working } from './ui.js';

/** The most cells the server's check gives, stopping at the first it finds. */
const MOST = 100;

/**
 * Looks for cells of a cube that rules calculate and hold a value, but that no feeder feeds,
 * and shows every one the check gives; `button` shows it at work.
 */
export async function checkFeeders(cube, button) {
  let found;
  try {
    found = await working(button, () => get(`/cubes/${part(cube)}/feeders/check`));
  } catch (problem) {
    notify(problem.message, 'bad');
    return;
  }
  if (!found) return;
  const { unfed } = found;
  if (!unfed.length) {
    notify(`Every cell of ${cube} that a rule calculates and holds a value is fed.`, 'good');
    return;
  }
  const more = unfed.length >= MOST;
  await dialog({
    title: `${unfed.length.toLocaleString()}${more ? ' or more' : ''} cell${unfed.length === 1 ? '' : 's'} not fed`,
    confirm: 'Close',
    cancel: null,
    body: h('div', { class: 'facts' },
      h('p', { class: 'muted' }, 'These cells hold a value a rule works out, but no feeder feeds them, so the totals above them leave them out. A feeder that names them, or something beneath them, brings them in.'),
      // Every cell the check gave, the dialog scrolling where they are many.
      h('ul', { class: 'plain-list mono' }, unfed.map((cell) => h('li', {}, cell.at.join(' · ')))),
      more ? h('p', { class: 'hint' }, 'The check stops at the first hundred it finds: once these are fed, check again for any others.') : null),
  });
}
