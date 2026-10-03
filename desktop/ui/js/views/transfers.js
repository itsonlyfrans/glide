import { h, clear, formatBytes, formatRate } from '../dom.js';
import { call } from '../api.js';
import { store, nameOf } from '../store.js';
import { toast } from '../ui.js';

const STATE_TEXT = { queued: 'Waiting', awaiting_confirm: 'Needs your OK', active: 'Copying', done: 'Ready to paste', failed: 'Failed', cancelled: 'Cancelled' };

export function mountTransfers(root) {
  const col = h('div', { class: 'col' });
  root.append(h('div', { class: 'scroll' }, col));
  const bars = new Map();   // id → { bar, meta } so progress ticks don't rebuild the list
  let sig = '';

  const act = async (method, params) => { try { await call(method, params); } catch (e) { toast(e.message, 'error'); } };

  function row(t) {
    const pct = t.bytes_total ? Math.min(100, (t.bytes_done / t.bytes_total) * 100) : 0;
    const bar = h('i', { style: { width: `${pct}%` } });
    const meta = h('div', { class: 'desc' });
    bars.set(t.id, { bar, meta, t });
    const el = h('div', { class: 'row' },
      h('div', { class: 'grow' },
        h('div', { class: 'title' }, t.name, t.items > 1 ? ` and ${t.items - 1} more` : ''),
        meta,
        t.state === 'active' || t.state === 'done' ? h('div', { class: `progress ${t.state === 'done' ? 'done' : ''}` }, bar) : null,
        t.error ? h('div', { class: 'desc', style: { color: 'var(--danger)' } }, t.error) : null),
      t.state === 'awaiting_confirm'
        ? [h('button', { class: 'btn sm quiet', onclick: () => act('transfer.confirm', { id: t.id, accept: false }) }, 'Decline'),
           h('button', { class: 'btn sm primary', onclick: () => act('transfer.confirm', { id: t.id, accept: true }) }, 'Accept')]
        : (t.state === 'active' || t.state === 'queued') ? h('button', { class: 'btn sm quiet', onclick: () => act('transfer.cancel', { id: t.id }) }, 'Cancel') : null);
    paintMeta(t);
    return el;
  }

  function paintMeta(t) {
    const rec = bars.get(t.id);
    if (!rec) return;
    const who = t.direction === 'send' ? `to ${nameOf(t.peer_id)}` : `from ${nameOf(t.peer_id)}`;
    const size = `${formatBytes(t.bytes_done)} of ${formatBytes(t.bytes_total)}`;
    rec.meta.textContent = t.state === 'active'
      ? `${who} · ${size} · ${formatRate(t.rate_bps)}`
      : `${who} · ${t.state === 'done' ? formatBytes(t.bytes_total) + ' · ' : ''}${STATE_TEXT[t.state] ?? t.state}`;
    rec.bar.style.width = `${t.bytes_total ? Math.min(100, (t.bytes_done / t.bytes_total) * 100) : 0}%`;
  }

  function update(state, kind) {
    if (kind === 'progress') { for (const t of state.transfers) { const r = bars.get(t.id); if (r) { r.t = t; paintMeta(t); } } return; }
    const next = JSON.stringify(state.transfers.map((t) => [t.id, t.state, t.name, t.items, t.error]));
    if (next === sig) { for (const t of state.transfers) paintMeta(t); return; }
    sig = next; bars.clear();
    clear(col).append(h('section', { class: 'section' },
      h('h2', null, 'Files and clipboard'),
      h('p', null, 'Files you copy on one computer travel to the others in the background. When one finishes you can paste it anywhere.'),
      state.transfers.length
        ? h('div', { class: 'rows' }, state.transfers.map(row))
        : h('div', { class: 'rows' }, h('div', { class: 'empty' }, h('strong', null, 'Nothing is moving'), 'Copy a file on one paired computer and its progress will show up here.'))));
  }
  update(store.state);
  return { update };
}
