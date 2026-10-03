// Self-updating (Tauri app only). The app checks in the background and announces "update.available"; installing
// downloads the signed update, replaces Glide and restarts it.
const listeners = new Set();
export const updates = {
  supported: typeof window.glide?.checkForUpdates === 'function',
  available: null,      // { version } once an update is waiting
  checking: false,
  installing: false,
  progress: null,       // 0..1 while downloading
  message: '',          // last result of a manual check
};
const changed = () => listeners.forEach((fn) => fn());
export const onUpdateChange = (fn) => { listeners.add(fn); return () => listeners.delete(fn); };

export function handleUpdateEvent(name, data) {
  if (name === 'update.available') {
    updates.available = { version: String(data?.version ?? '') };
  } else if (name === 'update.progress') {
    const total = Number(data?.total) || 0;
    updates.progress = total > 0 ? Math.min(1, Number(data?.received) / total) : null;
  } else return false;
  changed();
  return true;
}

export async function checkNow() {
  if (!updates.supported || updates.checking) return;
  updates.checking = true; updates.message = ''; changed();
  try {
    const r = await window.glide.checkForUpdates();
    if (r?.available) { updates.available = { version: String(r.version) }; updates.message = ''; }
    else updates.message = 'You have the latest version.';
  } catch (e) {
    updates.message = typeof e === 'string' ? e : 'Could not check for updates.';
  } finally {
    updates.checking = false; changed();
  }
}

export async function installNow() {
  if (!updates.supported || updates.installing || !updates.available) return;
  updates.installing = true; updates.progress = 0; changed();
  try {
    await window.glide.installUpdate(); // Glide restarts when this succeeds
  } catch (e) {
    updates.installing = false; updates.progress = null;
    updates.message = typeof e === 'string' ? e : 'The update could not be installed.';
    changed();
  }
}
