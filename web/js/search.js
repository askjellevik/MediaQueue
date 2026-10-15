// The user page: access-code gate, then search, confirm and "My requests".
// Nothing but the gate is rendered until /api/me says a session exists.

import { api, latestOnly } from './api.js';
import { watch } from './live.js';
import {
  STATE_LABEL,
  STATUS_LABEL,
  badge,
  chip,
  clear,
  countdown,
  el,
  extLink,
  footer,
  formatDate,
  formatWait,
  imdbUrl,
  posterLarge,
  posterSmall,
  spinner,
  tmdbUrl,
  toast,
} from './ui.js';

const root = document.getElementById('app');
const APP_NAME = 'MediaQueue';
const DEBOUNCE_MS = 200;
const MIN_QUERY = 2;
const MEMO_SIZE = 20;
const NOTE_MAX = 200;

// Which search-result state each request status shows as.
const STATE_OF_STATUS = { pending: 'requested', added: 'added', already_had: 'have', refused: 'unavailable' };

// Stops the live updates of the signed-in page when it goes away.
let stopLive = null;

// ---------------------------------------------------------------------
// Access gate
// ---------------------------------------------------------------------

function renderGate() {
  if (stopLive) stopLive();
  stopLive = null;
  document.title = APP_NAME;
  const input = el('input', {
    id: 'code',
    class: 'field code-input',
    type: 'text',
    inputmode: 'text',
    autocapitalize: 'characters',
    autocomplete: 'one-time-code',
    autocorrect: 'off',
    spellcheck: 'false',
    maxlength: 64,
    required: true,
    placeholder: 'Access code',
    'aria-describedby': 'code-error',
  });
  const button = el('button', { class: 'btn btn-primary', type: 'submit', text: 'Continue' });
  const error = el('p', { id: 'code-error', class: 'error', role: 'alert' });
  let stopCountdown = null;

  const lock = (seconds) => {
    input.disabled = true;
    button.disabled = true;
    if (stopCountdown) stopCountdown();
    stopCountdown = countdown(
      seconds,
      (left) => {
        error.textContent = `Too many attempts. Try again in ${formatWait(left)}.`;
      },
      () => {
        input.disabled = false;
        button.disabled = false;
        error.textContent = '';
        input.focus();
      },
    );
  };

  const form = el(
    'form',
    { class: 'gate-form', novalidate: true },
    el('h1', { text: APP_NAME }),
    el('label', { class: 'vh', for: 'code', text: 'Access code' }),
    input,
    button,
    error,
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const code = input.value.trim();
    if (!code) {
      error.textContent = "That code didn't work";
      return;
    }
    button.disabled = true;
    error.textContent = '';
    const res = await api.login(code);
    button.disabled = false;
    if (res.ok) {
      boot();
    } else if (res.status === 429) {
      lock(res.retryAfter || 60);
    } else if (res.status === 0) {
      error.textContent = "Can't reach the server. Check your connection and try again.";
    } else {
      error.textContent = "That code didn't work";
      input.select();
    }
  });

  clear(root);
  root.append(el('main', { class: 'gate' }, form));
  input.focus();
}

// ---------------------------------------------------------------------
// Signed-in page
// ---------------------------------------------------------------------

function renderApp(me) {
  document.title = APP_NAME;
  const state = {
    remaining: me.remaining_today,
    results: [],
    query: '',
    // Normalised query of the search in flight or last answered.
    sent: null,
    active: -1,
    memo: new Map(),
    mineCursor: null,
  };
  const latest = latestOnly();
  let debounceTimer = 0;
  let stopSearchLock = null;

  // ----- Header -----
  const remainingText = el('span');
  const updateRemaining = () => {
    const n = state.remaining;
    // Only members with an admin-set limit see a count.
    remainingText.textContent = typeof n === 'number' ? ` · ${n} request${n === 1 ? '' : 's'} left today` : '';
  };
  updateRemaining();
  const header = el(
    'header',
    { class: 'top' },
    el('h1', { text: APP_NAME }),
    el('p', { class: 'muted' }, `Hi ${me.name}`, remainingText),
  );

  // ----- Search combobox -----
  const input = el('input', {
    id: 'q',
    class: 'field',
    type: 'text',
    role: 'combobox',
    'aria-autocomplete': 'list',
    'aria-expanded': 'false',
    'aria-controls': 'results',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    enterkeyhint: 'search',
    maxlength: 80,
    placeholder: 'Search movies and series',
  });
  const list = el('ul', { id: 'results', class: 'results', role: 'listbox', 'aria-label': 'Search results', hidden: true });
  const status = el('p', { class: 'status', role: 'status', 'aria-live': 'polite' });
  const combo = el(
    'div',
    { class: 'combo' },
    el('label', { class: 'vh', for: 'q', text: 'Search movies and series' }),
    input,
    list,
  );

  const setStatus = (text, cls = '', action = null) => {
    status.className = `status ${cls}`;
    clear(status);
    if (text) status.append(text);
    if (action) status.append(' ', action);
  };

  const openList = () => {
    list.hidden = false;
    input.setAttribute('aria-expanded', 'true');
  };
  const closeList = () => {
    list.hidden = true;
    input.setAttribute('aria-expanded', 'false');
    setActive(-1);
  };

  const setActive = (index) => {
    state.active = index;
    let activeId = '';
    for (const [i, option] of [...list.children].entries()) {
      const on = i === index;
      option.setAttribute('aria-selected', on ? 'true' : 'false');
      if (on) {
        activeId = option.id;
        option.scrollIntoView({ block: 'nearest' });
      }
    }
    if (activeId) input.setAttribute('aria-activedescendant', activeId);
    else input.removeAttribute('aria-activedescendant');
  };

  const renderSkeleton = () => {
    clear(list);
    for (let i = 0; i < 3; i++) list.append(el('li', { class: 'skeleton', 'aria-hidden': 'true' }));
    openList();
  };

  const renderResults = (results) => {
    state.results = results;
    clear(list);
    state.active = -1;
    input.removeAttribute('aria-activedescendant');
    if (results.length === 0) {
      closeList();
      setStatus(`No matches for “${state.query}”.`);
      return;
    }
    results.forEach((item, i) => {
      const disabled = item.state !== 'none';
      const meta = el('span', { class: 'opt-meta' }, item.year ? String(item.year) : null, badge(item.type));
      const option = el(
        'li',
        {
          id: `opt-${i}`,
          class: 'option',
          role: 'option',
          'aria-selected': 'false',
          'aria-disabled': disabled ? 'true' : null,
        },
        posterSmall(item.poster),
        el('span', { class: 'opt-text' }, el('span', { class: 'opt-title', text: item.title }), meta),
        disabled ? chip(item.state, STATE_LABEL[item.state] || item.state) : null,
      );
      // mousedown, not click: keeps focus in the input on desktop.
      option.addEventListener('mousedown', (event) => event.preventDefault());
      option.addEventListener('click', () => choose(i));
      list.append(option);
    });
    openList();
    setStatus(`${results.length} result${results.length === 1 ? '' : 's'}. Use the arrow keys to choose.`);
  };

  const memoGet = (q) => {
    const hit = state.memo.get(q);
    if (hit) {
      // Refresh its place as most recently used.
      state.memo.delete(q);
      state.memo.set(q, hit);
    }
    return hit;
  };
  const memoSet = (q, results) => {
    state.memo.set(q, results);
    if (state.memo.size > MEMO_SIZE) state.memo.delete(state.memo.keys().next().value);
  };

  const lockSearch = (seconds) => {
    input.disabled = true;
    closeList();
    if (stopSearchLock) stopSearchLock();
    stopSearchLock = countdown(
      seconds,
      (left) => setStatus(`Too many searches. Try again in ${formatWait(left)}.`, 'status-bad'),
      () => {
        input.disabled = false;
        setStatus('');
        input.focus();
      },
    );
  };

  const runSearch = async (q) => {
    state.sent = q;
    renderSkeleton();
    setStatus('Searching…');
    const outcome = await latest.run((signal) => api.search(q, signal));
    if (!outcome.current) return;
    const res = outcome.value;
    if (res.ok && res.data && Array.isArray(res.data.results)) {
      memoSet(q, res.data.results);
      renderResults(res.data.results);
    } else if (res.status === 401) {
      renderGate();
    } else if (res.status === 429) {
      lockSearch(res.retryAfter || 1);
    } else {
      closeList();
      const retry = el('button', { class: 'btn-link', type: 'button', text: 'Retry', onclick: () => runSearch(q) });
      const text =
        res.status === 0
          ? "Can't reach the server."
          : res.status === 503
            ? 'The movie database is not answering right now.'
            : 'Search failed.';
      setStatus(text, 'status-bad', retry);
    }
  };

  input.addEventListener('input', () => {
    clearTimeout(debounceTimer);
    const q = input.value.trim().replace(/\s+/g, ' ').toLowerCase();
    // Any keystroke makes an in-flight search stale, even before the next
    // one is sent: only results for what is in the box may render.
    if (q === state.sent) return;
    latest.cancel();
    state.sent = null;
    state.query = input.value.trim();
    if (q.length < MIN_QUERY) {
      closeList();
      clear(list);
      state.results = [];
      setStatus('');
      return;
    }
    const memo = memoGet(q);
    if (memo) {
      renderResults(memo);
      return;
    }
    renderSkeleton();
    debounceTimer = setTimeout(() => runSearch(q), DEBOUNCE_MS);
  });

  input.addEventListener('keydown', (event) => {
    const count = state.results.length;
    const open = !list.hidden && count > 0;
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault();
        if (!open && count > 0) openList();
        if (count > 0) setActive(Math.min(state.active + 1, count - 1));
        break;
      case 'ArrowUp':
        event.preventDefault();
        if (open) setActive(Math.max(state.active - 1, -1));
        break;
      case 'Home':
        if (open && state.active >= 0) {
          event.preventDefault();
          setActive(0);
        }
        break;
      case 'End':
        if (open && state.active >= 0) {
          event.preventDefault();
          setActive(count - 1);
        }
        break;
      case 'Enter':
        if (open && state.active >= 0) {
          event.preventDefault();
          choose(state.active);
        }
        break;
      case 'Escape':
        if (open) {
          event.preventDefault();
          closeList();
        } else if (input.value) {
          event.preventDefault();
          input.value = '';
          input.dispatchEvent(new Event('input'));
        }
        break;
      case 'Tab':
        closeList();
        break;
      default:
    }
  });
  input.addEventListener('focus', () => {
    if (state.results.length > 0 && input.value.trim().length >= MIN_QUERY) openList();
  });
  input.addEventListener('blur', () => {
    // Let a click on an option land first.
    setTimeout(() => {
      if (document.activeElement !== input) closeList();
    }, 150);
  });

  // ----- Confirm sheet -----
  const sheet = el('dialog', { class: 'sheet', 'aria-labelledby': 'sheet-title' });
  sheet.addEventListener('close', () => {
    input.focus();
  });

  const choose = (index) => {
    const item = state.results[index];
    if (!item) return;
    if (item.state !== 'none') {
      setStatus(`“${item.title}” is already on the list: ${STATE_LABEL[item.state] || item.state}.`);
      return;
    }
    closeList();
    openSheet(item);
  };

  const openSheet = (item) => {
    clear(sheet);
    const note = el('textarea', {
      id: 'note',
      class: 'field note',
      maxlength: NOTE_MAX,
      rows: 3,
      placeholder: 'Anything I should know? (optional)',
      'aria-describedby': 'note-count',
    });
    const counter = el('p', { id: 'note-count', class: 'counter', text: `0 / ${NOTE_MAX}` });
    note.addEventListener('input', () => {
      counter.textContent = `${note.value.length} / ${NOTE_MAX}`;
    });
    const message = el('p', { class: 'status', role: 'status' });
    const cancel = el('button', { class: 'btn', type: 'button', text: 'Cancel', onclick: () => sheet.close() });
    const add = el('button', { class: 'btn btn-primary', type: 'submit', autofocus: true }, 'Add to queue');
    const overview = item.overview && item.overview.length > 240 ? `${item.overview.slice(0, 240).trimEnd()}…` : item.overview;

    const form = el(
      'form',
      { method: 'dialog' },
      el(
        'div',
        { class: 'sheet-body' },
        extLink(tmdbUrl(item.type, item.tmdb_id), `${item.title} on TMDB`, posterLarge(item.poster), true),
        el(
          'div',
          {},
          el('h2', { id: 'sheet-title' }, extLink(tmdbUrl(item.type, item.tmdb_id), `${item.title} on TMDB`, item.title)),
          el('p', { class: 'opt-meta' }, item.year ? String(item.year) : null, badge(item.type)),
          overview ? el('p', { class: 'overview', text: overview }) : null,
        ),
      ),
      el('label', { class: 'note-label', for: 'note', text: 'Note (optional)' }),
      note,
      counter,
      message,
      el('div', { class: 'actions' }, cancel, add),
    );
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      add.disabled = true;
      cancel.disabled = true;
      add.replaceChildren(spinner(), 'Adding…');
      message.className = 'status';
      message.textContent = '';
      const res = await api.submit(item.tmdb_id, item.type, note.value.trim());
      cancel.disabled = false;
      add.replaceChildren('Add to queue');
      if (res.status === 201 || res.status === 200) {
        state.remaining = res.data.remaining_today ?? null;
        updateRemaining();
        markRequested(item);
        sheet.close();
        setStatus(
          res.status === 201 ? `Requested “${item.title}”.` : `“${item.title}” was already requested. You're on the list too.`,
          'status-ok',
        );
        refreshMine();
      } else if (res.status === 401) {
        sheet.close();
        renderGate();
      } else if (res.status === 429) {
        if (res.data && typeof res.data.remaining_today === 'number') {
          state.remaining = res.data.remaining_today;
          updateRemaining();
        }
        message.className = 'status status-bad';
        const wait = formatWait(res.retryAfter || 60);
        message.textContent =
          res.data && res.data.remaining_today === 0
            ? `You've reached your daily limit. You can request again in ${wait}.`
            : `You're requesting too fast. Try again in ${wait}.`;
      } else {
        add.disabled = false;
        message.className = 'status status-bad';
        message.textContent =
          res.status === 0
            ? "Can't reach the server. Try again."
            : res.status === 503
              ? 'The movie database is not answering right now. Try again shortly.'
              : res.status === 404
                ? "That title couldn't be found."
                : 'Something went wrong. Try again.';
      }
    });
    sheet.append(form);
    sheet.showModal();
    add.focus();
  };

  // Results already shown (and memoised) learn the new state at once.
  const markRequested = (item) => {
    for (const results of [state.results, ...state.memo.values()]) {
      for (const r of results) {
        if (r.tmdb_id === item.tmdb_id && r.type === item.type) r.state = 'requested';
      }
    }
  };

  // ----- My requests -----
  const mineList = el('ul', { class: 'mine-list' });
  const mineEmpty = el('p', { class: 'muted', text: 'Nothing requested yet.', hidden: true });
  const mineStatus = el('p', { class: 'status', role: 'status' });
  const more = el('button', { class: 'btn btn-small', type: 'button', text: 'Load more', hidden: true });
  more.addEventListener('click', () => loadMine(false));

  // Rendered items by request id, so an update changes a row in place.
  const mineNodes = new Map();

  const mineItem = (r) => {
    const status = chip(r.status, STATUS_LABEL[r.status] || r.status);
    // IMDb once the request has an ID, the TMDB page until then.
    const href = r.imdb_id ? imdbUrl(r.imdb_id) : tmdbUrl(r.type, r.tmdb_id);
    const site = r.imdb_id ? 'IMDb' : 'TMDB';
    const li = el(
      'li',
      { class: 'mine-item' },
      extLink(href, `${r.title} on ${site}`, posterSmall(r.poster), true),
      el(
        'div',
        { class: 'opt-text' },
        el('p', { class: 'opt-title' }, extLink(href, `${r.title} on ${site}`, r.title)),
        el('p', { class: 'opt-meta' }, r.year ? String(r.year) : null, badge(r.type), `Requested ${formatDate(r.created_at)}`),
      ),
      status,
    );
    mineNodes.set(r.id, { li, status, state: r.status });
    return li;
  };

  const loadMine = async (reset) => {
    more.disabled = true;
    const res = await api.mine(reset ? null : state.mineCursor);
    more.disabled = false;
    if (res.status === 401) {
      renderGate();
      return;
    }
    if (!res.ok || !res.data) {
      mineStatus.className = 'status status-bad';
      mineStatus.textContent = "Couldn't load your requests.";
      return;
    }
    mineStatus.textContent = '';
    if (reset) {
      clear(mineList);
      mineNodes.clear();
    }
    for (const r of res.data.items) mineList.append(mineItem(r));
    state.mineCursor = res.data.next_cursor;
    more.hidden = !state.mineCursor;
    mineEmpty.hidden = mineList.children.length > 0;
  };

  // Search results learn a request's new status too.
  const syncResults = (r) => {
    const next = STATE_OF_STATUS[r.status];
    let changed = false;
    for (const results of [state.results, ...state.memo.values()]) {
      for (const item of results) {
        if (item.tmdb_id === r.tmdb_id && item.type === r.type && item.state !== next) {
          item.state = next;
          changed = true;
        }
      }
    }
    return changed;
  };

  // Re-reads the first page and updates rows in place: no clearing, no
  // scroll jump. A status that changed gets a toast.
  const refreshMine = async () => {
    const res = await api.mine(null);
    if (res.status === 401) {
      renderGate();
      return;
    }
    if (!res.ok || !res.data) return;
    const wasEmpty = mineNodes.size === 0;
    let previous = null;
    let resultsChanged = false;
    for (const r of res.data.items) {
      const known = mineNodes.get(r.id);
      if (known) {
        if (known.state !== r.status) {
          known.state = r.status;
          const fresh = chip(r.status, STATUS_LABEL[r.status] || r.status);
          known.status.replaceWith(fresh);
          known.status = fresh;
          toast(`“${r.title}” is now ${STATUS_LABEL[r.status] || r.status}.`);
        }
        previous = known.li;
      } else {
        const li = mineItem(r);
        if (previous) previous.after(li);
        else mineList.prepend(li);
        previous = li;
      }
      if (syncResults(r)) resultsChanged = true;
    }
    if (wasEmpty) {
      state.mineCursor = res.data.next_cursor;
      more.hidden = !state.mineCursor;
    }
    mineEmpty.hidden = mineList.children.length > 0;
    if (resultsChanged && !list.hidden) {
      const active = state.active;
      renderResults(state.results);
      setActive(active);
    }
  };

  const mine = el(
    'section',
    { class: 'mine', 'aria-labelledby': 'mine-title' },
    el('h2', { id: 'mine-title', text: 'My requests' }),
    mineEmpty,
    mineList,
    mineStatus,
    more,
  );

  const logout = async () => {
    await api.logout();
    renderGate();
  };

  clear(root);
  root.append(
    el(
      'div',
      { class: 'page' },
      header,
      el('main', {}, el('section', { class: 'search', 'aria-label': 'Search' }, combo, status), mine),
      footer(logout),
      sheet,
    ),
  );
  input.focus();
  loadMine(true);
  if (stopLive) stopLive();
  stopLive = watch('/api/events', refreshMine);
}

// ---------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------

async function boot() {
  const res = await api.me();
  if (res.ok && res.data) {
    renderApp(res.data);
  } else if (res.status === 401) {
    renderGate();
  } else {
    clear(root);
    const retry = el('button', { class: 'btn', type: 'button', text: 'Try again', onclick: boot });
    root.append(
      el(
        'main',
        { class: 'gate' },
        el('div', { class: 'gate-form' }, el('h1', { text: APP_NAME }), el('p', { text: "Can't reach the server right now." }), retry),
      ),
    );
  }
}

boot();
