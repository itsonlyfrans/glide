// The narrow window.glide surface the screens use, connected to Glide's Tauri commands. Loaded before the screens.
// In a plain browser (no Tauri) it does nothing, so a test page can supply its own window.glide.
(() => {
  const T = window.__TAURI__;
  if (window.glide || !T) return;
  const { invoke } = T.core;
  const appWindow = T.window.getCurrentWindow();
  const isWin = navigator.userAgent.includes('Windows');

  window.glide = {
    call: (method, params) => invoke('glide_call', { method: String(method), params: params ?? {} }),
    meta: () => invoke('glide_meta'),
    openDisplaySettings: () => invoke('glide_open_display_settings'),
    resetPermissions: (kinds) => invoke('glide_reset_permissions', { kinds: Array.isArray(kinds) ? kinds.map(String) : [] }),
    openLogs: () => invoke('glide_open_logs'),
    quitEngine: () => invoke('glide_quit_engine'),
    startEngine: () => invoke('glide_start_engine'),
    relaunch: () => invoke('glide_relaunch'),
    openExternal: (url) => invoke('glide_open_external', { url: String(url) }),
    checkForUpdates: () => invoke('glide_check_update'),
    installUpdate: () => invoke('glide_install_update'),
    getPrefs: () => invoke('glide_get_prefs'),
    setPrefs: (prefs) => invoke('glide_set_prefs', { prefs }),
    onEvent: (handler) => {
      let stop = null;
      let stopped = false;
      T.event.listen('glide:event', (e) => handler(e.payload?.name, e.payload?.data)).then((un) => {
        if (stopped) un(); else stop = un;
      });
      return () => { stopped = true; stop?.(); };
    },
  };

  // Drag the window by its empty top areas (the .drag regions), and double-click them to maximize, like a title bar.
  const interactive = '.no-drag, button, input, select, textarea, a, [role="button"]';
  document.addEventListener('mousedown', (e) => {
    if (e.button !== 0 || !e.target.closest?.('.drag') || e.target.closest(interactive)) return;
    if (e.detail === 2) appWindow.toggleMaximize();
    else appWindow.startDragging();
  });

  // Windows: the page draws the minimize / maximize / close buttons the system title bar used to provide.
  if (isWin) {
    const svgNs = 'http://www.w3.org/2000/svg';
    const glyph = (d) => {
      const svg = document.createElementNS(svgNs, 'svg');
      svg.setAttribute('viewBox', '0 0 10 10');
      svg.setAttribute('aria-hidden', 'true');
      const path = document.createElementNS(svgNs, 'path');
      path.setAttribute('d', d);
      svg.append(path);
      return svg;
    };
    const button = (label, d, onClick, extra = '') => {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = `caption-btn ${extra}`.trim();
      b.setAttribute('aria-label', label);
      b.title = label;
      b.append(glyph(d));
      b.addEventListener('click', onClick);
      return b;
    };
    const bar = document.createElement('div');
    bar.className = 'caption-buttons';
    bar.append(
      button('Minimize', 'M0 5h10', () => appWindow.minimize()),
      button('Maximize', 'M0.5 0.5h9v9h-9z', () => appWindow.toggleMaximize()),
      button('Close', 'M0 0l10 10M10 0L0 10', () => appWindow.close(), 'close'),
    );
    const mount = () => document.body.append(bar);
    if (document.body) mount(); else document.addEventListener('DOMContentLoaded', mount, { once: true });
  }
})();
