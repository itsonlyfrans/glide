// Single source of truth for UI. The daemon owns real state; we mirror its snapshots.
const listeners = new Set();
const activeListeners = new Set();

export const store = {
  state: null,           // last State snapshot from the engine
  engine: 'starting',    // starting | up | down | fatal
  engineMessage: '',
  view: 'desk',
  selectedId: null,
  pairHostOpen: false,
  lastActive: null,      // device id that held the cursor before the current one
};

export function subscribe(fn) { listeners.add(fn); return () => listeners.delete(fn); }
export function onActiveChanged(fn) { activeListeners.add(fn); return () => activeListeners.delete(fn); }
export function notify(kind = 'state') { for (const fn of listeners) fn(kind); }

export function applyEvent(name, data) {
  switch (name) {
    case 'ready': store.engine = 'up'; break;
    case 'state': {
      const prevActive = store.state?.active_device_id;
      store.state = data;
      store.engine = 'up';
      if (prevActive && prevActive !== data.active_device_id) store.lastActive = prevActive;
      break;
    }
    case 'peer.stats': {
      const p = store.state?.peers.find((x) => x.device_id === data.device_id);
      if (p) { p.latency_ms = data.latency_ms; p.rx_bps = data.rx_bps; p.tx_bps = data.tx_bps; }
      return notify('stats');
    }
    case 'transfer.progress': {
      const t = store.state?.transfers.find((x) => x.id === data.id);
      if (t) { t.bytes_done = data.bytes_done; t.rate_bps = data.rate_bps; }
      return notify('progress');
    }
    case 'active_changed':
      for (const fn of activeListeners) fn(store.lastActive ?? store.state?.self.device_id, data.device_id, data.reason);
      return undefined;
    case 'engine.down': store.engine = 'down'; break;
    case 'engine.fatal': store.engine = 'fatal'; store.engineMessage = data.message; break;
    default: return undefined;
  }
  return notify('state');
}

export const selfId = () => store.state?.self.device_id;
export const peerById = (id) => store.state?.peers.find((p) => p.device_id === id);
export const nameOf = (id) => (id === selfId() ? store.state.self.name : peerById(id)?.name ?? 'Unknown device');
