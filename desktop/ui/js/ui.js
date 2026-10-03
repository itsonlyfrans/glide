import { h, clear } from './dom.js';

const activeToasts = new Map();   // "kind|title|message" -> { el, count, timer }
const MAX_TOASTS = 4;

// Identical messages collapse into one toast with a count; the stack never grows past MAX_TOASTS.
export function toast(message, kind = 'info', title) {
  const root = document.getElementById('toasts');
  const key = `${kind}|${title ?? ''}|${message}`;
  const ttl = kind === 'error' ? 6500 : 3800;
  const existing = activeToasts.get(key);
  if (existing) {
    existing.count += 1;
    existing.badge.textContent = `×${existing.count}`;
    clearTimeout(existing.timer);
    existing.timer = setTimeout(() => dismiss(key), ttl);
    return;
  }
  const badge = h('span', { class: 'toast-count' });
  const el = h('div', { class: `toast ${kind}` }, title ? h('b', null, title, badge) : null, message, title ? null : badge);
  root.append(el);
  activeToasts.set(key, { el, count: 1, badge, timer: setTimeout(() => dismiss(key), ttl) });
  while (activeToasts.size > MAX_TOASTS) dismiss(activeToasts.keys().next().value);
}

function dismiss(key) {
  const t = activeToasts.get(key);
  if (!t) return;
  clearTimeout(t.timer);
  t.el.remove();
  activeToasts.delete(key);
}

// Opens a modal; returns { close, body, foot }. Escape and scrim click close it.
export function openModal({ title, lead, onClose }) {
  const root = document.getElementById('modalRoot');
  const prevFocus = document.activeElement;
  const body = h('div', { class: 'body' });
  const foot = h('div', { class: 'foot' });
  const modal = h('div', { class: 'modal', role: 'dialog', 'aria-modal': 'true', 'aria-label': title },
    h('h2', null, title), lead ? h('p', null, lead) : null, body, foot);
  const scrim = h('div', { class: 'scrim', onmousedown: (e) => { if (e.target === scrim) close(); } }, modal);
  const onKey = (e) => { if (e.key === 'Escape') close(); };
  function close() {
    document.removeEventListener('keydown', onKey);
    scrim.remove();
    prevFocus?.focus?.();
    onClose?.();
  }
  document.addEventListener('keydown', onKey);
  root.append(scrim);
  return { close, body, foot, modal };
}

export function confirmModal({ title, body, confirm = 'Confirm', cancel = 'Cancel', danger = false }) {
  return new Promise((resolve) => {
    let answered = false;
    const done = (v) => { if (!answered) { answered = true; resolve(v); } };
    const m = openModal({ title, lead: body, onClose: () => done(false) });
    const ok = h('button', { class: `btn ${danger ? 'danger' : 'primary'}`, onclick: () => { done(true); m.close(); } }, confirm);
    m.foot.append(h('button', { class: 'btn quiet', onclick: () => m.close() }, cancel), ok);
    ok.focus();
  });
}

export function switchRow({ title, desc, checked, onChange, disabled }) {
  const cb = h('input', { type: 'checkbox', disabled, onchange: (e) => onChange(e.target.checked) });
  cb.checked = !!checked;
  return h('div', { class: 'setting' },
    h('div', { class: 'grow' }, h('div', { class: 'title' }, title), desc ? h('div', { class: 'desc' }, desc) : null),
    h('label', { class: 'switch' }, cb, h('span', { class: 'track' }), h('span', { class: 'sr-only' }, title)));
}

export { clear };
