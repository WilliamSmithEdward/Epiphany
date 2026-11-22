// An accessibility check of the page as it stands, run in the browser: what a keyboard or a
// screen reader would miss, and text too faint to read. Self-contained, so that it can be
// evaluated in any page; it gives the problems it finds, each said once with where it is
// first seen and how often.

(() => {
  const found = new Map();
  const problem = (what, element) => {
    const seen = found.get(what) ?? { what, where: describe(element), count: 0 };
    seen.count += 1;
    found.set(what, seen);
  };
  function describe(element) {
    if (!element) return 'the page';
    const classes = typeof element.className === 'string' ? element.className.trim().split(/\s+/).filter(Boolean).slice(0, 2) : [];
    const text = (element.textContent ?? '').trim().slice(0, 30);
    return `${element.tagName.toLowerCase()}${element.id ? `#${element.id}` : ''}${classes.map((name) => `.${name}`).join('')}${text ? ` “${text}”` : ''}`;
  }
  const shown = (element) => {
    if (!element.isConnected || element.closest('[hidden], [aria-hidden="true"], [inert]')) return false;
    const box = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return box.width > 0 && box.height > 0 && style.visibility !== 'hidden';
  };

  // Every control the keys reach has a name to be told by.
  const name = (element) => {
    if (element.getAttribute('aria-label')?.trim()) return 'label';
    const by = element.getAttribute('aria-labelledby');
    if (by && by.split(/\s+/).some((id) => document.getElementById(id)?.textContent.trim())) return 'label';
    if (element.labels && [...element.labels].some((label) => label.textContent.trim())) return 'label';
    if (!['INPUT', 'SELECT', 'TEXTAREA'].includes(element.tagName) && element.textContent.trim()) return 'text';
    if (element.querySelector?.('img[alt]:not([alt=""])')) return 'text';
    if (element.tagName === 'SELECT' && element.selectedOptions[0]?.textContent.trim() && element.closest('label')) return 'label';
    if (element.title?.trim()) return 'title';
    if (element.placeholder?.trim()) return 'placeholder';
    return null;
  };
  const controls = 'button, a[href], input:not([type="hidden"]), select, textarea, [role="button"], [role="tab"], [role="radio"], [role="menuitem"], [role="option"], [role="treeitem"], [role="checkbox"], [role="switch"]';
  for (const element of document.querySelectorAll(controls)) {
    if (!shown(element)) continue;
    const named = name(element);
    if (!named) problem('A control has no name', element);
    else if (named === 'placeholder') problem('A field is named only by its placeholder', element);
  }
  for (const element of document.querySelectorAll('[tabindex]')) {
    if (Number(element.getAttribute('tabindex')) > 0) problem('An element sets a tab order of its own (tabindex above 0)', element);
  }
  for (const element of document.querySelectorAll('[aria-hidden="true"]')) {
    if (element.matches(controls) || element.querySelector('button, a[href], input, select, textarea, [tabindex="0"]')) {
      problem('Something hidden from screen readers can still take the keys', element);
    }
  }
  for (const image of document.querySelectorAll('img:not([alt])')) problem('An image has no alt text', image);
  // Asked for no motion, nothing takes a moment to change: a transition, however short, draws
  // its first frame from where it began.
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) {
    for (const element of document.querySelectorAll('*')) {
      if (getComputedStyle(element).transitionDuration.split(',').some((duration) => parseFloat(duration) > 0)) {
        problem('Something moves though motion is asked to be reduced', element);
      }
    }
  }
  // A row of tabs or choices is one stop for Tab, its arrow keys going along it.
  for (const row of document.querySelectorAll('[role="tablist"], [role="radiogroup"]')) {
    const stops = [...row.querySelectorAll('[role="tab"], [role="radio"]')].filter((element) => element.tabIndex >= 0);
    if (stops.length !== 1) problem('A row of tabs or choices is not one stop for Tab', row);
  }

  // Ids are each given once.
  const ids = new Map();
  for (const element of document.querySelectorAll('[id]')) ids.set(element.id, (ids.get(element.id) ?? 0) + 1);
  for (const [id, count] of ids) if (count > 1) problem(`The id “${id}” is given ${count} times`, document.getElementById(id));

  // The page has its landmarks and a heading to begin it.
  if (document.querySelectorAll('main, [role="main"]').length !== 1) problem('The page has no single main landmark', null);
  if (!document.querySelector('h1') && !document.querySelector('dialog[open]')) problem('The page has no top heading', null);
  for (const dialog of document.querySelectorAll('dialog[open]')) {
    if (!name(dialog)) problem('A dialog has no name', dialog);
  }

  // Text stands out from what is behind it: 4.5 to 1, or 3 to 1 where it is large.
  const canvas = document.createElement('canvas');
  canvas.width = canvas.height = 1;
  const paint = canvas.getContext('2d', { willReadFrequently: true });
  const rgba = (color) => {
    paint.clearRect(0, 0, 1, 1);
    paint.fillStyle = '#000';
    paint.fillStyle = color;
    paint.fillRect(0, 0, 1, 1);
    const [r, g, b, a] = paint.getImageData(0, 0, 1, 1).data;
    return [r, g, b, a / 255];
  };
  const behind = (element) => {
    for (let at = element; at; at = at.parentElement) {
      const style = getComputedStyle(at);
      if (style.backgroundImage !== 'none' && !at.matches('.select, td, .data-bar')) return null;
      const color = rgba(style.backgroundColor);
      if (color[3] > 0.9) return color;
    }
    return rgba(getComputedStyle(document.body).backgroundColor);
  };
  const luminance = ([r, g, b]) => {
    const channel = (value) => {
      const c = value / 255;
      return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
  };
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {
    acceptNode: (node) => (node.textContent.trim() ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT),
  });
  const checked = new Set();
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const element = node.parentElement;
    if (!element || checked.has(element) || !shown(element) || element.closest('button:disabled, [aria-disabled="true"], option, del')) continue;
    checked.add(element);
    const style = getComputedStyle(element);
    // Text in a drawing is painted with its fill, a colour or else a pattern not judged here.
    const color = element instanceof SVGElement ? style.fill : style.color;
    const ground = behind(element);
    if (!ground || !color.startsWith('rgb')) continue;
    const [r, g, b, a] = rgba(color);
    // A colour seen through is blended with what is behind it.
    const ink = [r, g, b].map((value, index) => value * a * Number(style.opacity) + ground[index] * (1 - a * Number(style.opacity)));
    const [lighter, darker] = [luminance(ink), luminance(ground)].sort((x, y) => y - x);
    const ratio = (lighter + 0.05) / (darker + 0.05);
    const size = parseFloat(style.fontSize);
    const large = size >= 24 || (size >= 18.66 && Number(style.fontWeight) >= 700);
    if (ratio < (large ? 3 : 4.5)) problem(`Text stands out ${ratio.toFixed(2)} to 1 from what is behind it (${color} on rgb(${ground.slice(0, 3).join(', ')}), ${size}px)`, element);
  }
  return [...found.values()];
})();
