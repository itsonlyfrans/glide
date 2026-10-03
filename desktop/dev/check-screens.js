'use strict';
// Opens every screen of the Tauri app (Windows) against the mock engine and checks that each one really shows (right
// title, real content), that no raw programming words leak into the text, and that the window.glide bridge answers
// and refuses methods outside its allowlist. Saves a screenshot per screen.
//   node dev/check-screens.js <path to glide.exe> [out dir]
const { spawn } = require('node:child_process');
const path = require('node:path');
const { connect } = require('./cdp');

const exe = process.argv[2];
const outDir = process.argv[3];
const scenarios = [{}, { GLIDE_MOCK_MAC: '1' }, { GLIDE_MOCK_USER: '1' }];
const views = [['desk', 'Desk'], ['devices', 'Computers'], ['transfers', 'Files'], ['settings', 'Settings']];
const bad = /\b(null|undefined|NaN)\b/;
let failed = 0;

const showView = (view) => `(async () => {
  document.dispatchEvent(new CustomEvent('glide:navigate', { detail: '${view}' }));
  await new Promise((r) => setTimeout(r, 500));
  const pick = '${view}' === 'desk' && document.querySelector('.canvas .dev');
  if (pick) pick.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  await new Promise((r) => setTimeout(r, 300));
  return {
    text: document.body.innerText.split('\\n').join(' | '),
    title: document.getElementById('viewTitle').innerText,
    main: document.getElementById('view').innerText.trim().length,
  };
})()`;

(async () => {
  for (const [index, scenario] of scenarios.entries()) {
    // A fresh port each run: a web view left over from an earlier run could still be holding a fixed one.
    const port = 20000 + Math.floor(Math.random() * 20000) + index;
    const app = spawn(exe, [], {
      env: { ...process.env, ...scenario, GLIDE_MOCK: '1', GLIDE_TEST_OFFSCREEN: '1', WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` },
      stdio: 'ignore',
    });
    const label = Object.keys(scenario)[0] || 'windows';
    try {
      const page = await connect(port);
      // Poll from outside: the page can still be switching from about:blank to Glide when we first connect.
      for (let i = 0; i < 60; i++) {
        const ready = await page.evaluate('!!(window.glide && document.querySelector("#nav button"))').catch(() => false);
        if (ready) break;
        await new Promise((r) => setTimeout(r, 250));
      }
      const meta = await page.evaluate('window.glide.meta()');
      if (meta?.platform !== 'win32' || meta?.mock !== true) { console.log(`FAIL ${label}: bridge meta ${JSON.stringify(meta)}`); failed++; }
      const state = await page.evaluate('window.glide.call("get_state", {})');
      if (!state?.ok) { console.log(`FAIL ${label}: get_state through the bridge`); failed++; }
      const blocked = await page.evaluate('window.glide.call("app.shutdown", {})');
      if (blocked?.ok !== false) { console.log(`FAIL ${label}: a method outside the allowlist was not refused`); failed++; }
      for (const [view, expected] of views) {
        const shown = await page.evaluate(showView(view));
        const hit = bad.exec(shown.text);
        if (shown.title !== expected || shown.main < 20) {
          console.log(`FAIL ${label}/${view}: showed "${shown.title}" with ${shown.main} characters of content`);
          failed++;
        } else if (hit) {
          console.log(`FAIL ${label}/${view}: shows "${hit[0]}"`);
          failed++;
        } else console.log(`ok   ${label}/${view}`);
        if (outDir) await page.screenshot(path.join(outDir, `tauri-${label}-${view}.png`));
      }
      page.close();
    } catch (e) {
      console.log(`FAIL ${label}: ${e.message}`);
      failed++;
    } finally {
      app.kill();
    }
  }
  process.exit(failed ? 1 : 0);
})();
