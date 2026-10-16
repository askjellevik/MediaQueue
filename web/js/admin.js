// The admin page: login, the request queue tabs, the archive with event
// history and CSV export, and member management. All data comes from the
// admin API; text goes into the page through textContent only.

import { call } from './api.js';
import { watch } from './live.js';
import { t } from './strings.js';
import {
  STATUS_LABEL,
  TYPE_LABEL,
  chip,
  clear,
  copyOnClick,
  countdown,
  el,
  extLink,
  footer,
  formatAgo,
  formatDateTime,
  formatWait,
  imdbUrl,
  posterSmall,
  toast,
} from './ui.js';

const root = document.getElementById('app');
const TITLE = t.adminTitle;

const TABS = [
  { id: 'pending', label: t.tabs.pending, status: 'pending', empty: t.empty.pending },
  { id: 'added', label: t.tabs.added, status: 'added', empty: t.empty.added },
  { id: 'refused', label: t.tabs.refused, status: 'refused', empty: t.empty.refused },
  { id: 'already_had', label: t.tabs.already_had, status: 'already_had', empty: t.empty.already_had },
  { id: 'archive', label: t.tabs.archive, status: 'all', empty: t.empty.archive },
  { id: 'members', label: t.tabs.members },
];

// Status buttons: every status but the current one.
const ACTIONS = [
  { status: 'added', label: t.actions.added },
  { status: 'refused', label: t.actions.refused },
  { status: 'already_had', label: t.actions.already_had },
  { status: 'pending', label: t.actions.pending },
];

const EVENT_TEXT = {
  submitted: (e) => t.orderedBy(e.actor),
  duplicate: (e) => t.orderedAgainBy(e.actor),
  status: (e) => `${STATUS_LABEL[e.from_status] || e.from_status} → ${STATUS_LABEL[e.to_status] || e.to_status}`,
};

const get = (path) => call('GET', path);

// Stops the live updates of the signed-in page when it goes away.
let stopLive = null;

const send = (method, path, body = {}) => call(method, path, { body });

// ---------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------

function renderLogin() {
  if (stopLive) stopLive();
  stopLive = null;
  document.title = TITLE;
  const user = el('input', { id: 'user', class: 'field', autocomplete: 'username', required: true });
  const pass = el('input', { id: 'pass', class: 'field', type: 'password', autocomplete: 'current-password', required: true });
  const button = el('button', { class: 'btn btn-primary', type: 'submit', text: t.signIn });
  const error = el('p', { class: 'error', role: 'alert' });
  const form = el(
    'form',
    { class: 'gate-form' },
    el('h1', { text: TITLE }),
    el('label', { class: 'sr-only', for: 'user', text: t.username }),
    user,
    el('label', { class: 'sr-only', for: 'pass', text: t.password }),
    pass,
    button,
    error,
  );
  user.placeholder = t.username;
  pass.placeholder = t.password;
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    button.disabled = true;
    error.textContent = '';
    const res = await send('POST', '/api/admin/login', { username: user.value, password: pass.value });
    pass.value = '';
    if (res.ok) {
      renderAdmin();
      return;
    }
    if (res.status === 429) {
      countdown(
        res.retryAfter || 60,
        (left) => {
          error.textContent = t.tooManyAttempts(formatWait(left));
        },
        () => {
          button.disabled = false;
          error.textContent = '';
        },
      );
      return;
    }
    button.disabled = false;
    error.textContent = res.status === 0 ? t.noServer : t.wrongLogin;
  });
  clear(root);
  root.append(el('main', { class: 'gate' }, form));
  user.focus();
}

// ---------------------------------------------------------------------
// Signed-in page
// ---------------------------------------------------------------------

function renderAdmin() {
  document.title = TITLE;
  const panel = el('div', { id: 'panel', role: 'tabpanel', tabindex: '-1' });
  const notice = el('p', { class: 'status', role: 'status', 'aria-live': 'polite' });
  const say = (text, cls = '') => {
    notice.className = `status ${cls}`;
    notice.textContent = text;
  };
  // Any 401 means the admin session ended (idle or absolute timeout).
  const expired = (res) => {
    if (res.status === 401) {
      renderLogin();
      return true;
    }
    return false;
  };

  // The section on show; its refresh() merges in what changed on the server.
  let current = null;
  const pendingCount = el('span', { class: 'count', hidden: true });
  const tabButtons = TABS.map((tab) =>
    el(
      'button',
      {
        id: `tab-${tab.id}`,
        class: 'tab',
        type: 'button',
        role: 'tab',
        'aria-selected': 'false',
        'aria-controls': 'panel',
      },
      tab.label,
      tab.id === 'pending' ? pendingCount : null,
    ),
  );

  const refreshCounts = async () => {
    const res = await get('/api/admin/counts');
    if (expired(res) || !res.ok) return;
    pendingCount.textContent = String(res.data.pending);
    pendingCount.hidden = res.data.pending === 0;
  };
  const tablist = el('div', { class: 'tabs', role: 'tablist', 'aria-label': t.sections }, tabButtons);

  const select = (index) => {
    tabButtons.forEach((b, i) => {
      b.setAttribute('aria-selected', i === index ? 'true' : 'false');
      b.tabIndex = i === index ? 0 : -1;
    });
    panel.setAttribute('aria-labelledby', tabButtons[index].id);
    say('');
    const tab = TABS[index];
    if (tab.id === 'members') current = renderMembers(panel, { say, expired });
    else current = renderQueue(panel, tab, { say, expired });
    if (location.hash !== `#${tab.id}`) history.replaceState(null, '', `#${tab.id}`);
  };
  tabButtons.forEach((button, i) => {
    button.addEventListener('click', () => select(i));
    button.addEventListener('keydown', (event) => {
      const moves = { ArrowRight: 1, ArrowLeft: -1 };
      let next = null;
      if (event.key in moves) next = (i + moves[event.key] + TABS.length) % TABS.length;
      if (event.key === 'Home') next = 0;
      if (event.key === 'End') next = TABS.length - 1;
      if (next !== null) {
        event.preventDefault();
        select(next);
        tabButtons[next].focus();
      }
    });
  });

  const logout = async () => {
    await send('POST', '/api/admin/logout');
    renderLogin();
  };

  // Every tick refetches the current section and the counts. A 401 from
  // either sends the page back to the login.
  const onTick = () => Promise.all([refreshCounts(), current ? current.refresh() : null]);

  clear(root);
  root.append(
    el(
      'div',
      { class: 'page wide' },
      el('header', { class: 'top' }, el('h1', { text: TITLE })),
      el('main', {}, tablist, notice, panel),
      footer(logout),
    ),
  );
  const fromHash = TABS.findIndex((t) => `#${t.id}` === location.hash);
  select(fromHash >= 0 ? fromHash : 0);
  refreshCounts();
  if (stopLive) stopLive();
  stopLive = watch('/api/admin/events', onTick);
}

// ---------------------------------------------------------------------
// Queue and archive tabs
// ---------------------------------------------------------------------

// One menu open at a time; it closes on an outside click or Escape.
document.addEventListener('click', (event) => {
  for (const menu of document.querySelectorAll('details.menu[open]')) {
    if (!menu.contains(event.target)) menu.open = false;
  }
});
document.addEventListener('keydown', (event) => {
  if (event.key !== 'Escape') return;
  const menu = document.querySelector('details.menu[open]');
  if (!menu) return;
  menu.open = false;
  menu.querySelector('summary').focus();
});

function menu(label, ariaLabel, items, busy) {
  return el(
    'details',
    { class: 'menu', inert: busy },
    el('summary', { class: 'btn btn-small', 'aria-label': ariaLabel, text: label }),
    el('div', { class: 'menu-list' }, items),
  );
}

function renderQueue(panel, tab, { say, expired }) {
  const archive = tab.id === 'archive';
  let cursor = null;
  let filterTimer = 0;
  let generation = 0;

  const filter = el('input', {
    id: 'filter',
    class: 'field',
    type: 'search',
    placeholder: t.filterPlaceholder,
    maxlength: 100,
  });
  const tbody = el('tbody');
  const empty = el('p', { class: 'muted', text: tab.empty, hidden: true });
  const more = el('button', { class: 'btn btn-small', type: 'button', text: t.loadMore, hidden: true });
  const refresh = el('button', { class: 'btn btn-small', type: 'button', text: t.refresh });
  const toolbar = el(
    'div',
    { class: 'toolbar' },
    el('label', { class: 'sr-only', for: 'filter', text: t.filter }),
    filter,
    refresh,
    archive ? el('a', { class: 'btn btn-small', href: '/api/admin/export.csv', download: 'mediaqueue.csv', text: t.exportCsv }) : null,
  );
  const head = el(
    'tr',
    {},
    ['', ...t.queueHead].map((h) =>
      el('th', { scope: 'col' }, h ? h : el('span', { class: 'sr-only', text: t.poster })),
    ),
  );
  const table = el('table', { class: 'table' }, el('thead', {}, head), tbody);

  // Rows on show, by request id, so a refresh can update them in place.
  const known = new Map();
  // Loads in flight; a refresh waits for the page to settle first.
  let loading = 0;
  let loaded = false;

  const load = async (reset) => {
    const mine = ++generation;
    loading++;
    const params = new URLSearchParams({ status: tab.status });
    const q = filter.value.trim();
    if (q) params.set('q', q);
    if (!reset && cursor) params.set('cursor', cursor);
    more.disabled = true;
    refresh.disabled = true;
    const res = await get(`/api/admin/requests?${params}`);
    loading--;
    more.disabled = false;
    refresh.disabled = false;
    if (mine !== generation || expired(res)) return;
    if (!res.ok) {
      say(t.loadRequestsFailed, 'status-bad');
      return;
    }
    if (reset) {
      clear(tbody);
      known.clear();
    }
    for (const item of res.data.items) {
      const shown = known.get(item.id);
      if (shown && shown.row.isConnected) continue;
      tbody.append(...rowFor(item).nodes);
    }
    loaded = true;
    cursor = res.data.next_cursor;
    more.hidden = !cursor;
    empty.textContent = q ? t.noRequestsMatch : tab.empty;
    empty.hidden = tbody.children.length > 0;
  };

  // A request's table row, plus (in the archive) a hidden history row.
  const rowFor = (item) => {
    const requesterCell = el('td');
    const paintRequester = () => {
      clear(requesterCell);
      requesterCell.append(
        item.requester,
        item.times_requested > 1 ? el('span', { class: 'muted', text: ` +${item.times_requested - 1}` }) : null,
      );
    };
    paintRequester();
    const statusCell = el('td', {}, chip(item.status, STATUS_LABEL[item.status] || item.status));
    const actions = el('div', { class: 'row-actions' });
    const row = el(
      'tr',
      {},
      el('td', {}, posterSmall(item.poster)),
      el(
        'td',
        {},
        el('strong', {}, item.imdb_id ? extLink(imdbUrl(item.imdb_id), t.onSite(item.title, 'IMDb'), item.title) : item.title),
        item.year ? ` (${item.year})` : null,
        item.note ? el('p', { class: 'muted', text: t.note(item.note) }) : null,
      ),
      el('td', { text: TYPE_LABEL[item.type] || item.type }),
      requesterCell,
      el('td', { text: formatDateTime(item.created_at) }),
      el('td', {}, imdbCell(item)),
      statusCell,
      el('td', {}, actions),
    );

    // True while a status change is in flight; the buttons stay disabled.
    let busy = false;
    // Where the row was before it left the tab, so Undo can put it back.
    let anchor = null;

    const setStatus = async (to, isUndo = false) => {
      const from = item.status;
      if (busy || from === to) return;
      // Optimistic: update at once, put it back if the server says no.
      const leaves = !archive && to !== tab.status;
      const returns = !archive && !row.isConnected && to === tab.status;
      busy = true;
      item.status = to;
      if (leaves) {
        anchor = row.nextSibling;
        row.remove();
      } else if (returns) {
        tbody.insertBefore(row, anchor && anchor.parentNode === tbody ? anchor : null);
      }
      empty.hidden = tbody.children.length > 0;
      paint();
      const res = await send('PATCH', `/api/admin/requests/${item.id}`, { status: to });
      busy = false;
      if (expired(res)) return;
      if (res.ok) {
        paint();
        toast(t.statusChanged(item.title, STATUS_LABEL[to]), {
          undo: isUndo ? null : () => setStatus(from, true),
        });
        // A history that is open would otherwise miss the new event.
        if (toggle.getAttribute('aria-expanded') === 'true') loadHistory();
      } else {
        item.status = from;
        if (leaves) tbody.insertBefore(row, anchor && anchor.parentNode === tbody ? anchor : null);
        else if (returns) row.remove();
        empty.hidden = tbody.children.length > 0;
        paint();
        toast(t.updateFailed(item.title), { kind: 'bad' });
      }
    };

    const paint = () => {
      clear(statusCell);
      statusCell.append(chip(item.status, STATUS_LABEL[item.status] || item.status));
      clear(actions);
      const button = (action) =>
        el('button', {
          class: 'btn btn-small',
          type: 'button',
          text: action.label,
          'aria-label': t.actionLabel(action.label, item.title),
          disabled: busy,
          onclick: () => {
            for (const open of actions.querySelectorAll('details[open]')) open.open = false;
            setStatus(action.status);
          },
        });
      const byStatus = (status) => ACTIONS.find((a) => a.status === status);
      if (tab.id === 'pending') {
        if (item.dmm_url) {
          actions.append(
            el('a', {
              class: 'btn btn-small',
              href: item.dmm_url,
              target: '_blank',
              rel: 'noopener noreferrer',
              text: t.openInDmm,
              'aria-label': t.actionLabel(t.openInDmm, item.title),
            }),
          );
        }
        actions.append(
          button(byStatus('added')),
          button(byStatus('refused')),
          menu(t.more, t.moreActions(item.title), [button(byStatus('already_had'))], busy),
        );
      } else {
        const others = ACTIONS.filter((a) => a.status !== item.status).map(button);
        actions.append(menu(t.changeStatus, t.changeStatusFor(item.title), others, busy));
      }
      if (archive) actions.append(toggle);
    };

    const historyRow = el('tr', { hidden: true }, el('td', { colspan: 8 }));
    const toggle = el('button', {
      class: 'btn btn-small chevron',
      type: 'button',
      text: '›',
      'aria-expanded': 'false',
      'aria-label': t.history(item.title),
    });
    const loadHistory = async () => {
      const cell = historyRow.firstChild;
      if (!cell.firstChild) cell.append(el('span', { class: 'muted', text: t.loading }));
      const res = await get(`/api/admin/requests/${item.id}/events`);
      if (expired(res)) return;
      clear(cell);
      if (!res.ok) {
        cell.append(el('span', { class: 'status-bad', text: t.historyFailed }));
        return;
      }
      cell.append(
        el(
          'ol',
          { class: 'events' },
          res.data.events.map((e) =>
            el(
              'li',
              {},
              `${formatDateTime(e.created_at)}: ${(EVENT_TEXT[e.kind] || (() => e.kind))(e)}`,
              e.note ? el('span', { class: 'muted', text: ` (“${e.note}”)` }) : null,
            ),
          ),
        ),
      );
    };
    toggle.addEventListener('click', () => {
      const open = toggle.getAttribute('aria-expanded') === 'true';
      toggle.setAttribute('aria-expanded', open ? 'false' : 'true');
      historyRow.hidden = open;
      if (open) return;
      // Fetched again on every open, so it is never stale.
      clear(historyRow.firstChild);
      loadHistory();
    });

    // A newer copy of this request from the server. A change in flight wins.
    const update = (fresh) => {
      if (fresh.times_requested !== item.times_requested) {
        item.times_requested = fresh.times_requested;
        paintRequester();
      }
      if (!busy && fresh.status !== item.status) {
        item.status = fresh.status;
        paint();
      }
    };

    paint();
    const entry = { item, row, nodes: archive ? [row, historyRow] : [row], update };
    known.set(item.id, entry);
    return entry;
  };

  // Is `item` at or before `last` in the list's order (newest first)?
  const withinPage = (item, last) =>
    item.created_at > last.created_at || (item.created_at === last.created_at && item.id >= last.id);

  // Merges the first page of the server's list into what is on show:
  // rows update where they are (open history stays open), new ones appear,
  // and ones that no longer belong here go. Older pages are left alone.
  const merge = async () => {
    if (loading > 0 || !loaded) return;
    const mine = generation;
    const params = new URLSearchParams({ status: tab.status });
    const q = filter.value.trim();
    if (q) params.set('q', q);
    const res = await get(`/api/admin/requests?${params}`);
    if (expired(res) || !res.ok || mine !== generation) return;

    const items = res.data.items;
    const fresh = new Set(items.map((i) => i.id));
    if (items.length > 0) {
      const last = items[items.length - 1];
      for (const [id, entry] of known) {
        if (!entry.row.isConnected || fresh.has(id)) continue;
        if (!res.data.next_cursor || withinPage(entry.item, last)) {
          for (const node of entry.nodes) node.remove();
          known.delete(id);
        }
      }
    } else {
      for (const [id, entry] of known) {
        if (!entry.row.isConnected) continue;
        for (const node of entry.nodes) node.remove();
        known.delete(id);
      }
    }

    let previous = null;
    for (const item of items) {
      const entry = known.get(item.id);
      if (entry && entry.row.isConnected) {
        entry.update(item);
        previous = entry.nodes[entry.nodes.length - 1];
        continue;
      }
      const added = rowFor(item);
      if (previous) previous.after(...added.nodes);
      else tbody.prepend(...added.nodes);
      previous = added.nodes[added.nodes.length - 1];
      if (tab.id === 'pending') toast(t.newOrder(item.title));
    }
    if (!cursor && res.data.next_cursor && tbody.children.length === items.length) {
      cursor = res.data.next_cursor;
    }
    more.hidden = !cursor;
    empty.textContent = q ? t.noRequestsMatch : tab.empty;
    empty.hidden = tbody.children.length > 0;
  };

  filter.addEventListener('input', () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(() => load(true), 250);
  });
  refresh.addEventListener('click', () => load(true));
  more.addEventListener('click', () => load(false));

  clear(panel);
  panel.append(toolbar, el('div', { class: 'table-wrap' }, table), empty, more);
  load(true);
  return { refresh: merge };
}

function imdbCell(item) {
  if (!item.imdb_id) return el('span', { class: 'muted', text: t.none });
  const id = el('button', {
    class: 'code-btn',
    type: 'button',
    text: item.imdb_id,
    'aria-label': t.copyImdb(item.imdb_id),
  });
  copyOnClick(id, item.imdb_id, t.copyImdbFailed(item.imdb_id));
  return id;
}

// ---------------------------------------------------------------------
// Members tab
// ---------------------------------------------------------------------

// Shows a freshly issued code exactly once, with a copy button.
function codeBox(name, code) {
  const copy = el('button', { class: 'btn btn-primary', type: 'button', text: t.copyCode });
  copyOnClick(copy, code, t.copyCodeFailed);
  return el(
    'div',
    { class: 'code-box', role: 'alert' },
    el('p', { text: t.codeFor(name) }),
    el('output', { text: code }),
    el('p', { class: 'muted', text: t.codeOnce }),
    el('div', {}, copy),
  );
}

// A 409 from the member routes is either a taken name or a taken code.
function codeTaken(res) {
  return Boolean(res.data && /koden/.test(res.data.message || ''));
}

function numberOrNull(value) {
  const v = value.trim();
  return v === '' ? null : Number(v);
}

// "3 enheter, sist sett 2 t siden", with the list behind a disclosure.
function devicesCell(m, deviceList) {
  if (!m.devices.length) return el('span', { class: 'muted', text: t.noDevices });
  const n = m.devices.length;
  const lastSeen = Math.max(...m.devices.map((d) => d.last_seen_at));
  return el(
    'details',
    {},
    el('summary', { text: t.devices(n, formatAgo(lastSeen)) }),
    deviceList,
  );
}

function renderMembers(panel, { say, expired }) {
  const codeSlot = el('div');
  const tbody = el('tbody');

  const name = el('input', { id: 'm-name', class: 'field', maxlength: 40, required: true });
  const daily = el('input', { id: 'm-daily', class: 'field', type: 'number', min: 0, max: 1000, inputmode: 'numeric', placeholder: t.noLimit });
  const code = el('input', { id: 'm-code', class: 'field', maxlength: 64, autocomplete: 'off', spellcheck: false });
  // Advice, not a block: the server accepts anything from 3 characters.
  const codeWarning = el('span', { class: 'muted', id: 'm-code-note' });
  code.setAttribute('aria-describedby', 'm-code-note');
  code.addEventListener('input', () => {
    const n = code.value.replace(/[\s-]/g, '').length;
    codeWarning.textContent = n > 0 && n < 6 ? t.shortCode : '';
  });
  const add = el('button', { class: 'btn btn-primary', type: 'submit', text: t.addUser });
  const form = el(
    'form',
    { class: 'form-grid', 'aria-label': t.addUserLabel },
    el('label', { for: 'm-name' }, t.name, name),
    el('label', { for: 'm-daily' }, t.dailyLimitOptional, daily),
    el('label', { for: 'm-code' }, t.codeOptional, code),
    codeWarning,
    add,
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const body = { name: name.value.trim() };
    if (numberOrNull(daily.value) !== null) body.daily_limit = numberOrNull(daily.value);
    if (code.value.trim() !== '') body.code = code.value;
    add.disabled = true;
    const res = await send('POST', '/api/admin/members', body);
    add.disabled = false;
    if (expired(res)) return;
    if (res.status === 201) {
      clear(codeSlot);
      if (res.data.code) {
        codeSlot.append(codeBox(res.data.name, res.data.code));
      } else {
        toast(t.addedWithCode(res.data.name));
      }
      form.reset();
      codeWarning.textContent = '';
      load();
    } else if (res.status === 409) {
      toast(codeTaken(res) ? t.codeTaken : t.nameTaken, { kind: 'bad' });
    } else {
      toast(t.checkAddFields, { kind: 'bad' });
    }
  });

  const head = el(
    'tr',
    {},
    t.userHead.map((h) => el('th', { scope: 'col', text: h })),
  );

  const memberRow = (m) => {
    const deviceList = el('ul', { class: 'events' });
    for (const d of m.devices) {
      const kill = el('button', {
        class: 'btn btn-small',
        type: 'button',
        text: t.logOut,
        'aria-label': t.deviceLogOutLabel(m.name, formatDateTime(d.created_at)),
      });
      kill.addEventListener('click', async () => {
        const res = await send('DELETE', `/api/admin/sessions/${d.id}`);
        if (expired(res)) return;
        toast(res.ok ? t.deviceLoggedOut(m.name) : t.deviceLogOutFailed, { kind: res.ok ? 'ok' : 'bad' });
        load();
      });
      deviceList.append(el('li', {}, t.deviceLine(formatDateTime(d.created_at), formatDateTime(d.last_seen_at)), kill));
    }
    const revoked = m.revoked_at !== null;
    const rotate = el('button', { class: 'btn btn-small', type: 'button', text: t.rotate, disabled: revoked });
    rotate.addEventListener('click', async () => {
      const chosen = window.prompt(t.rotatePrompt(m.name));
      if (chosen === null) return;
      const res = await send('POST', `/api/admin/members/${m.id}/rotate`, chosen.trim() === '' ? {} : { code: chosen });
      if (expired(res)) return;
      if (res.ok) {
        clear(codeSlot);
        if (res.data.code) {
          codeSlot.append(codeBox(m.name, res.data.code));
        } else {
          toast(t.rotatedToChosen(m.name));
        }
        load();
      } else if (res.status === 409 && codeTaken(res)) {
        toast(t.codeTaken, { kind: 'bad' });
      } else if (res.status === 400) {
        toast(t.codeLength, { kind: 'bad' });
      } else {
        toast(t.rotateFailed, { kind: 'bad' });
      }
    });
    const revoke = el('button', { class: 'btn btn-small', type: 'button', text: revoked ? t.activate : t.deactivate });
    revoke.addEventListener('click', async () => {
      if (!revoked && !window.confirm(t.deactivateConfirm(m.name))) {
        return;
      }
      const res = await send('POST', `/api/admin/members/${m.id}/${revoked ? 'restore' : 'revoke'}`);
      if (expired(res)) return;
      const done = revoked ? t.activatedDone(m.name) : t.deactivatedDone(m.name);
      toast(res.ok ? done : revoked ? t.activateFailed : t.deactivateFailed, {
        kind: res.ok ? 'ok' : 'bad',
      });
      load();
    });
    // Only a deactivated user can go; their requests stay under their name.
    const remove = el('button', { class: 'btn btn-small', type: 'button', text: t.delete });
    remove.addEventListener('click', async () => {
      if (!window.confirm(t.deleteConfirm(m.name))) {
        return;
      }
      const res = await send('DELETE', `/api/admin/members/${m.id}`);
      if (expired(res)) return;
      toast(
        res.ok ? t.deleted(m.name) : res.status === 409 ? t.deactivateFirst(m.name) : t.deleteFailed,
        { kind: res.ok ? 'ok' : 'bad' },
      );
      load();
    });
    const edit = el('button', { class: 'btn btn-small', type: 'button', text: t.edit, 'aria-label': t.editLabel(m.name) });
    edit.addEventListener('click', () => editMember(m, edit));
    const limits = el(
      'td',
      {},
      m.daily_limit === null ? el('span', { class: 'muted', text: t.noLimit }) : t.perDay(m.daily_limit),
    );
    return el(
      'tr',
      {},
      el('td', {}, el('strong', { text: m.name }), revoked ? el('p', {}, chip('refused', t.deactivated)) : null),
      el('td', { text: m.last_seen_at ? formatDateTime(m.last_seen_at) : t.never }),
      el('td', {}, devicesCell(m, deviceList)),
      el('td', { text: String(m.request_count) }),
      limits,
      el('td', {}, el('div', { class: 'row-actions' }, edit, rotate, revoke, revoked ? remove : null)),
    );
  };

  // Name and daily limit (blank = none) in a dialog.
  const editMember = (m, opener) => {
    const dialog = el('dialog', { class: 'sheet', 'aria-labelledby': 'edit-title' });
    const eName = el('input', { id: 'e-name', class: 'field', maxlength: 40, required: true, value: m.name });
    const eDaily = el('input', {
      id: 'e-daily',
      class: 'field',
      type: 'number',
      min: 0,
      max: 1000,
      inputmode: 'numeric',
      placeholder: t.noLimit,
      value: m.daily_limit === null ? null : String(m.daily_limit),
    });
    const message = el('p', { class: 'status', role: 'status' });
    const cancel = el('button', { class: 'btn', type: 'button', text: t.cancel, onclick: () => dialog.close() });
    const save = el('button', { class: 'btn btn-primary', type: 'submit', text: t.save });
    const form = el(
      'form',
      { method: 'dialog', class: 'form-grid' },
      el('h2', { id: 'edit-title', text: t.editLabel(m.name) }),
      el('label', { for: 'e-name' }, t.name, eName),
      el('p', { class: 'muted', text: t.renameWarning }),
      el('label', { for: 'e-daily' }, t.dailyLimitBlank, eDaily),
      message,
      el('div', { class: 'actions' }, cancel, save),
    );
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      save.disabled = true;
      message.className = 'status';
      message.textContent = '';
      const res = await send('PATCH', `/api/admin/members/${m.id}`, {
        name: eName.value.trim(),
        daily_limit: numberOrNull(eDaily.value),
      });
      save.disabled = false;
      if (expired(res)) {
        dialog.close();
        return;
      }
      if (res.ok) {
        dialog.close();
        toast(t.saved(eName.value.trim()));
        load();
      } else {
        message.className = 'status status-bad';
        message.textContent =
          res.status === 409 ? t.nameTaken : res.status === 400 ? t.checkEditFields : t.saveFailed;
      }
    });
    dialog.addEventListener('close', () => {
      dialog.remove();
      opener.focus();
    });
    dialog.append(form);
    document.body.append(dialog);
    dialog.showModal();
    eName.focus();
  };

  const load = async () => {
    const res = await get('/api/admin/members');
    if (expired(res)) return;
    if (!res.ok) {
      say(t.loadUsersFailed, 'status-bad');
      return;
    }
    clear(tbody);
    for (const m of res.data.members) tbody.append(memberRow(m));
  };

  clear(panel);
  panel.append(
    el('h2', { class: 'sr-only', text: t.users }),
    form,
    codeSlot,
    el('div', { class: 'table-wrap' }, el('table', { class: 'table' }, el('thead', {}, head), tbody)),
  );
  load();
  return { refresh: load };
}

// ---------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------

async function boot() {
  const res = await get('/api/admin/members');
  if (res.ok) renderAdmin();
  else renderLogin();
}

boot();
