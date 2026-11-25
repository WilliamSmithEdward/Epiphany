// Pages still being built.

import { empty } from './app.js';
import { h, mount } from './ui.js';

export function render(target, { title }) {
  mount(target, h('div', { class: 'page' },
    h('div', { class: 'page-head' }, h('div', { class: 'grow' }, h('h1', {}, title))),
    h('div', { class: 'card' }, empty('info', `${title} is on its way`, 'This part of Epiphany is being built. Everything it will do is already possible through the API.'))));
}
