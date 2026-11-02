# kwil.renner.dev

The project homepage, served by GitHub Pages from this folder. Plain static files:
no build step, no external fonts or scripts. `releases.js` reads the public GitHub
releases API to point the download links at the newest version; without it the
links fall back to the releases page.

- Screenshots in `assets/shots/` are real captures of the app, taken hidden with the
  `kwil-ui-gpu/frame-harness` feature, in Kwil's dark and light themes.
- Speed figures in `index.html` come from the Bench budgets workflow on main; update
  them together with the date when they change noticeably.
- Deploy: run the **Website Pages** workflow from the Actions tab (manual only).
