'use strict';
// Read-only check of attach mode against the engine already running on this PC: the window connects with the token
// and reads state. Changes nothing. Window stays off-screen.
const { spawn } = require('node:child_process');
const { connect } = require('./cdp');
(async () => {
  const port = 21000 + Math.floor(Math.random() * 9000);
  const app = spawn(process.argv[2], [], { env: { ...process.env, GLIDE_TEST_OFFSCREEN: '1', WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` }, stdio: 'ignore' });
  try {
    const page = await connect(port);
    let r = null;
    for (let i = 0; i < 40 && !r?.ok; i++) {
      r = await page.evaluate('window.glide ? window.glide.call("get_state", {}) : null').catch(() => null);
      if (!r?.ok) await new Promise((res) => setTimeout(res, 250));
    }
    const meta = await page.evaluate('window.glide.meta()');
    console.log('meta', JSON.stringify(meta));
    if (r?.ok) {
      const s = r.result;
      console.log(`state ok: this computer "${s.self.name}", ${s.self.monitors.length} monitors, peers: ${s.peers.map((p) => `${p.name}=${p.connection}`).join(', ')}`);
    } else console.log('FAILED', JSON.stringify(r));
    page.close();
  } finally { app.kill(); }
})();
