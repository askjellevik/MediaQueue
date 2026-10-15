// Browser test driver for the admin page; see tests/browser.rs. Clicks
// through the queue, archive and members tabs against a mocked API.

const failures = [];
const check = (ok, what) => {
  if (!ok) failures.push(what);
};
// Counts history fetches, to see that an open History refreshes.
const realFetch = window.fetch.bind(window);
window.fetch = (input, init) => {
  if (String(input && input.url ? input.url : input).includes('/events')) {
    window.__eventFetches = (window.__eventFetches || 0) + 1;
  }
  return realFetch(input, init);
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(find, ms = 3000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const found = find();
    if (found) return found;
    await sleep(20);
  }
  return null;
}

const button = (text) => [...document.querySelectorAll('button')].find((b) => b.textContent === text);
const rowWith = (text) => [...document.querySelectorAll('tbody tr')].find((r) => r.textContent.includes(text));

async function run() {
  const tab = await waitFor(() => document.getElementById('tab-pending'));
  check(!!tab, 'tabs never rendered');
  if (!tab) return;
  check(tab.getAttribute('aria-selected') === 'true', 'pending tab not selected first');
  check(document.querySelectorAll('[role="tab"]').length === 6, 'expected six tabs');

  const row = await waitFor(() => rowWith('Dune'));
  check(!!row, 'pending row not rendered');
  if (!row) return;
  check(row.textContent.includes('Alice'), 'requester missing');
  check(row.textContent.includes('tt1160419'), 'IMDb ID missing');
  const dmm = row.querySelector('a[aria-label^="Open in DMM"]');
  check(dmm && dmm.href === 'https://dmm.example/movie/tt1160419', `DMM link was ${dmm && dmm.href}`);
  check(dmm && dmm.rel.includes('noopener'), 'DMM link lacks rel=noopener');
  check(!row.querySelector('button[aria-label^="Reopen"]'), 'pending row offers Reopen');
  check(!!row.querySelector('button.code-btn'), 'IMDb ID is not a button');
  check(!!row.querySelector('details.menu button[aria-label^="Already in library"]'), 'no Already in library in More');
  check(!!button('Refresh'), 'no Refresh button');

  // First PATCH fails (mock answers 500): the row comes back.
  row.querySelector('button[aria-label^="Mark added"]').click();
  await waitFor(() => document.querySelector('.toast-bad'));
  check(!row.hidden && row.isConnected, 'row not restored after a failed update');
  check(row.textContent.includes('Pending'), 'status not rolled back');
  // Second PATCH succeeds: the row leaves the Pending tab.
  row.querySelector('button[aria-label^="Mark added"]').click();
  await waitFor(() => !row.isConnected);
  check(!row.isConnected, 'row stayed after a successful update');
  const toastEl = document.querySelector('.toast-ok');
  check(toastEl?.textContent.includes('Dune” is now Added'), `success toast was ${toastEl?.textContent}`);
  check(!!toastEl?.querySelector('button'), 'toast has no Undo');
  check(document.getElementById('toasts')?.getAttribute('aria-live') === 'polite', 'toast region not live');

  // Undo puts the row back in the Pending tab.
  [...toastEl.querySelectorAll('button')].find((b) => b.textContent === 'Undo').click();
  const back = await waitFor(() => rowWith('Dune'));
  check(!!back, 'Undo did not bring the row back');

  // Archive: history expands on demand.
  document.getElementById('tab-archive').click();
  const archived = await waitFor(() => rowWith('Dune'));
  check(!!archived?.querySelector('details.menu summary[aria-label^="Change status"]'), 'no Change status menu');
  check(!!archived, 'archive row not rendered');
  check(!!document.querySelector('a[href="/api/admin/export.csv"]'), 'no Export CSV link');
  const history = archived && archived.querySelector('button[aria-label^="History"]');
  check(!!history, 'no History button');
  if (history) {
    history.click();
    const events = await waitFor(() => document.querySelector('.events li'));
    check(events && events.textContent.includes('Requested by Alice'), 'history not shown');
    check(history.getAttribute('aria-expanded') === 'true', 'History not marked expanded');
    // A status change while History is open refetches it.
    const before = window.__eventFetches || 0;
    archived.querySelector('details.menu button[aria-label^="Decline"]').click();
    await waitFor(() => (window.__eventFetches || 0) > before);
    check((window.__eventFetches || 0) > before, 'open history was not refetched');
  }

  // Members: add one, see the code once.
  document.getElementById('tab-members').click();
  const name = await waitFor(() => document.getElementById('m-name'));
  check(!!(await waitFor(() => rowWith('Bob'))), 'member list not rendered');
  check(rowWith('Bob')?.textContent.includes('Log out'), 'device kill button missing');
  check(!!button('Edit'), 'no Edit button on a member');
  name.value = 'Carol';
  button('Add member').click();
  const box = await waitFor(() => document.querySelector('.code-box output'));
  check(box && box.textContent === 'ABCD-EFGH', `code box showed ${box && box.textContent}`);
  check(document.querySelector('.code-box')?.textContent.includes('You will not see this code again'), 'no show-once notice');
}

async function main() {
  try {
    await run();
  } catch (err) {
    failures.push(`exception: ${err && err.stack}`);
  }
  const out = document.createElement('pre');
  out.id = 'e2e-result';
  out.textContent = failures.length ? `FAIL\n${failures.join('\n')}` : 'PASS';
  document.body.append(out);
}

main();
