// Thin fetch wrapper for the JSON API. Every call resolves to
// { status, ok, data, retryAfter }; a network failure is status 0. Only an
// aborted request rejects (with an AbortError), so callers can ignore it.

const CSRF = { 'X-Requested-With': 'mp' };

async function call(method, path, { body, signal } = {}) {
  const headers = body === undefined ? { ...CSRF } : { ...CSRF, 'Content-Type': 'application/json' };
  let res;
  try {
    res = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      credentials: 'same-origin',
      cache: 'no-store',
      signal,
    });
  } catch (err) {
    if (err && err.name === 'AbortError') throw err;
    return { status: 0, ok: false, data: null, retryAfter: 0 };
  }
  let data = null;
  try {
    data = await res.json();
  } catch (err) {
    if (err && err.name === 'AbortError') throw err;
  }
  const retryAfter = Math.max(0, parseInt(res.headers.get('Retry-After') || '0', 10) || 0);
  return { status: res.status, ok: res.ok, data, retryAfter };
}

export const api = {
  me: () => call('GET', '/api/me'),
  login: (code) => call('POST', '/api/auth/code', { body: { code } }),
  logout: () => call('POST', '/api/auth/logout', { body: {} }),
  search: (q, signal) => call('GET', `/api/search?q=${encodeURIComponent(q)}`, { signal }),
  submit: (tmdbId, mediaType, note) =>
    call('POST', '/api/requests', {
      body: note ? { tmdb_id: tmdbId, media_type: mediaType, note } : { tmdb_id: tmdbId, media_type: mediaType },
    }),
  cancel: (id) => call('DELETE', `/api/requests/${id}`, { body: {} }),
  // `status` is 'pending' or 'earlier' (everything no longer pending).
  mine: (status, cursor) => {
    const params = new URLSearchParams({ status });
    if (cursor) params.set('cursor', cursor);
    return call('GET', `/api/requests/mine?${params}`);
  },
};

export { call };

// Runs async work so that only the most recent run's result is ever
// delivered: starting a new run aborts the previous one, and a result that
// still arrives late is dropped by the sequence check.
export function latestOnly() {
  let controller = null;
  let seq = 0;
  return {
    async run(work) {
      if (controller) controller.abort();
      controller = new AbortController();
      const mine = ++seq;
      try {
        const value = await work(controller.signal);
        return mine === seq ? { current: true, value } : { current: false };
      } catch (err) {
        if (mine !== seq || (err && err.name === 'AbortError')) return { current: false };
        throw err;
      }
    },
    cancel() {
      if (controller) controller.abort();
      controller = null;
      seq++;
    },
  };
}
