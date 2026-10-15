// The admin page: login, the request queue tabs, the archive with event
// history and CSV export, and member management. All data comes from the
// admin API; text goes into the page through textContent only.

import { call } from './api.js';
import {
  STATUS_LABEL,
  TYPE_LABEL,
  chip,
  clear,
  countdown,
  el,
  footer,
  formatDateTime,
  formatWait,
  posterSmall,
} from './ui.js';

const root = document.getElementById('app');
const TITLE = 'MediaQueue admin';

const TABS = [
  { id: 'pending', label: STATUS_LABEL.pending, status: 'pending' },
  { id: 'added', label: STATUS_LABEL.added, status: 'added' },
  { id: 'refused', label: STATUS_LABEL.refused, status: 'refused' },
  { id: 'already_had', label: STATUS_LABEL.already_had, status: 'already_had' },
  { id: 'archive', label: 'All requests', status: 'all' },
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
const send = (method, path, body = {}) => call(method, path, { body });

// ---------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------

function renderLogin() {
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

  const tabButtons = TABS.map((tab) =>
    el('button', {
      id: `tab-${tab.id}`,
      class: 'tab',
      type: 'button',
      role: 'tab',
      'aria-selected': 'false',
      'aria-controls': 'panel',
      text: tab.label,
    }),
  );
  const tablist = el('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Sections' }, tabButtons);

  const select = (index) => {
    tabButtons.forEach((b, i) => {
      b.setAttribute('aria-selected', i === index ? 'true' : 'false');
      b.tabIndex = i === index ? 0 : -1;
    });
    panel.setAttribute('aria-labelledby', tabButtons[index].id);
    say('');
    const tab = TABS[index];
    if (tab.id === 'members') renderMembers(panel, { say, expired });
    else renderQueue(panel, tab, { say, expired });
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
}

// ---------------------------------------------------------------------
// Queue and archive tabs
// ---------------------------------------------------------------------

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
  const empty = el('p', { class: 'muted', text: 'Nothing here.', hidden: true });
  const more = el('button', { class: 'btn btn-small', type: 'button', text: 'Load more', hidden: true });
  const toolbar = el(
    'div',
    { class: 'toolbar' },
    el('label', { class: 'vh', for: 'filter', text: 'Filter' }),
    filter,
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

  const load = async (reset) => {
    const mine = ++generation;
    const params = new URLSearchParams({ status: tab.status });
    const q = filter.value.trim();
    if (q) params.set('q', q);
    if (!reset && cursor) params.set('cursor', cursor);
    more.disabled = true;
    const res = await get(`/api/admin/requests?${params}`);
    more.disabled = false;
    if (mine !== generation || expired(res)) return;
    if (!res.ok) {
      say("Couldn't load requests.", 'status-bad');
      return;
    }
    if (reset) clear(tbody);
    for (const item of res.data.items) tbody.append(...rowFor(item));
    cursor = res.data.next_cursor;
    more.hidden = !cursor;
    empty.hidden = tbody.children.length > 0;
  };

  // A request's table row, plus (in the archive) a hidden history row.
  const rowFor = (item) => {
    const statusCell = el('td', {}, chip(item.status, STATUS_LABEL[item.status] || item.status));
    const actions = el('div', { class: 'row-actions' });
    const row = el(
      'tr',
      {},
      el('td', {}, posterSmall(item.poster)),
      el(
        'td',
        {},
        el('strong', { text: item.title }),
        item.year ? ` (${item.year})` : null,
        item.note ? el('p', { class: 'muted', text: `Note: ${item.note}` }) : null,
      ),
      el('td', { text: TYPE_LABEL[item.type] || item.type }),
      el('td', {}, item.requester, item.times_requested > 1 ? el('span', { class: 'muted', text: ` +${item.times_requested - 1}` }) : null),
      el('td', { text: formatDateTime(item.created_at) }),
      el('td', {}, imdbCell(item, say)),
      statusCell,
      el('td', {}, actions),
    );

    const setStatus = async (to) => {
      const from = item.status;
      // Optimistic: update at once, put it back if the server says no.
      const leaves = !archive && to !== tab.status;
      item.status = to;
      if (leaves) row.hidden = true;
      paint();
      const res = await send('PATCH', `/api/admin/requests/${item.id}`, { status: to });
      if (expired(res)) return;
      if (res.ok) {
        if (leaves) {
          row.remove();
          historyRow.remove();
          empty.hidden = tbody.children.length > 0;
        }
        say(`“${item.title}” is now ${STATUS_LABEL[to]}.`, 'status-ok');
      } else {
        item.status = from;
        row.hidden = false;
        paint();
        say(`Couldn't update “${item.title}”. Nothing was changed.`, 'status-bad');
      }
    };

    const paint = () => {
      clear(statusCell);
      statusCell.append(chip(item.status, STATUS_LABEL[item.status] || item.status));
      clear(actions);
      for (const action of ACTIONS) {
        if (action.status === item.status) continue;
        actions.append(
          el('button', {
            class: 'btn btn-small',
            type: 'button',
            text: action.label,
            'aria-label': `${action.label}: ${item.title}`,
            onclick: () => setStatus(action.status),
          }),
        );
      }
      if (archive) actions.append(toggle);
    };

    const historyRow = el('tr', { hidden: true }, el('td', { colspan: 8 }));
    const toggle = el('button', {
      class: 'btn btn-small',
      type: 'button',
      text: 'History',
      'aria-expanded': 'false',
      'aria-label': `History: ${item.title}`,
    });
    toggle.addEventListener('click', async () => {
      const open = toggle.getAttribute('aria-expanded') === 'true';
      toggle.setAttribute('aria-expanded', open ? 'false' : 'true');
      historyRow.hidden = open;
      if (open) return;
      const cell = historyRow.firstChild;
      clear(cell);
      cell.append(el('span', { class: 'muted', text: 'Loading…' }));
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
    });

    paint();
    return archive ? [row, historyRow] : [row];
  };

  filter.addEventListener('input', () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(() => load(true), 250);
  });
  more.addEventListener('click', () => load(false));

  clear(panel);
  panel.append(toolbar, el('div', { class: 'table-wrap' }, table), empty, more);
  load(true);
}

function imdbCell(item, say) {
  if (!item.imdb_id) return el('span', { class: 'muted', text: 'none' });
  const copy = el('button', {
    class: 'btn btn-small',
    type: 'button',
    text: 'Copy',
    'aria-label': `Copy IMDb ID ${item.imdb_id}`,
  });
  copy.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(item.imdb_id);
      say(`Copied ${item.imdb_id}.`, 'status-ok');
    } catch {
      say(`Couldn't copy. The ID is ${item.imdb_id}.`, 'status-bad');
    }
  });
  return el(
    'div',
    { class: 'row-actions' },
    el('code', { text: item.imdb_id }),
    copy,
    item.dmm_url
      ? el('a', { class: 'btn btn-small', href: item.dmm_url, target: '_blank', rel: 'noopener noreferrer', text: 'Open in DMM' })
      : null,
  );
}

// ---------------------------------------------------------------------
// Members tab
// ---------------------------------------------------------------------

// Shows a freshly issued code exactly once, with a copy button.
function codeBox(name, code, say) {
  const copy = el('button', { class: 'btn btn-primary', type: 'button', text: 'Copy code' });
  copy.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(code);
      say('Code copied.', 'status-ok');
    } catch {
      say("Couldn't copy. Select the code and copy it by hand.", 'status-bad');
    }
  });
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

function renderMembers(panel, { say, expired }) {
  const codeSlot = el('div');
  const tbody = el('tbody');

  const name = el('input', { id: 'm-name', class: 'field', maxlength: 40, required: true });
  const daily = el('input', { id: 'm-daily', class: 'field', type: 'number', min: 0, max: 1000, inputmode: 'numeric' });
  const devices = el('input', { id: 'm-devices', class: 'field', type: 'number', min: 1, max: 20, inputmode: 'numeric' });
  const code = el('input', { id: 'm-code', class: 'field', maxlength: 64, autocomplete: 'off', spellcheck: false });
  const add = el('button', { class: 'btn btn-primary', type: 'submit', text: 'Add member' });
  const form = el(
    'form',
    { class: 'form-grid', 'aria-label': 'Add a member' },
    el('label', { for: 'm-name' }, 'Name', name),
    el('label', { for: 'm-daily' }, 'Daily limit (optional)', daily),
    el('label', { for: 'm-devices' }, 'Device limit (optional)', devices),
    el('label', { for: 'm-code' }, 'Access code (optional, 8+ characters; blank makes one)', code),
    add,
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const body = { name: name.value.trim() };
    if (numberOrNull(daily.value) !== null) body.daily_limit = numberOrNull(daily.value);
    if (numberOrNull(devices.value) !== null) body.max_sessions = numberOrNull(devices.value);
    if (code.value.trim() !== '') body.code = code.value;
    add.disabled = true;
    const res = await send('POST', '/api/admin/members', body);
    add.disabled = false;
    if (expired(res)) return;
    if (res.status === 201) {
      clear(codeSlot);
      if (res.data.code) {
        codeSlot.append(codeBox(res.data.name, res.data.code, say));
      } else {
        say(`Added ${res.data.name} with the code you chose.`, 'status-ok');
      }
      form.reset();
      load();
    } else if (res.status === 409) {
      say(
        codeTaken(res) ? 'That code is already in use.' : 'A member with that name already exists.',
        'status-bad',
      );
    } else {
      say('Check the name (1 to 40 characters), the code (8 to 64 characters) and limits.', 'status-bad');
    }
  });

  const head = el(
    'tr',
    {},
    ['Name', 'Last seen', 'Devices', 'Requests', 'Limits', 'Actions'].map((h) => el('th', { scope: 'col', text: h })),
  );

  const memberRow = (m, defaults) => {
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
        say(res.ok ? `Logged out one of ${m.name}'s devices.` : "Couldn't log that device out.", res.ok ? 'status-ok' : 'status-bad');
        load();
      });
      deviceList.append(el('li', {}, `Since ${formatDateTime(d.created_at)}, seen ${formatDateTime(d.last_seen_at)} `, kill));
    }
    const revoked = m.revoked_at !== null;
    const rotate = el('button', { class: 'btn btn-small', type: 'button', text: 'Rotate code', disabled: revoked });
    rotate.addEventListener('click', async () => {
      const chosen = window.prompt(
        `Give ${m.name} a new code? The old one and all their devices stop working.\n\nType the new code (8 to 64 characters), or leave blank to generate one.`,
      );
      if (chosen === null) return;
      const res = await send('POST', `/api/admin/members/${m.id}/rotate`, chosen.trim() === '' ? {} : { code: chosen });
      if (expired(res)) return;
      if (res.ok) {
        clear(codeSlot);
        if (res.data.code) {
          codeSlot.append(codeBox(m.name, res.data.code, say));
        } else {
          say(`${m.name} now has the code you chose.`, 'status-ok');
        }
        load();
      } else if (res.status === 409 && codeTaken(res)) {
        say('That code is already in use.', 'status-bad');
      } else if (res.status === 400) {
        say('The code must be 8 to 64 characters.', 'status-bad');
      } else {
        say("Couldn't rotate the code.", 'status-bad');
      }
    });
    const revoke = el('button', { class: 'btn btn-small', type: 'button', text: 'Revoke', disabled: revoked });
    revoke.addEventListener('click', async () => {
      if (!window.confirm(`Revoke ${m.name}? Their code and devices stop working for good.`)) return;
      const res = await send('POST', `/api/admin/members/${m.id}/revoke`);
      if (expired(res)) return;
      say(res.ok ? `${m.name} is revoked.` : "Couldn't revoke.", res.ok ? 'status-ok' : 'status-bad');
      load();
    });
    const limits = `${m.daily_limit ?? defaults.daily} a day, ${m.max_sessions} device${m.max_sessions === 1 ? '' : 's'}`;
    return el(
      'tr',
      {},
      el('td', {}, el('strong', { text: m.name }), revoked ? el('p', {}, chip('refused', 'Revoked')) : null),
      el('td', { text: m.last_seen_at ? formatDateTime(m.last_seen_at) : 'Never' }),
      el('td', {}, m.devices.length ? deviceList : el('span', { class: 'muted', text: 'None' })),
      el('td', { text: String(m.request_count) }),
      el('td', { text: limits }),
      el('td', {}, el('div', { class: 'row-actions' }, rotate, revoke)),
    );
  };

  const load = async () => {
    const res = await get('/api/admin/members');
    if (expired(res)) return;
    if (!res.ok) {
      say("Couldn't load members.", 'status-bad');
      return;
    }
    clear(tbody);
    const defaults = { daily: res.data.default_daily_limit };
    for (const m of res.data.members) tbody.append(memberRow(m, defaults));
  };

  clear(panel);
  panel.append(
    el('h2', { class: 'vh', text: 'Members' }),
    form,
    codeSlot,
    el('div', { class: 'table-wrap' }, el('table', { class: 'table' }, el('thead', {}, head), tbody)),
  );
  load();
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
