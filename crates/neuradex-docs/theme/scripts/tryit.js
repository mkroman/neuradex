/* Request builders — build the query string from the field inputs, preview the
   URL, send the request, render the result viewer, and copy cURL lines. Each
   .tryit widget on the page is wired here. */
(function () {
  'use strict';

  const { $, el, fmtBytes, byteLen, highlightCode, errorEnvelope, wrapP, shellQuote,
    copyText, flash } = window.docsUI;
  const doc = document;

  function queryParams(widget) {
    const sp = new URLSearchParams();
    widget.querySelectorAll('.field input').forEach(input => {
      const raw = input.value.trim();
      if (!raw) return;
      if (input.dataset.array) {
        raw.split(/[,\s]+/).filter(Boolean).forEach(v => sp.append(input.dataset.name, v));
      } else {
        sp.append(input.dataset.name, raw);
      }
    });
    return sp.toString();
  }

  function renderResult(widget, res, text, ms) {
    const result = $('.result', widget);
    result.replaceChildren();
    const meta = el('div', 'result-meta');
    meta.appendChild(el('span', 'status-pill ' + (res.ok ? 'status-ok' : 'status-err'),
      String(res.status) + (res.statusText ? ' ' + res.statusText : '')));
    meta.appendChild(el('span', 'meta-bit', Math.round(ms) + ' ms'));
    meta.appendChild(el('span', 'meta-bit', fmtBytes(byteLen(text))));
    if (!text) meta.appendChild(el('span', 'meta-bit dim', 'empty body'));
    result.appendChild(meta);

    let value;
    let isJson = false;
    if (text) {
      const ct = res.headers.get('content-type') || '';
      if (/json/i.test(ct) || /^\s*[[{]/.test(text)) {
        try { value = JSON.parse(text); isJson = true; } catch (e) { isJson = false; }
      }
    }
    if (isJson) {
      const env = errorEnvelope(value);
      if (env) {
        const box = el('div', 'error-friendly');
        const line = el('div', 'err-line');
        line.appendChild(el('span', 'err-type-pill', env.type));
        line.appendChild(el('span', 'err-kind', 'error envelope'));
        box.appendChild(line);
        box.appendChild(wrapP('err-msg', env.message));
        result.appendChild(box);
      }
      const pre = el('pre', 'json-view');
      pre.appendChild(highlightCode(value, 120000));
      result.appendChild(pre);
    } else if (text) {
      const pre = el('pre', 'json-view rawtext');
      pre.textContent = text.length > 20000 ? text.slice(0, 20000) + '\n…' : text;
      result.appendChild(pre);
    }
    result.hidden = false;
  }

  function wireTryIt(widget) {
    const path = widget.dataset.path;
    const preview = $('.url-preview', widget);
    const sendBtn = $('button.action-primary', widget);
    const curlBtn = $('button.action-secondary', widget);
    const inputs = Array.from(widget.querySelectorAll('.field input'));

    function updatePreview() {
      const qs = queryParams(widget);
      preview.textContent = 'GET ' + path + (qs ? '?' + qs : '');
    }
    inputs.forEach(input => input.addEventListener('input', updatePreview));
    updatePreview();

    let inFlight = false;
    sendBtn.addEventListener('click', () => {
      if (inFlight) return;
      inFlight = true;
      sendBtn.disabled = true;
      sendBtn.classList.add('sending');
      sendBtn.textContent = 'sending';
      const dots = el('span', 'dots');
      dots.appendChild(el('span'));
      dots.appendChild(el('span'));
      dots.appendChild(el('span'));
      sendBtn.appendChild(dots);
      const result = $('.result', widget);
      result.hidden = true;

      const t0 = performance.now();
      const qs = queryParams(widget);
      fetch(path + (qs ? '?' + qs : ''), { headers: { accept: 'application/json' } })
        .then(res => res.text().then(text => ({ res, text })))
        .then(r => { renderResult(widget, r.res, r.text, performance.now() - t0); })
        .catch(err => {
          const result = $('.result', widget);
          result.replaceChildren();
          const p = el('p', 'network-error');
          p.textContent = 'The request could not be made — ' +
            (err && err.message ? err.message : 'network error') + '.';
          result.appendChild(p);
          result.hidden = false;
        })
        .then(() => {
          inFlight = false;
          sendBtn.disabled = false;
          sendBtn.classList.remove('sending');
          sendBtn.textContent = 'send request';
        });
    });

    curlBtn.addEventListener('click', () => {
      const qs = queryParams(widget);
      const origin = window.location.origin && window.location.origin.indexOf('http') === 0
        ? window.location.origin : '';
      const line = 'curl -sS ' + shellQuote(origin + path + (qs ? '?' + qs : ''));
      copyText(line).then(ok => flash(curlBtn, ok ? 'copied' : 'copy failed'));
    });
  }

  doc.querySelectorAll('.tryit').forEach(wireTryIt);
})();
