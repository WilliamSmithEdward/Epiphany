// Calls to the GUI server and, through it, to the Epiphany server.

/** Sent with every call, so that the GUI server knows it comes from this application. */
const CLIENT = { 'x-requested-by': 'epiphany-gui' };

/** An error in the shape the servers give: an HTTP status, a stable code and a message. */
export class ApiError extends Error {
  constructor(status, code, message, index) {
    super(message);
    this.status = status;
    this.code = code;
    this.index = index;
  }
}

/** Raised when the session has ended, so that the application can ask for a sign-in. */
export const events = new EventTarget();

/** The model's version as the last answer gave it. */
export let modelVersion = null;

/*
 * A call the user waits on shows, once it has taken a moment, as a bar moving along the top
 * of the window: quick calls show nothing, and the bar, once shown, stays a moment so that
 * it does not flicker. Calls that show their own progress, or that nobody waits on, are
 * made `quiet`.
 */
let waiting = 0;
let showing;
let hiding;
let shownAt = 0;

function wait(change) {
  waiting += change;
  const root = document.documentElement;
  if (waiting > 0) {
    clearTimeout(hiding);
    if (!root.classList.contains('waiting')) {
      showing ??= setTimeout(() => {
        showing = undefined;
        root.classList.add('waiting');
        shownAt = performance.now();
      }, 200);
    }
  } else {
    clearTimeout(showing);
    showing = undefined;
    hiding = setTimeout(() => root.classList.remove('waiting'), Math.max(0, 400 - (performance.now() - shownAt)));
  }
}

/**
 * Calls the API. `path` is an Epiphany server path such as `/model`, or a GUI server path
 * beginning `/gui/`. The answer's JSON is returned; an error is thrown as an ApiError. A
 * `quiet` call shows nothing while it runs, and one given a `signal` can be stopped, when it
 * throws an ApiError coded `stopped`.
 */
export async function api(method, path, body, { quiet = false, signal } = {}) {
  const init = { method, headers: { ...CLIENT }, credentials: 'same-origin', signal };
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const url = path.startsWith('/gui/') ? path : `/api${path}`;
  if (!quiet) wait(1);
  let response;
  let text;
  try {
    response = await fetch(url, init);
    text = response.status === 204 ? '' : await response.text();
  } catch {
    if (signal?.aborted) throw new ApiError(0, 'stopped', 'Stopped.');
    throw new ApiError(0, 'offline', 'The Epiphany GUI server cannot be reached. Check the connection and try again.');
  } finally {
    if (!quiet) wait(-1);
  }
  const version = response.headers.get('epiphany-model-version');
  if (version !== null) {
    const changed = modelVersion !== null && version !== modelVersion;
    modelVersion = version;
    if (changed) events.dispatchEvent(new CustomEvent('model-changed', { detail: version }));
  }
  if (response.status === 401 && !path.startsWith('/gui/')) {
    events.dispatchEvent(new Event('signed-out'));
  }
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = { message: text };
    }
  }
  if (!response.ok) {
    // The server tells API clients how to sign in; here, the application asks for it.
    const ended = response.status === 401 && !path.startsWith('/gui/');
    const message = ended ? 'Your session has ended.' : data?.message ?? `${response.status} ${response.statusText}`;
    throw new ApiError(response.status, data?.code ?? 'error', sentence(message), data?.index);
  }
  return data;
}

/** A server message as a sentence: capitalised and ending in a stop. */
export function sentence(text) {
  const trimmed = String(text).trim();
  if (!trimmed) return trimmed;
  const first = trimmed[0].toUpperCase() + trimmed.slice(1);
  return /[.!?]$/.test(first) ? first : `${first}.`;
}

export const get = (path, options) => api('GET', path, undefined, options);
export const post = (path, body, options) => api('POST', path, body, options);
export const put = (path, body, options) => api('PUT', path, body, options);
export const del = (path, options) => api('DELETE', path, undefined, options);

/** A name as it appears in a URL path. */
export const part = (name) => encodeURIComponent(name);
