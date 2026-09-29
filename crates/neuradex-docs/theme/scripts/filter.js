/* Instant filter — collects the filterable targets (endpoint sections and
   schema anchors), applies the query from the sidebar input, and wires the
   input, Escape, and `/` shortcut listeners. */
(function () {
  'use strict';

  const { $ } = window.docsUI;
  const doc = document;

  const filterInput = $('#filter');
  const filterCount = $('#filter-count');
  const filterTargets = [];

  doc.querySelectorAll('#content .ep').forEach(sec => {
    const a = $('#toc a[href="#' + sec.id + '"]');
    filterTargets.push({
      endpoint: true,
      section: sec,
      li: a ? a.parentElement : null,
      hay: sec.textContent.toLowerCase(),
    });
  });
  doc.querySelectorAll('#content .schema-anchor').forEach(anchor => {
    const a = $('#toc a[href="#' + anchor.id + '"]');
    filterTargets.push({
      endpoint: false,
      section: anchor,
      li: a ? a.parentElement : null,
      hay: ('schema ' + anchor.textContent).toLowerCase(),
    });
  });

  function applyFilter() {
    const q = filterInput.value.trim().toLowerCase();
    let shown = 0;
    let total = 0;
    filterTargets.forEach(t => {
      if (t.endpoint) total++;
      const match = !q || t.hay.indexOf(q) !== -1;
      if (t.section) t.section.classList.toggle('filtered', !match);
      if (t.li) t.li.hidden = !match;
      if (t.endpoint && match) shown++;
    });
    doc.querySelectorAll('#toc .toc-tag').forEach(grp => {
      const items = grp.querySelectorAll('li');
      grp.hidden = items.length > 0 && Array.prototype.every.call(items, li => li.hidden);
    });
    filterCount.textContent = q ? shown + ' of ' + total + ' endpoints' : '';
  }
  window.docsUI.applyFilter = applyFilter;

  filterInput.addEventListener('input', applyFilter);
  filterInput.addEventListener('keydown', e => {
    if (e.key === 'Escape') {
      filterInput.value = '';
      applyFilter();
      filterInput.blur();
    }
  });
  window.addEventListener('keydown', e => {
    if (e.key !== '/' || e.ctrlKey || e.metaKey || e.altKey) return;
    const t = e.target;
    const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' ||
      t.tagName === 'SELECT' || t.isContentEditable);
    if (typing) return;
    e.preventDefault();
    filterInput.focus();
    filterInput.select();
  });

  applyFilter();
})();
