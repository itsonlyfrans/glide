'use strict';
// Test hygiene. Every engine started with a fresh --data-dir creates its own identity, and the OS keystore keeps that
// credential after the temporary folder is gone. Windows Credential Manager has a limited total size ("Error 8: not enough
// memory resources"), so hundreds of leftovers eventually stop ALL new engines from starting.
// Test scripts call forgetUnder(tempDir) when they finish: it deletes the credential of every identity found in that folder.
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

function namespaces(dir, found = []) {
  let entries = [];
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return found; }
  for (const e of entries) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) { if (!['Cache', 'Code Cache', 'GPUCache', 'Network', 'Local Storage', 'DawnGraphiteCache', 'DawnWebGPUCache', 'GrShaderCache', 'ShaderCache', 'Session Storage'].includes(e.name)) namespaces(full, found); }
    else if (e.name === 'identity.namespace') {
      try { const ns = fs.readFileSync(full, 'utf8').trim(); if (/^[0-9a-f]{64}$/.test(ns)) found.push(ns); } catch { /* unreadable */ }
    }
  }
  return found;
}

function forgetUnder(dir) {
  if (process.platform !== 'win32') return 0;
  let n = 0;
  for (const ns of namespaces(dir)) {
    try { execFileSync('cmdkey', [`/delete:${ns}-device-pkcs8.com.glide.identity.v1`], { stdio: 'ignore' }); n += 1; } catch { /* already gone */ }
  }
  return n;
}

module.exports = { forgetUnder };
