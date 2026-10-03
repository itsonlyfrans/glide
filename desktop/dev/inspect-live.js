'use strict';
// Read-only look at the RUNNING Glide engine (the one you installed): what it believes about monitors, layout and peers.
// Prints no secrets. Uses the same local, same-user, token-protected channel the Glide window uses.
const net = require('node:net');
const readline = require('node:readline');
const fs = require('node:fs');
const path = require('node:path');

const dataDir = process.argv[2] || path.join(process.env.APPDATA, 'Glide', 'engine');
const info = JSON.parse(fs.readFileSync(path.join(dataDir, 'ipc.json'), 'utf8'));
const s = net.createConnection(info.endpoint);
const waiters = [];
readline.createInterface({ input: s }).on('line', (l) => { let m; try { m = JSON.parse(l); } catch { return; } waiters.forEach((w) => w(m)); });
s.on('error', (e) => { console.log('cannot connect:', e.code); process.exit(1); });
s.on('connect', () => s.write(`${JSON.stringify({ auth: info.token })}\n`));
const call = (method, params = {}) => new Promise((res) => {
  const id = Math.floor(Math.random() * 1e9);
  waiters.push((m) => { if (m.id === id) res(m); });
  s.write(`${JSON.stringify({ id, method, params })}\n`);
  setTimeout(() => res(null), 5000);
});
const mon = (m) => `${m.id}: at (${m.x},${m.y}) size ${m.w}x${m.h} scale ${m.scale}${m.primary ? ' PRIMARY' : ''}`;

(async () => {
  await new Promise((r) => setTimeout(r, 800));
  const r = await call('get_state');
  if (!r || !r.ok) { console.log('no state:', JSON.stringify(r)); process.exit(1); }
  const st = r.result;
  console.log(`engine ${st.self.version}, this computer "${st.self.name}" (${st.self.os})`);
  console.log(`sharing_enabled=${st.sharing_enabled}  active_device=${st.active_device_id === st.self.device_id ? 'THIS computer' : st.active_device_id.slice(0, 8)}`);
  console.log('permissions:', JSON.stringify(st.permissions));
  console.log('\nTHIS computer monitors (as the engine reports them):');
  for (const m of st.self.monitors) console.log('   ' + mon(m));
  console.log('\nPEERS:');
  for (const p of st.peers) {
    console.log(`   ${p.name} (${p.os}) connection=${p.connection} online=${p.online} latency=${p.latency_ms}ms address=${p.address}`);
    for (const m of p.monitors) console.log('      ' + mon(m));
  }
  console.log('\nLAYOUT (where each computer sits in the shared space):');
  for (const d of st.layout.devices) {
    const who = d.device_id === st.self.device_id ? st.self.name : (st.peers.find((p) => p.device_id === d.device_id)?.name ?? d.device_id.slice(0, 8));
    console.log(`   ${who}: origin (${d.x}, ${d.y})`);
  }
  console.log('\nswitching settings:', JSON.stringify(st.settings.switching));
  console.log('hotkeys:', JSON.stringify(st.settings.hotkeys));
  s.destroy();
  process.exit(0);
})();
