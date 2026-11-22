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

/**
 * Calls the API. `path` is an Epiphany server path such as `/model`, or a GUI server path
 * beginning `/gui/`. The answer's JSON is returned; an error is thrown as an ApiError.
 */
export async function api(method, path, body) {
  const init = { method, headers: { ...CLIENT }, credentials: 'same-origin' };
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const url = path.startsWith('/gui/') ? path : `/api${path}`;
  let response;
  try {
    response = await fetch(url, init);
  } catch {
    throw new ApiError(0, 'offline', 'The Epiphany GUI server cannot be reached. Check the connection and try again.');
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
  const text = response.status === 204 ? '' : await response.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = { message: text };
    }
  }
  if (!response.ok) {
    const message = data?.message ?? `${response.status} ${response.statusText}`;
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

export const get = (path) => api('GET', path);
export const post = (path, body) => api('POST', path, body);
export const put = (path, body) => api('PUT', path, body);
export const del = (path) => api('DELETE', path);

/** A name as it appears in a URL path. */
export const part = (name) => encodeURIComponent(name);
