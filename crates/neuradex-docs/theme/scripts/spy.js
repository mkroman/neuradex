/* Scrollspy and reveal-on-scroll — highlights the toc entry of the endpoint
   section currently in view and reveals sections as they scroll in (disabled
   under prefers-reduced-motion). */
(function () {
  'use strict';

  const { $ } = window.docsUI;
  const doc = document;

  /* ---------------------------------------------------------- scrollspy */

  const spyVisible = new Map();
  let currentActive = null;

  function setActive(id) {
    currentActive = id;
    doc.querySelectorAll('#toc a.active').forEach(a => {
      a.classList.remove('active');
      a.removeAttribute('aria-current');
    });
    if (!id) return;
    const a = $('#toc a[href="#' + id + '"]');
    if (a) {
      a.classList.add('active');
      a.setAttribute('aria-current', 'true');
    }
  }

  function onSpy(entries) {
    entries.forEach(en => spyVisible.set(en.target.id, en.isIntersecting));
    let active = null;
    spyVisible.forEach((visible, id) => { if (visible) active = id; });
    if (active && active !== currentActive) setActive(active);
  }

  /* The old single-file script kept an ordered id list; the map preserves
     insertion order, so iterating it directly is equivalent. */
  const epSections = Array.from(doc.querySelectorAll('#content .ep'));

  if ('IntersectionObserver' in window) {
    const spyObserver = new IntersectionObserver(onSpy, { rootMargin: '-12% 0px -78% 0px', threshold: 0 });
    epSections.forEach(s => spyObserver.observe(s));
    setActive(epSections.length ? epSections[0].id : null);
  }

  /* ------------------------------------------------------------- reveal */

  function reduceMotion() {
    return window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  }

  if (!reduceMotion() && 'IntersectionObserver' in window) {
    const io = new IntersectionObserver(entries => {
      entries.forEach(e => {
        if (e.isIntersecting) {
          e.target.classList.add('shown');
          io.unobserve(e.target);
        }
      });
    }, { threshold: 0.06 });
    epSections.forEach(s => {
      s.classList.add('reveal');
      io.observe(s);
    });
  }
})();
