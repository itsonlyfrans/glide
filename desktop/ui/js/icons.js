import { svg } from './dom.js';

const S = 'fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"';
const PATHS = {
  desk: `<svg viewBox="0 0 24 24" ${S}><rect x="2.5" y="5" width="10" height="8" rx="1.8"/><rect x="11.5" y="9" width="10" height="8" rx="1.8"/><path d="M7.5 17.5h0M16.5 21h0"/></svg>`,
  devices: `<svg viewBox="0 0 24 24" ${S}><rect x="3" y="4.5" width="18" height="11.5" rx="2"/><path d="M2 20h20M9 16.5v3.5M15 16.5v3.5"/></svg>`,
  transfers: `<svg viewBox="0 0 24 24" ${S}><path d="M7 4v14M7 18l-3.5-3.5M7 18l3.5-3.5M17 20V6M17 6l-3.5 3.5M17 6l3.5 3.5"/></svg>`,
  settings: `<svg viewBox="0 0 24 24" ${S}><path d="M4 7h9M17 7h3M4 17h3M11 17h9"/><circle cx="15" cy="7" r="2"/><circle cx="9" cy="17" r="2"/></svg>`,
  windows: `<svg viewBox="0 0 24 24" fill="currentColor"><path d="M3 5.4 10.5 4.4v6.7H3zM11.6 4.2 21 3v8.1h-9.4zM3 12.2h7.5v6.8L3 18zM11.6 12.2H21V21l-9.4-1.3z"/></svg>`,
  macos: `<svg viewBox="0 0 24 24" ${S}><path d="M9 9V6.5a2.5 2.5 0 1 0-2.5 2.5H9m0 0h6m-6 0v6m6-6V6.5A2.5 2.5 0 1 1 17.5 9H15m0 0v6m0 0h2.5a2.5 2.5 0 1 1-2.5 2.5V15m0 0H9m0 0H6.5A2.5 2.5 0 1 0 9 17.5V15"/></svg>`,
  plus: `<svg viewBox="0 0 24 24" ${S}><path d="M12 5v14M5 12h14"/></svg>`,
  key: `<svg viewBox="0 0 24 24" ${S}><circle cx="8" cy="15" r="4"/><path d="m11 12 9-9M16 7l3 3M14 9l2 2"/></svg>`,
  shield: `<svg viewBox="0 0 24 24" ${S}><path d="M12 3 4.5 6v5.5c0 4.4 3 8 7.5 9.5 4.5-1.500 7.500-5.100 7.500-9.500V6z"/><path d="m9 12 2 2 4-4"/></svg>`,
  copy: `<svg viewBox="0 0 24 24" ${S}><rect x="8.5" y="8.5" width="11" height="11" rx="2"/><path d="M15.500 8.500V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v7.500a2 2 0 0 0 2 2h2.500"/></svg>`,
  x: `<svg viewBox="0 0 24 24" ${S}><path d="m6 6 12 12M18 6 6 18"/></svg>`,
  home: `<svg viewBox="0 0 24 24" ${S}><path d="m4 11 8-7 8 7M6 9.500V20h12V9.500"/></svg>`,
};
export const icon = (name, cls) => svg(PATHS[name], cls);
export const osIcon = (os, cls) => icon(os === 'macos' ? 'macos' : 'windows', cls);
