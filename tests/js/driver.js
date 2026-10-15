// Browser test driver, loaded only by tests/browser.rs next to the real
// page modules (served from wiremock with mocked API responses). It types,
// presses keys and records what the page renders, then writes PASS or the
// failures into #e2e-result for the test to read from the dumped DOM.

import { formatWait } from './js/ui.js';
import { latestOnly } from './js/api.js';

const failures = [];
const check = (ok, what) => {
  if (!ok) failures.push(what);
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const key = (target, name) =>
  target.dispatchEvent(new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true }));
const type = (input, value) => {
  input.value = value;
  input.dispatchEvent(new Event('input', { bubbles: true }));
};

async function waitFor(find, ms = 3000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const found = find();
    if (found) return found;
    await sleep(20);
  }
  return null;
}

async function unitTests() {
  check(formatWait(45) === '45s' && formatWait(899) === '14:59' && formatWait(3700) === '1h 1m', 'formatWait');

  // A slow first run and a fast second run: only the second is current,
  // and the first one's request is aborted.
  const latest = latestOnly();
  let firstSignal = null;
  const first = latest.run((signal) => {
    firstSignal = signal;
    return sleep(200).then(() => 'first');
  });
  const second = latest.run(() => sleep(10).then(() => 'second'));
  const [a, b] = await Promise.all([first, second]);
  check(!a.current, 'stale run was delivered');
  check(b.current && b.value === 'second', 'latest run not delivered');
  check(firstSignal && firstSignal.aborted, 'stale run not aborted');
}

async function e2e() {
  const input = await waitFor(() => document.getElementById('q'));
  check(!!input, 'search input never rendered');
  if (!input) return;
  check(document.activeElement === input, 'search input not focused on load');

  // Record every set of titles the dropdown ever shows.
  const list = document.getElementById('results');
  const rendered = [];
  new MutationObserver(() => {
    const titles = [...list.querySelectorAll('.opt-title')].map((n) => n.textContent);
    if (titles.length) rendered.push(titles.join('|'));
  }).observe(list, { childList: true, subtree: true });

  // Typing fast: the moment the (slow) "du" search is on the wire, type
  // "dun". The "du" answer must never render.
  const realFetch = window.fetch;
  let duSent;
  const duOnWire = new Promise((resolve) => {
    duSent = resolve;
  });
  window.fetch = (url, options) => {
    if (String(url).endsWith('q=du')) duSent();
    return realFetch(url, options);
  };
  type(input, 'du');
  await duOnWire;
  type(input, 'dun');
  await waitFor(() => list.querySelector('.opt-title'), 3000);
  await sleep(800);
  check(rendered.length > 0, 'no results rendered');
  check(rendered.every((r) => !r.includes('Stale')), `stale results rendered: ${rendered.join(' / ')}`);
  const titles = [...list.querySelectorAll('.opt-title')].map((n) => n.textContent);
  check(titles.join('|') === 'Dune|Dune: Part Two', `final results were ${titles.join('|')}`);
  check(input.getAttribute('aria-expanded') === 'true', 'listbox not expanded');

  // Keyboard: down, down, up, End, Home, Enter on an available title.
  key(input, 'ArrowDown');
  check(input.getAttribute('aria-activedescendant') === 'opt-0', 'ArrowDown did not activate first option');
  key(input, 'ArrowDown');
  check(input.getAttribute('aria-activedescendant') === 'opt-1', 'second ArrowDown');
  key(input, 'Home');
  check(input.getAttribute('aria-activedescendant') === 'opt-0', 'Home');
  key(input, 'End');
  check(input.getAttribute('aria-activedescendant') === 'opt-1', 'End');
  // opt-1 is already requested: Enter must not open the sheet.
  check(list.children[1].getAttribute('aria-disabled') === 'true', 'requested result not disabled');
  key(input, 'Enter');
  check(!document.querySelector('dialog[open]'), 'disabled result opened the sheet');
  key(input, 'ArrowUp');
  key(input, 'Enter');
  const sheet = await waitFor(() => document.querySelector('dialog[open]'));
  check(!!sheet, 'Enter did not open the confirm sheet');
  if (!sheet) return;
  const add = document.activeElement;
  check(add && add.textContent === 'Add to queue', `focus in sheet was on ${add && add.textContent}`);

  // Enter on the focused button submits.
  add.click();
  await waitFor(() => !document.querySelector('dialog[open]'));
  check(!document.querySelector('dialog[open]'), 'sheet stayed open after submit');
  const status = document.querySelector('.status-ok');
  check(status && status.textContent.includes('Requested “Dune”'), `status was ${status && status.textContent}`);
  check(document.activeElement === input, 'focus did not return to the search input');
  const mine = await waitFor(() => document.querySelector('.mine-item'));
  check(mine && mine.textContent.includes('Pending'), 'my requests not refreshed');

  // Escape closes the list, a second Escape clears the input.
  type(input, 'dun');
  key(input, 'Escape');
  check(input.getAttribute('aria-expanded') === 'false', 'Escape did not close list');
  key(input, 'Escape');
  check(input.value === '', 'second Escape did not clear input');
}

async function main() {
  try {
    await unitTests();
    await e2e();
  } catch (err) {
    failures.push(`exception: ${err && err.stack}`);
  }
  const out = document.createElement('pre');
  out.id = 'e2e-result';
  out.textContent = failures.length ? `FAIL\n${failures.join('\n')}` : 'PASS';
  document.body.append(out);
}

main();
