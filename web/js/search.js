// The user page: access-code gate, then search, confirm and the user's own
// requests.
// Nothing but the gate is rendered until /api/me says a session exists.

import { api, latestOnly } from './api.js';
import { watch } from './live.js';
import { t } from './strings.js';
import {
  STATE_LABEL,
  STATUS_LABEL,
  appBar,
  appIcon,
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
const APP_NAME = t.appName;
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
    placeholder: t.accessCode,
    'aria-describedby': 'code-error',
  });
  const button = el('button', { class: 'btn btn-primary', type: 'submit', text: t.continue });
  const error = el('p', { id: 'code-error', class: 'error', role: 'alert' });
  let stopCountdown = null;

  const lock = (seconds) => {
    input.disabled = true;
    button.disabled = true;
    if (stopCountdown) stopCountdown();
    stopCountdown = countdown(
      seconds,
      (left) => {
        error.textContent = t.tooManyAttempts(formatWait(left));
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
    appIcon('mx-auto size-14 rounded-2xl shadow-md ring-1 ring-black/10'),
    el('h1', { text: APP_NAME }),
    el('label', { class: 'sr-only', for: 'code', text: t.accessCode }),
    input,
    button,
    error,
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const code = input.value.trim();
    if (!code) {
      error.textContent = t.badCode;
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
      error.textContent = t.noServerCheck;
    } else {
      error.textContent = t.badCode;
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
  };
  const latest = latestOnly();
  let debounceTimer = 0;
  let stopSearchLock = null;

  // ----- Header -----
  const remainingText = el('span');
  const updateRemaining = () => {
    const n = state.remaining;
    // Only members with an admin-set limit see a count.
    remainingText.textContent = typeof n === 'number' ? t.requestsLeft(n) : '';
  };
  updateRemaining();
  const greeting = el('p', { class: 'appbar-meta' }, t.hi(me.name), remainingText);

  // ----- Search combobox -----
  const input = el('input', {
    id: 'q',
    class: 'field search-field',
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
    placeholder: t.searchPlaceholder,
  });
  const list = el('ul', { id: 'results', class: 'results', role: 'listbox', 'aria-label': t.searchResults, hidden: true });
  const status = el('p', { class: 'status', role: 'status', 'aria-live': 'polite' });
  const combo = el(
    'div',
    { class: 'combo' },
    el('label', { class: 'sr-only', for: 'q', text: t.searchPlaceholder }),
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
      setStatus(t.noMatches(state.query));
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
    setStatus(t.resultCount(results.length));
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
      (left) => setStatus(t.tooManySearches(formatWait(left)), 'status-bad'),
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
    setStatus(t.searching);
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
      const retry = el('button', { class: 'btn-link', type: 'button', text: t.retry, onclick: () => runSearch(q) });
      const text =
        res.status === 0 ? t.noServer : res.status === 503 ? t.dbDown : t.searchFailed;
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
  const sheet = el('dialog', {
    class: 'sheet',
    'aria-labelledby': 'sheet-title',
  });
  sheet.addEventListener('close', () => {
    input.focus();
  });

  const choose = (index) => {
    const item = state.results[index];
    if (!item) return;
    if (item.state !== 'none') {
      setStatus(t.alreadyListed(item.title, STATE_LABEL[item.state] || item.state));
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
      placeholder: t.notePlaceholder,
      'aria-describedby': 'note-count',
    });
    const counter = el('p', { id: 'note-count', class: 'counter', text: `0 / ${NOTE_MAX}` });
    note.addEventListener('input', () => {
      counter.textContent = `${note.value.length} / ${NOTE_MAX}`;
    });
    const message = el('p', { class: 'status', role: 'status' });
    const cancel = el('button', { class: 'btn', type: 'button', text: t.cancel, onclick: () => sheet.close() });
    const add = el('button', { class: 'btn btn-primary', type: 'submit', autofocus: true }, t.order);
    const overview = item.overview && item.overview.length > 240 ? `${item.overview.slice(0, 240).trimEnd()}…` : item.overview;

    const form = el(
      'form',
      { method: 'dialog' },
      el(
        'div',
        { class: 'sheet-body' },
        extLink(tmdbUrl(item.type, item.tmdb_id), t.onTmdb(item.title), posterLarge(item.poster), true),
        el(
          'div',
          {},
          el('h2', { id: 'sheet-title', class: 'text-lg font-semibold' }, extLink(tmdbUrl(item.type, item.tmdb_id), t.onTmdb(item.title), item.title)),
          el('p', { class: 'opt-meta' }, item.year ? String(item.year) : null, badge(item.type)),
          overview ? el('p', { class: 'overview', text: overview }) : null,
        ),
      ),
      el('label', { class: 'note-label', for: 'note', text: t.noteLabel }),
      note,
      counter,
      message,
      el('div', { class: 'actions' }, cancel, add),
    );
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      add.disabled = true;
      cancel.disabled = true;
      add.replaceChildren(spinner(), t.ordering);
      message.className = 'status';
      message.textContent = '';
      const res = await api.submit(item.tmdb_id, item.type, note.value.trim());
      cancel.disabled = false;
      add.replaceChildren(t.order);
      if (res.status === 201 || res.status === 200) {
        state.remaining = res.data.remaining_today ?? null;
        updateRemaining();
        markRequested(item);
        sheet.close();
        setStatus(
          res.status === 201 ? t.ordered(item.title) : t.alreadyOrdered(item.title),
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
            ? t.limitReached(wait)
            : t.tooFast(wait);
      } else {
        add.disabled = false;
        message.className = 'status status-bad';
        message.textContent =
          res.status === 0
            ? t.noServerRetry
            : res.status === 503
              ? t.dbDownRetry
              : res.status === 404
                ? t.titleNotFound
                : t.wentWrong;
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
  // Two groups that page on their own: still pending, and everything that
  // has moved on (added, already had, declined).
  const mineStatus = el('p', { class: 'status', role: 'status' });
  const makeGroup = (key, title, emptyText) => {
    const group = {
      key,
      list: el('ul', { class: 'mine-list' }),
      empty: el('p', { class: 'muted', text: emptyText, hidden: true }),
      more: el('button', { class: 'btn btn-small', type: 'button', text: t.loadMore, hidden: true }),
      cursor: null,
      // Newest-first key of the last loaded row; rows older than this are
      // not loaded yet.
      last: null,
    };
    group.more.addEventListener('click', () => loadGroup(group, false));
    group.section = el(
      'div',
      { class: 'mine-group' },
      el('h3', { class: 'group-title', text: title }),
      group.empty,
      group.list,
      group.more,
    );
    return group;
  };
  const pendingGroup = makeGroup('pending', t.pending, t.nothingPending);
  const earlierGroup = makeGroup('earlier', t.earlier, t.nothingEarlier);
  earlierGroup.section.hidden = true;

  // Rendered items by request id, so an update changes a row in place.
  const mineNodes = new Map();

  const newer = (a, b) => a[0] > b[0] || (a[0] === b[0] && a[1] > b[1]);
  const keyOf = (r) => [r.created_at, r.id];

  const paintGroup = (group) => {
    const none = group.list.children.length === 0;
    group.empty.hidden = !none || group === earlierGroup;
    group.more.hidden = !group.cursor;
    // The earlier group is only worth a heading once it has rows.
    group.section.hidden = group === earlierGroup && none && !group.cursor;
  };

  // Withdraws a request that is still pending and that nobody else asked
  // for. The server decides; a refusal just reloads the list.
  const cancelButton = (r, li) => {
    const button = el('button', {
      class: 'btn btn-small',
      type: 'button',
      text: t.withdraw,
      'data-cancel': '',
      'aria-label': t.withdrawLabel(r.title),
    });
    button.addEventListener('click', async () => {
      if (!window.confirm(t.withdrawConfirm(r.title))) return;
      button.disabled = true;
      const res = await api.cancel(r.id);
      if (res.status === 401) {
        renderGate();
        return;
      }
      if (res.ok) {
        toast(t.withdrawn(r.title));
        for (const results of [state.results, ...state.memo.values()]) {
          for (const item of results) {
            if (item.tmdb_id === r.tmdb_id && item.type === r.type) item.state = 'none';
          }
        }
        if (!list.hidden) {
          const active = state.active;
          renderResults(state.results);
          setActive(active);
        }
      } else {
        button.disabled = false;
        toast(
          res.status === 409 || res.status === 404 ? t.cannotWithdraw : t.withdrawFailed,
          { kind: 'bad' },
        );
      }
      refreshMine();
    });
    return button;
  };

  const mineItem = (r, group) => {
    const status = chip(r.status, STATUS_LABEL[r.status] || r.status);
    // IMDb once the request has an ID, the TMDB page until then.
    const href = r.imdb_id ? imdbUrl(r.imdb_id) : tmdbUrl(r.type, r.tmdb_id);
    const site = r.imdb_id ? 'IMDb' : 'TMDB';
    const li = el(
      'li',
      { class: 'mine-item', 'data-id': r.id },
      extLink(href, t.onSite(r.title, site), posterSmall(r.poster), true),
      el(
        'div',
        { class: 'opt-text' },
        el('p', { class: 'opt-title' }, extLink(href, t.onSite(r.title, site), r.title)),
        el('p', { class: 'opt-meta' }, r.year ? String(r.year) : null, badge(r.type), t.orderedOn(formatDate(r.created_at))),
      ),
      status,
    );
    if (r.can_cancel) li.append(cancelButton(r, li));
    const node = { id: r.id, li, status, state: r.status, group, key: keyOf(r) };
    mineNodes.set(r.id, node);
    return node;
  };

  // Puts a row into its group at its newest-first position. A row older
  // than everything loaded is left for the load-more button to fetch.
  const place = (group, node) => {
    if (group.cursor && group.last && newer(group.last, node.key)) {
      mineNodes.delete(node.id);
      return;
    }
    let before = null;
    for (const child of group.list.children) {
      const other = mineNodes.get(Number(child.dataset.id));
      if (other && newer(node.key, other.key)) {
        before = child;
        break;
      }
    }
    if (before) before.before(node.li);
    else group.list.append(node.li);
    node.group = group;
    if (!group.last || newer(group.last, node.key)) group.last = node.key;
  };

  const loadGroup = async (group, reset) => {
    group.more.disabled = true;
    const res = await api.mine(group.key, reset ? null : group.cursor);
    group.more.disabled = false;
    if (res.status === 401) {
      renderGate();
      return;
    }
    if (!res.ok || !res.data) {
      mineStatus.className = 'status status-bad';
      mineStatus.textContent = t.loadMineFailed;
      return;
    }
    mineStatus.textContent = '';
    if (reset) {
      for (const [id, node] of mineNodes) if (node.group === group) mineNodes.delete(id);
      clear(group.list);
      group.last = null;
    }
    for (const r of res.data.items) {
      // A row already shown (it moved since the last page) is not added twice.
      if (mineNodes.has(r.id)) continue;
      group.list.append(mineItem(r, group).li);
      group.last = keyOf(r);
    }
    group.cursor = res.data.next_cursor;
    paintGroup(group);
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

  // Re-reads the first page of both groups and updates rows in place: no
  // clearing, no scroll jump. A row whose status changed gets a toast and
  // moves to the group it now belongs to.
  const refreshMine = async () => {
    const fresh = [
      [pendingGroup, await api.mine('pending', null)],
      [earlierGroup, await api.mine('earlier', null)],
    ];
    if (fresh.some(([, res]) => res.status === 401)) {
      renderGate();
      return;
    }
    if (fresh.some(([, res]) => !res.ok || !res.data)) return;
    let resultsChanged = false;
    const seen = new Set();
    for (const [group, res] of fresh) {
      if (!group.last) group.cursor = res.data.next_cursor;
      for (const r of res.data.items) {
        seen.add(r.id);
        const known = mineNodes.get(r.id);
        if (!known) {
          place(group, mineItem(r, group));
        } else {
          if (!r.can_cancel) {
            const stale = known.li.querySelector('[data-cancel]');
            if (stale) stale.remove();
          }
          if (known.state !== r.status) {
            known.state = r.status;
            const chipNow = chip(r.status, STATUS_LABEL[r.status] || r.status);
            known.status.replaceWith(chipNow);
            known.status = chipNow;
            toast(t.nowStatus(r.title, STATUS_LABEL[r.status] || r.status));
          }
          if (known.group !== group) {
            known.li.remove();
            place(group, known);
          }
        }
        if (syncResults(r)) resultsChanged = true;
      }
    }
    // A row that should be on a group's first page but is on neither has
    // moved to a spot beyond a first page. Drop it; if the other group has
    // loaded past its first page, reload that group so it shows up.
    const reload = new Set();
    for (const [group, res] of fresh) {
      const items = res.data.items;
      const edge = res.data.next_cursor && items.length ? keyOf(items[items.length - 1]) : null;
      for (const node of [...mineNodes.values()]) {
        if (node.group !== group || seen.has(node.id)) continue;
        if (edge && newer(edge, node.key)) continue;
        node.li.remove();
        mineNodes.delete(node.id);
        const other = group === pendingGroup ? earlierGroup : pendingGroup;
        if (other.last) reload.add(other);
      }
    }
    for (const group of reload) await loadGroup(group, true);
    for (const group of [pendingGroup, earlierGroup]) paintGroup(group);
    if (resultsChanged && !list.hidden) {
      const active = state.active;
      renderResults(state.results);
      setActive(active);
    }
  };

  const mine = el(
    'section',
    { class: 'mine', 'aria-labelledby': 'mine-title' },
    el('h2', { id: 'mine-title', class: 'section-title', text: t.myRequests }),
    pendingGroup.section,
    earlierGroup.section,
    mineStatus,
  );

  const logout = async () => {
    await api.logout();
    renderGate();
  };

  clear(root);
  root.append(
    appBar(APP_NAME, greeting),
    el(
      'div',
      { class: 'page' },
      el('main', {}, el('section', { class: 'search', 'aria-label': t.searchLabel }, combo, status), mine),
      footer(logout),
      sheet,
    ),
  );
  input.focus();
  loadGroup(pendingGroup, true);
  loadGroup(earlierGroup, true);
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
    const retry = el('button', { class: 'btn', type: 'button', text: t.tryAgain, onclick: boot });
    root.append(
      el(
        'main',
        { class: 'gate' },
        el('div', { class: 'gate-form' }, el('h1', { text: APP_NAME }), el('p', { text: t.serverDown }), retry),
      ),
    );
  }
}

boot();
