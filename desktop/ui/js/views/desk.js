import { h, clear, osName } from '../dom.js';
import { icon, osIcon } from '../icons.js';
import { call } from '../api.js';
import { store, onActiveChanged } from '../store.js';
import { confirmModal, toast } from '../ui.js';
import { openHostModal } from '../pair.js';

const SNAP_PX = 16;      // snap distance in screen pixels
const MARGIN = 0.3;      // spare room around the layout, as a fraction of its size

// Devices on the canvas: self + every paired peer, with sizes from their monitors.
function collectDevices(state) {
  const placed = new Map(state.layout.devices.map((d) => [d.device_id, d]));
  const all = [{ id: state.self.device_id, dev: state.self, self: true }, ...state.peers.map((p) => ({ id: p.device_id, dev: p }))];
  const list = all.map(({ id, dev, self }) => {
    const mons = dev.monitors?.length ? dev.monitors : [{ id: 'm', x: 0, y: 0, w: 1920, h: 1080, scale: 1, primary: true }];
    const minX = Math.min(...mons.map((m) => m.x)), minY = Math.min(...mons.map((m) => m.y));
    const w = Math.max(...mons.map((m) => m.x + m.w)) - minX, hgt = Math.max(...mons.map((m) => m.y + m.h)) - minY;
    return { id, dev, self: !!self, w, h: hgt, mons: mons.map((m) => ({ ...m, x: m.x - minX, y: m.y - minY })), x: placed.get(id)?.x, y: placed.get(id)?.y };
  });
  // Anything the engine hasn't placed yet goes to the right of what is.
  let right = Math.max(0, ...list.filter((d) => d.x != null).map((d) => d.x + d.w));
  for (const d of list) if (d.x == null) { d.x = right + 120; d.y = 0; right = d.x + d.w; }
  return list;
}

// Everything below works on the real MONITOR rectangles, not on a computer's overall bounding box: a computer with two
// monitors in an L-shape has empty corners, and the cursor can only cross where monitors actually touch.
const absMons = (d, x = d.x, y = d.y) => d.mons.map((m) => ({ x: x + m.x, y: y + m.y, w: m.w, h: m.h }));
const rectsOverlap = (a, b) => a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y;

// Snap a dragged computer so one of its monitors sits flush against another computer's monitor, then push it out of any overlap.
function snapDevice(dev, pos, others, threshold) {
  const theirs = others.flatMap((o) => absMons(o));
  let bestX = { d: threshold, v: null }, bestY = { d: threshold, v: null };
  for (const m of dev.mons) for (const o of theirs) {
    const xs = [o.x + o.w - m.x, o.x - m.w - m.x, o.x - m.x, o.x + o.w - m.w - m.x];
    const ys = [o.y + o.h - m.y, o.y - m.h - m.y, o.y - m.y, o.y + o.h - m.h - m.y];
    for (const t of xs) if (Math.abs(t - pos.x) < bestX.d) bestX = { d: Math.abs(t - pos.x), v: t };
    for (const t of ys) if (Math.abs(t - pos.y) < bestY.d) bestY = { d: Math.abs(t - pos.y), v: t };
  }
  const out = { x: bestX.v ?? pos.x, y: bestY.v ?? pos.y };
  for (let i = 0; i < 8; i++) {
    let pushed = false;
    for (const a of absMons(dev, out.x, out.y)) {
      const hit = theirs.find((b) => rectsOverlap(a, b));
      if (!hit) continue;
      const moves = [[hit.x + hit.w - a.x, 0], [hit.x - a.w - a.x, 0], [0, hit.y + hit.h - a.y], [0, hit.y - a.h - a.y]];
      moves.sort((p, q) => Math.abs(p[0]) + Math.abs(p[1]) - Math.abs(q[0]) - Math.abs(q[1]));
      out.x += moves[0][0]; out.y += moves[0][1];
      pushed = true;
      break;
    }
    if (!pushed) break;
  }
  return out;
}

// Snap one screen (world rectangle) flush against any of `others` (world rectangles), then push it out of any overlap.
function snapRect(rect, others, threshold) {
  let bestX = { d: threshold, v: null }, bestY = { d: threshold, v: null };
  for (const o of others) {
    for (const t of [o.x + o.w, o.x - rect.w, o.x, o.x + o.w - rect.w]) if (Math.abs(t - rect.x) < bestX.d) bestX = { d: Math.abs(t - rect.x), v: t };
    for (const t of [o.y + o.h, o.y - rect.h, o.y, o.y + o.h - rect.h]) if (Math.abs(t - rect.y) < bestY.d) bestY = { d: Math.abs(t - rect.y), v: t };
  }
  const out = { ...rect, x: bestX.v ?? rect.x, y: bestY.v ?? rect.y };
  for (let i = 0; i < 8; i++) {
    const hit = others.find((b) => rectsOverlap(out, b));
    if (!hit) break;
    const moves = [[hit.x + hit.w - out.x, 0], [hit.x - out.w - out.x, 0], [0, hit.y + hit.h - out.y], [0, hit.y - out.h - out.y]];
    moves.sort((p, q) => Math.abs(p[0]) + Math.abs(p[1]) - Math.abs(q[0]) - Math.abs(q[1]));
    out.x += moves[0][0]; out.y += moves[0][1];
  }
  return out;
}

// Where monitors of two different computers share an edge, the cursor can cross. Returns segments in world coordinates.
function seams(list) {
  const out = [];
  for (let i = 0; i < list.length; i++) for (let j = i + 1; j < list.length; j++) {
    for (const a of absMons(list[i])) for (const b of absMons(list[j])) {
      const vy0 = Math.max(a.y, b.y), vy1 = Math.min(a.y + a.h, b.y + b.h);
      const hx0 = Math.max(a.x, b.x), hx1 = Math.min(a.x + a.w, b.x + b.w);
      if (Math.abs(a.x + a.w - b.x) < 1 && vy1 > vy0) out.push({ x: b.x, y: vy0, w: 0, h: vy1 - vy0 });
      else if (Math.abs(b.x + b.w - a.x) < 1 && vy1 > vy0) out.push({ x: a.x, y: vy0, w: 0, h: vy1 - vy0 });
      else if (Math.abs(a.y + a.h - b.y) < 1 && hx1 > hx0) out.push({ x: hx0, y: b.y, w: hx1 - hx0, h: 0 });
      else if (Math.abs(b.y + b.h - a.y) < 1 && hx1 > hx0) out.push({ x: hx0, y: a.y, w: hx1 - hx0, h: 0 });
    }
  }
  return out;
}

export function mountDesk(root) {
  const canvas = h('div', { class: 'canvas' });
  const threadSvg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  threadSvg.setAttribute('class', 'thread');
  const wrap = h('div', { class: 'canvas-wrap' }, canvas, threadSvg,
    h('div', { class: 'hint' }, 'Drag screens to match how they sit on your desk. Where two screens touch, the cursor crosses. Drag empty space to look around; pinch or Ctrl+scroll to zoom.'));
  const recenter = h('button', { class: 'btn sm quiet recenter', title: 'Fit all screens in view', onclick: () => { fit(); place(); paint(store.state); } }, 'Center view');
  wrap.append(recenter);
  const welcome = h('div', { class: 'welcome' });
  wrap.append(welcome);
  const inspector = h('aside', { class: 'inspector', 'aria-label': 'Device details' });
  root.append(h('div', { class: 'desk' }, wrap, inspector));

  let list = [];
  let tf = { s: 0.1, ox: 0, oy: 0 };   // world → screen
  let fitScale = 0.1;                  // the scale "Center view" uses; zoom is limited around it
  let dragging = null;
  let panning = null;
  let frame = 0, framePaint = false;
  // Redraw at most once per display frame, however many pointer or wheel events arrive in between.
  const schedule = (withPaint) => {
    framePaint ||= withPaint;
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      place();
      if (framePaint) paint(store.state);
      framePaint = false;
    });
  };
  // Let computers glide to a new spot (after a drop or a change from the engine) instead of jumping.
  let settleTimer = 0;
  const settle = () => {
    canvas.classList.add('settle');
    clearTimeout(settleTimer);
    settleTimer = setTimeout(() => canvas.classList.remove('settle'), 260);
  };
  const els = new Map();               // id → { el, label, mons, pin }

  const toScreen = (x, y) => ({ x: tf.ox + x * tf.s, y: tf.oy + y * tf.s });

  function fit() {
    const { width: cw, height: ch } = wrap.getBoundingClientRect();
    if (!cw || !list.length) return;
    const minX = Math.min(...list.map((d) => d.x)), minY = Math.min(...list.map((d) => d.y));
    const maxX = Math.max(...list.map((d) => d.x + d.w)), maxY = Math.max(...list.map((d) => d.y + d.h));
    const w = (maxX - minX) * (1 + MARGIN), hh = (maxY - minY) * (1 + MARGIN);
    const s = Math.min(cw / w, (ch - 40) / hh, 0.3);
    fitScale = s;
    tf = { s, ox: (cw - (maxX - minX) * s) / 2 - minX * s, oy: (ch - (maxY - minY) * s) / 2 - minY * s - 14 + (store.state?.peers.length ? 0 : 150) };
  }

  // The view only re-fits on its own when a computer would be completely out of sight;
  // otherwise the canvas stays exactly where the user left it.
  function contained() {
    const { width: cw, height: ch } = wrap.getBoundingClientRect();
    return list.every((d) => {
      const p = toScreen(d.x, d.y);
      const cx = p.x + (d.w * tf.s) / 2, cy = p.y + (d.h * tf.s) / 2;
      return cx > 0 && cx < cw && cy > 0 && cy < ch;
    });
  }

  function place() {
    for (const d of list) {
      const rec = els.get(d.id);
      if (!rec) continue;
      const p = toScreen(d.x, d.y);
      Object.assign(rec.el.style, { left: `${p.x}px`, top: `${p.y}px`, width: `${d.w * tf.s}px`, height: `${d.h * tf.s}px` });
      d.mons.forEach((m, i) => Object.assign(rec.mons[i].style, {
        left: `${m.x * tf.s}px`, top: `${m.y * tf.s}px`, width: `${m.w * tf.s - 3}px`, height: `${m.h * tf.s - 3}px`,
      }));
    }
    for (const el of canvas.querySelectorAll('.seam')) el.remove();
    for (const sm of seams(list)) {
      const p = toScreen(sm.x, sm.y);
      canvas.append(h('i', { class: 'seam', style: {
        left: `${p.x - (sm.w === 0 ? 1.5 : 0)}px`, top: `${p.y - (sm.h === 0 ? 1.5 : 0)}px`,
        width: `${sm.w === 0 ? 3 : sm.w * tf.s}px`, height: `${sm.h === 0 ? 3 : sm.h * tf.s}px`,
      } }));
    }
  }

  function statusText(d) {
    if (d.self) return { dot: 'ok', text: 'This computer' };
    const p = d.dev;
    if (p.connection === 'connected') return { dot: 'ok', text: p.latency_ms != null ? `Connected · ${p.latency_ms.toFixed(1)} ms` : 'Connected' };
    if (p.connection === 'connecting') return { dot: 'off', text: 'Connecting…' };
    return { dot: 'off', text: 'Offline' };
  }

  function build() {
    clear(canvas); els.clear();
    for (const d of list) {
      const own = d.self && d.mons.length > 1;
      const mons = d.mons.map((_, i) => h('div', {
        class: own ? 'mon own' : 'mon',
        onpointerdown: own ? (e) => { if (e.button === 0 && !e.shiftKey) { e.stopPropagation(); startMonitorDrag(e, d, i); } } : null,
      }));
      const label = h('div', { class: 'info' });
      mons[0]?.append(label);
      const pin = h('i', { class: 'cursor-pin' });
      const el = h('div', {
        class: 'device', tabindex: 0, role: 'button', 'aria-label': `${d.dev.name}, ${osName(d.dev.os)}`,
        onpointerdown: (e) => startDrag(e, d), onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') select(d.id); },
      }, mons, pin);
      canvas.append(el);
      els.set(d.id, { el, label, mons, pin });
    }
  }

  function paint(state) {
    for (const d of list) {
      const rec = els.get(d.id);
      if (!rec) continue;
      const active = state.active_device_id === d.id && state.sharing_enabled;
      const offline = !d.self && d.dev.connection !== 'connected';
      rec.el.className = `device ${d.dev.os === 'macos' ? 'mac' : 'win'}${active ? ' active' : ''}${offline ? ' offline' : ''}${store.selectedId === d.id ? ' selected' : ''}${dragging?.d === d ? ' dragging' : ''}`;
      const st = statusText(d);
      clear(rec.label).append(osIcon(d.dev.os, 'glyph'), h('div', { class: 'nm' }, d.dev.name), h('div', { class: 'st' }, h('i', { class: `dot ${st.dot}` }), st.text));
      rec.pin.style.display = active ? '' : 'none';
      if (active) {
        const m = d.mons.find((x) => x.primary) ?? d.mons[0];
        Object.assign(rec.pin.style, { left: `${(m.x + m.w) * tf.s - 30}px`, top: `${m.y * tf.s + 14}px` });
      }
    }
  }

  function select(id) { store.selectedId = id; paint(store.state); renderInspector(store.state); }

  function startDrag(e, d) {
    if (e.button !== 0) return;
    select(d.id);
    const el = els.get(d.id).el;
    el.setPointerCapture(e.pointerId);
    const start = { px: e.clientX, py: e.clientY, x: d.x, y: d.y };
    dragging = { d, moved: false };
    paint(store.state);
    const others = list.filter((o) => o !== d);
    const move = (ev) => {
      const dx = (ev.clientX - start.px) / tf.s, dy = (ev.clientY - start.py) / tf.s;
      if (Math.abs(dx * tf.s) + Math.abs(dy * tf.s) > 3) dragging.moved = true;
      const r = snapDevice(d, { x: start.x + dx, y: start.y + dy }, others, SNAP_PX / tf.s);
      d.x = r.x; d.y = r.y; schedule(false);
    };
    const up = async () => {
      el.removeEventListener('pointermove', move); el.removeEventListener('pointerup', up); el.removeEventListener('pointercancel', up);
      const moved = dragging.moved; dragging = null;
      paint(store.state);
      if (!moved) return;
      const devices = list.map((x) => ({ device_id: x.id, x: Math.round(x.x), y: Math.round(x.y) }));
      store.state.layout.devices = devices;
      try { await call('set_layout', { devices }); } catch (err) { toast(err.message, 'error'); }
      // The view stays exactly where the person put it (they can pan, zoom or use Center view).
      settle(); place(); paint(store.state);
    };
    el.addEventListener('pointermove', move); el.addEventListener('pointerup', up); el.addEventListener('pointercancel', up);
  }

  // Move one of this computer's screens on its own. The engine stores the arrangement, reports the arranged screens
  // to the other computers and keeps the real screens in sync, so the cursor follows this arrangement.
  function startMonitorDrag(e, d, index) {
    select(d.id);
    const rec = els.get(d.id);
    const monEl = rec.mons[index];
    monEl.setPointerCapture(e.pointerId);
    const m = d.mons[index];
    const start = { px: e.clientX, py: e.clientY, x: m.x, y: m.y };
    dragging = { d, moved: false, monitor: index };
    monEl.classList.add('lifted');
    paint(store.state);
    const others = [
      ...d.mons.filter((_, j) => j !== index).map((o) => ({ x: d.x + o.x, y: d.y + o.y, w: o.w, h: o.h })),
      ...list.filter((o) => o !== d).flatMap((o) => absMons(o)),
    ];
    const move = (ev) => {
      const dx = (ev.clientX - start.px) / tf.s, dy = (ev.clientY - start.py) / tf.s;
      if (Math.abs(dx * tf.s) + Math.abs(dy * tf.s) > 3) dragging.moved = true;
      const r = snapRect({ x: d.x + start.x + dx, y: d.y + start.y + dy, w: m.w, h: m.h }, others, SNAP_PX / tf.s);
      m.x = r.x - d.x; m.y = r.y - d.y;
      schedule(false);
    };
    const up = async () => {
      monEl.removeEventListener('pointermove', move); monEl.removeEventListener('pointerup', up); monEl.removeEventListener('pointercancel', up);
      monEl.classList.remove('lifted');
      const moved = dragging.moved; dragging = null;
      paint(store.state);
      if (!moved) return;
      // Positions relative to this computer's current spot; the engine normalizes them and keeps everything in place.
      const arrangement = d.mons.map((x) => ({ monitor_id: x.id, x: Math.round(x.x), y: Math.round(x.y) }));
      try { await call('set_settings', { patch: { display: { arrangement } } }); } catch (err) { toast(err.message, 'error'); }
      settle(); place(); paint(store.state);
    };
    monEl.addEventListener('pointermove', move); monEl.addEventListener('pointerup', up); monEl.addEventListener('pointercancel', up);
  }

  function renderInspector(state) {
    clear(inspector);
    const d = list.find((x) => x.id === store.selectedId);
    if (!d) {
      inspector.append(h('p', { class: 'empty-inspector' }, 'Select a screen to see its details, or drag it to rearrange.'),
        h('p', { class: 'empty-inspector' }, `${list.length} ${list.length === 1 ? 'computer' : 'computers'} on your desk.`));
      return;
    }
    const dev = d.dev;
    const st = statusText(d);
    inspector.append(
      h('h2', null, dev.name),
      h('div', { class: 'meta' },
        h('span', { class: `chip ${dev.os === 'macos' ? 'mac' : 'win'}` }, osIcon(dev.os), osName(dev.os)),
        d.self ? h('span', { class: 'chip' }, 'This computer') : h('span', { class: `chip status ${dev.connection === 'connected' ? 'ok' : ''}` }, st.text)),
      h('dl', { class: 'kv' },
        h('dt', null, 'Screens'), h('dd', null, d.mons.map((m) => `${m.w}×${m.h}`).join(' + ') + (d.mons[0].scale > 1 ? (dev.os === 'macos' ? ' · Retina screen' : ` · ${Math.round(d.mons[0].scale * 100)}% scaling`) : '')),
        dev.address ? [h('dt', null, 'Address'), h('dd', null, dev.address)] : null,
        h('dt', null, 'Fingerprint'), h('dd', { class: 'fp' }, dev.fingerprint)),
    );
    if (d.mons.length > 1) {
      const system = dev.os === 'macos' ? 'macOS' : 'Windows';
      const custom = d.self && (state.settings?.display?.arrangement?.length ?? 0) > 0;
      inspector.append(h('div', { class: 'note' },
        d.self
          ? h('p', null, `Drag a screen to place it on its own, for example the top monitor to the right of the bottom one. The cursor follows the arrangement you make here. Hold Shift and drag to move the whole computer.${custom ? '' : ` Right now the screens are arranged as in ${system} Display settings.`}`)
          : h('p', null, `These ${d.mons.length} screens move together here. To arrange them one by one, open Glide on ${dev.name}.`),
        d.self && custom
          ? h('button', { class: 'btn sm', onclick: async () => {
            try { await call('set_settings', { patch: { display: { arrangement: [] } } }); toast(`Screens arranged as in ${system} again`); } catch (err) { toast(err.message, 'error'); }
          } }, `Use the ${system} arrangement`)
          : null));
    }
    const stack = h('div', { class: 'stack' });
    if (!d.self) {
      const cb = h('input', { type: 'checkbox', onchange: async (e) => {
        try { await call('peer.configure', { device_id: d.id, clipboard_enabled: e.target.checked }); } catch (err) { toast(err.message, 'error'); e.target.checked = !e.target.checked; }
      } });
      cb.checked = !!dev.clipboard_enabled;
      stack.append(
        h('label', { class: 'switch' }, cb, h('span', { class: 'track' }), h('span', { class: 'lbl' }, 'Share clipboard with this computer')),
        h('button', { class: 'btn', onclick: () => call('return_home').catch(() => {}) }, icon('home'), 'Bring cursor back here'),
        h('button', { class: 'btn danger', onclick: async () => {
          const yes = await confirmModal({ title: `Unpair ${dev.name}?`, body: 'It will stop receiving your keyboard, mouse and clipboard, and must be paired again with a new code.', confirm: 'Unpair', danger: true });
          if (!yes) return;
          try { await call('peer.unpair', { device_id: d.id }); store.selectedId = null; toast(`${dev.name} unpaired`); } catch (err) { toast(err.message, 'error'); }
        } }, 'Unpair this computer'),
      );
    }
    inspector.append(stack);
  }

  // Draw the cursor's journey between screens.
  const offActive = onActiveChanged((from, to) => {
    const a = els.get(from), b = els.get(to);
    if (!a || !b) return;
    const wr = wrap.getBoundingClientRect();
    const c = (rec) => { const r = rec.el.getBoundingClientRect(); return { x: r.left - wr.left + r.width / 2, y: r.top - wr.top + r.height / 2 }; };
    const p = c(a), q = c(b);
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', `M${p.x} ${p.y} Q${(p.x + q.x) / 2} ${Math.min(p.y, q.y) - 60} ${q.x} ${q.y}`);
    threadSvg.append(path);
    const len = path.getTotalLength();
    path.style.strokeDasharray = `${len}`;
    const anim = path.animate([{ strokeDashoffset: len, opacity: 1 }, { strokeDashoffset: 0, opacity: 1, offset: 0.5 }, { strokeDashoffset: 0, opacity: 0 }], { duration: 1100, easing: 'ease-out' });
    anim.onfinish = () => path.remove();
  });

  // Drag empty space to look around. A click on empty space (no drag) clears the selection.
  canvas.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || e.target.closest('.device')) return;
    canvas.setPointerCapture(e.pointerId);
    panning = { px: e.clientX, py: e.clientY, ox: tf.ox, oy: tf.oy, moved: false };
    wrap.classList.add('panning');
  });
  canvas.addEventListener('pointermove', (e) => {
    if (!panning) return;
    const dx = e.clientX - panning.px, dy = e.clientY - panning.py;
    if (Math.abs(dx) + Math.abs(dy) > 3) panning.moved = true;
    tf.ox = panning.ox + dx; tf.oy = panning.oy + dy;
    schedule(false);
  });
  const endPan = () => {
    if (!panning) return;
    const clicked = !panning.moved;
    panning = null;
    wrap.classList.remove('panning');
    if (clicked && store.selectedId) select(null);
  };
  canvas.addEventListener('pointerup', endPan);
  canvas.addEventListener('pointercancel', endPan);

  // Pinch (trackpad), Ctrl/Cmd + wheel, or a plain mouse wheel zooms around the pointer; two-finger scrolling pans.
  wrap.addEventListener('wheel', (e) => {
    e.preventDefault();
    const mouseWheel = e.deltaMode === 1 || (e.deltaX === 0 && e.wheelDeltaY !== 0 && e.wheelDeltaY % 120 === 0);
    if (!(e.ctrlKey || e.metaKey || mouseWheel)) {
      tf.ox -= e.deltaX; tf.oy -= e.deltaY;
      schedule(false);
      return;
    }
    const r = wrap.getBoundingClientRect();
    const cx = e.clientX - r.left, cy = e.clientY - r.top;
    const step = e.deltaMode === 1 ? e.deltaY * 33 : e.deltaY;
    const s = Math.min(fitScale * 6, Math.max(fitScale * 0.3, tf.s * Math.exp(-step * 0.0025)));
    const k = s / tf.s;
    tf = { s, ox: cx - (cx - tf.ox) * k, oy: cy - (cy - tf.oy) * k };
    schedule(true);
  }, { passive: false });

  const ro = new ResizeObserver(() => { if (!dragging && !panning) { fit(); place(); paint(store.state); } });
  ro.observe(wrap);

  function paintWelcome(state) {
    const first = state.peers.length === 0;
    welcome.style.display = first ? '' : 'none';
    wrap.classList.toggle('first-run', first);
    if (!first || welcome.dataset.built) return;
    welcome.dataset.built = '1';
    welcome.append(
      h('h2', null, 'Connect your first computer'),
      h('ol', null,
        h('li', null, 'Install Glide on the other computer and open it.'),
        h('li', null, 'Make sure both are on the same network.'),
        h('li', null, 'Pick that computer below, or show a code on this one.')),
      h('div', { class: 'actions' },
        h('button', { class: 'btn primary', onclick: () => document.dispatchEvent(new CustomEvent('glide:navigate', { detail: 'devices' })) }, 'Find computers nearby'),
        h('button', { class: 'btn', onclick: openHostModal }, 'Show a pairing code')));
  }

  let devSig = '', laySig = '';
  function update(state, kind) {
    if (dragging) return;
    // "Which computers and screens exist" rebuilds the canvas. "Where they sit" only moves them, keeping the view steady.
    const devs = JSON.stringify([state.self.name, state.self.monitors, state.peers.map((p) => [p.device_id, p.name, p.os, p.monitors])]);
    const lay = JSON.stringify(state.layout);
    if (devs !== devSig) {
      devSig = devs; laySig = lay;
      const firstBuild = list.length === 0;
      list = collectDevices(state);
      build();
      // A rename, a new screen or a hot-plug must not re-center what the user arranged: only fit the first time,
      // or when something would otherwise end up out of sight.
      if (firstBuild || !contained()) fit();
      place();
      if (store.selectedId && !list.some((d) => d.id === store.selectedId)) store.selectedId = null;
    } else if (lay !== laySig) {
      laySig = lay;
      for (const f of collectDevices(state)) { const d = list.find((x) => x.id === f.id); if (d) { d.x = f.x; d.y = f.y; } }
      // Positions changed (often our own drop coming back from the engine): move them, keep the person's view.
      settle();
      place();
    }
    paint(state);
    paintWelcome(state);
    if (kind === 'stats') {
      // Latency ticks must not rebuild the panel (it would eat clicks); refresh only the status chip.
      const d = list.find((x) => x.id === store.selectedId);
      const chip = inspector.querySelector('.chip.status');
      if (d && chip) chip.textContent = statusText(d).text;
    } else {
      renderInspector(state);
    }
  }

  update(store.state);
  return { update, unmount: () => { ro.disconnect(); offActive(); cancelAnimationFrame(frame); clearTimeout(settleTimer); } };
}
