// The keys the application takes, listed where `?` or the menus ask for them.

import { dialog, h, MOD } from './ui.js';

/**
 * The keys, by where they work: each line its keys, as alternatives that each name the keys
 * pressed together, and what they do.
 */
const KEYS = [
  ['Everywhere', [
    [[`${MOD}+K`, '/'], 'Search cubes, pages and commands'],
    [['?'], 'These keys'],
    [['Tab'], 'Its first press offers a way past the bar, straight to the page'],
    [['Esc'], 'Close a dialog or a menu'],
  ]],
  ['The grid', [
    [['←', '↑', '→', '↓'], 'Move; past the first row or column, on into the headings'],
    [['Page Up', 'Page Down'], 'Move by the rows in view'],
    [['Home', 'End'], 'To the row\'s first or last cell'],
    [[`${MOD}+Home`, `${MOD}+End`], 'To the grid\'s first or last cell'],
    [['Shift+←', 'Shift+→'], 'Stretch the choice of cells, as with any arrow'],
    [['Shift+Space'], 'Choose the row'],
    [[`${MOD}+Space`], 'Choose the column'],
    [[`${MOD}+A`], 'Choose every cell'],
    [['Enter', 'F2'], 'Write into the cell, or begin to by typing; into a total, a value or a code to spread'],
    [['Enter', 'Shift+Enter', 'Tab', 'Shift+Tab'], 'While writing: keep it and move down, up, right or left'],
    [['Esc'], 'While writing: leave the cell as it was'],
    [['Delete'], 'Empty the chosen cells'],
    [[`${MOD}+C`, `${MOD}+V`], 'Copy and paste, as a spreadsheet does'],
    [[`${MOD}+Z`, `${MOD}+Y`], 'Take a change back, and make it again'],
    [[`${MOD}+F`], 'Find among the headings; Enter and Shift+Enter go on and back'],
    [['Shift+F2'], 'The cell\'s comments, history and detail'],
    [['Shift+F10', 'Menu'], 'The menu of the cell or the heading'],
  ]],
  ['A heading of the grid', [
    [['Enter', 'Space', '+', '-'], 'Open or close its member'],
    [['*'], 'Open everything beneath it'],
  ]],
  ['A tree of members', [
    [['↑', '↓', 'Page Up', 'Page Down', 'Home', 'End'], 'Move'],
    [['→'], 'Open the member, or go to its first child'],
    [['←'], 'Close the member, or go to its parent'],
    [['*'], 'Open the member and those beside it'],
    [['a … z'], 'Go to the next member whose name begins so'],
  ]],
  ['The MDX editor', [
    [[`${MOD}+Enter`], 'Run the query'],
  ]],
];

/** One set of keys pressed together, each key in its own cap. */
function together(keys) {
  const caps = keys === '+' ? ['+'] : keys.split('+');
  return h('span', { class: 'keys' }, caps.flatMap((cap, index) => [index ? '+' : null, h('kbd', {}, cap)]));
}

/** Lists the keys the application takes, by where they work. */
export function showShortcuts() {
  if (document.querySelector('dialog[aria-label="Keyboard shortcuts"]')) return;
  const sections = KEYS.map(([where, lines]) => h('section', { class: 'shortcuts-section' },
    h('h3', {}, where),
    h('dl', { class: 'shortcuts' }, lines.flatMap(([alternatives, what]) => [
      h('dt', {}, alternatives.flatMap((keys, index) => [index ? h('span', { class: 'or' }, ' ') : null, together(keys)])),
      h('dd', {}, what),
    ]))));
  return dialog({ title: 'Keyboard shortcuts', body: h('div', { class: 'shortcuts-body' }, sections), confirm: 'Done', cancel: null });
}
