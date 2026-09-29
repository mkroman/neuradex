/* Schema trees — the expand/collapse-all controls of the widgets that host a
   .schema-tree. */
(function () {
  'use strict';

  const doc = document;
  const docsUI = (window.docsUI = window.docsUI || {});

  const setAllTrees = (docsUI.setAllTrees = function (root, open) {
    root.querySelectorAll('details').forEach(d => { d.open = open; });
  });

  doc.querySelectorAll('.widget-tools').forEach(tools => {
    const widget = tools.closest('.widget');
    const box = widget ? widget.querySelector('.schema-tree') : null;
    if (!box) return;
    tools.querySelectorAll('button.tool').forEach(btn => {
      btn.addEventListener('click', () => setAllTrees(box, btn.dataset.treeAction === 'expand'));
    });
  });
})();
