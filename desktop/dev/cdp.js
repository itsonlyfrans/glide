'use strict';
// Tiny Chrome DevTools Protocol client for the Tauri window on Windows (WebView2), used by the dev checks.
// Start the app with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>.

async function connect(port, { timeoutMs = 20000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let target = null;
  while (!target && Date.now() < deadline) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      target = list.find((t) => t.type === 'page' && /index\.html|tauri\.localhost/.test(t.url));
    } catch { /* not up yet */ }
    if (!target) await new Promise((r) => setTimeout(r, 300));
  }
  if (!target) throw new Error('the Glide window did not appear');
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let nextId = 1;
  const waiting = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg); waiting.delete(msg.id); }
  };
  const send = (method, params = {}) => new Promise((resolve) => {
    const id = nextId++;
    waiting.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description || 'evaluation failed');
    return r.result?.result?.value;
  };
  const screenshot = async (file) => {
    const r = await send('Page.captureScreenshot', { format: 'png' });
    require('node:fs').writeFileSync(file, Buffer.from(r.result.data, 'base64'));
  };
  return { send, evaluate, screenshot, close: () => ws.close() };
}

module.exports = { connect };
