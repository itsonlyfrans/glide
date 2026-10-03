'use strict';
// Debug helper: open one screen of the Tauri app (mock engine) and print what the main area contains.
//   node dev/probe-view.js <glide.exe> <view>
const { spawn } = require('node:child_process');
const { connect } = require('./cdp');

(async () => {
  const port = 9450;
  const app = spawn(process.argv[2], [], {
    env: { ...process.env, GLIDE_MOCK: '1', GLIDE_TEST_OFFSCREEN: '1', WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` },
    stdio: 'ignore',
  });
  try {
    const page = await connect(port);
    await page.evaluate('new Promise((r) => setTimeout(r, 1500))');
    const report = await page.evaluate(`(async () => {
      const errors = [];
      window.addEventListener('error', (e) => errors.push(e.message));
      window.addEventListener('unhandledrejection', (e) => errors.push(String(e.reason?.message || e.reason)));
      const nav = [...document.querySelectorAll('#nav button')].map((b) => b.innerText.trim());
      const target = [...document.querySelectorAll('#nav button')].find((b) => b.innerText.includes('${process.argv[3]}'));
      target?.click();
      await new Promise((r) => setTimeout(r, 1200));
      return { nav, title: document.getElementById('viewTitle').innerText, view: document.getElementById('view').innerText.slice(0, 400), children: document.getElementById('view').children.length, errors };
    })()`);
    console.log(JSON.stringify(report, null, 1));
    page.close();
  } finally { app.kill(); }
})();
