// Live updates: calls `onTick` whenever the server says something changed.
// The server sends content-free ticks over server-sent events, so `onTick`
// refetches what the page already shows. If the stream is unavailable the
// page falls back to polling; either way a dead session shows up as a 401
// from `onTick`'s own requests, which sends the page back to its gate.

const DEBOUNCE_MS = 300;
const POLL_MS = 60000;

// Returns a function that stops watching.
export function watch(path, onTick) {
  let source = null;
  let debounce = 0;
  let poll = 0;
  let stopped = false;
  // A tick arrived while the tab was hidden; refetch when it is shown.
  let pending = false;

  const run = () => {
    if (stopped) return;
    if (document.hidden) {
      pending = true;
      return;
    }
    pending = false;
    onTick();
  };
  const schedule = () => {
    clearTimeout(debounce);
    debounce = setTimeout(run, DEBOUNCE_MS);
  };

  const stopPolling = () => {
    clearInterval(poll);
    poll = 0;
  };
  const startPolling = () => {
    if (poll || stopped) return;
    poll = setInterval(() => {
      run();
      // The server may have come back; try the stream again.
      if (!source) connect();
    }, POLL_MS);
  };

  function connect() {
    if (stopped) return;
    if (typeof EventSource === 'undefined') {
      startPolling();
      return;
    }
    source = new EventSource(path);
    // Also fires after each automatic reconnect, which is when anything
    // sent in the gap has to be caught up on.
    source.addEventListener('open', () => {
      stopPolling();
      schedule();
    });
    source.addEventListener('tick', schedule);
    source.addEventListener('error', () => {
      // The browser retries by itself unless the server refused (401, 503)
      // or ended the stream for good.
      if (source && source.readyState === EventSource.CLOSED) {
        source.close();
        source = null;
        startPolling();
      }
    });
  }

  const onVisible = () => {
    if (!document.hidden && pending) schedule();
  };
  document.addEventListener('visibilitychange', onVisible);

  connect();

  return () => {
    stopped = true;
    clearTimeout(debounce);
    stopPolling();
    document.removeEventListener('visibilitychange', onVisible);
    if (source) source.close();
    source = null;
  };
}
