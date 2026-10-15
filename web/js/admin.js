// The admin page: login, the request queue tabs, the archive with event
// history and CSV export, and member management. All data comes from the
// admin API; text goes into the page through textContent only.

import { call } from './api.js';
import { watch } from './live.js';
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
const TITLE = 'MediaQueue admin';

const TABS = [
  { id: 'pending', label: STATUS_LABEL.pending, status: 'pending', empty: 'Nothing pending.' },
  { id: 'added', label: STATUS_LABEL.added, status: 'added', empty: 'Nothing marked added yet.' },
  { id: 'refused', label: STATUS_LABEL.refused, status: 'refused', empty: 'Nothing declined.' },
  { id: 'already_had', label: STATUS_LABEL.already_had, status: 'already_had', empty: 'Nothing marked in library.' },
  { id: 'archive', label: 'All requests', status: 'all', empty: 'No requests yet.' },
  { id: 'members', label: 'Members' },
];

// Status buttons: every status but the current one.
const ACTIONS = [
  { status: 'added', label: 'Mark added' },
  { status: 'refused', label: 'Decline' },
  { status: 'already_had', label: 'Already in library' },
  { status: 'pending', label: 'Reopen' },
];

const EVENT_TEXT = {
  submitted: (e) => `Requested by ${e.actor}`,
  duplicate: (e) => `Requested again by ${e.actor}`,
  status: (e) => `${STATUS_LABEL[e.from_status] || e.from_status} → ${STATUS_LABEL[e.to_status] || e.to_status}`,
};

const get = (path) => call('GET', path);
// What the server gives a new member; only a different value is worth showing.

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
  const button = el('button', { class: 'btn btn-primary', type: 'submit', text: 'Sign in' });
  const error = el('p', { class: 'error', role: 'alert' });
  const form = el(
    'form',
    { class: 'gate-form' },
    el('h1', { text: TITLE }),
    el('label', { class: 'vh', for: 'user', text: 'Username' }),
    user,
    el('label', { class: 'vh', for: 'pass', text: 'Password' }),
    pass,
    button,
    error,
  );
  user.placeholder = 'Username';
  pass.placeholder = 'Password';
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
          error.textContent = `Too many attempts. Try again in ${formatWait(left)}.`;
        },
        () => {
          button.disabled = false;
          error.textContent = '';
        },
      );
      return;
    }
    button.disabled = false;
    error.textContent = res.status === 0 ? "Can't reach the server." : 'Wrong username or password.';
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
  const tablist = el('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Sections' }, tabButtons);

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
    placeholder: 'Filter by title, requester or IMDb ID',
    maxlength: 100,
  });
  const tbody = el('tbody');
  const empty = el('p', { class: 'muted', text: tab.empty, hidden: true });
  const more = el('button', { class: 'btn btn-small', type: 'button', text: 'Load more', hidden: true });
  const refresh = el('button', { class: 'btn btn-small', type: 'button', text: 'Refresh' });
  const toolbar = el(
    'div',
    { class: 'toolbar' },
    el('label', { class: 'vh', for: 'filter', text: 'Filter' }),
    filter,
    refresh,
    archive ? el('a', { class: 'btn btn-small', href: '/api/admin/export.csv', download: 'mediaqueue.csv', text: 'Export CSV' }) : null,
  );
  const head = el(
    'tr',
    {},
    ['', 'Title', 'Type', 'Requested by', 'Requested', 'IMDb', 'Status', 'Actions'].map((h) =>
      el('th', { scope: 'col' }, h ? h : el('span', { class: 'vh', text: 'Poster' })),
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
      say("Couldn't load requests.", 'status-bad');
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
    empty.textContent = q ? 'No requests match.' : tab.empty;
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
        el('strong', {}, item.imdb_id ? extLink(imdbUrl(item.imdb_id), `${item.title} on IMDb`, item.title) : item.title),
        item.year ? ` (${item.year})` : null,
        item.note ? el('p', { class: 'muted', text: `Note: ${item.note}` }) : null,
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
        toast(`“${item.title}” is now ${STATUS_LABEL[to]}.`, {
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
        toast(`Couldn't update “${item.title}”. Nothing was changed.`, { kind: 'bad' });
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
          'aria-label': `${action.label}: ${item.title}`,
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
              text: 'Open in DMM',
              'aria-label': `Open in DMM: ${item.title}`,
            }),
          );
        }
        actions.append(
          button(byStatus('added')),
          button(byStatus('refused')),
          menu('More', `More actions: ${item.title}`, [button(byStatus('already_had'))], busy),
        );
      } else {
        const others = ACTIONS.filter((a) => a.status !== item.status).map(button);
        actions.append(menu('Change status', `Change status: ${item.title}`, others, busy));
      }
      if (archive) actions.append(toggle);
    };

    const historyRow = el('tr', { hidden: true }, el('td', { colspan: 8 }));
    const toggle = el('button', {
      class: 'btn btn-small chevron',
      type: 'button',
      text: '›',
      'aria-expanded': 'false',
      'aria-label': `History: ${item.title}`,
    });
    const loadHistory = async () => {
      const cell = historyRow.firstChild;
      if (!cell.firstChild) cell.append(el('span', { class: 'muted', text: 'Loading…' }));
      const res = await get(`/api/admin/requests/${item.id}/events`);
      if (expired(res)) return;
      clear(cell);
      if (!res.ok) {
        cell.append(el('span', { class: 'status-bad', text: "Couldn't load history." }));
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
      if (tab.id === 'pending') toast(`New request: ${item.title}`);
    }
    if (!cursor && res.data.next_cursor && tbody.children.length === items.length) {
      cursor = res.data.next_cursor;
    }
    more.hidden = !cursor;
    empty.textContent = q ? 'No requests match.' : tab.empty;
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
  if (!item.imdb_id) return el('span', { class: 'muted', text: 'none' });
  const id = el('button', {
    class: 'code-btn',
    type: 'button',
    text: item.imdb_id,
    'aria-label': `Copy IMDb ID ${item.imdb_id}`,
  });
  copyOnClick(id, item.imdb_id, `Couldn't copy. The ID is ${item.imdb_id}.`);
  return id;
}

// ---------------------------------------------------------------------
// Members tab
// ---------------------------------------------------------------------

// Shows a freshly issued code exactly once, with a copy button.
function codeBox(name, code) {
  const copy = el('button', { class: 'btn btn-primary', type: 'button', text: 'Copy code' });
  copyOnClick(copy, code, "Couldn't copy. Select the code and copy it by hand.");
  return el(
    'div',
    { class: 'code-box', role: 'alert' },
    el('p', { text: `Access code for ${name}` }),
    el('output', { text: code }),
    el('p', { class: 'muted', text: 'You will not see this code again. Send it to them now; if it is lost, rotate it.' }),
    el('div', {}, copy),
  );
}

// A 409 from the member routes is either a taken name or a taken code.
function codeTaken(res) {
  return Boolean(res.data && /code/.test(res.data.message || ''));
}

function numberOrNull(value) {
  const v = value.trim();
  return v === '' ? null : Number(v);
}

// "3 devices, last seen 2 h ago", with the list behind a disclosure.
function devicesCell(m, deviceList) {
  if (!m.devices.length) return el('span', { class: 'muted', text: 'None' });
  const n = m.devices.length;
  const lastSeen = Math.max(...m.devices.map((d) => d.last_seen_at));
  return el(
    'details',
    {},
    el('summary', { text: `${n} device${n === 1 ? '' : 's'}, last seen ${formatAgo(lastSeen)}` }),
    deviceList,
  );
}

function renderMembers(panel, { say, expired }) {
  const codeSlot = el('div');
  const tbody = el('tbody');

  const name = el('input', { id: 'm-name', class: 'field', maxlength: 40, required: true });
  const daily = el('input', { id: 'm-daily', class: 'field', type: 'number', min: 0, max: 1000, inputmode: 'numeric', placeholder: 'No limit' });
  const code = el('input', { id: 'm-code', class: 'field', maxlength: 64, autocomplete: 'off', spellcheck: false });
  // Advice, not a block: the server accepts anything from 3 characters.
  const codeWarning = el('span', { class: 'muted', id: 'm-code-note' });
  code.setAttribute('aria-describedby', 'm-code-note');
  code.addEventListener('input', () => {
    const n = code.value.replace(/[\s-]/g, '').length;
    codeWarning.textContent = n > 0 && n < 6 ? 'Short codes are easy to guess.' : '';
  });
  const add = el('button', { class: 'btn btn-primary', type: 'submit', text: 'Add member' });
  const form = el(
    'form',
    { class: 'form-grid', 'aria-label': 'Add a member' },
    el('label', { for: 'm-name' }, 'Name', name),
    el('label', { for: 'm-daily' }, 'Daily limit (optional)', daily),
    el('label', { for: 'm-code' }, 'Access code (optional, 3+ characters; blank generates one)', code),
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
        toast(`Added ${res.data.name} with the code you chose.`);
      }
      form.reset();
      codeWarning.textContent = '';
      load();
    } else if (res.status === 409) {
      toast(
        codeTaken(res) ? 'That code is already in use.' : 'A member with that name already exists.',
        { kind: 'bad' },
      );
    } else {
      toast('Check the name (1 to 40 characters), the code (3 to 64 characters) and limits.', { kind: 'bad' });
    }
  });

  const head = el(
    'tr',
    {},
    ['Name', 'Last seen', 'Logged in', 'Requests', 'Limits', 'Actions'].map((h) => el('th', { scope: 'col', text: h })),
  );

  const memberRow = (m) => {
    const deviceList = el('ul', { class: 'events' });
    for (const d of m.devices) {
      const kill = el('button', {
        class: 'btn btn-small',
        type: 'button',
        text: 'Log out',
        'aria-label': `Log out ${m.name}'s device from ${formatDateTime(d.created_at)}`,
      });
      kill.addEventListener('click', async () => {
        const res = await send('DELETE', `/api/admin/sessions/${d.id}`);
        if (expired(res)) return;
        toast(res.ok ? `Logged out one of ${m.name}'s devices.` : "Couldn't log that device out.", { kind: res.ok ? 'ok' : 'bad' });
        load();
      });
      deviceList.append(el('li', {}, `Since ${formatDateTime(d.created_at)}, seen ${formatDateTime(d.last_seen_at)} `, kill));
    }
    const revoked = m.revoked_at !== null;
    const rotate = el('button', { class: 'btn btn-small', type: 'button', text: 'Rotate code', disabled: revoked });
    rotate.addEventListener('click', async () => {
      const chosen = window.prompt(
        `Give ${m.name} a new code? The old one and all their devices stop working.\n\nType the new code (3 to 64 characters), or leave blank to generate one.`,
      );
      if (chosen === null) return;
      const res = await send('POST', `/api/admin/members/${m.id}/rotate`, chosen.trim() === '' ? {} : { code: chosen });
      if (expired(res)) return;
      if (res.ok) {
        clear(codeSlot);
        if (res.data.code) {
          codeSlot.append(codeBox(m.name, res.data.code));
        } else {
          toast(`${m.name} now has the code you chose.`);
        }
        load();
      } else if (res.status === 409 && codeTaken(res)) {
        toast('That code is already in use.', { kind: 'bad' });
      } else if (res.status === 400) {
        toast('The code must be 3 to 64 characters.', { kind: 'bad' });
      } else {
        toast("Couldn't rotate the code.", { kind: 'bad' });
      }
    });
    const revoke = el('button', { class: 'btn btn-small', type: 'button', text: 'Revoke', disabled: revoked });
    revoke.addEventListener('click', async () => {
      if (!window.confirm(`Revoke ${m.name}? Their code and devices stop working for good.`)) return;
      const res = await send('POST', `/api/admin/members/${m.id}/revoke`);
      if (expired(res)) return;
      toast(res.ok ? `${m.name} is revoked.` : "Couldn't revoke.", { kind: res.ok ? 'ok' : 'bad' });
      load();
    });
    const edit = el('button', { class: 'btn btn-small', type: 'button', text: 'Edit', 'aria-label': `Edit ${m.name}` });
    edit.addEventListener('click', () => editMember(m, edit));
    const limits = el(
      'td',
      {},
      m.daily_limit === null ? el('span', { class: 'muted', text: 'No limit' }) : `${m.daily_limit} a day`,
    );
    return el(
      'tr',
      {},
      el('td', {}, el('strong', { text: m.name }), revoked ? el('p', {}, chip('refused', 'Revoked')) : null),
      el('td', { text: m.last_seen_at ? formatDateTime(m.last_seen_at) : 'Never' }),
      el('td', {}, devicesCell(m, deviceList)),
      el('td', { text: String(m.request_count) }),
      limits,
      el('td', {}, el('div', { class: 'row-actions' }, edit, rotate, revoke)),
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
      placeholder: 'No limit',
      value: m.daily_limit === null ? null : String(m.daily_limit),
    });
    const message = el('p', { class: 'status', role: 'status' });
    const cancel = el('button', { class: 'btn', type: 'button', text: 'Cancel', onclick: () => dialog.close() });
    const save = el('button', { class: 'btn btn-primary', type: 'submit', text: 'Save' });
    const form = el(
      'form',
      { method: 'dialog', class: 'form-grid' },
      el('h2', { id: 'edit-title', text: `Edit ${m.name}` }),
      el('label', { for: 'e-name' }, 'Name', eName),
      el('p', { class: 'muted', text: 'Renaming a member can hide older duplicate requests from their list.' }),
      el('label', { for: 'e-daily' }, 'Daily limit (blank for no limit)', eDaily),
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
        toast(`Saved ${eName.value.trim()}.`);
        load();
      } else {
        message.className = 'status status-bad';
        message.textContent =
          res.status === 409
            ? 'A member with that name already exists.'
            : res.status === 400
              ? 'Check the name (1 to 40 characters) and the limits.'
              : "Couldn't save. Nothing was changed.";
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
      say("Couldn't load members.", 'status-bad');
      return;
    }
    clear(tbody);
    for (const m of res.data.members) tbody.append(memberRow(m));
  };

  clear(panel);
  panel.append(
    el('h2', { class: 'vh', text: 'Members' }),
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
