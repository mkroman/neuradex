/* Shared helpers — everything the other behavior files use lives on
   window.docsUI; this file creates the namespace and must come first. */
(function () {
  'use strict';

  /* The page content is generated at build time from the OpenAPI document (the
     neuradex-docs templates); these helpers exist so the behavior files can wire
     the request builders, filters, and viewers onto the static DOM without
     re-implementing the small utilities each of them needs. */

  const doc = document;
  const docsUI = (window.docsUI = window.docsUI || {});

  const $ = (docsUI.$ = (sel, root) => (root || doc).querySelector(sel));

  const el = (docsUI.el = function (tag, cls, text) {
    const node = doc.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  });

  const esc = (docsUI.esc = function (s) {
    return String(s).replace(/[&<>"']/g, c => (
      { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
    ));
  });

  /* Error prose: backticks become inline <code>, everything stays text nodes. */
  const prose = (docsUI.prose = function (text) {
    const f = doc.createDocumentFragment();
    String(text == null ? '' : text).split(/`([^`]+)`/g).forEach((part, i) => {
      if (!part) return;
      f.appendChild(i % 2 ? el('code', null, part) : doc.createTextNode(part));
    });
    return f;
  });

  const wrapP = (docsUI.wrapP = function (cls, text) {
    const p = el('p', cls);
    p.appendChild(prose(text));
    return p;
  });

  const fmtBytes = (docsUI.fmtBytes = function (n) {
    if (n < 1024) return n + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' kB';
    return (n / (1024 * 1024)).toFixed(2) + ' MB';
  });

  const byteLen = (docsUI.byteLen = function (s) {
    try { return new TextEncoder().encode(s).length; } catch (e) { return s.length; }
  });

  /* ------------------------------------------------ JSON display (runtime responses) */

  const highlightJsonText = (docsUI.highlightJsonText = function (text) {
    const RE = /("(?:[^"\\]|\\.)*")(\s*:)?|\b(?:true|false)\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/g;
    let out = '';
    let last = 0;
    let m;
    while ((m = RE.exec(text))) {
      out += esc(text.slice(last, m.index));
      if (m[1] !== undefined) {
        out += m[2] !== undefined
          ? '<span class="j-key">' + esc(m[1]) + '</span>' + esc(m[2])
          : '<span class="j-str">' + esc(m[1]) + '</span>';
      } else if (m[0] === 'true' || m[0] === 'false' || m[0] === 'null') {
        out += '<span class="j-bool">' + m[0] + '</span>';
      } else {
        out += '<span class="j-num">' + m[0] + '</span>';
      }
      last = RE.lastIndex;
      if (m.index === RE.lastIndex) RE.lastIndex++; /* zero-length safety */
    }
    out += esc(text.slice(last));
    return out;
  });

  const highlightCode = (docsUI.highlightCode = function (value, cap) {
    let text;
    try { text = JSON.stringify(value, null, 2); } catch (e) { text = String(value); }
    if (text === undefined) text = String(value);
    let truncated = false;
    if (cap && text.length > cap) { text = text.slice(0, cap); truncated = true; }
    const code = doc.createElement('code');
    code.innerHTML = highlightJsonText(text);
    if (truncated) code.appendChild(doc.createTextNode('\n… truncated for display'));
    return code;
  });

  /* The error envelope: {"error": {"type", "message"}} */
  const errorEnvelope = (docsUI.errorEnvelope = function (value) {
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      const e = value.error;
      if (e && typeof e === 'object' && !Array.isArray(e)
          && typeof e.type === 'string' && typeof e.message === 'string') return e;
    }
    return null;
  });

  /* --------------------------------------------------------- clipboard */

  const shellQuote = (docsUI.shellQuote = function (s) {
    return "'" + String(s).replace(/'/g, "'\\''") + "'";
  });

  const legacyCopy = (docsUI.legacyCopy = function (text) {
    const ta = doc.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    doc.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = doc.execCommand('copy'); } catch (e) { ok = false; }
    if (ta.parentNode) ta.parentNode.removeChild(ta);
    return ok;
  });

  const copyText = (docsUI.copyText = function (text) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(text).then(() => true, () => legacyCopy(text));
    }
    return Promise.resolve(legacyCopy(text));
  });

  const flash = (docsUI.flash = function (btn, word) {
    if (btn.dataset.orig === undefined) btn.dataset.orig = btn.textContent;
    btn.textContent = word;
    btn.classList.add('flashed');
    clearTimeout(btn._flashTimer);
    btn._flashTimer = setTimeout(() => {
      btn.textContent = btn.dataset.orig;
      btn.classList.remove('flashed');
    }, 1500);
  });
})();
