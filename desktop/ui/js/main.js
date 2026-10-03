import { h, clear } from './dom.js';
import { icon } from './icons.js';
import { call, onEvent, meta } from './api.js';
import { store, subscribe, applyEvent, nameOf } from './store.js';
import { toast } from './ui.js';
import { updates, handleUpdateEvent, onUpdateChange, installNow } from './updates.js';
import { mountDesk } from './views/desk.js';
import { mountDevices } from './views/devices.js';
import { mountTransfers } from './views/transfers.js';
import { mountSettings } from './views/settings.js';

const VIEWS = {
  desk: { title: 'Desk', icon: 'desk', mount: mountDesk },
  devices: { title: 'Computers', icon: 'devices', mount: mountDevices },
  transfers: { title: 'Files', icon: 'transfers', mount: mountTransfers },
  settings: { title: 'Settings', icon: 'settings', mount: mountSettings },
};

const $ = (id) => document.getElementById(id);
let current = null;      // { name, api }

function mountView(name) {
  current?.api.unmount?.();
  store.view = name;
  const root = clear($('view'));
  $('viewTitle').textContent = VIEWS[name].title;
  current = { name, api: VIEWS[name].mount(root) };
  renderNav();
}

function renderNav() {
  const s = store.state;
  const active = (s?.transfers ?? []).filter((t) => t.state === 'active' || t.state === 'awaiting_confirm').length;
  const nav = clear($('nav'));
  for (const [name, v] of Object.entries(VIEWS)) {
    const count = name === 'transfers' && active ? active : name === 'devices' && s?.discovered.length ? s.discovered.length : 0;
    nav.append(h('button', { 'aria-current': store.view === name ? 'page' : null, onclick: () => mountView(name) },
      icon(v.icon), v.title, count ? h('span', { class: 'count', 'aria-label': `${count} waiting` }, count) : null));
  }
}

function renderTopbar(s) {
  const right = clear($('topbarRight'));
  const cb = h('input', { type: 'checkbox', onchange: async (e) => {
    try { await call('set_sharing', { enabled: e.target.checked }); } catch (err) { toast(err.message, 'error'); e.target.checked = !e.target.checked; }
  } });
  cb.checked = s.sharing_enabled;
  right.append(h('label', { class: 'switch' }, cb, h('span', { class: 'track' }), h('span', { class: 'lbl' }, s.sharing_enabled ? 'Sharing is on' : 'Sharing is off')));
}

function renderRailFoot(s) {
  const foot = clear($('railFoot'));
  const onSelf = s.active_device_id === s.self.device_id;
  const connected = s.peers.filter((p) => p.connection === 'connected').length;
  foot.append(
    h('div', { class: 'where' }, s.sharing_enabled ? icon('pointer', 'here-icon') : h('i', { class: 'dot off' }),
      s.sharing_enabled ? (onSelf ? 'Cursor is here' : `Cursor is on ${nameOf(s.active_device_id)}`) : 'Sharing paused'),
    h('div', { class: 'sub' }, `${s.self.name} · ${connected} of ${s.peers.length} connected`));
}

const PERMISSIONS = [
  { key: 'accessibility', kind: 'accessibility', name: 'Accessibility', why: 'Lets Glide move the cursor and press keys on this Mac.' },
  { key: 'input_monitoring', kind: 'input_monitoring', name: 'Input Monitoring', why: 'Lets Glide notice your mouse and keyboard so it can share them.' },
];
const needs = (v) => v === 'denied' || v === 'unknown';
let askedAt = 0;
const stuck = new Set(); // permissions where Allow did not lead to a macOS prompt

async function allow(p) {
  askedAt = Date.now();
  try { await call('permissions.request'); } catch { /* the engine may not support it yet */ }
  // If nothing changed, macOS most likely kept quiet because an older Glide is still listed. Offer the reset.
  setTimeout(() => {
    if (!needs(store.state?.permissions?.[p.key])) return;
    stuck.add(p.key);
    renderBanner(store.state);
  }, 2500);
}

async function resetAndAsk(p) {
  const missing = PERMISSIONS.filter((x) => needs(store.state?.permissions?.[x.key])).map((x) => x.kind);
  const done = await window.glide.resetPermissions(missing.length ? missing : [p.kind]).catch(() => false);
  if (done) { window.glide.relaunch(); return; }
  // Could not reset (not a packaged Mac build): fall back to the System Settings list.
  call('permissions.open_settings', { kind: p.kind }).catch(() => {});
}

function permissionsCard(s) {
  const perms = s.permissions;
  const row = (p) => {
    const ok = perms[p.key] === 'granted';
    const isStuck = !ok && stuck.has(p.key);
    return h('div', { class: 'perm-row' },
      h('i', { class: `dot ${ok ? 'ok' : 'off'}` }),
      h('div', { class: 'grow' }, h('div', { class: 'title' }, p.name),
        h('div', { class: 'desc' }, isStuck ? 'macOS did not ask? An older copy of Glide is probably still in its list. Reset clears only Glide, then Glide restarts and macOS asks again.' : p.why)),
      ok ? h('span', { class: 'chip ok' }, 'Allowed')
        : isStuck ? h('button', { class: 'btn sm primary', onclick: () => resetAndAsk(p) }, 'Reset and ask again')
          : h('button', { class: 'btn sm primary', onclick: () => allow(p) }, 'Allow'));
  };
  return h('div', { class: 'banner perms', role: 'alert' },
    h('div', { class: 'grow' },
      h('strong', null, 'Allow Glide to control this Mac'),
      h('p', null, 'macOS needs your OK first. Choose Allow, then switch Glide on in the list that macOS opens.'),
      h('div', { class: 'perm-grid' }, PERMISSIONS.map(row)),
      s.permissions.restart_required
        ? h('p', { class: 'perm-restart' }, 'Almost done. macOS only applies this after Glide restarts. ', h('button', { class: 'btn sm primary', onclick: () => window.glide.relaunch() }, 'Restart Glide'))
        : null));
}

function renderBanner(s) {
  const b = clear($('banner'));
  const banner = (cls, text, action) => b.append(h('div', { class: `banner ${cls}`, role: 'alert' }, h('p', null, text), action));
  if (store.engine === 'fatal') return banner('error', store.engineMessage || 'The Glide engine stopped.');
  if (store.engine === 'down') {
    return document.body.classList.contains('attached')
      ? banner('error', 'Glide is not running in the background, so nothing is being shared.', h('button', { class: 'btn sm primary', onclick: () => window.glide.startEngine() }, 'Start Glide'))
      : banner('error', 'The Glide engine stopped unexpectedly. Restarting…');
  }
  if (!s) return;
  const p = s.permissions;
  if (PERMISSIONS.some((x) => needs(p[x.key])) || (p.restart_required && PERMISSIONS.some((x) => p[x.key] !== 'n/a'))) {
    b.append(permissionsCard(s));
  } else if (needs(p.injection)) {
    banner('', 'Glide cannot control this computer right now. Check that nothing is blocking it.');
  } else if (updates.installing) {
    banner('update', updates.progress == null ? 'Downloading the update…' : `Downloading the update… ${Math.round(updates.progress * 100)}%`);
  } else if (updates.available) {
    banner('update', `Glide ${updates.available.version} is ready. It installs in a few seconds and Glide restarts by itself.`,
      h('button', { class: 'btn sm primary', onclick: () => installNow() }, 'Update now'));
  }
}

function render(kind) {
  const s = store.state;
  renderBanner(s);
  if (!s) return;
  if (!current) mountView(store.view);      // first snapshot: bring the UI up
  if (kind !== 'stats' && kind !== 'progress') { renderTopbar(s); renderNav(); }
  renderRailFoot(s);
  current.api.update?.(s, kind);
}

async function boot() {
  const m = await meta();
  document.body.classList.add(m.platform === 'darwin' ? 'plat-mac' : 'plat-win');
  if (m.attached) document.body.classList.add('attached');
  clear($('view')).append(h('p', { class: 'empty', style: { padding: '28px' } }, 'Starting the Glide engine…'));
  onEvent((name, data) => {
    if (handleUpdateEvent(name, data)) return;
    if (name === 'notification') toast(data.body ?? '', data.level === 'error' ? 'error' : 'info', data.title);
    applyEvent(name, data);
  });
  onUpdateChange(() => renderBanner(store.state));
  subscribe(render);
  document.addEventListener('glide:navigate', (e) => mountView(e.detail));
  window.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && /^[1-4]$/.test(e.key)) mountView(Object.keys(VIEWS)[+e.key - 1]);
  });
  // The engine pushes a snapshot after "ready"; ask too in case we attached late.
  try { applyEvent('state', await call('get_state')); } catch { /* engine still starting */ }
}

boot();
