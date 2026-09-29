/* Theme toggle — wires the masthead button and follows system scheme changes
   when the visitor has not chosen a theme explicitly. The pre-paint bootstrap
   (theme/bootstrap.js) has already set data-theme on <html>. */
(function () {
  'use strict';

  const { $ } = window.docsUI;
  const doc = document;

  const THEME_KEY = 'neuradex-theme';

  function storedTheme() {
    try { return localStorage.getItem(THEME_KEY); } catch (e) { return null; }
  }
  function currentTheme() {
    return doc.documentElement.getAttribute('data-theme') === 'dark' ? 'dark' : 'light';
  }
  function applyTheme(theme, persist) {
    doc.documentElement.setAttribute('data-theme', theme);
    const btn = $('#theme-toggle');
    if (btn) {
      const target = theme === 'dark' ? 'light' : 'dark';
      const word = btn.querySelector('.theme-word');
      if (word) word.textContent = target;
      btn.setAttribute('aria-label', 'Switch to the ' + target + ' theme');
    }
    if (persist) {
      try { localStorage.setItem(THEME_KEY, theme); } catch (e) { /* private mode */ }
    }
  }

  const toggleBtn = $('#theme-toggle');
  if (toggleBtn) {
    toggleBtn.addEventListener('click', () => {
      applyTheme(currentTheme() === 'dark' ? 'light' : 'dark', true);
    });
  }
  const mq = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
  if (mq) {
    const onSystemChange = e => {
      if (!storedTheme()) applyTheme(e.matches ? 'dark' : 'light', false);
    };
    if (mq.addEventListener) mq.addEventListener('change', onSystemChange);
    else if (mq.addListener) mq.addListener(onSystemChange);
  }
  applyTheme(currentTheme(), false);
})();
