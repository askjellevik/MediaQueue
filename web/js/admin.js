// The admin page: login, the request queue tabs, the archive with event
// history and CSV export, and member management. All data comes from the
// admin API; text goes into the page through textContent only.

import { call } from './api.js';
import { watch } from './live.js';
import { t } from './strings.js';
import { ta } from './strings-admin.js';
import {
  STATUS_LABEL,
  appBar,
  appIcon,
  badge,
  proBadge,
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
const TITLE = ta.adminTitle;

const TABS = [
  { id: 'pending', label: ta.tabs.pending, status: 'pending', empty: ta.empty.pending },
  { id: 'added', label: ta.tabs.added, status: 'added', empty: ta.empty.added },
  { id: 'refused', label: ta.tabs.refused, status: 'refused', empty: ta.empty.refused },
  { id: 'already_had', label: ta.tabs.already_had, status: 'already_had', empty: ta.empty.already_had },
  { id: 'archive', label: ta.tabs.archive, status: 'all', empty: ta.empty.archive },
  { id: 'jobs', label: ta.tabs.jobs },
  { id: 'members', label: ta.tabs.members },
];

// Status buttons: every status but the current one.
const ACTIONS = [
  { status: 'added', label: ta.actions.added },
  { status: 'refused', label: ta.actions.refused },
  { status: 'already_had', label: ta.actions.already_had },
  { status: 'pending', label: ta.actions.pending },
];

const EVENT_TEXT = {
  submitted: (e) => ta.orderedBy(e.actor),
  duplicate: (e) => ta.orderedAgainBy(e.actor),
  status: (e) => `${STATUS_LABEL[e.from_status] || e.from_status} → ${STATUS_LABEL[e.to_status] || e.to_status}`,
};

const get = (path) => call('GET', path);

// Matches the server's limit on a reason.
const REASON_MAX = 300;

// Asks why a request is refused; the answer is optional. Resolves with the
// text ('' for none), or null if the admin backs out.
const askReason = (item, opener, current, confirmLabel) =>
  new Promise((resolve) => {
    const dialog = el('dialog', { class: 'sheet', 'aria-labelledby': 'reason-title' });
    const field = el('textarea', {
      id: 'reason',
      class: 'field note',
      maxlength: REASON_MAX,
      rows: 3,
      placeholder: ta.reasonPlaceholder,
      'aria-describedby': 'reason-count',
    });
    field.value = current;
    const counter = el('p', { id: 'reason-count', class: 'counter' });
    const count = () => {
      counter.textContent = `${field.value.length} / ${REASON_MAX}`;
    };
    field.addEventListener('input', count);
    count();
    let answer = null;
    const cancel = el('button', { class: 'btn', type: 'button', text: t.cancel, onclick: () => dialog.close() });
    const form = el(
      'form',
      { method: 'dialog', class: 'form-grid' },
      el('h2', { id: 'reason-title', text: ta.reasonTitle(item.title) }),
      el('label', { for: 'reason' }, ta.reasonLabel, field),
      counter,
      el('p', { class: 'muted', text: ta.reasonHelp }),
      el('div', { class: 'actions' }, cancel, el('button', { class: 'btn btn-primary', type: 'submit', text: confirmLabel })),
    );
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      answer = field.value.trim();
      dialog.close();
    });
    dialog.addEventListener('close', () => {
      dialog.remove();
      opener.focus();
      resolve(answer);
    });
    dialog.append(form);
    document.body.append(dialog);
    dialog.showModal();
    field.focus();
  });

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
  const button = el('button', { class: 'btn btn-primary', type: 'submit', text: ta.signIn });
  const error = el('p', { class: 'error', role: 'alert' });
  const form = el(
    'form',
    { class: 'gate-form' },
    appIcon('mx-auto size-14 rounded-2xl shadow-md ring-1 ring-black/10'),
    el('h1', { text: TITLE }),
    el('label', { class: 'sr-only', for: 'user', text: ta.username }),
    user,
    el('label', { class: 'sr-only', for: 'pass', text: ta.password }),
    pass,
    button,
    error,
  );
  user.placeholder = ta.username;
  pass.placeholder = ta.password;
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
    error.textContent = res.status === 0 ? t.noServer : ta.wrongLogin;
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
  const notice = el('p', { class: 'status notice', role: 'status', 'aria-live': 'polite' });
  const say = (text, cls = '') => {
    notice.className = `status notice ${cls}`;
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
  const tablist = el('div', { class: 'tabs', role: 'tablist', 'aria-label': ta.sections }, tabButtons);

  const select = (index) => {
    tabButtons.forEach((b, i) => {
      b.setAttribute('aria-selected', i === index ? 'true' : 'false');
      b.tabIndex = i === index ? 0 : -1;
    });
    panel.setAttribute('aria-labelledby', tabButtons[index].id);
    say('');
    const tab = TABS[index];
    if (tab.id === 'members') current = renderMembers(panel, { say, expired });
    else if (tab.id === 'jobs') current = renderJobs(panel, { say, expired });
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
    appBar(TITLE, null, true),
    el('div', { class: 'page wide' }, el('main', {}, tablist, notice, panel), footer(logout)),
  );
  const fromHash = TABS.findIndex((tab) => `#${tab.id}` === location.hash);
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
    placeholder: ta.filterPlaceholder,
    maxlength: 100,
  });
  const list = el('ul', { class: 'req-list' });
  const empty = el('p', { class: 'muted', text: tab.empty, hidden: true });
  const more = el('button', { class: 'btn btn-small', type: 'button', text: t.loadMore, hidden: true });
  const refresh = el('button', { class: 'btn btn-small', type: 'button', text: ta.refresh });
  const toolbar = el(
    'div',
    { class: 'toolbar' },
    el('label', { class: 'sr-only', for: 'filter', text: ta.filter }),
    filter,
    refresh,
    archive ? el('a', { class: 'btn btn-small', href: '/api/admin/export.csv', download: 'mediaqueue.csv', text: ta.exportCsv }) : null,
  );

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
      say(ta.loadRequestsFailed, 'status-bad');
      return;
    }
    if (reset) {
      clear(list);
      known.clear();
    }
    for (const item of res.data.items) {
      const shown = known.get(item.id);
      if (shown && shown.row.isConnected) continue;
      list.append(...rowFor(item).nodes);
    }
    loaded = true;
    cursor = res.data.next_cursor;
    more.hidden = !cursor;
    empty.textContent = q ? ta.noRequestsMatch : tab.empty;
    empty.hidden = list.children.length > 0;
  };

  // A request's card. In the archive it also holds a hidden history block.
  const rowFor = (item) => {
    const meta = el('p', { class: 'req-meta' });
    const paintMeta = () => {
      const again = item.times_requested > 1 ? ` +${item.times_requested - 1}` : '';
      meta.textContent = ta.orderedByOn(`${item.requester}${again}`, formatDateTime(item.created_at));
    };
    paintMeta();
    const reasonLine = el('p', { class: 'req-note', hidden: true });
    const statusCell = el('span');
    const actions = el('div', { class: 'row-actions' });
    const historyBox = el('div', { class: 'req-history', hidden: true });
    const heading = item.imdb_id
      ? extLink(imdbUrl(item.imdb_id), t.onSite(item.title, 'IMDb'), item.title)
      : item.title;
    const row = el(
      'li',
      { class: 'req-card', 'data-id': item.id },
      posterSmall(item.poster),
      el(
        'div',
        { class: 'min-w-0' },
        el('p', { class: 'req-title' }, el('strong', {}, heading), item.year ? ` (${item.year})` : null, badge(item.type), proBadge(item.infuse_pro ? ta.withPro : ta.withoutPro)),
        meta,
        item.note ? el('p', { class: 'req-note', text: ta.note(item.note) }) : null,
        reasonLine,
      ),
      el('div', { class: 'req-side' }, statusCell, actions),
      archive ? historyBox : null,
    );

    // True while a status change is in flight; the buttons stay disabled.
    let busy = false;
    // Where the row was before it left the tab, so Undo can put it back.
    let anchor = null;

    const setStatus = async (to, isUndo = false, reason = '') => {
      const from = item.status;
      const fromReason = item.reason;
      if (busy || from === to) return;
      // Optimistic: update at once, put it back if the server says no.
      const leaves = !archive && to !== tab.status;
      const returns = !archive && !row.isConnected && to === tab.status;
      busy = true;
      item.status = to;
      item.reason = to === 'refused' && reason ? reason : null;
      if (leaves) {
        anchor = row.nextSibling;
        row.remove();
      } else if (returns) {
        list.insertBefore(row, anchor && anchor.parentNode === list ? anchor : null);
      }
      empty.hidden = list.children.length > 0;
      paint();
      const res = await send('PATCH', `/api/admin/requests/${item.id}`, to === 'refused' && reason ? { status: to, reason } : { status: to });
      busy = false;
      if (expired(res)) return;
      if (res.ok) {
        paint();
        toast(ta.statusChanged(item.title, STATUS_LABEL[to]), {
          undo: isUndo ? null : () => setStatus(from, true, fromReason || ''),
        });
        // A history that is open would otherwise miss the new event.
        if (toggle.getAttribute('aria-expanded') === 'true') loadHistory();
      } else {
        item.status = from;
        item.reason = fromReason;
        if (leaves) list.insertBefore(row, anchor && anchor.parentNode === list ? anchor : null);
        else if (returns) row.remove();
        empty.hidden = list.children.length > 0;
        paint();
        toast(ta.updateFailed(item.title), { kind: 'bad' });
      }
    };

    // Adds, changes or removes the reason on a request that is already
    // refused. An empty answer removes it.
    const saveReason = async (opener) => {
      const reason = await askReason(item, opener, item.reason || '', ta.save);
      if (reason === null || busy || reason === (item.reason || '')) return;
      busy = true;
      paint();
      const res = await send('PATCH', `/api/admin/requests/${item.id}`, { status: 'refused', reason });
      busy = false;
      if (expired(res)) return;
      if (res.ok) {
        item.reason = reason || null;
        toast(ta.reasonSaved(item.title));
        if (toggle.getAttribute('aria-expanded') === 'true') loadHistory();
      } else {
        toast(ta.reasonFailed(item.title), { kind: 'bad' });
      }
      paint();
    };

    // Deletes the whole request, history included. The server also sends a
    // tick, so the other tabs and the counts catch up on their own.
    const removeRequest = async () => {
      if (!window.confirm(ta.deleteRequestConfirm(item.title))) return;
      busy = true;
      paint();
      const res = await send('DELETE', `/api/admin/requests/${item.id}`);
      busy = false;
      if (expired(res)) return;
      if (res.ok) {
        row.remove();
        known.delete(item.id);
        empty.hidden = list.children.length > 0;
        toast(ta.requestDeleted(item.title));
      } else {
        paint();
        toast(ta.deleteRequestFailed, { kind: 'bad' });
      }
    };

    const paint = () => {
      clear(statusCell);
      statusCell.append(chip(item.status, STATUS_LABEL[item.status] || item.status));
      reasonLine.hidden = !item.reason;
      reasonLine.textContent = item.reason ? ta.reasonNote(item.reason) : '';
      clear(actions);
      const button = (action, cls) =>
        el('button', {
          class: cls,
          type: 'button',
          text: action.label,
          'aria-label': ta.actionLabel(action.label, item.title),
          disabled: busy,
          onclick: async (event) => {
            for (const open of actions.querySelectorAll('details[open]')) open.open = false;
            if (action.status !== 'refused') {
              setStatus(action.status);
              return;
            }
            const reason = await askReason(item, event.currentTarget, '', ta.actions.refused);
            if (reason !== null) setStatus('refused', false, reason);
          },
        });
      const byStatus = (status) => ACTIONS.find((a) => a.status === status);
      const items = [];
      if (tab.id === 'pending') {
        // The two usual answers stay one click away; the rest is in the menu.
        actions.append(button(byStatus('added'), 'btn btn-small'), button(byStatus('refused'), 'btn btn-small'));
        items.push(button(byStatus('already_had'), 'menu-item'));
      } else {
        for (const action of ACTIONS.filter((a) => a.status !== item.status)) items.push(button(action, 'menu-item'));
      }
      if (item.status === 'refused') {
        items.push(
          el('button', {
            class: 'menu-item',
            type: 'button',
            text: item.reason ? ta.editReason : ta.addReason,
            'aria-label': ta.actionLabel(item.reason ? ta.editReason : ta.addReason, item.title),
            disabled: busy,
            onclick: (event) => {
              for (const open of actions.querySelectorAll('details[open]')) open.open = false;
              saveReason(event.currentTarget);
            },
          }),
        );
      }
      if (item.dmm_url) {
        items.push(
          el('a', {
            class: 'menu-item',
            href: item.dmm_url,
            target: '_blank',
            rel: 'noopener noreferrer',
            text: ta.openInDmm,
            'aria-label': ta.actionLabel(ta.openInDmm, item.title),
          }),
        );
      }
      if (item.imdb_id) items.push(copyImdbItem());
      if (archive) {
        items.push(
          toggle,
          el('hr', { class: 'menu-sep' }),
          el('button', {
            class: 'menu-item menu-item-danger',
            type: 'button',
            text: ta.deleteRequest,
            'aria-label': ta.actionLabel(ta.deleteRequest, item.title),
            disabled: busy,
            onclick: removeRequest,
          }),
        );
      }
      actions.append(menu(ta.actionsMenu, ta.actionsFor(item.title), items, busy));
    };

    // Copies the IMDb ID; rarely needed, so it lives in the menu.
    const copyImdbItem = () => {
      const copy = el('button', {
        class: 'menu-item',
        type: 'button',
        text: ta.copyImdbItem,
        'aria-label': ta.copyImdb(item.imdb_id),
      });
      copyOnClick(copy, item.imdb_id, ta.copyImdbFailed(item.imdb_id));
      return copy;
    };

    const toggle = el('button', {
      class: 'menu-item',
      type: 'button',
      text: ta.historyItem,
      'aria-expanded': 'false',
      'aria-label': ta.history(item.title),
    });
    const loadHistory = async () => {
      if (!historyBox.firstChild) historyBox.append(el('span', { class: 'muted', text: ta.loading }));
      const res = await get(`/api/admin/requests/${item.id}/events`);
      if (expired(res)) return;
      clear(historyBox);
      if (!res.ok) {
        historyBox.append(el('span', { class: 'status-bad', text: ta.historyFailed }));
        return;
      }
      historyBox.append(
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
      historyBox.hidden = open;
      const menuEl = toggle.closest('details');
      if (menuEl) menuEl.open = false;
      if (open) return;
      // Fetched again on every open, so it is never stale.
      clear(historyBox);
      loadHistory();
    });

    // A newer copy of this request from the server. A change in flight wins.
    const update = (fresh) => {
      if (fresh.times_requested !== item.times_requested) {
        item.times_requested = fresh.times_requested;
        paintMeta();
      }
      if (!busy && (fresh.status !== item.status || (fresh.reason || null) !== (item.reason || null))) {
        item.status = fresh.status;
        item.reason = fresh.reason;
        paint();
      }
    };

    paint();
    const entry = { item, row, nodes: [row], update };
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
      else list.prepend(...added.nodes);
      previous = added.nodes[added.nodes.length - 1];
      if (tab.id === 'pending') toast(ta.newOrder(item.title));
    }
    if (!cursor && res.data.next_cursor && list.children.length === items.length) {
      cursor = res.data.next_cursor;
    }
    more.hidden = !cursor;
    empty.textContent = q ? ta.noRequestsMatch : tab.empty;
    empty.hidden = list.children.length > 0;
  };

  filter.addEventListener('input', () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(() => load(true), 250);
  });
  refresh.addEventListener('click', () => load(true));
  more.addEventListener('click', () => load(false));

  clear(panel);
  panel.append(toolbar, list, empty, more);
  load(true);
  return { refresh: merge };
}

// ---------------------------------------------------------------------
// Jobs tab
// ---------------------------------------------------------------------

// Job state -> the existing chip colour keys.
const JOB_CHIP = { needs_pick: 'pending', done: 'added', failed: 'refused' };
const FREE_CHIP = { yes: 'added', unknown: 'pending', no: 'refused' };
const JOB_FILTERS = ['needs_pick', 'all', 'queued', 'fetching', 'failed', 'done'];

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const text = (v) => (typeof v === 'string' && v !== '' ? v : null);
const finite = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const NUMBER = new Intl.NumberFormat('nb-NO', { maximumFractionDigits: 1 });

// Everything a card shows that can change without the job's own timestamp
// moving: the request's status and the blocked hashes.
const jobSignature = (job) => `${job.updated_at}|${job.request_status}|${job.blocked_hashes.join(',')}`;

function renderJobs(panel, { say, expired }) {
  let cursor = null;
  let generation = 0;
  let loading = 0;
  let loaded = false;

  const filter = el(
    'select',
    { id: 'job-filter', class: 'field' },
    JOB_FILTERS.map((value) =>
      el('option', { value, text: value === 'all' ? ta.jobFilterAll : ta.jobStates[value] }),
    ),
  );
  filter.value = 'needs_pick';
  const refresh = el('button', { class: 'btn btn-small', type: 'button', text: ta.refresh });
  const toolbar = el(
    'div',
    { class: 'toolbar' },
    el('label', { class: 'sr-only', for: 'job-filter', text: ta.jobFilter }),
    filter,
    refresh,
  );
  const list = el('ul', { class: 'req-list' });
  const empty = el('p', { class: 'muted', text: ta.empty.jobs, hidden: true });
  const more = el('button', { class: 'btn btn-small', type: 'button', text: t.loadMore, hidden: true });

  // Cards on show, by job id, so a refresh can update them in place.
  const known = new Map();

  const query = () => new URLSearchParams({ state: filter.value });

  const load = async (reset) => {
    const mine = ++generation;
    loading++;
    const params = query();
    if (!reset && cursor) params.set('cursor', cursor);
    more.disabled = true;
    refresh.disabled = true;
    const res = await get(`/api/admin/jobs?${params}`);
    loading--;
    more.disabled = false;
    refresh.disabled = false;
    if (mine !== generation || expired(res)) return;
    if (!res.ok) {
      say(ta.loadJobsFailed, 'status-bad');
      return;
    }
    if (reset) {
      clear(list);
      known.clear();
    }
    for (const job of res.data.items) {
      const shown = known.get(job.id);
      if (shown && shown.card.isConnected) continue;
      list.append(cardFor(job).card);
    }
    loaded = true;
    cursor = res.data.next_cursor;
    more.hidden = !cursor;
    empty.hidden = list.children.length > 0;
  };

  // A job's card. Worker-supplied text only ever goes in through textContent.
  const cardFor = (initial) => {
    let job = initial;
    // True while an action is in flight; the buttons stay disabled.
    let busy = false;
    const card = el('li', { class: 'job-card', 'data-id': job.id });

    const post = async (path, body) => {
      busy = true;
      paint();
      const res = await send('POST', `/api/admin/jobs/${job.id}/${path}`, body);
      busy = false;
      if (expired(res)) return null;
      return res;
    };

    const pick = async (infohash) => {
      const res = await post('pick', { infohash });
      if (!res) return;
      if (res.ok) {
        job.state = res.data.state;
        job.chosen_infohash = infohash;
        job.picked = true;
        job.reason = res.data.state === 'done' ? 'dry_run' : null;
        toast(res.data.state === 'done' ? ta.pickedDryRun(job.title) : ta.pickedQueued(job.title));
      } else {
        toast(res.status === 409 ? ta.pickFailed : ta.jobActionFailed(job.title), { kind: 'bad' });
      }
      paint();
    };

    const retry = async () => {
      const res = await post('retry');
      if (!res) return;
      if (res.ok) {
        job.state = 'queued';
        job.reason = null;
        job.last_error = null;
        job.attempts = 0;
        toast(ta.jobRetried(job.title));
      } else {
        toast(ta.jobActionFailed(job.title), { kind: 'bad' });
      }
      paint();
    };

    const cancel = async () => {
      const res = await post('cancel');
      if (!res) return;
      if (res.ok) {
        job.state = 'failed';
        job.reason = 'cancelled';
        toast(ta.jobCancelled(job.title));
      } else {
        toast(ta.jobActionFailed(job.title), { kind: 'bad' });
      }
      paint();
    };

    const notPlaying = async () => {
      if (!window.confirm(ta.notPlayingConfirm(job.title))) return;
      const res = await post('not-playing');
      if (!res) return;
      if (res.ok) {
        if (!job.blocked_hashes.includes(job.chosen_infohash)) job.blocked_hashes.push(job.chosen_infohash);
        job.state = 'queued';
        job.chosen_infohash = null;
        job.picked = false;
        job.reason = null;
        job.last_error = null;
        job.attempts = 0;
        if (res.data.reopened) job.request_status = 'pending';
        toast(ta.notPlayingDone(job.title));
      } else {
        toast(ta.notPlayingFailed, { kind: 'bad' });
      }
      paint();
    };

    const button = (label, ariaTitle, onclick, cls = 'btn btn-small') =>
      el('button', {
        class: cls,
        type: 'button',
        text: label,
        'aria-label': ta.actionLabel(label, ariaTitle),
        disabled: busy,
        onclick,
      });

    // One shortlist entry.
    const candidateFor = (c, index) => {
      const hash = text(c.infohash);
      const name = text(c.title) || hash || '';
      const blocked = hash !== null && job.blocked_hashes.includes(hash);
      const free = typeof c.free_badge === 'string' && Object.hasOwn(ta.freeBadge, c.free_badge) ? c.free_badge : null;
      const specs = [c.resolution, c.source, c.video, ...(Array.isArray(c.audio) ? c.audio : [])]
        .filter((s) => text(s))
        .join(' · ');
      const score = finite(c.score);
      const size = finite(c.size_bytes);
      const seeders = finite(c.seeders);
      const rank = finite(c.rank) ?? index + 1;
      const facts = [
        score === null ? null : ta.scoreOf(NUMBER.format(score)),
        size === null ? null : ta.sizeGb(NUMBER.format(size / 1e9)),
        seeders === null ? null : ta.seedersOf(seeders),
        specs || null,
      ].filter(Boolean);
      const tag = (label) => el('span', { class: 'badge', text: label });
      const tags = [];
      if (hash !== null && hash === job.top_infohash) tags.push(tag(ta.topPick));
      if (c.clear_winner === true) tags.push(tag(ta.clearWinner));
      if (hash !== null && hash === job.chosen_infohash) tags.push(tag(ta.chosenTag));
      if (hash !== null && hash === job.chosen_infohash && job.reason === 'picked_failed') tags.push(tag(ta.lastFailed));
      if (blocked) tags.push(tag(ta.blockedTag));
      if (c.foreign_language === true) tags.push(tag(ta.foreignLang));
      const canPick = job.state === 'needs_pick' && job.request_status === 'pending' && hash !== null && !blocked;
      const freeChip = free ? chip(FREE_CHIP[free], ta.freeBadge[free]) : null;
      if (freeChip) freeChip.setAttribute('title', ta.freeBadgeClaim);
      const cls = ['cand', hash !== null && hash === job.top_infohash ? 'cand-top' : '', blocked ? 'cand-dim' : '']
        .filter(Boolean)
        .join(' ');
      return el(
        'li',
        { class: cls },
        el('p', { class: 'req-title' }, el('strong', { text: `#${rank}` }), el('span', { text: name }), tags, freeChip),
        facts.length > 0 ? el('p', { class: 'req-meta', text: facts.join(' · ') }) : null,
        canPick ? el('div', { class: 'row-actions' }, button(ta.pick, name, () => pick(hash), 'btn btn-small btn-primary')) : null,
      );
    };

    // The shortlist, or null when the job has no parsed list.
    let candidateBox = null;
    const shortlist = () => {
      if (!Array.isArray(job.candidates)) return null;
      const entries = job.candidates.filter(isObject);
      const survivors = entries.length > 0 ? finite(entries[0].survivors) : null;
      const summary = survivors === null ? ta.shownOnly(entries.length) : ta.shownOfSurvived(entries.length, survivors);
      const hasFree = entries.some((c) => typeof c.free_badge === 'string' && Object.hasOwn(ta.freeBadge, c.free_badge));
      const body =
        entries.length === 0
          ? el('p', { class: 'muted', text: ta.noCandidates })
          : [
              el('p', { class: 'muted', text: summary }),
              hasFree ? el('p', { class: 'muted', text: ta.freeBadgeClaim }) : null,
              el('ol', { class: 'cand-list' }, entries.map(candidateFor)),
            ];
      if (job.state === 'needs_pick') {
        return el('div', { class: 'grid gap-2' }, el('strong', { text: ta.candidatesHeading }), body);
      }
      const wasOpen = candidateBox instanceof HTMLDetailsElement && candidateBox.open;
      return el('details', { class: 'grid gap-2', open: wasOpen }, el('summary', { text: ta.candidatesHeading }), body);
    };

    const paint = () => {
      const actions = el('div', { class: 'row-actions' });
      const pending = job.request_status === 'pending';
      if (['failed', 'needs_pick', 'done'].includes(job.state) && pending && job.reason !== 'no_imdb_id') {
        actions.append(button(t.retry, job.title, retry));
      }
      if (['queued', 'fetching', 'needs_pick'].includes(job.state)) {
        actions.append(button(ta.cancelJob, job.title, cancel));
      }
      if (job.chosen_infohash && (pending || job.request_status === 'added')) {
        actions.append(button(ta.notPlaying, job.title, notPlaying));
      }
      const reason = job.reason ? (Object.hasOwn(ta.reasons, job.reason) ? ta.reasons[job.reason] : job.reason) : null;
      const next = shortlist();
      candidateBox = next;
      const parts = [
        el(
          'div',
          { class: 'min-w-0' },
          el(
            'p',
            { class: 'req-title' },
            el('strong', { text: job.title }),
            job.year ? ` (${job.year})` : null,
            badge(job.type),
            proBadge(job.infuse_pro ? ta.withPro : ta.withoutPro),
            chip(JOB_CHIP[job.state] ?? job.state, ta.jobStates[job.state] ?? job.state),
          ),
          el('p', { class: 'req-meta', text: ta.attemptsOf(job.attempts) }),
          reason ? el('p', { class: 'req-note', text: reason }) : null,
          job.last_error ? el('p', { class: 'muted', text: job.last_error }) : null,
        ),
        next,
        actions.children.length > 0 ? actions : null,
      ];
      card.replaceChildren(...parts.filter(Boolean));
    };

    // A newer copy of this job from the server. An action in flight wins.
    const update = (fresh) => {
      if (busy || jobSignature(fresh) === jobSignature(job)) return;
      job = fresh;
      paint();
    };

    paint();
    const entry = { card, update, job: () => job };
    known.set(job.id, entry);
    return entry;
  };

  // Is `job` at or before `last` in the list's order (newest first)?
  const withinPage = (job, last) =>
    job.created_at > last.created_at || (job.created_at === last.created_at && job.id >= last.id);

  // Merges the first page of the server's list into what is on show: cards
  // update where they are, new ones appear, and ones that no longer belong go.
  const merge = async () => {
    if (loading > 0 || !loaded) return;
    const mine = generation;
    const res = await get(`/api/admin/jobs?${query()}`);
    if (expired(res) || !res.ok || mine !== generation) return;

    const items = res.data.items;
    const fresh = new Set(items.map((j) => j.id));
    const last = items.length > 0 ? items[items.length - 1] : null;
    for (const [id, entry] of known) {
      if (!entry.card.isConnected || fresh.has(id)) continue;
      if (!last || !res.data.next_cursor || withinPage(entry.job(), last)) {
        entry.card.remove();
        known.delete(id);
      }
    }

    let previous = null;
    for (const job of items) {
      const entry = known.get(job.id);
      if (entry && entry.card.isConnected) {
        entry.update(job);
        previous = entry.card;
        continue;
      }
      const added = cardFor(job);
      if (previous) previous.after(added.card);
      else list.prepend(added.card);
      previous = added.card;
    }
    if (!cursor && res.data.next_cursor && list.children.length === items.length) {
      cursor = res.data.next_cursor;
    }
    more.hidden = !cursor;
    empty.hidden = list.children.length > 0;
  };

  filter.addEventListener('change', () => load(true));
  refresh.addEventListener('click', () => load(true));
  more.addEventListener('click', () => load(false));

  clear(panel);
  panel.append(toolbar, list, empty, more);
  load(true);
  return { refresh: merge };
}

// ---------------------------------------------------------------------
// Members tab
// ---------------------------------------------------------------------

// Shows a freshly issued code exactly once, with a copy button.
function codeBox(name, code) {
  const copy = el('button', { class: 'btn btn-primary', type: 'button', text: ta.copyCode });
  copyOnClick(copy, code, ta.copyCodeFailed);
  return el(
    'div',
    { class: 'code-box', role: 'alert' },
    el('p', { text: ta.codeFor(name) }),
    el('output', { text: code }),
    el('p', { class: 'muted', text: ta.codeOnce }),
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
  if (!m.devices.length) return el('span', { class: 'muted', text: ta.noDevices });
  const n = m.devices.length;
  const lastSeen = Math.max(...m.devices.map((d) => d.last_seen_at));
  return el(
    'details',
    {},
    el('summary', { text: ta.devices(n, formatAgo(lastSeen)) }),
    deviceList,
  );
}

function renderMembers(panel, { say, expired }) {
  const codeSlot = el('div');
  const tbody = el('tbody');

  const name = el('input', { id: 'm-name', class: 'field', maxlength: 40, required: true });
  const daily = el('input', { id: 'm-daily', class: 'field', type: 'number', min: 0, max: 1000, inputmode: 'numeric', placeholder: ta.noLimit });
  const code = el('input', { id: 'm-code', class: 'field', maxlength: 64, autocomplete: 'off', spellcheck: false });
  // Advice, not a block: the server accepts anything from 3 characters.
  const codeWarning = el('span', { class: 'muted', id: 'm-code-note' });
  code.setAttribute('aria-describedby', 'm-code-note');
  code.addEventListener('input', () => {
    const n = code.value.replace(/[\s-]/g, '').length;
    codeWarning.textContent = n > 0 && n < 6 ? ta.shortCode : '';
  });
  const add = el('button', { class: 'btn btn-primary', type: 'submit', text: ta.addUser });
  const form = el(
    'form',
    { class: 'form-grid card mb-4 p-4', 'aria-label': ta.addUserLabel },
    el('label', { for: 'm-name' }, ta.name, name),
    el('label', { for: 'm-daily' }, ta.dailyLimitOptional, daily),
    el('label', { for: 'm-code' }, ta.codeOptional, code),
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
        toast(ta.addedWithCode(res.data.name));
      }
      form.reset();
      codeWarning.textContent = '';
      load();
    } else if (res.status === 409) {
      toast(codeTaken(res) ? ta.codeTaken : ta.nameTaken, { kind: 'bad' });
    } else {
      toast(ta.checkAddFields, { kind: 'bad' });
    }
  });

  const head = el(
    'tr',
    {},
    ta.userHead.map((h) => el('th', { scope: 'col', text: h })),
  );

  const memberRow = (m) => {
    const deviceList = el('ul', { class: 'events' });
    for (const d of m.devices) {
      const kill = el('button', {
        class: 'btn btn-small',
        type: 'button',
        text: t.logOut,
        'aria-label': ta.deviceLogOutLabel(m.name, formatDateTime(d.created_at)),
      });
      kill.addEventListener('click', async () => {
        const res = await send('DELETE', `/api/admin/sessions/${d.id}`);
        if (expired(res)) return;
        toast(res.ok ? ta.deviceLoggedOut(m.name) : ta.deviceLogOutFailed, { kind: res.ok ? 'ok' : 'bad' });
        load();
      });
      deviceList.append(el('li', {}, ta.deviceLine(formatDateTime(d.created_at), formatDateTime(d.last_seen_at)), kill));
    }
    const revoked = m.revoked_at !== null;
    const rotate = el('button', { class: 'btn btn-small', type: 'button', text: ta.rotate, disabled: revoked });
    rotate.addEventListener('click', async () => {
      const chosen = window.prompt(ta.rotatePrompt(m.name));
      if (chosen === null) return;
      const res = await send('POST', `/api/admin/members/${m.id}/rotate`, chosen.trim() === '' ? {} : { code: chosen });
      if (expired(res)) return;
      if (res.ok) {
        clear(codeSlot);
        if (res.data.code) {
          codeSlot.append(codeBox(m.name, res.data.code));
        } else {
          toast(ta.rotatedToChosen(m.name));
        }
        load();
      } else if (res.status === 409 && codeTaken(res)) {
        toast(ta.codeTaken, { kind: 'bad' });
      } else if (res.status === 400) {
        toast(ta.codeLength, { kind: 'bad' });
      } else {
        toast(ta.rotateFailed, { kind: 'bad' });
      }
    });
    const revoke = el('button', { class: 'btn btn-small', type: 'button', text: revoked ? ta.activate : ta.deactivate });
    revoke.addEventListener('click', async () => {
      if (!revoked && !window.confirm(ta.deactivateConfirm(m.name))) {
        return;
      }
      const res = await send('POST', `/api/admin/members/${m.id}/${revoked ? 'restore' : 'revoke'}`);
      if (expired(res)) return;
      const done = revoked ? ta.activatedDone(m.name) : ta.deactivatedDone(m.name);
      toast(res.ok ? done : revoked ? ta.activateFailed : ta.deactivateFailed, {
        kind: res.ok ? 'ok' : 'bad',
      });
      load();
    });
    // Only a deactivated user can go; their requests stay under their name.
    const remove = el('button', { class: 'btn btn-small', type: 'button', text: ta.delete });
    remove.addEventListener('click', async () => {
      if (!window.confirm(ta.deleteConfirm(m.name))) {
        return;
      }
      const res = await send('DELETE', `/api/admin/members/${m.id}`);
      if (expired(res)) return;
      toast(
        res.ok ? ta.deleted(m.name) : res.status === 409 ? ta.deactivateFirst(m.name) : ta.deleteFailed,
        { kind: res.ok ? 'ok' : 'bad' },
      );
      load();
    });
    const edit = el('button', { class: 'btn btn-small', type: 'button', text: ta.edit, 'aria-label': ta.editLabel(m.name) });
    edit.addEventListener('click', () => editMember(m, edit));
    const limits = el(
      'td',
      {},
      m.daily_limit === null ? el('span', { class: 'muted', text: ta.noLimit }) : ta.perDay(m.daily_limit),
    );
    return el(
      'tr',
      {},
      el('td', {}, el('strong', { text: m.name }), revoked ? el('p', {}, chip('refused', ta.deactivated)) : null),
      el('td', { text: m.last_seen_at ? formatDateTime(m.last_seen_at) : ta.never }),
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
      placeholder: ta.noLimit,
      value: m.daily_limit === null ? null : String(m.daily_limit),
    });
    const message = el('p', { class: 'status', role: 'status' });
    const cancel = el('button', { class: 'btn', type: 'button', text: t.cancel, onclick: () => dialog.close() });
    const save = el('button', { class: 'btn btn-primary', type: 'submit', text: ta.save });
    const form = el(
      'form',
      { method: 'dialog', class: 'form-grid' },
      el('h2', { id: 'edit-title', text: ta.editLabel(m.name) }),
      el('label', { for: 'e-name' }, ta.name, eName),
      el('p', { class: 'muted', text: ta.renameWarning }),
      el('label', { for: 'e-daily' }, ta.dailyLimitBlank, eDaily),
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
        toast(ta.saved(eName.value.trim()));
        load();
      } else {
        message.className = 'status status-bad';
        message.textContent =
          res.status === 409 ? ta.nameTaken : res.status === 400 ? ta.checkEditFields : ta.saveFailed;
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
      say(ta.loadUsersFailed, 'status-bad');
      return;
    }
    clear(tbody);
    for (const m of res.data.members) tbody.append(memberRow(m));
  };

  clear(panel);
  panel.append(
    el('h2', { class: 'sr-only', text: ta.users }),
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
