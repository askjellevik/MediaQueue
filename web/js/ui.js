// DOM helpers shared by both pages. Text from the API only ever goes in
// through textContent (via el()'s children or `text`), never as HTML.

export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on') && typeof value === 'function') node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  for (const child of children.flat()) {
    if (child !== null && child !== undefined && child !== false) node.append(child);
  }
  return node;
}

export function clear(node) {
  node.replaceChildren();
}

// Resolved against this module's own (versioned) URL.
const LOGO_URL = new URL('../img/tmdb.svg', import.meta.url).href;

const POSTER_BASE = 'https://image.tmdb.org/t/p/';

// Fixed width and height on every poster so nothing shifts as images load.
export function poster(path, size, cls, width, height) {
  if (!path) return el('div', { class: `poster ${cls}`, 'aria-hidden': 'true' });
  return el('img', {
    class: `poster ${cls}`,
    src: `${POSTER_BASE}${size}${path}`,
    alt: '',
    width,
    height,
    loading: 'lazy',
    decoding: 'async',
  });
}

export const posterSmall = (path) => poster(path, 'w92', 'poster-sm', 46, 69);
export const posterLarge = (path) => poster(path, 'w185', 'poster-lg', 123, 185);

export const TYPE_LABEL = { movie: 'Movie', tv: 'Series' };

// Search result states and request statuses, as shown to users.
export const STATE_LABEL = {
  requested: 'Requested',
  added: 'Added',
  have: 'Already available',
  unavailable: 'Not available',
};

export const STATUS_LABEL = {
  pending: 'Pending',
  added: 'Added',
  already_had: 'Already available',
  refused: 'Not available',
};

export function chip(key, label) {
  return el('span', { class: `chip chip-${key}`, text: label });
}

export function badge(type) {
  return el('span', { class: 'badge', text: TYPE_LABEL[type] || type });
}

// "45s", "14:59" or "3h 12m".
export function formatWait(totalSeconds) {
  const s = Math.max(0, Math.ceil(totalSeconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h ${m}m`;
}

export function formatDate(unixSeconds) {
  return new Date(unixSeconds * 1000).toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  });
}

export function formatDateTime(unixSeconds) {
  return new Date(unixSeconds * 1000).toLocaleString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

// Calls tick(secondsLeft) every second until zero, then done(). Returns a
// function that stops it early.
export function countdown(seconds, tick, done) {
  const end = Date.now() + seconds * 1000;
  let timer = 0;
  const step = () => {
    const left = Math.ceil((end - Date.now()) / 1000);
    if (left <= 0) {
      done();
      return;
    }
    tick(left);
    timer = setTimeout(step, 1000);
  };
  step();
  return () => clearTimeout(timer);
}

export function spinner() {
  return el('span', { class: 'spinner', 'aria-hidden': 'true' });
}

export function footer(onLogout) {
  return el(
    'footer',
    { class: 'foot' },
    el(
      'div',
      { class: 'attribution' },
      el('img', { class: 'tmdb-logo', src: LOGO_URL, alt: 'TMDB', width: 92, height: 12 }),
      el('span', { text: 'This product uses the TMDB API but is not endorsed or certified by TMDB.' }),
    ),
    onLogout ? el('button', { class: 'btn-link', type: 'button', text: 'Log out', onclick: onLogout }) : null,
  );
}

// Access code input: uppercases, keeps letters and digits only, and shows
// a dash after the fourth character, so pasted codes with or without
// dashes and spaces all work.
export function formatCode(raw) {
  const clean = raw.toUpperCase().replace(/[^A-Z0-9]/g, '').slice(0, 16);
  return clean.length > 4 ? `${clean.slice(0, 4)}-${clean.slice(4)}` : clean;
}
