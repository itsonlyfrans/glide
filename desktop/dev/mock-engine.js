'use strict';
// Dev stand-in for glided: implements the SPEC §7 JSON-Lines contract with fake devices.
// Run via `pnpm dev` (electron . --mock). Pairing code accepted: 123456.
const readline = require('node:readline');

const out = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);
const event = (name, data = {}) => out({ event: name, data });
const clone = (o) => JSON.parse(JSON.stringify(o));
const deepMerge = (t, p) => {
  for (const [k, v] of Object.entries(p ?? {})) {
    if (v && typeof v === 'object' && !Array.isArray(v) && t[k] && typeof t[k] === 'object') deepMerge(t[k], v);
    else t[k] = v;
  }
  return t;
};

const hex = (n) => Array.from({ length: n }, () => Math.floor(Math.random() * 16).toString(16)).join('');
const fp = (id) => id.slice(0, 32).match(/.{4}/g).join(' ');

const SELF_ID = hex(64);
const MAC_ID = hex(64);
const MINI_ID = hex(64);
const NEAR_ID = hex(64);

const state = {
  self: { device_id: SELF_ID, name: 'Studio PC', os: 'windows', fingerprint: fp(SELF_ID), listen_port: 24800, version: '0.1.0-mock',
    model: { name: 'Windows PC', kind: 'desktop' } },
  sharing_enabled: true,
  active_device_id: SELF_ID,
  permissions: { accessibility: 'n/a', input_monitoring: 'n/a', injection: 'granted' },
  peers: [
    {
      device_id: MAC_ID, name: 'MacBook Pro', os: 'macos', fingerprint: fp(MAC_ID), online: true, connection: 'connected',
      address: '192.168.1.42:24800', latency_ms: 2.4, clipboard_enabled: true, app_version: '0.2.2',
      model: { name: 'MacBook Pro 14-inch', kind: 'laptop', builtin_monitor: 'm1' },
      monitors: [{ id: 'm1', x: 0, y: 0, w: 1512, h: 982, scale: 2, primary: true }],
    },
    {
      device_id: MINI_ID, name: 'Mac mini', os: 'macos', fingerprint: fp(MINI_ID), online: false, connection: 'offline',
      address: '192.168.1.57:24800', clipboard_enabled: true, app_version: '0.2.2', wake_mac: 'a4:83:e7:0a:1b:2c',
      model: { name: 'Mac mini', kind: 'mini' },
      monitors: [{ id: 'm1', x: 0, y: 0, w: 2560, h: 1440, scale: 1, primary: true }],
    },
  ],
  discovered: [{ device_id: NEAR_ID, name: 'Living room PC', os: 'windows', address: '192.168.1.71:24800' }],
  layout: {
    devices: [
      { device_id: SELF_ID, x: 0, y: 0 },
      { device_id: MAC_ID, x: 3840, y: 160 },
      { device_id: MINI_ID, x: 0, y: -1440 },
    ],
  },
  settings: {
    device_name: 'Studio PC',
    hotkeys: { return_home: 'Ctrl+Alt+Shift+Home', toggle_sharing: 'Ctrl+Alt+Shift+S' },
    clipboard: { enabled: true, sync_text: true, sync_images: true, sync_files: true, max_auto_mb: 2048, exclude_sensitive: true },
    switching: { edge_delay_ms: 0, corner_dead_zone_px: 4, double_tap: false, pointer_speed: 1, pointer_acceleration: 0 },
    keyboard: { swap_ctrl_cmd: 'auto' },
    startup: { launch_at_login: false, start_minimized: false },
    network: { port: 24800, discovery: true },
  },
  transfers: [],
};
// The mock self device has two 1920x1080 monitors side by side.
state.self.monitors = [
  { id: 'a', x: 0, y: 0, w: 1920, h: 1080, scale: 1, primary: true },
  { id: 'b', x: 1920, y: 0, w: 1920, h: 1080, scale: 1, primary: false },
];

// Test the window's "engine keeps stopping" screen: start, complain, and exit.
if (process.env.GLIDE_MOCK_CRASH) {
  console.error('2026-10-03T12:00:00Z ERROR glided: Error: could not open the secure key store (simulated)');
  setTimeout(() => process.exit(1), 150);
}

if (process.env.GLIDE_MOCK_EMPTY) { state.peers = []; state.layout.devices = [state.layout.devices[0]]; }

if (process.env.GLIDE_MOCK_MAC) {
  state.self.os = 'macos';
  state.permissions = { accessibility: 'denied', input_monitoring: 'denied', injection: 'denied', restart_required: false };
}

// A two-monitor PC with mixed scaling, as the corrected Windows backend reports it (uniform space, primary scale 1.5):
// primary 7680x2160 -> 5120x1440 at the bottom, a 5120x1440 monitor above it -> 3413x960 centred over it. A MacBook sits flush right of the top one.
if (process.env.GLIDE_MOCK_USER) {
  state.self.name = 'Office PC';
  state.self.monitors = [
    { id: '\\\\.\\DISPLAY1', x: 870, y: 0, w: 5120 / 1.5, h: 960, scale: 1, primary: false },
    { id: '\\\\.\\DISPLAY2', x: 0, y: 960, w: 5120, h: 1440, scale: 1.5, primary: true },
  ];
  state.peers = [{
    device_id: MAC_ID, name: 'MacBook Pro', os: 'macos', fingerprint: fp(MAC_ID), online: true, connection: 'connected',
    address: '192.168.1.30:24800', latency_ms: 1.0, clipboard_enabled: true, app_version: '0.2.2',
    model: { name: 'MacBook Pro 16-inch', kind: 'laptop', builtin_monitor: '1' },
    monitors: [{ id: '1', x: 0, y: 0, w: 1728, h: 1117, scale: 2, primary: true }],
  }];
  state.discovered = [];
  state.layout.devices = [{ device_id: SELF_ID, x: 0, y: 0 }, { device_id: MAC_ID, x: 4283, y: 0 }];
}

let hostCode = null;
let hostTimer = null;
let failedAttempts = 0;
let confirmWaiter = null;
let pushTimer = null;
const pushState = () => {
  if (pushTimer) return;
  pushTimer = setTimeout(() => { pushTimer = null; event('state', clone(state)); }, 30);
};

const ok = (id, result = {}) => out({ id, ok: true, result });
const fail = (id, code, message) => out({ id, ok: false, error: { code, message } });

let transferSeq = 1;
function startFakeTransfer(name, bytes, direction = 'receive') {
  const t = {
    id: `t${transferSeq++}`, direction, peer_id: MAC_ID, name, items: 1,
    bytes_total: bytes, bytes_done: 0, rate_bps: 0, state: 'active',
  };
  state.transfers.unshift(t);
  pushState();
  const timer = setInterval(() => {
    if (t.state !== 'active') { clearInterval(timer); return; }
    const rate = 90e6 + Math.random() * 25e6;
    t.bytes_done = Math.min(t.bytes_total, t.bytes_done + rate / 10);
    t.rate_bps = rate * 8;
    if (t.bytes_done >= t.bytes_total) {
      t.state = 'done';
      t.rate_bps = 0;
      clearInterval(timer);
      pushState();
      event('notification', { level: 'info', title: 'Ready to paste', body: `${t.name} arrived from ${state.peers[0]?.name ?? 'another computer'}.` });
    } else {
      event('transfer.progress', { id: t.id, bytes_done: t.bytes_done, rate_bps: t.rate_bps });
    }
  }, 100);
}

// Like glided: a custom arrangement of this computer's own screens is normalized to (0,0), this computer moves on the desk
// by the same offset so nothing jumps, and an arrangement that does not fit the screens falls back to the system one.
let nativeMonitors = null;
function applyArrangement() {
  nativeMonitors ??= state.self.monitors.map((m) => ({ ...m }));
  state.settings.display ??= { arrangement: [] };
  const placements = state.settings.display.arrangement ?? [];
  const fits = placements.length > 0 && nativeMonitors.every((m) => placements.some((p) => p.monitor_id === m.id));
  if (!fits) { state.self.monitors = nativeMonitors.map((m) => ({ ...m })); state.settings.display.arrangement = []; return; }
  const minX = Math.min(...placements.map((p) => p.x)), minY = Math.min(...placements.map((p) => p.y));
  state.settings.display.arrangement = placements.map((p) => ({ ...p, x: p.x - minX, y: p.y - minY }));
  state.self.monitors = nativeMonitors.map((m) => {
    const p = state.settings.display.arrangement.find((q) => q.monitor_id === m.id);
    return { ...m, x: p.x, y: p.y };
  });
  const me = state.layout.devices.find((d) => d.device_id === SELF_ID);
  if (me) { me.x += minX; me.y += minY; }
}

const handlers = {
  get_state: () => clone(state),
  set_settings: ({ patch }) => {
    const before = JSON.stringify(state.settings.display ?? {});
    deepMerge(state.settings, patch);
    if (patch?.device_name) state.self.name = patch.device_name;
    if (JSON.stringify(state.settings.display ?? {}) !== before) applyArrangement();
    pushState();
    return {};
  },
  set_sharing: ({ enabled }) => { state.sharing_enabled = !!enabled; if (!enabled) state.active_device_id = SELF_ID; pushState(); return {}; },
  set_layout: ({ devices }) => { state.layout.devices = devices; pushState(); return {}; },
  'pairing.start_host': () => {
    hostCode = String(Math.floor(100000 + Math.random() * 900000));
    const expires = Date.now() + 120000;
    clearTimeout(hostTimer);
    hostTimer = setTimeout(() => { hostCode = null; }, 120000);
    return { code: hostCode, expires_at_ms: expires };
  },
  'pairing.confirm': ({ accepted }) => { confirmWaiter?.(!!accepted); return {}; },
  'pairing.cancel_host': () => { hostCode = null; clearTimeout(hostTimer); return {}; },
  'pairing.join': async ({ device_id, address, code }) => {
    // Match the real engine: exactly one target and a six-digit code; answer at once, report progress as events.
    if (!!device_id === !!address || !/^\d{6}$/.test(code ?? '')) throw { code: 'invalid_params', message: 'provide one target and a six-digit code' };
    (async () => {
      await new Promise((r) => setTimeout(r, 700));
      const fail = (c, message) => event('pairing.result', { ok: false, error: { code: c, message } });
      if (failedAttempts >= 3) return fail('locked_out', 'Too many wrong codes. Ask the other computer for a new one.');
      if (code !== '123456') { failedAttempts += 1; return fail('bad_code', 'That code is not right.'); }
      failedAttempts = 0;
      const peerInfo = state.discovered.find((d) => d.device_id === device_id || d.address === address) ?? { name: 'New device', os: 'windows' };
      event('pairing.verify', { phrase: ['harbor', 'violet', 'anchor'], peer: { name: peerInfo.name, os: peerInfo.os }, expires_at_ms: Date.now() + 60000 });
      const accepted = await new Promise((resolve) => { confirmWaiter = resolve; setTimeout(() => resolve(false), 60000); });
      confirmWaiter = null;
      if (!accepted) return fail('bad_code', 'Pairing was cancelled because the words did not match.');
      const idx = state.discovered.findIndex((d) => d.device_id === device_id || d.address === address);
      const d = idx >= 0 ? state.discovered.splice(idx, 1)[0] : { device_id: hex(64), name: 'New device', os: 'windows', address };
      state.peers.push({ ...d, fingerprint: fp(d.device_id), online: true, connection: 'connected', latency_ms: 1.1, clipboard_enabled: true,
        monitors: [{ id: 'm1', x: 0, y: 0, w: 1920, h: 1080, scale: 1, primary: true }] });
      state.layout.devices.push({ device_id: d.device_id, x: 0, y: 1080 });
      pushState();
      event('pairing.result', { ok: true, device_id: d.device_id });
    })();
    return {};
  },
  'peer.add_manual': async ({ address }) => {
    await new Promise((r) => setTimeout(r, 500));
    if (!/^[\w.-]+:\d{2,5}$/.test(address ?? '')) throw { code: 'invalid_params', message: 'Use the form address:port, like 192.168.1.20:24800.' };
    if (!state.discovered.some((x) => x.address === address)) state.discovered.push({ device_id: hex(64), name: `Device at ${address.split(':')[0]}`, os: 'windows', address });
    pushState();
    return {};
  },
  'peer.unpair': ({ device_id }) => {
    state.peers = state.peers.filter((p) => p.device_id !== device_id);
    state.layout.devices = state.layout.devices.filter((d) => d.device_id !== device_id);
    if (state.active_device_id === device_id) state.active_device_id = SELF_ID;
    pushState();
    return {};
  },
  'peer.wake': ({ device_id }) => {
    const p = state.peers.find((x) => x.device_id === device_id);
    if (!p) throw { code: 'invalid_params', message: 'unknown device' };
    if (p.online) return {};
    if (!p.wake_mac) throw { code: 'unreachable', message: 'Glide learns how to wake this computer the next time both are connected.' };
    p.connection = 'connecting'; pushState();
    setTimeout(() => { p.online = true; p.connection = 'connected'; p.latency_ms = 3.1; pushState(); }, 2500);
    return {};
  },
  // Like glided on the other computer: the arrangement is normalized and that computer moves on the desk by the same offset.
  'peer.arrange': ({ device_id, arrangement }) => {
    const p = state.peers.find((x) => x.device_id === device_id);
    if (!p) throw { code: 'invalid_params', message: 'unknown device' };
    if (!p.online) throw { code: 'unreachable', message: `${p.name} is not connected right now.` };
    const minX = Math.min(...arrangement.map((a) => a.x)), minY = Math.min(...arrangement.map((a) => a.y));
    p.monitors = p.monitors.map((m) => { const a = arrangement.find((q) => q.monitor_id === m.id); return a ? { ...m, x: a.x - minX, y: a.y - minY } : m; });
    const placed = state.layout.devices.find((d) => d.device_id === device_id);
    if (placed) { placed.x += minX; placed.y += minY; }
    pushState();
    return {};
  },
  'peer.configure': ({ device_id, clipboard_enabled }) => {
    const p = state.peers.find((x) => x.device_id === device_id);
    if (!p) throw { code: 'not_paired', message: 'That device is not paired.' };
    if (typeof clipboard_enabled === 'boolean') p.clipboard_enabled = clipboard_enabled;
    pushState();
    return {};
  },
  return_home: () => { state.active_device_id = SELF_ID; event('active_changed', { device_id: SELF_ID, reason: 'hotkey' }); pushState(); return {}; },
  'transfer.cancel': ({ id }) => { const t = state.transfers.find((x) => x.id === id); if (t) t.state = 'cancelled'; pushState(); return {}; },
  'transfer.confirm': ({ id, accept }) => { const t = state.transfers.find((x) => x.id === id); if (t) t.state = accept ? 'active' : 'cancelled'; pushState(); return {}; },
  'permissions.request': () => {
    // GLIDE_MOCK_STUCK: like macOS when an older Glide is still listed - nothing happens at all.
    if (process.env.GLIDE_MOCK_STUCK) return {};
    // Like macOS: dialogs appear; the person grants Accessibility first, then Input Monitoring (which wants a restart).
    setTimeout(() => { state.permissions.accessibility = 'granted'; state.permissions.injection = 'granted'; pushState(); }, 1500);
    setTimeout(() => { state.permissions.input_monitoring = 'granted'; state.permissions.restart_required = true; pushState(); }, 3000);
    return {};
  },
  'permissions.open_settings': () => ({}),
  'app.shutdown': () => { setTimeout(() => process.exit(0), 20); return {}; },
};

readline.createInterface({ input: process.stdin }).on('line', async (line) => {
  let req;
  try { req = JSON.parse(line); } catch { return; }
  const fn = handlers[req.method];
  if (!fn) return fail(req.id, 'invalid_params', `Unknown method ${req.method}`);
  try {
    ok(req.id, await fn(req.params ?? {}));
  } catch (e) {
    fail(req.id, e.code ?? 'internal', e.message ?? String(e));
  }
});

event('ready', { version: '0.1.0-mock' });
event('state', clone(state));

// Ambient simulation so the UI has something alive to show.
setInterval(() => {
  const mac = state.peers[0];
  if (!mac || !mac.online) return;
  mac.latency_ms = +(1.8 + Math.random() * 1.4).toFixed(1);
  event('peer.stats', { device_id: MAC_ID, latency_ms: mac.latency_ms, rx_bps: Math.random() * 4e5, tx_bps: Math.random() * 4e5 });
}, 1000);

if (!process.env.GLIDE_MOCK_QUIET) {
  setInterval(() => {
    if (!state.sharing_enabled || !state.peers[0]?.online) return;
    const to = state.active_device_id === SELF_ID ? MAC_ID : SELF_ID;
    state.active_device_id = to;
    event('active_changed', { device_id: to, reason: 'edge' });
    pushState();
  }, 7000);
  setTimeout(() => startFakeTransfer('Launch teaser v3.mp4', 1.4e9), 4000);
}
