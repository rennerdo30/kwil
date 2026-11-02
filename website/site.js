// Kwil homepage: the screenshot viewer in the hero.
// Four real captures of the app, each in Kwil's dark and light theme.
// Without this script the page shows the Find capture in the visitor's colour scheme.
(function () {
  'use strict';

  var root = document.querySelector('.viewer');
  if (!root) return;

  var img = root.querySelector('[data-shot]');
  var picture = img.parentNode;
  var caption = root.querySelector('#viewer-caption');
  var viewButtons = root.querySelectorAll('[data-view]');
  var themeButtons = root.querySelectorAll('[data-theme]');

  var views = {
    editor: {
      size: [1600, 1015],
      alt: 'Kwil editing a Rust file with the Find bar open and four matches of “value” highlighted.',
      caption: 'Find highlights every match as you type, with case, whole-word, regex and in-selection options.'
    },
    palette: {
      size: [1600, 1015],
      alt: 'The command palette over a Rust file, listing file commands with their keyboard shortcuts.',
      caption: 'Every command is one search away, with the shortcut it has in your keymap.'
    },
    markdown: {
      size: [1600, 1015],
      alt: 'A Markdown note in split view: source on the left, the rendered preview on the right.',
      caption: 'Markdown in text, preview or split view. Ctrl+Shift+V switches.'
    },
    settings: {
      size: [1598, 1114],
      alt: 'The Settings page with categories on the left and searchable settings.',
      caption: 'Settings are searchable and save as you change them. Everything also lives in one TOML file.'
    }
  };

  var dark = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
  var state = { view: 'editor', theme: dark && !dark.matches ? 'light' : 'dark', chosenTheme: false };

  // Preload the other captures once the page is idle, so switching is instant.
  function preload() {
    Object.keys(views).forEach(function (v) {
      ['dark', 'light'].forEach(function (t) { (new Image()).src = 'assets/shots/' + v + '-' + t + '.webp'; });
    });
  }

  function show(announce) {
    var v = views[state.view];
    // The script owns the theme from now on; drop the CSS-media fallback source.
    var source = picture.querySelector('source');
    if (source) picture.removeChild(source);
    img.src = 'assets/shots/' + state.view + '-' + state.theme + '.webp';
    img.width = v.size[0];
    img.height = v.size[1];
    img.alt = v.alt;
    caption.textContent = v.caption;
    caption.setAttribute('aria-live', announce ? 'polite' : 'off');
    viewButtons.forEach(function (b) { b.setAttribute('aria-pressed', String(b.getAttribute('data-view') === state.view)); });
    themeButtons.forEach(function (b) { b.setAttribute('aria-pressed', String(b.getAttribute('data-theme') === state.theme)); });
  }

  viewButtons.forEach(function (b) {
    b.addEventListener('click', function () { state.view = b.getAttribute('data-view'); show(true); });
  });
  themeButtons.forEach(function (b) {
    b.addEventListener('click', function () { state.theme = b.getAttribute('data-theme'); state.chosenTheme = true; show(true); });
  });
  if (dark && dark.addEventListener) {
    dark.addEventListener('change', function (e) {
      if (state.chosenTheme) return;
      state.theme = e.matches ? 'dark' : 'light';
      show(false);
    });
  }

  show(false);
  if ('requestIdleCallback' in window) window.requestIdleCallback(preload); else setTimeout(preload, 1500);
})();
