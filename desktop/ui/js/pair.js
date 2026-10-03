import { h, clear, osName } from './dom.js';
import { osIcon } from './icons.js';
import { call, onEvent } from './api.js';
import { openModal, toast } from './ui.js';
import { store } from './store.js';


// ── Verify: both screens must show the same three words ──────────────────────
function showVerify(m, data) {
  m.modal.querySelector('h2').textContent = 'Check it’s really you';
  const lead = m.modal.querySelector('p');
  if (lead) lead.textContent = `Look at ${data.peer.name}. If it shows these same three words, tap “They match” on both computers.`;
  const words = h('div', { class: 'verify-words', 'aria-live': 'polite' }, data.phrase.map((w) => h('span', null, w)));
  const status = h('p', { class: 'center' });
  clear(m.body).append(words, status);
  const no = h('button', { class: 'btn danger', onclick: () => answer(false) }, 'They don’t match');
  const yes = h('button', { class: 'btn primary', onclick: () => answer(true) }, 'They match');
  clear(m.foot).append(no, yes);
  yes.focus();
  async function answer(accepted) {
    yes.disabled = no.disabled = true;
    if (accepted) status.textContent = `Waiting for ${data.peer.name} to confirm…`;
    try { await call('pairing.confirm', { accepted }); } catch (e) { status.textContent = e.message; }
    if (!accepted) m.close();
  }
}

// ── Join: type the 6-digit code shown on the other computer ──────────────────
// The engine answers `pairing.join` right away and then reports progress as events:
// `pairing.verify` (the three words) and finally `pairing.result` (paired, or why not).
export function openJoinModal(target) {
  let off = () => {};
  const m = openModal({
    title: `Pair with ${target.name}`,
    lead: `Open Glide on ${target.name}, choose “Show a pairing code”, then type the 6 digits here.`,
    onClose: () => off(),
  });
  const boxes = Array.from({ length: 6 }, (_, i) => h('input', {
    inputmode: 'numeric', maxlength: '1', autocomplete: 'off', 'aria-label': `Digit ${i + 1}`,
    oninput: (e) => {
      e.target.value = e.target.value.replace(/\D/g, '').slice(-1);
      if (e.target.value && i < 5) boxes[i + 1].focus();
      if (boxes.every((b) => b.value)) submit();
    },
    onkeydown: (e) => {
      if (e.key === 'Backspace' && !e.target.value && i > 0) boxes[i - 1].focus();
      if (e.key === 'ArrowLeft' && i > 0) boxes[i - 1].focus();
      if (e.key === 'ArrowRight' && i < 5) boxes[i + 1].focus();
    },
    onpaste: (e) => {
      const digits = (e.clipboardData.getData('text') || '').replace(/\D/g, '').slice(0, 6);
      if (!digits) return;
      e.preventDefault();
      digits.split('').forEach((d, k) => { boxes[k].value = d; });
      boxes[Math.min(digits.length, 5)].focus();
      if (digits.length === 6) submit();
    },
  }));
  const row = h('div', { class: 'code-boxes' }, boxes);
  const err = h('p', { class: 'err', role: 'alert' });
  const pairBtn = h('button', { class: 'btn primary', onclick: () => submit() }, 'Pair');
  m.body.append(row, err);
  m.foot.append(h('button', { class: 'btn quiet', onclick: () => m.close() }, 'Cancel'), pairBtn);
  boxes[0].focus();

  let busy = false;
  let verifying = false;
  const fail = (message, code) => {
    busy = false;
    if (verifying) {
      clear(m.body).append(h('p', { class: 'err' }, message || 'Pairing did not finish.'));
      clear(m.foot).append(h('button', { class: 'btn', onclick: () => m.close() }, 'Close'));
      return;
    }
    err.textContent = message || 'Pairing failed.';
    row.classList.add('bad');
    if (code === 'bad_code') { boxes.forEach((b) => { b.value = ''; }); boxes[0].focus(); }
    if (code === 'locked_out' || code === 'code_expired') pairBtn.remove();
    else { pairBtn.disabled = false; pairBtn.textContent = 'Pair'; }
  };

  async function submit() {
    const code = boxes.map((b) => b.value).join('');
    if (busy || code.length !== 6) { err.textContent = 'Enter all 6 digits.'; return; }
    busy = true; pairBtn.disabled = true; pairBtn.textContent = 'Pairing…'; err.textContent = ''; row.classList.remove('bad');
    off = onEvent((name, data) => {
      if (name === 'pairing.verify') { verifying = true; showVerify(m, data); }
      else if (name === 'pairing.result') {
        off();
        if (data.ok) { busy = false; m.close(); toast(`${target.name} is paired. Drag it into place on your desk.`, 'info', 'Paired'); }
        else fail(data.error?.message, data.error?.code);
      }
    });
    try {
      // The engine wants exactly ONE target: the device id when known (it knows every address), else the address.
      await call('pairing.join', target.device_id ? { device_id: target.device_id, code } : { address: target.address, code });
    } catch (e) {
      off();
      fail(e.message, e.code);
    }
  }
}

// ── Host: show a code on this computer ───────────────────────────────────────
export async function openHostModal() {
  let off = () => {};
  let timer = null;
  const m = openModal({
    title: 'Pair another computer',
    lead: 'On the other computer, open Glide, pick this computer under Nearby, and type this code.',
    onClose: () => { off(); clearInterval(timer); call('pairing.cancel_host').catch(() => {}); },
  });
  const codeEl = h('div', { class: 'code-display', 'aria-live': 'polite' }, '······');
  const status = h('div', { class: 'pulse-row' }, h('i', { class: 'dot cursor' }), 'Waiting for the other computer…');
  m.body.append(codeEl, status);
  m.foot.append(h('button', { class: 'btn quiet', onclick: () => m.close() }, 'Cancel'));

  try {
    const { code, expires_at_ms: exp } = await call('pairing.start_host');
    codeEl.textContent = code;
    const tick = () => {
      const left = Math.max(0, Math.round((exp - Date.now()) / 1000));
      if (left === 0) { clearInterval(timer); codeEl.textContent = '——————'; clear(status).append('This code expired. Close this and try again.'); return; }
      if (!status.dataset.busy) { clear(status).append(h('i', { class: 'dot cursor' }), `Waiting for the other computer… code expires in ${left} s`); }
    };
    timer = setInterval(tick, 1000); tick();
  } catch (e) {
    clear(status).append(e.message || 'Could not start pairing.');
    return;
  }

  off = onEvent((name, data) => {
    if (name === 'pairing.incoming') {
      status.dataset.busy = '1';
      clear(status).append(osIcon(data.os), `${data.name} is pairing…`);
    } else if (name === 'pairing.verify') {
      clearInterval(timer);
      showVerify(m, data);
    } else if (name === 'pairing.result') {
      if (data.ok) { m.close(); toast('The new computer is paired. Drag it into place on your desk.', 'info', 'Paired'); }
      else { delete status.dataset.busy; clear(status).append(`Pairing did not complete: ${data.error?.message ?? 'wrong code'}.`); }
    }
  });
}

// ── Manual: add by address ────────────────────────────────────────────────────
export function openManualModal() {
  const m = openModal({
    title: 'Add by address',
    lead: 'Use this when a computer does not show up under Nearby, for example on another subnet or VPN.',
  });
  const input = h('input', { class: 'input', placeholder: '192.168.1.20:24800', 'aria-label': 'Address', spellcheck: 'false' });
  const err = h('p', { class: 'err', role: 'alert' });
  const go = h('button', { class: 'btn primary', onclick: () => add() }, 'Find computer');
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') add(); });
  m.body.append(h('div', { class: 'field' }, h('label', null, 'Address and port'), input), err);
  m.foot.append(h('button', { class: 'btn quiet', onclick: () => m.close() }, 'Cancel'), go);
  input.focus();
  async function add() {
    go.disabled = true; err.textContent = '';
    const address = input.value.trim();
    try {
      await call('peer.add_manual', { address });
      // The engine adds the computer to its Nearby list; find it there.
      const st = await call('get_state');
      const host = address.replace(/:\d+$/, '');
      const found = st.discovered.find((d) => d.address === address) ?? st.discovered.find((d) => d.address.startsWith(`${host}:`));
      if (!found) {
        const paired = st.peers.find((p) => p.address?.startsWith(`${host}:`));
        throw { message: paired ? `${paired.name} is already paired.` : 'Glide did not answer at that address. Check that it is open there and the address is right.' };
      }
      m.close();
      openJoinModal(found);
    } catch (e) { err.textContent = e.message; go.disabled = false; }
  }
}

export const pairable = () => store.state?.discovered ?? [];
export { osName };
