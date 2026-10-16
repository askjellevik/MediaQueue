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
  const dmm = row.querySelector('a[aria-label^="Åpne i DMM"]');
  check(dmm && dmm.href === 'https://dmm.example/movie/tt1160419', `DMM link was ${dmm && dmm.href}`);
  check(dmm && dmm.rel.includes('noopener'), 'DMM link lacks rel=noopener');
  check(!row.querySelector('button[aria-label^="Åpne igjen"]'), 'pending row offers Åpne igjen');
  check(!!row.querySelector('button.code-btn'), 'IMDb ID is not a button');
  check(!!row.querySelector('details.menu button[aria-label^="Har fra før"]'), 'no Har fra før in the menu');
  check(!!button('Oppdater'), 'no Oppdater button');

  // First PATCH fails (mock answers 500): the row comes back.
  row.querySelector('button[aria-label^="Marker som lagt til"]').click();
  await waitFor(() => document.querySelector('.toast-bad'));
  check(!row.hidden && row.isConnected, 'row not restored after a failed update');
  check(row.textContent.includes('Venter'), 'status not rolled back');
  // Second PATCH succeeds: the row leaves the Pending tab.
  row.querySelector('button[aria-label^="Marker som lagt til"]').click();
  await waitFor(() => !row.isConnected);
  check(!row.isConnected, 'row stayed after a successful update');
  // The row leaves at once (optimistic); the toast waits for the answer.
  const toastEl = await waitFor(() => document.querySelector('.toast-ok'));
  check(toastEl?.textContent.includes('«Dune» er nå lagt til'), `success toast was ${toastEl?.textContent}`);
  check(!!toastEl?.querySelector('button'), 'toast has no Angre');
  check(document.getElementById('toasts')?.getAttribute('aria-live') === 'polite', 'toast region not live');

  // Undo puts the row back in the Pending tab.
  [...toastEl.querySelectorAll('button')].find((b) => b.textContent === 'Angre').click();
  const back = await waitFor(() => rowWith('Dune'));
  check(!!back, 'Angre did not bring the row back');

  // Archive: history expands on demand.
  document.getElementById('tab-archive').click();
  const archived = await waitFor(() => rowWith('Dune'));
  check(!!archived?.querySelector('details.menu summary[aria-label^="Endre status"]'), 'no Endre status menu');
  check(!!archived, 'archive row not rendered');
  check(!!document.querySelector('a[href="/api/admin/export.csv"]'), 'no Export CSV link');
  const history = archived && archived.querySelector('button[aria-label^="Historikk"]');
  check(!!history, 'no History button');
  if (history) {
    history.click();
    const events = await waitFor(() => document.querySelector('.events li'));
    check(events && events.textContent.includes('Bestilt av Alice'), 'history not shown');
    check(history.getAttribute('aria-expanded') === 'true', 'Historikk not marked expanded');
    // A status change while History is open refetches it.
    const before = window.__eventFetches || 0;
    archived.querySelector('details.menu button[aria-label^="Avvis"]').click();
    await waitFor(() => (window.__eventFetches || 0) > before);
    check((window.__eventFetches || 0) > before, 'open history was not refetched');
  }

  // Members: add one, see the code once.
  document.getElementById('tab-members').click();
  const name = await waitFor(() => document.getElementById('m-name'));
  check(!!(await waitFor(() => rowWith('Bob'))), 'member list not rendered');
  check(rowWith('Bob')?.textContent.includes('Logg ut'), 'device kill button missing');
  check(!!button('Rediger'), 'no Rediger button on a member');
  name.value = 'Carol';
  button('Legg til bruker').click();
  const box = await waitFor(() => document.querySelector('.code-box output'));
  check(box && box.textContent === 'ABCD-EFGH', `code box showed ${box && box.textContent}`);
  check(document.querySelector('.code-box')?.textContent.includes('Du får ikke se denne koden igjen'), 'no show-once notice');
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
