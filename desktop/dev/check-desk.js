'use strict';
// Desk interaction check (off-screen): drag empty space pans, the mouse wheel zooms around the pointer, dragging a
// computer moves only that computer, and a plain click on empty space clears the selection.
const { spawn } = require('node:child_process');
const { connect } = require('./cdp');
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) failed++; };
(async () => {
  const port = 21000 + Math.floor(Math.random() * 9000);
  const app = spawn(process.argv[2], [], { env: { ...process.env, GLIDE_MOCK: '1', GLIDE_MOCK_USER: '1', GLIDE_TEST_OFFSCREEN: '1', WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` }, stdio: 'ignore' });
  try {
    const page = await connect(port);
    for (let i = 0; i < 60 && !(await page.evaluate('!!document.querySelector(".canvas .device")').catch(() => false)); i++) await new Promise((r) => setTimeout(r, 250));
    const boxes = () => page.evaluate('JSON.stringify([...document.querySelectorAll(".canvas .device")].map((e) => { const r = e.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width }; }))').then(JSON.parse);
    const mouse = (type, x, y, extra = {}) => page.send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1, buttons: type === 'mouseReleased' ? 0 : 1, ...extra });
    const wait = (ms) => new Promise((r) => setTimeout(r, ms));
    // Wait until the layout has stopped moving (the window may still be settling its size).
    for (let last = '', i = 0; i < 40; i++) { const now = JSON.stringify(await boxes()); if (now === last) break; last = now; await wait(300); }
    // An empty spot: top-left area of the canvas.
    const spot = await page.evaluate('(() => { const r = document.querySelector(".canvas").getBoundingClientRect(); return JSON.stringify({ x: r.left + 30, y: r.top + 30 }); })()').then(JSON.parse);
    const a = await boxes();
    await mouse('mousePressed', spot.x, spot.y);
    for (let i = 1; i <= 10; i++) { await mouse('mouseMoved', spot.x + i * 12, spot.y + i * 6); await wait(16); }
    await mouse('mouseReleased', spot.x + 120, spot.y + 60);
    await wait(100);
    const b = await boxes();
    check(b.every((box, i) => Math.abs(box.x - a[i].x - 120) < 2 && Math.abs(box.y - a[i].y - 60) < 2), 'dragging empty space moves the whole view');
    await page.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: spot.x + 300, y: spot.y + 200, deltaX: 0, deltaY: -120 });
    await wait(100);
    const c = await boxes();
    check(c[0].w > b[0].w * 1.1, `the wheel zooms in (${b[0].w.toFixed(0)} -> ${c[0].w.toFixed(0)} px)`);
    // Drag the second computer by 80 px: only it moves.
    const m = c[1];
    await mouse('mousePressed', m.x + 10, m.y + 10);
    for (let i = 1; i <= 8; i++) { await mouse('mouseMoved', m.x + 10 + i * 10, m.y + 10 + i * 10); await wait(16); }
    await mouse('mouseReleased', m.x + 90, m.y + 90);
    await wait(400);
    const d = await boxes();
    check(Math.abs(d[0].x - c[0].x) < 2 && (Math.abs(d[1].x - c[1].x) > 20 || Math.abs(d[1].y - c[1].y) > 20), 'dragging a computer moves only that computer');
    check(await page.evaluate('!!document.querySelector(".inspector h2")'), 'dragging selects that computer');
    await mouse('mousePressed', spot.x, spot.y); await mouse('mouseReleased', spot.x, spot.y);
    await wait(150);
    check(!(await page.evaluate('!!document.querySelector(".inspector h2")')), 'a click on empty space clears the selection');
    // Move this computer's top screen to the right of its bottom screen, on its own.
    await page.evaluate("document.querySelector('.recenter').click()");
    await wait(300);
    const mons = await page.evaluate('JSON.stringify([...document.querySelectorAll(".canvas .device")][0] ? [...[...document.querySelectorAll(".canvas .device")][0].querySelectorAll(".mon")].map((e) => { const r = e.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; }) : [])').then(JSON.parse);
    const [top, bottom] = mons[0].y < mons[1].y ? [mons[0], mons[1]] : [mons[1], mons[0]];
    const grab = { x: top.x + top.w / 2, y: top.y + top.h / 2 };
    const target = { x: bottom.x + bottom.w + top.w / 2 + 4, y: bottom.y + top.h / 2 };
    await mouse('mousePressed', grab.x, grab.y);
    for (let i = 1; i <= 12; i++) { await mouse('mouseMoved', grab.x + (target.x - grab.x) * i / 12, grab.y + (target.y - grab.y) * i / 12); await wait(16); }
    await mouse('mouseReleased', target.x, target.y);
    await wait(500);
    const state = await page.evaluate('window.glide.call("get_state", {})');
    const placed = state.result.settings.display?.arrangement ?? [];
    const self = state.result.self.monitors;
    const topNow = self.find((m) => m.y === Math.min(...self.map((x) => x.y)) && m.w < 4000) ?? self[0];
    const bottomNow = self.find((m) => m.w >= 5000);
    check(placed.length === 2, 'moving one screen saves an arrangement for this computer');
    check(topNow && bottomNow && Math.abs(topNow.x - (bottomNow.x + bottomNow.w)) < 2, `the top screen now sits against the right edge of the bottom one (${JSON.stringify(self.map((m) => [m.id, m.x, m.y]))})`);
    const others = state.result.layout.devices.find((x) => x.device_id !== state.result.self.device_id);
    check(!!others, 'the other computer keeps its place');
    if (process.argv[3]) await page.screenshot(process.argv[3]);
    page.close();
  } finally { app.kill(); }
  process.exit(failed ? 1 : 0);
})();
