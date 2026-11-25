// Laying a dashboard's tiles out: where each stands, in columns across and rows down, and how
// the others make way as one is moved or made larger. A place is `{ x, y, w, h }`, changed
// where it is; nothing here touches the page.

/** Columns across a dashboard's page. */
export const COLUMNS = 12;

/** Whether two places share any ground; touching edges share none. */
export function overlaps(a, b) {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/** The places in the order they are read: by row, then across. */
export function inReadingOrder(places) {
  return [...places].sort((a, b) => a.y - b.y || a.x - b.x);
}

/** A place kept within the page: no wider than it, no further left or up than its edges. */
export function onPage(place) {
  const w = Math.min(Math.max(place.w, 1), COLUMNS);
  return { ...place, w, x: Math.min(Math.max(place.x, 0), COLUMNS - w), y: Math.max(place.y, 0) };
}

/** Whether `place` would stand at `at` within the page, clear of every other place. */
function fits(places, place, at) {
  const there = { ...place, ...at };
  return there.x >= 0 && there.y >= 0 && there.x + there.w <= COLUMNS && !places.some((other) => other !== place && overlaps(other, there));
}

/**
 * Settles a grid: in reading order, each place rises as far as it can without coming onto
 * one settled before it, so that no row is left empty above a tile that could stand in it.
 */
export function settle(places) {
  const settled = [];
  for (const place of inReadingOrder(places)) {
    while (place.y > 0 && !settled.some((other) => overlaps(other, { ...place, y: place.y - 1 }))) place.y -= 1;
    while (settled.some((other) => overlaps(other, place))) place.y += 1;
    settled.push(place);
  }
  return places;
}

/** Moves `place` down to `y`, and whatever it comes onto further down, out of its way; `keep` stays. */
function pushDown(places, place, y, keep) {
  place.y = y;
  for (const other of inReadingOrder(places)) {
    if (other !== place && other !== keep && overlaps(other, place)) pushDown(places, other, place.y + place.h, keep);
  }
}

/**
 * Moves `moving` to `to` on a grid, the others making way, and settles it.
 *
 * Moved along its row, the tiles of the row whose middles it passes slide back into the room
 * it left, and it keeps beside one it comes onto, on the side its middle is. Moved up or down,
 * a tile it comes onto takes the place it left, where that is clear; else the moving tile
 * passes it once its leading edge passes the tile's middle, the tile going above it where
 * there is room, and until then the tile goes below. So a tile dragged part of the way does
 * not throw the others about: they change places with it as it passes them.
 */
export function moveOnGrid(places, moving, to) {
  const start = { x: moving.x, y: moving.y };
  Object.assign(moving, onPage({ ...moving, ...to }));
  const middle = (place, axis) => (axis === 'x' ? place.x + place.w / 2 : place.y + place.h / 2);
  // Whether a tile shares at least half its rows, or the moving tile's, with the moving tile.
  const level = (place) => Math.min(place.y + place.h, moving.y + moving.h) - Math.max(place.y, moving.y) >= Math.min(place.h, moving.h) / 2;
  const across = Math.abs(moving.x - start.x) > Math.abs(moving.y - start.y);
  const down = moving.y > start.y;
  if (across) {
    const from = start.x + moving.w / 2;
    const towards = middle(moving, 'x');
    const passed = (place) => (towards > from
      ? middle(place, 'x') > from && middle(place, 'x') <= towards
      : middle(place, 'x') < from && middle(place, 'x') >= towards);
    const step = towards > from ? -moving.w : moving.w;
    // The nearest the room first, each into what the one before it left.
    const sliding = places.filter((place) => place !== moving && level(place) && passed(place)).sort((a, b) => (step < 0 ? a.x - b.x : b.x - a.x));
    for (const place of sliding) {
      if (fits(places, place, { x: place.x + step })) place.x += step;
    }
  }
  // Each turn moves one tile out of another's way, or the moving tile aside or down: few end it.
  for (let turns = places.length * 4; turns > 0; turns -= 1) {
    const other = places.find((each) => each !== moving && overlaps(each, moving));
    if (!other) break;
    const beside = middle(moving, 'x') < middle(other, 'x') ? other.x - moving.w : other.x + other.w;
    const passing = down ? moving.y + moving.h > middle(other, 'y') : moving.y < middle(other, 'y');
    if (across && level(other) && beside >= 0 && beside + moving.w <= COLUMNS) moving.x = beside;
    else if (!across && fits(places, other, start)) Object.assign(other, start);
    else if (down ? !passing : passing) pushDown(places, other, moving.y + moving.h, moving);
    else if (fits(places, other, { y: moving.y - other.h })) other.y = moving.y - other.h;
    else moving.y = other.y + other.h;
  }
  return settle(places);
}

/** Makes `place` the size given on a grid, pushing what it comes onto below it, and settles it. */
export function resizeOnGrid(places, place, size) {
  Object.assign(place, onPage({ ...place, ...size }));
  for (const other of inReadingOrder(places)) {
    if (other !== place && overlaps(other, place)) pushDown(places, other, place.y + place.h, place);
  }
  return settle(places);
}

/** The first place, in reading order, where a tile of `size` stands clear of the others. */
export function firstFree(places, { w, h }) {
  const width = Math.min(w, COLUMNS);
  for (let y = 0; ; y += 1) {
    for (let x = 0; x + width <= COLUMNS; x += 1) {
      const place = { x, y, w: width, h };
      if (!places.some((other) => overlaps(other, place))) return place;
    }
  }
}

/**
 * Brings places laid out freely onto the grid: each to whole columns and rows, in reading
 * order, as near where it was as it fits clear of those placed before it; then settles them.
 */
export function ontoGrid(places) {
  const placed = [];
  for (const place of inReadingOrder(places)) {
    Object.assign(place, onPage({ x: Math.round(place.x), y: Math.round(place.y), w: Math.round(place.w), h: Math.max(Math.round(place.h), 1) }));
    while (placed.some((other) => overlaps(other, place))) place.y += 1;
    placed.push(place);
  }
  return settle(places);
}

/**
 * The lines a place may be lined up with, across (`x`) or down (`y`): the edges and middle
 * of every other place, and the page's edges, with its middle across.
 */
export function linesOf(places, moving, axis) {
  const [start, size] = axis === 'x' ? ['x', 'w'] : ['y', 'h'];
  const lines = axis === 'x' ? [0, COLUMNS / 2, COLUMNS] : [0];
  for (const other of places) {
    if (other !== moving) lines.push(other[start], other[start] + other[size] / 2, other[start] + other[size]);
  }
  return lines;
}

/** The line nearest `value`, within `near` of it; or null where none is. */
export function nearest(value, lines, near) {
  let best = null;
  for (const line of lines) {
    if (Math.abs(line - value) <= near && (best === null || Math.abs(line - value) < Math.abs(best - value))) best = line;
  }
  return best;
}
