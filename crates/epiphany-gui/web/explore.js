// Exploring a cube. Replaced by the pivot explorer in stage G3.

import { empty } from './app.js';
import { h, mount } from './ui.js';

export function render(target) {
  mount(target, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', { class: 'grow' }, h('h1', {}, 'Explore'))),
    h('div', { class: 'card' }, empty('explore', 'The explorer is on its way', 'Choose a cube from the overview for now.'))));
}
