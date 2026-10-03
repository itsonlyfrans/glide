'use strict';
// Shows the update banner and the Settings > Updates section by simulating "update.available" (off-screen).
const { spawn } = require('node:child_process');
const { connect } = require('./cdp');
(async () => {
  const port = 21000 + Math.floor(Math.random() * 9000);
  const app = spawn(process.argv[2], [], { env: { ...process.env, GLIDE_MOCK: '1', GLIDE_TEST_OFFSCREEN: '1', WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` }, stdio: 'ignore' });
  try {
    const page = await connect(port);
    for (let i = 0; i < 60 && !(await page.evaluate('!!(window.glide && document.querySelector("#nav button"))').catch(() => false)); i++) await new Promise((r) => setTimeout(r, 250));
    // The page may not emit app events itself (by design), so hand the event to the screens' own update module.
    await page.evaluate("import('/js/updates.js').then((m) => m.handleUpdateEvent('update.available', { version: '0.2.0' }))");
    await new Promise((r) => setTimeout(r, 600));
    console.log('banner:', await page.evaluate("document.getElementById('banner').innerText"));
    await page.screenshot(process.argv[3] + '/update-banner.png');
    await page.evaluate("document.dispatchEvent(new CustomEvent('glide:navigate', { detail: 'settings' }))");
    await new Promise((r) => setTimeout(r, 600));
    console.log('settings:', await page.evaluate("[...document.querySelectorAll('h2')].find((x) => x.innerText === 'Updates')?.parentElement.innerText"));
    await page.evaluate("[...document.querySelectorAll('h2')].find((x) => x.innerText === 'Updates').scrollIntoView({ block: 'nearest' })");
    await new Promise((r) => setTimeout(r, 300));
    await page.screenshot(process.argv[3] + '/update-settings.png');
    page.close();
  } finally { app.kill(); }
})();
