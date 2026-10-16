// Light/dark choice. Loaded as a plain script in <head> on both pages so the
// "dark" class on <html> is set before the first paint. The footer toggle in
// ui.js uses window.mqTheme. "system" follows the OS setting and is the
// default.

(() => {
  const KEY = 'mq-theme';
  const system = window.matchMedia('(prefers-color-scheme: dark)');

  let choice = 'system';
  try {
    const stored = localStorage.getItem(KEY);
    if (stored === 'light' || stored === 'dark') choice = stored;
  } catch {
    // Storage blocked: the choice lasts until the page is closed.
  }

  const apply = () => {
    const theme = choice === 'system' ? (system.matches ? 'dark' : 'light') : choice;
    document.documentElement.classList.toggle('dark', theme === 'dark');
  };

  const set = (next) => {
    choice = next;
    try {
      if (next === 'system') localStorage.removeItem(KEY);
      else localStorage.setItem(KEY, next);
    } catch {
      // See above.
    }
    apply();
  };

  system.addEventListener('change', apply);
  window.mqTheme = { get: () => choice, set };
  apply();
})();
