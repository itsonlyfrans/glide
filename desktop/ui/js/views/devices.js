import { h, clear, osName } from '../dom.js';
import { icon, osIcon } from '../icons.js';
import { store } from '../store.js';
import { openJoinModal, openHostModal, openManualModal } from '../pair.js';

function osMark(os) { return h('div', { class: `osmark ${os === 'macos' ? 'mac' : 'win'}` }, osIcon(os)); }

function pairedRow(p) {
  const online = p.connection === 'connected';
  return h('div', { class: 'row' },
    osMark(p.os),
    h('div', { class: 'grow' },
      h('div', { class: 'title' }, p.name),
      h('div', { class: 'desc' }, `${osName(p.os)} · ${online ? (p.latency_ms != null ? `Connected, ${p.latency_ms.toFixed(1)} ms` : 'Connected') : p.connection === 'connecting' ? 'Connecting…' : 'Offline'}`)),
    h('i', { class: `dot ${online ? 'ok' : 'off'}`, title: online ? 'Online' : 'Offline' }),
    h('button', { class: 'btn sm quiet', onclick: () => { store.selectedId = p.device_id; store.view = 'desk'; document.dispatchEvent(new CustomEvent('glide:navigate', { detail: 'desk' })); } }, 'Show on desk'));
}

function nearbyRow(d) {
  return h('div', { class: 'row' },
    osMark(d.os),
    h('div', { class: 'grow' }, h('div', { class: 'title' }, d.name), h('div', { class: 'desc' }, `${osName(d.os)} · ${d.address}`)),
    h('button', { class: 'btn sm', onclick: () => openJoinModal(d) }, icon('key'), 'Pair'));
}

export function mountDevices(root) {
  const col = h('div', { class: 'col' });
  root.append(h('div', { class: 'scroll' }, col));

  function update(state) {
    clear(col).append(
      h('div', { class: 'actions' },
        h('button', { class: 'btn primary', onclick: openHostModal }, icon('plus'), 'Show a pairing code'),
        h('button', { class: 'btn', onclick: openManualModal }, 'Add by address')),
      h('section', { class: 'section' },
        h('h2', null, 'Paired computers'),
        h('p', null, 'These computers are trusted. Only they can send or receive your keyboard, mouse and clipboard.'),
        state.peers.length
          ? h('div', { class: 'rows' }, state.peers.map(pairedRow))
          : h('div', { class: 'rows' }, h('div', { class: 'empty' }, h('strong', null, 'No computers paired yet'), 'Open Glide on another computer on this network, then pair it from the Nearby list below.'))),
      h('section', { class: 'section' },
        h('h2', null, 'Nearby'),
        h('p', null, state.settings.network.discovery
          ? 'Computers running Glide on your network that you have not paired yet.'
          : 'Discovery is off, so nearby computers will not appear. You can turn it on in Settings, or add one by address.'),
        state.discovered.length
          ? h('div', { class: 'rows' }, state.discovered.map(nearbyRow))
          : h('div', { class: 'rows' }, h('div', { class: 'empty' }, h('strong', null, 'Nothing nearby'), 'Make sure Glide is open on the other computer and both are on the same network.'))),
    );
  }
  update(store.state);
  return { update };
}
