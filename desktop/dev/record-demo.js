'use strict';
// Records a short demo of the real Glide window against the simulated engine (made-up computers), off-screen, as PNG
// frames for ffmpeg. The simulated engine moves the cursor between computers and copies a large file by itself.
//   node dev/record-demo.js <glide.exe> <frames dir>
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const { connect } = require('./cdp');

const FPS = 15;
const exe = process.argv[2];
const out = process.argv[3];
fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out, { recursive: true });

(async () => {
  const port = 21000 + Math.floor(Math.random() * 9000);
  const app = spawn(exe, [], {
    env: { ...process.env, GLIDE_MOCK: '1', GLIDE_TEST_OFFSCREEN: '1', WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` },
    stdio: 'ignore',
  });
  try {
    const page = await connect(port);
    for (let i = 0; i < 60 && !(await page.evaluate('!!document.querySelector(".canvas .device")').catch(() => false)); i++) await new Promise((r) => setTimeout(r, 250));
    await new Promise((r) => setTimeout(r, 800));
    let frame = 0;
    const grab = async (seconds) => {
      const end = Date.now() + seconds * 1000;
      while (Date.now() < end) {
        const started = Date.now();
        const r = await page.send('Page.captureScreenshot', { format: 'png' });
        fs.writeFileSync(path.join(out, `f${String(frame++).padStart(5, '0')}.png`), Buffer.from(r.result.data, 'base64'));
        await new Promise((res) => setTimeout(res, Math.max(0, 1000 / FPS - (Date.now() - started))));
      }
    };
    const show = (view) => page.evaluate(`document.dispatchEvent(new CustomEvent('glide:navigate', { detail: '${view}' }))`);
    await grab(7);                      // the Desk: the cursor travels between the computers
    await show('transfers'); await grab(3.5);   // a large file copying in the background
    await show('devices'); await grab(2.5);     // paired and nearby computers
    await show('desk'); await grab(3);
    page.close();
    console.log(`${frame} frames at ${FPS} fps`);
  } finally { app.kill(); }
})();
