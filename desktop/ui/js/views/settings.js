import { h, clear, append } from '../dom.js';
import { icon } from '../icons.js';
import { call } from '../api.js';
import { store } from '../store.js';
import { switchRow, toast, confirmModal } from '../ui.js';
import { updates, onUpdateChange, checkNow, installNow } from '../updates.js';

// The Updates section re-draws itself while a check or download is running.
let updatesBox = null;
let shownVersion = '';
onUpdateChange(() => renderUpdates(shownVersion));
function renderUpdates(version) {
  shownVersion = version;
  if (!updatesBox) return;
  const status = updates.installing
    ? `Downloading the update… ${updates.progress == null ? '' : `${Math.round(updates.progress * 100)}%`}`
    : updates.available ? `Glide ${updates.available.version} is ready to install.` : updates.message;
  clear(updatesBox).append(
    setting(`Glide ${version}`, status || 'Glide checks for updates by itself every few hours.',
      updates.available && !updates.installing
        ? h('button', { class: 'btn primary', onclick: () => installNow() }, 'Update now')
        : h('button', { class: 'btn', disabled: updates.checking || updates.installing, onclick: () => checkNow() }, updates.checking ? 'Checking…' : 'Check for updates')));
}

const patch = async (p) => { try { await call('set_settings', { patch: p }); } catch (e) { toast(e.message, 'error'); } };

// Turn a keydown into "Ctrl+Alt+Shift+Home" style text.
function comboFromEvent(e) {
  if (['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)) return null;
  const mods = [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift', e.metaKey && 'Meta'].filter(Boolean);
  if (!mods.length) return null;
  const k = e.code.startsWith('Key') ? e.code.slice(3) : e.code.startsWith('Digit') ? e.code.slice(5) : e.code;
  return [...mods, k].join('+');
}

function hotkeyButton(value, onSet) {
  const render = (v) => v.split('+').flatMap((k, i) => (i ? ['+', h('kbd', null, k)] : [h('kbd', null, k)]));
  const btn = h('button', { class: 'hotkey', 'aria-label': `Shortcut ${value}. Click to change.` }, render(value));
  btn.addEventListener('click', () => {
    btn.classList.add('capturing');
    clear(btn).append('Press the new shortcut…');
    const stop = (restore) => { document.removeEventListener('keydown', onKey, true); btn.classList.remove('capturing'); clear(btn).append(...render(restore)); };
    const onKey = (e) => {
      e.preventDefault(); e.stopPropagation();
      if (e.key === 'Escape') return stop(value);
      const combo = comboFromEvent(e);
      if (!combo) return;
      stop(combo); onSet(combo);
    };
    document.addEventListener('keydown', onKey, true);
    btn.addEventListener('blur', () => stop(value), { once: true });
  });
  return btn;
}

function segmented(options, current, onPick) {
  const seg = h('div', { class: 'seg', role: 'group' });
  for (const [val, label] of options) {
    seg.append(h('button', { 'aria-pressed': String(val === current), onclick: () => { for (const b of seg.children) b.setAttribute('aria-pressed', 'false'); seg.children[options.findIndex((o) => o[0] === val)].setAttribute('aria-pressed', 'true'); onPick(val); } }, label));
  }
  return seg;
}

// macOS: show Glide in the Dock, the menu bar, both or neither. Kept by the window, applied at once.
const PRESENCE = [['both', 'Dock and menu bar'], ['menubar', 'Menu bar'], ['dock', 'Dock'], ['hidden', 'Neither']];
let presence = null;
function presenceRow() {
  const box = h('div');
  const help = (v) => (v === 'hidden'
    ? 'Glide keeps running quietly. To see this window again, open Glide from Applications or Spotlight.'
    : v === 'dock' ? 'Closing the window keeps Glide running; click it in the Dock to come back.'
      : 'Closing the window keeps Glide running; use the Glide icon in the menu bar to come back.');
  const draw = () => {
    const desc = h('div', { class: 'desc' }, help(presence ?? 'both'));
    clear(box).append(setting('Show Glide in', null, segmented(PRESENCE, presence ?? 'both', async (v) => {
      try { await window.glide.setPrefs({ mac_presence: v }); presence = v; desc.textContent = help(v); } catch (e) { toast(String(e), 'error'); }
    })));
    box.firstChild.querySelector('.grow').append(desc);
  };
  draw();
  if (presence == null) window.glide.getPrefs().then((p) => { presence = p?.mac_presence ?? 'both'; draw(); }).catch(() => {});
  return box;
}

// Cursor report: where time goes between the mouse and the cursor on the other computer.
async function openCursorReport() {
  const pre = h('pre', { class: 'report' }, 'Measuring…');
  const refresh = async () => {
    try { pre.textContent = (await call('diag.cursor')).text; } catch (e) { pre.textContent = e.message ?? String(e); }
  };
  const close = () => scrim.remove();
  const scrim = h('div', { class: 'scrim', onclick: (e) => { if (e.target === scrim) close(); } },
    h('div', { class: 'modal wide', role: 'dialog', 'aria-label': 'Cursor report' },
      h('h2', null, 'Cursor report'),
      h('p', null, 'Move the cursor onto the other computer and keep it moving for about 10 seconds, then press Refresh. Do the same in Glide on the other computer, and send both reports.'),
      pre,
      h('div', { class: 'foot' },
        h('button', { class: 'btn', onclick: refresh }, 'Refresh'),
        h('button', { class: 'btn', onclick: async () => { try { await navigator.clipboard.writeText(pre.textContent); toast('Report copied'); } catch { toast('Could not copy.', 'error'); } } }, icon('copy'), 'Copy'),
        h('button', { class: 'btn primary', onclick: close }, 'Done'))));
  document.getElementById('modalRoot').append(scrim);
  refresh();
}

const setting = (title, desc, control) => h('div', { class: 'setting' }, h('div', { class: 'grow' }, h('div', { class: 'title' }, title), desc ? h('div', { class: 'desc' }, desc) : null), control);

export function mountSettings(root) {
  const col = h('div', { class: 'col' });
  root.append(h('div', { class: 'scroll' }, col));
  let sig = '';

  function build(s, state) {
    append(clear(col), [
      h('section', { class: 'section' },
        h('h2', null, 'This computer'),
        h('div', { class: 'rows' },
          setting('Name', 'How other computers see this one.', (() => {
            const i = h('input', { class: 'input', value: s.device_name, maxlength: '40', 'aria-label': 'Computer name' });
            i.addEventListener('change', () => { const v = i.value.trim(); if (v) patch({ device_name: v }); else i.value = s.device_name; });
            return i;
          })()),
          switchRow({ title: 'Open Glide when I sign in', desc: 'Starts quietly in the background so sharing is ready.', checked: s.startup.launch_at_login, onChange: (v) => patch({ startup: { launch_at_login: v } }) }),
          switchRow({ title: 'Start minimized', desc: document.body.classList.contains('plat-mac') ? 'Skip the window and start quietly in the background.' : 'Skip the window and go straight to the tray.', checked: s.startup.start_minimized, onChange: (v) => patch({ startup: { start_minimized: v } }) }),
          document.body.classList.contains('plat-mac') && window.glide?.getPrefs ? presenceRow() : null)),

      h('section', { class: 'section' },
        h('h2', null, 'Moving between screens'),
        h('div', { class: 'rows' },
          setting('Edge delay', 'Wait this long at a screen edge before crossing. Stops accidental switches.', (() => {
            const out = h('span', { class: 'desc', style: { width: '54px', textAlign: 'right' } }, `${s.switching.edge_delay_ms} ms`);
            const r = h('input', { type: 'range', min: '0', max: '500', step: '10', value: String(s.switching.edge_delay_ms), 'aria-label': 'Edge delay' });
            r.addEventListener('input', () => { out.textContent = `${r.value} ms`; });
            r.addEventListener('change', () => patch({ switching: { edge_delay_ms: +r.value } }));
            return h('div', { style: { display: 'flex', alignItems: 'center', gap: '10px' } }, r, out);
          })()),
          setting('Pointer speed on other computers', 'How fast the mouse moves when you are controlling another computer. Raise it if the other computer feels slow.', (() => {
            const speed = s.switching.pointer_speed ?? 1;
            const out = h('span', { class: 'desc', style: { width: '54px', textAlign: 'right' } }, `${speed.toFixed(1)}×`);
            const r = h('input', { type: 'range', min: '50', max: '300', step: '10', value: String(Math.round(speed * 100)), 'aria-label': 'Pointer speed on other computers' });
            r.addEventListener('input', () => { out.textContent = `${(r.value / 100).toFixed(1)}×`; });
            r.addEventListener('change', () => patch({ switching: { pointer_speed: r.value / 100 } }));
            return h('div', { style: { display: 'flex', alignItems: 'center', gap: '10px' } }, r, out);
          })()),
          state.self.os === 'windows' ? setting('Pointer acceleration', 'Makes quick mouse movement travel further than slow movement, like a Mac trackpad. Raise it if the mouse feels sluggish on another computer.', (() => {
            const accel = s.switching.pointer_acceleration ?? 0;
            const label = (v) => (v <= 0 ? 'Off' : `${v.toFixed(1)}×`);
            const out = h('span', { class: 'desc', style: { width: '54px', textAlign: 'right' } }, label(accel));
            const r = h('input', { type: 'range', min: '0', max: '40', step: '1', value: String(Math.round(accel * 10)), 'aria-label': 'Pointer acceleration' });
            r.addEventListener('input', () => { out.textContent = label(r.value / 10); });
            r.addEventListener('change', () => patch({ switching: { pointer_acceleration: r.value / 10 } }));
            return h('div', { style: { display: 'flex', alignItems: 'center', gap: '10px' } }, r, out);
          })()) : null,
          state.self.os === 'macos' ? switchRow({ title: 'Smooth the cursor over Wi-Fi', desc: 'When the cursor arrives from another computer in bursts, spread them over the next frames instead of jumping. Adds at most 8 ms. Turn it off if the cursor feels delayed.', checked: s.switching.smooth_moves !== false, onChange: (v) => patch({ switching: { smooth_moves: v } }) }) : null,
          switchRow({ title: 'Double-tap the edge to cross', desc: 'Cross only when you push against the edge twice.', checked: s.switching.double_tap, onChange: (v) => patch({ switching: { double_tap: v } }) }),
          setting('Return to this computer', 'Works from anywhere, even if a connection drops.', hotkeyButton(s.hotkeys.return_home, (v) => patch({ hotkeys: { return_home: v } }))),
          setting('Turn sharing on or off', null, hotkeyButton(s.hotkeys.toggle_sharing, (v) => patch({ hotkeys: { toggle_sharing: v } }))))),

      h('section', { class: 'section' },
        h('h2', null, 'Keyboard'),
        h('div', { class: 'rows' },
          setting('Swap Ctrl and Cmd between Windows and Mac', 'So Ctrl+C on a Windows keyboard copies on a Mac, and Cmd+C on a Mac keyboard copies on Windows.',
            segmented([['auto', 'Automatic'], ['always', 'Always'], ['never', 'Never']], s.keyboard.swap_ctrl_cmd, (v) => patch({ keyboard: { swap_ctrl_cmd: v } }))))),

      h('section', { class: 'section' },
        h('h2', null, 'Clipboard and files'),
        h('div', { class: 'rows' },
          switchRow({ title: 'Share the clipboard', desc: 'Copy on one computer, paste on another.', checked: s.clipboard.enabled, onChange: (v) => patch({ clipboard: { enabled: v } }) }),
          switchRow({ title: 'Text', checked: s.clipboard.sync_text, onChange: (v) => patch({ clipboard: { sync_text: v } }) }),
          switchRow({ title: 'Images', checked: s.clipboard.sync_images, onChange: (v) => patch({ clipboard: { sync_images: v } }) }),
          switchRow({ title: 'Files and videos', desc: 'Large files copy in the background and are ready to paste when they arrive.', checked: s.clipboard.sync_files, onChange: (v) => patch({ clipboard: { sync_files: v } }) }),
          switchRow({ title: 'Never share passwords', desc: 'Skips anything a password manager marks as sensitive.', checked: s.clipboard.exclude_sensitive, onChange: (v) => patch({ clipboard: { exclude_sensitive: v } }) }),
          setting('Ask before copying more than', 'Bigger files wait for your OK on the Files tab.', (() => {
            const i = h('input', { class: 'input', type: 'number', min: '1', value: String(s.clipboard.max_auto_mb), style: { width: '110px' }, 'aria-label': 'Megabytes' });
            i.addEventListener('change', () => patch({ clipboard: { max_auto_mb: Math.max(1, Math.round(+i.value) || 2048) } }));
            return h('div', { style: { display: 'flex', alignItems: 'center', gap: '8px' } }, i, h('span', { class: 'desc' }, 'MB'));
          })()))),

      h('section', { class: 'section' },
        h('h2', null, 'Network'),
        h('div', { class: 'rows' },
          switchRow({ title: 'Find nearby computers', desc: 'Announces only this computer’s name and type on your local network.', checked: s.network.discovery, onChange: (v) => patch({ network: { discovery: v } }) }),
          setting('Port', 'Change only if another app uses it. Restart Glide afterwards.', (() => {
            const i = h('input', { class: 'input', type: 'number', min: '1024', max: '65535', value: String(s.network.port), style: { width: '110px' }, 'aria-label': 'Port' });
            i.addEventListener('change', () => { const v = Math.round(+i.value); if (v >= 1024 && v <= 65535) patch({ network: { port: v } }); else { i.value = s.network.port; toast('Choose a port from 1024 to 65535.', 'error'); } });
            return i;
          })()))),

      h('section', { class: 'section' },
        h('h2', null, 'Security'),
        h('p', null, 'Everything between your computers is encrypted, and each computer only talks to the ones you paired. To check you are really paired with the right machine, compare this fingerprint with the one shown on the other computer.'),
        h('div', { class: 'rows' },
          h('div', { class: 'setting' },
            h('div', { class: 'grow' }, h('div', { class: 'title' }, 'Fingerprint of this computer'), h('div', { class: 'plain-fp' }, state.self.fingerprint)),
            h('button', { class: 'btn sm', onclick: async () => { try { await navigator.clipboard.writeText(state.self.fingerprint); toast('Fingerprint copied'); } catch { toast('Could not copy.', 'error'); } } }, icon('copy'), 'Copy')))),

      document.body.classList.contains('attached') ? h('section', { class: 'section' },
        h('h2', null, 'Running in the background'),
        h('p', null, 'Glide keeps sharing your mouse, keyboard and clipboard after you close this window, using very little memory. Use the Glide icon in the taskbar tray to open it again or to quit.'),
        h('button', { class: 'btn danger', onclick: async () => {
          const yes = await confirmModal({ title: 'Quit Glide completely?', body: 'Sharing stops on this computer until you open Glide again.', confirm: 'Quit Glide', danger: true });
          if (yes) window.glide.quitEngine();
        } }, 'Quit Glide completely')) : null,

      updates.supported ? h('section', { class: 'section' },
        h('h2', null, 'Updates'),
        (updatesBox = h('div', { class: 'rows' }))) : null,

      h('section', { class: 'section' },
        h('h2', null, 'Troubleshooting'),
        h('p', null, 'Glide keeps short activity logs (never your keystrokes, clipboard or files) and deletes them after 7 days. Send the newest one if something goes wrong.'),
        h('div', { class: 'actions' },
          h('button', { class: 'btn', onclick: () => window.glide.openLogs() }, 'Open logs folder'),
          h('button', { class: 'btn', onclick: () => openCursorReport() }, 'Cursor report'))),

      updates.supported ? null : h('p', { class: 'desc', style: { marginTop: '8px' } }, `Glide ${state.self.version}`),
    ]);
    renderUpdates(state.self.version);
  }

  function update(state) {
    const next = JSON.stringify([state.settings, state.self.name, state.self.fingerprint]);
    if (next === sig) return;
    if (col.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return; // don't yank a field mid-edit
    sig = next;
    build(state.settings, state);
  }
  update(store.state);
  return { update };
}
