// DOM helpers shared by both pages. Text from the API only ever goes in
// through textContent (via el()'s children or `text`), never as HTML.

import { t } from './strings.js';

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
const ICON_URL = new URL('../img/icon.svg', import.meta.url).href;

// The app icon, for the gate card and the app bar.
export const appIcon = (cls = '') => el('img', { class: cls, src: ICON_URL, alt: '', width: 32, height: 32 });

// The bar across the top of the signed-in pages: icon and name on the left,
// whatever `right` holds on the right. `wide` matches the admin page width.
export function appBar(name, right = null, wide = false) {
  return el(
    'header',
    { class: 'appbar' },
    el(
      'div',
      { class: `appbar-inner ${wide ? 'max-w-6xl' : ''}`.trim() },
      el('h1', { class: 'brand' }, appIcon(), name),
      right,
    ),
  );
}

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

export const imdbUrl = (id) => `https://www.imdb.com/title/${encodeURIComponent(id)}/`;
export const tmdbUrl = (type, id) => `https://www.themoviedb.org/${type === 'tv' ? 'tv' : 'movie'}/${id}`;

// A link to an outside site. With `plain` it is a second route to the same
// place as a text link next to it, so it stays out of the tab order.
export function extLink(href, label, children, plain = false) {
  return el(
    'a',
    {
      class: 'ext-link',
      href,
      target: '_blank',
      rel: 'noopener noreferrer',
      'aria-label': plain ? null : label,
      'aria-hidden': plain ? 'true' : null,
      tabindex: plain ? '-1' : null,
    },
    children,
  );
}

export const TYPE_LABEL = t.type;

// Search result states and request statuses, as shown to users.
export const STATE_LABEL = t.state;

export const STATUS_LABEL = t.status;

// The colours of chip-<key> and the badge are in styles/base.css.
export function chip(key, label) {
  return el('span', { class: `chip chip-${key}`, text: label });
}

export function badge(type) {
  return el('span', { class: 'badge', text: TYPE_LABEL[type] || type });
}

// "45 s", "14:59" or "3 t 12 min".
export function formatWait(totalSeconds) {
  const s = Math.max(0, Math.ceil(totalSeconds));
  if (s < 60) return t.seconds(s);
  if (s < 3600) return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return t.hoursMinutes(h, m);
}

const LOCALE = 'nb-NO';

// "16. okt. 2026".
export function formatDate(unixSeconds) {
  return new Date(unixSeconds * 1000).toLocaleDateString(LOCALE, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  });
}

// "akkurat nå", "5 min siden", "2 t siden", "3 d siden".
export function formatAgo(unixSeconds) {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  if (secs < 60) return t.justNow;
  if (secs < 3600) return t.minutesAgo(Math.floor(secs / 60));
  if (secs < 86400) return t.hoursAgo(Math.floor(secs / 3600));
  return t.daysAgo(Math.floor(secs / 86400));
}

// "16. okt. 2026, 14:05".
export function formatDateTime(unixSeconds) {
  return new Date(unixSeconds * 1000).toLocaleString(LOCALE, {
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

// ----- Toasts -----
// Short messages next to where the user just acted. The live region exists
// from the start so screen readers announce what is added to it later.

const TOAST_MS = 5000;
const MAX_TOASTS = 3;

function toastRegion() {
  let region = document.getElementById('toasts');
  if (!region) {
    region = el('div', { id: 'toasts', class: 'toasts', 'aria-live': 'polite' });
    document.body.append(region);
  }
  return region;
}

toastRegion();

// `kind` is 'ok' or 'bad'; `undo`, if given, adds an undo button. The timer
// pauses while the pointer or keyboard focus is on the toast.
export function toast(text, { kind = 'ok', undo = null } = {}) {
  const region = toastRegion();
  let timer = 0;
  const dismiss = () => {
    clearTimeout(timer);
    node.remove();
  };
  const arm = () => {
    clearTimeout(timer);
    timer = setTimeout(dismiss, TOAST_MS);
  };
  const node = el('div', { class: `toast toast-${kind}` }, el('span', { text }));
  if (undo) {
    node.append(
      el('button', {
        class: 'btn btn-small',
        type: 'button',
        text: t.undo,
        onclick: () => {
          dismiss();
          undo();
        },
      }),
    );
  }
  node.addEventListener('mouseenter', () => clearTimeout(timer));
  node.addEventListener('focusin', () => clearTimeout(timer));
  node.addEventListener('mouseleave', arm);
  node.addEventListener('focusout', arm);
  region.append(node);
  while (region.children.length > MAX_TOASTS) region.firstChild.remove();
  arm();
}

// ----- Clipboard -----

// navigator.clipboard needs a secure context; fall back to selecting a
// hidden field and the old copy command. Resolves to whether it worked.
export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Insecure context or permission denied: try the fallback.
  }
  const active = document.activeElement;
  const field = el('textarea', { class: 'sr-only', readonly: true, 'aria-hidden': 'true', tabindex: '-1' });
  field.value = text;
  document.body.append(field);
  field.select();
  let ok = false;
  try {
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  }
  field.remove();
  if (active && active.focus) active.focus();
  return ok;
}

// Click copies `text`; the button says "Kopiert" for a moment. If copying is
// impossible, `failText` goes into a toast.
export function copyOnClick(button, text, failText) {
  const label = button.textContent;
  let timer = 0;
  button.addEventListener('click', async () => {
    if (!(await copyText(text))) {
      toast(failText, { kind: 'bad' });
      return;
    }
    clearTimeout(timer);
    button.textContent = t.copied;
    timer = setTimeout(() => {
      button.textContent = label;
    }, 1500);
  });
}

// System, Lys and Mørk as a small button group; the choice is kept by
// js/theme.js, which also puts it on <html> before the page paints.
const THEMES = [
  { id: 'system', label: t.theme.system },
  { id: 'light', label: t.theme.light },
  { id: 'dark', label: t.theme.dark },
];

function themeToggle() {
  const theme = window.mqTheme;
  if (!theme) return null;
  const buttons = THEMES.map(({ id, label }) =>
    el('button', {
      type: 'button',
      text: label,
      'aria-pressed': 'false',
      onclick: () => {
        theme.set(id);
        paint();
      },
    }),
  );
  const paint = () => {
    buttons.forEach((button, i) => button.setAttribute('aria-pressed', THEMES[i].id === theme.get() ? 'true' : 'false'));
  };
  paint();
  return el('div', { class: 'theme-toggle', role: 'group', 'aria-label': t.theme.label }, buttons);
}

export function footer(onLogout) {
  return el(
    'footer',
    { class: 'foot' },
    el(
      'div',
      { class: 'attribution' },
      el('img', { class: 'tmdb-logo', src: LOGO_URL, alt: 'TMDB', width: 92, height: 12 }),
      el('span', { text: t.tmdbNotice }),
    ),
    el(
      'div',
      { class: 'flex items-center gap-3' },
      themeToggle(),
      onLogout ? el('button', { class: 'btn-link', type: 'button', text: t.logOut, onclick: onLogout }) : null,
    ),
  );
}
