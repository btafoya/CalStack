/* Tabler theme: apply saved (or OS) color mode before first paint, wire the
   navbar toggle via delegation so it works on every page from one file. */
(function () {
  'use strict';
  var saved = null;
  try { saved = localStorage.getItem('cal-theme'); } catch (e) { /* private mode */ }
  var mode = saved === 'dark' || saved === 'light' ? saved
    : (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
  document.documentElement.setAttribute('data-bs-theme', mode);
  document.addEventListener('click', function (ev) {
    var btn = ev.target.closest && ev.target.closest('#theme-toggle');
    if (!btn) { return; }
    var next = document.documentElement.getAttribute('data-bs-theme') === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-bs-theme', next);
    try { localStorage.setItem('cal-theme', next); } catch (e) { /* private mode */ }
  });
})();