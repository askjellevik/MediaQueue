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
  if (!path) return el('div', { class: `poster ${cls} rounded-lg ring-1 ring-white/10`, 'aria-hidden': 'true' });
  return el('img', {
    class: `poster ${cls} rounded-lg shadow-md shadow-black/40 ring-1 ring-white/10`,
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

export const TYPE_LABEL = { movie: 'Movie', tv: 'Series' };

// Search result states and request statuses, as shown to users.
export const STATE_LABEL = {
  requested: 'Requested',
  added: 'Added',
  have: 'In library',
  unavailable: 'Declined',
};

export const STATUS_LABEL = {
  pending: 'Pending',
  added: 'Added',
  already_had: 'In library',
  refused: 'Declined',
};

// Whole class strings, so the Tailwind build can see them.
const CHIP_STYLE = {
  requested: 'bg-warn/15 text-warn ring-warn/30',
  pending: 'bg-warn/15 text-warn ring-warn/30',
  added: 'bg-ok/15 text-ok ring-ok/30',
  have: 'bg-ok/15 text-ok ring-ok/30',
  already_had: 'bg-ok/15 text-ok ring-ok/30',
  unavailable: 'bg-bad/15 text-bad ring-bad/30',
  refused: 'bg-bad/15 text-bad ring-bad/30',
};

export function chip(key, label) {
  const style = CHIP_STYLE[key] || 'bg-surface-2 text-muted ring-line';
  return el('span', {
    class: `chip chip-${key} rounded-full px-2.5 py-0.5 text-xs font-medium ring-1 ring-inset ${style}`,
    text: label,
  });
}

export function badge(type) {
  return el('span', {
    class: 'badge rounded-md border-line px-1.5 text-[0.68rem] font-medium uppercase tracking-wide',
    text: TYPE_LABEL[type] || type,
  });
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

// "just now", "5 min ago", "2 h ago", "3 d ago".
export function formatAgo(unixSeconds) {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  if (secs < 60) return 'just now';
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} h ago`;
  return `${Math.floor(secs / 86400)} d ago`;
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

// `kind` is 'ok' or 'bad'; `undo`, if given, adds an Undo button. The timer
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
        text: 'Undo',
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
  const field = el('textarea', { class: 'vh', readonly: true, 'aria-hidden': 'true', tabindex: '-1' });
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

// Click copies `text`; the button says "Copied" for a moment. If copying is
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
    button.textContent = 'Copied';
    timer = setTimeout(() => {
      button.textContent = label;
    }, 1500);
  });
}

export function footer(onLogout) {
  return el(
    'footer',
    { class: 'foot border-t border-line pt-4 text-muted' },
    el(
      'div',
      { class: 'attribution' },
      el('img', { class: 'tmdb-logo', src: LOGO_URL, alt: 'TMDB', width: 92, height: 12 }),
      el('span', { text: 'This product uses the TMDB API but is not endorsed or certified by TMDB.' }),
    ),
    onLogout ? el('button', { class: 'btn-link rounded-md hover:text-ink', type: 'button', text: 'Log out', onclick: onLogout }) : null,
  );
}
