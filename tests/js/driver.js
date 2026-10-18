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
  check(formatWait(45) === '45 s' && formatWait(899) === '14:59' && formatWait(3700) === '1 t 1 min', 'formatWait');

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
  check(add && add.textContent === 'Bestill', `focus in sheet was on ${add && add.textContent}`);
  const infuse = sheet.querySelector('#infuse');
  check(infuse && !infuse.checked, 'Infuse PRO switch is not off by default');

  // Enter on the focused button submits.
  add.click();
  await waitFor(() => !document.querySelector('dialog[open]'));
  check(!document.querySelector('dialog[open]'), 'sheet stayed open after submit');
  const status = document.querySelector('.status-ok');
  check(status && status.textContent.includes('Bestilte «Dune»'), `status was ${status && status.textContent}`);
  check(document.activeElement !== input, 'search input kept focus after the sheet closed');
  const mineItems = () => [...document.querySelectorAll('.mine-item')];
  const mine = await waitFor(() => mineItems().find((i) => i.textContent.includes('Venter')));
  check(!!mine, 'my requests not refreshed');
  check(!mine?.querySelector('.mine-reason'), 'a pending request shows a reason');

  // A refused request with a reason has a toggle, closed at first.
  const refused = mineItems().find((i) => i.textContent.includes('Arrival'));
  const reasonToggle = refused?.querySelector('button[aria-controls]');
  const reasonBox = refused?.querySelector('.mine-reason');
  check(!!reasonToggle && !!reasonBox, 'refused request has no Vis begrunnelse');
  if (reasonToggle && reasonBox) {
    check(reasonToggle.textContent === 'Vis begrunnelse', `toggle said ${reasonToggle.textContent}`);
    check(reasonToggle.getAttribute('aria-expanded') === 'false' && reasonBox.hidden, 'reason is not closed at first');
    check(reasonToggle.getAttribute('aria-controls') === reasonBox.id && !!reasonBox.id, 'toggle does not point at the reason');
    reasonToggle.click();
    check(!reasonBox.hidden && reasonBox.textContent === 'Ikke ute på disk ennå', `reason showed ${reasonBox.textContent}`);
    check(reasonToggle.getAttribute('aria-expanded') === 'true' && reasonToggle.textContent === 'Skjul begrunnelse', 'toggle did not flip');
    reasonToggle.click();
    check(reasonBox.hidden && reasonToggle.getAttribute('aria-expanded') === 'false', 'second click did not close the reason');
  }

  // The clear button shows while there is text, and empties the field.
  const clearButton = document.querySelector('.search-clear');
  type(input, 'dun');
  check(clearButton && !clearButton.hidden, 'clear button hidden while the field has text');
  clearButton.click();
  check(input.value === '', 'clear button did not empty the field');
  check(clearButton.hidden, 'clear button stayed visible on an empty field');

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
