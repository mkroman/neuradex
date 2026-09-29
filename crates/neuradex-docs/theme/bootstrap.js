/* Theme bootstrap — runs before first paint to avoid a flash of the wrong theme. */
(function () {
  try {
    var t = localStorage.getItem('neuradex-theme');
    if (t !== 'dark' && t !== 'light') {
      t = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    }
    document.documentElement.setAttribute('data-theme', t);
  } catch (e) { /* no storage — CSS defaults apply */ }
})();
